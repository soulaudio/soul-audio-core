//! DSD decode-vs-real-time headroom, per rate. `#[ignore]`d: the ratio is
//! machine-dependent, so this is a developer measurement (run with `--ignored
//! --nocapture`), not a CI gate. The finding is recorded in testdata/README.md.
//!
//!   cargo test -p soul-audio-cli --release --test dsd_realtime -- --ignored --nocapture

use std::path::PathBuf;
use std::time::Instant;

use soul_audio_core::Decoder;
use soul_audio_desktop::open_track;

fn testdata(name: &str) -> PathBuf {
    // crates/soul-audio-cli -> repo-root/testdata
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(name)
}

/// Fully decode `name`, returning decode_time / audio_time (< 1.0 = faster than
/// real-time). Runs on a big stack (the `OwnedDecoder` enum is ~150 KiB).
fn ratio(name: &str) -> f64 {
    let path = testdata(name);
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let mut d = open_track(&path).expect("open");
            let rate = d.stream_info().sample_rate as f64;
            let channels = d.stream_info().channels.max(1) as usize;
            let mut buf = [0i32; 8192];
            let mut frames = 0u64;
            let t0 = Instant::now();
            loop {
                let n = d.decode_into(&mut buf).expect("decode");
                if n == 0 {
                    break;
                }
                frames += (n / channels) as u64;
            }
            let decode_s = t0.elapsed().as_secs_f64();
            let audio_s = frames as f64 / rate;
            decode_s / audio_s
        })
        .unwrap()
        .join()
        .unwrap()
}

#[test]
#[ignore = "machine-dependent timing; run with --ignored --nocapture"]
fn dsd_decode_realtime_ratio_per_rate() {
    for name in [
        "tone.dsf",
        "tone-dsd128.dsf",
        "tone-dsd256.dsf",
        "tone-dsd512.dsf",
    ] {
        println!("{name}: decode/realtime = {:.3}x", ratio(name));
    }
}
