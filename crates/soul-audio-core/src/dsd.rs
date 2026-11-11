//! DSD → PCM adapters. Native SAI and DoP both need the raw 1-bit stream; only
//! decimation makes real PCM. The output *mode* is chosen by constructing the
//! matching adapter — there is no runtime flag.
//!
//! - [`DopAdapter`] wraps a [`DsdDecoder`] as a [`Decoder`] by DoP-framing (still
//!   DSD, for a DoP-capable DAC).
//! - [`DecimateAdapter`] wraps a [`DsdDecoder`] as a [`Decoder`] by decimating to
//!   real PCM (plays anywhere) via a table-driven low-pass FIR (8:1).
//!
//! For [`DsdMode::Native`](crate::model::DsdMode) use the [`DsdDecoder`]
//! directly (no adapter).

use crate::decoder::{DecodeError, Decoder, DsdDecoder};
use crate::model::{SampleFormat, StreamInfo};

use crate::decoder::{DsdSeek, secs_to_samples};

/// The two DoP marker bytes. They alternate **once per DoP frame** (not per
/// channel) so a DoP DAC recognises the stream; every channel of a frame carries
/// the same marker. Phase is `frame_index & 1`.
const DOP_MARKER: [u8; 2] = [0x05, 0xFA];

/// Maximum channels the DoP framer supports (group buffer = 2 bytes/channel).
const MAX_DOP_CHANNELS: usize = 8;

/// Wraps a [`DsdDecoder`] and presents it as a PCM [`Decoder`] using DoP.
///
/// Each DoP frame packs 16 DSD bits **per channel** into a 24-bit word
/// `marker<<16 | b_t0<<8 | b_t1` (earlier byte high), emitted as ordinary
/// interleaved PCM frames (`L,R,…`) at `DSD_rate/16`. The `0x05`/`0xFA` marker
/// toggles once per frame and is shared by all channels (phase `frame_index &
/// 1`). The 24-bit word occupies the low 24 bits of each `i32`; a 32-bit
/// transport shell must left-justify so the marker is the true MSByte.
pub struct DopAdapter<D> {
    inner: D,
    channels: usize,
    frame_index: u64,
    group: [u8; 2 * MAX_DOP_CHANNELS], // t0 row (C bytes) then t1 row (C bytes)
    ch_pos: usize,                     // next channel to emit within the current frame
}

impl<D: DsdDecoder> DopAdapter<D> {
    /// Wrap `inner`. The first frame uses marker `0x05`.
    pub fn new(inner: D) -> Self {
        let channels = inner.stream_info().channels as usize;
        Self {
            inner,
            channels,
            frame_index: 0,
            group: [0u8; 2 * MAX_DOP_CHANNELS],
            ch_pos: 0,
        }
    }

    /// Read the next DoP frame's `2*C` DSD bytes (t0 row then t1 row) into
    /// `self.group`. Returns `false` at end of stream (an incomplete trailing
    /// group is dropped).
    fn load_group(&mut self) -> Result<bool, DecodeError> {
        let need = 2 * self.channels;
        let mut filled = 0;
        while filled < need {
            let k = self.inner.read_dsd(&mut self.group[filled..need])?;
            if k == 0 {
                break;
            }
            filled += k;
        }
        Ok(filled == need)
    }
}

impl<D: DsdDecoder + DsdSeek> DopAdapter<D> {
    /// Seek to output DoP `frame` (random access). Re-derives the marker phase
    /// from the landed position so the alternation stays unbroken.
    ///
    /// # Errors
    /// Propagates the inner [`DsdSeek::seek_to_sample`] error.
    pub fn seek_to_frame(&mut self, frame: u64) -> Result<u64, DecodeError> {
        // One DoP frame carries 16 DSD bits (2 bytes) per channel. Saturate the
        // conversion: a wildly out-of-range target simply clamps to the end in the
        // inner seek rather than overflowing.
        let landed = self.inner.seek_to_sample(frame.saturating_mul(16))?;
        self.frame_index = landed / 16;
        self.ch_pos = 0; // drop any partial frame
        Ok(self.frame_index)
    }

    /// Seek to `secs` seconds (convenience over [`Self::seek_to_frame`]).
    ///
    /// # Errors
    /// Propagates the inner seek error.
    pub fn seek_to_time(&mut self, secs: f64) -> Result<u64, DecodeError> {
        self.seek_to_frame(secs_to_samples(secs, self.stream_info().sample_rate))
    }
}

impl<D: DsdDecoder> Decoder for DopAdapter<D> {
    fn stream_info(&self) -> StreamInfo {
        let si = self.inner.stream_info();
        // DoP carries 16 DSD bits per frame → frame rate = bit rate / 16.
        let frame_rate = match si.format {
            SampleFormat::Dsd { rate } => rate.hz() / 16,
            _ => si.sample_rate,
        };
        StreamInfo {
            sample_rate: frame_rate,
            channels: si.channels,
            format: SampleFormat::Int { bits: 24 },
        }
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        let c = self.channels;
        if c == 0 || c > MAX_DOP_CHANNELS {
            return Err(DecodeError::Unsupported);
        }
        let mut w = 0;
        while w < out.len() {
            // Load a fresh frame only at a frame boundary; a partial frame from a
            // previous call resumes from the persisted group.
            if self.ch_pos == 0 && !self.load_group()? {
                break; // end of stream
            }
            let marker = DOP_MARKER[(self.frame_index & 1) as usize];
            let b_t0 = self.group[self.ch_pos];
            let b_t1 = self.group[c + self.ch_pos];
            out[w] = (i32::from(marker) << 16) | (i32::from(b_t0) << 8) | i32::from(b_t1);
            w += 1;
            self.ch_pos += 1;
            if self.ch_pos == c {
                self.ch_pos = 0;
                self.frame_index += 1;
            }
        }
        Ok(w)
    }
}

/// Fixed decimation ratio: one PCM sample per input **byte** (8 DSD bits → 1
/// sample). DSD64 (2.8224 MHz) → 352.8 kHz; DSD128 → 705.6 kHz; DSD256 → 1.4112
/// MHz; DSD512 → 2.8224 MHz.
const DECIMATION: u32 = 8;

/// FIR length in taps (a multiple of 8): a 128-tap symmetric Kaiser-windowed
/// sinc low-pass (β ≈ 8.6, ~90 dB stopband) with cutoff at the output Nyquist
/// (`f_in / 16`). Longer + steeper than the original 64-tap Blackman design, for
/// better anti-alias rejection of the shaped DSD noise.
const TAPS: usize = 128;

/// Byte-groups spanning the filter — one history byte (8 bits) per group.
const GROUPS: usize = TAPS / 8;

/// Maximum channels the decimator supports (fixed per-channel history state).
const MAX_DECIMATE_CHANNELS: usize = 8;

/// DSD "digital silence" seed for the FIR history. A DSD `0` bit is −1 and `1`
/// is +1, so an all-zero byte is full-scale NEGATIVE, not silence — seeding the
/// history with it makes the filter start at a large negative DC and the first
/// output samples pop. `0xAA` (alternating bits) is a Nyquist-rate pattern the
/// low-pass filter attenuates to ≈0, i.e. true silence, so playback and
/// post-seek output start cleanly.
const DSD_SILENCE: u8 = 0xAA;

/// 24-bit signed PCM full-scale bounds; FIR overshoot is saturated into range.
const PCM_MAX: i32 = (1 << 23) - 1;
const PCM_MIN: i32 = -(1 << 23);

/// FIR taps, DC-normalised so the integer coefficients sum to exactly
/// [`PCM_MAX`]: a constant all-ones DSD input (every bit → `+1`) maps to
/// `+PCM_MAX`, all-zeros (every bit → `-1`) to `-PCM_MAX`. Symmetric (linear
/// phase). Generated from a Kaiser-windowed sinc (cutoff `f_in/16`, β ≈ 8.6)
/// rounded to i32; the Nyquist (alternating-bit) response is ≈ 0, rejecting the
/// DSD noise.
const COEFFS: [i32; TAPS] = [
    -11, -53, -121, -207, -287, -328, -288, -130, 165, 588, 1087, 1567, 1898, 1932, 1539, 640,
    -753, -2508, -4367, -5964, -6873, -6687, -5106, -2042, 2316, 7459, 12585, 16685, 18703, 17726,
    13210, 5162, -5732, -18090, -29950, -39016, -43024, -40167, -29520, -11390, 12505, 39070,
    64124, 82934, 90935, 84557, 62011, 23925, -26325, -82650, -136729, -178878, -199249, -189187,
    -142580, -56986, 65637, 218739, 391588, 570261, 739107, 882515, 986747, 1041584, 1041585,
    986747, 882515, 739107, 570261, 391588, 218739, 65637, -56986, -142580, -189187, -199249,
    -178878, -136729, -82650, -26325, 23925, 62011, 84557, 90935, 82934, 64124, 39070, 12505,
    -11390, -29520, -40167, -43024, -39016, -29950, -18090, -5732, 5162, 13210, 17726, 18703,
    16685, 12585, 7459, 2316, -2042, -5106, -6687, -6873, -5964, -4367, -2508, -753, 640, 1539,
    1932, 1898, 1567, 1087, 588, 165, -130, -288, -328, -287, -207, -121, -53, -11,
];

/// Per-(group, byte) partial-sum table, à la Gesemann `dsd2pcm` / FFmpeg
/// `dsd.c`: `TABLE[g][b]` is the FIR contribution of the 8 DSD bits packed in
/// byte `b` at tap-group `g`, so filtering a byte is `GROUPS` lookups + adds —
/// no per-bit branching at run time.
const TABLE: [[i32; 256]; GROUPS] = build_table();

/// Build [`TABLE`] at compile time from [`COEFFS`]. For tap-group `g` and byte
/// `b`, bit `p` (LSB = the newest sample within the byte) contributes
/// `±COEFFS[g*8 + p]`, `+` when the bit is set. Tap index `g*8 + p` equals the
/// sample's age (0 = newest), so this is a standard time-ordered FIR.
const fn build_table() -> [[i32; 256]; GROUPS] {
    let mut table = [[0i32; 256]; GROUPS];
    let mut g = 0;
    while g < GROUPS {
        let mut b = 0usize;
        while b < 256 {
            let mut acc = 0i32;
            let mut p = 0usize;
            while p < 8 {
                let coeff = COEFFS[g * 8 + p];
                if ((b >> p) & 1) == 1 {
                    acc += coeff;
                } else {
                    acc -= coeff;
                }
                p += 1;
            }
            table[g][b] = acc;
            b += 1;
        }
        g += 1;
    }
    table
}

/// Wraps a [`DsdDecoder`] and presents it as a PCM [`Decoder`] by decimating the
/// 1-bit stream to multi-bit PCM (plays on any DAC).
///
/// Runs a 128-tap table-driven low-pass FIR per channel and decimates 8:1 (one
/// 24-bit PCM sample per input byte), de-interleaving the clustered-frame byte
/// stream into per-channel filters and re-interleaving the PCM output (`L,R,…`).
/// Supports up to [`MAX_DECIMATE_CHANNELS`] channels; more are rejected as
/// [`DecodeError::Unsupported`].
pub struct DecimateAdapter<D> {
    inner: D,
    channels: usize,
    /// Ring of the last [`GROUPS`] input bytes per channel (newest at `ring_pos`).
    hist: [[u8; GROUPS]; MAX_DECIMATE_CHANNELS],
    /// Slot of the newest history byte; advances once per clustered frame.
    ring_pos: usize,
    /// Scratch for one clustered frame (one byte per channel).
    frame: [u8; MAX_DECIMATE_CHANNELS],
}

impl<D: DsdDecoder> DecimateAdapter<D> {
    /// Wrap `inner`. Per-channel history starts silent (zeroed).
    pub fn new(inner: D) -> Self {
        let channels = inner.stream_info().channels as usize;
        Self {
            inner,
            channels,
            hist: [[DSD_SILENCE; GROUPS]; MAX_DECIMATE_CHANNELS],
            ring_pos: GROUPS - 1, // first frame advances to slot 0
            frame: [0u8; MAX_DECIMATE_CHANNELS],
        }
    }

    /// Read one clustered frame (`channels` bytes, one per channel) into
    /// `self.frame`. Returns `false` at end of stream (a partial trailing frame
    /// is dropped).
    fn load_frame(&mut self) -> Result<bool, DecodeError> {
        let need = self.channels;
        let mut filled = 0;
        while filled < need {
            let k = self.inner.read_dsd(&mut self.frame[filled..need])?;
            if k == 0 {
                break;
            }
            filled += k;
        }
        Ok(filled == need)
    }

    /// Total output PCM frames, if the inner DSD decoder declares its length.
    /// One PCM frame = [`DECIMATION`] input DSD samples per channel, so this is
    /// the inner per-channel sample count divided by the ratio. `None` for a
    /// length-unknown source. Lets the deck report a DSD track's duration.
    #[must_use]
    pub fn total_frames(&self) -> Option<u64> {
        self.inner
            .total_samples()
            .map(|s| s / u64::from(DECIMATION))
    }
}

impl<D: DsdDecoder + DsdSeek> DecimateAdapter<D> {
    /// Seek to output PCM `frame` (random access). One PCM frame = [`DECIMATION`]
    /// DSD samples per channel; the FIR history is reset (so the first output
    /// samples after a seek are a short filter-warm-up transient).
    ///
    /// # Errors
    /// Propagates the inner [`DsdSeek::seek_to_sample`] error.
    pub fn seek_to_frame(&mut self, frame: u64) -> Result<u64, DecodeError> {
        let landed = self
            .inner
            .seek_to_sample(frame.saturating_mul(u64::from(DECIMATION)))?;
        self.hist = [[DSD_SILENCE; GROUPS]; MAX_DECIMATE_CHANNELS];
        self.ring_pos = GROUPS - 1;
        Ok(landed / u64::from(DECIMATION))
    }

    /// Seek to `secs` seconds (convenience over [`Self::seek_to_frame`]).
    ///
    /// # Errors
    /// Propagates the inner seek error.
    pub fn seek_to_time(&mut self, secs: f64) -> Result<u64, DecodeError> {
        self.seek_to_frame(secs_to_samples(secs, self.stream_info().sample_rate))
    }
}

impl<D: DsdDecoder> Decoder for DecimateAdapter<D> {
    fn stream_info(&self) -> StreamInfo {
        let si = self.inner.stream_info();
        // Decimated PCM rate = 1-bit rate / ratio; fall back to the base rate for
        // an already-PCM inner (defensive — an inner DSD decoder reports Dsd).
        let sample_rate = match si.format {
            SampleFormat::Dsd { rate } => rate.hz() / DECIMATION,
            _ => si.sample_rate,
        };
        StreamInfo {
            sample_rate,
            channels: si.channels,
            format: SampleFormat::Int { bits: 24 },
        }
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        let c = self.channels;
        if c == 0 || c > MAX_DECIMATE_CHANNELS {
            return Err(DecodeError::Unsupported);
        }
        let mut w = 0;
        // Emit whole PCM frames only (one sample per channel per clustered frame).
        while w + c <= out.len() {
            if !self.load_frame()? {
                break; // end of stream
            }
            self.ring_pos = (self.ring_pos + 1) % GROUPS;
            let slot = self.ring_pos;
            let mut ch = 0;
            while ch < c {
                self.hist[ch][slot] = self.frame[ch];
                // FIR dot product: group g holds the byte of age g (0 = newest).
                let mut acc = 0i32;
                let mut g = 0;
                while g < GROUPS {
                    let idx = (slot + GROUPS - g) % GROUPS;
                    acc += TABLE[g][self.hist[ch][idx] as usize];
                    g += 1;
                }
                out[w] = acc.clamp(PCM_MIN, PCM_MAX);
                w += 1;
                ch += 1;
            }
        }
        Ok(w)
    }
}

/// Second-stage PCM decimation ratio for the **playback** path. [`DecimateAdapter`]
/// lands DSD64 at 352.8 kHz (hi-res, the archival/export rate); a further 8:1 drop
/// brings it to its *native* 44.1 kHz family (2.8224 MHz / 64 = 44.1 kHz — DSD64 is
/// `64 × 44100`), so the deck's arbitrary-ratio resampler runs at 44.1 kHz instead of
/// 352.8 kHz. That is ~7× less resampling work — the single-stage 352.8 kHz→device
/// resample is the real-time bottleneck for DSD — while the cascade of two linear-phase
/// Kaiser-windowed stages keeps the anti-aliasing audiophile-clean (and strips the
/// ultrasonic DSD shaped noise). A 48 kHz device then gets a cheap 44.1→48 kHz resample;
/// a 44.1 kHz device plays bit-for-bit with no resampling at all.
const PCM_DECIMATION: usize = 8;

/// Stage-1 PCM frames pulled per refill of the second stage's input buffer. A
/// modest fixed chunk (bounded no-alloc scratch) amortises the per-call cost of
/// the inner [`DecimateAdapter`] without a large by-value buffer.
const PEND_FRAMES: usize = 128;

/// **Adaptive third-stage** FIR decimators that pull DSD128/256/512 down the last
/// factor onto 44.1 kHz. Stage-1 (÷8) + stage-2 (÷8) already land DSD64 exactly on
/// 44.1 kHz; the higher families leave `DSD_multiple / 64` ∈ {2, 4, 8} over, which a
/// third linear-phase FIR removes (DSD64's ratio is 1 — bypassed, so its output is
/// bit-for-bit the two-stage result). Each prototype has its −6 dB cutoff at the
/// **44.1 kHz output Nyquist (22.05 kHz)**, exactly like stage 2: for a ÷M drop the
/// output Nyquist is `f_in / (2M)`, so the normalised cutoff is `1/(2M)` of the input
/// rate — ÷2 → `f_in/4`, ÷4 → `f_in/8`, ÷8 → `f_in/16`.
///
/// ÷8 reuses the stage-2 [`COEFFS`] prototype unchanged (its cutoff is already
/// `f_in/16`). [`COEFFS_DIV2`]/[`COEFFS_DIV4`] were generated the same way the
/// original [`COEFFS`] were — a 128-tap Kaiser-windowed sinc (β = 8.6, ≈ 90 dB
/// stopband), sampled at cutoff `f_in/4` (÷2) and `f_in/8` (÷4), then DC-normalised
/// and rounded to `i32` so the taps sum to exactly [`PCM_MAX`] (unity DC gain, same
/// as [`COEFFS`]); the lone 1-LSB centre asymmetry is forced (an even-length
/// symmetric integer kernel cannot sum to the odd `PCM_MAX`). Regenerate with
/// `testdata`'s coefficient recipe. Measured: passband flat to 20 kHz, −6 dB at
/// 22.05 kHz, and ≥ 87 dB (÷2) rejection by the first fold, reaching ≈ 130 dB by the
/// input Nyquist — clean of both aliasing and residual ultrasonic DSD noise.
const COEFFS_DIV2: [i32; TAPS] = [
    -40, -67, 103, 149, -207, -279, 366, 472, -598, -748, 924, 1130, -1368, -1643, 1959, 2320,
    -2729, -3192, 3714, 4300, -4956, -5686, 6499, 7400, -8396, -9494, 10702, 12030, -13484, -15075,
    16813, 18710, -20776, -23025, 25471, 28130, -31019, -34159, 37572, 41284, -45324, -49727,
    54534, 59793, -65562, -71911, 78927, 86716, -95415, -105195, 116280, 128966, -143652, -160892,
    181472, 206550, -237903, -278405, 333023, 411140, -532873, -750526, 1255905, 3775275, 3775276,
    1255905, -750526, -532873, 411140, 333023, -278405, -237903, 206550, 181472, -160892, -143652,
    128966, 116280, -105195, -95415, 86716, 78927, -71911, -65562, 59793, 54534, -49727, -45324,
    41284, 37572, -34159, -31019, 28130, 25471, -23025, -20776, 18710, 16813, -15075, -13484,
    12030, 10702, -9494, -8396, 7400, 6499, -5686, -4956, 4300, 3714, -3192, -2729, 2320, 1959,
    -1643, -1368, 1130, 924, -748, -598, 472, 366, -279, -207, 149, 103, -67, -40,
];

/// Third-stage ÷4 FIR (cutoff `f_in/8` = 22.05 kHz output Nyquist). See
/// [`COEFFS_DIV2`] for the generation method; taps sum to [`PCM_MAX`].
const COEFFS_DIV4: [i32; TAPS] = [
    -21, -88, -135, -81, 112, 364, 478, 255, -324, -977, -1207, -611, 741, 2147, 2560, 1255, -1477,
    -4171, -4853, -2327, 2682, 7430, 8491, 4005, -4544, -12404, -13983, -6510, 7297, 19697, 21968,
    10126, -11244, -30083, -33279, -15224, 16787, 44631, 49091, 22343, -24529, -64972, -71252,
    -32360, 35482, 93956, 103123, 46931, -51638, -137444, -151927, -69796, 77744, 210216, 237105,
    111784, -128752, -363755, -435116, -222508, 288389, 980612, 1640923, 2043170, 2043171, 1640923,
    980612, 288389, -222508, -435116, -363755, -128752, 111784, 237105, 210216, 77744, -69796,
    -151927, -137444, -51638, 46931, 103123, 93956, 35482, -32360, -71252, -64972, -24529, 22343,
    49091, 44631, 16787, -15224, -33279, -30083, -11244, 10126, 21968, 19697, 7297, -6510, -13983,
    -12404, -4544, 4005, 8491, 7430, 2682, -2327, -4853, -4171, -1477, 1255, 2560, 2147, 741, -611,
    -1207, -977, -324, 255, 478, 364, 112, -81, -135, -88, -21,
];

/// DSD → PCM for **playback**: a cascade that lands *every* DSD family on its native
/// 44.1 kHz. [`DecimateAdapter`] (stage 1, ÷8 bit-domain) → a second linear-phase ÷8
/// PCM FIR (stage 2) already put DSD64 (`64 × 44100`) exactly on 44.1 kHz. Stage 2
/// reuses the [`COEFFS`] prototype — its cutoff `f_in/16` is, for an 8:1 drop at any
/// input rate, precisely the output Nyquist, so the same proven filter anti-aliases it.
///
/// DSD128/256/512 come out of stage 2 at 88.2/176.4/352.8 kHz — a leftover factor
/// `DSD_multiple / 64` ∈ {2, 4, 8}. An **adaptive third stage** (the [`COEFFS_DIV2`] /
/// [`COEFFS_DIV4`] / [`COEFFS`] decimators, cutoff pinned to the 22.05 kHz output
/// Nyquist) removes exactly that, so the output is *always* 44.1 kHz and the deck never
/// has to resample an ultrasonic DSD rate in real time. DSD64's third-stage ratio is 1:
/// the stage is **bypassed**, so DSD64 output stays bit-for-bit identical to the
/// two-stage path the conformance vectors pin.
///
/// Why a separate type (not a flag on [`DecimateAdapter`]): the output *mode* is chosen
/// by construction (see the module header). `DecimateAdapter::new` stays the archival
/// 352.8 kHz path the conformance vectors pin; playback constructs this instead.
pub struct PlaybackDecimateAdapter<D> {
    /// First stage: the proven bit-domain DSD → 352.8 kHz decimator.
    stage1: DecimateAdapter<D>,
    channels: usize,
    /// Per-channel ring of the last [`TAPS`] stage-1 PCM samples; the next write
    /// goes to `pos` (so the newest sample is at `pos - 1`). Zero is PCM silence
    /// (stage-1 maps DSD silence to ≈0), so a zeroed ring is a clean warm-up seed.
    hist: [[i32; TAPS]; MAX_DECIMATE_CHANNELS],
    pos: usize,
    /// Stage-1 frames accumulated toward the next output (0..[`PCM_DECIMATION`]).
    phase: usize,
    /// Adaptive third-stage decimation ratio `DSD_multiple / 64` ∈ {1, 2, 4, 8}.
    /// `1` (DSD64) bypasses stage 3 entirely (bit-identical two-stage output).
    ratio3: usize,
    /// Stage-3 prototype for this ratio ([`COEFFS_DIV2`]/[`COEFFS_DIV4`]/[`COEFFS`]);
    /// unused when `ratio3 == 1`.
    coeffs3: &'static [i32; TAPS],
    /// Native output sample rate (44.1 kHz for every DSD family); from the DSD rate.
    out_rate: u32,
    /// Per-channel ring of the last [`TAPS`] stage-2 PCM samples feeding stage 3.
    hist3: [[i32; TAPS]; MAX_DECIMATE_CHANNELS],
    pos3: usize,
    /// Stage-2 frames accumulated toward the next stage-3 output (0..`ratio3`).
    phase3: usize,
    /// Buffered stage-1 output frames awaiting consumption (`pend_pos..pend_len`).
    pend: [i32; PEND_FRAMES * MAX_DECIMATE_CHANNELS],
    pend_len: usize,
    pend_pos: usize,
    eof: bool,
}

impl<D: DsdDecoder> PlaybackDecimateAdapter<D> {
    /// Wrap `inner` for playback. Per-channel history starts silent (zeroed). The
    /// adaptive third-stage ratio and the output rate are fixed here from the inner
    /// DSD rate: ratio `DSD_multiple / 64` ∈ {1, 2, 4, 8} and `out_rate` = 44.1 kHz.
    pub fn new(inner: D) -> Self {
        let si = inner.stream_info();
        let channels = si.channels as usize;
        // stage 2 lands DSD at `DSD_rate / 64`; stage 3 removes the remaining
        // `DSD_multiple / 64` so the output is the native 44.1 kHz for every family.
        let (ratio3, coeffs3, out_rate): (usize, &'static [i32; TAPS], u32) = match si.format {
            SampleFormat::Dsd { rate } => {
                let stage2_rate = rate.hz() / (DECIMATION * PCM_DECIMATION as u32);
                let r3 = (rate.hz() / 44_100 / 64).max(1) as usize;
                let coeffs: &'static [i32; TAPS] = match r3 {
                    2 => &COEFFS_DIV2,
                    4 => &COEFFS_DIV4,
                    _ => &COEFFS, // r3 == 8 reuses the 8:1 prototype; r3 == 1 is bypassed
                };
                (r3, coeffs, stage2_rate / r3 as u32)
            }
            // Defensive: PlaybackDecimateAdapter always wraps a DSD decoder.
            _ => (1, &COEFFS, si.sample_rate / PCM_DECIMATION as u32),
        };
        Self {
            stage1: DecimateAdapter::new(inner),
            channels,
            hist: [[0i32; TAPS]; MAX_DECIMATE_CHANNELS],
            pos: 0,
            phase: 0,
            ratio3,
            coeffs3,
            out_rate,
            hist3: [[0i32; TAPS]; MAX_DECIMATE_CHANNELS],
            pos3: 0,
            phase3: 0,
            pend: [0i32; PEND_FRAMES * MAX_DECIMATE_CHANNELS],
            pend_len: 0,
            pend_pos: 0,
            eof: false,
        }
    }

    /// Total output PCM frames, if the inner DSD decoder declares its length. All
    /// three stages divide the frame count, so this is the inner sample count over
    /// `DECIMATION × PCM_DECIMATION × ratio3` (= `DSD_multiple`, i.e. 64/128/256/512),
    /// which for every family is `inner_samples / DSD_multiple` = the 44.1 kHz frame
    /// count. Duration in seconds is unchanged (rate and frame count drop together),
    /// so the deck's progress/length stays correct.
    #[must_use]
    pub fn total_frames(&self) -> Option<u64> {
        self.stage1
            .total_frames()
            .map(|f| f / (PCM_DECIMATION as u64 * self.ratio3 as u64))
    }

    /// Reset the second- and third-stage filter state (after a seek).
    fn reset_filters(&mut self) {
        self.hist = [[0i32; TAPS]; MAX_DECIMATE_CHANNELS];
        self.pos = 0;
        self.phase = 0;
        self.hist3 = [[0i32; TAPS]; MAX_DECIMATE_CHANNELS];
        self.pos3 = 0;
        self.phase3 = 0;
        self.pend_len = 0;
        self.pend_pos = 0;
        self.eof = false;
    }

    /// Linear-phase FIR dot product over a per-channel ring: `Σ coeffs[k]·hist[age k]`
    /// (newest sample at `pos - 1`), then the DC-normalising divide by [`PCM_MAX`] (the
    /// coefficient sum) back to 24-bit scale, rounded to nearest and saturated. `i64`
    /// accumulation covers the 128-tap × 24-bit × ~2²⁰-coefficient products without
    /// overflow. Shared by stage 2 ([`COEFFS`]) and the adaptive stage 3.
    #[inline]
    fn fir_at(hist: &[i32; TAPS], pos: usize, coeffs: &[i32; TAPS]) -> i32 {
        let mut acc = 0i64;
        let mut k = 0usize;
        while k < TAPS {
            let idx = (pos + TAPS - 1 - k) % TAPS;
            acc += i64::from(coeffs[k]) * i64::from(hist[idx]);
            k += 1;
        }
        let div = i64::from(PCM_MAX);
        let half = div / 2;
        let q = if acc >= 0 {
            (acc + half) / div
        } else {
            (acc - half) / div
        };
        (q as i32).clamp(PCM_MIN, PCM_MAX)
    }
}

impl<D: DsdDecoder + DsdSeek> PlaybackDecimateAdapter<D> {
    /// Seek to output PCM `frame`. One 44.1 kHz output frame spans
    /// `PCM_DECIMATION × ratio3` stage-1 frames (= `DSD_multiple / 8`); the filter
    /// state is reset (brief warm-up transient after the seek). Using the full
    /// adaptive span is what keeps the seek frame-mapping correct for DSD128/256/512.
    ///
    /// # Errors
    /// Propagates the inner [`DecimateAdapter::seek_to_frame`] error.
    pub fn seek_to_frame(&mut self, frame: u64) -> Result<u64, DecodeError> {
        let span = PCM_DECIMATION as u64 * self.ratio3 as u64;
        let landed = self.stage1.seek_to_frame(frame.saturating_mul(span))?;
        self.reset_filters();
        Ok(landed / span)
    }

    /// Seek to `secs` seconds (convenience over [`Self::seek_to_frame`]).
    ///
    /// # Errors
    /// Propagates the inner seek error.
    pub fn seek_to_time(&mut self, secs: f64) -> Result<u64, DecodeError> {
        self.seek_to_frame(secs_to_samples(secs, self.stream_info().sample_rate))
    }
}

impl<D: DsdDecoder> Decoder for PlaybackDecimateAdapter<D> {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: self.out_rate,
            channels: self.channels as u16,
            format: SampleFormat::Int { bits: 24 },
        }
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        let c = self.channels;
        if c == 0 || c > MAX_DECIMATE_CHANNELS {
            return Err(DecodeError::Unsupported);
        }
        let mut w = 0;
        while w + c <= out.len() {
            if self.pend_pos >= self.pend_len {
                if self.eof {
                    break;
                }
                // Disjoint-field borrow: `stage1` and `pend` are distinct fields.
                let n = self.stage1.decode_into(&mut self.pend[..PEND_FRAMES * c])?;
                self.pend_len = n / c;
                self.pend_pos = 0;
                if self.pend_len == 0 {
                    self.eof = true;
                    break;
                }
            }
            // Push one stage-1 frame into the per-channel stage-2 rings.
            let base = self.pend_pos * c;
            let mut ch = 0;
            while ch < c {
                self.hist[ch][self.pos] = self.pend[base + ch];
                ch += 1;
            }
            self.pend_pos += 1;
            self.pos = (self.pos + 1) % TAPS;
            self.phase += 1;
            // Stage 2 emits one frame every `PCM_DECIMATION` stage-1 inputs.
            if self.phase != PCM_DECIMATION {
                continue;
            }
            self.phase = 0;
            if self.ratio3 == 1 {
                // DSD64: no third stage — emit stage 2 directly (bit-identical path).
                let mut ch = 0;
                while ch < c {
                    out[w] = Self::fir_at(&self.hist[ch], self.pos, &COEFFS);
                    w += 1;
                    ch += 1;
                }
                continue;
            }
            // DSD128/256/512: feed each stage-2 frame into the stage-3 rings and
            // decimate by `ratio3` to land on 44.1 kHz.
            let mut ch = 0;
            while ch < c {
                self.hist3[ch][self.pos3] = Self::fir_at(&self.hist[ch], self.pos, &COEFFS);
                ch += 1;
            }
            self.pos3 = (self.pos3 + 1) % TAPS;
            self.phase3 += 1;
            if self.phase3 == self.ratio3 {
                self.phase3 = 0;
                let mut ch = 0;
                while ch < c {
                    out[w] = Self::fir_at(&self.hist3[ch], self.pos3, self.coeffs3);
                    w += 1;
                    ch += 1;
                }
            }
        }
        Ok(w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DsdRate, StreamInfo};

    /// A fake DSD decoder that hands out a fixed byte sequence.
    struct FakeDsd {
        bytes: &'static [u8],
        pos: usize,
        rate: DsdRate,
        channels: u16,
    }

    impl DsdDecoder for FakeDsd {
        fn stream_info(&self) -> StreamInfo {
            StreamInfo {
                sample_rate: 44_100,
                channels: self.channels,
                format: SampleFormat::Dsd { rate: self.rate },
            }
        }
        fn read_dsd(&mut self, out: &mut [u8]) -> Result<usize, DecodeError> {
            let rem = &self.bytes[self.pos..];
            let n = rem.len().min(out.len());
            out[..n].copy_from_slice(&rem[..n]);
            self.pos += n;
            Ok(n)
        }
    }

    impl DsdSeek for FakeDsd {
        fn seek_to_sample(&mut self, s: u64) -> Result<u64, DecodeError> {
            // DoP frame f starts at byte f * 2 * channels in the interleaved stream.
            self.pos = ((s / 16) * 2 * u64::from(self.channels)) as usize;
            Ok(s)
        }
    }

    #[test]
    fn decimate_total_frames_is_decimated_sample_count() {
        // A DSD decoder that declares its per-channel sample count -> the
        // decimator reports total_samples / DECIMATION as the PCM duration.
        // This is what gives a DSF track its now-playing length.
        struct Sized(Option<u64>);
        impl DsdDecoder for Sized {
            fn stream_info(&self) -> StreamInfo {
                StreamInfo {
                    sample_rate: 44_100,
                    channels: 2,
                    format: SampleFormat::Dsd {
                        rate: DsdRate::Dsd64,
                    },
                }
            }
            fn read_dsd(&mut self, _out: &mut [u8]) -> Result<usize, DecodeError> {
                Ok(0)
            }
            fn total_samples(&self) -> Option<u64> {
                self.0
            }
        }
        // 5_644_800 DSD samples / DECIMATION(8) = 705_600 PCM frames.
        assert_eq!(
            DecimateAdapter::new(Sized(Some(5_644_800))).total_frames(),
            Some(705_600)
        );
        // A length-unknown source stays None.
        assert_eq!(DecimateAdapter::new(Sized(None)).total_frames(), None);
    }

    #[test]
    fn decimate_silence_starts_near_zero_no_startup_pop() {
        // A DSD stream of 0xAA (alternating bits) is silence — the low-pass FIR
        // attenuates that Nyquist pattern to ~0. With the history seeded to
        // silence, output starts near 0. The old zero-seed (0x00 = eight -1
        // bits = full-scale negative) made the FIRST sample a large negative DC:
        // an audible startup/seek pop.
        struct Silence;
        impl DsdDecoder for Silence {
            fn stream_info(&self) -> StreamInfo {
                StreamInfo {
                    sample_rate: 44_100,
                    channels: 2,
                    format: SampleFormat::Dsd {
                        rate: DsdRate::Dsd64,
                    },
                }
            }
            fn read_dsd(&mut self, out: &mut [u8]) -> Result<usize, DecodeError> {
                out.fill(0xAA);
                Ok(out.len())
            }
        }
        let mut a = DecimateAdapter::new(Silence);
        let mut out = [0i32; 64];
        let n = a.decode_into(&mut out).unwrap();
        assert!(n > 0);
        // Near silence throughout, and critically the FIRST sample doesn't pop.
        // (Pre-fix, out[0] was ~ -0.6 * 2^23.) Threshold is a small fraction of
        // 24-bit full scale (2^23 = 8_388_608).
        let peak = out[..n].iter().map(|x| x.unsigned_abs()).max().unwrap();
        assert!(
            peak < 100_000,
            "silence should decode near 0, got peak {peak}"
        );
        assert!(
            out[0].unsigned_abs() < 100_000,
            "first sample must not pop, got {}",
            out[0]
        );
    }

    #[test]
    fn dop_mono_toggles_marker_per_frame() {
        let fake = FakeDsd {
            bytes: &[0xAA, 0xBB, 0xCC, 0xDD],
            pos: 0,
            rate: DsdRate::Dsd64,
            channels: 1,
        };
        let mut dop = DopAdapter::new(fake);
        let mut out = [0i32; 4];
        let n = dop.decode_into(&mut out).unwrap();
        assert_eq!(n, 2); // 4 DSD bytes → 2 mono frames
        assert_eq!(out[0], 0x0005_AABB); // frame 0, marker 0x05
        assert_eq!(out[1], 0x00FA_CCDD); // frame 1, marker 0xFA
    }

    #[test]
    fn dop_stereo_shares_marker_across_channels_and_deinterleaves() {
        // Interleaved input: [c0t0, c1t0, c0t1, c1t1] per frame.
        let fake = FakeDsd {
            bytes: &[0xA0, 0xB0, 0xA1, 0xB1, 0xC0, 0xD0, 0xC1, 0xD1],
            pos: 0,
            rate: DsdRate::Dsd64,
            channels: 2,
        };
        let mut dop = DopAdapter::new(fake);
        let mut out = [0i32; 4];
        let n = dop.decode_into(&mut out).unwrap();
        assert_eq!(n, 4); // 2 stereo frames = 4 DoP words (L,R,L,R)
        // Frame 0 (marker 0x05, shared by L and R): L uses ch0's t0/t1, R uses ch1's.
        assert_eq!(out[0], 0x0005_A0A1); // L
        assert_eq!(out[1], 0x0005_B0B1); // R — SAME marker as L
        // Frame 1 (marker toggled to 0xFA, again shared by L and R).
        assert_eq!(out[2], 0x00FA_C0C1); // L
        assert_eq!(out[3], 0x00FA_D0D1); // R
    }

    #[test]
    fn dop_stream_info_is_pcm_at_bitrate_over_16() {
        let fake = FakeDsd {
            bytes: &[],
            pos: 0,
            rate: DsdRate::Dsd64,
            channels: 2,
        };
        let dop = DopAdapter::new(fake);
        let si = dop.stream_info();
        assert_eq!(si.sample_rate, 2_822_400 / 16); // 176_400
        assert_eq!(si.channels, 2);
        assert_eq!(si.format, SampleFormat::Int { bits: 24 });
    }

    #[test]
    fn dop_seek_re_derives_marker_phase() {
        let fake = FakeDsd {
            bytes: &[0xA0, 0xB0, 0xA1, 0xB1, 0xC0, 0xD0, 0xC1, 0xD1],
            pos: 0,
            rate: DsdRate::Dsd64,
            channels: 2,
        };
        let mut dop = DopAdapter::new(fake);
        // Seek to DoP frame 1 (odd) → marker phase must be 0xFA, not 0x05.
        assert_eq!(dop.seek_to_frame(1).unwrap(), 1);
        let mut out = [0i32; 2];
        assert_eq!(dop.decode_into(&mut out).unwrap(), 2);
        assert_eq!(out[0], 0x00FA_C0C1); // L, marker 0xFA (re-derived from index)
        assert_eq!(out[1], 0x00FA_D0D1); // R, same marker
    }

    // ---- DecimateAdapter (DSD → PCM decimation FIR) ----

    /// Stereo clustered frames: ch0 = `0xFF` (all-ones), ch1 = `0x00` (all-zeros),
    /// interleaved `[c0, c1, c0, c1, …]` for 16 time indices (32 bytes).
    const STEREO_FF_00: [u8; 32] = {
        let mut a = [0u8; 32];
        let mut i = 0;
        while i < 32 {
            a[i] = if i % 2 == 0 { 0xFF } else { 0x00 };
            i += 1;
        }
        a
    };

    fn decimate(bytes: &'static [u8], channels: u16, rate: DsdRate) -> DecimateAdapter<FakeDsd> {
        DecimateAdapter::new(FakeDsd {
            bytes,
            pos: 0,
            rate,
            channels,
        })
    }

    #[test]
    fn decimate_dc_all_ones_is_positive_full_scale() {
        // 16 input bytes → 16 PCM samples; history (8 bytes) settles by sample 8.
        let mut dec = decimate(&[0xFF; 16], 1, DsdRate::Dsd64);
        let mut out = [0i32; 16];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(n, 16);
        // Once the filter is settled a constant +1 input maps to +full-scale.
        assert_eq!(out[15], PCM_MAX);
        assert!(
            out[8] > PCM_MAX * 9 / 10,
            "settled sample not near +FS: {}",
            out[8]
        );
    }

    #[test]
    fn decimate_dc_all_zeros_is_negative_full_scale() {
        let mut dec = decimate(&[0x00; 16], 1, DsdRate::Dsd64);
        let mut out = [0i32; 16];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(n, 16);
        assert_eq!(out[15], -PCM_MAX);
        assert!(
            out[8] < -PCM_MAX * 9 / 10,
            "settled sample not near -FS: {}",
            out[8]
        );
    }

    #[test]
    fn decimate_rejects_nyquist_tone() {
        // 0x55 = 0b0101_0101 repeated → perfectly alternating bits = Nyquist,
        // which the low-pass rejects to ≈ 0.
        let mut dec = decimate(&[0x55; 32], 1, DsdRate::Dsd64);
        let mut out = [0i32; 32];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(n, 32);
        // Settled magnitude is tiny compared with full-scale (~8.4M).
        assert!(out[31].abs() < 1_000, "Nyquist not rejected: {}", out[31]);
    }

    #[test]
    fn decimate_ratio_is_eight_to_one() {
        // N bytes/channel → N*8/ratio = N PCM frames (ratio = 8, 8 bits/byte).
        let mut dec = decimate(&[0xF0; 20], 1, DsdRate::Dsd64);
        let mut out = [0i32; 32];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(n, 20);
    }

    #[test]
    fn decimate_stereo_separates_channels() {
        // ch0 all-ones → L positive; ch1 all-zeros → R negative.
        let mut dec = decimate(&STEREO_FF_00, 2, DsdRate::Dsd64);
        let mut out = [0i32; 32];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(n, 32); // 16 stereo frames = 32 interleaved samples
        // Frame 15 (indices 30 = L, 31 = R) is fully settled.
        assert_eq!(out[30], PCM_MAX); // L = +full-scale
        assert_eq!(out[31], -PCM_MAX); // R = -full-scale
        // Every settled frame keeps L positive and R negative.
        for f in 8..16 {
            assert!(out[2 * f] > 0, "L not positive at frame {f}");
            assert!(out[2 * f + 1] < 0, "R not negative at frame {f}");
        }
    }

    #[test]
    fn decimate_stream_info_is_pcm_at_bitrate_over_ratio() {
        let dec = decimate(&[], 2, DsdRate::Dsd64);
        let si = dec.stream_info();
        assert_eq!(si.sample_rate, DsdRate::Dsd64.hz() / 8); // 2_822_400 / 8 = 352_800
        assert_eq!(si.sample_rate, 352_800);
        assert_eq!(si.channels, 2);
        assert_eq!(si.format, SampleFormat::Int { bits: 24 });
    }

    #[test]
    fn decimate_rejects_too_many_channels() {
        let mut dec = decimate(
            &[0xFF; 32],
            (MAX_DECIMATE_CHANNELS + 1) as u16,
            DsdRate::Dsd64,
        );
        let mut out = [0i32; 32];
        assert_eq!(
            dec.decode_into(&mut out).unwrap_err(),
            DecodeError::Unsupported
        );
    }

    #[test]
    fn dop_and_decimate_seek_to_time_convert_seconds() {
        // DoP frame rate (DSD64) = 2_822_400 / 16 = 176_400.
        let fake = FakeDsd {
            bytes: &[0u8; 8],
            pos: 0,
            rate: DsdRate::Dsd64,
            channels: 2,
        };
        let mut dop = DopAdapter::new(fake);
        // 0.0001 s × 176_400 = 17.64 → DoP frame 18.
        assert_eq!(dop.seek_to_time(0.0001).unwrap(), 18);

        // Decimated rate (DSD64, 8:1) = 2_822_400 / 8 = 352_800.
        let mut dec = decimate(&[0u8; 8], 2, DsdRate::Dsd64);
        // 0.0001 s × 352_800 = 35.28 → PCM frame 35.
        assert_eq!(dec.seek_to_time(0.0001).unwrap(), 35);
    }

    #[test]
    fn adapter_seek_to_extreme_frame_saturates_without_overflow() {
        // `seek_to_frame` scales the frame index (×16 for DoP, ×DECIMATION for the
        // decimator) into a DSD sample index; an extreme target must saturate, not
        // overflow the multiply — a debug panic the robustness sweep caught.
        let mut dop = DopAdapter::new(FakeDsd {
            bytes: &STEREO_FF_00,
            pos: 0,
            rate: DsdRate::Dsd64,
            channels: 2,
        });
        assert!(dop.seek_to_frame(u64::MAX).is_ok());

        let mut dec = decimate(&STEREO_FF_00, 2, DsdRate::Dsd64);
        assert!(dec.seek_to_frame(u64::MAX).is_ok());
    }

    fn dec_decode_all(d: &mut DecimateAdapter<FakeDsd>) -> Vec<i32> {
        let mut out = Vec::new();
        let mut buf = [0i32; 64];
        loop {
            let n = d.decode_into(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    #[test]
    fn decimate_seek_round_trip_and_reset() {
        // Mono counting pattern: with DECIMATION = 8, one input byte yields one
        // output frame, so frame f lives at byte f. Even targets keep the mock's
        // DoP-granular (2-byte) seek exact.
        static PATTERN: [u8; 256] = {
            let mut a = [0u8; 256];
            let mut i = 0;
            while i < 256 {
                a[i] = i as u8;
                i += 1;
            }
            a
        };
        let reference = dec_decode_all(&mut decimate(&PATTERN, 1, DsdRate::Dsd64));

        // After the FIR warm-up (the filter spans TAPS = GROUPS output frames) a
        // seeked decode is bit-identical to the linear decode.
        for f in [0u64, 8, 20, 50] {
            let mut d = decimate(&PATTERN, 1, DsdRate::Dsd64);
            assert_eq!(d.seek_to_frame(f).unwrap(), f);
            let got = dec_decode_all(&mut d);
            assert_eq!(
                got[GROUPS..],
                reference[f as usize + GROUPS..],
                "decimate round-trip after warm-up, seek({f})"
            );
        }

        // Seeking fully resets the FIR history and ring position: a decoder driven
        // to completion then seeked back matches a fresh decoder seeked the same.
        let mut dirty = decimate(&PATTERN, 1, DsdRate::Dsd64);
        let _ = dec_decode_all(&mut dirty);
        dirty.seek_to_frame(20).unwrap();
        let after_dirty = dec_decode_all(&mut dirty);
        let mut fresh = decimate(&PATTERN, 1, DsdRate::Dsd64);
        fresh.seek_to_frame(20).unwrap();
        assert_eq!(
            after_dirty,
            dec_decode_all(&mut fresh),
            "seek resets FIR history"
        );
    }

    // ---- PlaybackDecimateAdapter (DSD → native 44.1 kHz, the playback path) ----

    fn playback(
        bytes: &'static [u8],
        channels: u16,
        rate: DsdRate,
    ) -> PlaybackDecimateAdapter<FakeDsd> {
        PlaybackDecimateAdapter::new(FakeDsd {
            bytes,
            pos: 0,
            rate,
            channels,
        })
    }

    fn pb_decode_all(d: &mut PlaybackDecimateAdapter<FakeDsd>) -> Vec<i32> {
        let mut out = Vec::new();
        let mut buf = [0i32; 64];
        loop {
            let n = d.decode_into(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    #[test]
    fn playback_decimate_stream_info_is_native_44k() {
        // DSD64 = 64 × 44100. Two 8:1 stages (64:1 total) land exactly on 44.1 kHz,
        // DSD64's native PCM rate — so a 44.1 kHz DAC needs no resampling at all.
        let d = playback(&[], 2, DsdRate::Dsd64);
        let si = d.stream_info();
        assert_eq!(si.sample_rate, 44_100);
        assert_eq!(si.channels, 2);
        assert_eq!(si.format, SampleFormat::Int { bits: 24 });
    }

    #[test]
    fn playback_decimate_total_frames_is_sixty_fourth() {
        // Both stages divide the frame count by 8 → 64:1 overall. Duration in
        // seconds is unchanged (rate drops by 64 too), so the length stays right.
        struct Sized(Option<u64>);
        impl DsdDecoder for Sized {
            fn stream_info(&self) -> StreamInfo {
                StreamInfo {
                    sample_rate: 44_100,
                    channels: 2,
                    format: SampleFormat::Dsd {
                        rate: DsdRate::Dsd64,
                    },
                }
            }
            fn read_dsd(&mut self, _out: &mut [u8]) -> Result<usize, DecodeError> {
                Ok(0)
            }
            fn total_samples(&self) -> Option<u64> {
                self.0
            }
        }
        // 2_822_400 DSD samples / 64 = 44_100 PCM frames (= 1 s at 44.1 kHz).
        assert_eq!(
            PlaybackDecimateAdapter::new(Sized(Some(2_822_400))).total_frames(),
            Some(44_100)
        );
        assert_eq!(
            PlaybackDecimateAdapter::new(Sized(None)).total_frames(),
            None
        );
    }

    #[test]
    fn playback_decimate_dc_all_ones_is_positive_full_scale() {
        // A constant +1 DSD input is +full-scale after stage 1; the second stage's
        // coefficients sum to PCM_MAX (unity DC gain), so the settled output is
        // +PCM_MAX too — the cascade preserves level, no droop, no overshoot.
        let bytes: &'static [u8] = vec![0xFFu8; 2048].leak();
        let out = pb_decode_all(&mut playback(bytes, 1, DsdRate::Dsd64));
        assert_eq!(out.len(), 2048 / 8); // 64:1 after the 8:1 stage-1 (1 byte→1 frame)
        // Fully settled once both filters' histories fill (< ~160 output frames).
        assert_eq!(*out.last().unwrap(), PCM_MAX);
        assert!(
            out[200] > PCM_MAX * 9 / 10,
            "settled DC not near +FS: {}",
            out[200]
        );
    }

    #[test]
    fn playback_decimate_all_zeros_is_negative_full_scale() {
        let bytes: &'static [u8] = vec![0x00u8; 2048].leak();
        let out = pb_decode_all(&mut playback(bytes, 1, DsdRate::Dsd64));
        assert_eq!(*out.last().unwrap(), -PCM_MAX);
        assert!(out[200] < -PCM_MAX * 9 / 10);
    }

    #[test]
    fn playback_decimate_rejects_nyquist_tone() {
        // 0x55 = alternating bits = DSD Nyquist → stage 1 ≈ 0 → stage 2 ≈ 0.
        let bytes: &'static [u8] = vec![0x55u8; 2048].leak();
        let out = pb_decode_all(&mut playback(bytes, 1, DsdRate::Dsd64));
        assert!(
            out.last().unwrap().abs() < 1_000,
            "Nyquist not rejected: {}",
            out.last().unwrap()
        );
    }

    #[test]
    fn playback_decimate_ratio_is_sixty_four_to_one() {
        // 512 input bytes → 512 stage-1 frames → 512/8 = 64 output frames.
        let bytes: &'static [u8] = vec![0xF0u8; 512].leak();
        let out = pb_decode_all(&mut playback(bytes, 1, DsdRate::Dsd64));
        assert_eq!(out.len(), 64);
    }

    #[test]
    fn playback_decimate_stereo_separates_channels() {
        // ch0 all-ones (+FS), ch1 all-zeros (−FS), interleaved; both stages are
        // per-channel so the separation survives the cascade.
        let mut bytes = vec![0u8; 4096];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = if i % 2 == 0 { 0xFF } else { 0x00 };
        }
        let out = pb_decode_all(&mut playback(bytes.leak(), 2, DsdRate::Dsd64));
        let last = out.len() - 2;
        assert_eq!(out[last], PCM_MAX); // L
        assert_eq!(out[last + 1], -PCM_MAX); // R
    }

    #[test]
    fn playback_decimate_seek_to_time_converts_seconds() {
        // Native rate 44.1 kHz: 0.0001 s × 44_100 = 4.41 → PCM frame 4.
        let mut d = playback(&[0u8; 8], 2, DsdRate::Dsd64);
        assert_eq!(d.seek_to_time(0.0001).unwrap(), 4);
    }

    #[test]
    fn playback_decimate_seek_to_extreme_frame_saturates() {
        let mut d = playback(&STEREO_FF_00, 2, DsdRate::Dsd64);
        assert!(d.seek_to_frame(u64::MAX).is_ok());
    }

    // ---- adaptive decimation: DSD128/256/512 also land on native 44.1 kHz ----

    #[test]
    fn playback_decimate_every_family_lands_on_44100() {
        // The adaptive third stage (ratio DSD_multiple/64 ∈ {1,2,4,8}) pulls every
        // DSD family onto 44.1 kHz — not just DSD64. Before adaptive decimation
        // DSD128/256/512 came out at 88.2/176.4/352.8 kHz.
        for rate in [
            DsdRate::Dsd64,
            DsdRate::Dsd128,
            DsdRate::Dsd256,
            DsdRate::Dsd512,
        ] {
            let d = playback(&[], 2, rate);
            let si = d.stream_info();
            assert_eq!(si.sample_rate, 44_100, "{rate:?} did not land on 44.1 kHz");
            assert_eq!(si.channels, 2);
            assert_eq!(si.format, SampleFormat::Int { bits: 24 });
        }
    }

    #[test]
    fn playback_decimate_adaptive_ratio_matches_dsd_multiple() {
        // Total decimation = DSD_multiple: 8 bytes/stage-2 frame × ratio3 stage-2
        // frames per output. Mono: DSD64 → 8 B/frame, DSD128 → 16, DSD256 → 32,
        // DSD512 → 64. A wrong ratio3 (or a missing third stage) changes the count.
        for (rate, bytes_per_out) in [
            (DsdRate::Dsd64, 8usize),
            (DsdRate::Dsd128, 16),
            (DsdRate::Dsd256, 32),
            (DsdRate::Dsd512, 64),
        ] {
            let frames = 40usize;
            let buf: &'static [u8] = vec![0xF0u8; bytes_per_out * frames].leak();
            let out = pb_decode_all(&mut playback(buf, 1, rate));
            assert_eq!(out.len(), frames, "{rate:?}: wrong adaptive ratio");
        }
    }

    #[test]
    fn playback_decimate_higher_families_preserve_dc_full_scale() {
        // Constant +1 DSD → +full-scale through all three stages: every stage's FIR
        // has unity DC gain (taps sum to PCM_MAX), so the adaptive stage 3 neither
        // droops nor overshoots the level for DSD128/256/512.
        for rate in [DsdRate::Dsd128, DsdRate::Dsd256, DsdRate::Dsd512] {
            // Plenty of bytes so all three FIR histories fully settle (stage 3 is the
            // slowest: 128 taps at the adaptive ratio).
            let buf: &'static [u8] = vec![0xFFu8; 64 * 512].leak();
            let out = pb_decode_all(&mut playback(buf, 1, rate));
            assert!(out.len() > 300, "{rate:?}: too few frames ({})", out.len());
            assert_eq!(
                *out.last().unwrap(),
                PCM_MAX,
                "{rate:?}: settled DC not +FS"
            );
            let near_end = out[out.len() - 50];
            assert!(
                near_end > PCM_MAX * 9 / 10,
                "{rate:?}: settled DC {near_end} not near +FS"
            );
        }
    }

    #[test]
    fn playback_decimate_dsd512_total_frames_and_seek_are_full_ratio() {
        // DSD512: total decimation 512:1 and a seek span of 512 DSD samples/output
        // frame. total_frames = inner_samples / 512; seek frame-mapping uses the full
        // ratio (a seek that forgot the third stage would land 8× early).
        struct Sized512(Option<u64>);
        impl DsdDecoder for Sized512 {
            fn stream_info(&self) -> StreamInfo {
                StreamInfo {
                    sample_rate: 44_100,
                    channels: 2,
                    format: SampleFormat::Dsd {
                        rate: DsdRate::Dsd512,
                    },
                }
            }
            fn read_dsd(&mut self, _out: &mut [u8]) -> Result<usize, DecodeError> {
                Ok(0)
            }
            fn total_samples(&self) -> Option<u64> {
                self.0
            }
        }
        impl DsdSeek for Sized512 {
            fn seek_to_sample(&mut self, s: u64) -> Result<u64, DecodeError> {
                Ok(s)
            }
        }
        // 22_579_200 DSD samples / 512 = 44_100 output frames (= 1 s at 44.1 kHz).
        let d = PlaybackDecimateAdapter::new(Sized512(Some(22_579_200)));
        assert_eq!(d.stream_info().sample_rate, 44_100);
        assert_eq!(d.total_frames(), Some(44_100));
        assert_eq!(
            PlaybackDecimateAdapter::new(Sized512(None)).total_frames(),
            None
        );
        // Seek to output frame 100 → inner sample 100·512 = 51_200; the mock lands
        // exactly, so the returned frame round-trips back to 100.
        let mut d = PlaybackDecimateAdapter::new(Sized512(Some(22_579_200)));
        assert_eq!(d.seek_to_frame(100).unwrap(), 100);
    }
}
