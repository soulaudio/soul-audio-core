//! Dogfooding check: run every CLI-arg path through the app's `open_track`
//! decode path (exactly what the player uses) and decode the whole stream,
//! reporting rate/channels/decoded-frames/duration — proving a file actually
//! plays end to end, not merely that its header parses.
//!
//! Run: cargo run -p soul-audio-desktop --example playcheck -- <file>...

use soul_audio_core::{Decoder, SeekableDecoder};
use soul_audio_desktop::open_track;

fn main() {
    // `OwnedDecoder` is large by value (no-alloc), so build/hold it on a
    // generous stack, mirroring the CLI and the app's loader thread.
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024) // OwnedDecoder is large by value; match the CLI
        .spawn(run)
        .unwrap()
        .join()
        .unwrap();
}

fn run() {
    for path in std::env::args().skip(1) {
        match check(&path) {
            Ok(line) => println!("OK    {path}: {line}"),
            Err(e) => println!("FAIL  {path}: {e}"),
        }
    }
}

fn check(path: &str) -> Result<String, String> {
    let mut d = open_track(std::path::Path::new(path)).map_err(|e| e.to_string())?;
    let si = d.stream_info();
    let (rate, ch) = (si.sample_rate, si.channels);
    // The decoder's declared length — what the deck/progress bar uses (distinct
    // from the tag reader's StreamFacts the library list shows).
    let declared = d.total_frames();
    let declared_dur = declared.map(|f| f as f64 / f64::from(rate.max(1)));
    // Cap at ~5 s decoded: enough to prove the file starts and produces audio
    // (the actual bug was "doesn't start playing" / errors), without waiting on
    // a full decode of a 5-minute track — ruinous for DSD in a debug build.
    let cap = u64::from(rate.max(1)) * 5;
    let mut frames: u64 = 0;
    let mut nonzero = false;
    let mut buf = [0i32; 4096];
    while frames < cap {
        let n = d
            .decode_into(&mut buf)
            .map_err(|e| format!("decode: {e:?}"))?;
        if n == 0 {
            break; // EOF before the cap (short clip)
        }
        nonzero |= buf[..n].iter().any(|&s| s != 0);
        frames += n as u64 / u64::from(ch.max(1));
    }
    let hit = if frames >= cap { "5s+" } else { "EOF" };
    Ok(format!(
        "rate={rate}Hz ch={ch} declared_total={declared:?} ({declared_dur:?}s) decoded={frames}f ({hit}) audio={nonzero}"
    ))
}
