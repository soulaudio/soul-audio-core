//! AIFF / AIFF-C (`FORM`…`AIFF`/`AIFC`) demuxer — parses the container into
//! [`StreamInfo`] and streams the `SSND` chunk as PCM packets. Codec (widening
//! the sample bytes → `i32`) lives in [`crate::formats::aiff`], which reuses the
//! same per-sample helpers as [`crate::formats::wav`].
//!
//! AIFF is big-endian signed PCM. AIFF-C (`AIFC`) adds a 4-byte `compressionType`
//! in `COMM`: `NONE`/`twos` (big-endian PCM, same as AIFF), `sowt` (little-endian
//! PCM), `fl32`/`FL32`/`fl64`/`FL64` (IEEE float, big-endian), and `ulaw`/`ULAW`/
//! `alaw`/`ALAW` (G.711, 8-bit → 16-bit). Apple `ima4` ADPCM is not yet handled.

use embedded_io::SeekFrom;

use super::wav::Companding;
use super::{Demuxer, Packet, read_exact, skip};
use crate::decoder::DecodeError;
use crate::io::{SeekSource, Source};
use crate::model::{SampleFormat, StreamInfo};

/// RIFF-style AIFF/AIFF-C container parser over a byte [`Source`].
pub struct AiffDemuxer<S> {
    src: S,
    info: StreamInfo,
    /// Bytes per interleaved frame (`channels × input-bytes-per-sample`).
    block_align: u16,
    /// Absolute byte offset of the first sample (`SSND` body + 8 + `offset`).
    data_start: u64,
    /// Bytes left in the sample region; `u64::MAX` = unknown.
    data_remaining: u64,
    /// Total frames (`COMM.numSampleFrames`), if known.
    frames_total: Option<u64>,
    /// Frames read so far (packet timestamps).
    pos_frames: u64,
    /// G.711 companding of the samples, if the codec is A-law/µ-law.
    companding: Option<Companding>,
    /// Big-endian sample data (AIFF / `NONE` / `twos` / float). `sowt` is false.
    big_endian: bool,
}

/// Convert an 80-bit IEEE 754 extended-precision (SANE "extended") value to an
/// integer sample rate in Hz. Layout: sign(1) | exponent(15, bias 16383) |
/// mantissa(64, explicit integer bit). Pure integer math — `no_std`-safe, no
/// `libm`. Rounds to nearest; clamps absurd exponents to a sane rate range.
fn extended_to_rate(ext: &[u8; 10]) -> u32 {
    let exponent = ((u32::from(ext[0]) & 0x7f) << 8) | u32::from(ext[1]);
    let mantissa = u64::from_be_bytes([
        ext[2], ext[3], ext[4], ext[5], ext[6], ext[7], ext[8], ext[9],
    ]);
    if mantissa == 0 || exponent == 0 {
        return 0;
    }
    // value = mantissa × 2^(exponent − 16383 − 63)
    let e = exponent as i32 - 16383;
    let shift = 63 - e; // right-shift of the 64-bit mantissa to reach an integer
    if shift <= 0 {
        // Exponent too large for a real sample rate — clamp.
        return u32::MAX;
    }
    if shift >= 64 {
        return 0; // value < 1 Hz
    }
    let half = 1u64 << (shift - 1);
    let rounded = mantissa.saturating_add(half) >> shift;
    u32::try_from(rounded).unwrap_or(u32::MAX)
}

/// Read a big-endian `u32` from the first 4 bytes of `b`.
fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Parsed `COMM` chunk: stream parameters plus which codec decodes the samples.
struct CommInfo {
    info: StreamInfo,
    block_align: u16,
    frames_total: Option<u64>,
    companding: Option<Companding>,
    big_endian: bool,
}

impl<S: Source> AiffDemuxer<S> {
    /// Parse the AIFF/AIFF-C header from `src`.
    ///
    /// # Errors
    /// [`DecodeError::Malformed`] for a bad container, [`DecodeError::Unsupported`]
    /// for an unsupported compression type, [`DecodeError::Io`]/`UnexpectedEof`
    /// on read failure.
    pub fn new(mut src: S) -> Result<Self, DecodeError> {
        let mut hdr = [0u8; 12];
        read_exact(&mut src, &mut hdr)?;
        if &hdr[0..4] != b"FORM" {
            return Err(DecodeError::Malformed);
        }
        let is_aifc = &hdr[8..12] == b"AIFC";
        if &hdr[8..12] != b"AIFF" && !is_aifc {
            return Err(DecodeError::Malformed);
        }
        let mut pos: u64 = 12;
        let mut comm: Option<CommInfo> = None;
        loop {
            let mut ch = [0u8; 8];
            read_exact(&mut src, &mut ch)?;
            pos += 8;
            let id = [ch[0], ch[1], ch[2], ch[3]];
            let size = be_u32(&ch[4..8]);
            let padded = u64::from(size) + u64::from(size & 1);

            match &id {
                b"COMM" => {
                    comm = Some(parse_comm(&mut src, size, is_aifc)?);
                    pos += padded;
                }
                b"SSND" => {
                    let CommInfo {
                        info,
                        block_align,
                        frames_total,
                        companding,
                        big_endian,
                    } = comm.ok_or(DecodeError::Malformed)?;
                    if block_align == 0 {
                        return Err(DecodeError::Malformed);
                    }
                    // SSND body: offset:u32, blockSize:u32, then the samples. The
                    // first sample starts `offset` bytes into the body.
                    let mut head = [0u8; 8];
                    read_exact(&mut src, &mut head)?;
                    let offset = u64::from(be_u32(&head[0..4]));
                    skip(&mut src, offset)?;
                    let data_start = pos + 8 + offset;
                    // Sample bytes available in this chunk, after the 8-byte
                    // offset/blockSize header and the `offset` pad.
                    let ssnd_bytes = u64::from(size).saturating_sub(8 + offset);
                    // Prefer COMM.numSampleFrames (authoritative); otherwise derive
                    // from the SSND size, rounded down to a whole frame.
                    let (frames_total, data_remaining) = match frames_total {
                        Some(f) => (Some(f), f * u64::from(block_align)),
                        None => {
                            let f = ssnd_bytes / u64::from(block_align);
                            (Some(f), f * u64::from(block_align))
                        }
                    };
                    // Never read past the bytes the chunk actually holds.
                    let data_remaining = data_remaining.min(ssnd_bytes);
                    return Ok(Self {
                        src,
                        info,
                        block_align,
                        data_start,
                        data_remaining,
                        frames_total,
                        pos_frames: 0,
                        companding,
                        big_endian,
                    });
                }
                _ => {
                    skip(&mut src, padded)?;
                    pos += padded;
                }
            }
        }
    }

    /// Bytes per interleaved frame.
    #[must_use]
    pub fn block_align(&self) -> u16 {
        self.block_align
    }

    /// Total frames (`COMM.numSampleFrames`), if known.
    #[must_use]
    pub fn frames_total(&self) -> Option<u64> {
        self.frames_total
    }

    /// G.711 companding of the samples, if the codec is A-law/µ-law.
    pub(crate) fn companding(&self) -> Option<Companding> {
        self.companding
    }

    /// Whether sample data is big-endian (AIFF / `NONE` / float). `sowt` = false.
    pub(crate) fn big_endian(&self) -> bool {
        self.big_endian
    }

    /// Read raw sample bytes from the data region into `out`, bounded by the
    /// remaining data. Returns bytes read (`0` at end). Shared by the packet
    /// iterator and the codec.
    pub(crate) fn read_data(&mut self, out: &mut [u8]) -> Result<usize, DecodeError> {
        if self.data_remaining == 0 || out.is_empty() {
            return Ok(0);
        }
        let want = (out.len() as u64).min(self.data_remaining) as usize;
        let mut n = 0;
        while n < want {
            match self.src.read(&mut out[n..want]) {
                Ok(0) => break,
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

impl<S: SeekSource> AiffDemuxer<S> {
    /// Seek to interleaved `frame` (PCM is constant-rate → pure arithmetic).
    /// Returns the frame actually landed on.
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

impl<S: Source> Demuxer for AiffDemuxer<S> {
    fn stream_info(&self) -> StreamInfo {
        self.info
    }

    fn next_packet<'a>(
        &mut self,
        scratch: &'a mut [u8],
    ) -> Result<Option<Packet<'a>>, DecodeError> {
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

/// Parse a `COMM` chunk body of `size` bytes (the 8-byte chunk header already
/// consumed). Advances past the body + any odd pad byte.
fn parse_comm<S: Source>(src: &mut S, size: u32, is_aifc: bool) -> Result<CommInfo, DecodeError> {
    // AIFF COMM is 18 bytes; AIFC adds a 4-byte compressionType (+ a Pascal
    // compressionName we skip).
    let mut c = [0u8; 22];
    let take = (size as usize).min(c.len());
    if take < 18 {
        return Err(DecodeError::Malformed);
    }
    read_exact(src, &mut c[..take])?;
    skip(src, u64::from(size) + u64::from(size & 1) - take as u64)?;

    let channels = u16::from_be_bytes([c[0], c[1]]);
    let num_frames = u64::from(be_u32(&c[2..6]));
    let bits = u16::from_be_bytes([c[6], c[7]]); // COMM sampleSize
    let rate = extended_to_rate(&[
        c[8], c[9], c[10], c[11], c[12], c[13], c[14], c[15], c[16], c[17],
    ]);

    let compression = if is_aifc && take >= 22 {
        [c[18], c[19], c[20], c[21]]
    } else {
        *b"NONE"
    };

    let mut companding = None;
    let mut big_endian = true;
    let (format, in_bytes) = match &compression {
        b"NONE" | b"twos" => (SampleFormat::Int { bits: bits as u8 }, bits.div_ceil(8)),
        b"sowt" | b"SOWT" => {
            big_endian = false;
            (SampleFormat::Int { bits: bits as u8 }, bits.div_ceil(8))
        }
        b"fl32" | b"FL32" => (SampleFormat::Float { bits: 32 }, 4),
        b"fl64" | b"FL64" => (SampleFormat::Float { bits: 64 }, 8),
        b"ulaw" | b"ULAW" => {
            companding = Some(Companding::MuLaw);
            (SampleFormat::Int { bits: 16 }, 1)
        }
        b"alaw" | b"ALAW" => {
            companding = Some(Companding::ALaw);
            (SampleFormat::Int { bits: 16 }, 1)
        }
        _ => return Err(DecodeError::Unsupported),
    };
    if channels == 0 || in_bytes == 0 {
        return Err(DecodeError::Malformed);
    }
    let block_align = channels
        .checked_mul(in_bytes)
        .ok_or(DecodeError::Malformed)?;

    Ok(CommInfo {
        info: StreamInfo {
            sample_rate: rate,
            channels,
            format,
        },
        block_align,
        frames_total: Some(num_frames),
        companding,
        big_endian,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceSource;

    /// 80-bit extended for 44100 Hz: exponent 16398, mantissa 0xAC44000000000000.
    fn ext_44100() -> [u8; 10] {
        [0x40, 0x0E, 0xAC, 0x44, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
    }

    #[test]
    fn extended_decodes_common_rates() {
        assert_eq!(extended_to_rate(&ext_44100()), 44_100);
        // 48000 Hz: exponent 16398, mantissa 0xBB80000000000000.
        assert_eq!(
            extended_to_rate(&[0x40, 0x0E, 0xBB, 0x80, 0, 0, 0, 0, 0, 0]),
            48_000
        );
        // 8000 Hz: exponent 16395 (2^12 ≤ 8000 < 2^13), mantissa 0xFA00…
        assert_eq!(
            extended_to_rate(&[0x40, 0x0B, 0xFA, 0x00, 0, 0, 0, 0, 0, 0]),
            8_000
        );
    }

    /// Build a minimal AIFF (`FORM`/`AIFF`) with COMM + SSND for big-endian PCM.
    fn aiff(channels: u16, bits: u16, frames: u32, data: &[u8]) -> Vec<u8> {
        let mut comm = Vec::new();
        comm.extend_from_slice(&channels.to_be_bytes());
        comm.extend_from_slice(&frames.to_be_bytes());
        comm.extend_from_slice(&bits.to_be_bytes());
        comm.extend_from_slice(&ext_44100());

        let mut ssnd = Vec::new();
        ssnd.extend_from_slice(&0u32.to_be_bytes()); // offset
        ssnd.extend_from_slice(&0u32.to_be_bytes()); // blockSize
        ssnd.extend_from_slice(data);

        let mut v = Vec::new();
        v.extend_from_slice(b"FORM");
        v.extend_from_slice(&((4 + 8 + comm.len() + 8 + ssnd.len()) as u32).to_be_bytes());
        v.extend_from_slice(b"AIFF");
        v.extend_from_slice(b"COMM");
        v.extend_from_slice(&(comm.len() as u32).to_be_bytes());
        v.extend_from_slice(&comm);
        v.extend_from_slice(b"SSND");
        v.extend_from_slice(&(ssnd.len() as u32).to_be_bytes());
        v.extend_from_slice(&ssnd);
        v
    }

    #[test]
    fn parses_pcm16_stream_info() {
        let bytes = aiff(2, 16, 2, &[0, 1, 0, 2, 0, 3, 0, 4]);
        let d = AiffDemuxer::new(SliceSource::new(&bytes)).unwrap();
        let si = d.stream_info();
        assert_eq!(si.sample_rate, 44_100);
        assert_eq!(si.channels, 2);
        assert_eq!(si.format, SampleFormat::Int { bits: 16 });
        assert_eq!(d.block_align(), 4);
        assert_eq!(d.frames_total(), Some(2));
        assert!(d.big_endian());
    }

    #[test]
    fn next_packet_yields_sample_bytes() {
        let data = [0, 1, 0, 2, 0, 3]; // 3 mono 16-bit frames
        let bytes = aiff(1, 16, 3, &data);
        let mut d = AiffDemuxer::new(SliceSource::new(&bytes)).unwrap();
        let mut scratch = [0u8; 16];
        let pkt = d.next_packet(&mut scratch).unwrap().unwrap();
        assert_eq!(pkt.data, &data);
        assert_eq!(pkt.dur, 3);
        assert!(d.next_packet(&mut scratch).unwrap().is_none());
    }

    #[test]
    fn skips_unknown_chunks_before_ssnd() {
        // Insert a FVER-like chunk of odd size before COMM; it must be skipped
        // with its pad byte or the following chunks desync.
        let data = [0, 7, 0, 9];
        let inner = aiff(1, 16, 2, &data);
        // Rebuild with an extra odd chunk after the FORM header.
        let mut v = Vec::new();
        v.extend_from_slice(&inner[0..12]); // FORM/size/AIFF (size now stale, unused by parser)
        v.extend_from_slice(b"FVER");
        v.extend_from_slice(&3u32.to_be_bytes());
        v.extend_from_slice(&[0xA2, 0x80, 0x51, 0x00]); // 3 body + 1 pad
        v.extend_from_slice(&inner[12..]);
        let d = AiffDemuxer::new(SliceSource::new(&v)).unwrap();
        assert_eq!(d.frames_total(), Some(2));
        assert_eq!(d.stream_info().channels, 1);
    }

    #[test]
    fn seek_repositions_by_frame() {
        let data: Vec<u8> = (0..20).collect(); // 10 mono 16-bit frames
        let bytes = aiff(1, 16, 10, &data);
        let mut d = AiffDemuxer::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(d.seek_to(3).unwrap(), 3);
        let mut scratch = [0u8; 2];
        let pkt = d.next_packet(&mut scratch).unwrap().unwrap();
        assert_eq!(pkt.data, &[6, 7]);
        assert_eq!(pkt.ts, 3);
    }

    #[test]
    fn rejects_non_form() {
        let mut bytes = aiff(1, 16, 1, &[0, 1]);
        bytes[0] = b'R';
        assert!(matches!(
            AiffDemuxer::new(SliceSource::new(&bytes)),
            Err(DecodeError::Malformed)
        ));
    }
}
