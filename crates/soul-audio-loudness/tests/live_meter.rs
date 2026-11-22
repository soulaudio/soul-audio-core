//! The streaming `LiveLoudnessMeter` (the real-time meter shown during playback): validated
//! against the full `R128Analyzer` as the reference for short-term loudness, plus the
//! momentary-vs-short-term window behaviour, level linearity, and peak/silence readings.

use soul_audio_loudness::{LiveLoudnessMeter, NO_LOUDNESS, R128Analyzer};

/// 24-bit canonical full scale.
const FS24: f64 = 8_388_607.0;
const FS: u32 = 48_000;

/// Interleaved-stereo 1 kHz sine of `secs` seconds at linear amplitude `amp` (0..FS24).
fn tone(amp: f64, secs: f64) -> Vec<i32> {
    let n = (f64::from(FS) * secs) as usize;
    let dph = 2.0 * core::f64::consts::PI * 1_000.0 / f64::from(FS);
    let mut buf = Vec::with_capacity(n * 2);
    for i in 0..n {
        let s = (amp * (i as f64 * dph).sin()) as i32;
        buf.push(s);
        buf.push(s);
    }
    buf
}

/// The live meter's short-term loudness matches the reference analyzer's for the same steady
/// signal (same K-weighting + 3 s window), confirming the lightweight path is correct. A
/// steady tone also reads equal momentary and short-term loudness.
#[test]
fn short_term_matches_the_reference_analyzer() {
    let buf = tone(0.5 * FS24, 4.0);
    let mut meter = LiveLoudnessMeter::new(FS, 2);
    meter.feed(&buf);
    let mut reference = R128Analyzer::new(FS, 2);
    reference.feed(&buf);

    let got = meter.short_term_lufs();
    let want = reference.short_term_lufs();
    assert!(
        (got - want).abs() < 0.1,
        "live short-term {got:.2} LUFS should match the reference {want:.2} within 0.1 LU"
    );
    assert!(
        (meter.momentary_lufs() - got).abs() < 0.1,
        "a steady signal has equal momentary and short-term loudness"
    );
}

/// Loudness is linear in level: doubling the amplitude (+6.02 dB) reads +6 LU.
#[test]
fn loudness_tracks_level_linearly() {
    let mut quiet = LiveLoudnessMeter::new(FS, 2);
    quiet.feed(&tone(0.25 * FS24, 4.0));
    let mut loud = LiveLoudnessMeter::new(FS, 2);
    loud.feed(&tone(0.5 * FS24, 4.0));
    let delta = loud.short_term_lufs() - quiet.short_term_lufs();
    assert!(
        (delta - 6.02).abs() < 0.2,
        "doubling amplitude should raise loudness ~6 dB, got {delta:.2}"
    );
}

/// Momentary (400 ms) responds faster than short-term (3 s): after a loud passage drops to
/// quiet, the momentary reading falls well below the still-loud short-term reading.
#[test]
fn momentary_responds_faster_than_short_term() {
    let mut meter = LiveLoudnessMeter::new(FS, 2);
    meter.feed(&tone(0.5 * FS24, 3.0)); // fill the 3 s short-term window loud
    meter.feed(&tone(0.05 * FS24, 0.5)); // then 0.5 s much quieter
    let m = meter.momentary_lufs();
    let s = meter.short_term_lufs();
    assert!(
        m < s - 3.0,
        "momentary should drop below short-term after a loud->quiet step (m={m:.1}, s={s:.1})"
    );
}

/// A fresh meter and digital silence report no loudness/peak; a half-scale sine reads a
/// sample peak near -6 dBFS.
#[test]
fn silence_and_peak_readings() {
    let fresh = LiveLoudnessMeter::new(FS, 2);
    assert_eq!(
        fresh.short_term_lufs(),
        NO_LOUDNESS,
        "a fresh meter has no reading"
    );

    let mut silent = LiveLoudnessMeter::new(FS, 2);
    silent.feed(&vec![0i32; FS as usize * 2]); // 1 s of digital silence
    assert_eq!(
        silent.short_term_lufs(),
        NO_LOUDNESS,
        "silence reads no loudness"
    );
    assert_eq!(silent.peak_dbfs(), NO_LOUDNESS, "silence has no peak");

    let mut meter = LiveLoudnessMeter::new(FS, 2);
    meter.feed(&tone(0.5 * FS24, 1.0));
    let peak = meter.peak_dbfs();
    assert!(
        (peak - (-6.02)).abs() < 0.5,
        "a half-scale sine peaks near -6 dBFS, got {peak:.2}"
    );
}
