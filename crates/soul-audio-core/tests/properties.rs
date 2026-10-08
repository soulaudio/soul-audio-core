//! Cross-codec **invariant** tests — properties every decoder must satisfy,
//! asserted uniformly rather than per-format. These catch whole bug classes the
//! fixed-buffer goldens can't:
//!
//! 1. **Buffer-size invariance** — decoding a stream one interleaved sample at a
//!    time, in odd chunks, or in one huge buffer must yield *identical* output.
//!    A regression in a decoder's `pcm_pos`/`out_pos` drain bookkeeping (dropping
//!    or duplicating samples across `decode_into` boundaries) is invisible to the
//!    4096-sample goldens but breaks this.
//! 2. **`total_frames()` == decoded count** — when a decoder declares a length it
//!    must equal what it actually produces.
//! 3. **Declared bit-range** — every emitted sample must fit the signed range of
//!    `stream_info().bits_per_sample()` (a mis-shift / sign-extension regression
//!    on a wide-bit path shows up here).
//!
//! All decoding runs on a 16 MiB stack — `AnyDecoder` and the Vorbis/SBR state
//! are large by value (no-alloc).

use soul_audio_core::formats::vorbis::VorbisDecoder;
use soul_audio_core::{Decoder, SeekableDecoder, SliceSource, open};

fn on_big_stack<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn_scoped(s, f)
            .unwrap()
            .join()
            .unwrap()
    })
}

/// Decoded PCM + the decoder's reported shape, via `open()`, draining into a
/// `buf_len`-sample buffer (exercises the partial-copy path at that granularity).
/// `channels`/`bits` are read *after* draining so mid-stream promotions (HE-AACv2
/// PS mono→stereo) are reflected.
struct Decoded {
    pcm: Vec<i32>,
    channels: u16,
    bits: u8,
    total_frames: Option<u64>,
}

fn decode_open(bytes: &'static [u8], buf_len: usize) -> Decoded {
    on_big_stack(move || {
        let mut d = open(SliceSource::new(bytes)).expect("open");
        let total_frames = d.total_frames();
        let mut pcm = Vec::new();
        let mut buf = vec![0i32; buf_len];
        loop {
            let n = d.decode_into(&mut buf).expect("decode");
            if n == 0 {
                break;
            }
            pcm.extend_from_slice(&buf[..n]);
        }
        let si = d.stream_info();
        Decoded {
            pcm,
            channels: si.channels,
            bits: si.bits_per_sample(),
            total_frames,
        }
    })
}

fn decode_vorbis(bytes: &'static [u8], buf_len: usize) -> Decoded {
    on_big_stack(move || {
        let mut d =
            VorbisDecoder::new(SliceSource::new(bytes), vec![0u8; 128 * 1024]).expect("vorbis");
        let total_frames = d.total_frames();
        let mut pcm = Vec::new();
        let mut buf = vec![0i32; buf_len];
        loop {
            let n = d.decode_into(&mut buf).expect("decode");
            if n == 0 {
                break;
            }
            pcm.extend_from_slice(&buf[..n]);
        }
        let si = d.stream_info();
        Decoded {
            pcm,
            channels: si.channels,
            bits: si.bits_per_sample(),
            total_frames,
        }
    })
}

type DecodeFn = fn(&'static [u8], usize) -> Decoded;

/// (label, bytes, decode fn, length-is-exact). Covers every codec; short
/// fixtures keep the one-sample-at-a-time pass fast.
///
/// `exact_len == false` for the lossy formats whose `total_frames()` is a
/// container/header *intended* length rather than the raw decoded count:
/// - **MP3** — the Xing/Info header's encoder-declared sample count;
/// - **AAC (MP4)** — the `mdhd`/edit-list media duration, which excludes the
///   encoder priming (front) + last-frame padding (back) the raw decode still
///   emits.
///
/// The decoder emits every whole coded frame (applying gapless delay/padding
/// trim is a separate feature), so these differ by up to one coded frame —
/// approximate by design, not a bug. Lossless + container-granule formats
/// (WAV/FLAC/ALAC/Opus/Vorbis) are exact.
fn fixtures() -> Vec<(&'static str, &'static [u8], DecodeFn, bool)> {
    vec![
        (
            "wav",
            include_bytes!("../../../testdata/tone.wav"),
            decode_open,
            true,
        ),
        (
            "flac",
            include_bytes!("../../../testdata/tone.flac"),
            decode_open,
            true,
        ),
        (
            "mp3",
            include_bytes!("../../../testdata/tone.mp3"),
            decode_open,
            false,
        ),
        (
            "opus",
            include_bytes!("../../../testdata/tone.opus"),
            decode_open,
            true,
        ),
        (
            "ogg-flac",
            include_bytes!("../testdata/flac-in-ogg.ogg"),
            decode_open,
            true,
        ),
        (
            "aac-lc",
            include_bytes!("../testdata/aac.m4a"),
            decode_open,
            false,
        ),
        (
            "he-aac",
            include_bytes!("../testdata/sbr.m4a"),
            decode_open,
            false,
        ),
        (
            "alac",
            include_bytes!("../testdata/alac.m4a"),
            decode_open,
            true,
        ),
        (
            "alac24",
            include_bytes!("../testdata/alac24.m4a"),
            decode_open,
            true,
        ),
        (
            "vorbis",
            include_bytes!("../testdata/tone-vorbis-mono.ogg"),
            decode_vorbis,
            true,
        ),
    ]
}

/// Buffer size must not change the decoded output. One sample, an odd chunk, the
/// golden size, and one huge buffer must all agree bit-for-bit.
#[test]
fn decode_is_buffer_size_invariant() {
    for (label, bytes, f, _exact) in fixtures() {
        let reference = f(bytes, 4096).pcm;
        assert!(!reference.is_empty(), "{label}: decoded nothing");
        for &buf_len in &[1usize, 3, 100, 1 << 16] {
            let got = f(bytes, buf_len).pcm;
            assert_eq!(
                got.len(),
                reference.len(),
                "{label}: length differs at buf_len={buf_len}"
            );
            assert_eq!(
                got, reference,
                "{label}: output differs at buf_len={buf_len} (drain bookkeeping bug)"
            );
        }
    }
}

/// A declared `total_frames()` must equal the actual per-channel frame count
/// (exactly for container/lossless formats; within one MPEG frame for MP3's
/// approximate Xing-header length — see `fixtures()`).
#[test]
fn total_frames_matches_decoded_count() {
    for (label, bytes, f, exact) in fixtures() {
        let d = f(bytes, 4096);
        if let Some(total) = d.total_frames {
            let frames = (d.pcm.len() / d.channels.max(1) as usize) as u64;
            if exact {
                assert_eq!(
                    frames, total,
                    "{label}: total_frames()={total} but decoded {frames} frames"
                );
            } else {
                // Approximate length: must agree to within one coded frame
                // (MP3 1152 / AAC-LC 1024 / HE-AAC 2048 output samples) — the
                // last-frame padding + encoder priming the raw decode keeps.
                let diff = frames.abs_diff(total);
                assert!(
                    diff <= 2048,
                    "{label}: total_frames()={total} vs decoded {frames} differ by {diff} (> 1 coded frame — not just padding)"
                );
            }
        }
    }
}

/// Every emitted sample must fit the signed range of the declared bit depth.
#[test]
fn samples_fit_declared_bit_depth() {
    for (label, bytes, f, _exact) in fixtures() {
        let d = f(bytes, 4096);
        let bits = u32::from(d.bits);
        assert!(
            (1..=32).contains(&bits),
            "{label}: implausible bit depth {bits}"
        );
        let lo = if bits == 32 {
            i32::MIN as i64
        } else {
            -(1i64 << (bits - 1))
        };
        let hi = if bits == 32 {
            i32::MAX as i64
        } else {
            (1i64 << (bits - 1)) - 1
        };
        for &s in &d.pcm {
            let s = i64::from(s);
            assert!(
                s >= lo && s <= hi,
                "{label}: sample {s} out of {bits}-bit range [{lo}, {hi}]"
            );
        }
    }
}
