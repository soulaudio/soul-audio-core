//! CI-running metric conformance for the loudness analyzer, so the metrics the library
//! loudness/DR column shows are validated on every run, not only by the `#[ignore]`d
//! `ffmpeg_crosscheck` (which needs the local corpus + ffmpeg on PATH).
//!
//! - **DR** is checked against [`exact_dr14`] — the authoritative `dr14_t.meter` / dr.loudness-war
//!   order-statistics algorithm (sorted, no histogram). Our analyzer uses a bounded-memory
//!   histogram approximation, so matching the exact algorithm within ±1 is the real conformance.
//! - **Integrated LUFS** is checked for physical linearity: halving a signal's amplitude must drop
//!   its loudness by 6.02 dB. (Absolute LUFS vs ffmpeg's `ebur128` stays in `ffmpeg_crosscheck`.)

use soul_audio_loudness::R128Analyzer;

const FS24: f64 = 8_388_607.0; // 24-bit canonical full scale

/// Exact TT-DR14 over the SAME PCM (sorted order statistics, no histogram) — the authoritative
/// reference the analyzer's bounded-memory histogram DR must match. Mirrors the algorithm in
/// `ffmpeg_crosscheck::exact_dr14`.
fn exact_dr14(samples: &[i32], channels: usize, sample_rate: u32) -> i32 {
    const FULL_SCALE: f64 = 8_388_608.0;
    let delta = if sample_rate == 44_100 { 60 } else { 0 };
    let bs = 3 * (sample_rate + delta) as usize;
    let frames: Vec<&[i32]> = samples.chunks_exact(channels).collect();
    let n = frames.len();
    let (mut rms2, mut peak): (Vec<Vec<f64>>, Vec<Vec<f64>>) =
        (vec![Vec::new(); channels], vec![Vec::new(); channels]);
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
            rms2[c].push(2.0 * sumsq / m);
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
        let rms_top: f64 = rms2[c][mm - n_blk..].iter().sum::<f64>() / n_blk as f64;
        sum += 20.0 * peak[c][mm - 2].log10() - 10.0 * rms_top.log10();
    }
    (sum / channels as f64).round() as i32
}

/// A deterministic 1 kHz sine whose amplitude steps every 3 s through `levels` (fractions of
/// full scale), so per-block RMS spans a wide range and DR is non-trivial — exercising the
/// histogram's top-20% vs the exact sorted statistics.
fn stepped_sine(fs: u32, channels: usize, levels: &[f64]) -> Vec<i32> {
    let block = 3 * fs as usize; // one DR block
    let mut out = Vec::with_capacity(block * levels.len() * channels);
    for (b, &amp) in levels.iter().enumerate() {
        for i in 0..block {
            let t = (b * block + i) as f64 / fs as f64;
            let s = (amp * FS24 * (2.0 * core::f64::consts::PI * 1000.0 * t).sin()) as i32;
            for _ in 0..channels {
                out.push(s);
            }
        }
    }
    out
}

/// A steady 1 kHz sine at `amp` (fraction of full scale), 3 s.
fn steady_sine(fs: u32, channels: usize, amp: f64) -> Vec<i32> {
    stepped_sine(fs, channels, &[amp])
}

#[test]
fn dr_matches_the_exact_reference_algorithm() {
    let fs = 48_000u32;
    let ch = 2usize;
    // 6 blocks of widely varying level → a meaningful, non-degenerate DR.
    let sig = stepped_sine(fs, ch, &[0.9, 0.2, 0.7, 0.1, 0.8, 0.35]);

    let mut an = R128Analyzer::new(fs, ch as u8);
    an.feed(&sig);
    let ours = an.dynamic_range().round() as i32;
    let exact = exact_dr14(&sig, ch, fs);

    assert!(
        (ours - exact).abs() <= 1,
        "histogram DR {ours} vs exact dr14_t.meter {exact} (tolerance 1)"
    );
}

#[test]
fn integrated_lufs_is_linear_in_level() {
    let fs = 48_000u32;
    let ch = 2usize;

    let mut a = R128Analyzer::new(fs, ch as u8);
    a.feed(&steady_sine(fs, ch, 0.5));
    let mut b = R128Analyzer::new(fs, ch as u8);
    b.feed(&steady_sine(fs, ch, 0.25)); // exactly -6.02 dB

    let (loud, quiet) = (a.integrated_lufs(), b.integrated_lufs());
    assert!(loud.is_finite() && quiet.is_finite(), "both above the gate");
    let drop = loud - quiet;
    assert!(
        (drop - 6.0206).abs() < 0.1,
        "halving amplitude should drop integrated loudness ~6.02 LU, got {drop} ({loud} - {quiet})"
    );
}
