//! Multichannel → stereo downmix: the deck folds 3.0 / 5.1 / 7.1 sources onto a stereo
//! sink with the ITU-R BS.775 coefficients (center + surrounds at −3 dB, LFE dropped),
//! and leaves the layout-ambiguous counts (4/5/7) rejected. Constant per-channel values
//! make the exact coefficient arithmetic checkable.

use soul_audio_core::{
    Deck, DecodeError, Decoder, Progress, Quality, SampleFormat, SeekableDecoder, Sink, SinkError,
    StreamInfo,
};

/// Emits one constant `vals` frame per output frame until `frames` is exhausted.
struct ConstMulti {
    ch: usize,
    vals: Vec<i32>,
    pos: u64,
    frames: u64,
}
impl Decoder for ConstMulti {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: 48_000,
            channels: self.ch as u16,
            format: SampleFormat::Int { bits: 24 },
        }
    }
    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        let want = ((out.len() / self.ch) as u64).min(self.frames - self.pos) as usize;
        for f in 0..want {
            for c in 0..self.ch {
                out[f * self.ch + c] = self.vals[c];
            }
        }
        self.pos += want as u64;
        Ok(want * self.ch)
    }
    fn encoder_delay(&self) -> u64 {
        0
    }
    fn playable_frames(&self) -> Option<u64> {
        None
    }
}
impl SeekableDecoder for ConstMulti {
    fn seek_to(&mut self, s: u64) -> Result<u64, DecodeError> {
        self.pos = s.min(self.frames);
        Ok(self.pos)
    }
    fn total_frames(&self) -> Option<u64> {
        Some(self.frames)
    }
}

struct StereoCapture {
    written: Vec<i32>,
}
impl Sink for StereoCapture {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: 48_000,
            channels: 2,
            format: SampleFormat::Int { bits: 24 },
        }
    }
    fn write(&mut self, f: &[i32]) -> Result<(), SinkError> {
        self.written.extend_from_slice(f);
        Ok(())
    }
    fn flush(&mut self) -> Result<(), SinkError> {
        Ok(())
    }
}

/// −3 dB fold coefficient.
const C: f64 = std::f64::consts::FRAC_1_SQRT_2;

/// Play a constant `ch`-channel frame through a stereo deck and return a (Lo, Ro) sample
/// from well past the fade-in.
fn play_downmix(ch: usize, vals: Vec<i32>) -> (i32, i32) {
    let mut deck: Deck<_, ConstMulti, 0> = Deck::new(
        StereoCapture {
            written: Vec::new(),
        },
        Quality::Hq,
    );
    deck.load(ConstMulti {
        ch,
        vals,
        pos: 0,
        frames: 4_800,
    })
    .unwrap();
    for _ in 0..100_000 {
        match deck.pump().unwrap() {
            Progress::Ended | Progress::Idle => break,
            _ => {}
        }
    }
    let out = &deck.sink().written;
    let i = 3_000; // steady state, clear of the fade-in
    assert!(
        out.len() > i * 2 + 1,
        "too little output: {} samples",
        out.len()
    );
    (out[i * 2], out[i * 2 + 1])
}

#[test]
fn downmix_5_1_uses_itu_coefficients_and_drops_lfe() {
    // FL FR FC LFE BL BR — LFE is huge to prove it's dropped from the mix.
    let (lo, ro) = play_downmix(6, vec![1000, 2000, 3000, 9_000_000, 5000, 6000]);
    let exp_lo = (1000.0 + C * 3000.0 + C * 5000.0).round() as i32;
    let exp_ro = (2000.0 + C * 3000.0 + C * 6000.0).round() as i32;
    assert!((lo - exp_lo).abs() <= 1, "5.1 Lo {lo} != {exp_lo}");
    assert!((ro - exp_ro).abs() <= 1, "5.1 Ro {ro} != {exp_ro}");
}

#[test]
fn downmix_3_0_folds_center() {
    let (lo, ro) = play_downmix(3, vec![1000, 2000, 3000]);
    assert!(
        (lo - (1000.0 + C * 3000.0).round() as i32).abs() <= 1,
        "3.0 Lo {lo}"
    );
    assert!(
        (ro - (2000.0 + C * 3000.0).round() as i32).abs() <= 1,
        "3.0 Ro {ro}"
    );
}

#[test]
fn downmix_7_1_folds_side_and_back() {
    // FL FR FC LFE BL BR SL SR.
    let (lo, ro) = play_downmix(8, vec![1000, 2000, 3000, 9_000_000, 5000, 6000, 7000, 8000]);
    let exp_lo = (1000.0 + C * (3000.0 + 7000.0 + 5000.0)).round() as i32;
    let exp_ro = (2000.0 + C * (3000.0 + 8000.0 + 6000.0)).round() as i32;
    assert!((lo - exp_lo).abs() <= 1, "7.1 Lo {lo} != {exp_lo}");
    assert!((ro - exp_ro).abs() <= 1, "7.1 Ro {ro} != {exp_ro}");
}

#[test]
fn ambiguous_channel_counts_are_rejected() {
    // 4 (quad vs 3.1), 5 (5.0 vs 4.1), 7 (6.1 variants) stay rejected without a layout mask.
    for ch in [4usize, 5, 7] {
        let mut deck: Deck<_, ConstMulti, 0> = Deck::new(
            StereoCapture {
                written: Vec::new(),
            },
            Quality::Hq,
        );
        assert!(
            deck.load(ConstMulti {
                ch,
                vals: vec![100; ch],
                pos: 0,
                frames: 100,
            })
            .is_err(),
            "{ch}ch should stay rejected (ambiguous layout)"
        );
    }
}

#[test]
fn downmix_clips_gracefully_on_hot_in_phase_surround() {
    // Every channel near full scale and in phase → the summed Lo/Ro exceeds full scale and
    // must CLAMP (not wrap). No normalization is applied; the output clamp is the net.
    let fs = 8_000_000;
    let (lo, ro) = play_downmix(6, vec![fs, fs, fs, 0, fs, fs]);
    assert!(
        lo.abs() <= 8_388_607 && ro.abs() <= 8_388_607,
        "downmix must clamp, not wrap: lo={lo} ro={ro}"
    );
    assert!(
        lo > 7_000_000 && ro > 7_000_000,
        "clamped near full scale, not silent or wrapped: lo={lo} ro={ro}"
    );
}
