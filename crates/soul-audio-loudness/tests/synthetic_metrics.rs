//! Committed SYNTHETIC gates for the audiophile peak metrics, so they're validated in
//! every CI run — not only by the `#[ignore]`d real-corpus ffmpeg cross-check.
//!
//! True-peak (dBTP) measures the INTER-sample peak of the reconstructed waveform, which
//! can exceed the largest actual sample. The textbook case: a sine at fs/4 sampled at
//! 45° phase lands every sample at ±0.707·A, yet the continuous waveform peaks at A
//! between samples — a +3.01 dB overshoot. A true-peak estimator (ITU-R BS.1770
//! oversampling) must recover most of it; a plain sample-peak reading cannot see it.

use soul_audio_loudness::R128Analyzer;

/// 24-bit canonical full scale.
const FS24: f64 = 8_388_607.0;

#[test]
fn true_peak_catches_the_inter_sample_overshoot() {
    let fs = 48_000u32;
    let amp = 0.9 * FS24; // headroom so the reconstructed peak (A) stays in range
    let mut an = R128Analyzer::new(fs, 2);

    // sin(pi*i/2 + pi/4) = +0.707, +0.707, -0.707, -0.707, ...  (fs/4 at 45°)
    let n = fs as usize; // 1 s
    let mut buf = Vec::with_capacity(n * 2);
    for i in 0..n {
        let phase = std::f64::consts::PI * i as f64 / 2.0 + std::f64::consts::FRAC_PI_4;
        let s = (amp * phase.sin()) as i32;
        buf.push(s); // L
        buf.push(s); // R
    }
    an.feed(&buf);

    let sample_peak = an.sample_peak_dbfs();
    let true_peak = an.true_peak_dbtp();
    let overshoot = true_peak - sample_peak;
    println!(
        "sample_peak={sample_peak:.2} dBFS  true_peak={true_peak:.2} dBTP  overshoot={overshoot:.2} dB"
    );

    // Samples sit at 0.707·0.9 = 0.636 of full scale → ≈ -3.9 dBFS.
    let expected_sample = 20.0 * (0.707_f32 * 0.9).log10();
    assert!(
        (sample_peak - expected_sample).abs() < 0.3,
        "sample peak {sample_peak:.2} dBFS != expected {expected_sample:.2}"
    );
    // The reconstructed peak is A = 0.9·FS → ≈ -0.9 dBTP; the oversampled estimator must
    // recover most of the 3.01 dB overshoot (a plain sample peak would report 0 dB extra).
    assert!(
        overshoot > 2.5,
        "true-peak missed the inter-sample overshoot: only {overshoot:.2} dB above sample peak"
    );
    let expected_true = 20.0 * 0.9_f32.log10();
    assert!(
        (true_peak - expected_true).abs() < 0.6,
        "true peak {true_peak:.2} dBTP != expected {expected_true:.2}"
    );
}

/// A pure sine whose samples DON'T straddle an inter-sample peak (sampled at the crest)
/// must read true-peak ≈ sample-peak — the estimator doesn't invent overshoot that isn't
/// there (no false-positive headroom warnings).
#[test]
fn true_peak_matches_sample_peak_when_there_is_no_overshoot() {
    let fs = 48_000u32;
    let amp = 0.8 * FS24;
    let mut an = R128Analyzer::new(fs, 2);
    // A low-frequency sine (1 kHz) is densely sampled, so some sample lands very near the
    // crest — true-peak and sample-peak converge (overshoot well under 0.5 dB).
    let n = fs as usize;
    let mut buf = Vec::with_capacity(n * 2);
    let dph = 2.0 * std::f64::consts::PI * 1_000.0 / f64::from(fs);
    for i in 0..n {
        let s = (amp * (i as f64 * dph).sin()) as i32;
        buf.push(s);
        buf.push(s);
    }
    an.feed(&buf);
    let overshoot = an.true_peak_dbtp() - an.sample_peak_dbfs();
    println!("1 kHz sine overshoot: {overshoot:.3} dB");
    assert!(
        (-0.05..0.5).contains(&overshoot),
        "a densely-sampled sine should have ~0 inter-sample overshoot, got {overshoot:.3} dB"
    );
}
