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
use embedded_io::SeekFrom;

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
    /// Gapless encoder delay (leading priming, per-channel samples) from the
    /// `elst` edit list — `0` when the file has no edit list. Exposed for a
    /// player to trim; the decoders do not apply it themselves (decode output
    /// is the full `mdhd` span).
    encoder_delay: u64,
    /// Gapless presented length (per-channel samples to emit after the delay)
    /// from the `elst` `segment_duration`; `None` when no edit list. The player
    /// trims trailing padding down to this.
    present_frames: Option<u64>,
    /// Bytes left in the **current** `mdat`. In a fragmented file this is one
    /// fragment's `mdat`; [`Mp4Reader::read_audio`] advances to the next
    /// fragment's `mdat` (skipping its `moof`) when it hits 0.
    mdat_remaining: u64,
    /// `true` for a fragmented stream (`moof`/`mdat` fragments after the init
    /// `moov`): the sample data spans multiple `mdat`s, and the `moov` carries no
    /// sample tables (so seeking — which needs them — is unsupported) or total.
    fragmented: bool,
    /// Located lazily on the first seek (needs `S: SeekSource`); `None` until
    /// then so the forward-only decode-open path pays nothing.
    tables: Option<SampleTableLoc>,
}

/// Absolute file offsets + lengths of the `stbl` sample-table boxes needed to
/// map a coded-frame index to a file offset, located on the first seek. All
/// `Copy` (a few integers) — no heap, per the crate's no-alloc rule.
#[derive(Clone, Copy, Debug)]
struct SampleTableLoc {
    /// `stsc` (sample-to-chunk) content `(offset, len)`.
    stsc: (u64, u64),
    /// `stsz` (sample sizes) content `(offset, len)`.
    stsz: (u64, u64),
    /// `stco`/`co64` (chunk offsets) content `(offset, len)`.
    stco: (u64, u64),
    /// `true` when the chunk-offset box was `co64` (64-bit offsets).
    co64: bool,
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
    /// Parse the header of an ALAC/AAC `.m4a`, leaving `src` at the first
    /// `mdat`. Handles both progressive (`moov` then one `mdat`) and
    /// **fragmented** (`moov` then `moof`/`mdat` fragments) layouts; a `moof`
    /// seen before the first `mdat` flags the stream fragmented, and
    /// [`Mp4Reader::read_audio`] then spans the fragments' `mdat`s.
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] for a `moov`-after-`mdat` (non-faststart,
    /// non-fragmented) layout or missing/unknown boxes; [`DecodeError::Malformed`]
    /// on truncation.
    pub fn new(mut src: S) -> Result<Self, DecodeError> {
        let mut codec: Option<Mp4Codec> = None;
        let mut total_samples: u64 = 0;
        let mut encoder_delay: u64 = 0;
        let mut present_frames: Option<u64> = None;
        let mut fragmented = false;
        loop {
            let (len, fourcc, _) = box_header(&mut src)?;
            match &fourcc {
                b"moov" => {
                    let info = parse_moov(&mut src, len)?;
                    codec = Some(info.codec);
                    total_samples = info.total_samples;
                    encoder_delay = info.encoder_delay;
                    present_frames = info.present_frames;
                }
                // A movie fragment before the first `mdat`: the sample data is
                // split across `moof`/`mdat` fragments (DASH/CMAF/streaming).
                b"moof" => {
                    fragmented = true;
                    skip(&mut src, len)?;
                }
                b"mdat" => {
                    let codec = codec.ok_or(DecodeError::Unsupported)?; // moov must precede mdat
                    return Ok(Self {
                        src,
                        codec,
                        total_samples,
                        encoder_delay,
                        present_frames,
                        mdat_remaining: len,
                        fragmented,
                        tables: None,
                    });
                }
                _ => skip(&mut src, len)?, // ftyp, free, udta, ...
            }
        }
    }

    /// `true` if this is a fragmented stream (sample data in `moof`/`mdat`
    /// fragments). Fragmented streams carry no `moov` sample tables, so seeking
    /// is unsupported and the declared total length is unknown.
    pub fn is_fragmented(&self) -> bool {
        self.fragmented
    }

    /// Read up to `buf.len()` bytes of coded audio from the `mdat` stream,
    /// transparently spanning fragments: when the current `mdat` is exhausted in
    /// a fragmented file, skip the following `moof` (and any other non-`mdat`
    /// boxes) to the next `mdat` and continue. Returns the number of bytes read;
    /// `0` at the true end of the sample data. For a progressive file this is
    /// just a bounded read of the single `mdat` (identical to the former
    /// `src.read` + `mdat_remaining` decrement the codecs did inline).
    pub fn read_audio(&mut self, buf: &mut [u8]) -> Result<usize, DecodeError> {
        let mut filled = 0;
        while filled < buf.len() {
            if self.mdat_remaining == 0 {
                if self.fragmented && self.advance_to_next_mdat()? {
                    continue;
                }
                break; // progressive EOF, or no more fragments
            }
            let want = ((buf.len() - filled) as u64).min(self.mdat_remaining) as usize;
            let n = self
                .src
                .read(&mut buf[filled..filled + want])
                .map_err(|_| DecodeError::Io)?;
            if n == 0 {
                break;
            }
            filled += n;
            self.mdat_remaining -= n as u64;
        }
        Ok(filled)
    }

    /// Skip boxes (a `moof` and anything else that isn't audio) until the next
    /// `mdat`, setting [`Self::mdat_remaining`] to its length. Returns `false`
    /// at the end of the stream (no further `mdat`). A truncated box between
    /// fragments ends the stream cleanly rather than erroring: a partially
    /// received fragmented stream degrades to the audio decoded so far, the same
    /// graceful end the old per-`mdat` `read() == 0` path gave.
    fn advance_to_next_mdat(&mut self) -> Result<bool, DecodeError> {
        loop {
            let (len, fourcc, _) = match box_header(&mut self.src) {
                Ok(h) => h,
                Err(_) => return Ok(false), // no (complete) box left → end of stream
            };
            if &fourcc == b"mdat" {
                self.mdat_remaining = len;
                return Ok(true);
            }
            if skip(&mut self.src, len).is_err() {
                return Ok(false); // truncated mid-box → treat as end of stream
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
    /// Gapless encoder delay (leading priming, per-channel samples) from the
    /// `elst` edit list; `0` if the file declares none. A player trims this many
    /// samples off the front for gapless playback (the decode itself is not
    /// trimmed).
    pub fn encoder_delay(&self) -> u64 {
        self.encoder_delay
    }

    /// Gapless presented length (per-channel samples to emit after the encoder
    /// delay) from the `elst` edit list; `None` if the file declares none (or is
    /// fragmented — no `moov` edit list). A player caps playback to this to trim
    /// trailing padding.
    pub fn playable_frames(&self) -> Option<u64> {
        self.present_frames
    }
    pub fn mdat_remaining(&self) -> u64 {
        self.mdat_remaining
    }
    pub fn src_mut(&mut self) -> &mut S {
        &mut self.src
    }

    /// PCM samples per coded frame (MP4 "sample"): ALAC's `frame_length`, or
    /// AAC's fixed 1024-sample access unit. Callers divide a target per-channel
    /// sample by this to get the coded-frame index to seek to. Assumes a
    /// constant frame duration (the `stts` has a single run — true for every
    /// ALAC/AAC encoder in practice); variable-duration tracks are not handled.
    pub fn samples_per_frame(&self) -> u32 {
        match self.codec {
            Mp4Codec::Alac(c) => c.frame_length,
            Mp4Codec::Aac(_) => 1024,
        }
    }
}

impl<S: crate::io::SeekSource> Mp4Reader<S> {
    /// Locate (once, cached) the `stbl` sample-table boxes by re-walking the box
    /// tree from the start using `SeekSource`. Additive to `new`'s forward walk.
    fn ensure_tables(&mut self) -> Result<SampleTableLoc, DecodeError> {
        if let Some(t) = self.tables {
            return Ok(t);
        }
        let t = locate_tables(&mut self.src)?;
        self.tables = Some(t);
        Ok(t)
    }

    /// Total coded frames (MP4 samples), read from the `stsz` sample_count.
    pub fn coded_frame_count(&mut self) -> Result<u32, DecodeError> {
        let t = self.ensure_tables()?;
        self.src
            .seek(SeekFrom::Start(t.stsz.0))
            .map_err(|_| DecodeError::Io)?;
        let head: [u8; 12] = read_exact(&mut self.src)?;
        Ok(be32(&head[8..12]))
    }

    /// Absolute file offset of coded frame `frame_index` (0-based). Accepts
    /// `frame_index == coded_frame_count` to return the end-of-audio offset
    /// (so a caller can size the remaining byte span). **Leaves `src`
    /// repositioned** — the caller seeks to the returned offset to decode.
    pub fn frame_file_offset(&mut self, frame_index: u32) -> Result<u64, DecodeError> {
        let t = self.ensure_tables()?;
        // Total coded frames (stsz sample_count) and chunk count (stco entry_count).
        self.src
            .seek(SeekFrom::Start(t.stsz.0))
            .map_err(|_| DecodeError::Io)?;
        let stsz_head: [u8; 12] = read_exact(&mut self.src)?;
        let total_frames = be32(&stsz_head[8..12]);
        self.src
            .seek(SeekFrom::Start(t.stco.0))
            .map_err(|_| DecodeError::Io)?;
        let stco_head: [u8; 8] = read_exact(&mut self.src)?;
        let total_chunks = be32(&stco_head[4..8]);

        // Which chunk holds the frame (clamp the probe for the one-past index).
        let probe = frame_index.min(total_frames.saturating_sub(1));
        let (chunk0, first_sample, _spc) = stsc_locate(&mut self.src, t.stsc, total_chunks, probe)?;
        let chunk_off = chunk_offset(&mut self.src, t.stco, t.co64, chunk0)?;
        let within = stsz_sum(&mut self.src, t.stsz, first_sample, frame_index)?;
        Ok(chunk_off + within)
    }

    /// Reposition the `mdat` read cursor to absolute file `offset`, declaring
    /// `remaining` bytes of coded audio from there to the end of the `mdat`.
    /// Used by the codecs' sample-accurate seek after it maps a frame index to a
    /// file offset via the sample table; subsequent [`Mp4Reader::read_audio`]
    /// calls read that window. Progressive streams only — a fragmented stream
    /// has no sample table, so its seek never reaches here.
    pub fn seek_audio(&mut self, offset: u64, remaining: u64) -> Result<(), DecodeError> {
        self.src
            .seek(SeekFrom::Start(offset))
            .map_err(|_| DecodeError::Io)?;
        self.mdat_remaining = remaining;
        Ok(())
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
fn parse_moov<S: Source>(src: &mut S, len: u64) -> Result<MoovInfo, DecodeError> {
    // Walk the `moov`'s children to capture the movie timescale (`mvhd`, needed
    // to convert the edit list's presented duration to media samples) and the
    // (first) `trak`. Same exact-consumption loop used at the `trak`/`mdia`
    // levels — each visited box consumes exactly its declared length.
    let mut movie_ts = 0u32;
    let mut trak: Option<TrakInfo> = None;
    let mut consumed = 0u64;
    loop {
        let remaining = len.checked_sub(consumed).ok_or(DecodeError::Malformed)?;
        if remaining < 8 {
            if remaining > 0 {
                skip(src, remaining)?;
            }
            break;
        }
        let (l, f, box_total) = child_header(src, remaining)?;
        match &f {
            b"mvhd" => movie_ts = parse_mvhd(src, l)?,
            // First `trak` only (audio); a later one (e.g. video) is skipped.
            b"trak" if trak.is_none() => trak = Some(parse_trak(src, l)?),
            _ => skip(src, l)?,
        }
        consumed = consumed
            .checked_add(box_total)
            .ok_or(DecodeError::Malformed)?;
    }
    let t = trak.ok_or(DecodeError::Unsupported)?;

    // Gapless presented length: the edit list's `segment_duration` (movie
    // timescale) converted to media (per-channel) samples. `None` unless a
    // normal edit and both timescales are present. u128 intermediate avoids
    // overflow on the `seg * media_ts` product.
    let present_frames = if t.segment_duration > 0 && movie_ts > 0 {
        let media = u128::from(t.segment_duration) * u128::from(t.media_ts) / u128::from(movie_ts);
        u64::try_from(media).ok()
    } else {
        None
    };

    Ok(MoovInfo {
        codec: t.codec,
        total_samples: t.total_samples,
        encoder_delay: t.encoder_delay,
        present_frames,
    })
}

/// What [`parse_trak`] extracts from a `trak`: codec + `mdhd` sample count and
/// timescale, plus the `elst` edit list's gapless `media_time`/`segment_duration`.
struct TrakInfo {
    codec: Mp4Codec,
    total_samples: u64,
    media_ts: u32,
    encoder_delay: u64,
    /// `elst` presented duration in the MOVIE timescale (0 = no edit list).
    segment_duration: u64,
}

/// Walk a `trak`'s children for the edit list (`edts`→`elst`) and media
/// (`mdia`). Same exact-consumption loop as [`parse_mdia`].
fn parse_trak<S: Source>(src: &mut S, len: u64) -> Result<TrakInfo, DecodeError> {
    let mut encoder_delay = 0u64;
    let mut segment_duration = 0u64;
    let mut codec = None;
    let mut total = 0u64;
    let mut media_ts = 0u32;
    let mut consumed = 0u64;
    loop {
        let remaining = len.checked_sub(consumed).ok_or(DecodeError::Malformed)?;
        if remaining < 8 {
            if remaining > 0 {
                skip(src, remaining)?;
            }
            break;
        }
        let (l, f, box_total) = child_header(src, remaining)?;
        match &f {
            b"edts" => {
                let (delay, seg) = descend_for(src, l, b"elst", parse_elst)?;
                encoder_delay = delay;
                segment_duration = seg;
            }
            b"mdia" => {
                let (c, t, ts) = parse_mdia(src, l)?;
                codec = Some(c);
                total = t;
                media_ts = ts;
            }
            _ => skip(src, l)?,
        }
        consumed = consumed
            .checked_add(box_total)
            .ok_or(DecodeError::Malformed)?;
    }
    let codec = codec.ok_or(DecodeError::Unsupported)?;
    Ok(TrakInfo {
        codec,
        total_samples: total,
        media_ts,
        encoder_delay,
        segment_duration,
    })
}

/// What [`parse_moov`] extracts from an MP4 `moov`: the codec config, the
/// `mdhd` per-channel sample count, the gapless encoder delay (the `elst`
/// `media_time`), and the gapless presented length (`elst` `segment_duration`
/// converted to media samples via the movie/media timescales).
struct MoovInfo {
    codec: Mp4Codec,
    total_samples: u64,
    encoder_delay: u64,
    present_frames: Option<u64>,
}

/// Walk an `mdia` container's children for the `mdhd` (timescale + sample count)
/// and the codec config (`minf`→`stbl`→`stsd`). Returns
/// `(codec, total_samples, media_timescale)`.
fn parse_mdia<S: Source>(src: &mut S, len: u64) -> Result<(Mp4Codec, u64, u32), DecodeError> {
    let mut total = 0u64;
    let mut media_ts = 0u32;
    let mut codec = None;
    let mut consumed = 0u64;
    loop {
        let remaining = len.checked_sub(consumed).ok_or(DecodeError::Malformed)?;
        if remaining < 8 {
            // Trailing padding (if any) belongs to this container; the caller's
            // own descend_for will drain past `len` for us, but leave nothing
            // unread within our own bound.
            if remaining > 0 {
                skip(src, remaining)?;
            }
            break;
        }
        let (l, f, box_total) = child_header(src, remaining)?;
        match &f {
            b"mdhd" => {
                let (ts, dur) = parse_mdhd(src, l)?;
                media_ts = ts;
                total = dur;
            }
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
    Ok((codec, total, media_ts))
}

/// `elst` (edit list, ISO/IEC 14496-12 §8.6.6): version(1)+flags(3),
/// entry_count(4), then per entry `segment_duration` + `media_time` +
/// `media_rate` (32-bit fields for v0, 64-bit durations/times for v1). Returns
/// the first normal edit's `(media_time, segment_duration)` — the **encoder
/// delay** (leading media samples a player trims) and the **presented duration**
/// (in the *movie* timescale, which the caller converts to media samples for the
/// gapless playable length). An empty edit (`media_time == -1`, used to insert
/// initial silence) is skipped; `(0, 0)` is returned if the list is empty or
/// all-empty. `media_time` is in the media timescale (= the audio sample rate),
/// so it IS a per-channel sample count — no conversion needed.
fn parse_elst<S: Source>(src: &mut S, len: u64) -> Result<(u64, u64), DecodeError> {
    let head: [u8; 8] = read_exact(src)?; // version+flags(4) + entry_count(4)
    let version = head[0];
    let entry_count = be32(&head[4..8]);
    let entry_len: u64 = if version == 1 { 20 } else { 12 };
    let mut consumed = 8u64;
    let mut delay = 0u64;
    let mut segment_duration = 0u64;
    let mut found = false;
    for _ in 0..entry_count {
        if len.checked_sub(consumed).ok_or(DecodeError::Malformed)? < entry_len {
            break;
        }
        let (seg_dur, media_time): (u64, i64) = if version == 1 {
            let e: [u8; 16] = read_exact(src)?; // seg_dur u64 + media_time i64
            (be64(&e[0..8]), be64(&e[8..16]) as i64)
        } else {
            let e: [u8; 8] = read_exact(src)?; // seg_dur u32 + media_time i32
            (u64::from(be32(&e[0..4])), i64::from(be32(&e[4..8]) as i32))
        };
        skip(src, 4)?; // media_rate (int16 + frac16)
        consumed += entry_len;
        // First non-empty edit: its media_time is the encoder delay, its
        // segment_duration the presented length.
        if !found && media_time >= 0 {
            delay = media_time as u64;
            segment_duration = seg_dur;
            found = true;
        }
    }
    let remainder = len.checked_sub(consumed).ok_or(DecodeError::Malformed)?;
    skip(src, remainder)?;
    Ok((delay, segment_duration))
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
fn parse_mdhd<S: Source>(src: &mut S, len: u64) -> Result<(u32, u64), DecodeError> {
    let head: [u8; 4] = read_exact(src)?;
    let version = head[0];
    if version == 1 {
        let b: [u8; 28] = read_exact(src)?; // 8+8 times, 4 timescale, 8 duration
        let timescale = be32(&b[16..20]);
        let duration = be64(&b[20..28]);
        let remainder = len.checked_sub(4 + 28).ok_or(DecodeError::Malformed)?;
        skip(src, remainder)?;
        Ok((timescale, duration))
    } else {
        let b: [u8; 16] = read_exact(src)?; // 4+4 times, 4 timescale, 4 duration
        let timescale = be32(&b[8..12]);
        let duration = u64::from(be32(&b[12..16]));
        let remainder = len.checked_sub(4 + 16).ok_or(DecodeError::Malformed)?;
        skip(src, remainder)?;
        Ok((timescale, duration))
    }
}

/// `mvhd` (movie header, ISO/IEC 14496-12 §8.2.2): version+flags(4), then the
/// two creation/modification times (`u32×2` for v0, `u64×2` for v1) followed by
/// the `u32` timescale. Returns the **movie timescale** — the unit of the `elst`
/// edit list's `segment_duration`, which the gapless present-length conversion
/// divides by.
fn parse_mvhd<S: Source>(src: &mut S, len: u64) -> Result<u32, DecodeError> {
    let head: [u8; 4] = read_exact(src)?;
    // Read through the timescale: it's at offset 8 (v0: creation/mod u32×2) or
    // 16 (v1: u64×2) in the post-header bytes; 20 bytes covers both.
    let b: [u8; 20] = read_exact(src)?;
    let timescale = if head[0] == 1 {
        be32(&b[16..20])
    } else {
        be32(&b[8..12])
    };
    let remainder = len.checked_sub(4 + 20).ok_or(DecodeError::Malformed)?;
    skip(src, remainder)?;
    Ok(timescale)
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

/// Read `audioObjectType` (5 bits; `== 31` escapes to `32 + read(6)`), per the
/// `GetAudioObjectType()` primitive shared by the top-level and (for explicit
/// SBR) the core object-type reads.
fn read_object_type(r: &mut AscBits) -> Result<u32, DecodeError> {
    let ot = r.read(5)?;
    if ot == 31 {
        Ok(32 + r.read(6)?)
    } else {
        Ok(ot)
    }
}

/// Read `samplingFrequencyIndex` (4 bits; `== 15` escapes to a 24-bit explicit
/// rate) → Hz.
fn read_sample_rate(r: &mut AscBits) -> Result<u32, DecodeError> {
    let sfi = r.read(4)?;
    if sfi == 15 {
        r.read(24)
    } else {
        SAMPLE_RATES
            .get(sfi as usize)
            .copied()
            .ok_or(DecodeError::Malformed)
    }
}

/// Parse the `AudioSpecificConfig` fields this decoder needs
/// (`aac-reference.md` §2 / ISO/IEC 14496-3 Table 1.15). Handles both:
///
/// * **Plain AAC-LC** (`audioObjectType == 2`): `samplingFrequencyIndex` +
///   `channelConfiguration`; SBR (if any) is discovered implicitly at decode.
/// * **Explicit hierarchical HE-AAC** (`audioObjectType == 5` SBR or `29` PS):
///   the top AOT is followed by the *core* `samplingFrequencyIndex`,
///   `channelConfiguration`, then `extensionSamplingFrequencyIndex` (the SBR
///   output rate) and the *core* `audioObjectType` (LC). Confirmed against
///   FFmpeg `ff_mpeg4audio_get_config`: `object_type` becomes the core AOT,
///   `sample_rate` the core rate, `ext_sample_rate` the output rate.
///
/// The trailing `GASpecificConfig` / backward-compatible `0x2B7` sync-extension
/// signalling isn't parsed (the explicit AOT-5 form above covers the shipped
/// fixtures; implicit ADTS-style discovery via the fill element still works for
/// anything else).
fn parse_audio_specific_config(data: &[u8]) -> Result<AacConfig, DecodeError> {
    let mut r = AscBits::new(data);
    let aot = read_object_type(&mut r)?;

    if aot == 5 || aot == 29 {
        // Explicit hierarchical SBR (5) / PS (29): core rate, channels, then
        // the extension (output) rate and the core object type.
        let core_rate = read_sample_rate(&mut r)?;
        let channels = channels_for(r.read(4)?)?;
        let ext_rate = read_sample_rate(&mut r)?;
        let core_aot = read_object_type(&mut r)?;
        return Ok(AacConfig {
            object_type: core_aot as u8,
            sample_rate: core_rate,
            channels,
            sbr_signalled: true,
            ext_sample_rate: Some(ext_rate),
        });
    }

    let sample_rate = read_sample_rate(&mut r)?;
    let channels = channels_for(r.read(4)?)?;
    Ok(AacConfig {
        object_type: aot as u8,
        sample_rate,
        channels,
        sbr_signalled: false,
        ext_sample_rate: None,
    })
}

/// Find the first direct child box named `want` within a container whose
/// content spans `[content_start, content_start + content_len)`, using
/// `SeekSource` to skip siblings. Returns the matched box's `(content_offset,
/// content_len)`. `Unsupported` if absent.
fn descend_seek<S: crate::io::SeekSource>(
    src: &mut S,
    content_start: u64,
    content_len: u64,
    want: &[u8; 4],
) -> Result<(u64, u64), DecodeError> {
    let end = content_start
        .checked_add(content_len)
        .ok_or(DecodeError::Malformed)?;
    let mut here = content_start;
    while here.checked_add(8).ok_or(DecodeError::Malformed)? <= end {
        src.seek(SeekFrom::Start(here))
            .map_err(|_| DecodeError::Io)?;
        let (len, fourcc, hdr) = box_header(src)?;
        let cstart = here + hdr;
        if &fourcc == want {
            return Ok((cstart, len));
        }
        here = cstart.checked_add(len).ok_or(DecodeError::Malformed)?;
    }
    Err(DecodeError::Unsupported)
}

/// Re-walk the box tree (`moov`→`trak`→`mdia`→`minf`→`stbl`) from the file
/// start and record the `stbl` sample-table boxes needed for seeking. Uses the
/// first audio `trak`, matching [`parse_moov`]'s first-trak behavior. `stz2`
/// (compact sample sizes) is treated as absent → seeking that rare variant
/// reports `Unsupported` rather than mis-reading it.
fn locate_tables<S: crate::io::SeekSource>(src: &mut S) -> Result<SampleTableLoc, DecodeError> {
    let file_len = src.seek(SeekFrom::End(0)).map_err(|_| DecodeError::Io)?;
    let (moov_c, moov_l) = descend_seek(src, 0, file_len, b"moov")?;
    let (trak_c, trak_l) = descend_seek(src, moov_c, moov_l, b"trak")?;
    let (mdia_c, mdia_l) = descend_seek(src, trak_c, trak_l, b"mdia")?;
    let (minf_c, minf_l) = descend_seek(src, mdia_c, mdia_l, b"minf")?;
    let (stbl_c, stbl_l) = descend_seek(src, minf_c, minf_l, b"stbl")?;

    let end = stbl_c.checked_add(stbl_l).ok_or(DecodeError::Malformed)?;
    let mut stsc = None;
    let mut stsz = None;
    let mut stco = None;
    let mut co64 = false;
    let mut here = stbl_c;
    while here.checked_add(8).ok_or(DecodeError::Malformed)? <= end {
        src.seek(SeekFrom::Start(here))
            .map_err(|_| DecodeError::Io)?;
        let (len, fourcc, hdr) = box_header(src)?;
        let cstart = here + hdr;
        match &fourcc {
            b"stsc" => stsc = Some((cstart, len)),
            b"stsz" => stsz = Some((cstart, len)),
            b"stco" => {
                stco = Some((cstart, len));
                co64 = false;
            }
            b"co64" => {
                stco = Some((cstart, len));
                co64 = true;
            }
            _ => {}
        }
        here = cstart.checked_add(len).ok_or(DecodeError::Malformed)?;
    }
    Ok(SampleTableLoc {
        stsc: stsc.ok_or(DecodeError::Unsupported)?,
        stsz: stsz.ok_or(DecodeError::Unsupported)?,
        stco: stco.ok_or(DecodeError::Unsupported)?,
        co64,
    })
}

/// Map a 0-based coded sample to its chunk via the run-length `stsc` table.
/// Returns `(chunk0 /*0-based*/, first_sample_of_chunk, samples_per_chunk)`.
/// Streams the entries (no table stored). `total_chunks` (from `stco`) bounds
/// the final run.
fn stsc_locate<S: crate::io::SeekSource>(
    src: &mut S,
    stsc: (u64, u64),
    total_chunks: u32,
    sample: u32,
) -> Result<(u32, u32, u32), DecodeError> {
    src.seek(SeekFrom::Start(stsc.0))
        .map_err(|_| DecodeError::Io)?;
    let head: [u8; 8] = read_exact(src)?;
    let entry_count = be32(&head[4..8]);
    let mut last_first = 1u32; // first_chunk of the pending (not-yet-closed) run
    let mut last_spc = 1u32;
    let mut have = false;
    let mut samples_before = 0u32; // samples in all chunks before `last_first`
    for _ in 0..entry_count {
        let e: [u8; 12] = read_exact(src)?;
        let first_chunk = be32(&e[0..4]);
        let spc = be32(&e[4..8]);
        if have {
            // Close the previous run: chunks [last_first, first_chunk).
            let run_chunks = first_chunk.saturating_sub(last_first);
            let run_samples = run_chunks.saturating_mul(last_spc);
            if last_spc != 0 && sample < samples_before.saturating_add(run_samples) {
                let into = sample - samples_before;
                let chunk_in_run = into / last_spc;
                return Ok((
                    (last_first - 1) + chunk_in_run,
                    samples_before + chunk_in_run * last_spc,
                    last_spc,
                ));
            }
            samples_before = samples_before.saturating_add(run_samples);
        }
        last_first = first_chunk;
        last_spc = spc;
        have = true;
    }
    // Final run spans [last_first, total_chunks].
    let run_chunks = (total_chunks + 1).saturating_sub(last_first);
    let into = sample.saturating_sub(samples_before);
    let chunk_in_run = if last_spc == 0 {
        0
    } else {
        (into / last_spc).min(run_chunks.saturating_sub(1))
    };
    Ok((
        (last_first - 1) + chunk_in_run,
        samples_before + chunk_in_run * last_spc,
        last_spc,
    ))
}

/// Absolute file offset of chunk `chunk0` (0-based) from `stco`/`co64`.
fn chunk_offset<S: crate::io::SeekSource>(
    src: &mut S,
    stco: (u64, u64),
    co64: bool,
    chunk0: u32,
) -> Result<u64, DecodeError> {
    let entry_size: u64 = if co64 { 8 } else { 4 };
    let pos = stco.0 + 8 + u64::from(chunk0) * entry_size;
    src.seek(SeekFrom::Start(pos))
        .map_err(|_| DecodeError::Io)?;
    if co64 {
        let b: [u8; 8] = read_exact(src)?;
        Ok(be64(&b))
    } else {
        let b: [u8; 4] = read_exact(src)?;
        Ok(u64::from(be32(&b)))
    }
}

/// Sum the byte sizes of coded samples `[from, to)` from `stsz` (constant-size
/// shortcut when `sample_size != 0`). Streams the per-sample sizes otherwise.
fn stsz_sum<S: crate::io::SeekSource>(
    src: &mut S,
    stsz: (u64, u64),
    from: u32,
    to: u32,
) -> Result<u64, DecodeError> {
    if from >= to {
        return Ok(0);
    }
    src.seek(SeekFrom::Start(stsz.0))
        .map_err(|_| DecodeError::Io)?;
    let head: [u8; 12] = read_exact(src)?;
    let sample_size = be32(&head[4..8]);
    if sample_size != 0 {
        return Ok(u64::from(to - from) * u64::from(sample_size));
    }
    src.seek(SeekFrom::Start(stsz.0 + 12 + u64::from(from) * 4))
        .map_err(|_| DecodeError::Io)?;
    let mut sum = 0u64;
    for _ in from..to {
        let b: [u8; 4] = read_exact(src)?;
        sum += u64::from(be32(&b));
    }
    Ok(sum)
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
                assert!(!c.sbr_signalled); // plain LC, no explicit SBR
                assert_eq!(c.ext_sample_rate, None);
            }
            Mp4Codec::Alac(_) => panic!("expected AAC"),
        }
    }

    #[test]
    fn parses_explicit_he_aac_asc() {
        // HE-AAC (`sbr.m4a`): the esds ASC is `2b 92 08 00` = audioObjectType 5
        // (SBR), core samplingFrequencyIndex 7 (22050), channelConfiguration 2,
        // extensionSamplingFrequencyIndex 4 (44100), core audioObjectType 2 (LC).
        let m4a = include_bytes!("../../testdata/sbr.m4a");
        let r = Mp4Reader::new(crate::io::SliceSource::new(m4a)).unwrap();
        match r.codec() {
            Mp4Codec::Aac(c) => {
                assert_eq!(c.object_type, 2, "core AOT is LC");
                assert_eq!(c.sample_rate, 22_050, "core rate");
                assert_eq!(c.channels, 2);
                assert!(c.sbr_signalled, "explicit SBR must be flagged");
                assert_eq!(c.ext_sample_rate, Some(44_100), "SBR output rate");
            }
            Mp4Codec::Alac(_) => panic!("expected AAC"),
        }
    }

    /// The explicit ASC parser must decode `2b 92 08 00` exactly (unit-level,
    /// independent of the container walk).
    #[test]
    fn explicit_sbr_asc_bits() {
        let c = parse_audio_specific_config(&[0x2b, 0x92, 0x08, 0x00]).unwrap();
        assert_eq!(c.object_type, 2);
        assert_eq!(c.sample_rate, 22_050);
        assert_eq!(c.channels, 2);
        assert!(c.sbr_signalled);
        assert_eq!(c.ext_sample_rate, Some(44_100));
    }

    /// Task 1: the sample-table seek machinery maps coded-frame indices to file
    /// offsets that (a) start at the first chunk, (b) increase monotonically,
    /// and (c) span exactly the `mdat` payload (sum of all frame sizes). The
    /// absolute-correctness proof is the bit-exact ALAC seek round-trip.
    #[test]
    fn frame_offsets_span_mdat_and_increase() {
        let mut r = Mp4Reader::new(SliceSource::new(ALAC)).unwrap();
        let n = r.coded_frame_count().unwrap();
        assert_eq!(n, 4, "13230 samples / 4096 frame_length ⇒ 4 coded frames");
        let mdat_len = r.mdat_remaining();

        let mut offs = [0u64; 5];
        for k in 0..=n {
            offs[k as usize] = r.frame_file_offset(k).unwrap();
            if k > 0 {
                assert!(
                    offs[k as usize] > offs[(k - 1) as usize],
                    "frame offsets must strictly increase"
                );
            }
        }
        assert_eq!(
            offs[n as usize] - offs[0],
            mdat_len,
            "coded frames must exactly fill mdat"
        );
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

    /// The `edts`/`elst` edit list's `media_time` is surfaced as the gapless
    /// encoder delay (per-channel samples to trim off the front). The shipped
    /// fixtures carry a one-entry edit list: `aac.m4a` has `media_time = 1024`
    /// (the AAC-LC encoder priming), while `alac.m4a` has `media_time = 0`
    /// (lossless — no delay; its 300 is the `segment_duration`, not the delay).
    /// Parsing it must not disturb the codec/`mdhd`/`mdat` fields from the same
    /// `moov`.
    #[test]
    fn parses_edit_list_encoder_delay() {
        let alac = Mp4Reader::new(SliceSource::new(ALAC)).expect("parse alac mp4");
        assert_eq!(alac.encoder_delay(), 0); // media_time == 0, no priming
        assert_eq!(alac.total_samples(), 13_230); // unchanged by the edts walk
        // Presented length = segment_duration(300, movie ts 1000) × mdhd ts
        // 44100 / 1000 = 13230 = full (delay 0, lossless, no padding).
        assert_eq!(alac.playable_frames(), Some(13_230));

        let aac = include_bytes!("../../testdata/aac.m4a");
        let r = Mp4Reader::new(SliceSource::new(aac)).expect("parse aac mp4");
        assert_eq!(r.encoder_delay(), 1024);
        assert_eq!(r.total_samples(), 23_074); // unchanged by the edts walk
        // Presented length = segment_duration(500, movie ts 1000) × 44100/1000
        // = 22050 (the 0.5 s of content); skip 1024 + emit 22050 = 23074 = mdhd,
        // dropping the 478-sample last-AU padding.
        assert_eq!(r.playable_frames(), Some(22_050));
    }

    /// A faststart file without an `edts`/`elst` reports zero delay (and still
    /// parses) — the `aac.aac` ADTS path has no container, but a bare MP4 moov
    /// built without an edit list must default cleanly to 0.
    #[test]
    fn no_edit_list_means_zero_delay() {
        // `frag-aac.m4a` is a fragmented MP4 whose init `moov` carries no edit
        // list — a convenient no-`elst` MP4 already in the fixtures.
        let data = include_bytes!("../../testdata/frag-aac.m4a");
        let r = Mp4Reader::new(SliceSource::new(data)).expect("parse fragmented mp4");
        assert_eq!(r.encoder_delay(), 0);
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
