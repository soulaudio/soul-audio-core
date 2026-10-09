//! Windowed-sinc **sample-rate converter** (see the resampler design spec).
//! `no_std`, no-alloc, f32 internal, fixed-output-block API, continuously-variable
//! ratio.
//!
//! # Kernel
//!
//! One oversampled half of a Kaiser-windowed sinc prototype is walked from each
//! side of the output position (libsamplerate's layout): the coefficient index
//! advances by `L · min(1, out/in)` per input sample, so **downsampling
//! automatically lowers the cutoff and pulls in more taps** — the one const table
//! anti-aliases both directions without runtime filter design. The output is
//! scaled by `min(1, out/in)` to keep unity gain.
//!
//! # Streaming model
//!
//! Output count is fixed at construction (rubato's `SincFixedOut` model). Each
//! [`process`](Resampler::process) call fills exactly `output_frames` output
//! frames and consumes exactly [`input_frames_next`](Resampler::input_frames_next)
//! input frames — all absorbed into the internal history, so the caller never
//! re-sends a frame. The read cursor lags the newest input by one filter *wing*
//! (the group delay); that look-ahead is a one-time startup latency, so
//! steady-state input demand is ≈ `output_frames × in/out`.

const MAX_CHANNELS: usize = 8;
/// Per-channel history capacity (frames). Bounds the supported downsample factor
/// for a given tier; construction errors if the declared ratio range needs more.
const MAX_HISTORY: usize = 640;

// Generated Kaiser-windowed sinc half-prototypes and their oversampling/half-span:
// `FAST_SINC: [f32; FAST_HALF+1]`, `HQ_SINC: [f32; HQ_HALF+1]`, with `FAST_L`,
// `FAST_HALF`, `HQ_L`, `HQ_HALF`. Regenerate with `cargo xtask gen-resample-tables`.
include!("resample_tables.rs");

/// Quality tier — trades table size / CPU for stopband depth.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quality {
    /// Compact, embedded default (~90 dB class).
    Fast,
    /// Sharper, desktop / RAM-rich (~120 dB class).
    Hq,
}

impl Quality {
    /// Oversampling factor of the prototype table.
    fn l(self) -> usize {
        match self {
            Quality::Fast => FAST_L,
            Quality::Hq => HQ_L,
        }
    }
    /// Half-span of the prototype in table samples (= `L × zero-crossings`).
    fn half(self) -> usize {
        match self {
            Quality::Fast => FAST_HALF,
            Quality::Hq => HQ_HALF,
        }
    }
    /// Half the zero-crossing count (`half / L`) — the unity-ratio filter wing.
    fn z(self) -> usize {
        self.half() / self.l()
    }
}

/// Resampler construction / use errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResampleError {
    /// `channels` is 0 or exceeds the supported maximum (8).
    TooManyChannels,
    /// A sample rate was 0.
    BadRate,
    /// The declared ratio range implies more downsampling than the history can
    /// cover for this tier.
    RatioRangeTooWide,
    /// A `process` buffer length did not match the required frame count.
    BadBufferLen,
}

impl core::fmt::Display for ResampleError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            ResampleError::TooManyChannels => "unsupported channel count",
            ResampleError::BadRate => "invalid sample rate",
            ResampleError::RatioRangeTooWide => "conversion ratio range too wide",
            ResampleError::BadBufferLen => "wrong process buffer length",
        })
    }
}

impl core::error::Error for ResampleError {}

/// A streaming windowed-sinc resampler over interleaved `i32` PCM.
pub struct Resampler {
    channels: usize,
    quality: Quality,
    /// Filter wing in zero-crossings (unity-ratio); scaled by the ratio at use.
    z: usize,
    /// Active per-channel history depth (frames), ≤ [`MAX_HISTORY`].
    hist_depth: usize,
    output_frames: usize,
    /// Nominal input frames per output frame (`in_rate / out_rate`).
    nominal_step: f64,
    /// Current step (in/out) in effect.
    step: f64,
    /// Step to slew toward across the next block.
    target_step: f64,
    /// Whether the pending step change ramps across the block.
    ramp: bool,
    /// Max relative ratio deviation allowed at runtime (≥ 1.0).
    max_rel: f64,
    /// Fractional read position of the next output, relative to the next
    /// `process` call's `input[0]`.
    read: f64,
    /// Per-channel history: the last `hist_depth` input frames (newest last),
    /// zeros before the stream starts.
    hist: [[f32; MAX_HISTORY]; MAX_CHANNELS],
}

impl Resampler {
    /// Construct a resampler.
    ///
    /// - `channels`: 1..=8.
    /// - `in_rate` / `out_rate`: nominal source and sink rates (Hz).
    /// - `max_rel_ratio`: how far [`set_ratio`](Self::set_ratio) may deviate from
    ///   nominal, e.g. `1.1` for ±10 %. Values `< 1.0` are treated as `1.0`.
    /// - `output_frames`: fixed output-frame count per [`process`](Self::process).
    /// - `quality`: [`Quality`] tier.
    ///
    /// # Errors
    /// [`ResampleError::TooManyChannels`] / [`ResampleError::BadRate`] /
    /// [`ResampleError::RatioRangeTooWide`].
    pub fn new(
        channels: usize,
        in_rate: u32,
        out_rate: u32,
        max_rel_ratio: f64,
        output_frames: usize,
        quality: Quality,
    ) -> Result<Self, ResampleError> {
        if channels == 0 || channels > MAX_CHANNELS {
            return Err(ResampleError::TooManyChannels);
        }
        if in_rate == 0 || out_rate == 0 {
            return Err(ResampleError::BadRate);
        }
        let nominal_step = f64::from(in_rate) / f64::from(out_rate);
        let max_rel = max_rel_ratio.max(1.0);
        let z = quality.z();
        // Worst-case (most downsampling) step → largest wing → history depth.
        let s_max = nominal_step * max_rel;
        let wing_max = wing(z, s_max);
        let hist_depth = 2 * wing_max + 4;
        if hist_depth > MAX_HISTORY {
            return Err(ResampleError::RatioRangeTooWide);
        }
        Ok(Self {
            channels,
            quality,
            z,
            hist_depth,
            output_frames,
            nominal_step,
            step: nominal_step,
            target_step: nominal_step,
            ramp: false,
            max_rel,
            read: 0.0,
            hist: [[0.0; MAX_HISTORY]; MAX_CHANNELS],
        })
    }

    /// Fixed output-frame count per [`process`](Self::process) call.
    #[must_use]
    pub fn output_frames(&self) -> usize {
        self.output_frames
    }

    /// Number of input frames the caller must supply to the next
    /// [`process`](Self::process). Varies with the ratio.
    #[must_use]
    pub fn input_frames_next(&self) -> usize {
        let s = self.step.max(self.target_step);
        let last = self.read + (self.output_frames as f64 - 1.0) * s;
        // Rightmost input index the last output touches, +1 = frames needed.
        let idx = libm_floor(last) as isize + wing(self.z, s) as isize;
        (idx + 1).max(0) as usize
    }

    /// Group delay in output frames (≈ one filter wing at the nominal ratio).
    #[must_use]
    pub fn latency(&self) -> usize {
        wing(self.z, self.nominal_step)
    }

    /// Flush the filter state (history + read cursor) — call after a discontinuity
    /// such as a seek. The conversion ratio is preserved; the first outputs
    /// afterward are a short warm-up transient.
    pub fn reset(&mut self) {
        self.read = 0.0;
        self.hist = [[0.0; MAX_HISTORY]; MAX_CHANNELS];
    }

    /// Set the conversion ratio (`out_rate / in_rate` scale) for subsequent
    /// output. `ramp` slews it smoothly across the next block; the value is
    /// clamped to the range declared at construction.
    pub fn set_ratio(&mut self, out_over_in: f64, ramp: bool) {
        let nominal_ratio = 1.0 / self.nominal_step;
        let clamped = out_over_in.clamp(nominal_ratio / self.max_rel, nominal_ratio * self.max_rel);
        self.target_step = 1.0 / clamped;
        self.ramp = ramp;
    }

    /// Read input frame `gi` (relative to `input[0]`; negative reads history).
    fn at(&self, ch: usize, input: &[i32], gi: isize) -> f32 {
        if gi >= 0 {
            input[gi as usize * self.channels + ch] as f32
        } else if gi >= -(self.hist_depth as isize) {
            self.hist[ch][(self.hist_depth as isize + gi) as usize]
        } else {
            0.0
        }
    }

    /// Reconstruct one output frame for **all** channels at position `i0 + eta`
    /// under input/output step `step`, writing `acc[0..channels]`.
    ///
    /// The windowed-sinc weight `interp(sinc, fidx)` depends only on the tap
    /// position, not the channel, so it is computed **once per tap** and
    /// multiplied into every channel's accumulator — halving the dominant
    /// `interp` work for stereo (it was recomputed once per channel per tap).
    /// Each channel accumulates the same taps in the same order, so the result
    /// is bit-identical to the former per-channel kernel.
    fn kernel_multi(&self, input: &[i32], i0: isize, eta: f64, step: f64, acc: &mut [f32]) {
        let (sinc, l, half) = match self.quality {
            Quality::Fast => (&FAST_SINC[..], FAST_L, FAST_HALF),
            Quality::Hq => (&HQ_SINC[..], HQ_L, HQ_HALF),
        };
        let ratio_c = (1.0 / step).min(1.0); // out/in, capped at 1
        let inc = l as f64 * ratio_c;
        let half_f = half as f64;
        let ch = self.channels;
        for a in acc[..ch].iter_mut() {
            *a = 0.0;
        }
        // Left wing: input i0, i0-1, …; distances eta, eta+1, …
        let mut fidx = eta * inc;
        let mut di = i0;
        while fidx < half_f {
            let w = interp(sinc, fidx);
            for (c, a) in acc[..ch].iter_mut().enumerate() {
                *a += w * self.at(c, input, di);
            }
            fidx += inc;
            di -= 1;
        }
        // Right wing: input i0+1, i0+2, …; distances 1-eta, 2-eta, …
        let mut fidx = (1.0 - eta) * inc;
        let mut di = i0 + 1;
        while fidx < half_f {
            let w = interp(sinc, fidx);
            for (c, a) in acc[..ch].iter_mut().enumerate() {
                *a += w * self.at(c, input, di);
            }
            fidx += inc;
            di += 1;
        }
        let rc = ratio_c as f32;
        for a in acc[..ch].iter_mut() {
            *a *= rc;
        }
    }

    /// Resample: consume exactly `input_frames_next()` interleaved input frames,
    /// produce exactly `output_frames()` interleaved output frames.
    ///
    /// # Errors
    /// [`ResampleError::BadBufferLen`] if either slice length is not the exact
    /// required frame count × channels.
    pub fn process(&mut self, input: &[i32], output: &mut [i32]) -> Result<(), ResampleError> {
        let need = self.input_frames_next();
        if input.len() != need * self.channels || output.len() != self.output_frames * self.channels
        {
            return Err(ResampleError::BadBufferLen);
        }
        let of = self.output_frames;
        let mut pos = self.read;
        let mut acc = [0.0f32; MAX_CHANNELS];
        for o in 0..of {
            let s = if self.ramp {
                self.step + (self.target_step - self.step) * (o as f64 + 1.0) / of as f64
            } else {
                self.target_step
            };
            let i0 = libm_floor(pos) as isize;
            let eta = pos - libm_floor(pos);
            self.kernel_multi(input, i0, eta, s, &mut acc);
            for ch in 0..self.channels {
                output[o * self.channels + ch] = f32_to_i32(acc[ch]);
            }
            pos += s;
        }
        self.step = self.target_step;
        self.ramp = false;
        self.roll_history(input, need);
        self.read = pos - need as f64;
        Ok(())
    }

    fn roll_history(&mut self, input: &[i32], need: usize) {
        let mut tmp = [0.0f32; MAX_HISTORY];
        for ch in 0..self.channels {
            for (k, slot) in tmp.iter_mut().enumerate().take(self.hist_depth) {
                let gi = need as isize - self.hist_depth as isize + k as isize;
                *slot = self.at(ch, input, gi);
            }
            self.hist[ch][..self.hist_depth].copy_from_slice(&tmp[..self.hist_depth]);
        }
    }
}

/// Filter wing in input frames for step `s` (in/out): `ceil(z / min(1, out/in))`.
fn wing(z: usize, s: f64) -> usize {
    let ratio_c = (1.0 / s).min(1.0);
    libm_ceil(z as f64 / ratio_c) as usize
}

/// Linear interpolation into the half-sinc table at fractional index `fidx`
/// (`0 ≤ fidx < len-1`).
fn interp(sinc: &[f32], fidx: f64) -> f32 {
    let idx = fidx as usize;
    let f = (fidx - idx as f64) as f32;
    sinc[idx] + f * (sinc[idx + 1] - sinc[idx])
}

/// `floor` without `std` — exact for the finite, in-range positions used here.
fn libm_floor(x: f64) -> f64 {
    let t = x as i64 as f64;
    if x < t { t - 1.0 } else { t }
}

fn libm_ceil(x: f64) -> f64 {
    let t = x as i64 as f64;
    if x > t { t + 1.0 } else { t }
}

/// Round to nearest and clamp into i32 range.
fn f32_to_i32(x: f32) -> i32 {
    let r = if x >= 0.0 { x + 0.5 } else { x - 0.5 };
    if r >= 2_147_483_647.0 {
        i32::MAX
    } else if r <= -2_147_483_648.0 {
        i32::MIN
    } else {
        r as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate alloc;
    use alloc::vec;
    use alloc::vec::Vec;

    /// Drive `blocks` process calls, supplying input from `src(global_frame, ch)`.
    fn run(r: &mut Resampler, blocks: usize, mut src: impl FnMut(usize, usize) -> i32) -> Vec<i32> {
        let ch = r.channels;
        let mut out = Vec::new();
        let mut global = 0usize;
        let mut block = vec![0i32; r.output_frames() * ch];
        for _ in 0..blocks {
            let need = r.input_frames_next();
            let mut input = vec![0i32; need * ch];
            for f in 0..need {
                for c in 0..ch {
                    input[f * ch + c] = src(global + f, c);
                }
            }
            global += need;
            r.process(&input, &mut block).unwrap();
            out.extend_from_slice(&block);
        }
        out
    }

    fn sine(hz: f64, rate: u32, amp: f64) -> impl Fn(usize, usize) -> i32 {
        move |g: usize, _c: usize| {
            (amp * (2.0 * core::f64::consts::PI * hz * g as f64 / f64::from(rate)).sin()) as i32
        }
    }

    fn rms(xs: &[i32]) -> f64 {
        let s: f64 = xs.iter().map(|&x| f64::from(x) * f64::from(x)).sum();
        (s / xs.len() as f64).sqrt()
    }

    #[test]
    fn tables_have_expected_shape() {
        assert_eq!(FAST_SINC.len(), FAST_HALF + 1);
        assert_eq!(HQ_SINC.len(), HQ_HALF + 1);
        // Peak at the centre (t=0); tapers to ~0 at the window edge.
        assert!(FAST_SINC[0] > FAST_SINC[FAST_L]); // sinc[0] is the largest
        assert!(HQ_SINC[HQ_HALF].abs() < 1e-4);
    }

    #[test]
    fn dc_is_preserved_up_and_down() {
        for (inr, outr) in [(48_000, 48_000), (96_000, 44_100), (44_100, 96_000)] {
            let mut r = Resampler::new(2, inr, outr, 1.1, 128, Quality::Hq).unwrap();
            let out = run(&mut r, 16, |_g, _c| 1000);
            for &s in &out[out.len() - 128..] {
                assert!((s - 1000).abs() <= 2, "DC drift {s} for {inr}->{outr}");
            }
        }
    }

    #[test]
    fn unity_ratio_preserves_amplitude() {
        // A mid-band tone passes at ~unity gain (RMS preserved) at ratio 1.0.
        let mut r = Resampler::new(1, 48_000, 48_000, 1.1, 256, Quality::Hq).unwrap();
        let amp = 8_000_000.0;
        let out = run(&mut r, 8, sine(1_000.0, 48_000, amp));
        let steady = &out[512..];
        let expect = amp / core::f64::consts::SQRT_2; // sine RMS
        let got = rms(steady);
        assert!(
            (got / expect - 1.0).abs() < 0.02,
            "unity RMS {got} vs {expect}"
        );
    }

    #[test]
    fn steady_state_input_demand_matches_ratio() {
        for (inr, outr) in [(48_000, 48_000), (96_000, 48_000), (44_100, 48_000)] {
            let mut r = Resampler::new(1, inr, outr, 1.1, 512, Quality::Fast).unwrap();
            let _ = run(&mut r, 6, |_g, _c| 0);
            let step = f64::from(inr) / f64::from(outr);
            let expect = (512.0 * step).round() as usize;
            let got = r.input_frames_next();
            assert!(
                got.abs_diff(expect) <= 1,
                "steady demand {got} vs ~{expect} for {inr}->{outr}"
            );
        }
    }

    #[test]
    fn cross_block_continuity_no_boundary_glitch() {
        // A low-frequency sine through a non-unity ratio: the output first
        // difference stays bounded — a block-boundary skip/repeat would spike it.
        let mut r = Resampler::new(1, 44_100, 48_000, 1.1, 128, Quality::Hq).unwrap();
        let out = run(&mut r, 16, sine(100.0, 44_100, 5_000_000.0));
        let interior = &out[512..out.len() - 4];
        let maxdiff = interior
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .max()
            .unwrap();
        // A 100 Hz tone's natural output slope is amp·2π·100/48000 ≈ 65 k; a
        // block-boundary skip/repeat would spike far above that.
        assert!(maxdiff < 130_000, "continuity spike {maxdiff}");
    }

    #[test]
    fn rejects_bad_channels_rates_and_wide_range() {
        assert_eq!(
            Resampler::new(0, 48_000, 48_000, 1.0, 64, Quality::Fast).err(),
            Some(ResampleError::TooManyChannels)
        );
        assert_eq!(
            Resampler::new(2, 0, 48_000, 1.0, 64, Quality::Fast).err(),
            Some(ResampleError::BadRate)
        );
        // 384k -> 8k is a 48× downsample: exceeds the history budget.
        assert_eq!(
            Resampler::new(1, 384_000, 8_000, 1.0, 64, Quality::Hq).err(),
            Some(ResampleError::RatioRangeTooWide)
        );
    }

    #[test]
    fn process_rejects_wrong_buffer_lengths() {
        let mut r = Resampler::new(1, 48_000, 48_000, 1.1, 64, Quality::Fast).unwrap();
        let need = r.input_frames_next();
        assert_eq!(
            r.process(&vec![0i32; need - 1], &mut vec![0i32; 64]),
            Err(ResampleError::BadBufferLen)
        );
        assert_eq!(
            r.process(&vec![0i32; need], &mut vec![0i32; 63]),
            Err(ResampleError::BadBufferLen)
        );
    }

    #[test]
    fn set_ratio_changes_demand_and_clamps() {
        let mut r = Resampler::new(1, 48_000, 48_000, 1.2, 100, Quality::Fast).unwrap();
        let base = r.input_frames_next();
        // Play 20 % faster (out/in smaller ⇒ consume more input).
        r.set_ratio(1.0 / 1.2, false);
        let faster = r.input_frames_next();
        assert!(faster > base, "faster demand {faster} !> base {base}");
        // A 5× request is out of the ±20 % range → clamped to the same as 1.2.
        r.set_ratio(1.0 / 5.0, false);
        assert_eq!(r.input_frames_next(), faster, "ratio not clamped to range");
    }

    #[test]
    fn ramped_ratio_change_is_continuous() {
        // A low tone through a ramped speed-up mid-stream stays glitch-free.
        let mut r = Resampler::new(1, 48_000, 48_000, 1.5, 256, Quality::Hq).unwrap();
        let src = sine(120.0, 48_000, 5_000_000.0);
        let mut out = Vec::new();
        let mut global = 0usize;
        let mut block = vec![0i32; 256];
        for b in 0..10 {
            if b == 4 {
                r.set_ratio(1.0 / 1.3, true); // ramped
            }
            let need = r.input_frames_next();
            let input: Vec<i32> = (0..need).map(|f| src(global + f, 0)).collect();
            global += need;
            r.process(&input, &mut block).unwrap();
            out.extend_from_slice(&block);
        }
        let interior = &out[512..out.len() - 4];
        let maxdiff = interior
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .max()
            .unwrap();
        // 120 Hz natural slope ≈ 78 k even after 1.3× speed-up; a zipper glitch spikes.
        assert!(maxdiff < 160_000, "ramp discontinuity spike {maxdiff}");
    }

    #[test]
    fn tiny_output_blocks_stream_without_panic() {
        // output_frames = 1 exercises the input_frames_next edge (negative read /
        // idx near -1) that a large block never reaches. Must not panic.
        let mut r = Resampler::new(1, 44_100, 48_000, 1.1, 1, Quality::Fast).unwrap();
        let out = run(&mut r, 200, sine(200.0, 44_100, 4_000_000.0));
        assert_eq!(out.len(), 200);
        assert!(out[64..].iter().any(|&s| s != 0));
    }

    #[test]
    fn latency_scales_with_downsample() {
        // Unity: one wing of zero-crossings. Downsampling stretches the kernel, so
        // the group delay grows.
        let unity = Resampler::new(1, 48_000, 48_000, 1.0, 64, Quality::Fast).unwrap();
        assert_eq!(unity.latency(), 16); // Fast Z
        let down = Resampler::new(1, 96_000, 48_000, 1.0, 64, Quality::Fast).unwrap();
        assert!(down.latency() > unity.latency());
    }
}
