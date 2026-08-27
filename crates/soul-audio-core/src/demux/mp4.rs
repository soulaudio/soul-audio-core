//! Minimal, forward-streaming MP4/M4A box reader for **faststart** files
//! (`moov` before `mdat`). It walks the box tree to the `stsd` sample entry —
//! `alac` (extracting the ALAC magic cookie) or `mp4a` (descending into
//! `esds` for the AAC `AudioSpecificConfig`) — reads `mdhd` for the duration,
//! and stops at `mdat` with the source positioned on the audio. Structured so
//! the sample tables (`stsz`/`stco`, for seeking) are additive.

use crate::decoder::DecodeError;
use crate::demux::adts::AacConfig;
use crate::demux::{SAMPLE_RATES, channels_for};
use crate::io::Source;

/// The 24-byte `ALACSpecificConfig` from the `stsd`→`alac` cookie (big-endian on
/// disk; the fields we need for decode).
#[derive(Clone, Copy, Debug)]
pub struct AlacConfig {
    pub frame_length: u32,
    pub bit_depth: u8,
    pub pb: u8,
    pub mb: u8,
    pub kb: u8,
    pub channels: u8,
    pub sample_rate: u32,
    pub max_frame_bytes: u32,
}

/// Which audio codec the `stsd` sample entry described — determined once in
/// [`Mp4Reader::new`] and exposed via [`Mp4Reader::codec`] so a caller (e.g.
/// [`crate::dispatch::open`]) can branch to the matching decoder.
#[derive(Clone, Copy, Debug)]
pub enum Mp4Codec {
    /// Apple Lossless — the `stsd`→`alac` magic cookie.
    Alac(AlacConfig),
    /// AAC — the `stsd`→`mp4a`→`esds` `AudioSpecificConfig`.
    Aac(AacConfig),
}

/// A parsed MP4 header positioned at `mdat` audio.
pub struct Mp4Reader<S> {
    src: S,
    codec: Mp4Codec,
    total_samples: u64,
    mdat_remaining: u64,
}

/// Read exactly `N` bytes or error [`DecodeError::Malformed`].
fn read_exact<S: Source, const N: usize>(src: &mut S) -> Result<[u8; N], DecodeError> {
    let mut buf = [0u8; N];
    src.read_exact(&mut buf)
        .map_err(|_| DecodeError::Malformed)?;
    Ok(buf)
}

/// Discard `n` bytes from a forward source.
fn skip<S: Source>(src: &mut S, mut n: u64) -> Result<(), DecodeError> {
    let mut scratch = [0u8; 512];
    while n > 0 {
        let want = n.min(scratch.len() as u64) as usize;
        let got = src
            .read(&mut scratch[..want])
            .map_err(|_| DecodeError::Io)?;
        if got == 0 {
            return Err(DecodeError::Malformed);
        }
        n -= got as u64;
    }
    Ok(())
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// Read a box header: returns `(content_len, fourcc, header_len)`. `content_len`
/// excludes the header. Handles 64-bit sizes (`size == 1`).
fn box_header<S: Source>(src: &mut S) -> Result<(u64, [u8; 4], u64), DecodeError> {
    let hdr: [u8; 8] = read_exact(src)?;
    let size32 = be32(&hdr[0..4]);
    let fourcc = [hdr[4], hdr[5], hdr[6], hdr[7]];
    if size32 == 1 {
        let big: [u8; 8] = read_exact(src)?;
        let size = be64(&big);
        Ok((size.saturating_sub(16), fourcc, 16))
    } else if size32 == 0 {
        // "to end of file" — not expected before mdat in faststart.
        Err(DecodeError::Unsupported)
    } else {
        Ok((u64::from(size32).saturating_sub(8), fourcc, 8))
    }
}

impl<S: Source> Mp4Reader<S> {
    /// Parse the header of a faststart ALAC `.m4a`, leaving `src` at `mdat`.
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] for non-faststart / non-ALAC / missing boxes;
    /// [`DecodeError::Malformed`] on truncation.
    pub fn new(mut src: S) -> Result<Self, DecodeError> {
        let mut codec: Option<Mp4Codec> = None;
        let mut total_samples: u64 = 0;
        loop {
            let (len, fourcc, _) = box_header(&mut src)?;
            match &fourcc {
                b"moov" => {
                    let (c, ts) = parse_moov(&mut src, len)?;
                    codec = Some(c);
                    total_samples = ts;
                }
                b"mdat" => {
                    let codec = codec.ok_or(DecodeError::Unsupported)?; // moov must precede mdat
                    return Ok(Self {
                        src,
                        codec,
                        total_samples,
                        mdat_remaining: len,
                    });
                }
                _ => skip(&mut src, len)?, // ftyp, free, udta, ...
            }
        }
    }

    /// The codec the `stsd` sample entry described (`Alac` or `Aac`).
    pub fn codec(&self) -> Mp4Codec {
        self.codec
    }

    /// The ALAC config, for [`crate::formats::alac::AlacDecoder`] — which
    /// only ever constructs a reader over an ALAC `.m4a` and consumes this
    /// directly. If the sample entry actually described AAC (`mp4a`), this
    /// returns a zeroed placeholder rather than panicking: every field
    /// `AlacDecoder::new` validates (`channels`, `frame_length`, `kb`, …) is
    /// `0` there, so construction cleanly fails `Unsupported` instead of
    /// misreading absent ALAC data as a real cookie. New code should prefer
    /// [`Self::codec`].
    pub fn config(&self) -> &AlacConfig {
        const ZERO: AlacConfig = AlacConfig {
            frame_length: 0,
            bit_depth: 0,
            pb: 0,
            mb: 0,
            kb: 0,
            channels: 0,
            sample_rate: 0,
            max_frame_bytes: 0,
        };
        match &self.codec {
            Mp4Codec::Alac(c) => c,
            Mp4Codec::Aac(_) => &ZERO,
        }
    }
    pub fn total_samples(&self) -> u64 {
        self.total_samples
    }
    pub fn mdat_remaining(&self) -> u64 {
        self.mdat_remaining
    }
    pub fn src_mut(&mut self) -> &mut S {
        &mut self.src
    }
}

/// Descend `moov`→`trak`→`mdia`→(`mdhd`,`minf`→`stbl`→`stsd`). Returns the
/// parsed codec ([`Mp4Codec::Alac`] or [`Mp4Codec::Aac`]) and the total
/// per-channel sample count.
///
/// Sibling order within `mdia` (and any other container walked here) is not
/// spec-mandated (ISO/IEC 14496-12), so every child is visited regardless of
/// position, and each visited box — matched or skipped — consumes exactly its
/// declared length. See [`descend_for`] for the same invariant applied to a
/// single "find one child" descent.
fn parse_moov<S: Source>(src: &mut S, len: u64) -> Result<(Mp4Codec, u64), DecodeError> {
    descend_for(src, len, b"trak", |src, len| {
        descend_for(src, len, b"mdia", |src, len| {
            let mut total = 0u64;
            let mut codec = None;
            let mut consumed = 0u64;
            loop {
                let remaining = len.checked_sub(consumed).ok_or(DecodeError::Malformed)?;
                if remaining < 8 {
                    // Trailing padding (if any) belongs to this container; the
                    // caller's own descend_for will drain past `len` for us,
                    // but leave nothing unread within our own bound.
                    if remaining > 0 {
                        skip(src, remaining)?;
                    }
                    break;
                }
                let (l, f, box_total) = child_header(src, remaining)?;
                match &f {
                    b"mdhd" => total = parse_mdhd(src, l)?,
                    b"minf" => {
                        codec = Some(descend_for(src, l, b"stbl", |src, len| {
                            descend_for(src, len, b"stsd", parse_stsd)
                        })?);
                    }
                    _ => skip(src, l)?,
                }
                consumed = consumed
                    .checked_add(box_total)
                    .ok_or(DecodeError::Malformed)?;
            }
            let codec = codec.ok_or(DecodeError::Unsupported)?;
            Ok((codec, total))
        })
    })
}

/// Read one child box header bounded by `remaining` bytes left in the parent
/// container. Returns `(content_len, fourcc, total_len)` where `total_len` is
/// the header plus content and is guaranteed `<= remaining`; a declared size
/// that would run past the parent's bound is `Malformed`, never subtracted
/// into a wrapping/underflowing value.
fn child_header<S: Source>(
    src: &mut S,
    remaining: u64,
) -> Result<(u64, [u8; 4], u64), DecodeError> {
    let (l, fourcc, h) = box_header(src)?;
    let total = l.checked_add(h).ok_or(DecodeError::Malformed)?;
    if total > remaining {
        return Err(DecodeError::Malformed);
    }
    Ok((l, fourcc, total))
}

/// Read boxes within `[0,len)` until one matches `want`, then run `f` on its
/// content; skip others. `f` must consume exactly the content length it is
/// given — this function then drains any bytes remaining in `[0,len)` after
/// `f` returns (trailing siblings the match didn't need), so it always
/// consumes exactly `len` total regardless of where `want` sits among its
/// siblings, or whether the code that matched it under-read. Errors
/// `Unsupported` if `want` is not found; `Malformed` on any short/overrunning
/// declared size.
fn descend_for<S, R>(
    src: &mut S,
    len: u64,
    want: &[u8; 4],
    f: impl FnOnce(&mut S, u64) -> Result<R, DecodeError>,
) -> Result<R, DecodeError>
where
    S: Source,
{
    let mut consumed = 0u64;
    loop {
        let remaining = len.checked_sub(consumed).ok_or(DecodeError::Malformed)?;
        if remaining < 8 {
            if remaining > 0 {
                skip(src, remaining)?;
            }
            return Err(DecodeError::Unsupported);
        }
        let (l, fourcc, box_total) = child_header(src, remaining)?;
        if &fourcc == want {
            let result = f(src, l)?;
            // `f` is required to have consumed exactly `l`; drain whatever is
            // left of this container so the caller sees exactly `len` gone.
            let after = remaining
                .checked_sub(box_total)
                .ok_or(DecodeError::Malformed)?;
            skip(src, after)?;
            return Ok(result);
        }
        skip(src, l)?;
        consumed = consumed
            .checked_add(box_total)
            .ok_or(DecodeError::Malformed)?;
    }
}

/// `mdhd`: version(1)+flags(3), then (v0) creation/mod u32 + timescale u32 +
/// duration u32, or (v1) u64 times + timescale u32 + duration u64. Returns the
/// duration in timescale units (== per-channel sample count for audio).
fn parse_mdhd<S: Source>(src: &mut S, len: u64) -> Result<u64, DecodeError> {
    let head: [u8; 4] = read_exact(src)?;
    let version = head[0];
    if version == 1 {
        let b: [u8; 28] = read_exact(src)?; // 8+8 times, 4 timescale, 8 duration
        let duration = be64(&b[20..28]);
        let remainder = len.checked_sub(4 + 28).ok_or(DecodeError::Malformed)?;
        skip(src, remainder)?;
        Ok(duration)
    } else {
        let b: [u8; 16] = read_exact(src)?; // 4+4 times, 4 timescale, 4 duration
        let duration = u64::from(be32(&b[12..16]));
        let remainder = len.checked_sub(4 + 16).ok_or(DecodeError::Malformed)?;
        skip(src, remainder)?;
        Ok(duration)
    }
}

/// `stsd`: version+flags(4) + entry_count(4), then the sample entry. Both
/// `alac` and `mp4a` share an 8-byte entry box header + 28-byte
/// `AudioSampleEntry` body; what follows differs:
/// - `alac`: a 12-byte inner `alac` `FullBox` header, then the 24-byte cookie.
/// - `mp4a`: an `esds` box carrying the AAC `AudioSpecificConfig` (see
///   [`parse_esds`]).
///
/// Any other sample entry is `Unsupported`. The entry box's declared length is
/// checked against `stsd`'s own bound (via [`child_header`]), and each branch
/// consumes exactly that declared length before this function drains
/// whatever remains of `stsd`.
fn parse_stsd<S: Source>(src: &mut S, len: u64) -> Result<Mp4Codec, DecodeError> {
    let _vf_count: [u8; 8] = read_exact(src)?;
    let remaining = len.checked_sub(8).ok_or(DecodeError::Malformed)?;
    let (entry_len, fourcc, entry_total) = child_header(src, remaining)?;
    let codec = if &fourcc == b"alac" {
        // AudioSampleEntry body (28) + inner alac FullBox header (12) = 40,
        // then the 24-byte cookie, then whatever padding remains of the entry.
        let trailer = entry_len
            .checked_sub(28 + 12 + 24)
            .ok_or(DecodeError::Malformed)?;
        skip(src, 28 + 12)?;
        let c: [u8; 24] = read_exact(src)?;
        skip(src, trailer)?;
        Mp4Codec::Alac(AlacConfig {
            frame_length: be32(&c[0..4]),
            bit_depth: c[5],
            pb: c[6],
            mb: c[7],
            kb: c[8],
            channels: c[9],
            max_frame_bytes: be32(&c[12..16]),
            sample_rate: be32(&c[20..24]),
        })
    } else if &fourcc == b"mp4a" {
        // Same 28-byte AudioSampleEntry body, then an esds box (instead of an
        // inner alac box).
        let after_preamble = entry_len.checked_sub(28).ok_or(DecodeError::Malformed)?;
        skip(src, 28)?;
        let (esds_len, esds_fourcc, esds_total) = child_header(src, after_preamble)?;
        if &esds_fourcc != b"esds" {
            return Err(DecodeError::Unsupported);
        }
        let asc = parse_esds(src, esds_len)?;
        let trailer = after_preamble
            .checked_sub(esds_total)
            .ok_or(DecodeError::Malformed)?;
        skip(src, trailer)?;
        Mp4Codec::Aac(asc)
    } else {
        return Err(DecodeError::Unsupported); // some other sample entry
    };
    let after_entry = remaining
        .checked_sub(entry_total)
        .ok_or(DecodeError::Malformed)?;
    skip(src, after_entry)?;
    Ok(codec)
}

/// Read one MPEG-4 descriptor header: a 1-byte tag followed by an
/// **expandable length** (each byte's low 7 bits shift in; the chain
/// continues while the byte's top bit is set — capped at 4 length bytes per
/// ISO/IEC 14496-1, so a runaway continuation chain is `Malformed` rather
/// than looping unboundedly). Returns `(tag, content_len, header_len)`, where
/// `header_len` (tag + length bytes) and `content_len` are both checked
/// against `remaining` — the bytes left in the enclosing descriptor/box — so
/// a truncated or oversized descriptor is `Malformed`, never an OOB read.
fn descriptor_header<S: Source>(
    src: &mut S,
    remaining: u64,
) -> Result<(u8, u64, u64), DecodeError> {
    if remaining < 2 {
        return Err(DecodeError::Malformed);
    }
    let tag: [u8; 1] = read_exact(src)?;
    let mut header_len = 1u64;
    let mut size: u64 = 0;
    loop {
        if header_len >= remaining || header_len > 4 {
            return Err(DecodeError::Malformed);
        }
        let b: [u8; 1] = read_exact(src)?;
        header_len += 1;
        size = (size << 7) | u64::from(b[0] & 0x7F);
        if b[0] & 0x80 == 0 {
            break;
        }
    }
    let total = header_len.checked_add(size).ok_or(DecodeError::Malformed)?;
    if total > remaining {
        return Err(DecodeError::Malformed);
    }
    Ok((tag[0], size, header_len))
}

/// `esds`: a `FullBox` (version+flags, 4 bytes, skipped) wrapping the MPEG-4
/// descriptor chain down to the `AudioSpecificConfig` bytes:
/// `ES_Descriptor` (tag `0x03`) → `DecoderConfigDescriptor` (tag `0x04`) →
/// `DecoderSpecificInfo` (tag `0x05`) = the ASC. See `aac-reference.md` §2
/// for the descriptor field layout. Every descriptor's length is
/// bounds-checked (via [`descriptor_header`]) against the bytes left in its
/// enclosing descriptor, so a truncated/runaway chain is `Malformed`, never a
/// panic or OOB read. Consumes exactly `len` bytes.
fn parse_esds<S: Source>(src: &mut S, len: u64) -> Result<AacConfig, DecodeError> {
    let _version_flags: [u8; 4] = read_exact(src)?;
    let mut pos = 4u64;

    let box_left = len.checked_sub(pos).ok_or(DecodeError::Malformed)?;
    let (tag, es_len, es_hdr) = descriptor_header(src, box_left)?;
    if tag != 0x03 {
        return Err(DecodeError::Unsupported);
    }
    pos = pos.checked_add(es_hdr).ok_or(DecodeError::Malformed)?;

    // ES_ID(16) + flags(8: streamDependenceFlag/URL_flag/OCRstreamFlag/streamPriority(5)).
    let mut es_left = es_len.checked_sub(3).ok_or(DecodeError::Malformed)?;
    let es_fixed: [u8; 3] = read_exact(src)?;
    pos += 3;
    let flags = es_fixed[2];

    if flags & 0x80 != 0 {
        // dependsOn_ES_ID
        es_left = es_left.checked_sub(2).ok_or(DecodeError::Malformed)?;
        skip(src, 2)?;
        pos += 2;
    }
    if flags & 0x40 != 0 {
        // URL_flag: URLlength(8) + URL bytes
        es_left = es_left.checked_sub(1).ok_or(DecodeError::Malformed)?;
        let url_len: [u8; 1] = read_exact(src)?;
        pos += 1;
        let ul = u64::from(url_len[0]);
        es_left = es_left.checked_sub(ul).ok_or(DecodeError::Malformed)?;
        skip(src, ul)?;
        pos += ul;
    }
    if flags & 0x20 != 0 {
        // OCRstreamFlag
        es_left = es_left.checked_sub(2).ok_or(DecodeError::Malformed)?;
        skip(src, 2)?;
        pos += 2;
    }

    let (tag, dc_len, dc_hdr) = descriptor_header(src, es_left)?;
    if tag != 0x04 {
        return Err(DecodeError::Unsupported);
    }
    pos = pos.checked_add(dc_hdr).ok_or(DecodeError::Malformed)?;

    // objectTypeIndication(8) + streamType/upStream/reserved(8) +
    // bufferSizeDB(24) + maxBitrate(32) + avgBitrate(32) = 13 fixed bytes.
    let dc_left = dc_len.checked_sub(13).ok_or(DecodeError::Malformed)?;
    skip(src, 13)?;
    pos += 13;

    let (tag, di_len, di_hdr) = descriptor_header(src, dc_left)?;
    if tag != 0x05 {
        return Err(DecodeError::Unsupported);
    }
    pos = pos.checked_add(di_hdr).ok_or(DecodeError::Malformed)?;

    // The ASC fields this reader needs (audioObjectType[+ext],
    // samplingFrequencyIndex[+explicit rate], channelConfiguration) fit in
    // well under 8 bytes; a longer DecoderSpecificInfo (SBR/PS extension,
    // GASpecificConfig tail) is read up to that cap and the remainder is
    // skipped unparsed.
    const ASC_CAP: usize = 8;
    let take = (di_len as usize).min(ASC_CAP);
    let mut asc = [0u8; ASC_CAP];
    src.read_exact(&mut asc[..take])
        .map_err(|_| DecodeError::Malformed)?;
    pos = pos.checked_add(take as u64).ok_or(DecodeError::Malformed)?;
    let config = parse_audio_specific_config(&asc[..take])?;

    let remainder = len.checked_sub(pos).ok_or(DecodeError::Malformed)?;
    skip(src, remainder)?;
    Ok(config)
}

/// MSB-first, bounds-checked bit reader over a fixed byte slice — used only
/// for the handful of `AudioSpecificConfig` fields [`parse_audio_specific_config`]
/// needs. Unlike `formats::alac`'s `BitReader` (which zero-pads past-the-end
/// reads to tolerate a truncated final audio frame), reading past the end
/// here is `Malformed`: this is config data, not audio payload, and a value
/// derived from an out-of-range read must not silently look valid.
struct AscBits<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> AscBits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    /// Read `n` (≤ 24) bits, MSB-first.
    fn read(&mut self, n: u32) -> Result<u32, DecodeError> {
        let mut v: u32 = 0;
        for _ in 0..n {
            let byte = self.bit_pos >> 3;
            let bit = 7 - (self.bit_pos & 7);
            let b = *self.data.get(byte).ok_or(DecodeError::Malformed)?;
            v = (v << 1) | u32::from((b >> bit) & 1);
            self.bit_pos += 1;
        }
        Ok(v)
    }
}

/// Parse the `AudioSpecificConfig` fields this decoder needs:
/// `audioObjectType` (5 bits; `== 31` escapes to `32 + 6` more bits),
/// `samplingFrequencyIndex` (4 bits; `== 15` escapes to a 24-bit explicit
/// rate), `channelConfiguration` (4 bits). The trailing `GASpecificConfig`
/// bits (`frameLengthFlag`/`dependsOnCoreCoder`/`extensionFlag`) aren't
/// needed for AAC-LC container detection and are left unread. See
/// `aac-reference.md` §2.
fn parse_audio_specific_config(data: &[u8]) -> Result<AacConfig, DecodeError> {
    let mut r = AscBits::new(data);
    let mut object_type = r.read(5)?;
    if object_type == 31 {
        object_type = 32 + r.read(6)?;
    }
    let sfi = r.read(4)?;
    let sample_rate = if sfi == 15 {
        r.read(24)?
    } else {
        *SAMPLE_RATES
            .get(sfi as usize)
            .ok_or(DecodeError::Malformed)?
    };
    let channel_config = r.read(4)?;
    let channels = channels_for(channel_config)?;
    Ok(AacConfig {
        object_type: object_type as u8,
        sample_rate,
        channels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceSource;

    const ALAC: &[u8] = include_bytes!("../../testdata/alac.m4a");

    #[test]
    fn parses_mp4a_esds_asc() {
        let m4a = include_bytes!("../../testdata/aac.m4a");
        let r = Mp4Reader::new(crate::io::SliceSource::new(m4a)).unwrap();
        match r.codec() {
            Mp4Codec::Aac(c) => {
                assert_eq!(c.object_type, 2);
                assert_eq!(c.sample_rate, 44_100);
                assert_eq!(c.channels, 2);
            }
            Mp4Codec::Alac(_) => panic!("expected AAC"),
        }
    }

    #[test]
    fn parses_cookie_and_locates_mdat() {
        let r = Mp4Reader::new(SliceSource::new(ALAC)).expect("parse mp4");
        let c = r.config();
        assert_eq!(c.frame_length, 4096);
        assert_eq!(c.bit_depth, 16);
        assert_eq!(c.channels, 2);
        assert_eq!(c.sample_rate, 44_100);
        assert_eq!(c.pb, 40);
        assert_eq!(c.mb, 10);
        assert_eq!(c.kb, 14);
        assert_eq!(r.total_samples(), 13_230);
        // mdat holds the sum of the frame byte-sizes (17411 for this file).
        assert_eq!(r.mdat_remaining(), 17_411);
    }

    /// Read one 32-bit-size box header at `off` in a byte slice (test-only;
    /// mirrors `box_header`'s 32-bit path — the fixture never uses 64-bit
    /// sizes). Returns `(total_len_incl_header, fourcc)`.
    fn box_at(data: &[u8], off: usize) -> (usize, [u8; 4]) {
        let size = u32::from_be_bytes(data[off..off + 4].try_into().unwrap());
        let fourcc = [data[off + 4], data[off + 5], data[off + 6], data[off + 7]];
        assert!(size as usize >= 8, "box at {off} has an unsupported size");
        (size as usize, fourcc)
    }

    /// Find a direct child box named `want` within `[start,end)`; panics if
    /// absent (test helper — the fixture is expected to contain it). Returns
    /// `(offset, total_len_incl_header)`.
    fn find_child(data: &[u8], start: usize, end: usize, want: &[u8; 4]) -> (usize, usize) {
        let mut i = start;
        while i + 8 <= end {
            let (total, fourcc) = box_at(data, i);
            if &fourcc == want {
                return (i, total);
            }
            i += total;
        }
        panic!(
            "box {:?} not found in [{start},{end})",
            core::str::from_utf8(want)
        );
    }

    /// Swap two (possibly differently-sized, possibly non-adjacent) byte
    /// ranges in place, keeping everything outside and between them
    /// untouched. Used to reorder sibling boxes without touching any size
    /// field.
    fn swap_ranges(data: &mut Vec<u8>, a: (usize, usize), b: (usize, usize)) {
        let (a, b) = if a.0 <= b.0 { (a, b) } else { (b, a) };
        let (a_start, a_len) = a;
        let (b_start, b_len) = b;
        let a_end = a_start + a_len;
        let b_end = b_start + b_len;
        assert!(a_end <= b_start, "ranges must be disjoint and ordered");

        let mut result = Vec::with_capacity(data.len());
        result.extend_from_slice(&data[..a_start]);
        result.extend_from_slice(&data[b_start..b_end]); // b's bytes, where a was
        result.extend_from_slice(&data[a_end..b_start]); // untouched middle
        result.extend_from_slice(&data[a_start..a_end]); // a's bytes, where b was
        result.extend_from_slice(&data[b_end..]);
        *data = result;
    }

    /// Regression test for the box-tree walker panic: builds a variant of the
    /// golden fixture with `mdia`'s children reordered from
    /// `[mdhd, hdlr, minf]` to `[mdhd, minf, hdlr]` — legal per ISO/IEC
    /// 14496-12 (no mandated sibling order) — by swapping the `hdlr`/`minf`
    /// byte ranges wholesale (sizes untouched, so every ancestor's declared
    /// length still covers exactly the same bytes). Walks box headers
    /// generically (no hardcoded offsets) so it stays valid if the fixture is
    /// regenerated. Before the fix, `minf` was not `mdia`'s last child, its
    /// nested `stbl`→`stsd` match under-consumed relative to `mdia`'s
    /// declared child sizes, and the next `box_header` read landed inside
    /// leftover `stbl` sibling data, underflowing `remaining -= l + h` and
    /// panicking (`attempt to subtract with overflow`).
    #[test]
    fn parses_reordered_mdia_children() {
        let mut data = ALAC.to_vec();

        let (moov_off, moov_size) = find_child(&data, 0, data.len(), b"moov");
        let (moov_content, moov_end) = (moov_off + 8, moov_off + moov_size);

        let (trak_off, trak_size) = find_child(&data, moov_content, moov_end, b"trak");
        let (trak_content, trak_end) = (trak_off + 8, trak_off + trak_size);

        let (mdia_off, mdia_size) = find_child(&data, trak_content, trak_end, b"mdia");
        let (mdia_content, mdia_end) = (mdia_off + 8, mdia_off + mdia_size);

        let hdlr = find_child(&data, mdia_content, mdia_end, b"hdlr");
        let minf = find_child(&data, mdia_content, mdia_end, b"minf");
        swap_ranges(&mut data, hdlr, minf);

        let r = Mp4Reader::new(SliceSource::new(&data)).expect("parse reordered mp4");
        let c = r.config();
        assert_eq!(c.frame_length, 4096);
        assert_eq!(c.bit_depth, 16);
        assert_eq!(c.channels, 2);
        assert_eq!(c.sample_rate, 44_100);
        assert_eq!(r.total_samples(), 13_230);
        assert_eq!(r.mdat_remaining(), 17_411);
    }
}
