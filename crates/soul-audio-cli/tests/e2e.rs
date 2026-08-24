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
