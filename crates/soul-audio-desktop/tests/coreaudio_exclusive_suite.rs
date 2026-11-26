//! CoreAudio **exclusive** (hog mode / bit-perfect) hardware test suite — macOS,
//! `coreaudio-exclusive` feature, `#[ignore]`d because it takes over a real output device.
//!
//! ```text
//! cargo test -p soul-audio-desktop --features coreaudio-exclusive \
//!     --test coreaudio_exclusive_suite -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Device under test: `SOUL_CA_DEVICE=<name>` (e.g. `"BlackHole 2ch"` in CI), else the
//! system default output (resolved by name once, up front — see [`device`]). Writes SILENCE only to real devices; the loopback test writes a
//! signal but only to a virtual loopback device (BlackHole / SoulProfile, inaudible).
//!
//! Hog mode is per-device and per-process, so every test takes `SERIAL` (and the CLI
//! should still pass `--test-threads=1`). One test spawns a second copy of this binary to
//! hold the device from ANOTHER process — that is the only way to exercise the
//! "device in use" path and CoreAudio's release-on-process-death.

#![cfg(all(target_os = "macos", feature = "coreaudio-exclusive"))]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use soul_audio_core::Sink;
use soul_audio_core::model::SampleFormat as CoreFmt;
use soul_audio_desktop::exclusive_policy::{choose_rate, pick_physical_format, supported_rates};
use soul_audio_desktop::{
    CoreAudioExclusiveSink, ExclusiveError, hog_owner, nominal_rate, physical_format_ladder,
};

static SERIAL: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

/// The device under test, resolved to a NAME once, before anything is hogged: macOS moves
/// the system-default output OFF a hogged device (to the next available one) and back on
/// release, so re-resolving "default" mid-test would silently query a different device.
fn device() -> Option<String> {
    static DEV: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    DEV.get_or_init(|| {
        std::env::var("SOUL_CA_DEVICE")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(soul_audio_desktop::default_output_device_name)
    })
    .clone()
}

fn me() -> i32 {
    std::process::id() as i32
}

/// Open the device under test exclusively at `rate`, or skip the test (with a reason) if
/// the machine has no usable output device.
fn open(rate: u32) -> Option<CoreAudioExclusiveSink> {
    match CoreAudioExclusiveSink::open(device(), rate, 2) {
        Ok(s) => Some(s),
        Err(ExclusiveError::NoDevice) => {
            eprintln!("SKIP: no output device {:?}", device());
            None
        }
        Err(e) => panic!("exclusive open of {:?} failed: {e}", device()),
    }
}

fn rates() -> Vec<u32> {
    supported_rates(&physical_format_ladder(device().as_deref()), 2)
}

/// Write `secs` of silence at the sink's rate and return the wall time it took.
fn play_silence(sink: &mut CoreAudioExclusiveSink, secs: f64) -> Duration {
    let si = sink.stream_info();
    let frames = (f64::from(si.sample_rate) * secs) as usize;
    let block = vec![0i32; 1024 * si.channels as usize];
    let t = Instant::now();
    let mut done = 0;
    while done < frames {
        sink.write(&block).expect("write silence");
        done += 1024;
    }
    t.elapsed()
}

#[test]
#[ignore = "takes over a real output device; run with --ignored"]
fn open_takes_hog_mode_and_drop_releases_it() {
    let _g = lock();
    let before = hog_owner(device().as_deref());
    assert_eq!(
        before,
        Some(-1),
        "device must start un-hogged (got {before:?})"
    );
    let Some(sink) = open(48_000) else { return };
    assert!(sink.exclusive());
    assert_eq!(
        hog_owner(device().as_deref()),
        Some(me()),
        "we hold hog mode while open"
    );
    drop(sink);
    assert_eq!(
        hog_owner(device().as_deref()),
        Some(-1),
        "hog mode released on drop"
    );
}

#[test]
#[ignore = "takes over a real output device; run with --ignored"]
fn drop_restores_the_original_nominal_rate() {
    let _g = lock();
    let original = nominal_rate(device().as_deref()).expect("device has a nominal rate");
    let Some(other) = rates().into_iter().find(|&r| r != original) else {
        // A fixed-rate device (e.g. a 48k-only HDMI sink): any request maps onto the one
        // rate, so there is nothing to switch — and nothing to restore.
        let Some(sink) = open(44_100) else { return };
        assert_eq!(sink.stream_info().sample_rate, original);
        drop(sink);
        assert_eq!(nominal_rate(device().as_deref()), Some(original));
        return;
    };
    let Some(sink) = open(other) else { return };
    assert_eq!(sink.stream_info().sample_rate, other);
    assert_eq!(
        nominal_rate(device().as_deref()),
        Some(other),
        "device switched to {other}"
    );
    drop(sink);
    assert_eq!(
        nominal_rate(device().as_deref()),
        Some(original),
        "original rate {original} restored on drop"
    );
}

#[test]
#[ignore = "takes over a real output device; run with --ignored"]
fn native_rate_switching_across_the_whole_ladder() {
    let _g = lock();
    let ladder = rates();
    println!("ladder {ladder:?}");
    let Some(mut sink) = open(ladder[0]) else {
        return;
    };
    let phys = physical_format_ladder(device().as_deref());
    // Walk up then back down so every transition direction is exercised.
    let walk: Vec<u32> = ladder.iter().chain(ladder.iter().rev()).copied().collect();
    for rate in walk {
        sink.reopen(rate).expect("reopen at a supported rate");
        let si = sink.stream_info();
        assert_eq!(si.sample_rate, rate, "sink reports the native rate");
        assert_eq!(
            nominal_rate(device().as_deref()),
            Some(rate),
            "hardware runs at {rate}"
        );
        let (client, dev) = sink.auhal_rates().expect("unit running");
        assert_eq!(
            (client, dev),
            (rate, rate),
            "AUHAL client rate == device rate → no hidden SRC"
        );
        let want = pick_physical_format(&phys, rate, 2).unwrap().report();
        assert_eq!(si.format, want, "widest physical format at {rate}");
        assert_eq!(
            hog_owner(device().as_deref()),
            Some(me()),
            "hog held across reopen"
        );
        // The render callback must actually be consuming at this rate.
        let t = play_silence(&mut sink, 0.3);
        assert!(
            t < Duration::from_secs(2),
            "stream stalled at {rate}: {t:?}"
        );
        println!("  {rate} Hz → {:?} OK", si.format);
    }
}

#[test]
#[ignore = "takes over a real output device; run with --ignored"]
fn the_device_clock_paces_writes_in_real_time() {
    let _g = lock();
    for rate in [44_100, 48_000] {
        if !rates().contains(&rate) {
            continue;
        }
        let Some(mut sink) = open(rate) else { return };
        // Fill the ~200 ms ring first so the measured span is device-paced.
        play_silence(&mut sink, 0.3);
        let t = play_silence(&mut sink, 2.0).as_secs_f64();
        println!("{rate} Hz: 2.0 s of audio consumed in {t:.3} s");
        assert!(
            (1.8..2.25).contains(&t),
            "device clock wrong at {rate}: 2 s took {t:.3} s (wrong rate ⇒ wrong speed)"
        );
    }
}

#[test]
#[ignore = "takes over a real output device; run with --ignored"]
fn unsupported_rates_map_to_the_closest_same_family_rate() {
    let _g = lock();
    let ladder = rates();
    let Some(mut sink) = open(ladder[0]) else {
        return;
    };
    for asked in [22_050, 32_000, 176_400, 192_000, 352_800, 384_000] {
        let want = choose_rate(&ladder, asked).unwrap();
        sink.reopen(asked)
            .expect("reopen never fails on a mapped rate");
        assert_eq!(
            sink.stream_info().sample_rate,
            want,
            "{asked} → {want} per policy"
        );
        assert_eq!(nominal_rate(device().as_deref()), Some(want));
    }
}

#[test]
#[ignore = "takes over a real output device; run with --ignored"]
fn seek_discard_volume_and_flush_work_while_hogged() {
    let _g = lock();
    let Some(mut sink) = open(48_000) else { return };
    play_silence(&mut sink, 0.3); // ring now full
    let t = Instant::now();
    sink.discard().unwrap();
    assert!(
        t.elapsed() < Duration::from_millis(150),
        "discard ack came back from a live callback ({:?})",
        t.elapsed()
    );
    sink.set_volume(0.25);
    sink.set_volume(1.0);
    play_silence(&mut sink, 0.2);
    let t = Instant::now();
    sink.flush().unwrap();
    assert!(
        t.elapsed() < Duration::from_millis(600),
        "flush drained the ring"
    );
}

#[test]
#[ignore = "takes over a real output device; run with --ignored"]
fn reopen_then_reopen_back_and_open_again_in_the_same_process() {
    let _g = lock();
    // The host builds a new sink right after dropping the old one (live output change):
    // hog mode must be cleanly re-takeable, repeatedly.
    for i in 0..5 {
        let Some(mut s) = open(if i % 2 == 0 { 44_100 } else { 48_000 }) else {
            return;
        };
        play_silence(&mut s, 0.05);
        drop(s);
        assert_eq!(
            hog_owner(device().as_deref()),
            Some(-1),
            "released (round {i})"
        );
    }
}

/// Helper, not a test of its own: when `SOUL_CA_HOLD=1` it opens the device exclusively,
/// prints `HOLDING`, and sleeps — so the parent test can observe a FOREIGN hog owner.
#[test]
#[ignore = "helper process for another_process_holding_the_device_is_reported_in_use"]
fn zz_helper_hold_device() {
    if std::env::var_os("SOUL_CA_HOLD").is_none() {
        return;
    }
    let _s = CoreAudioExclusiveSink::open(device(), 48_000, 2).expect("helper open");
    println!("HOLDING");
    std::thread::sleep(Duration::from_secs(30));
}

#[test]
#[ignore = "takes over a real output device; run with --ignored"]
fn another_process_holding_the_device_is_reported_in_use() {
    let _g = lock();
    if hog_owner(device().as_deref()).is_none() {
        eprintln!("SKIP: no output device");
        return;
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "zz_helper_hold_device",
            "--nocapture",
        ])
        .env("SOUL_CA_HOLD", "1")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn helper");
    let pid = child.id() as i32;
    // Keep the reader alive until the child is killed, so its later output never hits a
    // closed pipe.
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let held = lines.any(|l| l.unwrap().contains("HOLDING"));
    assert!(held, "helper process took the device");
    assert_eq!(
        hog_owner(device().as_deref()),
        Some(pid),
        "helper owns hog mode"
    );

    let err = CoreAudioExclusiveSink::open(device(), 48_000, 2).err();
    assert_eq!(
        err,
        Some(ExclusiveError::DeviceInUse),
        "a foreign hog owner is reported as in-use, never stolen"
    );
    assert_eq!(
        hog_owner(device().as_deref()),
        Some(pid),
        "our failed attempt left the helper's hog untouched"
    );

    // Kill -9 the holder: CoreAudio must free the device on process death (the crash
    // safety net — a crashed player never bricks the DAC).
    child.kill().unwrap();
    child.wait().unwrap();
    drop(lines);
    let t = Instant::now();
    while hog_owner(device().as_deref()) != Some(-1) && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        hog_owner(device().as_deref()),
        Some(-1),
        "CoreAudio released hog mode when the holder died"
    );
    let s = open(48_000).expect("device usable again");
    drop(s);
}

#[test]
#[ignore = "takes over a real output device; run with --ignored"]
fn unknown_device_name_is_no_device() {
    let _g = lock();
    assert_eq!(
        CoreAudioExclusiveSink::open(Some("no such DAC 9000".into()), 48_000, 2).err(),
        Some(ExclusiveError::NoDevice)
    );
}

/// Bit-perfect proof by loopback: play a known 24-bit pattern through the exclusive sink
/// into a loopback device and capture its input side with cpal — every captured sample
/// must equal the canonical value we wrote (Float32 carries 24-bit exactly). Only runs on
/// a virtual loopback device ([`is_loopback_device`], inaudible): BlackHole covers a
/// Float32 physical format, the CI `SoulProfile` plug-in covers Int24 / Int32 ones (so
/// the HAL's float → integer conversion is part of what is proven). Capture needs the
/// microphone (TCC) permission for the running terminal/CI agent.
///
/// Each rate gets up to [`LOOPBACK_ATTEMPTS`] tries and passes only on a capture that is
/// **entirely** bit-identical. That keeps the proof sound — a path that alters samples
/// (dither, gain, truncation) can never reproduce a pseudo-random 24-bit pattern exactly
/// by chance — while tolerating the I/O-cycle dropouts a loaded CI VM causes (BlackHole
/// is sample-time indexed, so a late cycle leaves a whole-buffer hole, not a value
/// change). Failed attempts print the mismatch shape so a real defect stays visible.
#[test]
#[ignore = "needs a loopback device; run with --ignored"]
fn bit_perfect_loopback() {
    let _g = lock();
    let Some(name) = device().filter(|d| is_loopback_device(d)) else {
        eprintln!(
            "SKIP: set SOUL_CA_DEVICE to a loopback device (BlackHole*, SoulProfile*) for the loopback test"
        );
        return;
    };
    // Both families where the device has them; a fixed-rate device gets its one rate.
    let ladder = rates();
    let mut loop_rates: Vec<u32> = [44_100u32, 96_000]
        .into_iter()
        .filter(|r| ladder.contains(r))
        .collect();
    if loop_rates.is_empty() {
        loop_rates.push(ladder[0]);
    }
    for rate in loop_rates {
        let mut failures = Vec::new();
        let n = (1..=LOOPBACK_ATTEMPTS).find_map(|attempt| match loopback_once(&name, rate) {
            Ok(n) => Some(n),
            Err(why) => {
                eprintln!("{rate} Hz attempt {attempt}/{LOOPBACK_ATTEMPTS}: {why}");
                failures.push(why);
                None
            }
        });
        let Some(n) = n else {
            panic!(
                "{rate} Hz: no bit-identical capture in {LOOPBACK_ATTEMPTS} attempts — output is NOT bit-perfect: {failures:#?}"
            );
        };
        println!(
            "{rate} Hz: {n} samples bit-identical through hog-mode output (after {} failed attempt(s))",
            failures.len()
        );
    }
}

/// Virtual devices whose input side plays back what was written to the output: BlackHole
/// (Float32 physical) and the CI-only `SoulProfile` HAL plug-in
/// (`tests/fixtures/hal-profile-device`, integer physical formats).
fn is_loopback_device(name: &str) -> bool {
    name.starts_with("BlackHole") || name.starts_with("SoulProfile")
}

/// Tries per rate for [`bit_perfect_loopback`].
const LOOPBACK_ATTEMPTS: usize = 3;

/// One loopback round at `rate`: `Ok(samples compared)` when the whole captured pattern
/// is bit-identical, else a description of how it differs.
fn loopback_once(name: &str, rate: u32) -> Result<usize, String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let mut sink = CoreAudioExclusiveSink::open(Some(name.to_owned()), rate, 2).unwrap();
    assert_eq!(sink.stream_info().sample_rate, rate);

    let input = cpal::default_host()
        .input_devices()
        .unwrap()
        .find(|d| d.name().is_ok_and(|n| n == name))
        .expect("loopback input side");
    let cfg = cpal::StreamConfig {
        channels: 2,
        sample_rate: cpal::SampleRate(rate),
        buffer_size: cpal::BufferSize::Default,
    };
    let captured = std::sync::Arc::new(Mutex::new(Vec::<f32>::new()));
    let cap = std::sync::Arc::clone(&captured);
    let stream = input
        .build_input_stream(
            &cfg,
            move |d: &[f32], _| cap.lock().unwrap().extend_from_slice(d),
            |e| eprintln!("capture error {e}"),
            None,
        )
        .expect("capture stream");
    stream.play().unwrap();

    // A pseudo-random full-range 24-bit pattern, framed by a marker so it can be
    // found in the capture regardless of latency.
    let mut x: u32 = 0x1234_5678;
    let mut pattern = Vec::with_capacity(rate as usize);
    for _ in 0..rate as usize / 2 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        let s = ((x & 0x00FF_FFFF) as i32) - (1 << 23); // [-2^23, 2^23)
        pattern.push(s);
    }
    let marker = [(1 << 23) - 1, -(1 << 23), (1 << 22) + 7, -(1 << 21) - 3];
    sink.write(&vec![0; rate as usize / 5 * 2]).unwrap();
    sink.write(&marker).unwrap();
    sink.write(&pattern).unwrap();
    sink.write(&vec![0; rate as usize / 2 * 2]).unwrap();
    sink.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    drop(stream);
    drop(sink);

    let got: Vec<i32> = captured
        .lock()
        .unwrap()
        .iter()
        .map(|&f| (f * (1 << 23) as f32) as i32)
        .collect();
    assert!(
        got.iter().any(|&s| s != 0),
        "captured only silence — grant microphone access to the terminal / CI agent"
    );
    let Some(start) = got
        .windows(marker.len())
        .position(|w| w == marker)
        .map(|p| p + marker.len())
    else {
        return Err("marker not found bit-exact in the capture (dropout over the marker?)".into());
    };
    let n = pattern.len().min(got.len() - start);
    if n <= pattern.len() / 2 {
        return Err(format!(
            "captured only {n}/{} pattern samples",
            pattern.len()
        ));
    }
    let bad: Vec<usize> = (0..n).filter(|&i| got[start + i] != pattern[i]).collect();
    if bad.is_empty() {
        return Ok(n);
    }
    // Shape of the damage: contiguous runs (a dropout) vs scattered samples (a
    // conversion defect), and whether the wrong samples are silence.
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for &i in &bad {
        match runs.last_mut() {
            Some((s, len)) if *s + *len == i => *len += 1,
            _ => runs.push((i, 1)),
        }
    }
    let zeros = bad.iter().filter(|&&i| got[start + i] == 0).count();
    let first = bad[0];
    Err(format!(
        "{}/{n} samples differ in {} run(s) {:?}; {zeros} of them are 0; first at {first}: got {} want {}",
        bad.len(),
        runs.len(),
        &runs[..runs.len().min(8)],
        got[start + first],
        pattern[first],
    ))
}

#[test]
#[ignore = "takes over a real output device; run with --ignored"]
fn reports_an_honest_output_format() {
    let _g = lock();
    let Some(sink) = open(44_100) else { return };
    let si = sink.stream_info();
    match si.format {
        CoreFmt::Float { bits: 32 } | CoreFmt::Int { bits: 16..=32 } => {}
        other => panic!("unexpected reported format {other:?}"),
    }
    println!("{:?} @ {} Hz → {:?}", device(), si.sample_rate, si.format);
}
