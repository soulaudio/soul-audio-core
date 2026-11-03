//! Objective DSP-quality tests for the resampler (host-only; uses `rustfft`).
//!
//! Method mirrors the infinitewave.ca SRC yardstick: resample a pure sine and
//! inspect the output spectrum. A correct band-limited converter shows one peak
//! (the tone) and everything else — images/aliases — buried far below it.

use rustfft::{FftPlanner, num_complex::Complex};
use soul_audio_core::{Quality, Resampler};

/// Modified Bessel I₀ (series) — for the Kaiser analysis window.
fn i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half = x / 2.0;
    for k in 1..80 {
        term *= (half / k as f64) * (half / k as f64);
        sum += term;
        if term < 1e-15 * sum {
            break;
        }
    }
    sum
}

/// Resample a `tone_hz` sine (`in_rate`→`out_rate`) and return
/// dB(signal peak) − dB(worst non-signal bin) over ~64k output samples.
fn alias_rejection_db(quality: Quality, in_rate: u32, out_rate: u32, tone_hz: f64) -> f64 {
    let n = 65_536usize;
    let mut r = Resampler::new(1, in_rate, out_rate, 1.1, 1024, quality).unwrap();
    let mut out: Vec<f64> = Vec::with_capacity(n + 2048);
    let mut g = 0usize;
    let mut block = vec![0i32; 1024];
    let dph = 2.0 * std::f64::consts::PI * tone_hz / f64::from(in_rate);
    while out.len() < n {
        let need = r.input_frames_next();
        let input: Vec<i32> = (0..need)
            .map(|f| ((g + f) as f64 * dph).sin() * 8_000_000.0)
            .map(|s| s as i32)
            .collect();
        g += need;
        r.process(&input, &mut block).unwrap();
        out.extend(block.iter().map(|&s| f64::from(s)));
    }
    out.truncate(n);

    // Kaiser(beta=22) analysis window: sidelobes ~ -200 dB, so the measurement
    // floor sits well below even the HQ tier's stopband (a Blackman-Harris window
    // would cap the reading at ~ -96 dB and mask it).
    let beta = 22.0;
    let denom = i0(beta);
    for (i, s) in out.iter_mut().enumerate() {
        let r = 2.0 * i as f64 / (n - 1) as f64 - 1.0;
        *s *= i0(beta * (1.0 - r * r).sqrt()) / denom;
    }

    let mut buf: Vec<Complex<f64>> = out.iter().map(|&re| Complex { re, im: 0.0 }).collect();
    FftPlanner::new().plan_fft_forward(n).process(&mut buf);
    let mag: Vec<f64> = buf[..n / 2].iter().map(|c| c.norm()).collect();

    let sig_bin = (tone_hz / f64::from(out_rate) * n as f64).round() as usize;
    let sig = mag[sig_bin - 12..=sig_bin + 12]
        .iter()
        .copied()
        .fold(0.0, f64::max);
    // Worst bin outside the Kaiser main lobe (±16 bins) and away from DC.
    let mut worst = 0.0f64;
    for (i, &m) in mag.iter().enumerate() {
        if i > 16 && (i as isize - sig_bin as isize).unsigned_abs() > 16 {
            worst = worst.max(m);
        }
    }
    20.0 * (sig / worst).log10()
}

#[test]
fn end_to_end_decode_then_resample() {
    use soul_audio_core::{Decoder, SliceSource, open};
    // Decode a real stereo fixture and resample it to 48 kHz (an upsample).
    let wav = include_bytes!("../testdata/ima-adpcm-stereo.wav");
    let mut dec = open(SliceSource::new(wav)).unwrap();
    let ch = dec.stream_info().channels as usize;
    let src_rate = dec.stream_info().sample_rate;
    assert!(src_rate < 48_000, "fixture should be below the target rate");

    let mut src = Vec::new();
    let mut buf = [0i32; 4096];
    loop {
        let n = dec.decode_into(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        src.extend_from_slice(&buf[..n]);
    }
    let src_frames = src.len() / ch;

    let mut r = Resampler::new(ch, src_rate, 48_000, 1.1, 256, Quality::Hq).unwrap();
    let mut cursor = 0usize;
    let mut out = Vec::new();
    let mut block = vec![0i32; 256 * ch];
    while cursor + r.input_frames_next() <= src_frames {
        let need = r.input_frames_next();
        r.process(&src[cursor * ch..(cursor + need) * ch], &mut block)
            .unwrap();
        cursor += need;
        out.extend_from_slice(&block);
    }
    let out_frames = out.len() / ch;
    // 48000/44100 ≈ 1.088 → more output frames than input consumed.
    assert!(out_frames > cursor, "expected upsampled frame growth");
    assert!(out.iter().any(|&s| s != 0), "resampled output is silent");
}

#[test]
fn fast_tier_alias_rejection() {
    // Upsample 44.1k→48k of a 6 kHz tone: Fast tier ≳ 80 dB image rejection.
    let db = alias_rejection_db(Quality::Fast, 44_100, 48_000, 6_000.0);
    assert!(db > 80.0, "Fast alias rejection only {db:.1} dB");
}

#[test]
fn hq_tier_beats_fast_on_downsample() {
    // Downsample 96k→44.1k of a 10 kHz tone (the classic aliasing stress case).
    let fast = alias_rejection_db(Quality::Fast, 96_000, 44_100, 10_000.0);
    let hq = alias_rejection_db(Quality::Hq, 96_000, 44_100, 10_000.0);
    assert!(hq > 110.0, "HQ alias rejection only {hq:.1} dB");
    assert!(hq > fast, "HQ ({hq:.1}) should beat Fast ({fast:.1})");
}
