//! GATE 2 (deck level) — rigorous **de-click validation on REAL decoded audio**.
//!
//! The crude "max |Δ| at the splice" check only bounds the first step. Here we
//! drive a real [`Deck`] with a **real decoded fixture** (`tone.wav`, a lossless
//! PCM tone — so the decode is bit-exact and sample-accurate, and its second
//! difference is *meaningfully non-zero*, unlike a ramp (d2 ≡ 0) or a constant
//! (d2 ≡ 0); documented choice) and apply two research-backed metrics:
//!
//!   1. **Null-residual.** Capture a natural play-through and a `seek(P)`
//!      play-through. The seek must reproduce the *same* signal, modulated ONLY
//!      by the known de-click fade: `seek_out[j] == round(natural[P+j] ·
//!      smoothstep(j/fade_len))` inside the fade window and `== natural[P+j]`
//!      after it. Any click is a residual spike away from that identity. The
//!      fixture being lossless PCM with sample-accurate WAV seeking, the residual
//!      is bit-exact (tolerance 1 LSB for the f32 fade multiply).
//!
//!   2. **Second-difference outlier.** `second_difference_peak` over the splice
//!      window (which includes the fade-in from zero) must stay within a small
//!      factor K of the peak over a clean interior window of the same natural
//!      signal — i.e. the splice introduces no slope break beyond the audio's own
//!      curvature. A hard cut would spike d2 to ≈ full scale, orders of magnitude
//!      over the tone's natural d2.
//!
//! The sink is rate- and channel-matched to the fixture (44.1 kHz stereo) so the
//! deck takes its no-resampler pass-through path: the only transform between
//! source and captured output is the 16→24-bit normalization (×256, exact) and,
//! on the seek side, the smoothstep fade — which is exactly what the null-residual
//! identity models.

use soul_audio_core::{
    AnyDecoder, Deck, Decoder, Progress, Quality, SampleFormat, Sink, SinkError, SliceSource,
    StreamInfo, open,
};

const TONE_WAV: &[u8] = include_bytes!("../../../testdata/tone.wav");
const FADE_MS: u32 = 8; // mirrors deck.rs FADE_MS

// ---------------------------------------------------------------------------
// Test doubles + metric helpers.
// ---------------------------------------------------------------------------

/// Captures every sample the deck writes (a seek does NOT discard here — the
/// default no-op `discard` keeps the buffer, so pre- and post-seek output sit
/// adjacent and we can read the post-seek tail directly).
struct CaptureSink {
    rate: u32,
    ch: u16,
    written: Vec<i32>,
}
impl Sink for CaptureSink {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: self.rate,
            channels: self.ch,
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
}

/// `max |x[i] − 2·x[i−1] + x[i−2]|` — the discrete second difference's peak. A
/// smooth signal (incl. a smoothstep-faded one) keeps this bounded; a slope break
/// (a hard splice / click) spikes it.
fn second_difference_peak(xs: &[i32]) -> i64 {
    xs.windows(3)
        .map(|w| (i64::from(w[2]) - 2 * i64::from(w[1]) + i64::from(w[0])).abs())
        .max()
        .unwrap_or(0)
}

/// The deck's seek de-click gain for output frame `j` past the splice: a rising
/// smoothstep over `fade_len`, flat 1.0 after. Identical curve to `Deck`.
fn fade_gain(j: usize, fade_len: usize) -> f32 {
    if j >= fade_len {
        return 1.0;
    }
    let t = j as f32 / fade_len as f32;
    t * t * (3.0 - 2.0 * t)
}

/// Replicate the deck's per-sample output arithmetic: multiply by the fade gain,
/// round half away from zero (its no-`f32::round` form), truncate to i32. (The
/// 16→24-bit norm is already folded into `natural`, which is `source·256`, exact.)
fn apply_fade(natural_sample: i32, g: f32) -> i32 {
    let s = natural_sample as f32 * g;
    let r = if s >= 0.0 { s + 0.5 } else { s - 0.5 };
    r as i32
}

/// Deinterleave channel `c` of an interleaved `ch`-channel buffer.
fn channel(xs: &[i32], ch: usize, c: usize) -> Vec<i32> {
    xs.chunks_exact(ch).map(|f| f[c]).collect()
}

fn big_stack<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn_scoped(s, f)
            .expect("spawn")
            .join()
            .expect("thread panicked")
    })
}

fn tone_decoder() -> AnyDecoder<SliceSource<'static>> {
    open(SliceSource::new(TONE_WAV)).expect("open tone.wav")
}

/// Natural play-through from 0: pump to the end, return the full interleaved
/// output (so `natural[(P+j)*ch + c]` is the sample a straight decode emits).
fn natural_playthrough(rate: u32, ch: u16) -> Vec<i32> {
    let mut deck: Deck<_, _, 1> = Deck::new(
        CaptureSink {
            rate,
            ch,
            written: Vec::new(),
        },
        Quality::Fast,
    );
    deck.load(tone_decoder()).expect("load");
    for _ in 0..100_000 {
        match deck.pump().expect("pump") {
            Progress::Ended | Progress::Idle => break,
            _ => {}
        }
    }
    deck.sink().written.clone()
}

/// Pump one block (pre-seek), `seek(p)`, then pump `blocks` more; return the
/// post-seek interleaved output (everything after the pre-seek block).
fn seek_playthrough(rate: u32, ch: u16, p: u64, blocks: usize) -> Vec<i32> {
    let mut deck: Deck<_, _, 1> = Deck::new(
        CaptureSink {
            rate,
            ch,
            written: Vec::new(),
        },
        Quality::Fast,
    );
    deck.load(tone_decoder()).expect("load");
    deck.pump().expect("pre-seek pump");
    let pre = deck.sink().written.len();
    assert_eq!(
        deck.seek(p).expect("seek"),
        p,
        "WAV seek is sample-accurate"
    );
    for _ in 0..blocks {
        deck.pump().expect("post-seek pump");
    }
    deck.sink().written[pre..].to_vec()
}

#[test]
fn seek_declick_null_residual_and_bounded_second_difference_on_real_audio() {
    big_stack(|| {
        let si = tone_decoder().stream_info();
        let (rate, chn) = (si.sample_rate, si.channels);
        let ch = usize::from(chn);
        assert_eq!(rate, 44_100);
        assert_eq!(ch, 2);
        // Deck fade length in output frames: round(FADE_MS·rate/1000) — same
        // integer rounding as the deck (no f32::round).
        let fade_len = ((FADE_MS * rate + 500) / 1000) as usize;

        let natural = natural_playthrough(rate, chn);
        let total_frames = natural.len() / ch;
        assert!(total_frames > 60_000, "fixture too short: {total_frames}");

        let p = 40_000u64; // mid-stream, with ample runway after
        let window = fade_len + 2_000;
        // Enough post-seek blocks (BLOCK=256 frames each) to cover the window.
        let blocks = window / 256 + 4;
        let post = seek_playthrough(rate, chn, p, blocks);
        assert!(
            post.len() >= window * ch,
            "post-seek capture too short: {} frames",
            post.len() / ch
        );

        // ---- (1) Null-residual: the seek reproduces the natural signal,
        //         modulated only by the known smoothstep fade. ----
        // The splice starts at exactly zero (smoothstep t=0), not a step.
        assert_eq!(&post[..ch], &vec![0i32; ch][..], "fade starts at zero");
        let mut max_fade_resid = 0i64;
        for j in 0..window {
            let g = fade_gain(j, fade_len);
            for c in 0..ch {
                let seek_s = post[j * ch + c];
                let nat_s = natural[(p as usize + j) * ch + c];
                if j >= fade_len {
                    // Past the fade: bit-exact (gain is exactly 1.0).
                    assert_eq!(
                        seek_s, nat_s,
                        "post-fade frame {j} ch {c}: seek {seek_s} != natural {nat_s} \
                         (a residual = a click)"
                    );
                } else {
                    // Inside the fade: equals natural × smoothstep gain, to 1 LSB.
                    let expect = apply_fade(nat_s, g);
                    let resid = i64::from(seek_s) - i64::from(expect);
                    max_fade_resid = max_fade_resid.max(resid.abs());
                    assert!(
                        resid.abs() <= 1,
                        "fade frame {j} ch {c}: seek {seek_s} vs natural·gain {expect} \
                         (resid {resid}) — not a clean smoothstep-modulated signal"
                    );
                }
            }
        }

        // ---- (2) Second-difference outlier: the splice window's d2 peak is
        //         within K× the clean interior window's d2 peak. ----
        // The AUDIBLE splice is silence→post-seek: on a seek the sink discards
        // its buffered audio and plays silence until the deck's faded output
        // arrives, so the device hears `…0, 0, post[0], post[1], …`. We model
        // that by prepending a few silent frames to the post-seek output — now a
        // missing/broken fade (post[0] = full scale) is a 0→full STEP whose d2
        // spikes to ≈ full scale, while the real smoothstep (post[0] = 0, zero
        // slope) stays flat. Clean window: a stretch of the natural signal far
        // from any edge.
        const WIN: usize = 2_048;
        const LEAD: usize = 4; // silent frames the sink plays post-discard
        const K: i64 = 3;
        let clean_start = 20_000usize; // interior, nowhere near start/seek/end
        let clean = &natural[clean_start * ch..(clean_start + WIN) * ch];
        let mut splice = vec![0i32; LEAD * ch];
        splice.extend_from_slice(&post[..(WIN - LEAD) * ch]);

        // Per-channel (interleaving would fabricate slope breaks between L/R).
        let mut clean_d2 = 0i64;
        let mut splice_d2 = 0i64;
        for c in 0..ch {
            clean_d2 = clean_d2.max(second_difference_peak(&channel(clean, ch, c)));
            splice_d2 = splice_d2.max(second_difference_peak(&channel(&splice, ch, c)));
        }

        println!(
            "declick: fade_len={fade_len} max_fade_residual={max_fade_resid} LSB  \
             clean_d2={clean_d2}  splice_d2={splice_d2}  ratio={:.3}  (K={K})",
            splice_d2 as f64 / clean_d2.max(1) as f64
        );

        assert!(
            clean_d2 > 100,
            "clean-window d2 {clean_d2} is ~0 — fixture is not tonal enough to make \
             the ratio test honest; pick a tonal/music fixture"
        );
        assert!(
            splice_d2 <= K * clean_d2,
            "splice d2 {splice_d2} exceeds {K}× the clean d2 {clean_d2} — the seek \
             introduced a slope break (click) beyond the audio's own curvature"
        );
    });
}
