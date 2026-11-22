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

use crate::math::{cos, log10, powf, sin, tan};

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

/// Short-term window: 3 s = 30 × 100 ms sub-blocks.
const ST_BLOCKS: usize = 30;
/// LRA short-term sampling hop: 1 s = 10 × 100 ms sub-blocks (EBU Tech 3342).
const ST_HOP: u32 = 10;
/// LRA relative gate: −20 LU below the absolute-gated short-term mean (EBU Tech
/// 3342 — distinct from the integrated loudness's −10 LU gate).
const LRA_REL_GATE_LU: f64 = -20.0;

/// BS.1770-4 Annex 2 true-peak interpolation FIR: a 48-tap windowed-sinc,
/// polyphase-decomposed into `factor` phases (the libebur128 / sdroege design).
const TP_TAPS: usize = 48;
/// Max oversampling factor (4× below 96 kHz) → up to 4 polyphase sub-filters.
const TP_MAX_PHASES: usize = 4;
/// Max taps per phase = `TP_TAPS / min_factor` = 48 / 2 = 24 (the 2× case).
const TP_MAX_SUBTAPS: usize = 24;

/// Minimum full 3-second blocks for a meaningful DR (Pleasurize/TT): `floor(0.2·M)`
/// must be ≥ 1, so M ≥ 5 (≥ 15 s). Below this, dynamic range is undefined.
const DR_MIN_BLOCKS: u32 = 5;

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

/// Build the true-peak polyphase FIR for `sample_rate`: returns the oversampling
/// `factor` (4 below 96 kHz, 2 below 192 kHz, 1 at/above — sample peak suffices),
/// the taps-per-phase, and the per-phase coefficients. A 48-tap Hann-windowed sinc
/// (libebur128 / sdroege design), de-interleaved into `factor` sub-filters and
/// normalized to unity DC gain per phase so a constant passes through unchanged.
fn build_tp_fir(sample_rate: u32) -> (usize, usize, [[f64; TP_MAX_SUBTAPS]; TP_MAX_PHASES]) {
    let factor = if sample_rate < 96_000 {
        4
    } else if sample_rate < 192_000 {
        2
    } else {
        1
    };
    let mut coefs = [[0.0f64; TP_MAX_SUBTAPS]; TP_MAX_PHASES];
    if factor < 2 {
        return (factor, 0, coefs);
    }
    let subtaps = TP_TAPS / factor;
    for j in 0..TP_TAPS {
        let m = j as f64 - (TP_TAPS as f64) / 2.0;
        let arg = m * PI / factor as f64;
        let sinc = if m.abs() < 1e-6 { 1.0 } else { sin(arg) / arg };
        let hann = 0.5 * (1.0 - cos(2.0 * PI * j as f64 / TP_TAPS as f64));
        coefs[j % factor][j / factor] = sinc * hann;
    }
    // Normalize each phase to unity DC gain (so a DC/full-scale signal reconstructs
    // at the correct amplitude — the reference filters are near-unity; this pins it).
    for phase in coefs.iter_mut().take(factor) {
        let sum: f64 = phase[..subtaps].iter().sum();
        if sum != 0.0 {
            for c in &mut phase[..subtaps] {
                *c /= sum;
            }
        }
    }
    (factor, subtaps, coefs)
}

/// Over a loudness histogram, sum the block count and reconstructed block energy
/// for bins whose loudness is `>= threshold_lufs`. Shared by integrated loudness
/// and LRA gating.
fn hist_accumulate(hist: &[u32; HIST_BINS], threshold_lufs: f64) -> (u64, f64) {
    let mut count = 0u64;
    let mut sum_z = 0.0;
    for (i, &cnt) in hist.iter().enumerate() {
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

/// The loudness (LUFS) at percentile `frac` (0..1) of the histogram's samples that
/// clear `rel_threshold`, walking bins low→high — `total` is the gated sample count.
fn hist_percentile(hist: &[u32; HIST_BINS], rel_threshold: f64, total: u64, frac: f64) -> f64 {
    let target = (frac * total as f64) as u64;
    let mut cum = 0u64;
    let mut last = rel_threshold;
    for (i, &cnt) in hist.iter().enumerate() {
        if cnt == 0 {
            continue;
        }
        let l = HIST_MIN + i as f64 * HIST_STEP;
        if l < rel_threshold {
            continue;
        }
        last = l;
        cum += u64::from(cnt);
        if cum > target {
            return l;
        }
    }
    last
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
    // Short-term (3 s) + LRA. `st_ring` holds the last 30 sub-blocks' per-channel
    // energies; the short-term loudness is sampled on a 1 s hop into `st_hist` for
    // the Loudness Range percentiles.
    st_ring: [[f64; MAX_CHANNELS]; ST_BLOCKS],
    st_pos: usize,
    st_count: usize,
    st_hop: u32,
    st_hist: [u32; HIST_BINS],
    // True-peak (dBTP): a polyphase FIR oversampler on the RAW signal. `tp_factor`
    // is 4/2/1 by sample rate; `tp_coefs[p]` holds phase p's `tp_subtaps` taps;
    // `tp_delay[c]` is channel c's input delay line (newest at [0]).
    tp_factor: usize,
    tp_subtaps: usize,
    tp_coefs: [[f64; TP_MAX_SUBTAPS]; TP_MAX_PHASES],
    tp_delay: [[f64; TP_MAX_SUBTAPS]; MAX_CHANNELS],
    true_peak: f64,
    // Dynamic range (Pleasurize DR14): per-channel 3 s blocks of flat (un-weighted)
    // RMS and peak. The block RMS loudness is histogrammed (for the loudest-20%
    // energy mean); the two highest block peaks are tracked for the 2nd-highest peak.
    dr_block_samples: u32,
    dr_block_filled: u32,
    dr_block_sumsq: [f64; MAX_CHANNELS],
    dr_block_peak: [f64; MAX_CHANNELS],
    dr_hist: [[u32; HIST_BINS]; MAX_CHANNELS],
    dr_peak1: [f64; MAX_CHANNELS],
    dr_peak2: [f64; MAX_CHANNELS],
    dr_block_count: u32,
}

/// The per-track loudness &amp; dynamic-range metric set — the aggregate read back
/// from [`R128Analyzer::metrics`] after a whole-track `feed`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoudnessMetrics {
    /// Gated integrated loudness (LUFS), or [`NO_LOUDNESS`] if undefined.
    pub integrated_lufs: f32,
    /// Loudness Range (LU); `0.0` if too few short-term samples.
    pub lra: f32,
    /// Max true-peak (dBTP); `-inf` if silent.
    pub true_peak_dbtp: f32,
    /// Peak-to-Loudness Ratio (LU) = `true_peak_dbtp − integrated_lufs`; `0.0` if
    /// integrated loudness is undefined.
    pub plr: f32,
    /// Dynamic-range value (Pleasurize/TT "DR"); `0.0` if too few blocks.
    pub dr: f32,
}

impl R128Analyzer {
    /// Build an analyzer for `sample_rate` Hz and `channels` (1..=2).
    pub fn new(sample_rate: u32, channels: u8) -> Self {
        let channels = (channels as usize).clamp(1, MAX_CHANNELS);
        let fs = f64::from(sample_rate);
        let (tp_factor, tp_subtaps, tp_coefs) = build_tp_fir(sample_rate);
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
            st_ring: [[0.0; MAX_CHANNELS]; ST_BLOCKS],
            st_pos: 0,
            st_count: 0,
            st_hop: 0,
            st_hist: [0; HIST_BINS],
            tp_factor,
            tp_subtaps,
            tp_coefs,
            tp_delay: [[0.0; TP_MAX_SUBTAPS]; MAX_CHANNELS],
            true_peak: 0.0,
            // DR block length: 3·(fs + 60) only at 44.1 kHz, else 3·fs (dr14_t.meter).
            dr_block_samples: 3 * (sample_rate + if sample_rate == 44_100 { 60 } else { 0 }),
            dr_block_filled: 0,
            dr_block_sumsq: [0.0; MAX_CHANNELS],
            dr_block_peak: [0.0; MAX_CHANNELS],
            dr_hist: [[0; HIST_BINS]; MAX_CHANNELS],
            dr_peak1: [0.0; MAX_CHANNELS],
            dr_peak2: [0.0; MAX_CHANNELS],
            dr_block_count: 0,
        }
    }

    /// Feed interleaved canonical-24-bit PCM. `interleaved.len()` should be a
    /// whole number of frames (`channels` samples each); a trailing partial
    /// frame is ignored.
    pub fn feed(&mut self, interleaved: &[i32]) {
        let ch = self.channels;
        let factor = self.tp_factor;
        let subtaps = self.tp_subtaps;
        for frame in interleaved.chunks_exact(ch) {
            for (c, &sample) in frame.iter().enumerate() {
                let x = f64::from(sample) / FULL_SCALE;
                let ax = x.abs();
                if ax > self.sample_peak {
                    self.sample_peak = ax;
                }
                // DR (flat, un-weighted) block accumulation on the raw sample.
                self.dr_block_sumsq[c] += x * x;
                if ax > self.dr_block_peak[c] {
                    self.dr_block_peak[c] = ax;
                }
                // True-peak: oversample the RAW sample and track the max magnitude.
                if factor >= 2 {
                    let d = &mut self.tp_delay[c];
                    for i in (1..subtaps).rev() {
                        d[i] = d[i - 1];
                    }
                    d[0] = x;
                    let d = &self.tp_delay[c];
                    let mut mx = self.true_peak;
                    for p in 0..factor {
                        let cp = &self.tp_coefs[p];
                        let mut acc = 0.0;
                        for k in 0..subtaps {
                            acc += cp[k] * d[k];
                        }
                        let a = if acc < 0.0 { -acc } else { acc };
                        if a > mx {
                            mx = a;
                        }
                    }
                    self.true_peak = mx;
                }
                let y1 = self.s1[c].process(&self.c1, x);
                let y = self.s2[c].process(&self.c2, y1);
                self.sub_sumsq[c] += y * y;
            }
            self.subblock_filled += 1;
            if self.subblock_filled >= self.samples_per_subblock {
                self.close_subblock();
            }
            self.dr_block_filled += 1;
            if self.dr_block_filled >= self.dr_block_samples {
                self.close_dr_block(self.dr_block_samples);
            }
        }
    }

    /// Roll the current 100 ms sub-block into the 400 ms (integrated) and 3 s
    /// (short-term) rings; emit one 400 ms gating block once four sub-blocks are
    /// present (75 % overlap), and sample the short-term loudness on a 1 s hop for
    /// Loudness Range.
    fn close_subblock(&mut self) {
        // Short-term ring (last 30 sub-blocks = 3 s).
        self.st_ring[self.st_pos] = self.sub_sumsq;
        self.st_pos = (self.st_pos + 1) % ST_BLOCKS;
        if self.st_count < ST_BLOCKS {
            self.st_count += 1;
        }
        // LRA: sample the short-term loudness every 1 s once the 3 s window is full.
        if self.st_count >= ST_BLOCKS {
            if self.st_hop == 0 {
                self.sample_short_term();
                self.st_hop = ST_HOP;
            }
            self.st_hop -= 1;
        }

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

    /// Close one DR block: histogram each channel's factor-2 RMS loudness and fold
    /// its peak into the two-highest-peaks tracker, over exactly `n_samples`.
    fn close_dr_block(&mut self, n_samples: u32) {
        let n = f64::from(n_samples);
        for c in 0..self.channels {
            // Factor-2 RMS² (= +3.01 dB, so a full-scale sine reads 0 dB), flat
            // (un-weighted). Block loudness = 10·log10(rms²), no −0.691 offset.
            let rms2 = 2.0 * self.dr_block_sumsq[c] / n;
            let idx = if rms2 > 0.0 {
                let l = 10.0 * log10(rms2);
                (((l - HIST_MIN) / HIST_STEP + 0.5) as i64).clamp(0, (HIST_BINS - 1) as i64)
                    as usize
            } else {
                0
            };
            self.dr_hist[c][idx] += 1;
            let p = self.dr_block_peak[c];
            if p > self.dr_peak1[c] {
                self.dr_peak2[c] = self.dr_peak1[c];
                self.dr_peak1[c] = p;
            } else if p > self.dr_peak2[c] {
                self.dr_peak2[c] = p;
            }
            self.dr_block_sumsq[c] = 0.0;
            self.dr_block_peak[c] = 0.0;
        }
        self.dr_block_count += 1;
        self.dr_block_filled = 0;
    }

    /// DR value for one channel: `20·log10(peak2 / rms_top)`, where `rms_top` is the
    /// energy (quadratic) mean of the loudest-20%-by-RMS blocks and `peak2` is the
    /// channel's 2nd-highest block peak (Pleasurize/TT).
    fn dr_channel(&self, c: usize) -> f64 {
        let n_blk = ((0.2 * f64::from(self.dr_block_count)) as u32).max(1) as usize;
        let mut count = 0usize;
        let mut energy = 0.0;
        for i in (0..HIST_BINS).rev() {
            let cnt = self.dr_hist[c][i] as usize;
            if cnt == 0 {
                continue;
            }
            let l = HIST_MIN + i as f64 * HIST_STEP;
            let rms2 = powf(10.0, l / 10.0);
            let take = (n_blk - count).min(cnt);
            energy += take as f64 * rms2;
            count += take;
            if count >= n_blk {
                break;
            }
        }
        let rms_top2 = if count > 0 {
            energy / count as f64
        } else {
            0.0
        };
        let peak2 = self.dr_peak2[c];
        if peak2 <= 0.0 || rms_top2 <= 0.0 {
            return 0.0;
        }
        20.0 * log10(peak2) - 10.0 * log10(rms_top2)
    }

    /// Channel-weighted mean-square energy `z` over the current short-term (3 s)
    /// window, or `None` until the window is full.
    fn short_term_energy(&self) -> Option<f64> {
        if self.st_count < ST_BLOCKS {
            return None;
        }
        let n = f64::from(self.samples_per_subblock) * ST_BLOCKS as f64;
        let mut z = 0.0;
        for c in 0..self.channels {
            let mut s = 0.0;
            for block in &self.st_ring {
                s += block[c];
            }
            z += channel_weight(c) * (s / n);
        }
        Some(z)
    }

    /// Histogram the current short-term loudness for LRA (absolute −70 LUFS gate
    /// at sample time; the relative gate is applied in [`loudness_range`]).
    fn sample_short_term(&mut self) {
        if let Some(z) = self.short_term_energy() {
            if z > 0.0 {
                let l = LUFS_OFFSET + 10.0 * log10(z);
                if l >= ABS_GATE_LUFS {
                    let idx = ((l - HIST_MIN) / HIST_STEP + 0.5) as i64;
                    let idx = idx.clamp(0, (HIST_BINS - 1) as i64) as usize;
                    self.st_hist[idx] += 1;
                }
            }
        }
    }

    /// Short-term loudness (LUFS) over the last 3 s, or [`NO_LOUDNESS`] until a full
    /// 3 s window has been fed.
    pub fn short_term_lufs(&self) -> f32 {
        match self.short_term_energy() {
            Some(z) if z > 0.0 => (LUFS_OFFSET + 10.0 * log10(z)) as f32,
            _ => NO_LOUDNESS,
        }
    }

    /// Loudness Range (LRA, in LU) — the 95th minus 10th percentile of the gated
    /// short-term loudness distribution (EBU Tech 3342: absolute −70 LUFS, relative
    /// −20 LU). `0.0` if too few short-term samples cleared the gates (e.g. a track
    /// under ~1 min, which EBU notes is not meaningful anyway).
    pub fn loudness_range(&self) -> f32 {
        // Absolute-gated mean short-term energy → the −20 LU relative threshold.
        let (count, sum_z) = hist_accumulate(&self.st_hist, ABS_GATE_LUFS);
        if count == 0 {
            return 0.0;
        }
        let mean_z = sum_z / count as f64;
        let rel_threshold = LUFS_OFFSET + 10.0 * log10(mean_z) + LRA_REL_GATE_LU;
        // Total short-term samples above both gates.
        let total: u64 = self
            .st_hist
            .iter()
            .enumerate()
            .filter(|(i, _)| HIST_MIN + *i as f64 * HIST_STEP >= rel_threshold)
            .map(|(_, &c)| u64::from(c))
            .sum();
        if total == 0 {
            return 0.0;
        }
        let p10 = hist_percentile(&self.st_hist, rel_threshold, total, 0.10);
        let p95 = hist_percentile(&self.st_hist, rel_threshold, total, 0.95);
        (p95 - p10) as f32
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

    /// Sum block count and reconstructed block energy over this analyzer's
    /// integrated histogram for bins whose loudness is `>= threshold_lufs`.
    fn accumulate(&self, threshold_lufs: f64) -> (u64, f64) {
        hist_accumulate(&self.hist, threshold_lufs)
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

    /// Max **true-peak** in dBTP (BS.1770-4 Annex 2 inter-sample peak via 4×/2×
    /// oversampling; at ≥192 kHz the sample peak already captures it). `-inf` if
    /// silent. Unlike [`sample_peak_dbfs`](Self::sample_peak_dbfs) this catches
    /// inter-sample overshoots a sample-peak meter misses.
    pub fn true_peak_dbtp(&self) -> f32 {
        let p = if self.tp_factor >= 2 {
            self.true_peak
        } else {
            self.sample_peak
        };
        if p <= 0.0 {
            f32::NEG_INFINITY
        } else {
            (20.0 * log10(p)) as f32
        }
    }

    /// ReplayGain (dB) to reach `target_lufs` (e.g.
    /// [`REPLAYGAIN2_TARGET_LUFS`]). `0.0` if loudness is undefined.
    pub fn replay_gain_db(&self, target_lufs: f32) -> f32 {
        let i = self.integrated_lufs();
        if i.is_finite() { target_lufs - i } else { 0.0 }
    }

    /// Dynamic-range value (Pleasurize/TT "DR", DR-Database compatible) — the mean
    /// over channels of [`dr_channel`](Self::dr_channel), rounded to an integer.
    /// `0.0` for inputs shorter than [`DR_MIN_BLOCKS`] × 3 s (DR is not meaningful).
    pub fn dynamic_range(&self) -> f32 {
        if self.dr_block_count < DR_MIN_BLOCKS {
            return 0.0;
        }
        let mut sum = 0.0;
        for c in 0..self.channels {
            sum += self.dr_channel(c);
        }
        let mean = sum / self.channels as f64;
        // Round to the nearest integer (DR-Database convention); DR is non-negative
        // for real music.
        (if mean > 0.0 {
            (mean + 0.5) as i64
        } else {
            (mean - 0.5) as i64
        }) as f32
    }

    /// The whole per-track metric set in one call — integrated LUFS, LRA, true-peak,
    /// PLR (= true-peak − integrated), and DR. Read it once after feeding the whole
    /// track.
    pub fn metrics(&self) -> LoudnessMetrics {
        let integrated_lufs = self.integrated_lufs();
        let true_peak_dbtp = self.true_peak_dbtp();
        let plr = if integrated_lufs.is_finite() && true_peak_dbtp.is_finite() {
            true_peak_dbtp - integrated_lufs
        } else {
            0.0
        };
        LoudnessMetrics {
            integrated_lufs,
            lra: self.loudness_range(),
            true_peak_dbtp,
            plr,
            dr: self.dynamic_range(),
        }
    }
}

/// Momentary gating-block length: 400 ms = 4 × 100 ms sub-blocks (BS.1770 "M").
const MOMENTARY_BLOCKS: usize = 4;

/// A lightweight streaming loudness meter for a LIVE display (a LUFS/peak meter shown while a
/// track plays), as distinct from [`R128Analyzer`]'s whole-track integrated analysis. It
/// K-weights the signal with the same BS.1770 pre-filter into 100 ms sub-blocks and keeps a
/// 3 s sliding ring, exposing the current MOMENTARY (400 ms) and SHORT-TERM (3 s) loudness
/// plus a decaying sample peak.
///
/// It deliberately OMITS the histogram gating, true-peak FIR, and DR of the full analyzer, so
/// it is cheap enough to run per audio block on the playback pump thread without risking
/// dropouts. Fixed-size state, no-alloc, `no_std`, DAP-reusable.
pub struct LiveLoudnessMeter {
    channels: usize,
    c1: Coeffs,
    c2: Coeffs,
    s1: [State; MAX_CHANNELS],
    s2: [State; MAX_CHANNELS],
    samples_per_subblock: u32,
    subblock_filled: u32,
    sub_sumsq: [f64; MAX_CHANNELS],
    /// The last `ST_BLOCKS` (30 = 3 s) sub-block per-channel mean-squares. The momentary
    /// window is the most recent [`MOMENTARY_BLOCKS`] (400 ms); short-term is all 30.
    ring: [[f64; MAX_CHANNELS]; ST_BLOCKS],
    /// Next slot to write (so the most recent is `pos - 1`).
    pos: usize,
    count: usize,
    /// Per-sub-block max sample magnitude (normalized to full scale), parallel to `ring`; the
    /// peak meter reads the max over the recent momentary window so it rises on a transient and
    /// falls as it ages out, with no decay-timing artefact.
    peak_ring: [f64; ST_BLOCKS],
    /// Max sample magnitude seen in the in-progress sub-block.
    cur_peak: f64,
}

impl LiveLoudnessMeter {
    /// Build a meter for `sample_rate` Hz and `channels` (mono or stereo).
    #[must_use]
    pub fn new(sample_rate: u32, channels: u8) -> Self {
        let fs = f64::from(sample_rate);
        let ch = (channels as usize).clamp(1, MAX_CHANNELS);
        Self {
            channels: ch,
            c1: stage1(fs),
            c2: stage2(fs),
            s1: [State::default(); MAX_CHANNELS],
            s2: [State::default(); MAX_CHANNELS],
            samples_per_subblock: (sample_rate / 10).max(1), // 100 ms
            subblock_filled: 0,
            sub_sumsq: [0.0; MAX_CHANNELS],
            ring: [[0.0; MAX_CHANNELS]; ST_BLOCKS],
            pos: 0,
            count: 0,
            peak_ring: [0.0; ST_BLOCKS],
            cur_peak: 0.0,
        }
    }

    /// Feed interleaved canonical-24-bit `i32` PCM (`len` a multiple of `channels`).
    pub fn feed(&mut self, interleaved: &[i32]) {
        for frame in interleaved.chunks_exact(self.channels) {
            for (ch, &s) in frame.iter().enumerate() {
                let x = f64::from(s) / FULL_SCALE;
                let a = if x < 0.0 { -x } else { x };
                if a > self.cur_peak {
                    self.cur_peak = a;
                }
                let k1 = self.s1[ch].process(&self.c1, x);
                let y = self.s2[ch].process(&self.c2, k1);
                self.sub_sumsq[ch] += y * y;
            }
            self.subblock_filled += 1;
            if self.subblock_filled >= self.samples_per_subblock {
                self.finish_subblock();
            }
        }
    }

    fn finish_subblock(&mut self) {
        let n = f64::from(self.samples_per_subblock);
        for ch in 0..self.channels {
            self.ring[self.pos][ch] = self.sub_sumsq[ch] / n; // mean square
            self.sub_sumsq[ch] = 0.0;
        }
        self.peak_ring[self.pos] = self.cur_peak;
        self.cur_peak = 0.0;
        self.pos = (self.pos + 1) % ST_BLOCKS;
        self.count = (self.count + 1).min(ST_BLOCKS);
        self.subblock_filled = 0;
    }

    /// Mean loudness (LUFS) over the most recent `blocks` sub-blocks, or [`NO_LOUDNESS`] when
    /// nothing has accumulated yet (or the window is silent).
    fn windowed_lufs(&self, blocks: usize) -> f32 {
        let w = blocks.min(self.count);
        if w == 0 {
            return NO_LOUDNESS;
        }
        let mut z = 0.0f64;
        for ch in 0..self.channels {
            let mut sum = 0.0;
            for k in 0..w {
                let idx = (self.pos + ST_BLOCKS - 1 - k) % ST_BLOCKS;
                sum += self.ring[idx][ch];
            }
            z += channel_weight(ch) * (sum / w as f64);
        }
        if z <= 0.0 {
            return NO_LOUDNESS;
        }
        (LUFS_OFFSET + 10.0 * log10(z)) as f32
    }

    /// The current MOMENTARY (400 ms) loudness in LUFS — the fast meter reading.
    #[must_use]
    pub fn momentary_lufs(&self) -> f32 {
        self.windowed_lufs(MOMENTARY_BLOCKS)
    }

    /// The current SHORT-TERM (3 s) loudness in LUFS — the slower meter reading.
    #[must_use]
    pub fn short_term_lufs(&self) -> f32 {
        self.windowed_lufs(ST_BLOCKS)
    }

    /// The recent sample peak in dBFS — the max sample magnitude over the momentary (400 ms)
    /// window and the in-progress sub-block — or [`NO_LOUDNESS`] at silence.
    #[must_use]
    pub fn peak_dbfs(&self) -> f32 {
        let mut p = self.cur_peak;
        let w = MOMENTARY_BLOCKS.min(self.count);
        for k in 0..w {
            let idx = (self.pos + ST_BLOCKS - 1 - k) % ST_BLOCKS;
            if self.peak_ring[idx] > p {
                p = self.peak_ring[idx];
            }
        }
        if p <= 0.0 {
            return NO_LOUDNESS;
        }
        (20.0 * log10(p)) as f32
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

    /// A −12 dBFS 1 kHz sine across `blocks` 3-second blocks, with an optional
    /// full-scale single-sample spike at the centre of each block in `spike_blocks`
    /// (raising that block's PEAK to 0 dBFS without materially changing its RMS).
    fn dr_signal(fs: u32, blocks: usize, spike_blocks: &[usize]) -> Vec<i32> {
        let amp = powf(10.0, -12.0 / 20.0);
        let block_n = 3 * fs as usize;
        let n = blocks * block_n;
        let mut out = Vec::with_capacity(n * 2);
        for k in 0..n {
            let t = k as f64 / f64::from(fs);
            let mut v = amp * (2.0 * PI64 * 1000.0 * t).sin();
            let blk = k / block_n;
            if spike_blocks.contains(&blk) && (k % block_n) == block_n / 2 {
                v = 1.0; // full-scale inter-block transient
            }
            let s = (v * (FULL_SCALE - 1.0)).round() as i32;
            out.push(s);
            out.push(s);
        }
        out
    }

    /// A pure sine has peak == RMS in DR's sine-calibrated (factor-2) sense → DR ≈ 0.
    #[test]
    fn dr_of_a_pure_sine_is_near_zero() {
        let mut a = R128Analyzer::new(48_000, 2);
        a.feed(&dr_signal(48_000, 10, &[]));
        let dr = a.dynamic_range();
        assert!(dr.abs() <= 1.0, "DR={dr} for a pure sine, want ~0");
    }

    /// A −12 dBFS sine whose loud blocks carry full-scale (0 dBFS) transients: the
    /// loudest-20% RMS stays ≈ −12 dB but the 2nd-highest peak is ≈ 0 dB → DR ≈ 12.
    #[test]
    fn dr_reflects_peak_to_rms_crest() {
        let mut a = R128Analyzer::new(48_000, 2);
        a.feed(&dr_signal(48_000, 12, &[3, 7]));
        let dr = a.dynamic_range();
        assert!((dr - 12.0).abs() <= 2.0, "DR={dr}, want ~12");
    }

    #[test]
    fn dr_is_undefined_for_a_very_short_input() {
        let mut a = R128Analyzer::new(48_000, 2);
        a.feed(&sine(1000.0, sine_peak_at_dbfs(-12.0), 3.0, 48_000, 2)); // ~1 block
        assert_eq!(a.dynamic_range(), 0.0, "too few blocks → DR undefined (0)");
    }

    /// `metrics()` bundles the whole set and derives PLR = true-peak − integrated.
    #[test]
    fn metrics_bundles_the_set_and_derives_plr() {
        let mut a = R128Analyzer::new(48_000, 2);
        a.feed(&dr_signal(48_000, 10, &[3, 7]));
        let m = a.metrics();
        assert!(m.integrated_lufs.is_finite(), "integrated defined");
        assert!(m.true_peak_dbtp.is_finite(), "true-peak defined");
        let want_plr = m.true_peak_dbtp - m.integrated_lufs;
        assert!(
            (m.plr - want_plr).abs() <= 0.01,
            "PLR={} want {want_plr}",
            m.plr
        );
        assert!(m.dr > 8.0, "DR={} should reflect the transients", m.dr);
    }

    /// Exact Pleasurize/TT **DR14** from interleaved i32 PCM — sorted order-statistics
    /// (no histogram), the authoritative `dr14_t.meter` algorithm. The reference the
    /// analyzer's bounded-memory histogram DR is validated against.
    fn exact_dr14(samples: &[i32], channels: usize, sample_rate: u32) -> i32 {
        let delta = if sample_rate == 44_100 { 60 } else { 0 };
        let bs = 3 * (sample_rate + delta) as usize;
        let frames: Vec<&[i32]> = samples.chunks_exact(channels).collect();
        let n = frames.len();
        let mut rms2: Vec<Vec<f64>> = vec![Vec::new(); channels];
        let mut peak: Vec<Vec<f64>> = vec![Vec::new(); channels];
        let mut i = 0;
        while i < n {
            let end = (i + bs).min(n);
            let m = (end - i) as f64;
            for c in 0..channels {
                let (mut sumsq, mut pk) = (0.0f64, 0.0f64);
                for f in &frames[i..end] {
                    let x = f64::from(f[c]) / FULL_SCALE;
                    sumsq += x * x;
                    pk = pk.max(x.abs());
                }
                rms2[c].push(2.0 * sumsq / m); // factor-2 RMS²
                peak[c].push(pk);
            }
            i = end;
        }
        let mm = rms2[0].len();
        let n_blk = ((0.2 * mm as f64).floor() as usize).max(1);
        let mut sum = 0.0;
        for c in 0..channels {
            rms2[c].sort_by(|a, b| a.partial_cmp(b).unwrap());
            peak[c].sort_by(|a, b| a.partial_cmp(b).unwrap());
            let rms_top2: f64 = rms2[c][mm - n_blk..].iter().sum::<f64>() / n_blk as f64;
            let peak2 = peak[c][mm - 2]; // 2nd-highest block peak
            sum += 20.0 * peak2.log10() - 10.0 * rms_top2.log10();
        }
        (sum / channels as f64).round() as i32
    }

    /// 3-second blocks at the given per-block dBFS sine levels, each with one
    /// full-scale spike at its centre — so block RMS varies (exercising the DR
    /// loudest-20% selection) while every block's peak is 0 dBFS.
    fn dr_varied_signal(fs: u32, levels: &[f64]) -> Vec<i32> {
        let block_n = 3 * fs as usize;
        let mut out = Vec::with_capacity(levels.len() * block_n * 2);
        for (b, &dbfs) in levels.iter().enumerate() {
            let amp = powf(10.0, dbfs / 20.0);
            for k in 0..block_n {
                let t = (b * block_n + k) as f64 / f64::from(fs);
                let mut v = amp * (2.0 * PI64 * 1000.0 * t).sin();
                if k == block_n / 2 {
                    v = 1.0;
                }
                let s = (v * (FULL_SCALE - 1.0)).round() as i32;
                out.push(s);
                out.push(s);
            }
        }
        out
    }

    /// The analyzer's O(1) **histogram** DR must land on the **exact** sorted-order-
    /// statistics DR14. Checked on a varied-RMS signal (which actually exercises the
    /// loudest-20% selection the histogram approximates) within the ±1 quantization
    /// bound, and on a uniform-RMS signal where the two must be identical.
    #[test]
    fn dr_histogram_matches_the_exact_reference() {
        let varied = dr_varied_signal(
            48_000,
            &[
                -24.0, -20.0, -16.0, -12.0, -8.0, -20.0, -16.0, -12.0, -8.0, -24.0,
            ],
        );
        let mut a = R128Analyzer::new(48_000, 2);
        a.feed(&varied);
        let ours = a.dynamic_range() as i32;
        let exact = exact_dr14(&varied, 2, 48_000);
        assert!(
            (ours - exact).abs() <= 1,
            "varied-RMS: histogram DR {ours} vs exact DR14 {exact}"
        );

        let flat = dr_signal(48_000, 12, &[3, 7]);
        let mut b = R128Analyzer::new(48_000, 2);
        b.feed(&flat);
        assert_eq!(
            b.dynamic_range() as i32,
            exact_dr14(&flat, 2, 48_000),
            "uniform-RMS: histogram DR must equal the exact DR14"
        );
    }

    /// A full-scale fs/4 sine sampled 45° off its peaks: the samples sit at ±0.707
    /// (−3.01 dBFS) but the reconstructed waveform reaches 0 dBFS. Sample-peak misses
    /// the inter-sample peak; the 4× true-peak meter must catch it.
    #[test]
    fn true_peak_catches_an_inter_sample_peak() {
        let fs = 48_000u32;
        let n = fs as usize;
        let mut buf = Vec::with_capacity(n * 2);
        for k in 0..n {
            let v = (PI64 * k as f64 / 2.0 + PI64 / 4.0).sin(); // ±0.7071…
            let s = (v * (FULL_SCALE - 1.0)).round() as i32;
            buf.push(s);
            buf.push(s);
        }
        let mut a = R128Analyzer::new(fs, 2);
        a.feed(&buf);
        let sp = a.sample_peak_dbfs();
        let tp = a.true_peak_dbtp();
        assert!(
            (sp - (-3.01)).abs() < 0.1,
            "sample peak={sp} dBFS, want ~-3.01"
        );
        assert!(tp > -0.7, "true peak={tp} dBTP, want ~0 (> -0.7)");
        assert!(tp > sp + 2.0, "true peak {tp} must exceed sample peak {sp}");
    }

    /// An fs/4 sine sampled ON its peaks (0, 1, 0, −1): sample-peak is already full
    /// scale, and true-peak must land at ~0 dBTP without a large over/under-shoot.
    #[test]
    fn true_peak_of_an_aligned_full_scale_sine_is_about_zero() {
        let mut a = R128Analyzer::new(48_000, 2);
        a.feed(&sine(12_000.0, 1.0, 0.5, 48_000, 2));
        let tp = a.true_peak_dbtp();
        assert!((-0.3..0.7).contains(&tp), "true peak={tp} dBTP, want ~0");
    }

    #[test]
    fn short_term_tracks_a_steady_level() {
        let mut a = R128Analyzer::new(48_000, 2);
        a.feed(&sine(1000.0, sine_peak_at_dbfs(-23.0), 4.0, 48_000, 2));
        let s = a.short_term_lufs();
        assert!(
            (s - (-23.0)).abs() <= 0.2,
            "short-term={s} LUFS, want -23.0"
        );
    }

    #[test]
    fn short_term_is_undefined_before_three_seconds() {
        let mut a = R128Analyzer::new(48_000, 2);
        a.feed(&sine(1000.0, sine_peak_at_dbfs(-23.0), 1.0, 48_000, 2)); // only 1 s
        assert_eq!(
            a.short_term_lufs(),
            NO_LOUDNESS,
            "no full 3 s short-term window yet"
        );
    }

    /// LRA = P95 − P10 of the gated short-term distribution. Six seconds at
    /// −30 LUFS then six at −20 LUFS: the short-term value settles at each level,
    /// both clear the −20 LU relative gate, so P10 ≈ −30 and P95 ≈ −20 → LRA ≈ 10 LU.
    #[test]
    fn lra_spans_a_two_level_signal() {
        let mut a = R128Analyzer::new(48_000, 2);
        a.feed(&sine_segments(
            1000.0,
            &[(-30.0, 6.0), (-20.0, 6.0)],
            48_000,
            2,
        ));
        let lra = a.loudness_range();
        assert!((lra - 10.0).abs() <= 1.5, "LRA={lra} LU, want ~10");
    }

    #[test]
    fn lra_is_near_zero_for_a_constant_level() {
        let mut a = R128Analyzer::new(48_000, 2);
        a.feed(&sine(1000.0, sine_peak_at_dbfs(-20.0), 12.0, 48_000, 2));
        let lra = a.loudness_range();
        assert!(lra <= 1.0, "LRA={lra} LU for a constant level, want ~0");
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
