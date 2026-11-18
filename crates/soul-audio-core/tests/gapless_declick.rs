//! Inter-track declick: a continuous sine split across two GAPLESS tracks must join with
//! no slope break — a click spikes the second difference even where the sample VALUES
//! line up. Complements the existing gapless frame-count / boundary-value tests (which
//! can't see a phase glitch at the seam).

use soul_audio_core::{
    Deck, DecodeError, Decoder, Progress, Quality, SampleFormat, SeekableDecoder, Sink, SinkError,
    StreamInfo,
};

const FS: u32 = 48_000;
const CH: usize = 2;

/// A stereo sine whose phase is a function of the GLOBAL frame index `pos`, so two
/// decoders — one over `[0, split)`, one over `[split, total)` — produce a seamless
/// continuous sine when played back-to-back.
struct SineDecoder {
    pos: u64,
    total: u64,
    freq: f64,
    amp: f64,
}
impl Decoder for SineDecoder {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: FS,
            channels: CH as u16,
            format: SampleFormat::Int { bits: 24 },
        }
    }
    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        let want = ((out.len() / CH) as u64).min(self.total - self.pos) as usize;
        for f in 0..want {
            let g = self.pos + f as u64;
            let s = (self.amp
                * (2.0 * std::f64::consts::PI * self.freq * g as f64 / f64::from(FS)).sin())
                as i32;
            for c in 0..CH {
                out[f * CH + c] = s;
            }
        }
        self.pos += want as u64;
        Ok(want * CH)
    }
    fn encoder_delay(&self) -> u64 {
        0
    }
    fn playable_frames(&self) -> Option<u64> {
        None
    }
}
impl SeekableDecoder for SineDecoder {
    fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        self.pos = sample.min(self.total);
        Ok(self.pos)
    }
    fn total_frames(&self) -> Option<u64> {
        Some(self.total)
    }
}

struct CaptureSink {
    written: Vec<i32>,
}
impl Sink for CaptureSink {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: FS,
            channels: CH as u16,
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

/// Max |second difference| of channel 0 over frames `[from, to)`. The second difference of
/// a sine tracks its curvature (ω²·amp·|sin|); a slope break (click) spikes it.
fn max_abs_d2(pcm: &[i32], from: usize, to: usize) -> i64 {
    let ch0: Vec<i64> = pcm.chunks_exact(CH).map(|f| i64::from(f[0])).collect();
    let mut m = 0i64;
    for i in from.max(1)..to.min(ch0.len().saturating_sub(1)) {
        m = m.max((ch0[i + 1] - 2 * ch0[i] + ch0[i - 1]).abs());
    }
    m
}

#[test]
fn gapless_track_boundary_has_no_click() {
    let split = 4_800u64; // 0.1 s
    let total = 9_600u64;
    let (freq, amp) = (997.0, 4_000_000.0);

    let mut deck: Deck<_, SineDecoder, 1> = Deck::new(
        CaptureSink {
            written: Vec::new(),
        },
        Quality::Hq,
    );
    deck.load(SineDecoder {
        pos: 0,
        total: split,
        freq,
        amp,
    })
    .unwrap();
    deck.preload(SineDecoder {
        pos: split,
        total,
        freq,
        amp,
    })
    .unwrap();

    let mut swaps = 0;
    for _ in 0..100_000 {
        match deck.pump().unwrap() {
            Progress::TrackChanged => swaps += 1,
            Progress::Ended | Progress::Idle => break,
            Progress::Played => {}
        }
    }
    assert_eq!(swaps, 1, "exactly one gapless swap");
    let out = deck.sink().written.clone();
    let frames = out.len() / CH;
    assert!(
        frames as u64 >= total - 8,
        "both tracks played: {frames} frames"
    );

    // Compare the max 2nd-difference in a window AROUND the boundary (at `split` frames) to
    // a clean window in the middle of track A — both span several periods, so each includes
    // the sine's natural curvature peak. A click at the seam would make the boundary spike.
    let bnd = split as usize;
    let win = 240usize; // ~5 periods of 997 Hz
    let clean = max_abs_d2(&out, bnd / 2 - win, bnd / 2 + win);
    let boundary = max_abs_d2(&out, bnd - win, bnd + win);
    println!("clean max|d2|={clean}  boundary max|d2|={boundary}");
    assert!(
        boundary <= clean * 3,
        "gapless boundary click: boundary max|d2| {boundary} >> clean {clean} (a slope break at the seam)"
    );
}
