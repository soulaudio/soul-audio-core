//! Black-box E2E: run the built CLI binary and check its behaviour.

use assert_cmd::Command;

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

/// A constant-value 16-bit PCM WAV of `frames` per-channel frames.
fn wav_const(rate: u32, channels: u16, value: i16, frames: u32) -> Vec<u8> {
    let mut data = Vec::new();
    for _ in 0..frames {
        for _ in 0..channels {
            data.extend_from_slice(&value.to_le_bytes());
        }
    }
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
    v.extend_from_slice(&data);
    v
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
