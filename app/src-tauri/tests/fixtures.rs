//! Validates the committed testdata fixtures. Playable formats (incl. the DSD
//! rates and raw-DSD DSDIFF) decode to non-silent PCM; the tag-path fixtures
//! (Opus duration, embedded-art FLAC) validate through the `Library::scan` path
//! the app uses. Opus decodes fine in the core now; it is simply exercised in
//! the CLI e2e suite rather than re-decoded here.
use std::path::PathBuf;

use soul_audio_core::Decoder;
use soul_audio_desktop::decoder::open_track;
use soul_audio_service::Library;

fn testdata(name: &str) -> PathBuf {
    // app/src-tauri/tests -> repo-root/testdata
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(name)
}

/// Decode a whole (short) fixture on a big stack; return (samples, peak_abs, rate).
fn decode_all(name: &str) -> (u64, i32, u32) {
    let path = testdata(name);
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let mut d = open_track(&path).expect("open_track");
            let rate = d.stream_info().sample_rate;
            let mut buf = [0i32; 8192];
            let (mut samples, mut peak) = (0u64, 0i32);
            loop {
                let n = d.decode_into(&mut buf).expect("decode_into");
                if n == 0 {
                    break;
                }
                samples += n as u64;
                for &s in &buf[..n] {
                    peak = peak.max(s.abs());
                }
            }
            (samples, peak, rate)
        })
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn playable_fixtures_decode_nonsilent() {
    // tone.opus decodes too; it is covered in the CLI e2e suite, omitted here.
    for name in [
        "tone.wav",
        "tone.flac",
        "tone.mp3",
        "tone.m4a",
        "tone-aac.m4a",
        "tone.aac",
        "tone-sbr.m4a",
        "tone-vorbis.ogg",
        "tone.dsf",
        "tone-dsd128.dsf",
        "tone.dff",
    ] {
        let (samples, peak, _) = decode_all(name);
        assert!(samples > 0, "{name}: decoded 0 samples");
        assert!(peak > 1000, "{name}: silent (peak {peak})");
    }
}

#[test]
fn dsd_rates_decimate_eight_to_one() {
    // Fixed 8:1 decimation: each DSD rate → rate / 8 PCM.
    assert_eq!(decode_all("tone.dsf").2, 352_800); // DSD64
    assert_eq!(decode_all("tone-dsd128.dsf").2, 705_600); // DSD128
    assert_eq!(decode_all("tone-dsd256.dsf").2, 1_411_200); // DSD256
    assert_eq!(decode_all("tone-dsd512.dsf").2, 2_822_400); // DSD512
    assert_eq!(decode_all("tone.dff").2, 352_800); // DSD64 via DSDIFF
}

#[test]
fn opus_duration_reads_from_tags() {
    // The library view's duration comes from the tags layer (the last Ogg-page
    // granule), independent of decoding — the path the app relies on. Scan a
    // folder with only tone.opus so the single view is unambiguous.
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(testdata("tone.opus"), dir.path().join("tone.opus")).unwrap();
    let views = Library::scan(dir.path()).expect("scan").views();
    assert_eq!(views.len(), 1);
    assert!(
        (views[0].duration_secs - 2.0).abs() < 0.15,
        "opus dur {}",
        views[0].duration_secs
    );
}

#[test]
fn arted_flac_reports_embedded_art() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(testdata("arted.flac"), dir.path().join("arted.flac")).unwrap();
    let views = Library::scan(dir.path()).expect("scan").views();
    assert_eq!(views.len(), 1);
    assert!(views[0].has_art, "arted.flac should report has_art");
}
