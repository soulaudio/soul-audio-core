//! Hardware-dependent checks for output-device enumeration and selection.
//!
//! `#[ignore]`d — they open a real audio device (a headless CI machine has
//! none). The whole file is gated on the `playback` feature (the enumeration
//! API lives behind it). Run locally with:
//!
//! ```text
//! cargo test -p soul-audio-desktop --features playback --test device_selection -- --ignored --nocapture
//! ```
#![cfg(feature = "playback")]

use soul_audio_core::Sink;
use soul_audio_desktop::{CpalSink, default_output_device_name, output_device_names};

#[test]
#[ignore = "needs a real output device; run with --features playback -- --ignored"]
fn enumerates_and_opens_default_by_name() {
    let names = output_device_names();
    assert!(!names.is_empty(), "no output devices enumerated");
    let default = default_output_device_name().expect("a default output device exists");
    assert!(
        names.contains(&default),
        "default {default:?} not among {names:?}"
    );

    // Opening the default *by its name* must produce the same stream as opening
    // the default directly — proving name-based selection resolves correctly.
    let by_name = CpalSink::new_on_device(&default).expect("open default device by name");
    let by_default = CpalSink::new().expect("open default device");
    assert_eq!(
        by_name.stream_info().sample_rate,
        by_default.stream_info().sample_rate,
        "named-default and default sinks must agree on sample rate"
    );
    assert_eq!(
        by_name.stream_info().channels,
        by_default.stream_info().channels,
        "named-default and default sinks must agree on channel count"
    );
    println!("output devices: {names:?}");
    println!("default: {default}");
}

#[test]
#[ignore = "needs a real output device; run with --features playback -- --ignored"]
fn unknown_device_name_errors() {
    assert!(
        CpalSink::new_on_device("<<no such output device>>").is_err(),
        "opening a nonexistent device must error, not fall through"
    );
}
