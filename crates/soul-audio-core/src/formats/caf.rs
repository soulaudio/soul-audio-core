//! Apple **CAF** (Core Audio Format) decoder. Supports the two common audio
//! payloads: **`lpcm`** (integer 8/16/24/32-bit, big- or little-endian, and IEEE
//! float — reusing the shared [`wav::convert_samples`](crate::formats::wav::convert_samples)
//! path) and **`alac`** (Apple Lossless — driving the shared ALAC
//! [`decode_packet`](crate::formats::alac::decode_packet) core over the `data`
//! chunk, with the `ALACSpecificConfig` from the `kuki` magic cookie).
//!
//! A CAF file is a 8-byte `caff` header then a sequence of chunks
//! (`[type:4][size:i64 BE][body]`). We read `desc` (the audio format) and, for
//! ALAC, `kuki` (the cookie), then stream the `data` chunk. Other chunks
//! (`chan`, `info`, `pakt`, …) are skipped. A `data` chunk may declare size −1
//! ("to end of file"); we then read until EOF.

use embedded_io::SeekFrom;

use crate::decoder::{DecodeError, Decoder};
use crate::formats::alac::{self, AlacCfg, PACKET_CAP, Scratch};
use crate::formats::wav::convert_samples;
use crate::io::{SeekSource, Source};
use crate::model::{SampleFormat, StreamInfo};

/// `kCAFLinearPCMFormatFlagIsFloat`.
const LPCM_FLAG_FLOAT: u32 = 0x1;
/// `kCAFLinearPCMFormatFlagIsLittleEndian`.
const LPCM_FLAG_LE: u32 = 0x2;

/// Parsed `desc` (CAFAudioFormat) chunk.
struct Desc {
    sample_rate: u32,
    format_id: [u8; 4],
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    channels: u32,
    bits_per_channel: u32,
}

/// Read a big-endian `u32`/`u64`/`i64` from a slice.
fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
fn be_i64(b: &[u8]) -> i64 {
    i64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// Read exactly `buf.len()` bytes or fail.
fn read_exact<S: Source>(src: &mut S, buf: &mut [u8]) -> Result<(), DecodeError> {
    let mut n = 0;
    while n < buf.len() {
        match src.read(&mut buf[n..]) {
            Ok(0) => return Err(DecodeError::UnexpectedEof),
            Ok(k) => n += k,
            Err(_) => return Err(DecodeError::Io),
        }
    }
    Ok(())
}

fn skip<S: Source>(src: &mut S, mut n: u64) -> Result<(), DecodeError> {
    let mut buf = [0u8; 256];
    while n > 0 {
        let want = n.min(buf.len() as u64) as usize;
        read_exact(src, &mut buf[..want])?;
        n -= want as u64;
    }
    Ok(())
}

/// Read a 12-byte chunk header (type + i64 size). `Ok(false)` at a clean EOF
/// (nothing read), so a chunk walk can stop when it runs off the end.
fn read_chunk_header<S: Source>(src: &mut S, buf: &mut [u8; 12]) -> Result<bool, DecodeError> {
    let mut n = 0;
    while n < 12 {
        match src.read(&mut buf[n..]) {
            Ok(0) => {
                if n == 0 {
                    return Ok(false);
                }
                return Err(DecodeError::UnexpectedEof);
            }
            Ok(k) => n += k,
            Err(_) => return Err(DecodeError::Io),
        }
    }
    Ok(true)
}

/// Parse the `ALACSpecificConfig` (24 bytes, big-endian) out of a CAF `kuki`
/// magic cookie — either the bare config or wrapped in `frma`/`alac` format
/// atoms (`[size:u32][type:4][body]`). Returns the config + sample rate.
fn parse_alac_cookie(kuki: &[u8]) -> Result<(AlacCfg, u32), DecodeError> {
    // Locate the 24-byte ALACSpecificConfig.
    let cfg = locate_alac_config(kuki).ok_or(DecodeError::Unsupported)?;
    let frame_length = be_u32(&cfg[0..4]) as usize;
    let bit_depth = u32::from(cfg[5]);
    let pb = u32::from(cfg[6]);
    let mb = u32::from(cfg[7]);
    let kb = u32::from(cfg[8]);
    let channels = usize::from(cfg[9]);
    let sample_rate = be_u32(&cfg[20..24]);
    Ok((
        AlacCfg {
            bit_depth,
            channels,
            pb,
            mb,
            kb,
            frame_length,
        },
        sample_rate,
    ))
}

/// Find the 24-byte ALACSpecificConfig in a `kuki` cookie. Walks format atoms
/// looking for an `alac` atom (its body is `[version/flags:4][config:24]`); if
/// the cookie isn't atom-structured, falls back to treating the first 24 bytes
/// as the bare config.
fn locate_alac_config(kuki: &[u8]) -> Option<&[u8]> {
    let mut p = 0usize;
    while p + 8 <= kuki.len() {
        let size = be_u32(&kuki[p..p + 4]) as usize;
        let ty = &kuki[p + 4..p + 8];
        // A plausible atom size; otherwise this isn't atom-structured.
        if size < 8 || p + size > kuki.len() {
            break;
        }
        if ty == b"alac" && size >= 8 + 4 + 24 {
            let body = &kuki[p + 8..p + size];
            return body.get(4..4 + 24);
        }
        p += size;
    }
    // Bare 24-byte config fallback.
    kuki.get(0..24)
}

/// Apple CAF decoder over a byte [`Source`].
///
/// The `Alac` variant is much larger than `Pcm` (it embeds the ALAC decode
/// scratch + packet buffer). Boxing is impossible under `#![no_std]`, so — like
/// [`AnyDecoder`](crate::dispatch::AnyDecoder) — the size gap is accepted.
#[allow(clippy::large_enum_variant)]
pub enum CafDecoder<S> {
    /// Linear PCM (`lpcm`).
    Pcm(CafPcm<S>),
    /// Apple Lossless (`alac`).
    Alac(CafAlac<S>),
}

impl<S: Source> CafDecoder<S> {
    /// Parse the CAF header + chunks and build the decoder for the `data` chunk.
    ///
    /// # Errors
    /// [`DecodeError::Malformed`] for a non-CAF / headerless file;
    /// [`DecodeError::Unsupported`] for a format other than `lpcm`/`alac`.
    pub fn new(mut src: S) -> Result<Self, DecodeError> {
        let mut hdr = [0u8; 8];
        read_exact(&mut src, &mut hdr)?;
        if &hdr[0..4] != b"caff" {
            return Err(DecodeError::Malformed);
        }
        // Walk chunks for `desc`, optional `kuki`, up to `data`. `pos` tracks the
        // absolute byte offset so the `data` body's start can be recorded for
        // seeking (the 8-byte `caff` header is already consumed).
        let mut desc: Option<Desc> = None;
        let mut kuki = [0u8; 64];
        let mut kuki_len = 0usize;
        let mut pos: u64 = 8;
        loop {
            let mut ch = [0u8; 12];
            read_exact(&mut src, &mut ch)?;
            pos += 12;
            let ty = [ch[0], ch[1], ch[2], ch[3]];
            let size = be_i64(&ch[4..12]);
            match &ty {
                b"desc" => {
                    if size < 32 {
                        return Err(DecodeError::Malformed);
                    }
                    let mut d = [0u8; 32];
                    read_exact(&mut src, &mut d)?;
                    skip(&mut src, size as u64 - 32)?;
                    pos += size as u64;
                    desc = Some(Desc {
                        sample_rate: f64_be(&d[0..8]) as u32,
                        format_id: [d[8], d[9], d[10], d[11]],
                        format_flags: be_u32(&d[12..16]),
                        bytes_per_packet: be_u32(&d[16..20]),
                        frames_per_packet: be_u32(&d[20..24]),
                        channels: be_u32(&d[24..28]),
                        bits_per_channel: be_u32(&d[28..32]),
                    });
                }
                b"kuki" => {
                    if size < 0 {
                        return Err(DecodeError::Malformed);
                    }
                    let take = (size as usize).min(kuki.len());
                    read_exact(&mut src, &mut kuki[..take])?;
                    skip(&mut src, size as u64 - take as u64)?;
                    pos += size as u64;
                    kuki_len = take;
                }
                b"data" => {
                    let desc = desc.ok_or(DecodeError::Malformed)?;
                    // The `data` body begins with a 4-byte edit count.
                    let mut edit = [0u8; 4];
                    read_exact(&mut src, &mut edit)?;
                    let data_start = pos + 4; // past the edit count = first sample byte
                    let data_bytes = if size < 0 {
                        u64::MAX // "to end of file"
                    } else {
                        (size as u64).saturating_sub(4)
                    };
                    return Self::build(src, desc, &kuki[..kuki_len], data_bytes, data_start);
                }
                _ => {
                    if size < 0 {
                        return Err(DecodeError::Malformed); // only `data` may be unsized
                    }
                    skip(&mut src, size as u64)?;
                    pos += size as u64;
                }
            }
        }
    }

    fn build(
        src: S,
        desc: Desc,
        kuki: &[u8],
        data_bytes: u64,
        data_start: u64,
    ) -> Result<Self, DecodeError> {
        match &desc.format_id {
            b"lpcm" => Ok(CafDecoder::Pcm(CafPcm::new(
                src, &desc, data_bytes, data_start,
            )?)),
            b"alac" => Ok(CafDecoder::Alac(CafAlac::new(
                src, kuki, data_bytes, data_start,
            )?)),
            _ => Err(DecodeError::Unsupported),
        }
    }

    #[must_use]
    pub fn total_frames(&self) -> Option<u64> {
        match self {
            CafDecoder::Pcm(d) => d.total_frames,
            CafDecoder::Alac(_) => None, // needs the `pakt` table — a follow-up
        }
    }
}

impl<S: SeekSource> CafDecoder<S> {
    /// Seek to per-channel `sample`: PCM is a direct byte offset; ALAC rewinds
    /// and decode-discards forward.
    ///
    /// # Errors
    /// Propagates the seek + forward-decode errors.
    pub fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        match self {
            CafDecoder::Pcm(d) => d.seek_to(sample),
            CafDecoder::Alac(d) => d.seek_to(sample),
        }
    }

    /// Seek to `secs` seconds (convenience over [`Self::seek_to`]).
    ///
    /// # Errors
    /// As [`Self::seek_to`].
    pub fn seek_to_time(&mut self, secs: f64) -> Result<u64, DecodeError> {
        let rate = self.stream_info().sample_rate.max(1);
        self.seek_to(crate::decoder::secs_to_samples(secs, rate))
    }
}

impl<S: Source> Decoder for CafDecoder<S> {
    fn stream_info(&self) -> StreamInfo {
        match self {
            CafDecoder::Pcm(d) => d.info,
            CafDecoder::Alac(d) => d.info,
        }
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        match self {
            CafDecoder::Pcm(d) => d.decode_into(out),
            CafDecoder::Alac(d) => d.decode_into(out),
        }
    }
}

/// `lpcm` CAF: stream the `data` region, convert each frame to `i32`.
pub struct CafPcm<S> {
    src: S,
    info: StreamInfo,
    total_frames: Option<u64>,
    bps: usize,
    bits: u8,
    float: bool,
    big_endian: bool,
    /// Interleaved-frame byte stride (`bytes_per_packet`), for the seek offset.
    block_align: u64,
    /// Absolute byte offset of the first sample, and the total sample-data size.
    data_start: u64,
    data_total: u64,
    data_remaining: u64,
}

impl<S: Source> CafPcm<S> {
    fn new(src: S, desc: &Desc, data_bytes: u64, data_start: u64) -> Result<Self, DecodeError> {
        if desc.channels == 0 || desc.frames_per_packet != 1 {
            return Err(DecodeError::Unsupported);
        }
        let float = desc.format_flags & LPCM_FLAG_FLOAT != 0;
        // CAF lpcm is big-endian unless the little-endian flag is set.
        let big_endian = desc.format_flags & LPCM_FLAG_LE == 0;
        let bits = desc.bits_per_channel as u8;
        let bps = (desc.bytes_per_packet / desc.channels) as usize;
        if bps == 0 {
            return Err(DecodeError::Unsupported);
        }
        let valid = if float {
            bits == 32 || bits == 64
        } else {
            bits != 0 && bits % 8 == 0 && bits <= 32
        };
        if !valid {
            return Err(DecodeError::Unsupported);
        }
        let block_align = desc.bytes_per_packet as u64;
        let total_frames = if data_bytes == u64::MAX {
            None
        } else {
            Some(data_bytes / block_align)
        };
        Ok(Self {
            src,
            info: StreamInfo {
                sample_rate: desc.sample_rate,
                channels: desc.channels as u16,
                format: if float {
                    SampleFormat::Int { bits: 32 }
                } else {
                    SampleFormat::Int { bits }
                },
            },
            total_frames,
            bps,
            bits,
            float,
            big_endian,
            block_align,
            data_start,
            data_total: if data_bytes == u64::MAX {
                u64::MAX
            } else {
                total_frames.unwrap() * block_align
            },
            data_remaining: if data_bytes == u64::MAX {
                u64::MAX
            } else {
                total_frames.unwrap() * block_align
            },
        })
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        let mut buf = [0u8; 768];
        let mut produced = 0;
        while produced < out.len() {
            let cap = (out.len() - produced).min(buf.len() / self.bps) * self.bps;
            if cap == 0 {
                break;
            }
            let want = (cap as u64).min(self.data_remaining) as usize;
            if want == 0 {
                break;
            }
            let mut n = 0;
            while n < want {
                match self.src.read(&mut buf[n..want]) {
                    Ok(0) => break,
                    Ok(k) => n += k,
                    Err(_) => return Err(DecodeError::Io),
                }
            }
            if n == 0 {
                break;
            }
            if self.data_remaining != u64::MAX {
                self.data_remaining -= n as u64;
            }
            let samples = n / self.bps;
            convert_samples(
                &buf[..n],
                self.bps,
                self.bits,
                self.float,
                None,
                self.big_endian,
                &mut out[produced..produced + samples],
            );
            produced += samples;
            if n < want {
                break;
            }
        }
        Ok(produced)
    }
}

impl<S: SeekSource> CafPcm<S> {
    /// Seek to per-channel `sample` — PCM is constant-rate, so this is a direct
    /// byte offset (clamped to the data region). Returns the frame landed on.
    ///
    /// # Errors
    /// [`DecodeError::Io`] if the underlying seek fails.
    fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        let frame = self.total_frames.map_or(sample, |t| sample.min(t));
        let byte = self.data_start + frame * self.block_align;
        self.src
            .seek(SeekFrom::Start(byte))
            .map_err(|_| DecodeError::Io)?;
        self.data_remaining = self.data_total.saturating_sub(frame * self.block_align);
        Ok(frame)
    }
}

/// `alac` CAF: drive the shared ALAC [`decode_packet`] core over the `data`
/// chunk's self-delimiting packets, with the `kuki` config.
pub struct CafAlac<S> {
    src: S,
    info: StreamInfo,
    cfg: AlacCfg,
    scratch: Scratch,
    inbuf: [u8; PACKET_CAP],
    filled: usize,
    used: usize,
    /// Absolute offset of the first packet + the total sample-data size (for the
    /// restart-based seek).
    data_start: u64,
    data_total: u64,
    data_remaining: u64,
    out_len: usize,
    out_pos: usize,
    eof: bool,
}

impl<S: Source> CafAlac<S> {
    fn new(src: S, kuki: &[u8], data_bytes: u64, data_start: u64) -> Result<Self, DecodeError> {
        let (cfg, sample_rate) = parse_alac_cookie(kuki)?;
        if cfg.channels == 0 || cfg.channels > alac::MAX_CHANNELS {
            return Err(DecodeError::Unsupported);
        }
        if cfg.frame_length == 0 || cfg.frame_length > alac::MAX_FRAME {
            return Err(DecodeError::Unsupported);
        }
        if !matches!(cfg.bit_depth, 16 | 20 | 24) || cfg.kb == 0 || cfg.kb > 31 {
            return Err(DecodeError::Unsupported);
        }
        Ok(Self {
            src,
            info: StreamInfo {
                sample_rate,
                channels: cfg.channels as u16,
                format: SampleFormat::Int {
                    bits: cfg.bit_depth as u8,
                },
            },
            cfg,
            scratch: Scratch::new(),
            inbuf: [0; PACKET_CAP],
            filled: 0,
            used: 0,
            data_start,
            data_total: data_bytes,
            data_remaining: data_bytes,
            out_len: 0,
            out_pos: 0,
            eof: false,
        })
    }

    fn decode_next_packet(&mut self) -> Result<bool, DecodeError> {
        // Slide out the previously-consumed bytes.
        if self.used > 0 {
            if self.used < self.filled {
                self.inbuf.copy_within(self.used..self.filled, 0);
                self.filled -= self.used;
            } else {
                self.filled = 0;
            }
            self.used = 0;
        }
        // Refill from the data region (bounded).
        while self.filled < self.inbuf.len() && self.data_remaining > 0 {
            let space = self.inbuf.len() - self.filled;
            // Bound the read to the data chunk (u64::MAX = "to end of file").
            let want = if self.data_remaining >= space as u64 {
                space
            } else {
                self.data_remaining as usize
            };
            let n = match self
                .src
                .read(&mut self.inbuf[self.filled..self.filled + want])
            {
                Ok(0) => break,
                Ok(k) => k,
                Err(_) => return Err(DecodeError::Io),
            };
            self.filled += n;
            if self.data_remaining != u64::MAX {
                self.data_remaining -= n as u64;
            }
        }
        if self.filled == 0 {
            self.eof = true;
            return Ok(false);
        }
        let (bytes, frames) =
            alac::decode_packet(&self.inbuf[..self.filled], &mut self.scratch, self.cfg)?;
        if frames == 0 {
            self.eof = true;
            return Ok(false);
        }
        self.used = bytes;
        self.out_len = frames * self.cfg.channels;
        self.out_pos = 0;
        Ok(true)
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        let mut written = 0;
        while written < out.len() {
            if self.out_pos >= self.out_len && (self.eof || !self.decode_next_packet()?) {
                break;
            }
            let avail = self.out_len - self.out_pos;
            let take = avail.min(out.len() - written);
            out[written..written + take]
                .copy_from_slice(&self.scratch.out[self.out_pos..self.out_pos + take]);
            self.out_pos += take;
            written += take;
        }
        Ok(written)
    }
}

impl<S: SeekSource> CafAlac<S> {
    /// Seek to per-channel `sample`. Fast path: the CAF `pakt` chunk lists each
    /// packet's byte size, and ALAC packets are a constant `frame_length` frames
    /// and independently decodable — so packet `k` begins at exactly
    /// `k * frame_length` output samples. We sum the byte sizes of the packets
    /// before the target packet (O(packets) tiny reads, no entropy decode), jump
    /// straight to that packet, decode it, and discard the within-packet head —
    /// sample-accurate with no rounding. Falls back to a linear decode-discard
    /// from the first packet when no `pakt` is reachable (e.g. a `data` chunk
    /// sized to EOF, or a non-standard chunk order). Returns the frame landed on.
    ///
    /// # Errors
    /// [`DecodeError::Io`] on seek failure, plus the forward-decode errors.
    fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        let fl = self.cfg.frame_length as u64;
        if fl != 0 {
            if let Some(offset) = self.pakt_offset_for_packet(sample / fl)? {
                self.src
                    .seek(SeekFrom::Start(self.data_start + offset))
                    .map_err(|_| DecodeError::Io)?;
                self.reset_for_seek();
                self.data_remaining = self.data_total.saturating_sub(offset);
                return self.discard_forward(sample / fl * fl, sample);
            }
        }
        // Fallback: rewind to the first packet and decode-discard forward.
        self.src
            .seek(SeekFrom::Start(self.data_start))
            .map_err(|_| DecodeError::Io)?;
        self.reset_for_seek();
        self.data_remaining = self.data_total;
        self.discard_forward(0, sample)
    }

    /// Reset the decode buffers/EOF flag after a seek repositions the source.
    fn reset_for_seek(&mut self) {
        self.scratch = Scratch::new();
        self.filled = 0;
        self.used = 0;
        self.out_len = 0;
        self.out_pos = 0;
        self.eof = false;
    }

    /// Decode and discard from a position whose first decoded sample is `base`
    /// up to `target`, returning the frame actually landed on (clamped at EOF).
    fn discard_forward(&mut self, base: u64, target: u64) -> Result<u64, DecodeError> {
        let ch = self.cfg.channels.max(1);
        let mut landed = base;
        while landed < target {
            if self.out_pos >= self.out_len && !self.decode_next_packet()? {
                break;
            }
            let avail = (self.out_len - self.out_pos) / ch;
            let take = avail.min((target - landed) as usize);
            self.out_pos += take * ch;
            landed += take as u64;
        }
        Ok(landed)
    }

    /// Sum the byte sizes of the first `target_packet` packets from the `pakt`
    /// chunk (located after the sized `data` chunk), giving the byte offset of
    /// that packet within the data region. Packet sizes are base-128 MSB-first
    /// variable-length integers. Returns `None` — so the caller falls back to a
    /// linear seek — when there is no reachable `pakt`, the `data` chunk runs to
    /// EOF, or `target_packet` is past the table. O(`target_packet`) small reads.
    fn pakt_offset_for_packet(&mut self, target_packet: u64) -> Result<Option<u64>, DecodeError> {
        if target_packet == 0 {
            return Ok(Some(0));
        }
        if self.data_total == u64::MAX {
            return Ok(None); // data runs to EOF — no pakt can follow
        }
        self.src
            .seek(SeekFrom::Start(self.data_start + self.data_total))
            .map_err(|_| DecodeError::Io)?;
        loop {
            let mut ch = [0u8; 12];
            if !read_chunk_header(&mut self.src, &mut ch)? {
                return Ok(None); // ran off the end without a pakt
            }
            let size = be_i64(&ch[4..12]);
            if &ch[0..4] == b"pakt" {
                if size < 24 {
                    return Ok(None);
                }
                let mut h = [0u8; 24];
                read_exact(&mut self.src, &mut h)?;
                let num_packets = be_i64(&h[0..8]);
                if num_packets < 0 || target_packet >= num_packets as u64 {
                    // At/past the last packet: `packet_index * frame_length` can
                    // exceed the true total (the last packet may be short), so let
                    // the linear path decode-discard and clamp to the real end.
                    return Ok(None);
                }
                let mut table_left = size as u64 - 24;
                let mut offset = 0u64;
                let mut cur = 0u64;
                let mut done = 0u64;
                let mut buf = [0u8; 256];
                while done < target_packet {
                    if table_left == 0 {
                        return Ok(None); // truncated size table
                    }
                    let want = table_left.min(buf.len() as u64) as usize;
                    read_exact(&mut self.src, &mut buf[..want])?;
                    table_left -= want as u64;
                    for &byte in &buf[..want] {
                        cur = (cur << 7) | u64::from(byte & 0x7f);
                        if byte & 0x80 == 0 {
                            offset += cur;
                            cur = 0;
                            done += 1;
                            if done == target_packet {
                                break;
                            }
                        }
                    }
                }
                return Ok(Some(offset));
            }
            if size < 0 {
                return Ok(None); // only `data` may be unsized; bail to linear
            }
            skip(&mut self.src, size as u64)?;
        }
    }
}

/// Read a big-endian IEEE `f64`.
fn f64_be(b: &[u8]) -> f64 {
    f64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locates_bare_and_wrapped_cookie() {
        // Bare 24-byte config: frameLength 4096, bitDepth 16, channels 2, rate 44100.
        let mut bare = [0u8; 24];
        bare[0..4].copy_from_slice(&4096u32.to_be_bytes());
        bare[5] = 16;
        bare[8] = 14; // kb
        bare[9] = 2;
        bare[20..24].copy_from_slice(&44_100u32.to_be_bytes());
        let (cfg, rate) = parse_alac_cookie(&bare).unwrap();
        assert_eq!(cfg.frame_length, 4096);
        assert_eq!(cfg.bit_depth, 16);
        assert_eq!(cfg.channels, 2);
        assert_eq!(rate, 44_100);

        // Wrapped: [frma atom][alac atom(version/flags + config)].
        let mut wrapped = Vec::new();
        wrapped.extend_from_slice(&12u32.to_be_bytes());
        wrapped.extend_from_slice(b"frma");
        wrapped.extend_from_slice(b"alac");
        wrapped.extend_from_slice(&(8u32 + 4 + 24).to_be_bytes());
        wrapped.extend_from_slice(b"alac");
        wrapped.extend_from_slice(&[0u8; 4]); // version/flags
        wrapped.extend_from_slice(&bare);
        let (cfg2, rate2) = parse_alac_cookie(&wrapped).unwrap();
        assert_eq!(cfg2.frame_length, 4096);
        assert_eq!(rate2, 44_100);
    }
}
