//! WAV (RIFF/WAVE) demuxer — parses the container into [`StreamInfo`] and
//! streams the `data` chunk as PCM packets. Codec (widening PCM bytes → `i32`)
//! lives in [`crate::formats::wav`].

use embedded_io::SeekFrom;

use super::{Demuxer, Packet, read_exact, skip};
use crate::decoder::DecodeError;
use crate::io::{SeekSource, Source};
use crate::model::{SampleFormat, StreamInfo};

/// Unknown/streamed `data` size marker (also `0` is treated as unknown).
const UNKNOWN_SIZE: u32 = 0xFFFF_FFFF;

/// RIFF/WAVE container parser over a byte [`Source`].
pub struct WavDemuxer<S> {
    src: S,
    info: StreamInfo,
    block_align: u16,
    /// Absolute byte offset of the first sample (start of the `data` body).
    data_start: u64,
    /// Bytes left in the `data` chunk; `u64::MAX` = unknown/streamed.
    data_remaining: u64,
    /// Total frames if known.
    frames_total: Option<u64>,
    /// Frames read so far (packet timestamps).
    pos_frames: u64,
}

impl<S: Source> WavDemuxer<S> {
    /// Parse the RIFF/WAVE header from `src`.
    ///
    /// # Errors
    /// [`DecodeError::Malformed`] for a bad container, [`DecodeError::Unsupported`]
    /// for a non-PCM/float codec, [`DecodeError::Io`]/`UnexpectedEof` on read
    /// failure.
    pub fn new(mut src: S) -> Result<Self, DecodeError> {
        let mut hdr = [0u8; 12];
        read_exact(&mut src, &mut hdr)?;
        if &hdr[0..4] != b"RIFF" || &hdr[8..12] != b"WAVE" {
            return Err(DecodeError::Malformed);
        }
        let mut pos: u64 = 12;

        let mut fmt: Option<(StreamInfo, u16)> = None;
        loop {
            let mut ch = [0u8; 8];
            read_exact(&mut src, &mut ch)?;
            pos += 8;
            let id = [ch[0], ch[1], ch[2], ch[3]];
            let size = u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]);
            let padded = u64::from(size) + u64::from(size & 1);

            match &id {
                b"fmt " => {
                    fmt = Some(parse_fmt(&mut src, size)?);
                    pos += padded;
                }
                b"data" => {
                    let (info, block_align) = fmt.ok_or(DecodeError::Malformed)?;
                    if block_align == 0 {
                        return Err(DecodeError::Malformed);
                    }
                    let (data_remaining, frames_total) = if size == UNKNOWN_SIZE || size == 0 {
                        (u64::MAX, None)
                    } else {
                        // Round the data chunk DOWN to a whole number of frames — a
                        // trailing partial frame would otherwise corrupt post-seek
                        // decoding (dr_wav regression).
                        let frames = u64::from(size) / u64::from(block_align);
                        (frames * u64::from(block_align), Some(frames))
                    };
                    return Ok(Self {
                        src,
                        info,
                        block_align,
                        data_start: pos,
                        data_remaining,
                        frames_total,
                        pos_frames: 0,
                    });
                }
                _ => {
                    skip(&mut src, padded)?;
                    pos += padded;
                }
            }
        }
    }

    /// Bytes per interleaved frame (`nBlockAlign`).
    #[must_use]
    pub fn block_align(&self) -> u16 {
        self.block_align
    }

    /// Total frames, if the `data` size was known.
    #[must_use]
    pub fn frames_total(&self) -> Option<u64> {
        self.frames_total
    }

    /// Read raw PCM bytes from the `data` region into `out`, bounded by the
    /// remaining data. Returns bytes read (`0` at end of data). Shared by the
    /// packet iterator and the codec.
    pub(crate) fn read_data(&mut self, out: &mut [u8]) -> Result<usize, DecodeError> {
        if self.data_remaining == 0 || out.is_empty() {
            return Ok(0);
        }
        let want = (out.len() as u64).min(self.data_remaining) as usize;
        let mut n = 0;
        while n < want {
            match self.src.read(&mut out[n..want]) {
                Ok(0) => break, // EOF (expected for unknown/streamed size)
                Ok(k) => n += k,
                Err(_) => return Err(DecodeError::Io),
            }
        }
        if self.data_remaining != u64::MAX {
            self.data_remaining -= n as u64;
        }
        Ok(n)
    }
}

impl<S: SeekSource> WavDemuxer<S> {
    /// Seek to interleaved `frame` (random access; PCM is constant-rate, so this
    /// is pure arithmetic). Returns the frame actually landed on.
    ///
    /// # Errors
    /// [`DecodeError::Io`] if the underlying seek fails.
    pub fn seek_to(&mut self, frame: u64) -> Result<u64, DecodeError> {
        let frame = self.frames_total.map_or(frame, |t| frame.min(t));
        let byte = self.data_start + frame * u64::from(self.block_align);
        self.src
            .seek(SeekFrom::Start(byte))
            .map_err(|_| DecodeError::Io)?;
        self.pos_frames = frame;
        if let Some(total) = self.frames_total {
            self.data_remaining = (total - frame) * u64::from(self.block_align);
        }
        Ok(frame)
    }
}

impl<S: Source> Demuxer for WavDemuxer<S> {
    fn stream_info(&self) -> StreamInfo {
        self.info
    }

    fn next_packet<'a>(
        &mut self,
        scratch: &'a mut [u8],
    ) -> Result<Option<Packet<'a>>, DecodeError> {
        // Round the scratch down to a whole number of frames.
        let cap = scratch.len() - scratch.len() % usize::from(self.block_align);
        let n = self.read_data(&mut scratch[..cap])?;
        if n == 0 {
            return Ok(None);
        }
        let dur = (n / usize::from(self.block_align)) as u32;
        let ts = self.pos_frames;
        self.pos_frames += u64::from(dur);
        Ok(Some(Packet {
            ts,
            dur,
            data: &scratch[..n],
        }))
    }
}

/// Parse a `fmt ` chunk body of `size` bytes (already consumed the 8-byte chunk
/// header). Advances past the body + pad. Returns `(StreamInfo, block_align)`.
fn parse_fmt<S: Source>(src: &mut S, size: u32) -> Result<(StreamInfo, u16), DecodeError> {
    let mut fmt = [0u8; 40];
    let take = (size as usize).min(fmt.len());
    if take < 16 {
        return Err(DecodeError::Malformed);
    }
    read_exact(src, &mut fmt[..take])?;
    // Skip the rest of the body + any odd pad byte.
    let consumed = take as u64;
    skip(src, u64::from(size) + u64::from(size & 1) - consumed)?;

    let tag = u16::from_le_bytes([fmt[0], fmt[1]]);
    let channels = u16::from_le_bytes([fmt[2], fmt[3]]);
    let sample_rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
    let block_align = u16::from_le_bytes([fmt[12], fmt[13]]);
    let bits = u16::from_le_bytes([fmt[14], fmt[15]]);

    // WAVE_FORMAT_EXTENSIBLE: the true format tag is the first u16 of the
    // SubFormat GUID at byte 24.
    let real_tag = if tag == 0xFFFE && take >= 26 {
        u16::from_le_bytes([fmt[24], fmt[25]])
    } else {
        tag
    };

    let format = match real_tag {
        0x0001 => SampleFormat::Int { bits: bits as u8 },
        0x0003 => SampleFormat::Float { bits: bits as u8 },
        _ => return Err(DecodeError::Unsupported),
    };

    Ok((
        StreamInfo {
            sample_rate,
            channels,
            format,
        },
        block_align,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceSource;

    /// Build a canonical little-endian WAV: PCM/int (tag 1) or float (tag 3),
    /// with the given `data` bytes.
    fn wav(tag: u16, channels: u16, rate: u32, bits: u16, data: &[u8]) -> Vec<u8> {
        let block_align = channels * (bits / 8);
        let byte_rate = rate * u32::from(block_align);
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36u32 + data.len() as u32).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&tag.to_le_bytes());
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&byte_rate.to_le_bytes());
        v.extend_from_slice(&block_align.to_le_bytes());
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(data);
        v
    }

    #[test]
    fn parses_pcm16_stream_info() {
        let bytes = wav(1, 2, 44_100, 16, &[0; 8]);
        let d = WavDemuxer::new(SliceSource::new(&bytes)).unwrap();
        let si = d.stream_info();
        assert_eq!(si.sample_rate, 44_100);
        assert_eq!(si.channels, 2);
        assert_eq!(si.format, SampleFormat::Int { bits: 16 });
        assert_eq!(d.block_align(), 4);
        assert_eq!(d.frames_total(), Some(2));
    }

    #[test]
    fn float_tag_gives_float_format() {
        let bytes = wav(3, 1, 48_000, 32, &[0; 8]);
        let d = WavDemuxer::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(d.stream_info().format, SampleFormat::Float { bits: 32 });
    }

    #[test]
    fn skips_unknown_and_odd_sized_chunks_before_data() {
        // A JUNK chunk of odd size (3) must be skipped as 3 + 1 pad = 4 bytes,
        // or every following chunk desyncs.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&100u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"JUNK");
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0x00]); // 3 body + 1 pad
        // fmt
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&8000u32.to_le_bytes());
        bytes.extend_from_slice(&16000u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&[1, 2, 3, 4]);

        let d = WavDemuxer::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(d.stream_info().sample_rate, 8000);
        assert_eq!(d.frames_total(), Some(2));
    }

    #[test]
    fn extensible_unwraps_subformat_guid() {
        // WAVE_FORMAT_EXTENSIBLE (0xFFFE), 24-bit, SubFormat = PCM GUID.
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&0xFFFEu16.to_le_bytes()); // tag
        fmt.extend_from_slice(&2u16.to_le_bytes()); // channels
        fmt.extend_from_slice(&96_000u32.to_le_bytes()); // rate
        fmt.extend_from_slice(&576_000u32.to_le_bytes()); // byte rate
        fmt.extend_from_slice(&6u16.to_le_bytes()); // block align (2ch*3)
        fmt.extend_from_slice(&24u16.to_le_bytes()); // bits
        fmt.extend_from_slice(&22u16.to_le_bytes()); // cbSize
        fmt.extend_from_slice(&24u16.to_le_bytes()); // valid bits
        fmt.extend_from_slice(&0x3u32.to_le_bytes()); // channel mask
        fmt.extend_from_slice(&1u16.to_le_bytes()); // GUID first u16 = PCM
        fmt.extend_from_slice(&[0u8; 14]); // rest of GUID

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&fmt);
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&6u32.to_le_bytes());
        bytes.extend_from_slice(&[0; 6]);

        let d = WavDemuxer::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(d.stream_info().format, SampleFormat::Int { bits: 24 });
        assert_eq!(d.stream_info().channels, 2);
    }

    #[test]
    fn next_packet_yields_data_bytes() {
        let bytes = wav(1, 1, 8000, 16, &[1, 0, 2, 0, 3, 0]);
        let mut d = WavDemuxer::new(SliceSource::new(&bytes)).unwrap();
        let mut scratch = [0u8; 16];
        let pkt = d.next_packet(&mut scratch).unwrap().unwrap();
        assert_eq!(pkt.data, &[1, 0, 2, 0, 3, 0]);
        assert_eq!(pkt.dur, 3); // 6 bytes / block_align 2
        assert_eq!(pkt.ts, 0);
        assert!(d.next_packet(&mut scratch).unwrap().is_none());
    }

    #[test]
    fn seek_repositions_by_frame() {
        let data: Vec<u8> = (0..20).collect(); // 10 frames of 16-bit mono
        let bytes = wav(1, 1, 8000, 16, &data);
        let mut d = WavDemuxer::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(d.seek_to(3).unwrap(), 3);
        let mut scratch = [0u8; 2]; // one 16-bit mono frame
        let pkt = d.next_packet(&mut scratch).unwrap().unwrap();
        assert_eq!(pkt.data, &[6, 7]); // frame 3 = data bytes 6,7
        assert_eq!(pkt.ts, 3);
    }
}
