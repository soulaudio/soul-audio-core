//! Vorbis decode-vs-real-time on a dense (broadband-noise) fixture — the #34
//! residue hot path with the FAST_HUFFMAN acceleration active. `#[ignore]`d: the
//! ratio is machine-dependent, so this is a developer measurement (run with
//! `--ignored --nocapture`), not a CI gate.
//!
//!   cargo test -p soul-audio-cli --release --test vorbis_realtime -- --ignored --nocapture

use std::path::PathBuf;
use std::time::Instant;

use soul_audio_core::Decoder;
use soul_audio_desktop::open_track;

fn testdata(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(name)
}

#[test]
#[ignore = "machine-dependent timing; run with --ignored --nocapture"]
fn vorbis_dense_decode_realtime_ratio() {
    let path = testdata("tone-vorbis-dense.ogg");
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let mut d = open_track(&path).expect("open");
            let rate = d.stream_info().sample_rate as f64;
            let ch = d.stream_info().channels.max(1) as usize;
            let mut buf = [0i32; 8192];
            let mut frames = 0u64;
            let t0 = Instant::now();
            loop {
                let n = d.decode_into(&mut buf).expect("decode");
                if n == 0 {
                    break;
                }
                frames += (n / ch) as u64;
            }
            let ratio = t0.elapsed().as_secs_f64() / (frames as f64 / rate);
            println!("tone-vorbis-dense.ogg: decode/realtime = {ratio:.3}x (fast-Huffman)");
        })
        .unwrap()
        .join()
        .unwrap();
}
