//! Minimal, forward-streaming MP4/M4A box reader for **faststart** files
//! (`moov` before `mdat`). It walks the box tree to the `stsd`→`alac` sample
//! entry (extracting the ALAC magic cookie), reads `mdhd` for the duration, and
//! stops at `mdat` with the source positioned on the audio. Structured so the
//! sample tables (`stsz`/`stco`, for seeking) and the `mp4a` entry (for AAC)
//! are additive.

use crate::decoder::DecodeError;
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

/// A parsed MP4 header positioned at `mdat` audio.
pub struct Mp4Reader<S> {
    src: S,
    config: AlacConfig,
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
        let mut config: Option<AlacConfig> = None;
        let mut total_samples: u64 = 0;
        loop {
            let (len, fourcc, _) = box_header(&mut src)?;
            match &fourcc {
                b"moov" => {
                    let (c, ts) = parse_moov(&mut src, len)?;
                    config = Some(c);
                    total_samples = ts;
                }
                b"mdat" => {
                    let config = config.ok_or(DecodeError::Unsupported)?; // moov must precede mdat
                    return Ok(Self {
                        src,
                        config,
                        total_samples,
                        mdat_remaining: len,
                    });
                }
                _ => skip(&mut src, len)?, // ftyp, free, udta, ...
            }
        }
    }

    pub fn config(&self) -> &AlacConfig {
        &self.config
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

/// Descend `moov`→`trak`→`mdia`→(`mdhd`,`minf`→`stbl`→`stsd`). Returns the ALAC
/// config and the total per-channel sample count.
///
/// Sibling order within `mdia` (and any other container walked here) is not
/// spec-mandated (ISO/IEC 14496-12), so every child is visited regardless of
/// position, and each visited box — matched or skipped — consumes exactly its
/// declared length. See [`descend_for`] for the same invariant applied to a
/// single "find one child" descent.
fn parse_moov<S: Source>(src: &mut S, len: u64) -> Result<(AlacConfig, u64), DecodeError> {
    descend_for(src, len, b"trak", |src, len| {
        descend_for(src, len, b"mdia", |src, len| {
            let mut total = 0u64;
            let mut config = None;
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
                        config = Some(descend_for(src, l, b"stbl", |src, len| {
                            descend_for(src, len, b"stsd", parse_stsd)
                        })?);
                    }
                    _ => skip(src, l)?,
                }
                consumed = consumed
                    .checked_add(box_total)
                    .ok_or(DecodeError::Malformed)?;
            }
            let config = config.ok_or(DecodeError::Unsupported)?;
            Ok((config, total))
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

/// `stsd`: version+flags(4) + entry_count(4), then the sample entry. For `alac`,
/// skip 8 (entry box header) + 28 (AudioSampleEntry body) + 12 (inner `alac`
/// FullBox header) to reach the 24-byte config.
fn parse_stsd<S: Source>(src: &mut S, len: u64) -> Result<AlacConfig, DecodeError> {
    let _vf_count: [u8; 8] = read_exact(src)?;
    let (_entry_len, fourcc, _h) = box_header(src)?;
    if &fourcc != b"alac" {
        return Err(DecodeError::Unsupported); // e.g. mp4a (AAC) — future
    }
    // AudioSampleEntry body (28) + inner alac FullBox header (12) = 40, then the
    // 24-byte config; then skip the remainder of the entry and stsd.
    skip(src, 28 + 12)?;
    let c: [u8; 24] = read_exact(src)?;
    // Skip whatever remains of stsd after the config.
    let read_so_far = 8u64 /*stsd vf+count*/ + 8 /*entry hdr*/ + 28 + 12 + 24;
    let after = len.checked_sub(read_so_far).ok_or(DecodeError::Malformed)?;
    skip(src, after)?;
    Ok(AlacConfig {
        frame_length: be32(&c[0..4]),
        bit_depth: c[5],
        pb: c[6],
        mb: c[7],
        kb: c[8],
        channels: c[9],
        max_frame_bytes: be32(&c[12..16]),
        sample_rate: be32(&c[20..24]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceSource;

    const ALAC: &[u8] = include_bytes!("../../testdata/alac.m4a");

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
