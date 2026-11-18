//! WASAPI **exclusive clock probe** — catches "plays slow / distorted" bit-perfect
//! bugs WITHOUT a DAC or loopback, by using the device's own hardware clock as the
//! reference.
//!
//! The contract the deck relies on: the rate a [`WasapiExclusiveSink`] reports via
//! [`Sink::stream_info`] MUST equal the rate the hardware actually clocks at — because
//! the deck resamples (or passes through) to the reported rate, then the device plays
//! those frames at its real clock. If the two disagree, playback is pitched/sped wrong
//! (the reported-vs-real ratio) and sounds "slow and distorted". The silence-only
//! `wasapi_exclusive_suite` can't see this — silence has no timeline.
//!
//! Measurement: saturate the sink's ring, then time how long the device takes to drain
//! a known number of frames at steady state (ring full → full, so exactly `N` frames
//! were consumed). `effective_rate = N / elapsed` is the TRUE hardware clock. We assert
//! it matches the reported rate at the device default AND after each native-rate reopen
//! (48000 → 44100 → 96000) — the exact sequence the app's native-rate switching runs.
//!
//! Plays a soft 440 Hz tone (not silence) so a human can also hear whether it's clean.
//!
//! ```text
//! cargo test -p soul-audio-desktop --features wasapi-exclusive \
//!     --test wasapi_clock_probe -- --ignored --nocapture
//! ```

#![cfg(all(windows, feature = "wasapi-exclusive"))]

use std::time::Instant;

use soul_audio_core::Sink;
use soul_audio_desktop::{
    WasapiExclusiveSink, default_output_config, default_output_device_name, output_device_names,
};

/// Soft tone amplitude (~0.07 of 24-bit full scale) — audible but gentle.
const AMP: f32 = 600_000.0;
const TONE_HZ: f32 = 440.0;

/// Append `secs` of a 440 Hz tone (interleaved, `channels`) at `rate` to `buf`,
/// continuing the phase from frame offset `start_frame`.
fn tone(rate: u32, channels: u16, secs: f32, start_frame: u64, buf: &mut Vec<i32>) {
    let n = (rate as f32 * secs) as u64;
    for f in 0..n {
        let t = (start_frame + f) as f32 / rate as f32;
        let s = (AMP * (2.0 * std::f32::consts::PI * TONE_HZ * t).sin()) as i32;
        for _ in 0..channels {
            buf.push(s);
        }
    }
}

/// Drive the sink at steady state and return the TRUE device clock (frames/sec) it
/// consumes at. Saturates the ring first (warmup), then times a measured burst during
/// which the ring stays full — so exactly the measured frames are consumed in the
/// elapsed wall-clock.
fn effective_clock(sink: &mut WasapiExclusiveSink, rate: u32, channels: u16) -> f64 {
    // Warm up: ~1 s reaches steady state (ring saturated, writes pacing to the device).
    let mut warm = Vec::new();
    tone(rate, channels, 1.0, 0, &mut warm);
    sink.write(&warm).expect("warmup write");

    // Measured burst: ring is full now; writing these frames blocks, paced exactly by
    // device consumption, and leaves the ring full again — so the device consumed
    // precisely `measure_frames` during `elapsed`.
    let measure_secs = 3.0f32;
    let mut burst = Vec::new();
    tone(rate, channels, measure_secs, rate as u64, &mut burst);
    let measure_frames = (burst.len() / channels as usize) as f64;

    let t0 = Instant::now();
    sink.write(&burst).expect("measured write");
    let elapsed = t0.elapsed().as_secs_f64();

    measure_frames / elapsed
}

/// Assert the device's real clock matches the rate the sink reports to the deck. A
/// mismatch beyond ~3 % is the slow/distorted bit-perfect bug (3 % absorbs scheduling
/// jitter but is far tighter than the 44100-vs-48000 = 8.8 % or 2× failures we hunt).
fn check(sink: &mut WasapiExclusiveSink, stage: &str) {
    let reported = sink.stream_info().sample_rate;
    let channels = sink.stream_info().channels;
    let effective = effective_clock(sink, reported, channels);
    let ratio = effective / reported as f64;
    println!(
        "  [{stage}] reported {reported} Hz, {channels}ch → measured device clock \
         {effective:.0} Hz (ratio {ratio:.4})"
    );
    assert!(
        (ratio - 1.0).abs() < 0.03,
        "{stage}: the device clocks at {effective:.0} Hz but the sink reports {reported} Hz \
         to the deck (ratio {ratio:.3}) — playback is pitched/sped by that factor = the \
         'slow and distorted' bug"
    );
}

#[test]
#[ignore = "touches real output devices; run with --ignored"]
fn wasapi_effective_clock_matches_reported_rate_across_reopens() {
    // Measure the clock on EVERY exclusive-capable device, not just the default — the
    // default may be a Bluetooth / shared-only device (no exclusive mode), and we
    // specifically want to validate the bit-perfect path on a real exclusive DAC/speaker.
    let mut names: Vec<Option<String>> = output_device_names().into_iter().map(Some).collect();
    if names.is_empty() {
        names.push(default_output_device_name());
    }
    let mut tested = 0usize;
    for name in names {
        let Some((def_rate, channels)) = default_output_config(name.as_deref()) else {
            continue;
        };
        let mut sink = match WasapiExclusiveSink::open(name.clone(), def_rate, channels) {
            Ok(s) => s,
            Err(_) => continue, // not exclusive-capable (e.g. Bluetooth) — skip
        };
        println!("\n=== clock probe on {name:?} ({def_rate} Hz, {channels}ch) ===");
        tested += 1;

        // 1) Initial open at the device default rate.
        check(&mut sink, &format!("open@{def_rate}"));

        // 2) Native-rate reopen to each standard rate the app switches to per track.
        for &rate in &[44_100u32, 96_000, def_rate] {
            if rate == sink.stream_info().sample_rate {
                continue;
            }
            match sink.reopen(rate) {
                Ok(()) => {
                    let landed = sink.stream_info().sample_rate;
                    if landed != rate {
                        println!(
                            "  reopen to {rate} landed on {landed} (device refused) — skipped"
                        );
                        continue;
                    }
                    check(&mut sink, &format!("reopen@{rate}"));
                }
                Err(e) => println!("  reopen to {rate} failed ({e:?}) — skipped"),
            }
        }
    }
    if tested == 0 {
        println!("no exclusive-capable output device present — nothing to validate");
    } else {
        println!("\n=== clock probe done: {tested} device(s) clock matches reported rate ===");
    }
}
