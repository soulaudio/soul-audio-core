//! WASAPI **exclusive** (bit-perfect) hardware test suite — Windows, `wasapi-exclusive`
//! feature, `#[ignore]`d (touches real output devices, so CI can't run it).
//!
//! Run on a machine with real output devices:
//! ```text
//! cargo test -p soul-audio-desktop --features wasapi-exclusive \
//!     --test wasapi_exclusive_suite -- --ignored --nocapture
//! ```
//!
//! WASAPI exclusive is **one-client-per-device**, so everything here runs as a SINGLE
//! sequential test — splitting it into parallel tests makes them fight over the device
//! and report spurious `DeviceInUse`. Unlike `wasapi_device_probe` (default device only),
//! this walks EVERY output device, probes exclusive support across the standard rate
//! ladder, and — on each device that supports it — exercises open/write/flush and the
//! native-rate reopen (release + re-acquire) path. Writes SILENCE only; each exclusive
//! open briefly takes the device.

#![cfg(all(windows, feature = "wasapi-exclusive"))]

use soul_audio_core::Sink;
use soul_audio_desktop::{
    WasapiExclusiveSink, default_output_config, default_output_device_name, output_device_names,
};

/// The standard sample-rate ladder DACs advertise for exclusive mode.
const RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

/// Open `name` exclusively at `rate`; `Ok(negotiated_rate)` or `Err(reason)`. Releases the
/// device immediately.
fn probe(name: &str, rate: u32, ch: u16) -> Result<u32, String> {
    match WasapiExclusiveSink::open(Some(name.to_string()), rate, ch) {
        Ok(s) => {
            let r = s.stream_info().sample_rate;
            drop(s);
            Ok(r)
        }
        Err(e) => Err(format!("{e:?}")),
    }
}

#[test]
#[ignore = "touches real output devices; run with --ignored"]
fn wasapi_exclusive_capability_and_paths() {
    let devices = output_device_names();
    let default = default_output_device_name();
    println!(
        "\n=== {} output device(s); default = {default:?} ===",
        devices.len()
    );
    assert!(
        !devices.is_empty() || default.is_some(),
        "no output devices enumerated at all"
    );

    let mut exclusive_devices = 0usize;
    let mut open_write_ok = 0usize;
    let mut reopen_ok = 0usize;

    for name in &devices {
        let marker = if Some(name) == default.as_ref() {
            "  (default)"
        } else {
            ""
        };
        let Some((def_rate, ch)) = default_output_config(Some(name)) else {
            println!("\n[{name}]{marker}  unavailable (no shared config) — skipped");
            continue;
        };
        println!("\n[{name}]{marker}  shared mix-format: {def_rate} Hz {ch}ch");

        // 1) Capability matrix: which exclusive rates does the device accept?
        let mut supported: Vec<u32> = Vec::new();
        for &r in &RATES {
            match probe(name, r, ch) {
                Ok(neg) => {
                    supported.push(neg);
                    println!("    {r:>7} Hz: EXCLUSIVE OK (negotiated {neg} Hz)");
                }
                Err(e) => println!("    {r:>7} Hz: {e}"),
            }
        }
        if supported.is_empty() {
            println!("    -> no exclusive support on this device");
            continue;
        }
        exclusive_devices += 1;

        // 2) open / write / flush at the native shared-mix rate (the rate a DAC also takes
        //    exclusively) — push silence through the real event-driven render loop.
        match WasapiExclusiveSink::open(Some(name.clone()), def_rate, ch) {
            Ok(mut sink) => {
                let si = sink.stream_info();
                let block = vec![0i32; si.channels as usize * 2048];
                for _ in 0..4 {
                    sink.write(&block)
                        .expect("write silence to exclusive device");
                }
                sink.flush().expect("flush exclusive device");
                drop(sink);
                open_write_ok += 1;
                println!("    open/write/flush @ {def_rate} Hz: OK ({:?})", si.format);
            }
            Err(e) => println!("    open @ {def_rate} Hz failed: {e:?}"),
        }

        // 3) native-rate reopen (release + re-acquire) between two supported rates — the
        //    bit-perfect switch path; must land on each rate without AUDCLNT_E_DEVICE_IN_USE.
        if let Some(&other) = supported.iter().find(|&&r| r != def_rate) {
            if let Ok(mut sink) = WasapiExclusiveSink::open(Some(name.clone()), def_rate, ch) {
                sink.reopen(other).expect("reopen to the other rate");
                assert_eq!(
                    sink.stream_info().sample_rate,
                    other,
                    "reopen must land on `other`"
                );
                sink.reopen(def_rate).expect("reopen back");
                assert_eq!(sink.stream_info().sample_rate, def_rate, "reopen back");
                reopen_ok += 1;
                println!("    native-rate reopen {def_rate} <-> {other} Hz: OK (no device-in-use)");
            }
        } else {
            println!("    only one exclusive rate — native-rate reopen not exercisable here");
        }
    }

    println!(
        "\n=== SUMMARY: {exclusive_devices}/{} device(s) support exclusive; \
         open/write OK on {open_write_ok}; native-rate reopen OK on {reopen_ok} ===",
        devices.len()
    );
    if exclusive_devices == 0 {
        println!(
            "No device supports exclusive — every output here falls back to shared. Bit-perfect \
             exclusive output needs a device with 'Allow applications to take exclusive control' \
             enabled and a supported format."
        );
    }
}
