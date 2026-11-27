//! **Contract suite** for the exclusive-output engine, run against the fake HAL loaded
//! with every recorded device profile in `tests/fixtures/coreaudio/` (MacBook speakers,
//! DisplayPort monitor, headphone jack, Intel built-in, USB DAC, HDMI AVR, Studio Display,
//! AirPods, AirPlay, aggregate devices, BlackHole) plus injected driver faults.
//!
//! No device, no clock, any OS: this is where every Apple device class and failure path
//! is exercised deterministically. A new device class = drop a JSON profile (from the
//! `coreaudio_device_report` example) into the fixtures folder; the per-fixture contract
//! below runs against it automatically.

use soul_audio_core::Sink;
use soul_audio_core::SinkError;
use soul_audio_core::model::SampleFormat as CoreFmt;
use soul_audio_desktop::exclusive_engine::{ExclusiveEngine, ExclusiveError, HOG_FREE};
use soul_audio_desktop::exclusive_policy::{
    PhysFormat, STANDARD_RATES, choose_rate, pick_physical_format, supported_rates,
};
use soul_audio_desktop::fake_hal::{DeviceProfile, FakeHal, Faults, HogSupport};

// ---- fixtures ---------------------------------------------------------------------

struct Fixture {
    id: String,
    source: String,
    profile: DeviceProfile,
}

fn load_fixtures() -> Vec<Fixture> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/coreaudio");
    let mut out = Vec::new();
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .expect("fixtures dir")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    for p in paths {
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        out.push(Fixture {
            id: v["id"].as_str().unwrap().into(),
            source: v["source"].as_str().unwrap().into(),
            profile: profile_from_json(&v),
        });
    }
    assert!(
        out.len() >= 10,
        "fixture library present ({} found)",
        out.len()
    );
    out
}

fn profile_from_json(v: &serde_json::Value) -> DeviceProfile {
    let formats = v["formats"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| PhysFormat {
            rate_min: f["rate_min"].as_u64().unwrap() as u32,
            rate_max: f["rate_max"].as_u64().unwrap() as u32,
            float: f["float"].as_bool().unwrap(),
            bits: f["bits"].as_u64().unwrap() as u32,
            channels: f["channels"].as_u64().unwrap() as u32,
        })
        .collect();
    DeviceProfile {
        name: v["name"].as_str().unwrap().into(),
        transport: v["transport"].as_str().unwrap().into(),
        formats,
        nominal_rate: v["nominal_rate"].as_u64().unwrap() as u32,
        hog: match v["hog"].as_str().unwrap() {
            "supported" => HogSupport::Supported,
            "refused" => HogSupport::Refused,
            "absent" => HogSupport::Absent,
            o => panic!("bad hog value {o}"),
        },
        fixed_rate: v["fixed_rate"].as_bool().unwrap(),
    }
}

fn fixture(id: &str) -> DeviceProfile {
    load_fixtures()
        .into_iter()
        .find(|f| f.id == id)
        .unwrap_or_else(|| panic!("no fixture {id}"))
        .profile
}

/// A world with `main` as the system default and a spare device (so the default has
/// somewhere to move when `main` is hogged — like a real Mac with speakers + a monitor).
fn world(main: DeviceProfile) -> (FakeHal, String) {
    let name = main.name.clone();
    let spare = DeviceProfile {
        name: "Spare Output".into(),
        transport: "built-in".into(),
        formats: vec![PhysFormat {
            rate_min: 48_000,
            rate_max: 48_000,
            float: true,
            bits: 32,
            channels: 2,
        }],
        nominal_rate: 48_000,
        hog: HogSupport::Supported,
        fixed_rate: false,
    };
    (FakeHal::new(vec![main, spare]), name)
}

fn open(hal: &FakeHal, name: &str, rate: u32) -> Result<ExclusiveEngine<FakeHal>, ExclusiveError> {
    ExclusiveEngine::open_with(hal.clone(), Some(name), rate, 2)
}

/// Pull everything rendered so far and convert Float32 back to canonical-24 integers.
fn rendered_canonical(hal: &FakeHal, frames: usize) -> Vec<i32> {
    hal.render(frames)
        .into_iter()
        .map(|f| (f * (1 << 23) as f32) as i32)
        .collect()
}

// ---- the per-fixture contract -----------------------------------------------------

/// For EVERY device profile: an exclusive-capable device opens at every standard rate
/// exactly as the policy dictates (native when supported, honest in-family fallback
/// otherwise), runs the hardware at that rate with its best physical format, holds hog
/// mode, and on drop hands the device back untouched. A device that can't do exclusive
/// is refused cleanly and left exactly as it was.
#[test]
fn every_fixture_honours_the_exclusive_contract() {
    for fx in load_fixtures() {
        let p = fx.profile.clone();
        let exclusive_capable =
            p.hog == HogSupport::Supported && !supported_rates(&p.formats, 2).is_empty();
        for &rate in &STANDARD_RATES {
            let (hal, name) = world(p.clone());
            let me = hal.our_pid();
            let original = hal.nominal(&name);
            let res = open(&hal, &name, rate);
            if !exclusive_capable {
                let err = res
                    .err()
                    .unwrap_or_else(|| panic!("[{}] must refuse", fx.id));
                assert_eq!(
                    err,
                    ExclusiveError::Unsupported,
                    "[{}] refusal is typed",
                    fx.id
                );
                assert_eq!(hal.nominal(&name), original, "[{}] rate untouched", fx.id);
                assert_eq!(hal.physical(&name), None, "[{}] format untouched", fx.id);
                assert_ne!(hal.hog(&name), me, "[{}] hog not held", fx.id);
                assert!(hal.streaming().is_none());
                continue;
            }
            let sink = res.unwrap_or_else(|e| panic!("[{}] open @{rate}: {e}", fx.id));
            let want = choose_rate(&supported_rates(&p.formats, 2), rate).unwrap();
            let si = sink.stream_info();
            assert_eq!(si.sample_rate, want, "[{}] {rate} → {want}", fx.id);
            assert_eq!(hal.nominal(&name), want, "[{}] hardware at {want}", fx.id);
            let best = pick_physical_format(&p.formats, want, 2).unwrap();
            assert_eq!(
                hal.physical(&name),
                Some((best, want)),
                "[{}] best physical format at {want}",
                fx.id
            );
            assert_eq!(si.format, best.report(), "[{}] honest format report", fx.id);
            assert_eq!(
                hal.streaming(),
                Some((want, 2)),
                "[{}] stream at {want}",
                fx.id
            );
            assert_eq!(hal.hog(&name), me, "[{}] hog held", fx.id);
            assert!(sink.exclusive());
            drop(sink);
            assert_eq!(hal.hog(&name), HOG_FREE, "[{}] hog released", fx.id);
            assert_eq!(hal.nominal(&name), original, "[{}] rate restored", fx.id);
            assert!(hal.streaming().is_none(), "[{}] stream stopped", fx.id);
        }
        println!("[{}] ({}) contract OK", fx.id, fx.source);
    }
}

/// Track-by-track native switching on every capable fixture: walking the device's whole
/// rate ladder up and down via `reopen` keeps hog mode, never leaves the stream down, and
/// always lands the hardware on the native rate.
#[test]
fn every_fixture_switches_natively_across_its_ladder() {
    for fx in load_fixtures() {
        let p = fx.profile;
        if p.hog != HogSupport::Supported {
            continue;
        }
        let ladder = supported_rates(&p.formats, 2);
        if ladder.is_empty() {
            continue;
        }
        let (hal, name) = world(p);
        let mut sink = open(&hal, &name, ladder[0]).unwrap();
        for &r in ladder.iter().chain(ladder.iter().rev()) {
            sink.reopen(r).unwrap();
            assert_eq!(sink.stream_info().sample_rate, r, "[{}]", fx.id);
            assert_eq!(hal.nominal(&name), r, "[{}]", fx.id);
            assert_eq!(hal.streaming(), Some((r, 2)), "[{}]", fx.id);
            assert_eq!(hal.hog(&name), hal.our_pid(), "[{}] hog kept", fx.id);
        }
    }
}

// ---- bit-perfect output -------------------------------------------------------------

#[test]
fn output_is_bit_exact_at_unity_volume() {
    let (hal, name) = world(fixture("usb-uac2-dac-384k"));
    let mut sink = open(&hal, &name, 96_000).unwrap();
    // Full-range pseudo-random 24-bit pattern, including both extremes.
    let mut x: u32 = 0xC0FF_EE11;
    let mut pattern: Vec<i32> = vec![(1 << 23) - 1, -(1 << 23), 0, 1, -1];
    while pattern.len() < 4096 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        pattern.push(((x & 0x00FF_FFFF) as i32) - (1 << 23));
    }
    sink.write(&pattern).unwrap();
    let got = rendered_canonical(&hal, pattern.len() / 2);
    assert_eq!(
        got, pattern,
        "Float32 path reproduces every 24-bit sample exactly"
    );
}

/// A **DoP** stream is the strictest thing this path can carry: the `0x05`/`0xFA`
/// marker is the top byte of every 24-bit word, so any scaling at all turns the
/// stream into full-level noise rather than quieter music.
///
/// Two claims, because a host needs both: at unity volume the words arrive
/// untouched (the Float32 round-trip and the smoothed gain are exact), and when
/// the volume is *not* unity the sink says `modifies_samples` — which is the
/// term that stops
/// [`DecodeInfo::bit_perfect`](soul_audio_core::deck::DecodeInfo::bit_perfect)
/// from claiming a bit-perfect path through a software attenuator.
#[test]
fn dop_words_survive_the_exclusive_path_only_at_unity_volume() {
    /// A 24-bit DoP word as the framer emits it: the canonical signed sample
    /// carrying that bit pattern (a `0xFA` marker sets bit 23 → negative).
    fn word(w: u32) -> i32 {
        ((w & 0x00FF_FFFF) as i32) << 8 >> 8
    }

    let (hal, name) = world(fixture("usb-uac2-dac-384k"));
    // DSD64's carrier rate — the rate a DoP stream actually runs at.
    let mut sink = open(&hal, &name, 176_400).unwrap();
    assert!(
        !sink.modifies_samples(),
        "an exclusive sink at unity volume alters nothing"
    );

    // Alternating marker phases, stereo, with payloads on both sides of the
    // sign bit of the payload bytes.
    let mut dop = Vec::new();
    let mut x: u32 = 0x5041_5243;
    while dop.len() < 4096 {
        let marker = if (dop.len() / 2) % 2 == 0 { 0x05 } else { 0xFA };
        for _ in 0..2 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            dop.push(word((marker << 16) | (x & 0xFFFF)));
        }
    }
    sink.write(&dop).unwrap();
    let got = rendered_canonical(&hal, dop.len() / 2);
    assert_eq!(
        got, dop,
        "the exclusive path altered DoP words at unity volume — a DAC gets noise"
    );
    for (i, &s) in got.iter().enumerate() {
        let marker = (s as u32 >> 16) & 0xFF;
        assert!(
            marker == 0x05 || marker == 0xFA,
            "word {i}: marker {marker:#04x} did not survive"
        );
    }

    // Software attenuation scales the words — markers included — so the sink
    // must stop claiming an untouched path.
    sink.set_volume(0.99);
    assert!(
        sink.modifies_samples(),
        "a sink attenuating in software must not be reported as bit-perfect"
    );
    sink.write(&dop).unwrap();
    assert_ne!(
        rendered_canonical(&hal, dop.len() / 2),
        dop,
        "0.99 volume should be observable in the output"
    );
}

/// A `NaN` volume must never reach the renderer. Its gain is smoothed per
/// sample (`cur += (target - cur) * k`) so a change is heard without a click,
/// and that recurrence cannot recover from a NaN: the gain stays NaN for the
/// life of the stream, so every later sample is NaN — silence that a sane
/// volume cannot undo. `clamp` does not catch it (it returns NaN for NaN), so
/// the sink refuses it outright.
#[test]
fn a_nan_volume_is_refused_and_the_output_stays_finite() {
    let (hal, name) = world(fixture("usb-uac2-dac-384k"));
    let mut sink = open(&hal, &name, 48_000).unwrap();

    sink.set_volume(0.5);
    sink.set_volume(f32::NAN);
    // The refused request must not even be recorded: a sink reporting a NaN
    // volume would also make `modifies_samples()` nonsense (NaN != 1.0), so a
    // bit-perfect claim would flip on a value that was never applied.
    assert!(
        sink.modifies_samples(),
        "the volume is still the 0.5 set before the refused NaN"
    );

    let pattern: Vec<i32> = (0..2048)
        .map(|n| if n % 64 == 0 { 1 << 22 } else { -(1 << 20) })
        .collect();
    sink.write(&pattern).unwrap();
    let got = hal.render(pattern.len() / 2);
    assert!(
        got.iter().all(|s| s.is_finite()),
        "a NaN volume poisoned the renderer's gain"
    );
    assert!(
        got.iter().any(|&s| s != 0.0),
        "output went silent, which is what a poisoned gain looks like"
    );

    // Infinities are ordinary out-of-range values and still clamp, so the
    // stream keeps playing (at full scale) rather than being refused.
    sink.set_volume(f32::INFINITY);
    sink.write(&pattern).unwrap();
    let got = hal.render(pattern.len() / 2);
    assert!(got.iter().all(|s| s.is_finite()));
}

#[test]
fn underrun_plays_silence_not_garbage() {
    let (hal, name) = world(fixture("macbook-pro-speakers"));
    let _sink = open(&hal, &name, 44_100).unwrap();
    assert!(hal.render(256).iter().all(|&s| s == 0.0));
}

// ---- hog-mode ownership -------------------------------------------------------------

#[test]
fn a_foreign_hog_is_reported_in_use_and_never_stolen() {
    let (hal, name) = world(fixture("usb-uac2-dac-384k"));
    hal.hog_by(&name, 999);
    hal.clear_log();
    assert_eq!(
        open(&hal, &name, 44_100).err(),
        Some(ExclusiveError::DeviceInUse)
    );
    assert_eq!(hal.hog(&name), 999, "the other app keeps the device");
    assert!(
        hal.log()
            .iter()
            .all(|l| !l.starts_with("hog ") && !l.starts_with("rate ")),
        "never toggled hog or touched the rate: {:?}",
        hal.log()
    );
    // Once the other app quits, we can take it.
    hal.release_foreign_hog(&name);
    assert!(open(&hal, &name, 44_100).is_ok());
}

#[test]
fn refused_and_absent_hog_fall_back_cleanly() {
    for id in ["airpods", "airplay", "aggregate-multi-output"] {
        let (hal, name) = world(fixture(id));
        let before = hal.nominal(&name);
        assert_eq!(
            open(&hal, &name, 48_000).err(),
            Some(ExclusiveError::Unsupported),
            "[{id}] typed refusal → host falls back to shared"
        );
        assert_eq!(hal.nominal(&name), before, "[{id}] untouched");
    }
}

#[test]
fn hogging_the_default_moves_the_system_default_and_the_sink_stays_pinned() {
    let (hal, name) = world(fixture("macbook-pro-speakers"));
    assert_eq!(hal.default_name().as_deref(), Some(name.as_str()));
    // Open "the default" (None) — exactly what the player does with no device chosen.
    let mut sink = ExclusiveEngine::open_with(hal.clone(), None, 44_100, 2).unwrap();
    assert_eq!(
        hal.default_name().as_deref(),
        Some("Spare Output"),
        "macOS moved the default off the hogged device"
    );
    // Per-track switching must keep targeting the ORIGINAL device, not the new default.
    sink.reopen(96_000).unwrap();
    assert_eq!(hal.nominal(&name), 96_000);
    assert_eq!(hal.nominal("Spare Output"), 48_000, "spare untouched");
    drop(sink);
    assert_eq!(
        hal.default_name().as_deref(),
        Some(name.as_str()),
        "default returns on release"
    );
}

// ---- driver faults ----------------------------------------------------------------

#[test]
fn a_refused_rate_switch_keeps_playing_at_the_previous_rate() {
    let (hal, name) = world(fixture("usb-uac2-dac-384k"));
    let mut sink = open(&hal, &name, 44_100).unwrap();
    hal.set_faults(Faults {
        refuse_rates: vec![192_000],
        ..Faults::default()
    });
    sink.reopen(192_000).expect("never left closed");
    assert_eq!(
        sink.stream_info().sample_rate,
        44_100,
        "kept the previous rate"
    );
    assert_eq!(hal.streaming(), Some((44_100, 2)));
    sink.write(&[0; 256]).expect("still writable");
}

#[test]
fn a_switch_that_fails_both_ways_reports_and_closes_honestly() {
    let (hal, name) = world(fixture("usb-uac2-dac-384k"));
    let mut sink = open(&hal, &name, 44_100).unwrap();
    hal.set_faults(Faults {
        refuse_rates: vec![192_000, 44_100],
        ..Faults::default()
    });
    assert_eq!(sink.reopen(192_000), Err(SinkError::Unsupported));
    assert_eq!(
        sink.write(&[0; 2]),
        Err(SinkError::Closed),
        "no silent zombie"
    );
    drop(sink);
    assert_eq!(hal.hog(&name), HOG_FREE, "hog still released");
}

#[test]
fn a_refused_physical_format_falls_down_the_ladder() {
    let (hal, name) = world(fixture("displayport-monitor"));
    hal.set_faults(Faults {
        refuse_formats: vec![(false, 24)],
        ..Faults::default()
    });
    let sink = open(&hal, &name, 48_000).unwrap();
    assert_eq!(
        sink.stream_info().format,
        CoreFmt::Int { bits: 20 },
        "24-bit refused → 20-bit, reported honestly"
    );
    assert_eq!(hal.physical(&name).unwrap().0.bits, 20);
}

#[test]
fn every_format_refused_fails_open_and_restores_the_device() {
    let (hal, name) = world(fixture("displayport-monitor"));
    hal.set_faults(Faults {
        refuse_formats: vec![(false, 24), (false, 20), (false, 16)],
        ..Faults::default()
    });
    let before = hal.nominal(&name);
    assert!(matches!(
        open(&hal, &name, 44_100),
        Err(ExclusiveError::Other(_))
    ));
    assert_eq!(
        hal.nominal(&name),
        before,
        "rate restored after failed open"
    );
    assert_eq!(hal.hog(&name), HOG_FREE, "hog released after failed open");
}

#[test]
fn a_stream_start_failure_releases_everything() {
    let (hal, name) = world(fixture("macbook-pro-speakers"));
    hal.set_faults(Faults {
        fail_start: true,
        ..Faults::default()
    });
    assert!(open(&hal, &name, 44_100).is_err());
    assert_eq!(hal.hog(&name), HOG_FREE);
    assert_eq!(hal.nominal(&name), 48_000);
}

#[test]
fn unplugging_mid_play_fails_fast_instead_of_hanging() {
    let (hal, name) = world(fixture("usb-uac2-dac-384k"));
    let mut sink = open(&hal, &name, 44_100).unwrap();
    hal.unplug(&name);
    // More than the ring holds, and nothing draining it: must return, not block forever.
    let t = std::time::Instant::now();
    assert_eq!(sink.write(&vec![0; 44_100 * 2]), Err(SinkError::Closed));
    assert_eq!(sink.flush(), Err(SinkError::Closed));
    assert_eq!(sink.reopen(96_000), Err(SinkError::Closed));
    assert!(t.elapsed() < std::time::Duration::from_millis(500));
    hal.clear_log();
    drop(sink);
    assert!(
        hal.log()
            .iter()
            .all(|l| !l.starts_with("rate ") && !l.starts_with("format ")),
        "no restore attempts against a dead device: {:?}",
        hal.log()
    );
}

#[test]
fn an_external_rate_change_is_reverted_before_more_audio_plays() {
    // e.g. a driver-initiated switch while we hold hog mode: we must not keep rendering
    // at a rate the hardware is no longer running at (wrong speed / hidden SRC).
    let (hal, name) = world(fixture("usb-uac2-dac-384k"));
    let mut sink = open(&hal, &name, 96_000).unwrap();
    hal.external_rate_change(&name, 48_000);
    sink.write(&[0; 512]).unwrap();
    assert_eq!(hal.nominal(&name), 96_000, "our native rate re-asserted");
    assert_eq!(hal.streaming(), Some((96_000, 2)), "stream restarted at it");
    assert_eq!(sink.stream_info().sample_rate, 96_000);
}

// ---- sequencing ----------------------------------------------------------------------

#[test]
fn a_rate_switch_stops_the_stream_before_touching_the_hardware() {
    let (hal, name) = world(fixture("usb-uac2-dac-384k"));
    let mut sink = open(&hal, &name, 44_100).unwrap();
    hal.clear_log();
    sink.reopen(192_000).unwrap();
    let log = hal.log();
    let pos = |p: &str| {
        log.iter()
            .position(|l| l.starts_with(p))
            .unwrap_or_else(|| panic!("{p} missing from {log:?}"))
    };
    assert!(pos("stop 44100") < pos("rate "), "{log:?}");
    assert!(pos("rate ") < pos("format "), "{log:?}");
    assert!(pos("format ") < pos("start 192000"), "{log:?}");
}

#[test]
fn drop_restores_then_releases_hog_last() {
    let (hal, name) = world(fixture("usb-uac2-dac-384k"));
    let sink = open(&hal, &name, 192_000).unwrap();
    hal.clear_log();
    drop(sink);
    let log = hal.log();
    assert!(log[0].starts_with("stop"), "{log:?}");
    assert_eq!(
        log.last().unwrap(),
        &format!("hog {name} -> -1"),
        "hog released last: {log:?}"
    );
    assert!(
        log.iter().any(|l| l == &format!("rate {name} 44100")),
        "{log:?}"
    );
}

#[test]
fn reopening_at_the_current_rate_does_nothing() {
    let (hal, name) = world(fixture("macbook-pro-speakers"));
    let mut sink = open(&hal, &name, 48_000).unwrap();
    hal.clear_log();
    sink.reopen(48_000).unwrap();
    // 32k maps to 48k on this device (in-family) — also a no-op.
    sink.reopen(32_000).unwrap();
    assert!(hal.log().is_empty(), "{:?}", hal.log());
}

// ---- device-class specifics -------------------------------------------------------

#[test]
fn hdmi_stereo_uses_the_exact_2ch_format_and_8ch_uses_8ch() {
    let (hal, name) = world(fixture("hdmi-avr-apple-silicon"));
    let s = open(&hal, &name, 44_100).unwrap();
    assert_eq!(
        s.stream_info().sample_rate,
        48_000,
        "HDMI on M-series is 48k-only"
    );
    assert_eq!(hal.physical(&name).unwrap().0.channels, 2);
    drop(s);
    let s8 = ExclusiveEngine::open_with(hal.clone(), Some(&name), 48_000, 8).unwrap();
    assert_eq!(hal.physical(&name).unwrap().0.channels, 8);
    assert_eq!(s8.stream_info().channels, 8);
}

#[test]
fn usb_dac_plays_dxd_natively_and_maps_beyond_its_ceiling() {
    let (hal, name) = world(fixture("usb-uac2-dac-384k"));
    let mut s = open(&hal, &name, 352_800).unwrap();
    assert_eq!(s.stream_info().sample_rate, 352_800, "DXD native");
    assert_eq!(
        s.stream_info().format,
        CoreFmt::Int { bits: 32 },
        "Int32 word"
    );
    s.reopen(705_600).unwrap();
    assert_eq!(
        s.stream_info().sample_rate,
        352_800,
        "705.6k → 352.8k (same family)"
    );
}

#[test]
fn unknown_device_is_no_device() {
    let (hal, _) = world(fixture("macbook-pro-speakers"));
    assert_eq!(
        open(&hal, "Nope", 44_100).err(),
        Some(ExclusiveError::NoDevice)
    );
}
