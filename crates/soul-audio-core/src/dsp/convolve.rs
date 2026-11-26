//! **Convolution** with a measured impulse response — room correction and
//! headphone correction, the heavyweight DSP stage.
//!
//! You measure a room (or a headphone) and get an impulse response: the filter
//! that, applied to the audio, cancels what the room does to it. Applying it is
//! a convolution, and the IR is long — a headphone correction is a few hundred
//! to a few thousand taps, a room correction half a second or more (24 000 taps
//! at 48 kHz).
//!
//! # Two paths, by IR length
//!
//! - **Direct FIR** for short IRs (`len <= `[`DIRECT_MAX_TAPS`]): the plain
//!   `Σ h[k]·x[n−k]`. At these lengths it beats the transform (no transform
//!   overhead, no latency bookkeeping) and it is exact to the last bit of the
//!   arithmetic, which makes it the reference the FFT path is tested against.
//! - **Uniform-partitioned FFT overlap-save** above that: the IR is cut into
//!   [`PARTITION`]-tap partitions, each transformed once at load time; every
//!   input block is transformed once and multiplied against each partition's
//!   spectrum, accumulating into a frequency-domain delay line. Cost per output
//!   sample grows with `log(PARTITION)` rather than with the IR length, so a
//!   24 000-tap room correction costs ~94 complex multiply-accumulates per
//!   partition instead of 24 000 real ones.
//!
//! The direct path adds no latency. The partitioned path adds exactly one
//! [`PARTITION`] (5.3 ms at 48 kHz), constant and reported by
//! [`Convolver::latency_frames`]: overlap-save transforms
//! `[previous block | current block]` and keeps the aliasing-free second half,
//! so a block's output exists only once that block is complete. The delay is
//! the same however the caller chunks the stream, which is what makes the
//! output independent of the block size.
//!
//! # Where the memory lives
//!
//! An IR's spectra do not fit in an inline struct — 24 000 taps is 94
//! partitions × 512 complex `f32` = 385 KiB per channel — so the storage is
//! **caller-provided**, exactly as the decoders' scratch is
//! ([`crate::dispatch::decoder_scratch_len`]): `Convolver<P>` carves everything
//! from one `P: AsMut<[f32]>`, which is a `Vec<f32>` on the desktop and a
//! `&'static mut [f32]` (a `StaticCell` slice) on the DAP. [`storage_len`]
//! sizes it for a given IR length and channel count, and
//! [`Convolver::set_impulse_response`] declines rather than overrunning if it
//! is short.
//!
//! The crate stays `no_std`, no-alloc and `forbid(unsafe_code)`; the FFT reads
//! a checked-in twiddle table (`cargo xtask gen-convolve-tables`) since core
//! cannot call `sin`/`cos` at runtime.
//!
//! # How a host runs it
//!
//! [`Convolver`] implements [`DspStage`], so it processes blocks exactly like
//! the EQ and crossfeed stages — but it is **generic over its storage**, and
//! [`Stage`](super::Stage) (the chain's slot enum) is not: a chain slot is an
//! inline value, and this stage's filter is a megabyte that has to come from
//! the caller. So today a host runs the convolver where it already runs
//! block DSP that owns memory — as a sink decorator, reporting
//! [`Sink::modifies_samples`](crate::sink::Sink::modifies_samples) while it is
//! active so the bit-perfect claim
//! ([`DecodeInfo::bit_perfect`](crate::deck::DecodeInfo::bit_perfect)) stays
//! honest — with a `Vec<f32>` on the desktop and a `StaticCell` slice on the
//! DAP.
//!
//! Putting it *inside* [`Chain`](super::Chain) — so it can be reordered against
//! the EQ and crossfeed — means giving the chain (and therefore [`Deck`](crate::deck::Deck))
//! a storage type parameter. That is a deliberate API change to make on its
//! own, not a side effect of landing the engine; [`StageKind::Convolver`]
//! already exists so a signal path can name the stage either way.

use super::{DspStage, StageKind};

include!("convolve_tables.rs");

/// The convolver's FFT size. Twice [`PARTITION`], so a partition's linear
/// convolution (`PARTITION` taps × `PARTITION` samples = `2·PARTITION − 1`
/// points) fits without wrapping onto itself.
pub const FFT_N: usize = 512;

/// Taps per partition — and the block the partitioned path processes at a time.
/// It matches the deck's output block (256 frames), so the deck's stream is
/// block-aligned and each pump drives exactly one transform.
pub const PARTITION: usize = FFT_N / 2;

/// IR length up to which the direct (time-domain) FIR path is used. Below this
/// the transform's fixed cost dominates; the crossover is also where a
/// headphone correction typically sits, so the common small-filter case never
/// pays for a transform at all.
pub const DIRECT_MAX_TAPS: usize = 64;

/// The widest stream the convolver carries state for — the pipeline's own
/// channel cap.
const MAX_CHANNELS: usize = crate::channels::MAX_CHANNELS;

/// Why an impulse response could not be installed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum ConvolveError {
    /// The caller's storage is smaller than [`storage_len`] for this IR and
    /// channel count. The convolver is left **bypassed** (and inactive), never
    /// half-configured.
    StorageTooSmall {
        /// What this IR needs, in `f32` slots.
        need: usize,
        /// What the caller provided.
        have: usize,
    },
    /// More channels than the pipeline carries ([`crate::channels::MAX_CHANNELS`]),
    /// or zero.
    UnsupportedChannels,
    /// An empty impulse response (nothing to convolve with).
    EmptyImpulseResponse,
    /// The impulse response contains a NaN or an infinity.
    ///
    /// A corrupt measurement, or a file read as the wrong format. Convolving
    /// with it poisons every sample downstream — and since the deck's output
    /// cast turns a non-finite sample into zero, the audible result would be
    /// *silence* with nothing to point at. Refusing the filter and staying
    /// bypassed is the diagnosable outcome.
    NonFiniteImpulseResponse,
}

/// `f32` slots [`Convolver::set_impulse_response`] needs for an IR of
/// `taps` samples on `channels` channels.
///
/// - **Direct** path (`taps <= `[`DIRECT_MAX_TAPS`]): the IR plus a
///   per-channel history ring.
/// - **Partitioned** path, per channel: the IR's partition spectra, the
///   frequency-domain delay line of input spectra (the same shape), and three
///   [`PARTITION`]-sample buffers (the filling input block, the previous input
///   block, and the pending output block).
///
/// A measured correction is per channel, so the IR spectra are stored per
/// channel even when the same IR is installed on all of them — a stereo room
/// correction is two different filters, and sharing would be a one-filter
/// special case the caller cannot rely on.
#[must_use]
pub fn storage_len(taps: usize, channels: usize) -> usize {
    if taps == 0 || channels == 0 {
        return 0;
    }
    if taps <= DIRECT_MAX_TAPS {
        return channels * 2 * taps; // IR + history ring
    }
    let partitions = taps.div_ceil(PARTITION);
    // A spectrum is FFT_N complex values, stored as FFT_N reals then FFT_N
    // imaginaries (so a spectrum is two plain sub-slices — no transmute, no
    // `unsafe`).
    let spectra = 2 * channels * partitions * 2 * FFT_N; // IR + delay line
    let fifos = 3 * channels * PARTITION; // input, previous, output
    spectra + fifos
}

/// In-place iterative radix-2 DIT complex FFT over `re`/`im` (length a power of
/// two, `<= FFT_N`). `inverse` selects the `exp(+i…)` kernel and does **not**
/// normalize — [`Convolver`] folds the `1/N` into the IR spectra once, at load
/// time, so the per-block path never pays for it.
///
/// Split real/imaginary arrays rather than an interleaved complex type: the
/// convolver's storage is one `[f32]` slice, and this way a spectrum is two
/// plain sub-slices of it.
fn fft(re: &mut [f32], im: &mut [f32], inverse: bool) {
    let n = re.len();
    debug_assert_eq!(n, im.len());
    debug_assert!(n.is_power_of_two() && n <= FFT_N);
    if n < 2 {
        return;
    }
    // Bit-reversal permutation.
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let half = len / 2;
        let stride = FFT_N / len;
        let mut start = 0;
        while start < n {
            for k in 0..half {
                let idx = k * stride;
                let c = FFT_TW_COS[idx];
                // Forward twiddle is exp(-iθ) = (cos, -sin); inverse (cos, +sin).
                let s = if inverse {
                    FFT_TW_SIN[idx]
                } else {
                    -FFT_TW_SIN[idx]
                };
                let (br, bi) = (re[start + k + half], im[start + k + half]);
                let wr = br * c - bi * s;
                let wi = br * s + bi * c;
                let (ur, ui) = (re[start + k], im[start + k]);
                re[start + k] = ur + wr;
                im[start + k] = ui + wi;
                re[start + k + half] = ur - wr;
                im[start + k + half] = ui - wi;
            }
            start += len;
        }
        len <<= 1;
    }
}

/// Which path the installed IR runs on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// No IR installed — a passthrough.
    Bypass,
    /// Direct time-domain FIR, `taps` per channel. No latency.
    Direct { taps: usize },
    /// Uniform-partitioned FFT **overlap-save**, with a one-[`PARTITION`]
    /// latency (see [`Convolver::latency_frames`]).
    Partitioned { taps: usize, partitions: usize },
}

/// A convolution engine over caller-provided storage.
///
/// Create with [`Convolver::new`], install a filter with
/// [`Convolver::set_impulse_response`], then [`process`](DspStage::process) each
/// block. With no IR installed it is inactive and a bit-exact passthrough.
///
/// # Latency
///
/// The direct path is latency-free. The partitioned path runs
/// **overlap-save**: it transforms `[previous block | current block]` and keeps
/// the aliasing-free second half, so a block's output is ready only once the
/// block is complete — one [`PARTITION`] of delay (5.3 ms at 48 kHz), constant
/// and reported by [`Convolver::latency_frames`]. That delay is uniform
/// regardless of how the caller chunks the stream, which is what makes the
/// output independent of the block size; a host that cares (lip sync, an A/B
/// against the unprocessed path) compensates by that many frames.
pub struct Convolver<P> {
    storage: P,
    mode: Mode,
    channels: usize,
    enabled: bool,
    /// Samples in the filling input block, `0..PARTITION`. Shared by every
    /// channel (they advance together), and also the read cursor into the
    /// pending output block.
    fill: usize,
    /// Which delay-line slot the next completed block's spectrum goes to.
    slot: usize,
    /// One block's transform (the input window) and the accumulated product.
    /// Inline (8 KiB) so the hot path needs no aliasing against the spectra it
    /// reads from `storage`.
    work_re: [f32; FFT_N],
    work_im: [f32; FFT_N],
    acc_re: [f32; FFT_N],
    acc_im: [f32; FFT_N],
}

impl<P: AsRef<[f32]> + AsMut<[f32]>> Convolver<P> {
    /// A bypassed convolver over `storage`.
    pub fn new(storage: P) -> Self {
        Self {
            storage,
            mode: Mode::Bypass,
            channels: 0,
            enabled: true,
            fill: 0,
            slot: 0,
            work_re: [0.0; FFT_N],
            work_im: [0.0; FFT_N],
            acc_re: [0.0; FFT_N],
            acc_im: [0.0; FFT_N],
        }
    }

    /// Install `ir` as the impulse response for every channel of a
    /// `channels`-channel stream, choosing the direct or partitioned path by
    /// its length, and clear any running state.
    ///
    /// # Errors
    /// [`ConvolveError`] — an empty IR, an unsupported channel count, or
    /// storage smaller than [`storage_len`]. The convolver is left bypassed in
    /// every error case rather than half-configured.
    pub fn set_impulse_response(
        &mut self,
        ir: &[f32],
        channels: usize,
    ) -> Result<(), ConvolveError> {
        self.mode = Mode::Bypass;
        self.channels = 0;
        self.fill = 0;
        self.slot = 0;
        if ir.is_empty() {
            return Err(ConvolveError::EmptyImpulseResponse);
        }
        if ir.iter().any(|t| !t.is_finite()) {
            return Err(ConvolveError::NonFiniteImpulseResponse);
        }
        if channels == 0 || channels > MAX_CHANNELS {
            return Err(ConvolveError::UnsupportedChannels);
        }
        let need = storage_len(ir.len(), channels);
        let have = self.storage.as_ref().len();
        if have < need {
            return Err(ConvolveError::StorageTooSmall { need, have });
        }
        self.storage.as_mut()[..need].fill(0.0);

        if ir.len() <= DIRECT_MAX_TAPS {
            let taps = ir.len();
            let store = self.storage.as_mut();
            for ch in 0..channels {
                store[ch * taps..(ch + 1) * taps].copy_from_slice(ir);
            }
            self.mode = Mode::Direct { taps };
        } else {
            let partitions = ir.len().div_ceil(PARTITION);
            // Transform each partition, zero-padded to FFT_N and scaled by
            // 1/FFT_N so the inverse transform on the hot path needs no
            // normalization of its own.
            let scale = 1.0 / FFT_N as f32;
            for ch in 0..channels {
                for p in 0..partitions {
                    let from = p * PARTITION;
                    let to = (from + PARTITION).min(ir.len());
                    self.work_re.fill(0.0);
                    self.work_im.fill(0.0);
                    for (dst, &src) in self.work_re[..to - from].iter_mut().zip(&ir[from..to]) {
                        *dst = src * scale;
                    }
                    fft(&mut self.work_re, &mut self.work_im, false);
                    let base = ir_base(channels, partitions, ch, p);
                    let store = self.storage.as_mut();
                    store[base..base + FFT_N].copy_from_slice(&self.work_re);
                    store[base + FFT_N..base + 2 * FFT_N].copy_from_slice(&self.work_im);
                }
            }
            self.mode = Mode::Partitioned {
                taps: ir.len(),
                partitions,
            };
        }
        self.channels = channels;
        Ok(())
    }

    /// Remove the filter: back to a bit-exact passthrough.
    pub fn clear(&mut self) {
        self.mode = Mode::Bypass;
        self.channels = 0;
        self.fill = 0;
        self.slot = 0;
    }

    /// Enable/disable without discarding the filter. A disabled convolver is
    /// inactive, so the chain stays bit-exact.
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
    }

    /// Whether the convolver is enabled (it is only *active* with an IR
    /// installed too — see [`DspStage::is_active`]).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The installed IR's length in taps, or `None` when bypassed.
    #[must_use]
    pub fn taps(&self) -> Option<usize> {
        match self.mode {
            Mode::Bypass => None,
            Mode::Direct { taps } | Mode::Partitioned { taps, .. } => Some(taps),
        }
    }

    /// Whether the installed IR runs on the partitioned FFT path (`false` =
    /// the direct FIR path, or bypassed).
    #[must_use]
    pub fn is_partitioned(&self) -> bool {
        matches!(self.mode, Mode::Partitioned { .. })
    }

    /// The delay this convolver adds, in frames: `0` on the direct path, one
    /// [`PARTITION`] on the partitioned one (see the type's docs).
    #[must_use]
    pub fn latency_frames(&self) -> usize {
        match self.mode {
            Mode::Bypass | Mode::Direct { .. } => 0,
            Mode::Partitioned { .. } => PARTITION,
        }
    }

    /// Direct FIR over the interleaved block.
    fn process_direct(&mut self, buf: &mut [f32], channels: usize, taps: usize) {
        let store = self.storage.as_mut();
        let (ir_all, hist_all) = store.split_at_mut(channels * taps);
        for ch in 0..channels {
            let ir = &ir_all[ch * taps..(ch + 1) * taps];
            let hist = &mut hist_all[ch * taps..(ch + 1) * taps];
            let mut cursor = self.fill % taps;
            let mut i = ch;
            while i < buf.len() {
                hist[cursor] = buf[i];
                // y[n] = Σ h[k]·x[n−k], walking the ring back from the newest.
                let mut acc = 0.0f32;
                let mut idx = cursor;
                for &h in ir.iter() {
                    acc += h * hist[idx];
                    idx = if idx == 0 { taps - 1 } else { idx - 1 };
                }
                buf[i] = acc;
                cursor = (cursor + 1) % taps;
                i += channels;
            }
            if ch == channels - 1 {
                self.fill = cursor;
            }
        }
    }

    /// Transform one completed input block per channel and leave its output in
    /// the pending-output buffers. Overlap-save: the input window is
    /// `[previous block | this block]`, and the aliasing-free output is the
    /// second half of the inverse transform.
    fn transform_block(&mut self, channels: usize, partitions: usize) {
        for ch in 0..channels {
            let (prev_b, in_b, out_b) = (
                prev_base(channels, partitions, ch),
                in_base(channels, partitions, ch),
                out_base(channels, partitions, ch),
            );
            {
                let store = self.storage.as_ref();
                self.work_re[..PARTITION].copy_from_slice(&store[prev_b..prev_b + PARTITION]);
                self.work_re[PARTITION..].copy_from_slice(&store[in_b..in_b + PARTITION]);
            }
            self.work_im.fill(0.0);
            fft(&mut self.work_re, &mut self.work_im, false);

            // This block's spectrum into the delay line, then
            // Y = Σ_p H[p]·X[slot − p].
            let xb = fdl_base(channels, partitions, ch, self.slot);
            {
                let store = self.storage.as_mut();
                store[xb..xb + FFT_N].copy_from_slice(&self.work_re);
                store[xb + FFT_N..xb + 2 * FFT_N].copy_from_slice(&self.work_im);
            }
            self.acc_re.fill(0.0);
            self.acc_im.fill(0.0);
            let store = self.storage.as_ref();
            for p in 0..partitions {
                let x = fdl_base(
                    channels,
                    partitions,
                    ch,
                    (self.slot + partitions - p) % partitions,
                );
                let h = ir_base(channels, partitions, ch, p);
                for k in 0..FFT_N {
                    let (xr, xi) = (store[x + k], store[x + FFT_N + k]);
                    let (hr, hi) = (store[h + k], store[h + FFT_N + k]);
                    self.acc_re[k] += xr * hr - xi * hi;
                    self.acc_im[k] += xr * hi + xi * hr;
                }
            }
            fft(&mut self.acc_re, &mut self.acc_im, true);

            let store = self.storage.as_mut();
            // The second half is this block's output; this block becomes the
            // next window's leading half.
            store[out_b..out_b + PARTITION].copy_from_slice(&self.acc_re[PARTITION..]);
            let (a, b) = (in_b, prev_b);
            for k in 0..PARTITION {
                store[b + k] = store[a + k];
            }
        }
        self.slot = (self.slot + 1) % partitions;
    }

    /// Partitioned path over an interleaved block of any length: fill the input
    /// block sample by sample while emitting the pending output block, and
    /// transform whenever the input block completes.
    fn process_partitioned(&mut self, buf: &mut [f32], channels: usize, partitions: usize) {
        let frames = buf.len() / channels;
        let mut done = 0usize;
        while done < frames {
            let take = (frames - done).min(PARTITION - self.fill);
            for ch in 0..channels {
                let (in_b, out_b) = (
                    in_base(channels, partitions, ch),
                    out_base(channels, partitions, ch),
                );
                let store = self.storage.as_mut();
                for k in 0..take {
                    let i = (done + k) * channels + ch;
                    let x = buf[i];
                    // Emit the pending output for this sample slot, then take
                    // the input: they advance in lockstep, which is what makes
                    // the latency exactly one PARTITION.
                    buf[i] = store[out_b + self.fill + k];
                    store[in_b + self.fill + k] = x;
                }
            }
            self.fill += take;
            done += take;
            if self.fill == PARTITION {
                self.transform_block(channels, partitions);
                self.fill = 0;
            }
        }
    }
}

// --- storage layout ---------------------------------------------------------
//
// Direct:      [ch][taps] IR, then [ch][taps] history ring.
// Partitioned: [ch][p][2·FFT_N] IR spectra,
//              [ch][p][2·FFT_N] input-spectrum delay line,
//              [ch][PARTITION] filling input block,
//              [ch][PARTITION] previous input block,
//              [ch][PARTITION] pending output block.

fn ir_base(_channels: usize, partitions: usize, ch: usize, p: usize) -> usize {
    ((ch * partitions) + p) * 2 * FFT_N
}

fn fdl_base(channels: usize, partitions: usize, ch: usize, p: usize) -> usize {
    channels * partitions * 2 * FFT_N + ((ch * partitions) + p) * 2 * FFT_N
}

fn fifo_origin(channels: usize, partitions: usize) -> usize {
    2 * channels * partitions * 2 * FFT_N
}

fn in_base(channels: usize, partitions: usize, ch: usize) -> usize {
    fifo_origin(channels, partitions) + ch * PARTITION
}

fn prev_base(channels: usize, partitions: usize, ch: usize) -> usize {
    fifo_origin(channels, partitions) + (channels + ch) * PARTITION
}

fn out_base(channels: usize, partitions: usize, ch: usize) -> usize {
    fifo_origin(channels, partitions) + (2 * channels + ch) * PARTITION
}

impl<P: AsRef<[f32]> + AsMut<[f32]>> DspStage for Convolver<P> {
    /// Convolve the block in place. A channel count other than the one the IR
    /// was installed for passes through untouched — a correction filter is
    /// measured for a specific channel layout, and applying it to another is
    /// not a correction.
    fn process(&mut self, buf: &mut [f32], channels: usize, _sample_rate: u32) {
        if !self.is_active() || channels != self.channels || channels == 0 {
            return;
        }
        match self.mode {
            Mode::Bypass => {}
            Mode::Direct { taps } => self.process_direct(buf, channels, taps),
            Mode::Partitioned { partitions, .. } => {
                self.process_partitioned(buf, channels, partitions)
            }
        }
    }

    /// Active only when enabled **and** carrying an impulse response.
    fn is_active(&self) -> bool {
        self.enabled && !matches!(self.mode, Mode::Bypass)
    }

    /// Clear the running state — history ring / delay line / block buffers —
    /// but keep the filter: a seek must not cost a filter reload, and the
    /// pre-seek audio must not ring into the new position.
    fn reset(&mut self) {
        self.fill = 0;
        self.slot = 0;
        let channels = self.channels;
        match self.mode {
            Mode::Bypass => {}
            Mode::Direct { taps } => {
                self.storage.as_mut()[channels * taps..channels * 2 * taps].fill(0.0);
            }
            Mode::Partitioned { partitions, .. } => {
                let from = fdl_base(channels, partitions, 0, 0);
                let to = out_base(channels, partitions, channels - 1) + PARTITION;
                self.storage.as_mut()[from..to].fill(0.0);
            }
        }
    }

    fn kind(&self) -> StageKind {
        StageKind::Convolver
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random signal in `[-0.5, 0.5]` — broadband, with
    /// non-trivial low bits (a tone would mask a partition-boundary error).
    fn noise(n: usize, seed: u32) -> Vec<f32> {
        let mut x = seed | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x as f32 / u32::MAX as f32) - 0.5
            })
            .collect()
    }

    /// The reference: a straight time-domain convolution of `x` with `ir`,
    /// truncated to `x`'s length (what a streaming convolver produces).
    fn reference(x: &[f32], ir: &[f32]) -> Vec<f32> {
        let mut y = vec![0.0f32; x.len()];
        for (n, out) in y.iter_mut().enumerate() {
            let mut acc = 0.0f64;
            for (k, &h) in ir.iter().enumerate() {
                if k <= n {
                    acc += f64::from(h) * f64::from(x[n - k]);
                }
            }
            *out = acc as f32;
        }
        y
    }

    fn convolver(taps: usize, channels: usize) -> Convolver<Vec<f32>> {
        Convolver::new(vec![0.0f32; storage_len(taps, channels)])
    }

    /// Interleave `channels` identical copies of `mono`.
    fn spread(mono: &[f32], channels: usize) -> Vec<f32> {
        mono.iter()
            .flat_map(|&s| core::iter::repeat_n(s, channels))
            .collect()
    }

    fn channel(buf: &[f32], ch: usize, channels: usize) -> Vec<f32> {
        buf.iter().skip(ch).step_by(channels).copied().collect()
    }

    /// Compare a convolver's output against a reference, allowing for the
    /// path's own constant latency: `got[n + latency] == want[n]`. The first
    /// `latency` samples are the convolver's priming (zeros) and are checked as
    /// such — a path that emitted something else there would be ringing audio
    /// that does not exist yet.
    fn assert_matches_shifted(got: &[f32], want: &[f32], latency: usize, tol: f32, what: &str) {
        assert!(
            got[..latency].iter().all(|&v| v == 0.0),
            "{what}: the first {latency} samples must be the path's priming silence"
        );
        let n = want.len() - latency;
        let err = max_err(&got[latency..], &want[..n]);
        assert!(
            err < tol,
            "{what}: max err {err} (tolerance {tol}, latency {latency})"
        );
    }

    /// Largest absolute difference, and the scale to judge it against.
    fn max_err(a: &[f32], b: &[f32]) -> f32 {
        a.iter()
            .zip(b)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0f32, f32::max)
    }

    #[test]
    fn the_twiddle_table_is_a_unit_circle() {
        // The table cannot silently rot: every entry is on the unit circle, and
        // the quarter/eighth-turn values are the exact ones.
        for m in 0..FFT_N / 2 {
            let (c, s) = (FFT_TW_COS[m], FFT_TW_SIN[m]);
            assert!(
                (c * c + s * s - 1.0).abs() < 1e-6,
                "twiddle {m} is not on the unit circle: ({c}, {s})"
            );
        }
        assert_eq!((FFT_TW_COS[0], FFT_TW_SIN[0]), (1.0, 0.0));
        let quarter = FFT_N / 4; // 2π·N/4 / N = π/2
        assert!(FFT_TW_COS[quarter].abs() < 1e-6);
        assert!((FFT_TW_SIN[quarter] - 1.0).abs() < 1e-6);
        let eighth = FFT_N / 8;
        let root_half = core::f32::consts::FRAC_1_SQRT_2;
        assert!((FFT_TW_COS[eighth] - root_half).abs() < 1e-6);
        assert!((FFT_TW_SIN[eighth] - root_half).abs() < 1e-6);
    }

    #[test]
    fn the_fft_round_trips_and_matches_a_naive_dft() {
        let n = 64;
        let x = noise(n, 0x1234_5678);
        let mut re = x.clone();
        let mut im = vec![0.0f32; n];
        fft(&mut re, &mut im, false);

        // Against a naive DFT in f64.
        for k in 0..n {
            let (mut dr, mut di) = (0.0f64, 0.0f64);
            for (t, &v) in x.iter().enumerate() {
                let a = -2.0 * core::f64::consts::PI * (k * t) as f64 / n as f64;
                dr += f64::from(v) * a.cos();
                di += f64::from(v) * a.sin();
            }
            assert!(
                (f64::from(re[k]) - dr).abs() < 1e-3 && (f64::from(im[k]) - di).abs() < 1e-3,
                "bin {k}: fft ({}, {}) vs dft ({dr}, {di})",
                re[k],
                im[k]
            );
        }

        // Inverse (unnormalized) recovers n·x.
        fft(&mut re, &mut im, true);
        for (i, (&got, &want)) in re.iter().zip(&x).enumerate() {
            assert!(
                (got / n as f32 - want).abs() < 1e-5,
                "round trip sample {i}: {} vs {want}",
                got / n as f32
            );
        }
    }

    /// A bypassed / disabled / IR-less convolver must not touch the block —
    /// the property the chain's bit-exact bypass rests on.
    #[test]
    fn a_bypassed_convolver_is_an_exact_passthrough() {
        let input = spread(&noise(1_000, 7), 2);

        let mut c = convolver(0, 2);
        assert!(!c.is_active(), "no IR installed");
        assert_eq!(c.taps(), None);
        let mut buf = input.clone();
        c.process(&mut buf, 2, 48_000);
        assert_eq!(buf, input);

        // With an IR, then disabled.
        let ir = noise(32, 11);
        let mut c = convolver(ir.len(), 2);
        c.set_impulse_response(&ir, 2).expect("install");
        assert!(c.is_active());
        c.set_enabled(false);
        assert!(!c.is_active());
        let mut buf = input.clone();
        c.process(&mut buf, 2, 48_000);
        assert_eq!(buf, input, "a disabled convolver must not touch the block");

        // And cleared.
        c.set_enabled(true);
        c.clear();
        assert!(!c.is_active());
        let mut buf = input.clone();
        c.process(&mut buf, 2, 48_000);
        assert_eq!(buf, input);
    }

    /// A unit impulse is the identity filter — the simplest correctness check
    /// there is, on both paths.
    #[test]
    fn a_unit_impulse_is_the_identity() {
        for taps in [1usize, DIRECT_MAX_TAPS, DIRECT_MAX_TAPS + 1, 1_000] {
            let mut ir = vec![0.0f32; taps];
            ir[0] = 1.0;
            let mut c = convolver(taps, 2);
            c.set_impulse_response(&ir, 2).expect("install");
            let latency = c.latency_frames() * 2; // interleaved stereo
            let input = spread(&noise(2_000, 0x5eed), 2);
            let mut buf = input.clone();
            c.process(&mut buf, 2, 48_000);
            assert_matches_shifted(&buf, &input, latency, 1e-5, &format!("{taps} taps"));
        }
    }

    /// A delayed impulse delays the signal by exactly that many samples —
    /// which is the partitioned path's alignment, where an off-by-one would
    /// otherwise be inaudible but wrong.
    #[test]
    fn a_delayed_impulse_delays_by_exactly_that_many_samples() {
        for delay in [1usize, 63, 64, 255, 256, 257, 700] {
            let taps = delay + 1;
            let mut ir = vec![0.0f32; taps];
            ir[delay] = 1.0;
            let mut c = convolver(taps, 1);
            c.set_impulse_response(&ir, 1).expect("install");
            let latency = c.latency_frames();
            let input = noise(2_048, 0xabcd);
            let mut buf = input.clone();
            c.process(&mut buf, 1, 48_000);
            // The output is the input delayed by the IR's delay plus the
            // path's own latency — and nothing before that.
            let total = delay + latency;
            assert!(
                buf[..total].iter().all(|&v| v.abs() < 1e-6),
                "delay {delay}: audio appeared before the delay elapsed"
            );
            for n in total..input.len() {
                let want = input[n - total];
                assert!(
                    (buf[n] - want).abs() < 1e-5,
                    "delay {delay}, sample {n}: {} vs {want}",
                    buf[n]
                );
            }
        }
    }

    /// The headline: both paths match a straight time-domain convolution, for
    /// IR lengths either side of the crossover and well past one partition.
    #[test]
    fn both_paths_match_a_direct_time_domain_convolution() {
        for taps in [1usize, 7, 64, 65, 100, 256, 257, 512, 1_000, 2_048] {
            let ir = noise(taps, 0x1111 + taps as u32);
            let x = noise(3_000, 0x2222);
            let want = reference(&x, &ir);

            let mut c = convolver(taps, 1);
            c.set_impulse_response(&ir, 1).expect("install");
            assert_eq!(c.is_partitioned(), taps > DIRECT_MAX_TAPS);
            let latency = c.latency_frames();
            let mut buf = x.clone();
            c.process(&mut buf, 1, 48_000);

            // Scale-relative tolerance: the output of a 2048-tap random IR
            // over a ±0.5 signal is a few units, and f32 FFT round-off grows
            // with the transform size.
            let scale = want.iter().fold(0.0f32, |m, v| m.max(v.abs())).max(1.0);
            assert_matches_shifted(&buf, &want, latency, 2e-4 * scale, &format!("{taps} taps"));
        }
    }

    /// Block-size independence: the same stream convolved in odd-sized pieces
    /// must give the same answer as one long call. A partition-boundary or
    /// overlap bug shows up here and nowhere else.
    #[test]
    fn the_result_is_independent_of_how_the_stream_is_blocked() {
        let ir = noise(700, 0x3333);
        let x = spread(&noise(4_000, 0x4444), 2);

        let mut one = convolver(ir.len(), 2);
        one.set_impulse_response(&ir, 2).expect("install");
        let mut whole = x.clone();
        one.process(&mut whole, 2, 48_000);

        for piece_frames in [1usize, 17, 64, 255, 256, 257, 1_000] {
            let mut c = convolver(ir.len(), 2);
            c.set_impulse_response(&ir, 2).expect("install");
            let mut got = x.clone();
            let mut done = 0usize;
            while done < got.len() {
                let take = (piece_frames * 2).min(got.len() - done);
                c.process(&mut got[done..done + take], 2, 48_000);
                done += take;
            }
            let err = max_err(&got, &whole);
            assert!(
                err < 1e-4,
                "{piece_frames}-frame pieces diverge from one call: max err {err}"
            );
        }
    }

    /// Channels are independent: a filter applied to one channel must not leak
    /// into the other (the classic interleaving bug).
    #[test]
    fn channels_are_processed_independently() {
        let ir = noise(300, 0x5555);
        let left = noise(2_000, 0x6666);
        let right = noise(2_000, 0x7777);
        let mut buf: Vec<f32> = left
            .iter()
            .zip(&right)
            .flat_map(|(&l, &r)| [l, r])
            .collect();

        let mut c = convolver(ir.len(), 2);
        c.set_impulse_response(&ir, 2).expect("install");
        let latency = c.latency_frames();
        c.process(&mut buf, 2, 48_000);

        let want_l = reference(&left, &ir);
        let want_r = reference(&right, &ir);
        let scale = want_l.iter().fold(0.0f32, |m, v| m.max(v.abs())).max(1.0);
        assert_matches_shifted(&channel(&buf, 0, 2), &want_l, latency, 2e-4 * scale, "left");
        assert_matches_shifted(
            &channel(&buf, 1, 2),
            &want_r,
            latency,
            2e-4 * scale,
            "right",
        );
    }

    /// A block whose channel count is not the one the IR was installed for
    /// passes through: a correction measured for stereo is not a correction for
    /// 5.1, and silently applying it would be worse than not applying it.
    #[test]
    fn a_mismatched_channel_count_passes_through() {
        let ir = noise(300, 0x8888);
        let mut c = convolver(ir.len(), 2);
        c.set_impulse_response(&ir, 2).expect("install");
        let input = spread(&noise(600, 0x9999), 6);
        let mut buf = input.clone();
        c.process(&mut buf, 6, 48_000);
        assert_eq!(buf, input);
    }

    /// `reset` clears the running state but keeps the filter: a seek must not
    /// cost a reload, and must not ring the pre-seek audio into the new
    /// position.
    #[test]
    fn reset_clears_the_state_but_keeps_the_filter() {
        for taps in [32usize, 700] {
            let ir = noise(taps, 0xaaaa);
            let mut c = convolver(taps, 1);
            c.set_impulse_response(&ir, 1).expect("install");
            let x = noise(1_000, 0xbbbb);

            let mut first = x.clone();
            c.process(&mut first, 1, 48_000);
            // Without a reset the state carries (as it must for streaming).
            let mut again = x.clone();
            c.process(&mut again, 1, 48_000);
            assert!(max_err(&first, &again) > 1e-6, "{taps}: state must carry");

            c.reset();
            assert!(c.is_active(), "{taps}: the filter survives a reset");
            let mut third = x.clone();
            c.process(&mut third, 1, 48_000);
            assert!(
                max_err(&first, &third) < 1e-6,
                "{taps}: reset must restore the initial state"
            );
        }
    }

    /// Storage is sized by [`storage_len`], and an undersized buffer is a
    /// clean error that leaves the convolver bypassed — never a partial
    /// configuration, and never an overrun.
    #[test]
    fn undersized_storage_is_refused_and_leaves_it_bypassed() {
        let ir = noise(1_000, 0xcccc);
        let need = storage_len(ir.len(), 2);
        let mut c = Convolver::new(vec![0.0f32; need - 1]);
        assert_eq!(
            c.set_impulse_response(&ir, 2),
            Err(ConvolveError::StorageTooSmall {
                need,
                have: need - 1
            })
        );
        assert!(!c.is_active(), "a refused install must leave it bypassed");
        let input = spread(&noise(100, 0xdddd), 2);
        let mut buf = input.clone();
        c.process(&mut buf, 2, 48_000);
        assert_eq!(buf, input);

        // Exactly enough is enough.
        let mut c = Convolver::new(vec![0.0f32; need]);
        c.set_impulse_response(&ir, 2).expect("install");
        assert!(c.is_active());
    }

    /// Hostile and degenerate inputs: whatever a host hands this stage, it
    /// must not panic — it sits in the audio callback, where a panic is an
    /// aborted stream rather than an error message.
    #[test]
    fn degenerate_inputs_are_survived_on_both_paths() {
        // A buffer that is not a whole number of frames, an empty one, and one
        // shorter than a single frame — on both paths.
        for taps in [3usize, 700] {
            let ir = vec![0.01f32; taps];
            let mut c = convolver(taps, 2);
            c.set_impulse_response(&ir, 2).expect("install");
            for len in [0usize, 1, 7] {
                let mut buf = vec![1.0f32; len];
                c.process(&mut buf, 2, 48_000);
                assert!(buf.iter().all(|v| v.is_finite()), "taps {taps} len {len}");
            }
            // A sample rate of zero (a misconfigured sink) must not divide by it.
            let mut buf = vec![0.5f32; 512];
            c.process(&mut buf, 2, 0);
            assert!(buf.iter().all(|v| v.is_finite()), "taps {taps} at rate 0");
        }

        // A single-tap IR is a gain, and the widest channel count works on
        // both paths.
        let mut c = convolver(1, 1);
        c.set_impulse_response(&[2.0], 1).expect("install");
        let mut buf = vec![1.0f32; 5];
        c.process(&mut buf, 1, 48_000);
        assert!(buf.iter().all(|&v| (v - 2.0).abs() < 1e-6), "{buf:?}");

        for taps in [DIRECT_MAX_TAPS, DIRECT_MAX_TAPS + 1] {
            let ir = vec![0.01f32; taps];
            let mut c = convolver(taps, MAX_CHANNELS);
            c.set_impulse_response(&ir, MAX_CHANNELS).expect("install");
            let mut buf = vec![0.5f32; MAX_CHANNELS * 300];
            c.process(&mut buf, MAX_CHANNELS, 48_000);
            assert!(buf.iter().all(|v| v.is_finite()), "taps {taps}");
        }

        // reset / clear in any order and in any mode.
        let mut c = convolver(700, 2);
        c.set_impulse_response(&vec![0.01f32; 700], 2)
            .expect("install");
        c.reset();
        c.clear();
        c.reset();
        assert!(!c.is_active());
    }

    /// A corrupt measurement — a NaN or an infinity in the IR — is refused.
    ///
    /// Convolving with it poisons every sample, and because the deck's output
    /// cast turns a non-finite sample into zero the user would hear *silence*
    /// with nothing to point at. The filter is declined and the stage stays
    /// bypassed, which is diagnosable.
    #[test]
    fn a_non_finite_impulse_response_is_refused() {
        let mut c = convolver(8, 2);
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut ir = vec![0.1f32; 8];
            ir[5] = bad;
            assert_eq!(
                c.set_impulse_response(&ir, 2),
                Err(ConvolveError::NonFiniteImpulseResponse),
                "{bad}"
            );
            assert!(!c.is_active(), "a refused filter leaves it bypassed");
        }
        // Long enough for the partitioned path, too.
        let mut c = convolver(700, 2);
        let mut ir = vec![0.01f32; 700];
        ir[699] = f32::NAN;
        assert_eq!(
            c.set_impulse_response(&ir, 2),
            Err(ConvolveError::NonFiniteImpulseResponse)
        );
        // And a finite one still installs.
        assert!(c.set_impulse_response(&vec![0.01f32; 700], 2).is_ok());
    }

    #[test]
    fn empty_and_oversized_channel_counts_are_refused() {
        let mut c = convolver(100, 2);
        assert_eq!(
            c.set_impulse_response(&[], 2),
            Err(ConvolveError::EmptyImpulseResponse)
        );
        let ir = noise(100, 0xeeee);
        assert_eq!(
            c.set_impulse_response(&ir, 0),
            Err(ConvolveError::UnsupportedChannels)
        );
        assert_eq!(
            c.set_impulse_response(&ir, MAX_CHANNELS + 1),
            Err(ConvolveError::UnsupportedChannels)
        );
        assert!(!c.is_active());
    }

    /// Re-installing a different filter mid-stream works and does not carry
    /// the old one's state (a user switching correction profiles).
    #[test]
    fn a_new_impulse_response_replaces_the_old_one_cleanly() {
        let short = noise(16, 0x1a1a);
        let long = noise(900, 0x2b2b);
        let need = storage_len(long.len(), 1).max(storage_len(short.len(), 1));
        let mut c = Convolver::new(vec![0.0f32; need]);
        let x = noise(1_500, 0x3c3c);

        c.set_impulse_response(&short, 1).expect("install short");
        assert!(!c.is_partitioned());
        assert_eq!(c.latency_frames(), 0, "the direct path adds no latency");
        let mut buf = x.clone();
        c.process(&mut buf, 1, 48_000);
        assert_matches_shifted(&buf, &reference(&x, &short), 0, 2e-4, "short IR");

        c.set_impulse_response(&long, 1).expect("install long");
        assert!(c.is_partitioned());
        let latency = c.latency_frames();
        let mut buf = x.clone();
        c.process(&mut buf, 1, 48_000);
        let want = reference(&x, &long);
        let scale = want.iter().fold(0.0f32, |m, v| m.max(v.abs())).max(1.0);
        assert_matches_shifted(
            &buf,
            &want,
            latency,
            2e-4 * scale,
            "the new filter must apply with no trace of the old one's state",
        );
    }

    /// A room-correction-sized IR (half a second at 48 kHz) works, and its
    /// storage is what `storage_len` says — the "decide the ceiling vs the
    /// cost" question, answered concretely.
    #[test]
    fn a_room_correction_sized_impulse_response_works() {
        let taps = 24_000; // 0.5 s at 48 kHz
        let ir = noise(taps, 0x4d4d);
        let need = storage_len(taps, 2);
        // 94 partitions × (FFT_N complex = 2·FFT_N floats) × 2 (IR + delay
        // line) × 2 channels, plus the three PARTITION buffers per channel.
        assert_eq!(need, 2 * 2 * 94 * 2 * FFT_N + 3 * 2 * PARTITION);
        // Concretely: 1510 KiB of f32 for a 0.5 s stereo room correction —
        // the "IR-length ceiling vs cost" tradeoff, in bytes.
        assert_eq!(need * 4 / 1024, 1_510, "storage, KiB");
        let mut c = Convolver::new(vec![0.0f32; need]);
        c.set_impulse_response(&ir, 2).expect("install");
        assert!(c.is_partitioned());
        assert_eq!(c.taps(), Some(taps));

        // Two blocks through it: the first emits the path's priming silence
        // (one PARTITION of latency), the second real audio — finite and
        // bounded, with a 24 000-tap filter actually applied.
        assert_eq!(c.latency_frames(), PARTITION);
        let mut first = spread(&noise(PARTITION, 0x5e5e), 2);
        c.process(&mut first, 2, 48_000);
        assert!(
            first.iter().all(|&v| v == 0.0),
            "the latency block is silent"
        );
        let mut second = spread(&noise(PARTITION, 0x6f6f), 2);
        c.process(&mut second, 2, 48_000);
        assert!(second.iter().all(|v| v.is_finite()));
        assert!(
            second.iter().any(|&v| v != 0.0),
            "the second block must carry the convolved first one"
        );
    }
}
