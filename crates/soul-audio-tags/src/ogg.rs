//! Ogg container walk (Opus and Vorbis-in-Ogg): reassembles the first two
//! packets — the codec identification header and the comment header — then
//! delegates the comment header's Vorbis comment vector to
//! [`vorbis::read_comment_vector`].
//!
//! **Ogg page layout**: `OggS`(4) capture pattern, version(1), header_type(1),
//! granule(8, LE), serial(4, LE), sequence(4, LE), checksum(4, LE),
//! page_segments(1), then `page_segments` lacing bytes. The page body length
//! is the sum of the lacing bytes; a lacing byte of 255 means the packet
//! continues into the next segment (or, if it's the page's last segment,
//! into the next page) — a packet ends at the first lacing byte `< 255`.
//!
//! Packet reassembly is bounded: [`PageWalker::read_packet`] copies at most
//! `buf.len()` bytes into the caller's buffer, discarding (not buffering)
//! anything beyond that bound, so an oversized packet is truncated rather
//! than requiring unbounded memory.
//!
//! **Duration**: `total_frames` comes from the granule position of the
//! *last* page — the total decoded sample count at end of stream — rather
//! than from walking every page in between. [`last_page_granule`] finds it
//! with a bounded backward scan of just the file's tail; see its doc comment
//! and [`read_ogg`]'s for the Opus-vs-Vorbis granule/sample-rate handling.

use crate::locate::read_full;
use crate::util::{discard, read_exact};
use crate::vorbis;
use crate::{StreamFacts, TagError, TagSink};
use embedded_io::SeekFrom;
use soul_audio_core::SeekSource;
#[cfg(test)]
use soul_audio_core::SliceSource;

const MAX_SEGMENTS: usize = 255;

/// How much of the file's tail to pull into a stack buffer when scanning
/// backward for the last page's granule (see [`last_page_granule`]). An Ogg
/// page tops out around 65307 bytes (255 lacing segments, each up to 255
/// bytes), so 64 KiB comfortably covers even a maximal final page while
/// staying a bounded, stack-sized read rather than an unbounded whole-file
/// scan.
const GRANULE_SCAN_WINDOW: usize = 65536;

/// Walks Ogg pages read from `src`, handing out packet bytes one packet at a
/// time via [`read_packet`](Self::read_packet). Holds only the current
/// page's lacing table (bounded, `MAX_SEGMENTS` bytes) — never buffers page
/// or packet payload bytes itself.
struct PageWalker<'s, S> {
    src: &'s mut S,
    lacing: [u8; MAX_SEGMENTS],
    seg_count: usize,
    seg_idx: usize,
}

impl<'s, S: SeekSource> PageWalker<'s, S> {
    fn new(src: &'s mut S) -> Self {
        Self {
            src,
            lacing: [0u8; MAX_SEGMENTS],
            seg_count: 0,
            seg_idx: 0,
        }
    }

    /// Read one Ogg page's fixed header + lacing table from `src`, leaving
    /// the cursor at the start of the page's segment data.
    fn load_page(&mut self) -> Result<(), TagError> {
        let mut header = [0u8; 27];
        read_exact(self.src, &mut header)?;
        if &header[0..4] != b"OggS" {
            return Err(TagError::Malformed);
        }
        let seg_count = header[26] as usize;
        read_exact(self.src, &mut self.lacing[..seg_count])?;
        self.seg_count = seg_count;
        self.seg_idx = 0;
        Ok(())
    }

    /// Read the next packet, copying up to `buf.len()` bytes into `buf` and
    /// discarding the rest; pulls additional pages as needed when a packet
    /// continues past the current page's lacing table (segment value 255).
    /// Returns the number of bytes copied into `buf` (the packet itself may
    /// have been longer — truncated, not an error).
    fn read_packet(&mut self, buf: &mut [u8]) -> Result<usize, TagError> {
        let mut copied = 0usize;
        loop {
            while self.seg_idx >= self.seg_count {
                self.load_page()?;
            }
            let seg_len = self.lacing[self.seg_idx] as usize;
            self.seg_idx += 1;

            let space = buf.len().saturating_sub(copied);
            let take = seg_len.min(space);
            if take > 0 {
                read_exact(self.src, &mut buf[copied..copied + take])?;
            }
            if seg_len > take {
                discard(self.src, seg_len - take)?;
            }
            copied += take;

            if seg_len < MAX_SEGMENTS {
                // A lacing byte < 255 marks the end of the packet.
                break;
            }
        }
        Ok(copied)
    }
}

enum Codec {
    Opus,
    Vorbis,
}

/// Streams the packet [`PageWalker`] is currently positioned at, crossing
/// page boundaries as needed, *without* buffering the packet itself: an
/// ordinary forward-only [`embedded_io::Read`] that reports EOF (`Ok(0)`)
/// once the packet ends (a lacing byte `< 255`).
///
/// Real-world comment packets can be huge — a `METADATA_BLOCK_PICTURE`
/// (base64 embedded cover art) comment routinely runs to hundreds of KB —
/// and materializing a packet into a small fixed buffer before parsing it
/// (the previous approach) meant an oversized packet had to be truncated
/// up front, which could cut off `TITLE`/`ARTIST`/`ALBUM` entirely if they
/// happened to fall past the truncation point, or hit
/// [`vorbis::read_comment_vector`]'s internal budget check on a length field
/// split across the truncation boundary and fail the whole comment header.
/// Streaming instead means only a single field's *value* is ever bounded by
/// a caller-sized buffer (`scratch`, same as every other format this crate
/// reads) — an oversized field (the cover art) is read in bounded chunks and
/// discarded, never forcing the fields around it to be dropped.
struct PacketReader<'w, 's, S> {
    walker: &'w mut PageWalker<'s, S>,
    /// Bytes left to read from the segment currently being consumed.
    seg_remaining: usize,
    /// Whether finishing the current segment (`seg_remaining` hitting 0)
    /// ends the packet (its lacing value was `< 255`).
    seg_terminal: bool,
    /// Set once the packet has ended; every further `read` reports `Ok(0)`.
    done: bool,
}

impl<'w, 's, S: SeekSource> PacketReader<'w, 's, S> {
    fn new(walker: &'w mut PageWalker<'s, S>) -> Self {
        Self {
            walker,
            seg_remaining: 0,
            seg_terminal: false,
            done: false,
        }
    }
}

impl<'w, 's, S: SeekSource> embedded_io::ErrorType for PacketReader<'w, 's, S> {
    type Error = embedded_io::ErrorKind;
}

impl<'w, 's, S: SeekSource> embedded_io::Read for PacketReader<'w, 's, S> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.seg_remaining == 0 {
            while self.walker.seg_idx >= self.walker.seg_count {
                self.walker
                    .load_page()
                    .map_err(|_| embedded_io::ErrorKind::Other)?;
            }
            let seg_len = self.walker.lacing[self.walker.seg_idx] as usize;
            self.walker.seg_idx += 1;
            self.seg_remaining = seg_len;
            self.seg_terminal = seg_len < MAX_SEGMENTS;
            if seg_len == 0 {
                // A 0-length segment is always terminal (an empty packet, or
                // one whose length is an exact multiple of 255) — genuine
                // EOF, not an error.
                self.done = true;
                return Ok(0);
            }
        }
        let take = buf.len().min(self.seg_remaining);
        let mut got = 0usize;
        while got < take {
            let n = self
                .walker
                .src
                .read(&mut buf[got..take])
                .map_err(|_| embedded_io::ErrorKind::Other)?;
            if n == 0 {
                return Err(embedded_io::ErrorKind::Other); // truncated stream
            }
            got += n;
        }
        self.seg_remaining -= take;
        if self.seg_remaining == 0 && self.seg_terminal {
            self.done = true;
        }
        Ok(take)
    }
}

/// Scan `buf` (the tail of an Ogg file — see [`last_page_granule`])
/// backward for the last `OggS` capture pattern whose granule position is a
/// real end-of-packet marker rather than `u64::MAX` ("no packet finished on
/// this page" — e.g. a continued packet still spanning into a page that
/// never arrived within the scan window). Never panics: bails out (`None`)
/// once the search runs off the front of `buf` without finding one.
fn granule_from_tail(buf: &[u8]) -> Option<u64> {
    if buf.len() < 14 {
        return None;
    }
    let mut idx = buf.len() - 14;
    loop {
        if &buf[idx..idx + 4] == b"OggS" {
            let granule = u64::from_le_bytes(buf[idx + 6..idx + 14].try_into().unwrap());
            if granule != u64::MAX {
                return Some(granule);
            }
        }
        if idx == 0 {
            return None;
        }
        idx -= 1;
    }
}

/// Find the last page's granule position by seeking to the end of `src` and
/// reading only its final [`GRANULE_SCAN_WINDOW`] bytes (or the whole file,
/// if shorter) into a stack buffer — bounded, never an unbounded whole-file
/// scan. Leaves `src`'s cursor wherever that read ended; callers that need
/// the cursor elsewhere afterward must reposition it themselves. Returns
/// `None` on any I/O failure or if no page with a real granule is found,
/// rather than erroring — a missing/malformed trailer shouldn't fail the
/// whole read, it should just leave `total_frames` unknown.
fn last_page_granule<S: SeekSource>(src: &mut S) -> Option<u64> {
    let len = src.seek(SeekFrom::End(0)).ok()?;
    let window = len.min(GRANULE_SCAN_WINDOW as u64) as usize;
    if window < 14 {
        return None;
    }
    src.seek(SeekFrom::Start(len - window as u64)).ok()?;
    let mut buf = [0u8; GRANULE_SCAN_WINDOW];
    let n = read_full(src, &mut buf[..window]).ok()?;
    granule_from_tail(&buf[..n])
}

/// Read stream facts + tags from an Ogg (Opus or Vorbis) container: the
/// first packet identifies the codec (`OpusHead` or `\x01vorbis`) and
/// carries sample rate / channel count; the second packet is the comment
/// header (`OpusTags` or `\x03vorbis` followed by a Vorbis comment vector),
/// streamed via [`PacketReader`] into [`vorbis::read_comment_vector`].
///
/// `total_frames` comes from the *last* Ogg page's granule position (the
/// total decoded sample count at end of stream), found via a bounded
/// backward scan of the file's tail ([`last_page_granule`]) rather than
/// walking every page. For Opus, the granule is always in 48 kHz samples
/// regardless of the codec's own `input_rate` — so `sample_rate` is reported
/// as `48_000` (not the OpusHead `input_rate`) and `total_frames` is the
/// granule minus `pre_skip` (the leading samples the decoder discards), so
/// `total_frames / sample_rate` is a correct duration. For Vorbis, the
/// granule is already in the identification header's sample rate, so
/// `total_frames` is the granule as-is. If no granule can be found (e.g. a
/// truncated or unusually-laid-out file), `total_frames` is left `None`
/// rather than erroring.
pub fn read_ogg<S: SeekSource>(
    src: &mut S,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<StreamFacts, TagError> {
    let mut facts = StreamFacts::default();
    let mut pre_skip: u16 = 0;
    let codec;
    {
        let mut walker = PageWalker::new(src);

        // --- Packet 1: codec identification header (small, read in one shot).
        // 64 bytes so a FLAC-in-Ogg mapping packet (`0x7F"FLAC"` header + `fLaC`
        // + a 34-byte STREAMINFO block = 51 bytes) fits whole, not just the
        // ~30-byte Opus/Vorbis id headers.
        let mut id_buf = [0u8; 64];
        let id_len = walker.read_packet(&mut id_buf)?;
        let id = &id_buf[..id_len];

        // FLAC-in-Ogg has a different shape (metadata blocks as their own
        // packets, not a single comment header), so branch off before the
        // Opus/Vorbis codec match. STREAMINFO gives frame-exact facts, so that
        // path returns directly without the granule tail-scan below.
        if id.starts_with(b"\x7FFLAC") {
            return read_flac_in_ogg(walker, id, scratch, sink);
        }

        codec = if id.starts_with(b"OpusHead") {
            // OpusHead: magic(8) version(1) channels(1) pre-skip(2, LE)
            // input sample rate(4, LE) output gain(2) channel mapping
            // family(1).
            if id.len() >= 16 {
                facts.channels = Some(u16::from(id[9]));
                pre_skip = u16::from_le_bytes([id[10], id[11]]);
                // Opus always decodes at 48 kHz, independent of the
                // declared input rate — report that (not `input_rate`) so
                // `total_frames / sample_rate` below is a correct duration.
                facts.sample_rate = Some(48_000);
            }
            Codec::Opus
        } else if id.starts_with(b"\x01vorbis") {
            // Vorbis identification header: magic(7) version(4) channels(1)
            // sample rate(4, LE) ...
            if id.len() >= 16 {
                facts.channels = Some(u16::from(id[11]));
                facts.sample_rate = Some(u32::from_le_bytes([id[12], id[13], id[14], id[15]]));
            }
            Codec::Vorbis
        } else {
            return Err(TagError::Unsupported);
        };

        // --- Packet 2: comment header. Streamed via `PacketReader` (see its doc
        // comment for why) rather than materialized into a buffer first.
        let mut packet = PacketReader::new(&mut walker);
        let magic: &[u8] = match codec {
            Codec::Opus => b"OpusTags",
            Codec::Vorbis => b"\x03vorbis",
        };
        let mut magic_buf = [0u8; 8];
        read_exact(&mut packet, &mut magic_buf[..magic.len()])?;
        if &magic_buf[..magic.len()] != magic {
            return Err(TagError::Malformed);
        }

        // No externally-declared total length to bound the comment vector by
        // (unlike FLAC's METADATA_BLOCK header) — the packet's own lacing-based
        // framing already bounds it: `PacketReader` reports `Ok(0)` at the real
        // end, which surfaces as `TagError::Malformed` via `read_exact` if a
        // declared field length overruns it. `u32::MAX` just means "trust the
        // stream framing" instead of imposing a second, redundant bound.
        vorbis::read_comment_vector(&mut packet, u32::MAX, scratch, sink)?;
        // `walker`/`packet` (and the reborrow of `src` they hold) end here,
        // freeing `src` for the direct seek below.
    }

    if let Some(granule) = last_page_granule(src) {
        facts.total_frames = Some(match codec {
            Codec::Opus => granule.saturating_sub(u64::from(pre_skip)),
            Codec::Vorbis => granule,
        });
    }

    Ok(facts)
}

/// Read facts + tags from a FLAC-in-Ogg stream. `id` is the fully-read first
/// Ogg packet — the `\x7FFLAC` mapping header immediately followed by the
/// native `fLaC` signature and the STREAMINFO block — and `walker` is
/// positioned at the start of the second packet. Each subsequent packet
/// carries exactly one FLAC metadata block (VORBIS_COMMENT, PICTURE, …) until
/// one marks itself last. Only VORBIS_COMMENT is surfaced (text fields plus any
/// `METADATA_BLOCK_PICTURE`); a native PICTURE block (type 6) would need
/// Ogg-frame-aware offset handling the no-alloc art path doesn't model, so it
/// is skipped. STREAMINFO supplies frame-exact facts.
fn read_flac_in_ogg<S: SeekSource>(
    mut walker: PageWalker<'_, S>,
    id: &[u8],
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<StreamFacts, TagError> {
    let mut facts = StreamFacts::default();

    // Packet 1: `0x7F"FLAC"`(5) major(1) minor(1) header_count(2 BE) `fLaC`(4)
    // STREAMINFO header(4) STREAMINFO body(34). The 8 bytes at STREAMINFO body
    // offset 10..18 pack sample_rate(20) | channels-1(3) | bps-1(5) |
    // total_samples(36); that body starts at packet offset 17, so they are
    // `id[27..35]`.
    if id.len() >= 35 {
        let si = &id[27..35];
        facts.sample_rate =
            Some((u32::from(si[0]) << 12) | (u32::from(si[1]) << 4) | (u32::from(si[2]) >> 4));
        facts.channels = Some(u16::from(((si[2] >> 1) & 0x7) + 1));
        facts.total_frames = Some(
            (u64::from(si[3] & 0xF) << 32)
                | u64::from(u32::from_be_bytes([si[4], si[5], si[6], si[7]])),
        );
    }

    // Packets 2..: one FLAC metadata block each, until the last-block flag.
    loop {
        let mut packet = PacketReader::new(&mut walker);
        let Some(hdr) = read_block_header(&mut packet)? else {
            break; // stream ended before a last-block flag — tolerate it
        };
        let last = hdr[0] & 0x80 != 0;
        let block_type = hdr[0] & 0x7F;
        let body_len = (u32::from(hdr[1]) << 16) | (u32::from(hdr[2]) << 8) | u32::from(hdr[3]);
        if block_type == 4 {
            vorbis::read_comment_vector(&mut packet, body_len, scratch, sink)?;
        }
        // Drain the rest of this packet so the walker aligns on the next one.
        drain_packet(&mut packet)?;
        if last {
            break;
        }
    }

    // STREAMINFO's `total_samples` is 0 ("unknown") when a FLAC stream is muxed
    // without a known length — exactly what ffmpeg writes for FLAC-in-Ogg. Fall
    // back to the last page's granule (the decoded sample count at end of
    // stream), which is frame-exact, just like the Vorbis path.
    if facts.total_frames.is_none_or(|t| t == 0)
        && let Some(granule) = last_page_granule(walker.src)
    {
        facts.total_frames = Some(granule);
    }

    Ok(facts)
}

/// Read a 4-byte FLAC metadata-block header from a packet stream. `Ok(None)` if
/// the packet ended cleanly before any byte (no further metadata packets);
/// [`TagError::Malformed`] on a partial header.
fn read_block_header<R: embedded_io::Read>(r: &mut R) -> Result<Option<[u8; 4]>, TagError> {
    let mut hdr = [0u8; 4];
    let mut off = 0;
    while off < 4 {
        match r.read(&mut hdr[off..]).map_err(|_| TagError::Io)? {
            0 => break,
            n => off += n,
        }
    }
    match off {
        0 => Ok(None),
        4 => Ok(Some(hdr)),
        _ => Err(TagError::Malformed),
    }
}

/// Read and discard the remainder of a packet stream to its end.
fn drain_packet<R: embedded_io::Read>(r: &mut R) -> Result<(), TagError> {
    let mut buf = [0u8; 256];
    while r.read(&mut buf).map_err(|_| TagError::Io)? != 0 {}
    Ok(())
}

/// Longest `METADATA_BLOCK_PICTURE` base64 value we'll materialize, ~4/3 of the
/// ~40 MB decoded cover-art cap the service enforces — a defensive bound so a
/// hostile comment can't force an unbounded allocation (`data_len` may be 0 for
/// an Ogg picture, so the caller's `data_len` check can't be relied on).
#[cfg(feature = "alloc")]
const MAX_PICTURE_B64: usize = 56 * 1024 * 1024;

/// The `METADATA_BLOCK_PICTURE` key with its `=` separator; a comment matches
/// iff its first [`KEY_EQ`]`.len()` bytes equal this (case-insensitive key,
/// exact `=`).
#[cfg(feature = "alloc")]
const KEY_EQ: &[u8] = b"METADATA_BLOCK_PICTURE=";

/// Walk a Vorbis comment vector streamed from `packet`, returning the raw
/// base64 **value** (no key, no `=`) of the `ordinal`-th
/// `METADATA_BLOCK_PICTURE` comment. Non-target comments are discarded, not
/// buffered; only the target's value is allocated. Errors
/// [`TagError::Malformed`] if the ordinal isn't found or the target value
/// exceeds [`MAX_PICTURE_B64`].
#[cfg(feature = "alloc")]
fn nth_picture_value<R: embedded_io::Read>(
    packet: &mut R,
    ordinal: u64,
) -> Result<alloc::vec::Vec<u8>, TagError> {
    let mut u32buf = [0u8; 4];

    read_exact(packet, &mut u32buf)?;
    let vendor_len = u32::from_le_bytes(u32buf) as usize;
    discard(packet, vendor_len)?;

    read_exact(packet, &mut u32buf)?;
    let count = u32::from_le_bytes(u32buf);

    let mut seen = 0u64;
    for _ in 0..count {
        read_exact(packet, &mut u32buf)?;
        let comment_len = u32::from_le_bytes(u32buf) as usize;

        // Probe just the leading key bytes to classify the comment without
        // buffering its (possibly huge) value.
        let probe_len = comment_len.min(KEY_EQ.len());
        let mut probe = [0u8; KEY_EQ.len()];
        read_exact(packet, &mut probe[..probe_len])?;
        let is_pic = probe_len == KEY_EQ.len() && probe[..probe_len].eq_ignore_ascii_case(KEY_EQ);
        let rest = comment_len - probe_len;

        if is_pic {
            if seen == ordinal {
                if rest > MAX_PICTURE_B64 {
                    return Err(TagError::Malformed);
                }
                let mut value = alloc::vec![0u8; rest];
                read_exact(packet, &mut value)?;
                return Ok(value);
            }
            seen += 1;
        }
        discard(packet, rest)?;
    }
    Err(TagError::Malformed)
}

/// Extract the image `data` bytes from an in-memory FLAC PICTURE `block`
/// (big-endian `type · mime_len · mime · desc_len · desc · width · height ·
/// depth · colors · data_len · data`). Every field is bounds-checked; a block
/// truncated before its declared `data` end is [`TagError::Malformed`] rather
/// than a panic. Reuses `block`'s allocation (drains the header, truncates to
/// `data_len`) instead of copying the image again.
#[cfg(feature = "alloc")]
fn extract_picture_data(mut block: alloc::vec::Vec<u8>) -> Result<alloc::vec::Vec<u8>, TagError> {
    fn be32(b: &[u8], pos: usize) -> Result<u32, TagError> {
        let end = pos.checked_add(4).ok_or(TagError::Malformed)?;
        b.get(pos..end)
            .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
            .ok_or(TagError::Malformed)
    }

    let mut pos = 0usize; // type
    pos = pos.checked_add(4).ok_or(TagError::Malformed)?;
    let mime_len = be32(&block, pos)? as usize;
    pos = pos
        .checked_add(4)
        .and_then(|p| p.checked_add(mime_len))
        .ok_or(TagError::Malformed)?;
    let desc_len = be32(&block, pos)? as usize;
    pos = pos
        .checked_add(4)
        .and_then(|p| p.checked_add(desc_len))
        .ok_or(TagError::Malformed)?;
    // width, height, depth, colors (4 × u32), then data_len.
    pos = pos.checked_add(16).ok_or(TagError::Malformed)?;
    let data_len = be32(&block, pos)? as usize;
    pos = pos.checked_add(4).ok_or(TagError::Malformed)?;

    let end = pos.checked_add(data_len).ok_or(TagError::Malformed)?;
    if end > block.len() {
        return Err(TagError::Malformed);
    }
    block.truncate(end);
    block.drain(..pos);
    Ok(block)
}

/// Re-walk an Ogg (Opus or Vorbis) stream from the start and materialize the
/// `ordinal`-th `METADATA_BLOCK_PICTURE` cover image: find the comment,
/// base64-decode its full value (reassembled across pages by [`PacketReader`]),
/// and return the FLAC PICTURE block's image `data` bytes.
///
/// This is the alloc/std consumer for a [`crate::Picture`] whose `encoding` is
/// [`crate::DataEncoding::Base64`] — the descriptor the no-alloc scan path
/// emits, with the picture ordinal stashed in its `data_offset`. Allocation is
/// fine here (desktop only); the no-alloc scan path stays untouched.
#[cfg(feature = "alloc")]
pub(crate) fn read_ogg_picture<S: SeekSource>(
    src: &mut S,
    ordinal: u64,
) -> Result<alloc::vec::Vec<u8>, TagError> {
    src.seek(SeekFrom::Start(0)).map_err(|_| TagError::Io)?;
    let mut walker = PageWalker::new(src);

    // Packet 1: codec identification header — used only to pick the comment
    // header's magic (its bytes are otherwise ignored here).
    let mut id_buf = [0u8; 32];
    let id_len = walker.read_packet(&mut id_buf)?;
    let id = &id_buf[..id_len];
    let magic: &[u8] = if id.starts_with(b"OpusHead") {
        b"OpusTags"
    } else if id.starts_with(b"\x01vorbis") {
        b"\x03vorbis"
    } else {
        return Err(TagError::Unsupported);
    };

    // Packet 2: comment header — verify magic, then walk the comment vector.
    let mut packet = PacketReader::new(&mut walker);
    let mut magic_buf = [0u8; 8];
    read_exact(&mut packet, &mut magic_buf[..magic.len()])?;
    if &magic_buf[..magic.len()] != magic {
        return Err(TagError::Malformed);
    }

    let value = nth_picture_value(&mut packet, ordinal)?;

    // Decode the full base64 value into the FLAC PICTURE block, then hand back
    // just its image bytes.
    let out_cap = value.len() / 4 * 3;
    let mut decoded = alloc::vec![0u8; out_cap];
    let n = crate::base64::decode_into(&value, &mut decoded).map_err(|_| TagError::Malformed)?;
    decoded.truncate(n);
    extract_picture_data(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Field;
    use crate::testfix::Collect;
    extern crate alloc;
    use alloc::vec::Vec;

    /// Standard Ogg lacing encoding for a packet of `len` bytes: repeated
    /// 255s for each full 255-byte run, terminated by a value `< 255`
    /// (always emits at least one byte, even for a zero-length packet).
    fn lacing_for(len: usize) -> Vec<u8> {
        let mut v = Vec::new();
        let mut rem = len;
        loop {
            if rem >= 255 {
                v.push(255);
                rem -= 255;
            } else {
                v.push(rem as u8);
                break;
            }
        }
        v
    }

    fn ogg_page(header_type: u8, granule: u64, serial: u32, seq: u32, packet: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"OggS");
        v.push(0); // version
        v.push(header_type);
        v.extend_from_slice(&granule.to_le_bytes());
        v.extend_from_slice(&serial.to_le_bytes());
        v.extend_from_slice(&seq.to_le_bytes());
        v.extend_from_slice(&[0, 0, 0, 0]); // checksum, unchecked by the reader
        let lacing = lacing_for(packet.len());
        v.push(lacing.len() as u8);
        v.extend_from_slice(&lacing);
        v.extend_from_slice(packet);
        v
    }

    fn opus_head(sample_rate: u32, channels: u8) -> Vec<u8> {
        opus_head_with_pre_skip(sample_rate, channels, 0)
    }

    fn opus_head_with_pre_skip(sample_rate: u32, channels: u8, pre_skip: u16) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"OpusHead");
        v.push(1); // version
        v.push(channels);
        v.extend_from_slice(&pre_skip.to_le_bytes());
        v.extend_from_slice(&sample_rate.to_le_bytes());
        v.extend_from_slice(&0i16.to_le_bytes()); // output gain
        v.push(0); // channel mapping family 0
        v
    }

    fn opus_tags(comments: &[&str]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"OpusTags");
        v.extend_from_slice(&(6u32).to_le_bytes());
        v.extend_from_slice(b"vendor");
        v.extend_from_slice(&(comments.len() as u32).to_le_bytes());
        for c in comments {
            v.extend_from_slice(&(c.len() as u32).to_le_bytes());
            v.extend_from_slice(c.as_bytes());
        }
        v
    }

    fn vorbis_id_header(sample_rate: u32, channels: u8) -> Vec<u8> {
        let mut v = Vec::new();
        v.push(0x01);
        v.extend_from_slice(b"vorbis");
        v.extend_from_slice(&0u32.to_le_bytes()); // vorbis_version
        v.push(channels);
        v.extend_from_slice(&sample_rate.to_le_bytes());
        v.extend_from_slice(&0i32.to_le_bytes()); // bitrate_maximum
        v.extend_from_slice(&0i32.to_le_bytes()); // bitrate_nominal
        v.extend_from_slice(&0i32.to_le_bytes()); // bitrate_minimum
        v.push(0); // blocksizes
        v.push(1); // framing flag
        v
    }

    fn vorbis_comment_header(comments: &[&str]) -> Vec<u8> {
        let mut v = Vec::new();
        v.push(0x03);
        v.extend_from_slice(b"vorbis");
        v.extend_from_slice(&(6u32).to_le_bytes());
        v.extend_from_slice(b"vendor");
        v.extend_from_slice(&(comments.len() as u32).to_le_bytes());
        for c in comments {
            v.extend_from_slice(&(c.len() as u32).to_le_bytes());
            v.extend_from_slice(c.as_bytes());
        }
        v.push(1); // framing bit
        v
    }

    #[test]
    fn opus_two_page_stream_yields_title_and_sample_rate() {
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, 0, 1, 0, &opus_head(48_000, 2))); // BOS
        bytes.extend(ogg_page(0x00, 0, 1, 1, &opus_tags(&["TITLE=Test Track"])));

        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = read_ogg(&mut src, &mut scratch, &mut c).unwrap();

        assert_eq!(facts.sample_rate, Some(48_000));
        assert_eq!(facts.channels, Some(2));
        assert!(
            c.fields
                .contains(&(Some(Field::Title), "TITLE".into(), "Test Track".into()))
        );
    }

    #[test]
    fn vorbis_two_page_stream_yields_title_and_sample_rate() {
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, 0, 7, 0, &vorbis_id_header(44_100, 2)));
        bytes.extend(ogg_page(
            0x00,
            0,
            7,
            1,
            &vorbis_comment_header(&["ARTIST=Someone", "TITLE=A Song"]),
        ));

        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = read_ogg(&mut src, &mut scratch, &mut c).unwrap();

        assert_eq!(facts.sample_rate, Some(44_100));
        assert_eq!(facts.channels, Some(2));
        assert!(
            c.fields
                .contains(&(Some(Field::Title), "TITLE".into(), "A Song".into()))
        );
        assert!(
            c.fields
                .contains(&(Some(Field::Artist), "ARTIST".into(), "Someone".into()))
        );
    }

    #[test]
    fn comment_packet_spanning_multiple_segments_in_one_page() {
        // Pad a comment value long enough to force >1 lacing segment
        // (255+ bytes) within the *same* page, exercising the mid-page
        // continuation path (segment value == 255) without a page boundary.
        let long_value: Vec<u8> = core::iter::repeat_n(b'x', 300).collect();
        let long_value = core::str::from_utf8(&long_value).unwrap();
        let mut comment = alloc::string::String::from("COMMENT=");
        comment.push_str(long_value);

        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, 0, 3, 0, &opus_head(48_000, 1)));
        bytes.extend(ogg_page(0x00, 0, 3, 1, &opus_tags(&[&comment])));

        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 1024];
        let mut c = Collect::default();
        let facts = read_ogg(&mut src, &mut scratch, &mut c).unwrap();

        assert_eq!(facts.sample_rate, Some(48_000));
        let found = c
            .fields
            .iter()
            .find(|(_, k, _)| k == "COMMENT")
            .expect("COMMENT field present");
        assert_eq!(found.2.len(), 300);
    }

    #[test]
    fn packet_spanning_a_page_boundary_reassembles() {
        // A single OpusTags packet whose lacing table ends with a 255 in
        // page 1 (continuation) and finishes with a <255 segment at the
        // start of page 2 — genuine cross-page packet reassembly.
        let long_value: Vec<u8> = core::iter::repeat_n(b'y', 400).collect();
        let long_value = core::str::from_utf8(&long_value).unwrap();
        let mut comment = alloc::string::String::from("COMMENT=");
        comment.push_str(long_value);
        let tags = opus_tags(&[&comment]);

        // `tags` is long enough (>255, <510 bytes) that its lacing table is
        // exactly two entries: [255, remainder]. Split the packet's raw
        // bytes across two pages at that boundary — page 2 ends its lacing
        // table on the continuation value (255), page 3 finishes the packet
        // with the remainder (< 255).
        let lacing = lacing_for(tags.len());
        assert_eq!(lacing.len(), 2, "test fixture needs a two-segment packet");
        assert_eq!(lacing[0], 255);
        let split_at = 255usize;

        let mut page1 = Vec::new();
        page1.extend_from_slice(b"OggS");
        page1.push(0);
        page1.push(0x02); // BOS page carries packet 1 (OpusHead) fully
        page1.extend_from_slice(&0u64.to_le_bytes());
        page1.extend_from_slice(&9u32.to_le_bytes());
        page1.extend_from_slice(&0u32.to_le_bytes());
        page1.extend_from_slice(&[0, 0, 0, 0]);
        let head = opus_head(48_000, 1);
        let head_lacing = lacing_for(head.len());
        page1.push(head_lacing.len() as u8);
        page1.extend_from_slice(&head_lacing);
        page1.extend_from_slice(&head);

        let mut page2 = Vec::new();
        page2.extend_from_slice(b"OggS");
        page2.push(0);
        page2.push(0x00);
        page2.extend_from_slice(&0u64.to_le_bytes());
        page2.extend_from_slice(&9u32.to_le_bytes());
        page2.extend_from_slice(&1u32.to_le_bytes());
        page2.extend_from_slice(&[0, 0, 0, 0]);
        // First segment(s) of the OpusTags packet only.
        let seg_a = &lacing[..1];
        page2.push(seg_a.len() as u8);
        page2.extend_from_slice(seg_a);
        page2.extend_from_slice(&tags[..split_at]);

        let mut page3 = Vec::new();
        page3.extend_from_slice(b"OggS");
        page3.push(0);
        page3.push(0x00);
        page3.extend_from_slice(&0u64.to_le_bytes());
        page3.extend_from_slice(&9u32.to_le_bytes());
        page3.extend_from_slice(&2u32.to_le_bytes());
        page3.extend_from_slice(&[0, 0, 0, 0]);
        let seg_b = &lacing[1..];
        page3.push(seg_b.len() as u8);
        page3.extend_from_slice(seg_b);
        page3.extend_from_slice(&tags[split_at..]);

        let mut bytes = Vec::new();
        bytes.extend(page1);
        bytes.extend(page2);
        bytes.extend(page3);

        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 1024];
        let mut c = Collect::default();
        let facts = read_ogg(&mut src, &mut scratch, &mut c).unwrap();

        assert_eq!(facts.sample_rate, Some(48_000));
        let found = c
            .fields
            .iter()
            .find(|(_, k, _)| k == "COMMENT")
            .expect("COMMENT field present after cross-page reassembly");
        assert_eq!(found.2.len(), 400);
    }

    #[test]
    fn oversized_comment_packet_truncates_without_panicking() {
        let long_value: Vec<u8> = core::iter::repeat_n(b'z', 2000).collect();
        let long_value = core::str::from_utf8(&long_value).unwrap();
        let mut comment = alloc::string::String::from("COMMENT=");
        comment.push_str(long_value);

        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, 0, 5, 0, &opus_head(48_000, 1)));
        bytes.extend(ogg_page(0x00, 0, 5, 1, &opus_tags(&[&comment])));

        let mut src = SliceSource::new(&bytes);
        // Deliberately small scratch: the COMMENT value (2000 bytes) is
        // larger than it, so its value gets truncated/discarded.
        let mut scratch = [0u8; 64];
        let mut c = Collect::default();
        // Must not panic; may or may not recover the (truncated) field.
        let _ = read_ogg(&mut src, &mut scratch, &mut c);
    }

    #[test]
    fn fields_before_an_oversized_trailing_comment_survive() {
        // Regression test for a real-world corpus finding (Task 9's
        // differential test vs `lofty`): real Opus files routinely carry a
        // huge embedded `METADATA_BLOCK_PICTURE` comment (base64 cover art,
        // often hundreds of KB) — conventionally the *last* comment, after
        // TITLE/ARTIST/ALBUM. The old implementation materialized the whole
        // packet into half of `scratch` before parsing it, so an oversized
        // packet was truncated up front and could lose every field —
        // including ones that appear *before* the oversized one.
        // `PacketReader` streams the packet instead, so only the oversized
        // field's own *value* is truncated/discarded; the fields around it
        // must still come through.
        let long_value: Vec<u8> = core::iter::repeat_n(b'z', 5000).collect();
        let long_value = core::str::from_utf8(&long_value).unwrap();
        let mut cover = alloc::string::String::from("METADATA_BLOCK_PICTURE=");
        cover.push_str(long_value);

        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, 0, 6, 0, &opus_head(48_000, 2)));
        bytes.extend(ogg_page(
            0x00,
            0,
            6,
            1,
            &opus_tags(&["TITLE=A Song", "ARTIST=Someone", "ALBUM=An Album", &cover]),
        ));

        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256]; // same size `read_owned` backs real reads with
        let mut c = Collect::default();
        let facts = read_ogg(&mut src, &mut scratch, &mut c).unwrap();

        assert_eq!(facts.sample_rate, Some(48_000));
        assert!(
            c.fields
                .contains(&(Some(Field::Title), "TITLE".into(), "A Song".into()))
        );
        assert!(
            c.fields
                .contains(&(Some(Field::Artist), "ARTIST".into(), "Someone".into()))
        );
        assert!(
            c.fields
                .contains(&(Some(Field::Album), "ALBUM".into(), "An Album".into()))
        );
    }

    #[test]
    fn unrecognized_codec_magic_is_unsupported() {
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, 0, 11, 0, b"NotACodecHeader."));
        bytes.extend(ogg_page(0x00, 0, 11, 1, b"whatever"));

        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        assert_eq!(
            read_ogg(&mut src, &mut scratch, &mut c).unwrap_err(),
            TagError::Unsupported
        );
    }

    #[test]
    fn truncated_stream_is_malformed_not_a_panic() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"OggS");
        bytes.push(0);
        bytes.push(0x02);
        bytes.extend_from_slice(&[0u8; 8]); // granule
        bytes.extend_from_slice(&[0u8; 4]); // serial
        bytes.extend_from_slice(&[0u8; 4]); // seq
        bytes.extend_from_slice(&[0u8; 4]); // checksum
        bytes.push(5); // claims 5 lacing bytes but none follow

        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        assert_eq!(
            read_ogg(&mut src, &mut scratch, &mut c).unwrap_err(),
            TagError::Malformed
        );
    }

    #[test]
    fn opus_total_frames_from_last_page_granule_minus_pre_skip() {
        let pre_skip: u16 = 312; // libopus's usual default
        let granule: u64 = 960_000; // 20 s of 48 kHz samples
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(
            0x02,
            0,
            21,
            0,
            &opus_head_with_pre_skip(48_000, 2, pre_skip),
        )); // BOS
        bytes.extend(ogg_page(0x00, 0, 21, 1, &opus_tags(&["TITLE=Long Track"])));
        // Final (EOS) page: the real end-of-stream granule.
        bytes.extend(ogg_page(0x04, granule, 21, 2, &[0u8; 8]));

        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = read_ogg(&mut src, &mut scratch, &mut c).unwrap();

        assert_eq!(facts.sample_rate, Some(48_000));
        assert_eq!(facts.total_frames, Some(granule - u64::from(pre_skip)));
    }

    #[test]
    fn opus_total_frames_skips_trailing_page_with_no_finished_packet() {
        // A granule of `u64::MAX` means "no packet finished on this page"
        // (e.g. a packet still spanning into a page beyond the scan
        // window). The backward scan must keep looking for an earlier page
        // with a real granule instead of reporting a bogus
        // `u64::MAX - pre_skip`.
        let real_granule: u64 = 480_000;
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, 0, 22, 0, &opus_head(48_000, 1)));
        bytes.extend(ogg_page(0x00, 0, 22, 1, &opus_tags(&["TITLE=Track"])));
        bytes.extend(ogg_page(0x00, real_granule, 22, 2, &[0u8; 8]));
        bytes.extend(ogg_page(0x04, u64::MAX, 22, 3, &[0u8; 8]));

        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = read_ogg(&mut src, &mut scratch, &mut c).unwrap();

        assert_eq!(facts.total_frames, Some(real_granule));
    }

    #[test]
    fn vorbis_total_frames_from_last_page_granule() {
        let granule: u64 = 132_300; // 3 s @ 44.1 kHz
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, 0, 23, 0, &vorbis_id_header(44_100, 2)));
        bytes.extend(ogg_page(
            0x00,
            0,
            23,
            1,
            &vorbis_comment_header(&["TITLE=Song"]),
        ));
        bytes.extend(ogg_page(0x04, granule, 23, 2, &[0u8; 8]));

        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = read_ogg(&mut src, &mut scratch, &mut c).unwrap();

        assert_eq!(facts.sample_rate, Some(44_100));
        assert_eq!(facts.total_frames, Some(granule));
    }

    /// 34-byte FLAC STREAMINFO body with the given facts (16-bit samples).
    fn flac_streaminfo(sr: u32, ch: u32, total: u64) -> [u8; 34] {
        let mut si = [0u8; 34];
        let bps: u32 = 16;
        si[10] = (sr >> 12) as u8;
        si[11] = (sr >> 4) as u8;
        si[12] =
            (((sr & 0xF) as u8) << 4) | ((((ch - 1) & 0x7) as u8) << 1) | (((bps - 1) >> 4) as u8);
        si[13] = ((((bps - 1) & 0xF) as u8) << 4) | (((total >> 32) & 0xF) as u8);
        si[14] = (total >> 24) as u8;
        si[15] = (total >> 16) as u8;
        si[16] = (total >> 8) as u8;
        si[17] = total as u8;
        si
    }

    /// The FLAC-in-Ogg first packet: mapping header + `fLaC` + STREAMINFO block.
    fn flac_ogg_packet1(sr: u32, ch: u32, total: u64) -> Vec<u8> {
        let mut p = Vec::new();
        p.push(0x7F);
        p.extend_from_slice(b"FLAC");
        p.push(1); // major version
        p.push(0); // minor version
        p.extend_from_slice(&1u16.to_be_bytes()); // header packet count (advisory)
        p.extend_from_slice(b"fLaC");
        p.push(0x00); // STREAMINFO block header: type 0, not last
        p.extend_from_slice(&[0, 0, 34]); // 24-bit BE length
        p.extend_from_slice(&flac_streaminfo(sr, ch, total));
        p
    }

    /// A FLAC VORBIS_COMMENT metadata block as its own (last) Ogg packet.
    fn flac_ogg_comment_packet(comments: &[&str]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&6u32.to_le_bytes());
        body.extend_from_slice(b"vendor");
        body.extend_from_slice(&(comments.len() as u32).to_le_bytes());
        for c in comments {
            body.extend_from_slice(&(c.len() as u32).to_le_bytes());
            body.extend_from_slice(c.as_bytes());
        }
        let mut p = Vec::new();
        p.push(0x84); // last-block flag | type 4 (VORBIS_COMMENT)
        p.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]); // 24-bit BE length
        p.extend_from_slice(&body);
        p
    }

    #[test]
    fn flac_in_ogg_yields_title_and_streaminfo_facts() {
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(
            0x02,
            0,
            42,
            0,
            &flac_ogg_packet1(44_100, 2, 132_300),
        )); // BOS
        bytes.extend(ogg_page(
            0x00,
            132_300,
            42,
            1,
            &flac_ogg_comment_packet(&["TITLE=Aria", "ARTIST=Kimiko"]),
        ));

        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = read_ogg(&mut src, &mut scratch, &mut c).unwrap();

        // Facts come from STREAMINFO (frame-exact), not the granule tail-scan.
        assert_eq!(facts.sample_rate, Some(44_100));
        assert_eq!(facts.channels, Some(2));
        assert_eq!(facts.total_frames, Some(132_300));
        assert!(
            c.fields
                .contains(&(Some(Field::Title), "TITLE".into(), "Aria".into()))
        );
        assert!(
            c.fields
                .contains(&(Some(Field::Artist), "ARTIST".into(), "Kimiko".into()))
        );
    }

    #[test]
    fn flac_in_ogg_falls_back_to_granule_when_streaminfo_total_is_zero() {
        // ffmpeg writes STREAMINFO total_samples = 0 for streamed FLAC-in-Ogg;
        // the reader must recover the length from the last page's granule.
        let granule: u64 = 264_600; // 6 s @ 44.1 kHz
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, 0, 43, 0, &flac_ogg_packet1(44_100, 2, 0))); // total=0
        bytes.extend(ogg_page(
            0x00,
            0,
            43,
            1,
            &flac_ogg_comment_packet(&["TITLE=X"]),
        ));
        bytes.extend(ogg_page(0x04, granule, 43, 2, &[0u8; 8])); // EOS page

        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = read_ogg(&mut src, &mut scratch, &mut c).unwrap();

        assert_eq!(facts.sample_rate, Some(44_100));
        assert_eq!(facts.total_frames, Some(granule));
    }

    /// Build an Ogg page with an *explicit* lacing table and body (unlike
    /// [`ogg_page`], which derives lacing from the body length) — needed to
    /// split one packet across two pages for the cross-page reassembly test.
    #[cfg(feature = "alloc")]
    fn raw_page(header_type: u8, serial: u32, seq: u32, lacing: &[u8], body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"OggS");
        v.push(0);
        v.push(header_type);
        v.extend_from_slice(&0u64.to_le_bytes());
        v.extend_from_slice(&serial.to_le_bytes());
        v.extend_from_slice(&seq.to_le_bytes());
        v.extend_from_slice(&[0, 0, 0, 0]);
        v.push(lacing.len() as u8);
        v.extend_from_slice(lacing);
        v.extend_from_slice(body);
        v
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn opus_embedded_picture_round_trips() {
        let png = b"\x89PNG\r\n\x1a\nopus-cover-bytes";
        let comment = crate::testfix::metadata_block_picture(3, b"image/png", 1, 1, png);
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, 0, 1, 0, &opus_head(48_000, 2)));
        bytes.extend(ogg_page(0x00, 0, 1, 1, &opus_tags(&["TITLE=x", &comment])));

        let mut src = SliceSource::new(&bytes);
        let got = read_ogg_picture(&mut src, 0).unwrap();
        assert_eq!(got, png);
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn vorbis_embedded_picture_round_trips() {
        // The same code path drives Vorbis-in-Ogg, not just Opus.
        let png = b"\x89PNG\r\n\x1a\nvorbis-cover";
        let comment = crate::testfix::metadata_block_picture(3, b"image/png", 1, 1, png);
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, 0, 7, 0, &vorbis_id_header(44_100, 2)));
        bytes.extend(ogg_page(0x00, 0, 7, 1, &vorbis_comment_header(&[&comment])));

        let mut src = SliceSource::new(&bytes);
        let got = read_ogg_picture(&mut src, 0).unwrap();
        assert_eq!(got, png);
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn read_ogg_picture_selects_by_ordinal() {
        let png0 = b"first-image-000";
        let png1 = b"second-image-11";
        let c0 = crate::testfix::metadata_block_picture(3, b"image/png", 1, 1, png0);
        let c1 = crate::testfix::metadata_block_picture(4, b"image/jpeg", 2, 2, png1);
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, 0, 3, 0, &opus_head(48_000, 2)));
        bytes.extend(ogg_page(0x00, 0, 3, 1, &opus_tags(&[&c0, "TITLE=x", &c1])));

        let mut src = SliceSource::new(&bytes);
        assert_eq!(read_ogg_picture(&mut src, 0).unwrap(), png0);
        // read_ogg_picture re-seeks to 0 itself, so the same src serves again.
        assert_eq!(read_ogg_picture(&mut src, 1).unwrap(), png1);
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn read_ogg_picture_reassembles_value_across_a_page_boundary() {
        // A cover big enough that the OpusTags packet spans two pages — the
        // classic Ogg paging trap. The PacketReader must stitch it back.
        let png: Vec<u8> = (0..600u32).map(|i| i as u8).collect();
        let comment = crate::testfix::metadata_block_picture(3, b"image/png", 4, 4, &png);
        let tags = opus_tags(&[&comment]);

        let full = lacing_for(tags.len());
        assert!(
            full.len() >= 2 && full[0] == 255,
            "test needs a multi-segment packet"
        );
        // Page A carries the first (continuation) 255-byte segment, page B the
        // remaining lacing/body — a genuine cross-page packet.
        let a_bytes = 255usize;
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, 0, 9, 0, &opus_head(48_000, 2)));
        bytes.extend(raw_page(0x00, 9, 1, &full[..1], &tags[..a_bytes]));
        bytes.extend(raw_page(0x00, 9, 2, &full[1..], &tags[a_bytes..]));

        let mut src = SliceSource::new(&bytes);
        let got = read_ogg_picture(&mut src, 0).unwrap();
        assert_eq!(got, png);
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn read_ogg_picture_missing_ordinal_is_malformed() {
        let comment = crate::testfix::metadata_block_picture(3, b"image/png", 1, 1, b"img0");
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, 0, 4, 0, &opus_head(48_000, 2)));
        bytes.extend(ogg_page(0x00, 0, 4, 1, &opus_tags(&[&comment])));

        let mut src = SliceSource::new(&bytes);
        assert_eq!(
            read_ogg_picture(&mut src, 1).unwrap_err(),
            TagError::Malformed
        );
    }
}
