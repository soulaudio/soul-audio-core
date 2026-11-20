//! ALAC multichannel **bit-exact** decode, CI-running (committed golden, no ffmpeg at test
//! time).
//!
//! ALAC is lossless, so the strongest conformance check is bit-exact PCM against an
//! independent reference. The committed 5.1 fixture `tone-mc-5.1.m4a` (a distinct tone per
//! channel) must decode to `tone-mc-5.1.ref.pcm` — ffmpeg's own decode of the SAME stream,
//! reordered (via ffmpeg `pan`) into our decoder's channel order (ALAC element order, centre
//! first). Every sample must match. The ffmpeg-gated `downmix_conformance` suite proves the
//! same thing on demand with a wider matrix; this guards the multichannel ALAC path on every
//! CI run.
//!
//! Not under `embedded-profile`: that profile caps ALAC at mono/stereo
//! (`alac::MAX_CHANNELS = 2`), so a 6-channel fixture is intentionally `Unsupported` there.
#![cfg(not(feature = "embedded-profile"))]

use soul_audio_core::{Decoder, SliceSource, decoder_scratch_len, open};

const FIXTURE: &[u8] = include_bytes!("../testdata/tone-mc-5.1.m4a");
const GOLDEN: &[u8] = include_bytes!("../testdata/tone-mc-5.1.ref.pcm");

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

#[test]
fn alac_5_1_decodes_bit_exact_vs_reference() {
    let (channels, ours) = decode_all(FIXTURE);
    assert_eq!(channels, 6, "expected a 6-channel (5.1) ALAC stream");

    // Golden: interleaved 16-bit little-endian, 6 channels, in our (ALAC element) order.
    let golden: Vec<i32> = GOLDEN
        .chunks_exact(2)
        .map(|b| i32::from(i16::from_le_bytes([b[0], b[1]])))
        .collect();

    assert_eq!(
        ours.len(),
        golden.len(),
        "sample count mismatch: decoded {} vs reference {} ({} vs {} frames)",
        ours.len(),
        golden.len(),
        ours.len() / 6,
        golden.len() / 6,
    );

    // Lossless + matching channel order ⇒ every sample identical to the reference decode.
    if let Some((i, (&a, &b))) = ours
        .iter()
        .zip(&golden)
        .enumerate()
        .find(|(_, (a, b))| a != b)
    {
        panic!(
            "ALAC 5.1 decode differs from the reference at sample {i} (frame {}, channel {}): \
             decoded {a} != reference {b}",
            i / 6,
            i % 6
        );
    }
}
