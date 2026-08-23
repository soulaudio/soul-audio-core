//! Black-box E2E: run the built CLI binary and check its behaviour.
//!
//! TEMPLATE: decoders are stubs, so `decode` is expected to fail with a clear
//! message. Once a real decoder lands, extend this to assert the emitted PCM
//! matches a golden fixture.

use assert_cmd::Command;

#[test]
fn help_runs() {
    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .arg("--help")
        .assert()
        .success();
}

#[test]
fn decode_reports_stub_cleanly() {
    let dir = std::env::temp_dir();
    let input = dir.join("soul_audio_cli_e2e_in.bin");
    let out = dir.join("soul_audio_cli_e2e_out.pcm");
    std::fs::write(&input, b"not a real audio file").unwrap();

    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .arg("decode")
        .arg(&input)
        .arg("--out")
        .arg(&out)
        .assert()
        .failure()
        .stderr(predicates::str::contains("stubs in this template"));
}
