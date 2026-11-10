//! DSD **playback-path** correctness tests.
//!
//! Exercises the real player path end to end: `DsfDecoder`/`DffDecoder` →
//! [`PlaybackDecimateAdapter`] (DSD → native 44.1 kHz-family PCM) → optionally a
//! [`Deck`] resample to a device rate. The `tone.*` fixtures (see
//! `testdata/generate_dsd.py`) carry A4 = 440 Hz on the LEFT channel and
//! E5 = 659.25 Hz on the RIGHT, synthesized at the stated DSD rate, so pitch,
//! channel order, rate and level are all checkable against known truth.
//!
//! These pin the shared DSD→PCM decimation that both containers run: a wrong
//! decimation ratio (pitch/rate), a channel swap, a bit-order error (noise), or a
//! level regression each break a specific assertion here.

use soul_audio_core::formats::dff::DffDecoder;
use soul_audio_core::formats::dsf::DsfDecoder;
use soul_audio_core::{
    Deck, DeckState, DecodeError, Decoder, PlaybackDecimateAdapter, Progress, Quality,
    SampleFormat, SeekableDecoder, Sink, SinkError, SliceSource, StreamInfo,
};

const TONE_DSF: &[u8] = include_bytes!("../../../testdata/tone.dsf");
const TONE_DFF: &[u8] = include_bytes!("../../../testdata/tone.dff");
const TONE_DSD128: &[u8] = include_bytes!("../../../testdata/tone-dsd128.dsf");
const TONE_DSD256: &[u8] = include_bytes!("../../../testdata/tone-dsd256.dsf");
const TONE_DSD512: &[u8] = include_bytes!("../../../testdata/tone-dsd512.dsf");

/// 24-bit full scale; the modulator fixture drives AMP = 0.4 → peak ≈ 0.4·2^23.
const FS24: i32 = 8_388_607;

// ---- decode helpers (big-stack thread: the adapter + decoder are large) ----

fn decode_dsf(bytes: &'static [u8]) -> (Vec<i32>, u32, u16) {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn_scoped(scope, || {
                let inner = DsfDecoder::new(SliceSource::new(bytes)).expect("open dsf");
                let mut dec = PlaybackDecimateAdapter::new(inner);
                let si = dec.stream_info();
                (drain(&mut dec), si.sample_rate, si.channels)
            })
            .expect("spawn")
            .join()
            .expect("panicked")
    })
}

fn decode_dff(bytes: &'static [u8]) -> (Vec<i32>, u32, u16) {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn_scoped(scope, || {
                let inner = DffDecoder::new(SliceSource::new(bytes)).expect("open dff");
                let mut dec = PlaybackDecimateAdapter::new(inner);
                let si = dec.stream_info();
                (drain(&mut dec), si.sample_rate, si.channels)
            })
            .expect("spawn")
            .join()
            .expect("panicked")
    })
}

fn drain<D: Decoder>(dec: &mut D) -> Vec<i32> {
    let mut out = Vec::new();
    let mut buf = [0i32; 4096];
    loop {
        let n = dec.decode_into(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

// ---- signal measurement ----

fn deinterleave(pcm: &[i32], channels: usize, ch: usize) -> Vec<i32> {
    pcm.iter().skip(ch).step_by(channels).copied().collect()
}

/// Goertzel single-bin power (unnormalized; compare against another same-length call).
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

fn peak(s: &[i32]) -> i32 {
    s.iter().map(|x| x.abs()).max().unwrap_or(0)
}

/// Assert a stereo decode is the expected 2-tone: L≈440 Hz, R≈659.25 Hz (each
/// channel dominated by its own tone by a wide margin), at ~0.4 full-scale.
fn assert_two_tone(tag: &str, pcm: &[i32], rate: u32, ch: u16, expect_rate: u32) {
    assert_eq!(rate, expect_rate, "{tag}: wrong playback rate");
    assert_eq!(ch, 2, "{tag}: not stereo");
    let l = deinterleave(pcm, 2, 0);
    let r = deinterleave(pcm, 2, 1);
    assert!(l.len() > 1000, "{tag}: too few frames ({})", l.len());
    let sr = f64::from(rate);
    let l440 = goertzel_power(&l, sr, 440.0);
    let l659 = goertzel_power(&l, sr, 659.25);
    let r440 = goertzel_power(&r, sr, 440.0);
    let r659 = goertzel_power(&r, sr, 659.25);
    assert!(
        l440 > l659 * 100.0,
        "{tag}: LEFT not dominated by 440 Hz (440={l440:.3e} 659={l659:.3e}) — \
         wrong pitch/rate, channel swap, or bit-order noise"
    );
    assert!(
        r659 > r440 * 100.0,
        "{tag}: RIGHT not dominated by 659.25 Hz (659={r659:.3e} 440={r440:.3e})"
    );
    // Level: AMP 0.4 → peak in a generous [0.2, 0.75]·FS window (modulator + FIR
    // overshoot headroom). Catches a scale regression without being brittle.
    let p = peak(pcm);
    assert!(
        p > FS24 / 5 && p < FS24 * 3 / 4,
        "{tag}: peak {p} outside expected ~0.4·FS band — level regression"
    );
}

#[test]
fn dsf_dsd64_decodes_to_native_44100_two_tone() {
    let (pcm, rate, ch) = decode_dsf(TONE_DSF);
    assert_two_tone("DSF/DSD64", &pcm, rate, ch, 44_100);
    // 0.5 s fixture → ~22_050 frames.
    assert!((pcm.len() / 2).abs_diff(22_050) < 256, "frame count off");
}

#[test]
fn dff_dsd64_decodes_to_native_44100_two_tone() {
    let (pcm, rate, ch) = decode_dff(TONE_DFF);
    assert_two_tone("DFF/DSD64", &pcm, rate, ch, 44_100);
}

#[test]
fn dsf_and_dff_decode_identically() {
    // Same recording in both containers (DSF LSB-first+planar, DFF MSB-first
    // interleaved) must normalize to the same PCM — a bit-order/transpose bug in
    // either decoder would decorrelate them.
    let (dsf, _, _) = decode_dsf(TONE_DSF);
    let (dff, _, _) = decode_dff(TONE_DFF);
    let n = dsf.len().min(dff.len());
    assert!(n > 1000);
    let num: f64 = dsf[..n]
        .iter()
        .zip(&dff[..n])
        .map(|(&a, &b)| f64::from(a) * f64::from(b))
        .sum();
    let da: f64 = dsf[..n].iter().map(|&x| f64::from(x).powi(2)).sum();
    let db: f64 = dff[..n].iter().map(|&x| f64::from(x).powi(2)).sum();
    let r = num / (da.sqrt() * db.sqrt()).max(1.0);
    assert!(
        r > 0.999,
        "DSF vs DFF correlation {r:.4} — containers disagree"
    );
}

#[test]
fn higher_dsd_families_land_on_their_native_rate() {
    // The 8:1×8:1 (=64:1) cascade maps each family to DSD_rate/64; pitch unchanged.
    for (tag, bytes, exp) in [
        ("DSD128", TONE_DSD128, 88_200u32),
        ("DSD256", TONE_DSD256, 176_400),
        ("DSD512", TONE_DSD512, 352_800),
    ] {
        let (pcm, rate, ch) = decode_dsf(bytes);
        assert_two_tone(tag, &pcm, rate, ch, exp);
    }
}

// ---- full Deck path: DSD → deck resample (44.1k → 48k device) → sink ----

/// A `SeekableDecoder` wrapper over the DSD playback adapter — the shape the
/// desktop's `OwnedDecoder` presents to the deck.
struct DeckDsf(PlaybackDecimateAdapter<DsfDecoder<SliceSource<'static>>>);
impl Decoder for DeckDsf {
    fn stream_info(&self) -> StreamInfo {
        self.0.stream_info()
    }
    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        self.0.decode_into(out)
    }
}
impl SeekableDecoder for DeckDsf {
    fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        self.0.seek_to_frame(sample)
    }
    fn total_frames(&self) -> Option<u64> {
        self.0.total_frames()
    }
}

struct CaptureSink {
    rate: u32,
    written: Vec<i32>,
}
impl Sink for CaptureSink {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: self.rate,
            channels: 2,
            format: SampleFormat::Int { bits: 24 },
        }
    }
    fn write(&mut self, frames: &[i32]) -> Result<(), SinkError> {
        self.written.extend_from_slice(frames);
        Ok(())
    }
    fn flush(&mut self) -> Result<(), SinkError> {
        Ok(())
    }
    fn set_volume(&mut self, _v: f32) {}
}

#[test]
fn dsd_plays_clean_through_deck_at_48k() {
    // The actual runtime path: DSD64 (→44.1 kHz) resampled by the deck to a
    // 48 kHz device. Output must still be the clean 2-tone at 48 kHz — not
    // pitch-shifted (rate bug), swapped, hashy (bit order), or clipped.
    let out = std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn_scoped(scope, || {
                let inner = DsfDecoder::new(SliceSource::new(TONE_DSF)).expect("dsf");
                let dec = DeckDsf(PlaybackDecimateAdapter::new(inner));
                let sink = CaptureSink {
                    rate: 48_000,
                    written: Vec::new(),
                };
                let mut deck: Deck<_, _, 0> = Deck::new(sink, Quality::Hq);
                deck.load(dec).expect("load dsd into deck");
                for _ in 0..100_000 {
                    match deck.pump().unwrap() {
                        Progress::Ended | Progress::Idle => break,
                        _ => {}
                    }
                }
                assert_eq!(deck.state(), DeckState::Ended);
                deck.sink().written.clone()
            })
            .expect("spawn")
            .join()
            .expect("panicked")
    });

    assert!(
        out.len() > 2000,
        "deck produced too little audio ({})",
        out.len()
    );
    let l = deinterleave(&out, 2, 0);
    let r = deinterleave(&out, 2, 1);
    let l440 = goertzel_power(&l, 48_000.0, 440.0);
    let l659 = goertzel_power(&l, 48_000.0, 659.25);
    let r440 = goertzel_power(&r, 48_000.0, 440.0);
    let r659 = goertzel_power(&r, 48_000.0, 659.25);
    assert!(
        l440 > l659 * 100.0,
        "deck@48k LEFT not 440 Hz (440={l440:.3e} 659={l659:.3e})"
    );
    assert!(
        r659 > r440 * 100.0,
        "deck@48k RIGHT not 659.25 Hz (659={r659:.3e} 440={r440:.3e})"
    );
    // Not clipped to canonical full scale, and not collapsed to silence.
    let p = peak(&out);
    assert!(
        p > FS24 / 5 && p < FS24 * 15 / 16,
        "deck@48k peak {p} — clipping or level regression"
    );
}
