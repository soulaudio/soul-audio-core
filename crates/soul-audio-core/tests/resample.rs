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

/// Steady-state gain (dB) the resampler applies to a `tone_hz` sine (`in_rate`→
/// `out_rate`). A transparent converter is unity (0 dB) across its passband; the
/// spread of this across frequencies is the **passband ripple**. RMS-based: images
/// sit ≳80 dB down, so they add negligibly to the steady-state RMS, leaving the tone's
/// own gain. The startup transient (filter history fill) is skipped.
fn passband_gain_db(quality: Quality, in_rate: u32, out_rate: u32, tone_hz: f64) -> f64 {
    let amp = 6_000_000.0; // well below 24-bit full scale — headroom for any ripple
    let mut r = Resampler::new(1, in_rate, out_rate, 1.1, 1024, quality).unwrap();
    let skip = r.latency() * 4 + 2048;
    let want = skip + 32_768;
    let mut out: Vec<f64> = Vec::with_capacity(want + 2048);
    let mut g = 0usize;
    let mut block = vec![0i32; 1024];
    let dph = 2.0 * std::f64::consts::PI * tone_hz / f64::from(in_rate);
    while out.len() < want {
        let need = r.input_frames_next();
        let input: Vec<i32> = (0..need)
            .map(|f| ((g + f) as f64 * dph).sin() * amp)
            .map(|s| s as i32)
            .collect();
        g += need;
        r.process(&input, &mut block).unwrap();
        out.extend(block.iter().map(|&s| f64::from(s)));
    }
    let steady = &out[skip..want];
    let out_rms = (steady.iter().map(|&s| s * s).sum::<f64>() / steady.len() as f64).sqrt();
    let in_rms = amp / std::f64::consts::SQRT_2;
    20.0 * (out_rms / in_rms).log10()
}

#[test]
fn hq_passband_is_flat_and_unity() {
    // Across the flat passband (up to ~0.4·Nyquist, clear of the transition band), the
    // HQ resampler must be unity-gain with negligible ripple — the audiophile bar is
    // << 0.1 dB. Tested on the 44.1→48 upsample (a ratio every CD-on-48k-DAC hits).
    let (in_r, out_r) = (44_100u32, 48_000u32);
    let nyq = f64::from(in_r.min(out_r)) / 2.0;
    let freqs = [
        100.0,
        500.0,
        1_000.0,
        2_000.0,
        3_000.0,
        5_000.0,
        7_000.0,
        nyq * 0.4,
    ];
    let gains: Vec<f64> = freqs
        .iter()
        .map(|&f| passband_gain_db(Quality::Hq, in_r, out_r, f))
        .collect();
    let max = gains.iter().copied().fold(f64::MIN, f64::max);
    let min = gains.iter().copied().fold(f64::MAX, f64::min);
    let ripple = max - min;
    println!("HQ 44.1->48 passband gains (dB): {gains:?}  ripple={ripple:.5} dB");
    assert!(
        ripple < 0.1,
        "HQ passband ripple {ripple:.5} dB exceeds 0.1 dB (gains {gains:?})"
    );
    assert!(
        max.abs() < 0.15 && min.abs() < 0.15,
        "HQ passband gain not unity (gains {gains:?})"
    );
}

#[test]
fn fast_passband_is_reasonably_flat() {
    // The Fast (embedded/DAP) tier trades stopband depth for size, but its passband must
    // still be essentially flat — no audible coloration from the converter.
    let (in_r, out_r) = (44_100u32, 48_000u32);
    let nyq = f64::from(in_r.min(out_r)) / 2.0;
    let freqs = [100.0, 1_000.0, 4_000.0, 7_000.0, nyq * 0.4];
    let gains: Vec<f64> = freqs
        .iter()
        .map(|&f| passband_gain_db(Quality::Fast, in_r, out_r, f))
        .collect();
    let ripple = gains.iter().copied().fold(f64::MIN, f64::max)
        - gains.iter().copied().fold(f64::MAX, f64::min);
    println!("Fast 44.1->48 passband ripple: {ripple:.5} dB  gains={gains:?}");
    assert!(
        ripple < 0.2,
        "Fast passband ripple {ripple:.5} dB too high (gains {gains:?})"
    );
}

#[test]
fn hq_alias_rejection_across_rate_ratios() {
    // The image/alias rejection measured in `hq_tier_beats_fast_on_downsample` must hold
    // across the full spread of real conversions (CD/hi-res families, up AND down), not
    // just 96→44.1. Measured at 14 kHz: the hard non-integer 44.1 ratios sit ~102 dB,
    // integer downsamples 137–143 dB — worst case ~102 dB. Require 100 dB everywhere (a
    // deterministic regression guard well above the 96 dB high-quality SRC threshold).
    let cases = [
        (44_100u32, 48_000u32),
        (48_000, 44_100),
        (44_100, 96_000),
        (96_000, 48_000),
        (88_200, 44_100),
        (176_400, 44_100),
        (44_100, 192_000),
        (192_000, 48_000),
    ];
    let mut worst = f64::INFINITY;
    for (in_r, out_r) in cases {
        let db = alias_rejection_db(Quality::Hq, in_r, out_r, 14_000.0);
        println!("HQ {in_r}->{out_r} @14kHz: alias rejection {db:.1} dB");
        worst = worst.min(db);
        assert!(
            db > 100.0,
            "HQ {in_r}->{out_r}: alias rejection only {db:.1} dB (want >100 — regressed below the high-quality SRC bar)"
        );
    }
    println!("HQ worst-case alias rejection across ratios: {worst:.1} dB");
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
