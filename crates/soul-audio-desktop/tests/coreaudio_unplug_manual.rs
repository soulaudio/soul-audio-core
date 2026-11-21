//! **Manual** hardware test: unplugging the output device mid-play makes BOTH macOS sinks
//! fail fast with `SinkError::Closed` (so the player pauses, Apple Music style) instead
//! of hanging (exclusive) or silently moving to the laptop speakers (shared).
//!
//! Needs a person: start it, then unplug the named device (wired headphones on Apple
//! silicon, a USB DAC, or disconnect AirPods) within 30 s. Plays SILENCE only.
//!
//! ```text
//! SOUL_UNPLUG_DEVICE="External Headphones" SOUL_UNPLUG_MODE=shared \
//! cargo test -p soul-audio-desktop --features coreaudio-exclusive \
//!     --test coreaudio_unplug_manual -- --ignored --nocapture
//! ```
//! `SOUL_UNPLUG_MODE` = `shared` (default) or `exclusive`.
#![cfg(all(target_os = "macos", feature = "coreaudio-exclusive"))]

use std::time::{Duration, Instant};

use soul_audio_core::{Sink, SinkError};
use soul_audio_desktop::{CoreAudioExclusiveSink, CpalSink};

#[test]
#[ignore = "needs a person to unplug a device; see the module docs"]
fn unplugging_the_device_mid_play_reports_closed() {
    let Ok(name) = std::env::var("SOUL_UNPLUG_DEVICE") else {
        eprintln!("SKIP: set SOUL_UNPLUG_DEVICE to the device you will unplug");
        return;
    };
    let exclusive = std::env::var("SOUL_UNPLUG_MODE").is_ok_and(|m| m == "exclusive");
    let mut sink: Box<dyn Sink> = if exclusive {
        Box::new(CoreAudioExclusiveSink::open(Some(name.clone()), 48_000, 2).expect("open"))
    } else {
        Box::new(CpalSink::new_on_device(&name).expect("open"))
    };
    let ch = sink.stream_info().channels as usize;
    let block = vec![0i32; 1024 * ch];
    println!(
        "playing silence on {name:?} ({}) — UNPLUG IT NOW (30 s)…",
        if exclusive { "exclusive" } else { "shared" }
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match sink.write(&block) {
            Ok(()) => {}
            Err(e) => {
                assert_eq!(e, SinkError::Closed);
                println!("OK: write reported {e:?} after unplug");
                assert_eq!(sink.flush(), Err(SinkError::Closed));
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "device never reported gone — was it unplugged?"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
