//! Real-device check that `WasapiExclusiveSink::reopen` **releases then re-acquires**
//! the SAME exclusive device in place — the fix for the native-rate switch, which
//! previously opened a second exclusive client alongside the first and failed with
//! `AUDCLNT_E_DEVICE_IN_USE`. `#[ignore]`d (touches the real device); Windows only.
//!
//! ```text
//! cargo test -p soul-audio-desktop --features wasapi-exclusive \
//!     --test wasapi_reopen_probe -- --ignored --nocapture
//! ```
#![cfg(all(windows, feature = "wasapi-exclusive"))]

use soul_audio_core::Sink;
use soul_audio_desktop::{WasapiExclusiveSink, default_output_config};

#[test]
#[ignore = "touches the real default output device; run manually with --ignored"]
fn reopen_in_place_releases_and_reacquires_without_device_in_use() {
    let Some((rate, channels)) = default_output_config(None) else {
        println!("no default output device — skipping");
        return;
    };
    let mut sink = WasapiExclusiveSink::open(None, rate, channels).expect("open @ native rate");
    println!("opened @ {:?}", sink.stream_info());
    let block = vec![0i32; channels as usize * 1024]; // silence
    sink.write(&block).expect("write");

    // Reopen IN PLACE at a different rate — the exact scenario that used to fail with
    // device-in-use (a second exclusive client). It must release the current stream
    // first, so this succeeds.
    let other = if rate == 44_100 { 48_000 } else { 44_100 };
    sink.reopen(other)
        .expect("in-place reopen must NOT be device-in-use");
    println!("reopened @ {:?}", sink.stream_info());
    assert_eq!(
        sink.stream_info().sample_rate,
        other,
        "reopened at the requested rate"
    );
    sink.write(&block).expect("write after reopen");
    drop(sink);
    println!("OK — in-place reopen released + reacquired the device, no device-in-use");
}
