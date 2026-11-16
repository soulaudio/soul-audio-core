//! Real-device WASAPI **exclusive** probe. `#[ignore]`-gated so CI never runs it
//! (CI machines are headless / have no usable output device); run it manually on a
//! machine with a real output device:
//!
//! ```text
//! cargo test -p soul-audio-desktop --features wasapi-exclusive \
//!     --test wasapi_device_probe -- --ignored --nocapture
//! ```
//!
//! It opens the real **default** output device in exclusive mode, reports the
//! negotiated format, reopens it at a different rate (the native-rate-switch path on
//! hardware), and maps which standard rates the device accepts exclusively. It only
//! ever writes SILENCE, so nothing audible is produced; exclusive mode does briefly
//! take over the device.
//!
//! What this proves: the WASAPI open / reopen / format-negotiation path works on real
//! hardware. What it does NOT prove: digital bit-exactness (that needs a DAC/loopback
//! to capture the digital output and compare it to the source).
#![cfg(all(windows, feature = "wasapi-exclusive"))]

use soul_audio_core::Sink;
use soul_audio_desktop::{ExclusiveError, WasapiExclusiveSink, default_output_config};

#[test]
#[ignore = "touches the real default output device; run manually with --ignored"]
fn probe_exclusive_open_reopen_and_formats() {
    let (rate, channels) = match default_output_config(None) {
        Some(c) => c,
        None => {
            println!("NO DEFAULT OUTPUT DEVICE — nothing to probe");
            return;
        }
    };
    println!("\n=== default output device: {rate} Hz, {channels} ch ===");

    // 1. Open exclusive at the device's native (shared mix-format) rate.
    println!("\n[1] exclusive open @ native {rate} Hz:");
    match WasapiExclusiveSink::open(None, rate, channels) {
        Ok(mut sink) => {
            let si = sink.stream_info();
            println!(
                "    OK — negotiated {si:?}, exclusive()={}",
                sink.exclusive()
            );
            // Push a few blocks of SILENCE through the real event-driven render loop,
            // then flush + drop (exercises write_to_device on hardware, no sound).
            let block = vec![0i32; channels as usize * 2048];
            for _ in 0..4 {
                sink.write(&block).expect("write silence");
            }
            sink.flush().expect("flush");
            drop(sink);
            println!("    wrote 4 silent blocks + flushed + closed OK");
        }
        Err(e) => println!("    FAILED: {e}  ({e:?})"),
    }

    // 2. Reopen at a DIFFERENT standard rate — the native-rate-switch path on hardware.
    let other = if rate == 44_100 { 48_000 } else { 44_100 };
    println!("\n[2] exclusive reopen @ {other} Hz (native-rate-switch path):");
    match WasapiExclusiveSink::open(None, other, channels) {
        Ok(sink) => println!("    OK — {:?}", sink.stream_info()),
        Err(ExclusiveError::Unsupported) => {
            println!("    Unsupported — the fallback would keep the current sink + resample")
        }
        Err(e) => println!("    {e}  ({e:?})"),
    }

    // 3. Map the device's exclusive capability across common rates.
    println!("\n[3] exclusive capability across standard rates:");
    for r in [44_100u32, 48_000, 88_200, 96_000, 176_400, 192_000] {
        match WasapiExclusiveSink::open(None, r, channels) {
            Ok(s) => println!("    {r:>7} Hz: OK  ({:?})", s.stream_info().format),
            Err(e) => println!("    {r:>7} Hz: {e:?}"),
        }
    }
    println!();
}
