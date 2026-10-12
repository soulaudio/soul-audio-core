//! AIFF / AIFF-C decoder = [`AiffDemuxer`](crate::demux::AiffDemuxer) (container)
//! + PCM decoding (codec). Streams interleaved `i32` PCM.
//!
//! Big-endian integer PCM (8/16/24/32-bit) is widened preserving its true depth;
//! `sowt` is the little-endian variant. IEEE **float** PCM (32/64-bit) is
//! converted to full-scale 32-bit integer PCM, and G.711 A-law/µ-law (8-bit) is
//! expanded to 16-bit — all through the shared
//! [`convert_samples`](crate::formats::wav::convert_samples) helper that
//! [`crate::formats::wav`] uses, so AIFF and WAV share one PCM conversion path.

use super::wav::convert_samples;
use crate::decoder::{DecodeError, Decoder, SeekableDecoder, secs_to_samples};
use crate::demux::wav::Companding;
use crate::demux::{AiffDemuxer, Demuxer};
use crate::io::{SeekSource, Source};
use crate::model::{SampleFormat, StreamInfo};

/// AIFF / AIFF-C decoder over a byte [`Source`].
pub struct AiffDecoder<S> {
    demux: AiffDemuxer<S>,
    /// Input bytes per sample (`block_align / channels`): 1 (companded), 2, 3, 4
    /// (int) or 4/8 (float).
    bps: usize,
    bits: u8,
    float: bool,
    companding: Option<Companding>,
    big_endian: bool,
    channels: u16,
    sample_pos: u64, // interleaved samples produced since the start
}

impl<S: Source> AiffDecoder<S> {
    /// Parse the AIFF/AIFF-C header and prepare to stream PCM.
    ///
    /// # Errors
    /// Propagates [`AiffDemuxer::new`] errors; [`DecodeError::Unsupported`] for an
    /// unsupported bit depth.
    pub fn new(src: S) -> Result<Self, DecodeError> {
        let demux = AiffDemuxer::new(src)?;
        let info = demux.stream_info();
        if info.channels == 0 {
            return Err(DecodeError::Unsupported);
        }
        let companding = demux.companding();
        let (bits, float) = if companding.is_some() {
            (16, false) // 8-bit companded in, 16-bit out (bits unused by that path)
        } else {
            match info.format {
                SampleFormat::Int { bits } => (bits, false),
                SampleFormat::Float { bits } => (bits, true),
                SampleFormat::Dsd { .. } => return Err(DecodeError::Unsupported),
            }
        };
        if companding.is_none() {
            let valid = if float {
                bits == 32 || bits == 64
            } else {
                bits != 0 && bits % 8 == 0 && bits <= 32
            };
            if !valid {
                return Err(DecodeError::Unsupported);
            }
        }
        let bps = usize::from(demux.block_align()) / usize::from(info.channels);
        if bps == 0 {
            return Err(DecodeError::Unsupported);
        }
        let big_endian = demux.big_endian();
        Ok(Self {
            demux,
            bps,
            bits,
            float,
            companding,
            big_endian,
            channels: info.channels,
            sample_pos: 0,
        })
    }

    /// Total per-channel frames, if the header declared them.
    #[must_use]
    pub fn total_frames(&self) -> Option<u64> {
        self.demux.frames_total()
    }

    /// Current playback position, in interleaved frames from the start.
    #[must_use]
    pub fn position(&self) -> u64 {
        self.sample_pos / u64::from(self.channels)
    }
}

impl<S: SeekSource> AiffDecoder<S> {
    /// Seek to interleaved `frame`. Returns the frame landed on.
    ///
    /// # Errors
    /// [`DecodeError::Io`] if the seek fails.
    pub fn seek_to(&mut self, frame: u64) -> Result<u64, DecodeError> {
        let landed = self.demux.seek_to(frame)?;
        self.sample_pos = landed * u64::from(self.channels);
        Ok(landed)
    }

    /// Seek to `secs` seconds. Returns the frame landed on.
    ///
    /// # Errors
    /// [`DecodeError::Io`] if the seek fails.
    pub fn seek_to_time(&mut self, secs: f64) -> Result<u64, DecodeError> {
        self.seek_to(secs_to_samples(secs, self.demux.stream_info().sample_rate))
    }
}

impl<S: Source> Decoder for AiffDecoder<S> {
    fn stream_info(&self) -> StreamInfo {
        let si = self.demux.stream_info();
        // Float is converted to full-scale 32-bit integer PCM on output.
        if self.float {
            StreamInfo {
                format: SampleFormat::Int { bits: 32 },
                ..si
            }
        } else {
            si
        }
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        // Batch buffer: 768 is divisible by every supported sample width (1..4, 8).
        let mut buf = [0u8; 768];
        let mut produced = 0;
        while produced < out.len() {
            let cap = (out.len() - produced).min(buf.len() / self.bps) * self.bps;
            if cap == 0 {
                break;
            }
            let n = self.demux.read_data(&mut buf[..cap])?;
            if n == 0 {
                break;
            }
            let samples = n / self.bps;
            convert_samples(
                &buf[..n],
                self.bps,
                self.bits,
                self.float,
                self.companding,
                self.big_endian,
                &mut out[produced..produced + samples],
            );
            produced += samples;
            if n < cap {
                break; // end of data (or a short final read)
            }
        }
        self.sample_pos += produced as u64;
        Ok(produced)
    }
}

impl<S: SeekSource> SeekableDecoder for AiffDecoder<S> {
    fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        AiffDecoder::seek_to(self, sample)
    }

    fn total_frames(&self) -> Option<u64> {
        self.demux.frames_total()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceSource;

    /// 80-bit extended for 44100 Hz (exponent 0x400E, mantissa 0xAC44…).
    const EXT_44100: [u8; 10] = [0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0];

    /// Build an AIFF (`FORM`/`AIFF`, big-endian PCM) or AIFF-C (`AIFC`) with a
    /// 4-byte compression type.
    fn aiff(
        form: &[u8; 4],
        channels: u16,
        bits: u16,
        frames: u32,
        in_bytes: u16,
        compression: Option<&[u8; 4]>,
        data: &[u8],
    ) -> Vec<u8> {
        let mut comm = Vec::new();
        comm.extend_from_slice(&channels.to_be_bytes());
        comm.extend_from_slice(&frames.to_be_bytes());
        comm.extend_from_slice(&bits.to_be_bytes());
        comm.extend_from_slice(&EXT_44100);
        if let Some(c) = compression {
            comm.extend_from_slice(c);
        }
        let _ = in_bytes;
        let mut ssnd = Vec::new();
        ssnd.extend_from_slice(&0u32.to_be_bytes());
        ssnd.extend_from_slice(&0u32.to_be_bytes());
        ssnd.extend_from_slice(data);
        let mut v = Vec::new();
        v.extend_from_slice(b"FORM");
        v.extend_from_slice(&((4 + 8 + comm.len() + 8 + ssnd.len()) as u32).to_be_bytes());
        v.extend_from_slice(form);
        v.extend_from_slice(b"COMM");
        v.extend_from_slice(&(comm.len() as u32).to_be_bytes());
        v.extend_from_slice(&comm);
        v.extend_from_slice(b"SSND");
        v.extend_from_slice(&(ssnd.len() as u32).to_be_bytes());
        v.extend_from_slice(&ssnd);
        v
    }

    fn decode(bytes: &[u8]) -> (StreamInfo, Vec<i32>) {
        let mut d = AiffDecoder::new(SliceSource::new(bytes)).unwrap();
        let si = d.stream_info();
        let mut out = vec![0i32; 64];
        let n = d.decode_into(&mut out).unwrap();
        out.truncate(n);
        (si, out)
    }

    #[test]
    fn decodes_be_pcm16() {
        // Two stereo frames: L/R = 0x0102, 0x0304, 0x0506, 0x0708 (big-endian).
        let data = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let bytes = aiff(b"AIFF", 2, 16, 2, 2, None, &data);
        let (si, pcm) = decode(&bytes);
        assert_eq!(si.format, SampleFormat::Int { bits: 16 });
        assert_eq!(si.channels, 2);
        assert_eq!(pcm, vec![0x0102, 0x0304, 0x0506, 0x0708]);
    }

    #[test]
    fn sowt_is_little_endian_pcm16() {
        // Same bytes as above but little-endian (AIFF-C `sowt`).
        let data = [0x02, 0x01, 0x04, 0x03];
        let bytes = aiff(b"AIFC", 1, 16, 2, 2, Some(b"sowt"), &data);
        let (_, pcm) = decode(&bytes);
        assert_eq!(pcm, vec![0x0102, 0x0304]);
    }

    #[test]
    fn decodes_be_pcm24() {
        // One mono 24-bit sample = 0x7FFFFF (max positive), big-endian.
        let data = [0x7F, 0xFF, 0xFF];
        let bytes = aiff(b"AIFF", 1, 24, 1, 3, None, &data);
        let (si, pcm) = decode(&bytes);
        assert_eq!(si.format, SampleFormat::Int { bits: 24 });
        assert_eq!(pcm, vec![0x7F_FFFF]);
    }

    #[test]
    fn fl32_converts_to_full_scale_i32() {
        // One mono float32 sample = 1.0 (big-endian 0x3F800000) → ~i32::MAX.
        let data = [0x3F, 0x80, 0x00, 0x00];
        let bytes = aiff(b"AIFC", 1, 32, 1, 4, Some(b"fl32"), &data);
        let (si, pcm) = decode(&bytes);
        assert_eq!(si.format, SampleFormat::Int { bits: 32 });
        assert_eq!(pcm, vec![2_147_483_647]);
    }

    #[test]
    fn seek_repositions_by_frame() {
        let data: Vec<u8> = (0..20).collect(); // 10 mono 16-bit frames
        let bytes = aiff(b"AIFF", 1, 16, 10, 2, None, &data);
        let mut d = AiffDecoder::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(SeekableDecoder::seek_to(&mut d, 3).unwrap(), 3);
        let mut out = [0i32; 1];
        d.decode_into(&mut out).unwrap();
        assert_eq!(out[0], i32::from(i16::from_be_bytes([6, 7]))); // frame 3
        assert_eq!(d.total_frames(), Some(10));
    }
}
