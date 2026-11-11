//! Streaming ITU-R BS.1770-4 integrated-loudness analyzer with no-alloc
//! histogram gating.
//!
//! Signal path per channel: normalize to `[-1, 1)`, two cascaded K-weighting
//! biquads (high-shelf + RLB high-pass, Direct-Form I), accumulate the squared
//! output into 100 ms sub-blocks. A 400 ms gating block (the last four
//! sub-blocks — 75 % overlap) yields one block loudness, histogrammed if it
//! clears the −70 LUFS absolute gate. Integration applies the relative
//! (−10 LU) gate over the histogram (libebur128's histogram mode), so memory is
//! O(1) in track length.

use core::f64::consts::PI;

use crate::math::{log10, powf, tan};

/// ReplayGain 2.0 reference level (EBU R128 is −23; RG2 uses −18 LUFS).
pub const REPLAYGAIN2_TARGET_LUFS: f32 = -18.0;

/// Max channels the fixed-size state supports (mono + stereo for now).
const MAX_CHANNELS: usize = 2;

const LUFS_OFFSET: f64 = -0.691;
const ABS_GATE_LUFS: f64 = -70.0;
const REL_GATE_OFFSET_LU: f64 = -10.0;
/// Canonical 24-bit full scale (`2^23`); matches `soul-audio-core`'s PCM.
const FULL_SCALE: f64 = 8_388_608.0;

// Block-loudness histogram: [-70.0, +5.0] LUFS at 0.1 LU.
const HIST_MIN: f64 = -70.0;
const HIST_STEP: f64 = 0.1;
const HIST_BINS: usize = 751;

/// Returned by [`R128Analyzer::integrated_lufs`] when nothing cleared the gate.
pub const NO_LOUDNESS: f32 = f32::NEG_INFINITY;

#[derive(Clone, Copy, Default)]
struct Coeffs {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

#[derive(Clone, Copy, Default)]
struct State {
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl State {
    #[inline]
    fn process(&mut self, c: &Coeffs, x: f64) -> f64 {
        let y = c.b0 * x + c.b1 * self.x1 + c.b2 * self.x2 - c.a1 * self.y1 - c.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// Stage-1 K-weighting high-shelf biquad for `fs`, via the bilinear transform
/// (libebur128 / BS.1770 reference constants).
fn stage1(fs: f64) -> Coeffs {
    const F0: f64 = 1_681.974_450_955_532;
    const GAIN_DB: f64 = 3.999_843_853_97;
    // The BS.1770 high-shelf Q; close to but not 1/√2 (clippy's approx_constant).
    #[allow(clippy::approx_constant)]
    const Q: f64 = 0.707_175_236_955_419_3;
    let k = tan(PI * F0 / fs);
    let vh = powf(10.0, GAIN_DB / 20.0);
    let vb = powf(vh, 0.499_666_774_154_541_6);
    let a0 = 1.0 + k / Q + k * k;
    Coeffs {
        b0: (vh + vb * k / Q + k * k) / a0,
        b1: 2.0 * (k * k - vh) / a0,
        b2: (vh - vb * k / Q + k * k) / a0,
        a1: 2.0 * (k * k - 1.0) / a0,
        a2: (1.0 - k / Q + k * k) / a0,
    }
}

/// Stage-2 K-weighting RLB high-pass biquad for `fs`.
fn stage2(fs: f64) -> Coeffs {
    const F0: f64 = 38.135_470_876_024_44;
    const Q: f64 = 0.500_327_037_323_877_3;
    let k = tan(PI * F0 / fs);
    let a0 = 1.0 + k / Q + k * k;
    Coeffs {
        b0: 1.0,
        b1: -2.0,
        b2: 1.0,
        a1: 2.0 * (k * k - 1.0) / a0,
        a2: (1.0 - k / Q + k * k) / a0,
    }
}

/// BS.1770 per-channel summation weight (`Ls`/`Rs` get +1.5 dB; here mono/stereo
/// are all 1.0).
#[inline]
fn channel_weight(ch: usize) -> f64 {
    match ch {
        3 | 4 => 1.412_537_544_622_754_4,
        _ => 1.0,
    }
}

/// A streaming BS.1770-4 integrated-loudness + sample-peak analyzer.
///
/// Feed interleaved canonical-24-bit `i32` PCM (`len` a multiple of
/// `channels`); read back [`integrated_lufs`](Self::integrated_lufs),
/// [`sample_peak_dbfs`](Self::sample_peak_dbfs), and
/// [`replay_gain_db`](Self::replay_gain_db).
pub struct R128Analyzer {
    channels: usize,
    c1: Coeffs,
    c2: Coeffs,
    s1: [State; MAX_CHANNELS],
    s2: [State; MAX_CHANNELS],
    samples_per_subblock: u32,
    subblock_filled: u32,
    sub_sumsq: [f64; MAX_CHANNELS],
    ring: [[f64; MAX_CHANNELS]; 4],
    ring_pos: usize,
    ring_count: usize,
    hist: [u32; HIST_BINS],
    sample_peak: f64,
}

impl R128Analyzer {
    /// Build an analyzer for `sample_rate` Hz and `channels` (1..=2).
    pub fn new(sample_rate: u32, channels: u8) -> Self {
        let channels = (channels as usize).clamp(1, MAX_CHANNELS);
        let fs = f64::from(sample_rate);
        R128Analyzer {
            channels,
            c1: stage1(fs),
            c2: stage2(fs),
            s1: [State::default(); MAX_CHANNELS],
            s2: [State::default(); MAX_CHANNELS],
            samples_per_subblock: (sample_rate / 10).max(1),
            subblock_filled: 0,
            sub_sumsq: [0.0; MAX_CHANNELS],
            ring: [[0.0; MAX_CHANNELS]; 4],
            ring_pos: 0,
            ring_count: 0,
            hist: [0; HIST_BINS],
            sample_peak: 0.0,
        }
    }

    /// Feed interleaved canonical-24-bit PCM. `interleaved.len()` should be a
    /// whole number of frames (`channels` samples each); a trailing partial
    /// frame is ignored.
    pub fn feed(&mut self, interleaved: &[i32]) {
        let ch = self.channels;
        for frame in interleaved.chunks_exact(ch) {
            for (c, &sample) in frame.iter().enumerate() {
                let x = f64::from(sample) / FULL_SCALE;
                let ax = x.abs();
                if ax > self.sample_peak {
                    self.sample_peak = ax;
                }
                let y1 = self.s1[c].process(&self.c1, x);
                let y = self.s2[c].process(&self.c2, y1);
                self.sub_sumsq[c] += y * y;
            }
            self.subblock_filled += 1;
            if self.subblock_filled >= self.samples_per_subblock {
                self.close_subblock();
            }
        }
    }

    /// Roll the current 100 ms sub-block into the 400 ms ring; emit one gating
    /// block once four sub-blocks are present (75 % overlap).
    fn close_subblock(&mut self) {
        self.ring[self.ring_pos] = self.sub_sumsq;
        self.sub_sumsq = [0.0; MAX_CHANNELS];
        self.ring_pos = (self.ring_pos + 1) % 4;
        self.subblock_filled = 0;
        if self.ring_count < 4 {
            self.ring_count += 1;
        }
        if self.ring_count < 4 {
            return;
        }
        let n = f64::from(self.samples_per_subblock) * 4.0;
        let mut z = 0.0;
        for c in 0..self.channels {
            let s = self.ring[0][c] + self.ring[1][c] + self.ring[2][c] + self.ring[3][c];
            z += channel_weight(c) * (s / n);
        }
        if z > 0.0 {
            let l = LUFS_OFFSET + 10.0 * log10(z);
            if l >= ABS_GATE_LUFS {
                let idx = ((l - HIST_MIN) / HIST_STEP + 0.5) as i64;
                let idx = idx.clamp(0, (HIST_BINS - 1) as i64) as usize;
                self.hist[idx] += 1;
            }
        }
    }

    /// Integrated loudness (LUFS) over everything fed, or [`NO_LOUDNESS`] if no
    /// gating block cleared the absolute/relative gate.
    pub fn integrated_lufs(&self) -> f32 {
        // Pass 1: mean block energy over the absolute-gated blocks (= all
        // histogrammed blocks), to derive the relative threshold.
        let (count, sum_z) = self.accumulate(ABS_GATE_LUFS);
        if count == 0 {
            return NO_LOUDNESS;
        }
        let mean_z_abs = sum_z / count as f64;
        let rel_threshold = LUFS_OFFSET + 10.0 * log10(mean_z_abs) + REL_GATE_OFFSET_LU;
        // Pass 2: mean block energy over blocks at/above the relative gate.
        let (count2, sum_z2) = self.accumulate(rel_threshold);
        if count2 == 0 {
            return NO_LOUDNESS;
        }
        (LUFS_OFFSET + 10.0 * log10(sum_z2 / count2 as f64)) as f32
    }

    /// Sum block count and reconstructed block energy over histogram bins whose
    /// loudness is `>= threshold_lufs`.
    fn accumulate(&self, threshold_lufs: f64) -> (u64, f64) {
        let mut count = 0u64;
        let mut sum_z = 0.0;
        for (i, &cnt) in self.hist.iter().enumerate() {
            if cnt == 0 {
                continue;
            }
            let l = HIST_MIN + i as f64 * HIST_STEP;
            if l < threshold_lufs {
                continue;
            }
            count += u64::from(cnt);
            // Invert l = -0.691 + 10·log10(z): z = 10^((l - LUFS_OFFSET)/10).
            sum_z += f64::from(cnt) * powf(10.0, (l - LUFS_OFFSET) / 10.0);
        }
        (count, sum_z)
    }

    /// Peak sample magnitude in dBFS (ReplayGain-1.0-style sample peak; a true
    /// inter-sample peak is a follow-up). `-inf` if silent.
    pub fn sample_peak_dbfs(&self) -> f32 {
        if self.sample_peak <= 0.0 {
            f32::NEG_INFINITY
        } else {
            (20.0 * log10(self.sample_peak)) as f32
        }
    }

    /// ReplayGain (dB) to reach `target_lufs` (e.g.
    /// [`REPLAYGAIN2_TARGET_LUFS`]). `0.0` if loudness is undefined.
    pub fn replay_gain_db(&self, target_lufs: f32) -> f32 {
        let i = self.integrated_lufs();
        if i.is_finite() { target_lufs - i } else { 0.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI as PI64;
    use std::vec::Vec;

    /// Synthesize `secs` of a `freq`-Hz sine at `peak` (normalized full scale)
    /// in `channels`, as canonical-24-bit interleaved `i32`.
    fn sine(freq: f64, peak: f64, secs: f64, fs: u32, channels: u8) -> Vec<i32> {
        let n = (secs * f64::from(fs)) as usize;
        let mut out = Vec::with_capacity(n * channels as usize);
        for i in 0..n {
            let t = i as f64 / f64::from(fs);
            let s = (peak * (2.0 * PI64 * freq * t).sin() * (FULL_SCALE - 1.0)).round() as i32;
            for _ in 0..channels {
                out.push(s);
            }
        }
        out
    }

    /// Peak amplitude (normalized) of a sine at `dbfs` (0 dBFS = full-scale sine).
    fn sine_peak_at_dbfs(dbfs: f64) -> f64 {
        powf(10.0, dbfs / 20.0)
    }

    #[test]
    fn stereo_1khz_minus_23_dbfs_reads_minus_23_lufs() {
        let mut a = R128Analyzer::new(48_000, 2);
        a.feed(&sine(1000.0, sine_peak_at_dbfs(-23.0), 4.0, 48_000, 2));
        let l = a.integrated_lufs();
        assert!(
            (l - (-23.0)).abs() <= 0.1,
            "integrated={l} LUFS, want -23.0 ±0.1"
        );
    }

    #[test]
    fn linearity_minus_20_dbfs_reads_minus_20_lufs() {
        let mut a = R128Analyzer::new(48_000, 2);
        a.feed(&sine(1000.0, sine_peak_at_dbfs(-20.0), 4.0, 48_000, 2));
        let l = a.integrated_lufs();
        assert!(
            (l - (-20.0)).abs() <= 0.1,
            "integrated={l} LUFS, want -20.0 ±0.1"
        );
    }

    #[test]
    fn silence_is_below_gate() {
        let mut a = R128Analyzer::new(48_000, 2);
        a.feed(&sine(1000.0, 0.0, 2.0, 48_000, 2));
        assert_eq!(a.integrated_lufs(), NO_LOUDNESS);
    }

    #[test]
    fn plus_6db_raises_loudness_by_6lu() {
        let mut lo = R128Analyzer::new(48_000, 2);
        lo.feed(&sine(1000.0, sine_peak_at_dbfs(-30.0), 4.0, 48_000, 2));
        let mut hi = R128Analyzer::new(48_000, 2);
        hi.feed(&sine(1000.0, sine_peak_at_dbfs(-24.0), 4.0, 48_000, 2));
        let d = (hi.integrated_lufs() - lo.integrated_lufs()) as f64;
        assert!((d - 6.0).abs() <= 0.05, "delta={d} LU, want +6.0");
    }

    #[test]
    fn duplicating_mono_to_stereo_adds_3lu() {
        let mut m = R128Analyzer::new(48_000, 1);
        m.feed(&sine(1000.0, sine_peak_at_dbfs(-23.0), 4.0, 48_000, 1));
        let mut s = R128Analyzer::new(48_000, 2);
        s.feed(&sine(1000.0, sine_peak_at_dbfs(-23.0), 4.0, 48_000, 2));
        let d = (s.integrated_lufs() - m.integrated_lufs()) as f64;
        assert!((d - 3.0103).abs() <= 0.05, "stereo-mono={d} LU, want +3.01");
    }

    #[test]
    fn replay_gain_targets_minus_18() {
        let mut a = R128Analyzer::new(48_000, 2);
        a.feed(&sine(1000.0, sine_peak_at_dbfs(-23.0), 4.0, 48_000, 2));
        let g = a.replay_gain_db(REPLAYGAIN2_TARGET_LUFS);
        // -18 target, source ~-23 → +5 dB.
        assert!((g - 5.0).abs() <= 0.15, "gain={g} dB, want ~+5.0");
    }

    #[test]
    fn sample_peak_matches_input() {
        let mut a = R128Analyzer::new(48_000, 2);
        a.feed(&sine(1000.0, sine_peak_at_dbfs(-6.0), 1.0, 48_000, 2));
        let p = a.sample_peak_dbfs();
        assert!((p - (-6.0)).abs() <= 0.05, "peak={p} dBFS, want -6.0");
    }

    /// Synthesize a continuous-phase sine whose level steps through `segments`
    /// (`(dbfs, seconds)`); `dbfs <= -200` is silence. Continuous phase across
    /// boundaries avoids spurious splice transients between segments.
    fn sine_segments(freq: f64, segments: &[(f64, f64)], fs: u32, channels: u8) -> Vec<i32> {
        let mut out = Vec::new();
        let mut i = 0usize;
        for &(dbfs, secs) in segments {
            let peak = if dbfs <= -200.0 {
                0.0
            } else {
                powf(10.0, dbfs / 20.0)
            };
            let n = (secs * f64::from(fs)) as usize;
            for _ in 0..n {
                let t = i as f64 / f64::from(fs);
                let s = (peak * (2.0 * PI64 * freq * t).sin() * (FULL_SCALE - 1.0)).round() as i32;
                for _ in 0..channels {
                    out.push(s);
                }
                i += 1;
            }
        }
        out
    }

    /// EBU Tech 3341 Case 2: a 1 kHz sine at −33 dBFS reads −33.0 LUFS (a second
    /// calibration point — linearity).
    #[test]
    fn ebu_case2_minus_33() {
        let mut a = R128Analyzer::new(48_000, 2);
        a.feed(&sine(1000.0, sine_peak_at_dbfs(-33.0), 4.0, 48_000, 2));
        let l = a.integrated_lufs();
        assert!(
            (l - (-33.0)).abs() <= 0.1,
            "integrated={l} LUFS, want -33.0 ±0.1"
        );
    }

    /// EBU Tech 3341 Case 3: −36, −23, −36 LUFS segments. The quiet −36 segments
    /// fall below the RELATIVE gate (−10 LU under the gated mean), so integrated
    /// loudness is the −23 segment's: −23.0 LUFS ±0.1. (Shorter than the spec's
    /// 10/60/10 s but the same proportions — gating is duration-independent.)
    #[test]
    fn ebu_case3_relative_gating() {
        let mut a = R128Analyzer::new(48_000, 2);
        a.feed(&sine_segments(
            1000.0,
            &[(-36.0, 2.0), (-23.0, 24.0), (-36.0, 2.0)],
            48_000,
            2,
        ));
        let l = a.integrated_lufs();
        assert!(
            (l - (-23.0)).abs() <= 0.1,
            "integrated={l} LUFS, want -23.0 ±0.1 (relative gate)"
        );
    }

    /// EBU Tech 3341 Case 4: −72, −36, −23, −36, −72 LUFS segments. The −72
    /// segments fall below the −70 LUFS ABSOLUTE gate and the −36 below the
    /// relative gate, leaving the −23 segment: −23.0 LUFS ±0.1. Exercises BOTH
    /// gates.
    #[test]
    fn ebu_case4_absolute_and_relative_gating() {
        let mut a = R128Analyzer::new(48_000, 2);
        a.feed(&sine_segments(
            1000.0,
            &[
                (-72.0, 2.0),
                (-36.0, 2.0),
                (-23.0, 24.0),
                (-36.0, 2.0),
                (-72.0, 2.0),
            ],
            48_000,
            2,
        ));
        let l = a.integrated_lufs();
        assert!(
            (l - (-23.0)).abs() <= 0.1,
            "integrated={l} LUFS, want -23.0 ±0.1 (abs+rel gate)"
        );
    }
}
