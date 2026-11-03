//! In-depth seek tests for WAV and DSD, mirroring the seek suites of reference
//! libraries (dr_wav, miniaudio #420/#78, FFmpeg `dsfdec`, foobar2000 SACD):
//! round-trip consistency (the primary oracle), position tracking, beyond-end
//! clamping without an extra frame, backward/repeated/exhaustive seeks, and —
//! for DSD — the **padding-never-emitted** and **no-read-past-chunk** leakage
//! guards plus **DoP marker-phase continuity** across a seek.
//!
//! Fixtures are counting patterns so every seeked read is verifiable with zero
//! tolerance; the from-start decode is the oracle for round-trip checks.

use soul_audio_core::formats::dff::DffDecoder;
use soul_audio_core::formats::dsf::DsfDecoder;
use soul_audio_core::formats::wav::WavDecoder;
use soul_audio_core::{DecodeError, Decoder, DopAdapter, DsdDecoder, DsdSeek, SliceSource};

// ===================== WAV =====================

/// 16-bit LE WAV with a counting ramp: sample[frame][ch] = frame*channels + ch.
/// Returns (bytes, expected interleaved i32).
fn ramp_wav(frames: usize, channels: u16) -> (Vec<u8>, Vec<i32>) {
    let mut data = Vec::new();
    let mut expected = Vec::new();
    for f in 0..frames {
        for c in 0..channels {
            let s = (f as i32 * i32::from(channels) + i32::from(c)) as i16;
            data.extend_from_slice(&s.to_le_bytes());
            expected.push(i32::from(s));
        }
    }
    (build_wav16(channels, &data), expected)
}

fn build_wav16(channels: u16, data: &[u8]) -> Vec<u8> {
    let block_align = channels * 2;
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36u32 + data.len() as u32).to_le_bytes());
    v.extend_from_slice(b"WAVE");
    v.extend_from_slice(b"fmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&channels.to_le_bytes());
    v.extend_from_slice(&44_100u32.to_le_bytes());
    v.extend_from_slice(&(44_100 * u32::from(block_align)).to_le_bytes());
    v.extend_from_slice(&block_align.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(data);
    v
}

fn wav_decode_all(bytes: &[u8]) -> Vec<i32> {
    let mut dec = WavDecoder::new(SliceSource::new(bytes)).unwrap();
    let mut out = Vec::new();
    let mut buf = [0i32; 64];
    loop {
        let n = dec.decode_into(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

/// Read `frames` interleaved frames after constructing/positioning — returns the
/// flat samples actually produced.
fn wav_read(dec: &mut WavDecoder<SliceSource<'_>>, frames: usize, channels: u16) -> Vec<i32> {
    let mut out = vec![0i32; frames * usize::from(channels)];
    let n = dec.decode_into(&mut out).unwrap();
    out.truncate(n);
    out
}

#[test]
fn wav_seek_round_trip_matches_from_start() {
    let (bytes, full) = ramp_wav(40, 2);
    for &n in &[0u64, 1, 17, 39] {
        for &m in &[1usize, 5, 100] {
            let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
            assert_eq!(dec.seek_to(n).unwrap(), n);
            let got = wav_read(&mut dec, m, 2);
            let start = (n as usize) * 2;
            let want = &full[start..(start + got.len())];
            assert_eq!(got, want, "seek({n}) read({m}) mismatch");
        }
    }
}

#[test]
fn wav_seek_exhaustive_per_frame() {
    let (bytes, full) = ramp_wav(64, 1); // mono
    for n in 0..64u64 {
        let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(dec.seek_to(n).unwrap(), n);
        let got = wav_read(&mut dec, 1, 1);
        assert_eq!(got, &full[n as usize..n as usize + 1], "off-by-one at {n}");
    }
}

#[test]
fn wav_seek_to_start_after_partial_matches_fresh() {
    let (bytes, full) = ramp_wav(30, 2);
    let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
    let _ = wav_read(&mut dec, 10, 2); // partial decode leaves internal state
    assert_eq!(dec.seek_to(0).unwrap(), 0);
    let mut rest = Vec::new();
    let mut buf = [0i32; 8];
    loop {
        let n = dec.decode_into(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        rest.extend_from_slice(&buf[..n]);
    }
    assert_eq!(rest, full); // no stale samples after seek(0)
}

#[test]
fn wav_backward_and_repeated_seeks() {
    let (bytes, full) = ramp_wav(50, 2);
    let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
    let _ = wav_read(&mut dec, 30, 2); // move forward first
    // backward
    assert_eq!(dec.seek_to(5).unwrap(), 5);
    assert_eq!(wav_read(&mut dec, 3, 2), &full[10..16]);
    // repeated seeks with no reads between: only the last one counts
    dec.seek_to(40).unwrap();
    dec.seek_to(12).unwrap();
    assert_eq!(dec.seek_to(20).unwrap(), 20);
    assert_eq!(wav_read(&mut dec, 2, 2), &full[40..44]);
}

#[test]
fn wav_seek_beyond_end_reads_zero() {
    // miniaudio #420: a post-seek read at end must return 0, not one extra frame.
    let (bytes, _) = ramp_wav(20, 2);
    let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
    assert_eq!(dec.seek_to(20).unwrap(), 20); // exactly end → clamp to 20
    assert_eq!(dec.position(), 20);
    let mut out = [0i32; 8];
    assert_eq!(dec.decode_into(&mut out).unwrap(), 0);
    // far beyond end → clamped, still no over-read
    assert_eq!(dec.seek_to(10_000).unwrap(), 20);
    assert_eq!(dec.decode_into(&mut out).unwrap(), 0);
}

#[test]
fn wav_seek_exact_end_vs_last_frame() {
    let (bytes, full) = ramp_wav(20, 2);
    let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
    assert_eq!(dec.seek_to(19).unwrap(), 19);
    let got = wav_read(&mut dec, 8, 2);
    assert_eq!(got, &full[38..40]); // exactly the last frame, one frame only
    assert_eq!(dec.decode_into(&mut [0i32; 8]).unwrap(), 0);
}

#[test]
fn wav_total_frames_after_seek_is_remaining() {
    let (bytes, _) = ramp_wav(37, 2);
    for n in [0u64, 1, 18, 36] {
        let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
        dec.seek_to(n).unwrap();
        let mut total = 0usize;
        let mut buf = [0i32; 8];
        loop {
            let k = dec.decode_into(&mut buf).unwrap();
            if k == 0 {
                break;
            }
            total += k;
        }
        assert_eq!(
            total,
            (37 - n as usize) * 2,
            "wrong remaining after seek({n})"
        );
    }
}

#[test]
fn wav_position_tracks_seek_and_reads() {
    let (bytes, _) = ramp_wav(30, 2);
    let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
    assert_eq!(dec.position(), 0);
    dec.seek_to(12).unwrap();
    assert_eq!(dec.position(), 12); // before any read
    let _ = wav_read(&mut dec, 5, 2);
    assert_eq!(dec.position(), 17); // after reading 5 frames
}

#[test]
fn wav_data_not_frame_multiple_ignores_trailing_bytes() {
    // dr_wav regression: data size = frames*blockAlign + r (r != 0). The trailing
    // partial frame must be ignored, and seeking must stay frame-aligned.
    let (mut bytes, full) = ramp_wav(10, 2); // blockAlign = 4
    // Append 3 junk bytes and grow the data chunk size to include them.
    let data_len_off = bytes.len();
    bytes.extend_from_slice(&[0xDE, 0xAD, 0xBE]); // 3 trailing bytes (< one frame)
    // patch the `data` chunk size (last 4-byte size before the audio) — it is at
    // offset 40 in our canonical layout (RIFF(12)+fmt(24)+"data"(4) = 40).
    let new_data_size = (data_len_off - 44 + 3) as u32; // original audio + 3
    bytes[40..44].copy_from_slice(&new_data_size.to_le_bytes());

    let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
    // Round-trip still holds and the trailing bytes never surface.
    assert_eq!(dec.seek_to(7).unwrap(), 7);
    assert_eq!(wav_read(&mut dec, 5, 2), &full[14..20]);
    // Decode-all yields exactly 10 frames, not 10.75.
    assert_eq!(wav_decode_all(&bytes).len(), 20);
}

// ===================== DSD: DSF (planar) =====================

/// Deterministic per-channel on-disk byte (audio region) — distinct per position.
fn dsf_byte(g: usize, c: usize) -> u8 {
    ((g * 31 + c * 7) & 0xFF) as u8
}

const DSF_BLOCK: usize = 4096;

/// Build a stereo DSF with `n_blocks` blocks/channel and `sample_count`
/// per-channel samples; audio region filled by `dsf_byte`, padding = 0x00.
/// `trailer` is appended after the data chunk (e.g. an ID3 blob).
fn build_dsf(sample_count: u64, n_blocks: usize, trailer: &[u8]) -> Vec<u8> {
    let channels = 2usize;
    let valid = sample_count.div_ceil(8) as usize; // valid bytes per channel
    let per_ch: Vec<Vec<u8>> = (0..channels)
        .map(|c| {
            (0..n_blocks * DSF_BLOCK)
                .map(|g| if g < valid { dsf_byte(g, c) } else { 0 })
                .collect()
        })
        .collect();
    // On-disk: interleaved by block-row.
    let mut audio = Vec::new();
    for r in 0..n_blocks {
        for ch in &per_ch {
            audio.extend_from_slice(&ch[r * DSF_BLOCK..(r + 1) * DSF_BLOCK]);
        }
    }

    let mut v = Vec::new();
    v.extend_from_slice(b"DSD ");
    v.extend_from_slice(&28u64.to_le_bytes());
    v.extend_from_slice(&0u64.to_le_bytes());
    v.extend_from_slice(&0u64.to_le_bytes());
    v.extend_from_slice(b"fmt ");
    v.extend_from_slice(&52u64.to_le_bytes());
    v.extend_from_slice(&1u32.to_le_bytes()); // version
    v.extend_from_slice(&0u32.to_le_bytes()); // format id
    v.extend_from_slice(&2u32.to_le_bytes()); // channel type
    v.extend_from_slice(&(channels as u32).to_le_bytes());
    v.extend_from_slice(&2_822_400u32.to_le_bytes());
    v.extend_from_slice(&1u32.to_le_bytes()); // bits
    v.extend_from_slice(&sample_count.to_le_bytes());
    v.extend_from_slice(&(DSF_BLOCK as u32).to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(audio.len() as u64 + 12).to_le_bytes());
    v.extend_from_slice(&audio);
    v.extend_from_slice(trailer);
    v
}

fn dsd_read_all(mut read: impl FnMut(&mut [u8]) -> Result<usize, DecodeError>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 128];
    loop {
        let n = read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

#[test]
fn dsf_seek_round_trip_byte_exact() {
    // 1 block, 800 per-channel samples (100 valid bytes/ch).
    let bytes = build_dsf(800, 1, &[]);
    let mut d0 = DsfDecoder::new(SliceSource::new(&bytes)).unwrap();
    let reference = dsd_read_all(|b| d0.read_dsd(b)); // normalized interleaved bytes

    for s in [0u64, 8, 80, 400, 792] {
        let mut d = DsfDecoder::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(d.seek_to_sample(s).unwrap(), s);
        let got = dsd_read_all(|b| d.read_dsd(b));
        let off = (s as usize / 8) * 2; // interleaved output byte offset
        assert_eq!(got, &reference[off..], "dsf seek_to_sample({s}) mismatch");
    }
}

#[test]
fn dsf_padding_never_emitted_after_seek_into_last_block() {
    // 2 blocks; sample_count = 1 block + 800 samples → last block 100 valid
    // bytes/ch + 3996 padding bytes/ch (0x00). Seek to the last block start.
    let spb = (DSF_BLOCK * 8) as u64; // 32768 samples/ch/block
    let bytes = build_dsf(spb + 800, 2, &[]);
    let mut d = DsfDecoder::new(SliceSource::new(&bytes)).unwrap();
    assert_eq!(d.seek_to_sample(spb).unwrap(), spb);
    let got = dsd_read_all(|b| d.read_dsd(b));
    // Only the 100 valid bytes/ch of the last block, interleaved → 200 bytes.
    assert_eq!(got.len(), 200);
    // None of it is the zero padding, and it matches the audio formula.
    for (i, &byte) in got.iter().enumerate() {
        let time = i / 2 + DSF_BLOCK; // global time-byte index into channel data
        let ch = i % 2;
        assert_eq!(byte, dsf_byte(time, ch).reverse_bits());
    }
}

#[test]
fn dsf_seek_clamped_to_sample_count() {
    let bytes = build_dsf(800, 1, &[]);
    let mut d = DsfDecoder::new(SliceSource::new(&bytes)).unwrap();
    assert_eq!(d.seek_to_sample(10_000).unwrap(), 800); // clamped
    assert_eq!(d.read_dsd(&mut [0u8; 16]).unwrap(), 0); // EOF, no over-read
}

#[test]
fn dsf_does_not_read_trailing_metadata() {
    // ID3 blob after the data chunk must never surface in the audio output.
    let id3 = b"ID3\x03\x00\x00\x00\x00\x00\x0aHELLO_TAG!";
    let bytes = build_dsf(800, 1, id3);
    let mut d = DsfDecoder::new(SliceSource::new(&bytes)).unwrap();
    d.seek_to_sample(792).unwrap(); // near the end
    let got = dsd_read_all(|b| d.read_dsd(b));
    assert!(!got.windows(3).any(|w| w == b"ID3"));
    // reversed 'I','D','3' shouldn't appear either
    for &b in b"ID3" {
        assert!(!got.contains(&b.reverse_bits()));
    }
}

// ===================== DSD: DFF (interleaved) =====================

/// Build a mono/stereo DFF: `audio` is the interleaved sound data. `trailer` is
/// appended after the sound-data chunk (inside FRM8), e.g. a COMT chunk.
fn build_dff(channels: u16, audio: &[u8], trailer: &[u8]) -> Vec<u8> {
    let mut prop = Vec::new();
    prop.extend_from_slice(b"SND ");
    prop.extend_from_slice(b"FS  ");
    prop.extend_from_slice(&4u64.to_be_bytes());
    prop.extend_from_slice(&2_822_400u32.to_be_bytes());
    let chnl_body = 2 + usize::from(channels) * 4;
    prop.extend_from_slice(b"CHNL");
    prop.extend_from_slice(&(chnl_body as u64).to_be_bytes());
    prop.extend_from_slice(&channels.to_be_bytes());
    for _ in 0..channels {
        prop.extend_from_slice(b"SLFT");
    }
    prop.extend_from_slice(b"CMPR");
    prop.extend_from_slice(&8u64.to_be_bytes());
    prop.extend_from_slice(b"DSD ");
    prop.extend_from_slice(&[3, b'r', b'a', b'w']);

    let mut body = Vec::new();
    body.extend_from_slice(b"PROP");
    body.extend_from_slice(&(prop.len() as u64).to_be_bytes());
    body.extend_from_slice(&prop);
    body.extend_from_slice(b"DSD ");
    body.extend_from_slice(&(audio.len() as u64).to_be_bytes());
    body.extend_from_slice(audio);
    body.extend_from_slice(trailer);

    let mut v = Vec::new();
    v.extend_from_slice(b"FRM8");
    v.extend_from_slice(&(4u64 + body.len() as u64).to_be_bytes());
    v.extend_from_slice(b"DSD ");
    v.extend_from_slice(&body);
    v
}

#[test]
fn dff_seek_round_trip_byte_exact() {
    // Stereo interleaved: [c0,c1] per group; 32 groups.
    let audio: Vec<u8> = (0..64u8).collect();
    let bytes = build_dff(2, &audio, &[]);
    let mut d0 = DffDecoder::new(SliceSource::new(&bytes)).unwrap();
    let reference = dsd_read_all(|b| d0.read_dsd(b));
    assert_eq!(reference, audio);

    for s in [0u64, 8, 64, 200] {
        let mut d = DffDecoder::new(SliceSource::new(&bytes)).unwrap();
        let landed = d.seek_to_sample(s).unwrap();
        let got = dsd_read_all(|b| d.read_dsd(b));
        let off = (landed as usize / 8) * 2;
        assert_eq!(got, &reference[off..], "dff seek({s}) mismatch");
    }
}

#[test]
fn dff_seek_clamped_and_no_read_past_sound_chunk() {
    // A COMT chunk after the sound data must never surface, and clamping works.
    let audio: Vec<u8> = (0..32u8).collect();
    let mut comt = Vec::new();
    comt.extend_from_slice(b"COMT");
    comt.extend_from_slice(&8u64.to_be_bytes());
    comt.extend_from_slice(b"COMMENT!");
    let bytes = build_dff(2, &audio, &comt);

    let mut d = DffDecoder::new(SliceSource::new(&bytes)).unwrap();
    // seek near end, read to EOF — must stop at the 32-byte sound chunk.
    d.seek_to_sample(120).unwrap(); // group 15 → byte 30
    let got = dsd_read_all(|b| d.read_dsd(b));
    assert_eq!(got, &[30, 31]);
    assert!(!got.windows(4).any(|w| w == b"COMT"));

    // clamp
    let mut d2 = DffDecoder::new(SliceSource::new(&bytes)).unwrap();
    assert_eq!(d2.seek_to_sample(10_000).unwrap(), 128); // 32 bytes/2ch*8
    assert_eq!(d2.read_dsd(&mut [0u8; 8]).unwrap(), 0);
}

// ===================== DoP marker phase across seeks =====================

fn dop_decode_all(adapter: &mut DopAdapter<DffDecoder<SliceSource<'_>>>) -> Vec<i32> {
    let mut out = Vec::new();
    let mut buf = [0i32; 32];
    loop {
        let n = adapter.decode_into(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

#[test]
fn dop_marker_phase_even_and_odd_after_seek() {
    // Mono DFF so one DoP word per frame; plenty of frames.
    let audio: Vec<u8> = (0..64u8).collect();
    let bytes = build_dff(1, &audio, &[]);

    // even DoP frame → 0x05
    let mut a = DopAdapter::new(DffDecoder::new(SliceSource::new(&bytes)).unwrap());
    assert_eq!(a.seek_to_frame(4).unwrap(), 4);
    let mut out = [0i32; 1];
    a.decode_into(&mut out).unwrap();
    assert_eq!((out[0] >> 16) & 0xFF, 0x05);

    // odd DoP frame → 0xFA
    let mut b = DopAdapter::new(DffDecoder::new(SliceSource::new(&bytes)).unwrap());
    assert_eq!(b.seek_to_frame(5).unwrap(), 5);
    b.decode_into(&mut out).unwrap();
    assert_eq!((out[0] >> 16) & 0xFF, 0xFA);
}

#[test]
fn dop_alternation_unbroken_after_seek() {
    let audio: Vec<u8> = (0..64u8).collect();
    let bytes = build_dff(1, &audio, &[]);
    let mut a = DopAdapter::new(DffDecoder::new(SliceSource::new(&bytes)).unwrap());
    a.seek_to_frame(3).unwrap();
    let mut words = [0i32; 8];
    let n = a.decode_into(&mut words).unwrap();
    for (i, &w) in words[..n].iter().enumerate() {
        let want = if (3 + i) % 2 == 0 { 0x05 } else { 0xFA };
        assert_eq!((w >> 16) & 0xFF, want, "phase drift at frame {}", 3 + i);
    }
    // never two identical markers in a row
    for w in words[..n].windows(2) {
        assert_ne!((w[0] >> 16) & 0xFF, (w[1] >> 16) & 0xFF);
    }
}

#[test]
fn dop_round_trip_via_adapter() {
    let audio: Vec<u8> = (0..64u8).collect();
    let bytes = build_dff(1, &audio, &[]);
    let mut base = DopAdapter::new(DffDecoder::new(SliceSource::new(&bytes)).unwrap());
    let reference = dop_decode_all(&mut base);

    for f in [0u64, 1, 4, 7] {
        let mut a = DopAdapter::new(DffDecoder::new(SliceSource::new(&bytes)).unwrap());
        assert_eq!(a.seek_to_frame(f).unwrap(), f);
        let got = dop_decode_all(&mut a);
        assert_eq!(got, &reference[f as usize..], "dop round-trip seek({f})");
    }
}
