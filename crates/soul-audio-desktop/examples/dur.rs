//! Throwaway check: open a track via the app's `open_track` path and print the
//! decoder's reported PCM rate, total_frames, and derived duration.
//! Run: cargo run -p soul-audio-desktop --example dur -- <file>

use soul_audio_core::{Decoder, SeekableDecoder};
use soul_audio_desktop::open_track;

fn main() {
    // OwnedDecoder is ~150 KB by value (FLAC-dominated), so build/hold it on a
    // big-stack thread, mirroring the CLI.
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(run)
        .unwrap()
        .join()
        .unwrap();
}

fn run() {
    let path = std::env::args().nth(1).expect("usage: dur <file>");
    let d = open_track(std::path::Path::new(&path)).expect("open");
    let si = d.stream_info();
    let total = d.total_frames();
    let dur = total.map(|f| f as f64 / f64::from(si.sample_rate));
    println!(
        "rate={}Hz ch={} total_frames={:?} duration={:?}s",
        si.sample_rate, si.channels, total, dur
    );
}
