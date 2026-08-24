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
