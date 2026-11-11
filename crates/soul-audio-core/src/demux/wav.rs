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

/// G.711 companding of the `data` bytes (8-bit → 16-bit PCM), if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Companding {
    ALaw,
    MuLaw,
}

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
    /// G.711 companding of the samples, if the codec is A-law/µ-law.
    companding: Option<Companding>,
    /// Big-endian sample data (RIFX). PCM samples must be byte-swapped on decode.
    big_endian: bool,
    /// `Some(samples_per_block)` for IMA-ADPCM (the `data` holds compressed blocks).
    adpcm: Option<u16>,
}

/// Read a little- or big-endian `u32` from the first 4 bytes of `b`.
fn read_u32(b: &[u8], be: bool) -> u32 {
    let a = [b[0], b[1], b[2], b[3]];
    if be {
        u32::from_be_bytes(a)
    } else {
        u32::from_le_bytes(a)
    }
}

/// Read a little- or big-endian `u16` from the first 2 bytes of `b`.
fn read_u16(b: &[u8], be: bool) -> u16 {
    let a = [b[0], b[1]];
    if be {
        u16::from_be_bytes(a)
    } else {
        u16::from_le_bytes(a)
    }
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
        // RIFF/RF64/BW64 are little-endian; RIFX is the big-endian variant (all
        // chunk sizes, `fmt ` fields, and sample data are big-endian).
        let rf64 = matches!(&hdr[0..4], b"RF64" | b"BW64");
        let be = &hdr[0..4] == b"RIFX";
        let valid_magic = &hdr[0..4] == b"RIFF" || rf64 || be;
        if !valid_magic || &hdr[8..12] != b"WAVE" {
            return Err(DecodeError::Malformed);
        }
        let mut pos: u64 = 12;
        // The true `data` size for RF64/BW64, read from the `ds64` chunk.
        let mut ds64_data_size: Option<u64> = None;

        let mut fmt: Option<FmtInfo> = None;
        loop {
            let mut ch = [0u8; 8];
            read_exact(&mut src, &mut ch)?;
            pos += 8;
            let id = [ch[0], ch[1], ch[2], ch[3]];
            let size = read_u32(&ch[4..8], be);
            let padded = u64::from(size) + u64::from(size & 1);

            match &id {
                b"ds64" => {
                    // RF64/BW64 sizes: riffSize:u64, dataSize:u64, sampleCount:u64, …
                    let mut d = [0u8; 24];
                    let take = (size as usize).min(24);
                    read_exact(&mut src, &mut d[..take])?;
                    if take >= 16 {
                        ds64_data_size = Some(u64::from_le_bytes([
                            d[8], d[9], d[10], d[11], d[12], d[13], d[14], d[15],
                        ]));
                    }
                    skip(&mut src, padded - take as u64)?;
                    pos += padded;
                }
                b"fmt " => {
                    fmt = Some(parse_fmt(&mut src, size, be)?);
                    pos += padded;
                }
                b"data" => {
                    let FmtInfo {
                        info,
                        block_align,
                        companding,
                        adpcm,
                    } = fmt.ok_or(DecodeError::Malformed)?;
                    if block_align == 0 {
                        return Err(DecodeError::Malformed);
                    }
                    // In RF64/BW64 the 32-bit `data` size is 0xFFFFFFFF; the real
                    // size comes from `ds64`.
                    let true_size = if size == UNKNOWN_SIZE {
                        ds64_data_size
                    } else if size == 0 {
                        None
                    } else {
                        Some(u64::from(size))
                    };
                    let (data_remaining, frames_total) = match true_size {
                        None => (u64::MAX, None), // unknown/streamed
                        Some(sz) => {
                            // Round DOWN to a whole frame — a trailing partial frame
                            // would corrupt post-seek decoding (dr_wav regression).
                            let frames = sz / u64::from(block_align);
                            (frames * u64::from(block_align), Some(frames))
                        }
                    };
                    return Ok(Self {
                        src,
                        info,
                        block_align,
                        data_start: pos,
                        data_remaining,
                        frames_total,
                        pos_frames: 0,
                        companding,
                        big_endian: be,
                        adpcm,
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

    /// G.711 companding of the samples, if the codec is A-law/µ-law.
    pub(crate) fn companding(&self) -> Option<Companding> {
        self.companding
    }

    /// Whether sample data is big-endian (RIFX).
    pub(crate) fn big_endian(&self) -> bool {
        self.big_endian
    }

    /// IMA-ADPCM samples-per-block, if the codec is IMA-ADPCM. When set, the
    /// `data` region holds compressed blocks of [`Self::block_align`] bytes each.
    pub(crate) fn adpcm_samples_per_block(&self) -> Option<u16> {
        self.adpcm
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
        // Saturate: a stream with an unknown data size (`frames_total == None`)
        // does NOT clamp `frame`, so an extreme seek target would otherwise
        // overflow the byte-offset multiply/add (a seek must never panic). A
        // byte offset past EOF simply yields `Ok(0)` reads. Real frames are far
        // below `u64::MAX`, so this is behaviour-identical for every in-range
        // seek.
        let byte = self
            .data_start
            .saturating_add(frame.saturating_mul(u64::from(self.block_align)));
        self.src
            .seek(SeekFrom::Start(byte))
            .map_err(|_| DecodeError::Io)?;
        self.pos_frames = frame;
        if let Some(total) = self.frames_total {
            self.data_remaining = total
                .saturating_sub(frame)
                .saturating_mul(u64::from(self.block_align));
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

/// Parsed `fmt ` chunk: stream parameters plus which codec decodes the `data`.
struct FmtInfo {
    info: StreamInfo,
    block_align: u16,
    companding: Option<Companding>,
    /// `Some(samples_per_block)` for IMA-ADPCM.
    adpcm: Option<u16>,
}

/// Parse a `fmt ` chunk body of `size` bytes (already consumed the 8-byte chunk
/// header). Advances past the body + pad.
fn parse_fmt<S: Source>(src: &mut S, size: u32, be: bool) -> Result<FmtInfo, DecodeError> {
    let mut fmt = [0u8; 40];
    let take = (size as usize).min(fmt.len());
    if take < 16 {
        return Err(DecodeError::Malformed);
    }
    read_exact(src, &mut fmt[..take])?;
    // Skip the rest of the body + any odd pad byte.
    let consumed = take as u64;
    skip(src, u64::from(size) + u64::from(size & 1) - consumed)?;

    let tag = read_u16(&fmt[0..2], be);
    let channels = read_u16(&fmt[2..4], be);
    let sample_rate = read_u32(&fmt[4..8], be);
    let block_align = read_u16(&fmt[12..14], be);
    let bits = read_u16(&fmt[14..16], be);

    // WAVE_FORMAT_EXTENSIBLE: the true format tag is the first u16 of the
    // SubFormat GUID at byte 24.
    let real_tag = if tag == 0xFFFE && take >= 26 {
        read_u16(&fmt[24..26], be)
    } else {
        tag
    };

    // A-law/µ-law (companded) and IMA-ADPCM all decode to 16-bit PCM.
    let mut companding = None;
    let mut adpcm = None;
    let format = match real_tag {
        0x0001 => SampleFormat::Int { bits: bits as u8 },
        0x0003 => SampleFormat::Float { bits: bits as u8 },
        0x0006 => {
            companding = Some(Companding::ALaw);
            SampleFormat::Int { bits: 16 }
        }
        0x0007 => {
            companding = Some(Companding::MuLaw);
            SampleFormat::Int { bits: 16 }
        }
        0x0011 => {
            // IMA/DVI-ADPCM: samples-per-block is in the fmt extension (bytes
            // 18..20, after the 2-byte cbSize).
            let spb = if take >= 20 {
                read_u16(&fmt[18..20], be)
            } else {
                return Err(DecodeError::Unsupported);
            };
            adpcm = Some(spb);
            SampleFormat::Int { bits: 16 }
        }
        _ => return Err(DecodeError::Unsupported),
    };

    Ok(FmtInfo {
        info: StreamInfo {
            sample_rate,
            channels,
            format,
        },
        block_align,
        companding,
        adpcm,
    })
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
    fn seek_on_unknown_size_stream_does_not_overflow() {
        // Regression (tests/fuzz.rs: a length-field corruption of a minimal
        // WAV). A `data` chunk declaring size 0 is an unknown/streamed length,
        // so `frames_total` is None and `seek_to` does NOT clamp its argument.
        // An extreme target must saturate the byte-offset multiply/add rather
        // than overflowing — a seek must never panic.
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&36u32.to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // PCM
        v.extend_from_slice(&1u16.to_le_bytes()); // mono
        v.extend_from_slice(&8000u32.to_le_bytes());
        v.extend_from_slice(&16000u32.to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes()); // block_align
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&0u32.to_le_bytes()); // size 0 → unknown/streamed
        v.extend_from_slice(&[0, 0, 1, 0, 2, 0, 3, 0]);
        let mut d = WavDemuxer::new(SliceSource::new(&v)).expect("open");
        assert_eq!(d.frames_total(), None, "precondition: length is unknown");
        // Must return gracefully, never panic on the byte-offset arithmetic.
        let _ = d.seek_to(u64::MAX);
        let _ = d.seek_to(u64::MAX / 2);
    }

    #[test]
    fn rf64_reads_64bit_data_size_from_ds64() {
        let data = [1u8, 2, 3, 4, 5, 6, 7, 8]; // 4 frames of 16-bit mono
        let mut v = Vec::new();
        v.extend_from_slice(b"RF64");
        v.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        v.extend_from_slice(b"WAVE");
        // ds64: riffSize, dataSize, sampleCount, tableLength(=0).
        v.extend_from_slice(b"ds64");
        v.extend_from_slice(&28u32.to_le_bytes());
        v.extend_from_slice(&9999u64.to_le_bytes()); // riffSize (unused)
        v.extend_from_slice(&(data.len() as u64).to_le_bytes()); // dataSize
        v.extend_from_slice(&(data.len() as u64 / 2).to_le_bytes()); // sampleCount
        v.extend_from_slice(&0u32.to_le_bytes()); // tableLength
        // fmt (16-bit mono)
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&44_100u32.to_le_bytes());
        v.extend_from_slice(&88_200u32.to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        // data with the RF64 sentinel size.
        v.extend_from_slice(b"data");
        v.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        v.extend_from_slice(&data);

        let mut d = WavDemuxer::new(SliceSource::new(&v)).unwrap();
        assert_eq!(d.stream_info().sample_rate, 44_100);
        assert_eq!(d.frames_total(), Some(4)); // ds64 dataSize 8 / blockAlign 2
        let mut scratch = [0u8; 16];
        let pkt = d.next_packet(&mut scratch).unwrap().unwrap();
        assert_eq!(pkt.data, &data); // bounded by the ds64 size, not 0xFFFFFFFF
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
