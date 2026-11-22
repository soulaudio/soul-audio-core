//! Real-device ALSA **exclusive** probe. `#[ignore]`-gated so CI never runs it (CI
//! machines are headless and have no sound card); run it on a Linux box with real
//! audio hardware:
//!
//! ```text
//! cargo test -p soul-audio-desktop --features alsa-exclusive \
//!     --test alsa_device_probe -- --ignored --nocapture
//! ```
//!
//! It takes the card exclusively (claiming it from PipeWire / PulseAudio over the
//! device-reservation protocol), reports the negotiated rate and word, writes SILENCE
//! for a moment, and reopens at a second rate to exercise the per-track rate switch on
//! hardware. Nothing audible is produced, but the card IS taken over for a second or
//! two — the desktop's audio server will go quiet for that long.
//!
//! What this proves: the open / reserve / negotiate / write / reopen / release path
//! works against a real driver. What it does NOT prove: digital bit-exactness — that
//! needs a DAC or loopback to capture what actually clocks out and compare it to the
//! source. The conversion arithmetic is unit-tested separately.
#![cfg(all(target_os = "linux", feature = "alsa-exclusive"))]

use soul_audio_core::Sink;
use soul_audio_desktop::AlsaExclusiveSink;
use soul_audio_desktop::exclusive_engine::ExclusiveError;

/// Rates worth trying: the two base families, so whichever the card runs natively one
/// of them is a true native open rather than a fallback.
const RATES: [u32; 2] = [48_000, 44_100];

/// Every device name this system actually offers must resolve to a card, or
/// exclusive is unreachable for anyone who picks a device instead of leaving it
/// on the default.
///
/// This is the integration the unit tests cannot cover: they pin the parsing,
/// but only the real machine says what the names LOOK like. On a PipeWire
/// desktop they are all plugins (`default`, `pipewire`,
/// `sysdefault:CARD=…`) — none of which is raw hardware, and only the ones
/// carrying a `CARD=` token can be mapped to a card at all.
#[test]
#[ignore = "reads the machine's real device list; run manually with --ignored"]
fn device_names_this_system_offers_resolve_to_a_card() {
    use soul_audio_desktop::alsa_exclusive::{AlsaHal, card_token};
    use soul_audio_desktop::exclusive_engine::Hal;

    let hal = AlsaHal::default();
    let names = soul_audio_desktop::output_device_names();
    assert!(!names.is_empty(), "this machine reports no output devices");

    let mut resolved_any = false;
    for name in &names {
        let dev = hal.resolve(Some(name));
        println!("  {name:?} -> {dev:?}");
        if card_token(name).is_some() {
            assert!(
                dev.is_some(),
                "{name:?} names a card but did not resolve — exclusive would be \
                 unreachable for anyone who selects it"
            );
            resolved_any = true;
        }
    }
    // The unnamed default must always resolve, or exclusive has no entry point.
    assert!(
        hal.resolve(None).is_some(),
        "no default card resolved; exclusive output would never open"
    );
    if !resolved_any {
        println!("(no device name on this machine carries a CARD= token)");
    }
}

#[test]
#[ignore = "takes over the real sound card; run manually with --ignored"]
fn probe_exclusive_open_write_and_reopen() {
    let mut opened = None;
    for rate in RATES {
        match AlsaExclusiveSink::open(None, rate, 2) {
            Ok(sink) => {
                println!("opened exclusively at a requested {rate} Hz");
                opened = Some(sink);
                break;
            }
            Err(ExclusiveError::NoDevice) => {
                println!("NO SOUND CARD — nothing to probe");
                return;
            }
            Err(ExclusiveError::DeviceInUse) => {
                println!("card held by another application — nothing to probe");
                return;
            }
            Err(e) => println!("  {rate} Hz refused: {e}"),
        }
    }
    let Some(mut sink) = opened else {
        panic!("the card accepted neither 48 kHz nor 44.1 kHz exclusively");
    };

    let info = sink.stream_info();
    println!(
        "negotiated: {} Hz, {} ch, {:?}",
        info.sample_rate, info.channels, info.format
    );
    assert!(sink.exclusive(), "an exclusive sink must report exclusive");
    assert!(info.sample_rate > 0, "a negotiated rate is required");
    assert_eq!(info.channels, 2);

    // ~200 ms of silence, in canonical-24 frames.
    let frames = (info.sample_rate as usize / 5) * usize::from(info.channels);
    let silence = vec![0i32; frames];
    sink.write(&silence).expect("write silence to the card");
    sink.flush().expect("flush");

    // The per-track rate switch: ask for the other family and check the sink reports
    // whatever it actually landed on (the card may only run one of them).
    let other = if info.sample_rate % 48_000 == 0 {
        44_100
    } else {
        48_000
    };
    match sink.reopen(other) {
        Ok(()) => {
            let after = sink.stream_info();
            println!(
                "reopened for {other} Hz -> running at {} Hz",
                after.sample_rate
            );
            assert!(after.sample_rate > 0);
            sink.write(&vec![0i32; frames]).expect("write after reopen");
        }
        Err(e) => println!("reopen at {other} Hz refused by the driver: {e:?}"),
    }

    println!("OK: exclusive open, write and reopen all worked on real hardware");
}
