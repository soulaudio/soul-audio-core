//! WAV decoder = [`WavDemuxer`](crate::demux::WavDemuxer) (container) + PCM
//! widening (codec). Streams interleaved `i32` PCM.

use crate::decoder::{DecodeError, Decoder};
use crate::demux::{Demuxer, WavDemuxer};
use crate::io::{SeekSource, Source};
use crate::model::{SampleFormat, StreamInfo};

/// WAV decoder over a byte [`Source`]. Integer PCM (8/16/24/32-bit) is supported;
/// float PCM is a later slice.
pub struct WavDecoder<S> {
    demux: WavDemuxer<S>,
    bits: u8,
    channels: u16,
    sample_pos: u64, // interleaved samples produced since the start
}

impl<S: Source> WavDecoder<S> {
    /// Parse the WAV header and prepare to stream PCM.
    ///
    /// # Errors
    /// Propagates [`WavDemuxer::new`] errors; [`DecodeError::Unsupported`] for a
    /// non-integer sample format (float is a later slice).
    pub fn new(src: S) -> Result<Self, DecodeError> {
        let demux = WavDemuxer::new(src)?;
        let info = demux.stream_info();
        let bits = match info.format {
            SampleFormat::Int { bits } => bits,
            // Float decode is a later slice; DSD never reaches this codec.
            _ => return Err(DecodeError::Unsupported),
        };
        if bits == 0 || bits % 8 != 0 || bits > 32 || info.channels == 0 {
            return Err(DecodeError::Unsupported);
        }
        Ok(Self {
            demux,
            bits,
            channels: info.channels,
            sample_pos: 0,
        })
    }

    /// Current playback position, in interleaved frames from the start.
    #[must_use]
    pub fn position(&self) -> u64 {
        self.sample_pos / u64::from(self.channels)
    }
}

impl<S: SeekSource> WavDecoder<S> {
    /// Seek to interleaved `frame`. Returns the frame landed on.
    ///
    /// # Errors
    /// [`DecodeError::Io`] if the seek fails.
    pub fn seek_to(&mut self, frame: u64) -> Result<u64, DecodeError> {
        let landed = self.demux.seek_to(frame)?;
        self.sample_pos = landed * u64::from(self.channels);
        Ok(landed)
    }
}

impl<S: Source> Decoder for WavDecoder<S> {
    fn stream_info(&self) -> StreamInfo {
        self.demux.stream_info()
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        let bps = usize::from(self.bits) / 8;
        // Batch buffer: 768 is divisible by 1, 2, 3, and 4 (all sample widths).
        let mut buf = [0u8; 768];
        let mut produced = 0;
        while produced < out.len() {
            let cap = (out.len() - produced).min(buf.len() / bps) * bps;
            if cap == 0 {
                break;
            }
            let n = self.demux.read_data(&mut buf[..cap])?;
            if n == 0 {
                break;
            }
            let samples = n / bps;
            for i in 0..samples {
                out[produced + i] = widen(&buf[i * bps..i * bps + bps], self.bits);
            }
            produced += samples;
            if n < cap {
                break; // end of data (or a short final read)
            }
        }
        self.sample_pos += produced as u64;
        Ok(produced)
    }
}

/// Widen one little-endian PCM sample to `i32`, preserving true depth (values are
/// not rescaled). 8-bit PCM is unsigned; 16/24/32-bit are signed two's-complement.
fn widen(bytes: &[u8], bits: u8) -> i32 {
    match bits {
        8 => i32::from(bytes[0]) - 128,
        16 => i32::from(i16::from_le_bytes([bytes[0], bytes[1]])),
        24 => {
            let raw =
                u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16);
            ((raw << 8) as i32) >> 8 // sign-extend from bit 23
        }
        _ => i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceSource;

    fn wav(bits: u16, data: &[u8]) -> Vec<u8> {
        let channels = 1u16;
        let block_align = channels * (bits / 8);
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
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(data);
        v
    }

    #[test]
    fn decodes_16bit_pcm_including_negatives() {
        // samples: 0, 1, -1, 32767, -32768
        let mut data = Vec::new();
        for s in [0i16, 1, -1, 32767, -32768] {
            data.extend_from_slice(&s.to_le_bytes());
        }
        let bytes = wav(16, &data);
        let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 8];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&out[..5], &[0, 1, -1, 32767, -32768]);
        assert_eq!(dec.decode_into(&mut out).unwrap(), 0); // EOF
    }

    #[test]
    fn decodes_24bit_pcm_sign_extended() {
        // 0x000000 = 0, 0x7FFFFF = max+, 0x800000 = max- (-8388608), 0xFFFFFF = -1
        let data = [
            0x00, 0x00, 0x00, // 0
            0xFF, 0xFF, 0x7F, // 8388607
            0x00, 0x00, 0x80, // -8388608
            0xFF, 0xFF, 0xFF, // -1
        ];
        let bytes = wav(24, &data);
        let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 4];
        assert_eq!(dec.decode_into(&mut out).unwrap(), 4);
        assert_eq!(out, [0, 8_388_607, -8_388_608, -1]);
    }

    #[test]
    fn decodes_8bit_unsigned_to_signed() {
        let data = [128u8, 129, 127, 255, 0]; // → 0, 1, -1, 127, -128
        let bytes = wav(8, &data);
        let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 5];
        assert_eq!(dec.decode_into(&mut out).unwrap(), 5);
        assert_eq!(out, [0, 1, -1, 127, -128]);
    }

    #[test]
    fn decode_after_seek_lands_on_the_right_frame() {
        let mut data = Vec::new();
        for s in 0i16..10 {
            data.extend_from_slice(&s.to_le_bytes());
        }
        let bytes = wav(16, &data); // mono 16-bit, frames 0..9
        let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(dec.seek_to(5).unwrap(), 5);
        let mut out = [0i32; 3];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(&out[..n], &[5, 6, 7]);
    }

    #[test]
    fn decode_drains_across_multiple_calls() {
        let mut data = Vec::new();
        for s in 0i16..100 {
            data.extend_from_slice(&s.to_le_bytes());
        }
        let bytes = wav(16, &data);
        let mut dec = WavDecoder::new(SliceSource::new(&bytes)).unwrap();
        let mut out = [0i32; 30];
        let mut total = 0;
        loop {
            let n = dec.decode_into(&mut out).unwrap();
            if n == 0 {
                break;
            }
            for (i, &s) in out[..n].iter().enumerate() {
                assert_eq!(s, (total + i) as i32);
            }
            total += n;
        }
        assert_eq!(total, 100);
    }
}
