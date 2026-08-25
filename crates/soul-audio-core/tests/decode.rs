//! Integration tests exercising the public decode API end-to-end.

use soul_audio_core::{DecodeError, Decoder, SliceSource, open};

/// A minimal valid 16-bit mono WAV carrying `samples`.
fn wav16(samples: &[i16]) -> Vec<u8> {
    let mut data = Vec::new();
    for s in samples {
        data.extend_from_slice(&s.to_le_bytes());
    }
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36u32 + data.len() as u32).to_le_bytes());
    v.extend_from_slice(b"WAVE");
    v.extend_from_slice(b"fmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&2u16.to_le_bytes()); // stereo
    v.extend_from_slice(&44_100u32.to_le_bytes());
    v.extend_from_slice(&176_400u32.to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(&data);
    v
}

#[test]
fn open_probes_and_decodes_wav_end_to_end() {
    // Interleaved stereo: L0,R0, L1,R1 …
    let samples = [10i16, -10, 20, -20, 30, -30];
    let bytes = wav16(&samples);
    let mut dec = open(SliceSource::new(&bytes)).expect("probe + construct");
    assert_eq!(dec.stream_info().channels, 2);
    assert_eq!(dec.stream_info().sample_rate, 44_100);

    let mut out = [0i32; 16];
    let n = dec.decode_into(&mut out).unwrap();
    assert_eq!(n, samples.len());
    for (got, want) in out[..n].iter().zip(samples.iter()) {
        assert_eq!(*got, i32::from(*want));
    }
    assert_eq!(dec.decode_into(&mut out).unwrap(), 0); // end of stream
}

#[test]
fn open_rejects_unknown_container() {
    let result = open(SliceSource::new(b"XXXXYYYY"));
    assert_eq!(result.err(), Some(DecodeError::Unsupported));
}

#[test]
fn open_decodes_flac_in_ogg_matching_ffmpeg() {
    // Real ffmpeg-encoded FLAC-in-Ogg; reference = ffmpeg's own decode.
    let ogg = include_bytes!("../testdata/flac-in-ogg.ogg");
    let mut dec = open(SliceSource::new(ogg)).expect("probe + construct Ogg-FLAC");
    assert_eq!(dec.stream_info().sample_rate, 44_100);
    assert_eq!(dec.stream_info().channels, 1);
    let mut out = Vec::new();
    let mut buf = [0i32; 4096];
    loop {
        let n = dec.decode_into(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    assert_eq!(out.len(), 13230);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &s in &out {
        h ^= u64::from(s as u32);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    assert_eq!(h, 0xc9ba_54f0_06b2_9f75);
}

#[test]
fn open_surfaces_flac_stream_info() {
    // fLaC + STREAMINFO (44.1 kHz stereo 16-bit) with no audio frames.
    let mut si = [0u8; 34];
    let v: u64 = (44_100u64 << 44) | (1u64 << 41) | (15u64 << 36);
    si[10..18].copy_from_slice(&v.to_be_bytes());
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"fLaC");
    bytes.push(0x80);
    bytes.extend_from_slice(&[0, 0, 34]);
    bytes.extend_from_slice(&si);

    let mut dec = open(SliceSource::new(&bytes)).expect("probe + construct");
    assert_eq!(dec.stream_info().sample_rate, 44_100);
    assert_eq!(dec.stream_info().channels, 2);
    // No frames follow the metadata, so the FLAC codec reports a clean end of
    // stream (frame-level decode is covered by the unit tests in `formats::flac`).
    assert_eq!(dec.decode_into(&mut [0i32; 4]), Ok(0));
}

/// Decode a full stream through the public `open` API and return interleaved PCM.
fn decode_all(bytes: &[u8]) -> Vec<i32> {
    let mut dec = open(SliceSource::new(bytes)).expect("probe + construct");
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

fn fnv1a(samples: &[i32]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &s in samples {
        h ^= u64::from(s as u32);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// MPEG-1/2 **Layer II** golden vectors. References are decoded by kjmp2 (Martin
/// Fiedler, the decoder this port follows); output is bit-exact against it. The
/// set spans allocation tables A/B/C/D, mono + stereo, and 32/44.1/48 kHz.
#[test]
fn open_decodes_layer2_matching_kjmp2() {
    struct Case {
        data: &'static [u8],
        rate: u32,
        ch: u16,
        samples: usize,
        fnv: u64,
    }
    let cases = [
        // mono 44.1 kHz 128 kbps → table B
        Case {
            data: include_bytes!("../testdata/l2_m44_128.mp2"),
            rate: 44_100,
            ch: 1,
            samples: 13_824,
            fnv: 0xda7b_4ad1_b2ea_fcf1,
        },
        // stereo 44.1 kHz 128 kbps → table A
        Case {
            data: include_bytes!("../testdata/l2_s44_128.mp2"),
            rate: 44_100,
            ch: 2,
            samples: 27_648,
            fnv: 0x5f56_1272_433b_0ecd,
        },
        // mono 44.1 kHz 48 kbps → table C (sblimit 8)
        Case {
            data: include_bytes!("../testdata/l2_m44_48.mp2"),
            rate: 44_100,
            ch: 1,
            samples: 13_824,
            fnv: 0x1175_5393_00f7_6d13,
        },
        // mono 32 kHz 48 kbps → table D (sblimit 12)
        Case {
            data: include_bytes!("../testdata/l2_m32_48.mp2"),
            rate: 32_000,
            ch: 1,
            samples: 10_368,
            fnv: 0x7096_2bc4_c107_79fe,
        },
        // stereo 48 kHz 256 kbps → table A
        Case {
            data: include_bytes!("../testdata/l2_s48_256.mp2"),
            rate: 48_000,
            ch: 2,
            samples: 29_952,
            fnv: 0x9f6c_9a7f_4b09_77aa,
        },
    ];
    for c in cases {
        let dec = open(SliceSource::new(c.data)).expect("probe + construct MP2");
        assert_eq!(dec.stream_info().sample_rate, c.rate);
        assert_eq!(dec.stream_info().channels, c.ch);
        let pcm = decode_all(c.data);
        assert_eq!(pcm.len(), c.samples, "sample count");
        assert_eq!(fnv1a(&pcm), c.fnv, "bit-exact vs kjmp2");
    }
}

/// MPEG-1 **Layer I** decode. Reference is a real ISO Layer I conformance vector
/// (`mp1-sample.mp1` from the FFmpeg samples server: 32 kHz stereo, 48 audio
/// frames after a leading warm-up frame). Two checks:
///
/// 1. A regression hash locking this decoder's exact (deterministic) output.
/// 2. A tolerance check against ffmpeg's own decode over a 16-frame warm window:
///    every sample is within 128 of ffmpeg. (Bit-exact is impossible — ffmpeg
///    uses a higher-precision fixed-point filterbank; the whole-signal max
///    deviation measured 83.) The window skips the first output frame, where
///    ffmpeg's decoder-delay convention discards a frame that this decoder — like
///    kjmp2 for Layer II — emits, so the two are offset by exactly one frame.
#[test]
fn open_decodes_layer1_conformance_vector() {
    let mp1 = include_bytes!("../testdata/layer1-32k-stereo.mp1");
    let dec = open(SliceSource::new(mp1)).expect("probe + construct MP1");
    assert_eq!(dec.stream_info().sample_rate, 32_000);
    assert_eq!(dec.stream_info().channels, 2);

    let pcm = decode_all(mp1);
    assert_eq!(pcm.len(), 49 * 384 * 2, "49 frames × 384 samples × stereo");
    assert_eq!(fnv1a(&pcm), 0xf57c_6f7c_2fec_67f7, "regression hash");

    // ffmpeg reference window = its output frames 1..=16 (interleaved i16). Our
    // output leads ffmpeg by one frame, so ffmpeg frame f aligns to our f+1:
    // window[k] (= ffmpeg sample 768+k) matches our sample 1536+k.
    let ff: &[u8] = include_bytes!("../testdata/layer1-32k-stereo.ffref16.pcm");
    let window: Vec<i32> = ff
        .chunks_exact(2)
        .map(|c| i32::from(i16::from_le_bytes([c[0], c[1]])))
        .collect();
    let mut maxdiff = 0i32;
    for (k, &want) in window.iter().enumerate() {
        maxdiff = maxdiff.max((pcm[1536 + k] - want).abs());
    }
    assert!(
        maxdiff <= 128,
        "within tolerance of ffmpeg (maxdiff={maxdiff})"
    );
}

#[test]
fn open_surfaces_mp3_stream_info() {
    // MPEG-1 Layer III, 128 kbps, 44.1 kHz, stereo. A synthetic all-zero frame
    // body carries no spectral energy, so the codec decodes it to silence.
    // (Bit-exact decode of real LAME clips is covered by the golden-vector tests
    // in `formats::mp3`.)
    let b1 = 0xE0 | (0b11 << 3) | (0b01 << 1);
    let b2 = 9u8 << 4; // bitrate idx 9 (128k), srate idx 0 (44100), no padding
    let mut bytes = vec![0xFFu8, b1, b2, 0];
    bytes.resize(1024, 0);

    let mut dec = open(SliceSource::new(&bytes)).expect("probe + construct");
    assert_eq!(dec.stream_info().sample_rate, 44_100);
    assert_eq!(dec.stream_info().channels, 2);
    let mut out = [7i32; 8];
    let n = dec.decode_into(&mut out).unwrap();
    assert_eq!(n, 8);
    assert!(out.iter().all(|&s| s == 0), "silent frame decodes to zeros");
}
