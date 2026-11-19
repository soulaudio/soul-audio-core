//! Multichannel → stereo downmix: the deck folds a multichannel source onto a stereo
//! sink with the ITU-R BS.775 coefficients (center + surrounds at −3 dB, LFE dropped).
//! The unambiguous counts (3.0 / 5.1 / 7.1) fold on the count alone; the layout-ambiguous
//! counts (4/5/7) fold only when the decoder reports a channel mask, and stay rejected
//! without one. Constant per-channel values make the exact coefficient arithmetic
//! checkable.

use soul_audio_core::{
    AnyDecoder, Deck, DecodeError, Decoder, Progress, Quality, SampleFormat, SeekableDecoder, Sink,
    SinkError, SliceSource, StreamInfo, channels::speaker, decoder_scratch_len, open,
};

/// Emits one constant `vals` frame per output frame until `frames` is exhausted, reporting
/// an optional WAVEFORMATEXTENSIBLE channel `mask` (as a real multichannel decoder would).
struct ConstMulti {
    ch: usize,
    mask: Option<u32>,
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
    fn channel_mask(&self) -> Option<u32> {
        self.mask
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

/// Play a constant `ch`-channel frame (optionally with a channel `mask`) through a stereo
/// deck and return a (Lo, Ro) sample from well past the fade-in.
fn play_downmix(ch: usize, mask: Option<u32>, vals: Vec<i32>) -> (i32, i32) {
    let mut deck: Deck<_, ConstMulti, 0> = Deck::new(
        StereoCapture {
            written: Vec::new(),
        },
        Quality::Hq,
    );
    deck.load(ConstMulti {
        ch,
        mask,
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
    // FL FR FC LFE BL BR — LFE is huge to prove it's dropped from the mix. No mask: the
    // count-based canonical-layout fallback handles the unambiguous 5.1.
    let (lo, ro) = play_downmix(6, None, vec![1000, 2000, 3000, 9_000_000, 5000, 6000]);
    let exp_lo = (1000.0 + C * 3000.0 + C * 5000.0).round() as i32;
    let exp_ro = (2000.0 + C * 3000.0 + C * 6000.0).round() as i32;
    assert!((lo - exp_lo).abs() <= 1, "5.1 Lo {lo} != {exp_lo}");
    assert!((ro - exp_ro).abs() <= 1, "5.1 Ro {ro} != {exp_ro}");
}

#[test]
fn downmix_3_0_folds_center() {
    let (lo, ro) = play_downmix(3, None, vec![1000, 2000, 3000]);
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
    let (lo, ro) = play_downmix(
        8,
        None,
        vec![1000, 2000, 3000, 9_000_000, 5000, 6000, 7000, 8000],
    );
    let exp_lo = (1000.0 + C * (3000.0 + 7000.0 + 5000.0)).round() as i32;
    let exp_ro = (2000.0 + C * (3000.0 + 8000.0 + 6000.0)).round() as i32;
    assert!((lo - exp_lo).abs() <= 1, "7.1 Lo {lo} != {exp_lo}");
    assert!((ro - exp_ro).abs() <= 1, "7.1 Ro {ro} != {exp_ro}");
}

#[test]
fn downmix_quad_with_mask_folds_back_pair() {
    // 4ch quad: FL FR BL BR (no center). Only resolvable with a mask (vs 3.1 FL FR FC LFE).
    let mask = speaker::FL | speaker::FR | speaker::BL | speaker::BR;
    let (lo, ro) = play_downmix(4, Some(mask), vec![1000, 2000, 5000, 6000]);
    let exp_lo = (1000.0 + C * 5000.0).round() as i32;
    let exp_ro = (2000.0 + C * 6000.0).round() as i32;
    assert!((lo - exp_lo).abs() <= 1, "quad Lo {lo} != {exp_lo}");
    assert!((ro - exp_ro).abs() <= 1, "quad Ro {ro} != {exp_ro}");
}

#[test]
fn downmix_5_0_with_mask_folds_center_and_back() {
    // 5ch 5.0: FL FR FC BL BR (no LFE). Resolvable with a mask (vs 4.1 FL FR LFE BL BR).
    let mask = speaker::FL | speaker::FR | speaker::FC | speaker::BL | speaker::BR;
    let (lo, ro) = play_downmix(5, Some(mask), vec![1000, 2000, 3000, 5000, 6000]);
    let exp_lo = (1000.0 + C * (3000.0 + 5000.0)).round() as i32;
    let exp_ro = (2000.0 + C * (3000.0 + 6000.0)).round() as i32;
    assert!((lo - exp_lo).abs() <= 1, "5.0 Lo {lo} != {exp_lo}");
    assert!((ro - exp_ro).abs() <= 1, "5.0 Ro {ro} != {exp_ro}");
}

#[test]
fn downmix_6_1_with_mask_folds_center_back_and_side() {
    // 7ch 6.1: FL FR FC LFE BC SL SR. Back-center folds to BOTH outputs at −3 dB; LFE dropped.
    let mask = speaker::FL
        | speaker::FR
        | speaker::FC
        | speaker::LFE
        | speaker::BC
        | speaker::SL
        | speaker::SR;
    // vals in ascending-bit (channel) order: FL FR FC LFE BC SL SR.
    let (lo, ro) = play_downmix(
        7,
        Some(mask),
        vec![1000, 2000, 3000, 9_000_000, 4000, 5000, 6000],
    );
    let exp_lo = (1000.0 + C * (3000.0 + 4000.0 + 5000.0)).round() as i32;
    let exp_ro = (2000.0 + C * (3000.0 + 4000.0 + 6000.0)).round() as i32;
    assert!((lo - exp_lo).abs() <= 1, "6.1 Lo {lo} != {exp_lo}");
    assert!((ro - exp_ro).abs() <= 1, "6.1 Ro {ro} != {exp_ro}");
}

#[test]
fn ambiguous_channel_counts_without_a_mask_are_rejected() {
    // 4 (quad vs 3.1), 5 (5.0 vs 4.1), 7 (6.1 variants) stay rejected with no layout mask.
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
                mask: None,
                vals: vec![100; ch],
                pos: 0,
                frames: 100,
            })
            .is_err(),
            "{ch}ch should stay rejected (ambiguous layout, no mask)"
        );
    }
}

#[test]
fn downmix_clips_gracefully_on_hot_in_phase_surround() {
    // Every channel near full scale and in phase → the summed Lo/Ro exceeds full scale and
    // must CLAMP (not wrap). No normalization is applied; the output clamp is the net.
    let fs = 8_000_000;
    let (lo, ro) = play_downmix(6, None, vec![fs, fs, fs, 0, fs, fs]);
    assert!(
        lo.abs() <= 8_388_607 && ro.abs() <= 8_388_607,
        "downmix must clamp, not wrap: lo={lo} ro={ro}"
    );
    assert!(
        lo > 7_000_000 && ro > 7_000_000,
        "clamped near full scale, not silent or wrapped: lo={lo} ro={ro}"
    );
}

// ===========================================================================
// Real-decoder path: a multichannel WAV decoded through the PUBLIC `open()` API
// (the exact probe + construct path the app runs), proving the channel mask
// travels from the container `fmt ` chunk through the real decoder to the deck
// fold. The tests above use a synthetic decoder; these exercise the whole chain.
// ===========================================================================

/// Build a 24-bit PCM WAV (24-bit ⇒ deck norm = 1, so the fold math stays exact). `mask`
/// `Some` writes a WAVE_FORMAT_EXTENSIBLE header carrying that `dwChannelMask`; `None`
/// writes a plain `fmt ` with no layout. Each of `frames` frames carries `vals` (one
/// 24-bit value per channel).
fn wav_24(ch: u16, mask: Option<u32>, vals: &[i32], frames: usize) -> Vec<u8> {
    assert_eq!(vals.len(), ch as usize);
    let block_align = ch * 3;
    let byte_rate = 48_000 * u32::from(block_align);
    let mut fmt = Vec::new();
    let tag: u16 = if mask.is_some() { 0xFFFE } else { 0x0001 };
    fmt.extend_from_slice(&tag.to_le_bytes());
    fmt.extend_from_slice(&ch.to_le_bytes());
    fmt.extend_from_slice(&48_000u32.to_le_bytes());
    fmt.extend_from_slice(&byte_rate.to_le_bytes());
    fmt.extend_from_slice(&block_align.to_le_bytes());
    fmt.extend_from_slice(&24u16.to_le_bytes()); // bits/sample
    if let Some(m) = mask {
        fmt.extend_from_slice(&22u16.to_le_bytes()); // cbSize
        fmt.extend_from_slice(&24u16.to_le_bytes()); // valid bits
        fmt.extend_from_slice(&m.to_le_bytes()); // dwChannelMask
        fmt.extend_from_slice(&1u16.to_le_bytes()); // SubFormat GUID[0..2] = PCM
        fmt.extend_from_slice(&[0u8; 14]); // rest of the GUID
    }

    let mut data = Vec::with_capacity(frames * ch as usize * 3);
    for _ in 0..frames {
        for &v in vals {
            data.push((v & 0xFF) as u8);
            data.push(((v >> 8) & 0xFF) as u8);
            data.push(((v >> 16) & 0xFF) as u8);
        }
    }

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(4 + 8 + fmt.len() as u32 + 8 + data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&fmt);
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&data);
    bytes
}

/// Decode real WAV `bytes` through the public `open()` path into a stereo deck, returning a
/// steady-state (Lo, Ro) sample well past the fade-in.
fn play_real_wav(bytes: &[u8]) -> (i32, i32) {
    let mut deck: Deck<StereoCapture, AnyDecoder<SliceSource, Vec<i32>>, 0> = Deck::new(
        StereoCapture {
            written: Vec::new(),
        },
        Quality::Hq,
    );
    let dec = open(SliceSource::new(bytes), vec![0i32; decoder_scratch_len()]).expect("open wav");
    deck.load(dec).expect("load");
    for _ in 0..100_000 {
        match deck.pump().unwrap() {
            Progress::Ended | Progress::Idle => break,
            _ => {}
        }
    }
    let out = &deck.sink().written;
    let i = 3_000; // steady state, clear of the fade-in
    assert!(out.len() > i * 2 + 1, "too little output: {}", out.len());
    (out[i * 2], out[i * 2 + 1])
}

#[test]
fn real_wav_5_1_folds_via_the_decoded_mask() {
    // The dwChannelMask in the WAV fmt chunk must reach the deck fold. LFE huge ⇒ dropped.
    let mask = speaker::FL | speaker::FR | speaker::FC | speaker::LFE | speaker::BL | speaker::BR;
    let wav = wav_24(
        6,
        Some(mask),
        &[1000, 2000, 3000, 4_000_000, 5000, 6000],
        4_800,
    );
    let (lo, ro) = play_real_wav(&wav);
    let exp_lo = (1000.0 + C * 3000.0 + C * 5000.0).round() as i32;
    let exp_ro = (2000.0 + C * 3000.0 + C * 6000.0).round() as i32;
    assert!((lo - exp_lo).abs() <= 2, "real 5.1 Lo {lo} != {exp_lo}");
    assert!((ro - exp_ro).abs() <= 2, "real 5.1 Ro {ro} != {exp_ro}");
}

#[test]
fn real_wav_quad_folds_via_the_decoded_mask() {
    // 4ch quad folds ONLY because the WAV carries a mask — the headline new capability,
    // proven through the real decoder rather than a synthetic one.
    let mask = speaker::FL | speaker::FR | speaker::BL | speaker::BR;
    let wav = wav_24(4, Some(mask), &[1000, 2000, 5000, 6000], 4_800);
    let (lo, ro) = play_real_wav(&wav);
    let exp_lo = (1000.0 + C * 5000.0).round() as i32;
    let exp_ro = (2000.0 + C * 6000.0).round() as i32;
    assert!((lo - exp_lo).abs() <= 2, "real quad Lo {lo} != {exp_lo}");
    assert!((ro - exp_ro).abs() <= 2, "real quad Ro {ro} != {exp_ro}");
}

#[test]
fn real_wav_plain_4ch_without_a_mask_stays_rejected() {
    // A plain (non-extensible) 4ch WAV carries no layout → still rejected through the real
    // decode path (the deck won't guess an ambiguous count).
    let wav = wav_24(4, None, &[1000, 2000, 5000, 6000], 100);
    let mut deck: Deck<StereoCapture, AnyDecoder<SliceSource, Vec<i32>>, 0> = Deck::new(
        StereoCapture {
            written: Vec::new(),
        },
        Quality::Hq,
    );
    let dec = open(SliceSource::new(&wav), vec![0i32; decoder_scratch_len()]).expect("open");
    assert!(deck.load(dec).is_err(), "plain 4ch WAV must stay rejected");
}
