//! Black-box E2E: run the built CLI binary and check its behaviour.

use assert_cmd::Command;
use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

/// A minimal valid 16-bit mono WAV carrying `samples`.
fn wav16(samples: &[i16]) -> Vec<u8> {
    let mut data = Vec::new();
    for s in samples {
        data.extend_from_slice(&s.to_le_bytes());
    }
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36u32 + data.len() as u32).to_le_bytes());
    v.extend_from_slice(b"WAVE");
    v.extend_from_slice(b"fmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&8000u32.to_le_bytes());
    v.extend_from_slice(&16000u32.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(&data);
    v
}

#[test]
fn help_runs() {
    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .arg("--help")
        .assert()
        .success();
}

#[test]
fn decode_rejects_unknown_input() {
    let dir = std::env::temp_dir();
    let input = dir.join("soul_audio_cli_e2e_bad_in.bin");
    let out = dir.join("soul_audio_cli_e2e_bad_out.pcm");
    std::fs::write(&input, b"not a real audio file").unwrap();

    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .arg("decode")
        .arg(&input)
        .arg("--out")
        .arg(&out)
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot decode"));
}

#[test]
fn decode_wav_to_pcm() {
    let dir = std::env::temp_dir();
    let input = dir.join("soul_audio_cli_e2e_wav_in.wav");
    let out = dir.join("soul_audio_cli_e2e_wav_out.pcm");
    let samples = [100i16, -100, 32767, -32768];
    std::fs::write(&input, wav16(&samples)).unwrap();

    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .arg("decode")
        .arg(&input)
        .arg("--out")
        .arg(&out)
        .assert()
        .success();

    // Output is interleaved i32 LE PCM.
    let bytes = std::fs::read(&out).unwrap();
    let mut expected = Vec::new();
    for s in samples {
        expected.extend_from_slice(&i32::from(s).to_le_bytes());
    }
    assert_eq!(bytes, expected);
}

// --- `render` end-to-end: the full engine pipeline to a file (no device) ------

/// Wrap 16-bit PCM `data` (interleaved LE) in a WAV container.
fn wav16_pcm(rate: u32, channels: u16, data: &[u8]) -> Vec<u8> {
    let byte_rate = rate * u32::from(channels) * 2;
    let block_align = channels * 2;
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    v.extend_from_slice(b"WAVE");
    v.extend_from_slice(b"fmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&channels.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&byte_rate.to_le_bytes());
    v.extend_from_slice(&block_align.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(data);
    v
}

/// A constant-value 16-bit PCM WAV of `frames` per-channel frames.
fn wav_const(rate: u32, channels: u16, value: i16, frames: u32) -> Vec<u8> {
    let mut data = Vec::new();
    for _ in 0..frames {
        for _ in 0..channels {
            data.extend_from_slice(&value.to_le_bytes());
        }
    }
    wav16_pcm(rate, channels, &data)
}

/// A sine WAV with a (possibly distinct) frequency per channel, `amp` = 16-bit
/// peak amplitude.
fn wav_sine(rate: u32, channels: u16, freqs: &[f64], amp: i16, frames: u32) -> Vec<u8> {
    assert_eq!(freqs.len(), channels as usize);
    let mut data = Vec::new();
    for n in 0..frames {
        for &f in freqs {
            let t = f64::from(n) / f64::from(rate);
            let v = (f64::from(amp) * (2.0 * std::f64::consts::PI * f * t).sin()).round() as i16;
            data.extend_from_slice(&v.to_le_bytes());
        }
    }
    wav16_pcm(rate, channels, &data)
}

/// A unique temp path (tests run in parallel).
fn tmp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("soul_audio_cli_e2e_{name}"))
}

/// Read a file of raw interleaved i32 LE PCM back into samples.
fn read_pcm(path: &std::path::Path) -> Vec<i32> {
    std::fs::read(path)
        .unwrap()
        .chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Canonical 24-bit value the pipeline produces from a 16-bit source value
/// (normalized by `2^(24-16)`).
fn canon(v16: i16) -> i32 {
    i32::from(v16) << 8
}

/// Extract channel `ch` of the interleaved i32 PCM as f32, skipping `skip`
/// frames (past any resampler warm-up) then taking `n`.
fn channel_window(pcm: &[i32], channels: usize, ch: usize, skip: usize, n: usize) -> Vec<f32> {
    pcm.iter()
        .skip(ch)
        .step_by(channels)
        .skip(skip)
        .take(n)
        .map(|&s| s as f32)
        .collect()
}

/// Result of a spectral analysis: the dominant tone and how clean it is.
struct Spectrum {
    peak_hz: f64,
    /// Spurious-free dynamic range: the dominant tone vs the largest spurious
    /// peak outside a guard band around it. Aliasing/imaging artifacts show up as
    /// spurious peaks and drop this; the analysis window's own leakage is
    /// localized near the tone and excluded by the guard band.
    sfdr_db: f64,
}

/// Hann-windowed magnitude spectrum of `signal` → its dominant frequency and its
/// spurious-free dynamic range.
fn analyze(signal: &[f32], rate: u32) -> Spectrum {
    let n = signal.len();
    let mut buf: Vec<Complex<f32>> = signal
        .iter()
        .enumerate()
        .map(|(i, &x)| {
            // Hann window to suppress spectral leakage.
            let w = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (n as f64 - 1.0)).cos();
            Complex {
                re: x * w as f32,
                im: 0.0,
            }
        })
        .collect();
    FftPlanner::<f32>::new()
        .plan_fft_forward(n)
        .process(&mut buf);

    let half = n / 2;
    let mag: Vec<f64> = buf[..half].iter().map(|c| f64::from(c.norm())).collect();
    // Dominant bin, skipping DC and the lowest window-leakage bins.
    let lo = 3;
    let peak_bin = mag
        .iter()
        .enumerate()
        .skip(lo)
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
        .map(|(b, _)| b)
        .unwrap();
    let peak_hz = peak_bin as f64 * f64::from(rate) / n as f64;
    // Largest spurious peak outside a guard band (±16 bins) around the tone —
    // far enough that Hann leakage is well below any real artifact.
    let guard = 16;
    let mut spur = 0f64;
    for (b, &m) in mag.iter().enumerate().skip(lo) {
        if b >= peak_bin.saturating_sub(guard) && b <= peak_bin + guard {
            continue;
        }
        spur = spur.max(m);
    }
    let sfdr_db = 20.0 * (mag[peak_bin] / spur.max(1e-12)).log10();
    Spectrum { peak_hz, sfdr_db }
}

#[test]
fn render_gapless_concatenates_tracks_exactly() {
    // Two equal-rate/equal-channel constant tracks → the pipeline is a pure
    // passthrough, so the render is the two tracks back-to-back with no gap and
    // exact sample values.
    let a = tmp("gap_a.wav");
    let b = tmp("gap_b.wav");
    let out = tmp("gap_out.pcm");
    std::fs::write(&a, wav_const(48_000, 2, 1000, 500)).unwrap();
    std::fs::write(&b, wav_const(48_000, 2, 2000, 300)).unwrap();

    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .args(["render"])
        .arg(&a)
        .arg(&b)
        .args(["--out"])
        .arg(&out)
        .args(["--rate", "48000", "--channels", "2"])
        .assert()
        .success();

    let pcm = read_pcm(&out);
    // 500 + 300 frames, stereo → 1600 samples, no gap.
    assert_eq!(pcm.len(), (500 + 300) * 2);
    assert!(pcm[..1000].iter().all(|&s| s == canon(1000)), "track A");
    assert!(pcm[1000..].iter().all(|&s| s == canon(2000)), "track B");
}

#[test]
fn render_upsamples_44100_to_48000() {
    // A 44.1 kHz constant (DC) source resampled to 48 kHz: ~ratio more frames,
    // non-silent, and DC is preserved (a settled mid-sample matches the input).
    let a = tmp("up_a.wav");
    let out = tmp("up_out.pcm");
    let frames_in = 4410u32; // 0.1 s
    std::fs::write(&a, wav_const(44_100, 2, 500, frames_in)).unwrap();

    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .args(["render"])
        .arg(&a)
        .args(["--out"])
        .arg(&out)
        .args(["--rate", "48000", "--channels", "2"])
        .assert()
        .success();

    let pcm = read_pcm(&out);
    let frames_out = pcm.len() / 2;
    let expect = (f64::from(frames_in) * 48_000.0 / 44_100.0) as usize; // ~4800
    assert!(
        frames_out.abs_diff(expect) <= 1024,
        "resampled frames {frames_out} vs ~{expect}"
    );
    // A settled mid-sample preserves the DC level (500 << 8 = 128000).
    let mid = pcm[frames_out]; // ~halfway, past the resampler warm-up
    assert!((mid - canon(500)).abs() <= 64, "DC not preserved: {mid}");
}

#[test]
fn render_downsamples_48000_to_44100() {
    // Downsampling exercises a different resampler regime (larger per-block input
    // demand) than upsampling.
    let a = tmp("down_a.wav");
    let out = tmp("down_out.pcm");
    let frames_in = 4800u32; // 0.1 s
    std::fs::write(&a, wav_const(48_000, 2, 700, frames_in)).unwrap();

    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .args(["render"])
        .arg(&a)
        .args(["--out"])
        .arg(&out)
        .args(["--rate", "44100", "--channels", "2"])
        .assert()
        .success();

    let pcm = read_pcm(&out);
    let frames_out = pcm.len() / 2;
    let expect = (f64::from(frames_in) * 44_100.0 / 48_000.0) as usize; // ~4410
    assert!(
        frames_out.abs_diff(expect) <= 1024,
        "resampled frames {frames_out} vs ~{expect}"
    );
    let mid = pcm[frames_out];
    assert!((mid - canon(700)).abs() <= 64, "DC not preserved: {mid}");
}

#[test]
fn render_duplicates_mono_source_to_stereo() {
    // Mono source → stereo output: each frame's two samples are equal, and the
    // frame count is unchanged (no resampling at equal rate).
    let a = tmp("mono_a.wav");
    let out = tmp("mono_out.pcm");
    std::fs::write(&a, wav_const(48_000, 1, 300, 400)).unwrap();

    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .args(["render"])
        .arg(&a)
        .args(["--out"])
        .arg(&out)
        .args(["--rate", "48000", "--channels", "2"])
        .assert()
        .success();

    let pcm = read_pcm(&out);
    assert_eq!(pcm.len(), 400 * 2);
    assert!(
        pcm.iter().all(|&s| s == canon(300)),
        "L and R both the source"
    );
}

#[test]
fn render_rejects_incompatible_channels() {
    // A stereo source into a mono output is neither passthrough nor mono→stereo,
    // so the pipeline rejects it cleanly (rather than producing garbage).
    let a = tmp("bad_a.wav");
    let out = tmp("bad_out.pcm");
    std::fs::write(&a, wav_const(48_000, 2, 1000, 100)).unwrap();

    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .args(["render"])
        .arg(&a)
        .args(["--out"])
        .arg(&out)
        .args(["--rate", "48000", "--channels", "1"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("channel"));
}

// --- signal analysis: what a human ear checks, measured automatically ---------

#[test]
fn render_preserves_tone_pitch_level_and_purity() {
    // A 1 kHz tone resampled 44.1k → 48k must stay a 1 kHz tone. A wrong resample
    // ratio would shift the pitch (the audible "chipmunk"/slow-tape bug an ear
    // catches); it must also stay clean (resampler SNR) and keep its level.
    let a = tmp("tone_a.wav");
    let out = tmp("tone_out.pcm");
    let amp = 16_383i16; // ~0.5 full scale
    std::fs::write(&a, wav_sine(44_100, 2, &[1000.0, 1000.0], amp, 22_050)).unwrap();

    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .args(["render"])
        .arg(&a)
        .args(["--out"])
        .arg(&out)
        .args(["--rate", "48000", "--channels", "2"])
        .assert()
        .success();

    let pcm = read_pcm(&out);
    let n = 16_384;
    let sig = channel_window(&pcm, 2, 0, 4000, n); // skip warm-up, then a full window
    assert_eq!(sig.len(), n, "need a full analysis window of output");

    let s = analyze(&sig, 48_000);
    assert!(
        (s.peak_hz - 1000.0).abs() < 5.0,
        "pitch drifted to {:.1} Hz (resample ratio wrong?)",
        s.peak_hz
    );
    // Measured ~84 dB for the clean Hq resampler; 55 dB is a wide margin that
    // still fails on real aliasing (which lands at 20–40 dB).
    assert!(
        s.sfdr_db > 55.0,
        "tone not clean: {:.1} dB SFDR (aliasing/artifacts?)",
        s.sfdr_db
    );
    // Level preserved end-to-end: the settled peak ≈ input amplitude, canonical.
    let peak = sig.iter().fold(0f32, |m, &x| m.max(x.abs()));
    let expect = f64::from(i32::from(amp) << 8);
    assert!(
        (f64::from(peak) - expect).abs() < expect * 0.05,
        "level off: {peak} vs ~{expect}"
    );
}

#[test]
fn render_keeps_stereo_channels_distinct() {
    // Left = 1 kHz, right = 4 kHz through a passthrough render: each channel must
    // carry its own tone — catches a channel swap or a mono collapse.
    let a = tmp("stereo_a.wav");
    let out = tmp("stereo_out.pcm");
    std::fs::write(&a, wav_sine(48_000, 2, &[1000.0, 4000.0], 16_383, 24_000)).unwrap();

    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .args(["render"])
        .arg(&a)
        .args(["--out"])
        .arg(&out)
        .args(["--rate", "48000", "--channels", "2"])
        .assert()
        .success();

    let pcm = read_pcm(&out);
    let n = 16_384;
    let left = analyze(&channel_window(&pcm, 2, 0, 2000, n), 48_000);
    let right = analyze(&channel_window(&pcm, 2, 1, 2000, n), 48_000);
    assert!(
        (left.peak_hz - 1000.0).abs() < 5.0,
        "left channel is {:.1} Hz, not 1000",
        left.peak_hz
    );
    assert!(
        (right.peak_hz - 4000.0).abs() < 5.0,
        "right channel is {:.1} Hz, not 4000",
        right.peak_hz
    );
}

// --- committed per-format fixtures decoded by our own decoders ----------------

/// Path to a committed `testdata/` fixture (relative to the workspace).
fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(name)
}

/// Decode a fixture through the CLI and return the raw i32 LE PCM bytes.
fn decode_fixture(name: &str) -> Vec<u8> {
    let out = tmp(&format!("fx_{name}.pcm"));
    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .arg("decode")
        .arg(fixture(name))
        .arg("--out")
        .arg(&out)
        .assert()
        .success();
    std::fs::read(&out).unwrap()
}

#[test]
fn real_flac_decodes_losslessly_vs_wav() {
    // The fixtures carry the same signal; our from-scratch FLAC decoder must
    // reproduce the reference WAV bit-for-bit.
    assert_eq!(
        decode_fixture("tone.flac"),
        decode_fixture("tone.wav"),
        "FLAC decode is not lossless vs the reference WAV"
    );
}

#[test]
fn real_ogg_flac_decodes_losslessly_vs_wav() {
    assert_eq!(
        decode_fixture("tone.ogg"),
        decode_fixture("tone.wav"),
        "Ogg-FLAC decode is not lossless vs the reference WAV"
    );
}

#[test]
fn real_mp3_decodes_to_plausible_audio() {
    let pcm = decode_fixture("tone.mp3");
    let samples: Vec<i32> = pcm
        .chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    // ~2 s stereo @ 44.1 kHz = 176_400 samples, ± MP3 encoder/decoder delay.
    assert!(
        (samples.len() as i64 - 176_400).abs() < 20_000,
        "unexpected MP3 length: {}",
        samples.len()
    );
    assert!(
        samples.iter().any(|&s| s.abs() > 1000),
        "MP3 decoded to silence"
    );
}

#[test]
fn real_dsd_decodes_decimates_and_preserves_stereo_tones() {
    // A real DSF (DSD64) all the way through: decode 1-bit → decimate to PCM →
    // resample to the device rate. The A4-left / E5-right tones must come back
    // cleanly on the correct channels — this exercises the whole DSD path.
    let out = tmp("dsd_out.pcm");
    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .args(["render"])
        .arg(fixture("tone.dsf"))
        .args(["--out"])
        .arg(&out)
        .args(["--rate", "48000", "--channels", "2"])
        .assert()
        .success();

    let pcm = read_pcm(&out);
    let n = 16_384;
    let left = analyze(&channel_window(&pcm, 2, 0, 4000, n), 48_000);
    let right = analyze(&channel_window(&pcm, 2, 1, 4000, n), 48_000);
    assert!(
        (left.peak_hz - 440.0).abs() < 6.0,
        "DSD left tone {:.1} Hz, not 440",
        left.peak_hz
    );
    assert!(
        (right.peak_hz - 659.25).abs() < 6.0,
        "DSD right tone {:.1} Hz, not 659",
        right.peak_hz
    );
    // Well clear of aliasing (measured ~78-83 dB; the modulator noise is shaped
    // out of band).
    assert!(
        left.sfdr_db > 40.0 && right.sfdr_db > 40.0,
        "DSD not clean: {:.0}/{:.0} dB",
        left.sfdr_db,
        right.sfdr_db
    );
}

#[test]
fn real_flac_plays_through_the_render_pipeline() {
    // A real FLAC all the way through decode → resample → normalize → file.
    let out = tmp("fx_flac_render.pcm");
    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .args(["render"])
        .arg(fixture("tone.flac"))
        .args(["--out"])
        .arg(&out)
        .args(["--rate", "48000", "--channels", "2"])
        .assert()
        .success();
    let pcm = read_pcm(&out);
    // 2 s resampled 44.1k → 48k ≈ 96_000 frames × 2 ch.
    let frames = pcm.len() / 2;
    assert!((frames as i64 - 96_000).abs() < 2000, "frames {frames}");
    assert!(pcm.iter().any(|&s| s.abs() > 1000), "render is silent");
}
