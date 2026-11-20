//! WavPack multichannel **bit-exact** decode, CI-running (committed goldens, no ffmpeg at test
//! time).
//!
//! WavPack is lossless, so the strongest conformance check is bit-exact PCM against an
//! independent reference. The committed fixtures (a distinct tone per channel) must decode to
//! ffmpeg's own decode of the SAME stream. Unlike ALAC, WavPack carries an explicit
//! WAVEFORMATEXTENSIBLE channel mask, so its stream order already matches our decoder's
//! (`ChannelLayout::from_mask`) and ffmpeg's PCM output order; no `pan` reorder is needed and
//! every sample must match position-for-position.
//!
//! This also proves the multi-block frame assembly: a multichannel WavPack frame is a run of
//! ≤2ch sub-blocks (INITIAL_BLOCK…FINAL_BLOCK) interleaved at the right channel offsets — a
//! wrong offset or a missed sub-block shows up as a sample mismatch. The 5.0 fixture adds the
//! odd-channel case: its last sub-block is **mono** (the BR channel), exercising the 1-channel
//! interleave branch the all-stereo 5.1 fixture doesn't.
//!
//! Not under `embedded-profile`: that profile caps WavPack at mono/stereo, so a >2-channel
//! fixture is intentionally `Unsupported` there.
#![cfg(not(feature = "embedded-profile"))]

use soul_audio_core::channels::ChannelLayout;
use soul_audio_core::{Decoder, SliceSource, decoder_scratch_len, open, open_seekable};

const FIXTURE_51: &[u8] = include_bytes!("../testdata/tone-wv-5.1.wv");
const GOLDEN_51: &[u8] = include_bytes!("../testdata/tone-wv-5.1.ref.pcm");
const FIXTURE_50: &[u8] = include_bytes!("../testdata/tone-wv-5.0.wv");
const GOLDEN_50: &[u8] = include_bytes!("../testdata/tone-wv-5.0.ref.pcm");

/// Decode `bytes` through the public `open()` path to interleaved `i32` PCM.
fn decode_all(bytes: &[u8]) -> (u16, Vec<i32>) {
    let mut dec =
        open(SliceSource::new(bytes), vec![0i32; decoder_scratch_len()]).expect("open fixture");
    let channels = dec.stream_info().channels;
    let mut pcm = Vec::new();
    let mut buf = vec![0i32; 4096 * usize::from(channels.max(1))];
    for _ in 0..100_000 {
        let n = dec.decode_into(&mut buf).expect("decode");
        if n == 0 {
            break;
        }
        pcm.extend_from_slice(&buf[..n]);
    }
    (channels, pcm)
}

/// Assert a multichannel WavPack fixture decodes bit-exact to its golden (interleaved 16-bit
/// little-endian, in WAVEFORMATEXTENSIBLE order — exactly our decoder's order for the mask).
fn assert_bit_exact(fixture: &[u8], golden_bytes: &[u8], channels: u16) {
    let (ch, ours) = decode_all(fixture);
    assert_eq!(ch, channels, "unexpected channel count");

    let golden: Vec<i32> = golden_bytes
        .chunks_exact(2)
        .map(|b| i32::from(i16::from_le_bytes([b[0], b[1]])))
        .collect();

    let n = usize::from(channels);
    assert_eq!(
        ours.len(),
        golden.len(),
        "sample count mismatch: decoded {} vs reference {} ({} vs {} frames)",
        ours.len(),
        golden.len(),
        ours.len() / n,
        golden.len() / n,
    );

    // Lossless + matching channel order ⇒ every sample identical to the reference decode.
    if let Some((i, (&a, &b))) = ours
        .iter()
        .zip(&golden)
        .enumerate()
        .find(|(_, (a, b))| a != b)
    {
        panic!(
            "decode differs from the reference at sample {i} (frame {}, channel {}): \
             decoded {a} != reference {b}",
            i / n,
            i % n
        );
    }
}

/// 5.1 (three stereo sub-blocks). Mask 0x3f = FL FR FC LFE BL BR.
#[test]
fn wavpack_5_1_decodes_bit_exact_vs_reference() {
    assert_bit_exact(FIXTURE_51, GOLDEN_51, 6);
}

/// 5.0 (two stereo sub-blocks + a trailing **mono** sub-block). Mask 0x37 = FL FR FC BL BR.
/// Exercises the odd-channel count and the 1-channel interleave branch.
#[test]
fn wavpack_5_0_decodes_bit_exact_vs_reference() {
    assert_bit_exact(FIXTURE_50, GOLDEN_50, 5);
}

/// Seeking within a multichannel frame must land bit-exact. `seek_to` re-derives the
/// within-frame offset and adds the frame's base in the output region (`frame_off`); a wrong
/// base would read stale samples from the per-block staging region instead.
#[test]
fn wavpack_5_1_seek_within_frame_lands_bit_exact() {
    let golden: Vec<i32> = GOLDEN_51
        .chunks_exact(2)
        .map(|b| i32::from(i16::from_le_bytes([b[0], b[1]])))
        .collect();
    let ch = 6usize;
    let mut dec = open_seekable(
        SliceSource::new(FIXTURE_51),
        vec![0i32; decoder_scratch_len()],
    )
    .expect("open_seekable fixture");

    for &target in &[0usize, 1000, 4000, 8000] {
        let landed = dec.seek_to(target as u64).expect("seek");
        assert_eq!(landed, target as u64, "seek({target}) landed off-target");
        let mut out = vec![0i32; 8 * ch];
        let n = dec.decode_into(&mut out).expect("decode after seek");
        let frames = n / ch;
        assert!(frames > 0, "no samples after seek({target})");
        assert_eq!(
            &out[..frames * ch],
            &golden[target * ch..(target + frames) * ch],
            "seek to frame {target} did not land bit-exact"
        );
    }
}

/// The layout the deck folds through: WavPack's `ID_CHANNEL_INFO` mask, reported as a
/// WAVEFORMATEXTENSIBLE `ChannelLayout`. A wrong layout here mis-routes the 5.1→stereo
/// downmix even when the decode is bit-exact.
#[test]
fn wavpack_5_1_reports_waveformatextensible_layout() {
    let dec = open(
        SliceSource::new(FIXTURE_51),
        vec![0i32; decoder_scratch_len()],
    )
    .expect("open fixture");
    let layout = dec
        .channel_layout()
        .expect("a multichannel stream reports a channel layout");
    // 5.1 mask 0x3f = FL FR FC LFE BL BR, in ascending-bit (WAVEFORMATEXTENSIBLE) order.
    assert_eq!(
        layout,
        ChannelLayout::from_mask(0x3f),
        "WavPack 5.1 should report the WAVEFORMATEXTENSIBLE 5.1 layout"
    );
}
