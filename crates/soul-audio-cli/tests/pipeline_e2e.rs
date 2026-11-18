//! FULL-PIPELINE signal end-to-end tests — the exact audio path the desktop app runs
//! on Play, exercised headlessly (no device) and validated on the SIGNAL.
//!
//! A committed tone (A4 = 440 Hz on LEFT, E5 = 659.25 Hz on RIGHT) is decoded by the
//! real `soul-audio-desktop` decoder and driven through the real [`Deck`] (ReplayGain +
//! resample), optionally through a parametric EQ ([`soul_audio_eq`] designed into
//! [`soul_audio_core::Equalizer`], mirroring the host's `DspSink`), into a capturing
//! sink. The captured samples are then checked with Goertzel tone/speed analysis.
//!
//! Coverage (the matrix that kept biting us on real hardware):
//!   - **shared output** — the deck resamples the track rate → the device rate;
//!   - **exclusive output** — the device is reopened to the track's NATIVE rate
//!     ([`Deck::reopen_sink_at`]) so playback is bit-perfect (no resampling);
//!   - **EQ on** — a band's boost shows up in the captured spectrum;
//!   - **EQ off / bypassed** — the output is bit-identical to no EQ at all;
//!   - **live output switch** — swapping the sink mid-track ([`Deck::swap_sink`], the
//!     core mechanism behind the app's live device/exclusive change) keeps the signal
//!     correct at the new rate.
//!
//! `OwnedDecoder` is large by value, so the pipeline runs on a big-stack thread.

use soul_audio_core::{
    Deck, Equalizer, Progress, Quality, SampleFormat, Sink, SinkError, StreamInfo,
};
use soul_audio_desktop::{OwnedDecoder, open_track};
use soul_audio_eq::{Band, BandKind, MAX_BANDS, design};

const LEFT_HZ: f64 = 440.0;
const RIGHT_HZ: f64 = 659.25;
/// Canonical 24-bit full scale (the deck normalizes every source to this).
const FS24: i64 = 8_388_607;
/// The committed tone's native sample rate.
const TONE_RATE: u32 = 44_100;

fn testdata(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(name)
}

// ---------------------------------------------------------------------------
// Sinks
// ---------------------------------------------------------------------------

/// Records every interleaved sample the deck writes. Reports a configurable device
/// rate and exclusive flag, and models an exclusive device's native-rate `reopen`
/// (a shared one keeps its fixed rate and lets the deck resample) — mirroring the real
/// `CpalSink` / `WasapiExclusiveSink`.
struct CaptureSink {
    rate: u32,
    exclusive: bool,
    written: Vec<i32>,
}

impl CaptureSink {
    fn shared(rate: u32) -> Self {
        Self {
            rate,
            exclusive: false,
            written: Vec::new(),
        }
    }
    fn exclusive(rate: u32) -> Self {
        Self {
            rate,
            exclusive: true,
            written: Vec::new(),
        }
    }
}

impl Sink for CaptureSink {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: self.rate,
            channels: 2,
            format: SampleFormat::Int { bits: 24 },
        }
    }
    fn exclusive(&self) -> bool {
        self.exclusive
    }
    fn write(&mut self, frames: &[i32]) -> Result<(), SinkError> {
        self.written.extend_from_slice(frames);
        Ok(())
    }
    fn flush(&mut self) -> Result<(), SinkError> {
        Ok(())
    }
    fn discard(&mut self) -> Result<(), SinkError> {
        Ok(())
    }
    fn set_volume(&mut self, _v: f32) {}
    fn reopen(&mut self, rate: u32) -> Result<(), SinkError> {
        // Only an exclusive device re-acquires at the track's native rate; a shared
        // device keeps its fixed rate (the deck resamples) — the real behaviour.
        if self.exclusive {
            self.rate = rate;
        }
        Ok(())
    }
}

/// An EQ decorator over [`CaptureSink`], mirroring the host's `DspSink`: convert the
/// deck's i32 block to f32, run the parametric EQ in place, clamp back to canonical
/// 24-bit, forward to the inner sink. When disabled it writes the block straight
/// through (bit-exact passthrough — the exact shape the host relies on).
struct EqSink {
    inner: CaptureSink,
    eq: Equalizer<MAX_BANDS>,
    bands: Vec<Band>,
    enabled: bool,
    channels: usize,
    buf: Vec<f32>,
}

impl EqSink {
    fn new(inner: CaptureSink, bands: Vec<Band>, enabled: bool) -> Self {
        let si = inner.stream_info();
        let channels = si.channels as usize;
        let mut eq = Equalizer::new();
        for (i, b) in bands.iter().enumerate().take(MAX_BANDS) {
            eq.set_band(i, design(b, si.sample_rate as f32));
        }
        eq.set_enabled(enabled);
        EqSink {
            inner,
            eq,
            bands,
            enabled,
            channels,
            buf: Vec::new(),
        }
    }
}

impl Sink for EqSink {
    fn stream_info(&self) -> StreamInfo {
        self.inner.stream_info()
    }
    fn exclusive(&self) -> bool {
        self.inner.exclusive()
    }
    fn write(&mut self, frames: &[i32]) -> Result<(), SinkError> {
        if !self.enabled {
            return self.inner.write(frames);
        }
        self.buf.clear();
        self.buf.extend(frames.iter().map(|&x| x as f32));
        self.eq.process(&mut self.buf, self.channels);
        let out: Vec<i32> = self
            .buf
            .iter()
            .map(|&y| y.round().clamp(-(FS24 as f32) - 1.0, FS24 as f32) as i32)
            .collect();
        self.inner.write(&out)
    }
    fn flush(&mut self) -> Result<(), SinkError> {
        self.inner.flush()
    }
    fn discard(&mut self) -> Result<(), SinkError> {
        self.eq.reset();
        self.inner.discard()
    }
    fn set_volume(&mut self, _v: f32) {}
    fn reopen(&mut self, rate: u32) -> Result<(), SinkError> {
        self.inner.reopen(rate)?;
        // Re-design the bands for the new device rate (coefficients are rate-dependent),
        // exactly as the host's DspSink does on a native-rate reopen.
        let fs = self.inner.stream_info().sample_rate as f32;
        for (i, b) in self.bands.iter().enumerate().take(MAX_BANDS) {
            self.eq.set_band(i, design(b, fs));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Signal analysis
// ---------------------------------------------------------------------------

fn deinterleave(xs: &[i32], ch: usize, c: usize) -> Vec<i32> {
    xs.chunks_exact(ch).map(|f| f[c]).collect()
}

fn peak(xs: &[i32]) -> i64 {
    xs.iter().map(|&s| i64::from(s).abs()).max().unwrap_or(0)
}

/// Goertzel single-bin power (unnormalized; only relative powers are compared).
fn goertzel_power(samples: &[i32], sample_rate: f64, target_hz: f64) -> f64 {
    let n = samples.len();
    if n == 0 {
        return 0.0;
    }
    let k = (0.5 + (n as f64) * target_hz / sample_rate).floor();
    let w = 2.0 * std::f64::consts::PI * k / n as f64;
    let coeff = 2.0 * w.cos();
    let (mut q1, mut q2) = (0.0f64, 0.0f64);
    for &s in samples {
        let q0 = coeff * q1 - q2 + f64::from(s);
        q2 = q1;
        q1 = q0;
    }
    q1 * q1 + q2 * q2 - q1 * q2 * coeff
}

/// Assert the captured stereo audio is the clean 440 Hz(L) / 659.25 Hz(R) tone at
/// `rate`, with no wrong-speed candidate (half/double, or the 44.1↔48 kHz shift) rivaling
/// the fundamental, no channel swap, and a sane (non-silent, non-clipped) level.
fn assert_two_tone(tag: &str, pcm: &[i32], rate: u32) {
    assert!(
        pcm.len() > 2000,
        "{tag}: too little audio captured ({})",
        pcm.len()
    );
    let l = deinterleave(pcm, 2, 0);
    let r = deinterleave(pcm, 2, 1);
    let sr = f64::from(rate);
    for (chan, name, want, other) in [
        (&l, "LEFT", LEFT_HZ, RIGHT_HZ),
        (&r, "RIGHT", RIGHT_HZ, LEFT_HZ),
    ] {
        let p_want = goertzel_power(chan, sr, want);
        let p_other = goertzel_power(chan, sr, other);
        let wrong = [
            want * 0.5,
            want * 2.0,
            want * 48_000.0 / 44_100.0,
            want * 44_100.0 / 48_000.0,
        ];
        let p_wrong = wrong
            .iter()
            .map(|&hz| goertzel_power(chan, sr, hz))
            .fold(0.0f64, f64::max);
        assert!(
            p_want > p_other * 50.0,
            "{tag} {name}: not {want:.2} Hz / channels swapped (want={p_want:.2e} other={p_other:.2e})"
        );
        assert!(
            p_want > p_wrong * 10.0,
            "{tag} {name}: wrong speed — {want:.2} Hz power {p_want:.2e} not >10x the best wrong candidate {p_wrong:.2e} ({wrong:?})"
        );
    }
    let p = peak(pcm);
    assert!(
        p > FS24 / 8 && p <= FS24,
        "{tag}: peak {p} — silent or clipped"
    );
}

// ---------------------------------------------------------------------------
// Pipeline drivers
// ---------------------------------------------------------------------------

/// Run `f` on a 64 MiB stack (`OwnedDecoder` is large by value).
fn on_big_stack<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn_scoped(s, f)
            .expect("spawn pipeline thread")
            .join()
            .expect("pipeline thread panicked")
    })
}

/// Pump the deck to end (bounded).
fn pump_to_end<K: Sink>(deck: &mut Deck<K, OwnedDecoder, 0>) {
    for _ in 0..1_000_000 {
        match deck.pump().expect("deck pump") {
            Progress::Ended | Progress::Idle => break,
            _ => {}
        }
    }
}

/// Decode `file` through a `Deck` writing into `sink`, pump to end, and return the
/// captured samples via `extract`.
fn run_pipeline<K: Sink + Send>(
    file: &str,
    sink: K,
    extract: impl FnOnce(&K) -> Vec<i32> + Send,
) -> Vec<i32> {
    let file = file.to_string();
    on_big_stack(move || {
        let dec = open_track(&testdata(&file)).expect("open_track");
        let mut deck: Deck<K, OwnedDecoder, 0> = Deck::new(sink, Quality::Hq);
        deck.load(dec).expect("load decoder into deck");
        pump_to_end(&mut deck);
        extract(deck.sink())
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// SHARED output: a 44.1 kHz tone played on a 48 kHz device must be resampled to a
/// clean 440/659 tone at 48 kHz (the exact resample path that mangled speed in the wild).
#[test]
fn shared_output_resamples_the_tone_to_the_device_rate() {
    let out = run_pipeline("tone.flac", CaptureSink::shared(48_000), |s| {
        s.written.clone()
    });
    assert_two_tone("shared@48k", &out, 48_000);
}

/// EXCLUSIVE output: the device is reopened to the track's NATIVE rate, so playback is
/// bit-perfect — no resampling. The captured tone must be clean at 44.1 kHz, and the
/// sink must have actually landed on 44.1 kHz.
#[test]
fn exclusive_output_reopens_to_native_rate_bit_perfect() {
    let out = on_big_stack(|| {
        let dec = open_track(&testdata("tone.flac")).expect("open_track");
        // Exclusive device opens at a 48 kHz mix-format first.
        let mut deck: Deck<CaptureSink, OwnedDecoder, 0> =
            Deck::new(CaptureSink::exclusive(48_000), Quality::Hq);
        deck.load(dec).expect("load");
        // Native-rate switch: reopen the exclusive device to the track's rate.
        deck.reopen_sink_at(TONE_RATE)
            .expect("reopen to native rate");
        assert_eq!(
            deck.sink().rate,
            TONE_RATE,
            "exclusive device should have reopened to the native rate"
        );
        pump_to_end(&mut deck);
        deck.sink().written.clone()
    });
    assert_two_tone("exclusive@44.1k(bit-perfect)", &out, TONE_RATE);
}

/// EQ ON: a +12 dB peaking band centred on the LEFT tone (440 Hz) must visibly boost
/// that bin — its power with EQ well above the same bin with EQ off — while leaving the
/// untouched RIGHT tone (659 Hz) essentially unchanged.
#[test]
fn eq_peak_boosts_the_target_band() {
    // A narrow (Q=4) +12 dB peak centred on 440 Hz: the -3 dB band (~385..495 Hz) is
    // well clear of the 659 Hz RIGHT tone, so the boost is frequency-selective.
    let bands = vec![Band {
        kind: BandKind::Peaking,
        freq: 440.0,
        gain_db: 12.0,
        q: 4.0,
        enabled: true,
    }];

    let flat = run_pipeline("tone.flac", CaptureSink::shared(TONE_RATE), |s| {
        s.written.clone()
    });
    let eqd = run_pipeline(
        "tone.flac",
        EqSink::new(CaptureSink::shared(TONE_RATE), bands, true),
        |s| s.inner.written.clone(),
    );

    let sr = f64::from(TONE_RATE);
    let l_flat = goertzel_power(&deinterleave(&flat, 2, 0), sr, LEFT_HZ);
    let l_eqd = goertzel_power(&deinterleave(&eqd, 2, 0), sr, LEFT_HZ);
    let r_flat = goertzel_power(&deinterleave(&flat, 2, 1), sr, RIGHT_HZ);
    let r_eqd = goertzel_power(&deinterleave(&eqd, 2, 1), sr, RIGHT_HZ);
    let l_boost = l_eqd / l_flat.max(1.0);
    let r_boost = r_eqd / r_flat.max(1.0);

    // +12 dB ≈ x4 amplitude ≈ x16 power; require a clear boost (>x3) on the on-target
    // LEFT tone, allowing for the tone not sitting exactly on the design bin.
    assert!(
        l_eqd > l_flat * 3.0,
        "EQ +12 dB @ 440 Hz did not boost the on-target LEFT tone: flat={l_flat:.2e} eq={l_eqd:.2e}"
    );
    // Frequency-selective: the on-centre 440 Hz tone is boosted far more than the
    // off-centre 659 Hz tone (which sits outside the narrow band).
    assert!(
        l_boost > r_boost * 3.0,
        "EQ @ 440 Hz (Q=4) should boost the on-centre 440 Hz tone much more than the off-centre 659 Hz tone: 440 boost x{l_boost:.1} vs 659 boost x{r_boost:.1}"
    );
    // (No tone-purity assert here: a +12 dB boost intentionally drives the tone into the
    // 24-bit clamp, whose harmonics are expected — purity is covered by the EQ-off tests.)
}

/// EQ OFF/BYPASSED: a disabled EQ must be a bit-exact passthrough — the captured audio
/// is identical, sample for sample, to running with no EQ sink at all.
#[test]
fn disabled_eq_is_bit_identical_to_no_eq() {
    let no_eq = run_pipeline("tone.flac", CaptureSink::shared(48_000), |s| {
        s.written.clone()
    });
    let bands = soul_audio_eq::default_bands().to_vec();
    let bypassed = run_pipeline(
        "tone.flac",
        EqSink::new(CaptureSink::shared(48_000), bands, false),
        |s| s.inner.written.clone(),
    );
    assert_eq!(
        no_eq,
        bypassed,
        "a disabled EQ must be a bit-exact passthrough (lengths {} vs {})",
        no_eq.len(),
        bypassed.len()
    );
}

/// LIVE OUTPUT SWITCH: swap the sink mid-track (the core mechanism behind the app's live
/// device/exclusive change) — audio captured on the NEW sink must still be the correct
/// tone at the new rate, proving `swap_sink` rebuilds the resampler without corrupting
/// the signal or its speed.
#[test]
fn live_output_swap_keeps_the_signal_correct() {
    let out = on_big_stack(|| {
        let dec = open_track(&testdata("tone.flac")).expect("open_track");
        let mut deck: Deck<CaptureSink, OwnedDecoder, 0> =
            Deck::new(CaptureSink::shared(48_000), Quality::Hq);
        deck.load(dec).expect("load");
        // Play a bit on the 48 kHz device.
        for _ in 0..40 {
            if matches!(deck.pump().expect("pump"), Progress::Ended | Progress::Idle) {
                break;
            }
        }
        // Switch output live to a 44.1 kHz device — fresh sink, so what it captures is
        // purely post-swap audio.
        deck.swap_sink(CaptureSink::shared(44_100))
            .expect("swap_sink mid-track");
        pump_to_end(&mut deck);
        deck.sink().written.clone()
    });
    assert_two_tone("post-swap@44.1k", &out, 44_100);
}
