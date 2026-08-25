//! Runtime format dispatch: [`probe`](crate::probe) → [`AnyDecoder`] → uniform
//! [`Decoder`].
//!
//! `no_std` + no-alloc rules out `Box<dyn Decoder>`, so [`AnyDecoder`] is the
//! stand-in: an enum over the concrete PCM decoders that delegates the trait to
//! whichever variant is active. DSD containers are *identified* by `probe` but
//! are **not** routed here — they require an explicit
//! [`DsdMode`](crate::model::DsdMode) (see [`crate::dsd`]).

use crate::decoder::{DecodeError, Decoder};
use crate::demux::ogg::OggReader;
use crate::formats::{flac::FlacDecoder, mp3::Mp3Decoder, wav::WavDecoder};
use crate::io::BufSource;
use crate::model::{Format, StreamInfo};
use crate::probe::probe;

/// A PCM decoder whose concrete format was chosen at runtime by [`open`].
///
/// The `Flac` variant embeds an inline, no-alloc decoded-block buffer, so it is
/// far larger than the other variants. Clippy's suggested remedy (boxing) is
/// impossible here — this crate is `#![no_std]` with no allocator — so the size
/// difference is accepted deliberately.
#[allow(clippy::large_enum_variant)]
pub enum AnyDecoder<S> {
    /// RIFF/WAVE.
    Wav(WavDecoder<S>),
    /// Native FLAC.
    Flac(FlacDecoder<S>),
    /// MPEG Layer III.
    Mp3(Mp3Decoder<S>),
    /// FLAC in an Ogg container.
    OggFlac(FlacDecoder<OggReader<S>>),
}

impl<S: crate::io::Source> Decoder for AnyDecoder<S> {
    fn stream_info(&self) -> StreamInfo {
        match self {
            AnyDecoder::Wav(d) => d.stream_info(),
            AnyDecoder::Flac(d) => d.stream_info(),
            AnyDecoder::Mp3(d) => d.stream_info(),
            AnyDecoder::OggFlac(d) => d.stream_info(),
        }
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        match self {
            AnyDecoder::Wav(d) => d.decode_into(out),
            AnyDecoder::Flac(d) => d.decode_into(out),
            AnyDecoder::Mp3(d) => d.decode_into(out),
            AnyDecoder::OggFlac(d) => d.decode_into(out),
        }
    }
}

/// Probe `src`, then construct the matching PCM decoder.
///
/// # Errors
/// - [`DecodeError::Unsupported`] if the container is unknown, or is a DSD
///   container (which needs an explicit mode — see [`crate::dsd`]).
/// - Whatever the chosen decoder's `new` reports.
pub fn open<S: BufSource>(mut src: S) -> Result<AnyDecoder<S>, DecodeError> {
    match probe(&mut src)? {
        Format::Wav => WavDecoder::new(src).map(AnyDecoder::Wav),
        Format::Flac => FlacDecoder::new(src).map(AnyDecoder::Flac),
        Format::Mp3 => Mp3Decoder::new(src).map(AnyDecoder::Mp3),
        Format::OggFlac => FlacDecoder::new(OggReader::new(src)?).map(AnyDecoder::OggFlac),
        // DSD containers need an explicit DsdMode (native / DoP / decimate);
        // they are not routed through the PCM `open`.
        Format::Dsf | Format::Dff => Err(DecodeError::Unsupported),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceSource;

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
        v.extend_from_slice(&1u16.to_le_bytes()); // mono
        v.extend_from_slice(&8000u32.to_le_bytes());
        v.extend_from_slice(&16000u32.to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(&data);
        v
    }

    #[test]
    fn open_routes_and_decodes_wav() {
        let wav = wav16(&[100, -100, 32767]);
        let mut dec = open(SliceSource::new(&wav)).expect("probe + construct");
        assert!(matches!(dec, AnyDecoder::Wav(_))); // routed to the WAV variant
        let mut out = [0i32; 8];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(&out[..n], &[100, -100, 32767]); // real PCM, delegated through AnyDecoder
    }

    /// A minimal valid FLAC: signature + a STREAMINFO block (44.1 kHz stereo 16-bit).
    fn flac_min() -> Vec<u8> {
        let mut si = [0u8; 34];
        let v: u64 = (44_100u64 << 44) | (1u64 << 41) | (15u64 << 36); // ch-1=1, bps-1=15
        si[10..18].copy_from_slice(&v.to_be_bytes());
        let mut b = Vec::new();
        b.extend_from_slice(b"fLaC");
        b.push(0x80); // last block + STREAMINFO
        b.extend_from_slice(&[0, 0, 34]);
        b.extend_from_slice(&si);
        b
    }

    #[test]
    fn open_routes_flac_with_real_stream_info() {
        let bytes = flac_min();
        let dec = open(SliceSource::new(&bytes)).expect("probe + construct");
        assert_eq!(dec.stream_info().sample_rate, 44_100);
        assert_eq!(dec.stream_info().channels, 2);
        assert!(matches!(dec, AnyDecoder::Flac(_)));
    }

    #[test]
    fn open_rejects_dsd_containers_without_a_mode() {
        // DSF is a valid container but needs an explicit DsdMode, so `open` (the
        // PCM path) declines it.
        assert_eq!(
            open(SliceSource::new(b"DSD \0\0\0\0")).err(),
            Some(DecodeError::Unsupported)
        );
    }

    #[test]
    fn open_rejects_unknown() {
        assert_eq!(
            open(SliceSource::new(b"XXXXYYYY")).err(),
            Some(DecodeError::Unsupported)
        );
    }
}
