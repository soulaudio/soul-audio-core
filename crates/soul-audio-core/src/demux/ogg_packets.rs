//! Packet-preserving Ogg demuxer.
//!
//! Ports the logic from `soul-audio-tags`'s `PageWalker`/`PacketReader`
//! (core may not depend on `soul-audio-tags` — see `architecture-tests`'
//! layering test), adapted to core's [`Source`]/[`SeekSource`] traits.
//!
//! Separate from the existing [`super::ogg::OggReader`], which byte-flattens
//! FLAC-in-Ogg into a native FLAC stream and discards packet boundaries.
//! Vorbis is a packet codec: the (future) Vorbis decoder needs whole packets
//! and each page's granule position, so this reader preserves both.

use embedded_io::SeekFrom;

use super::{read_exact, skip};
use crate::decoder::DecodeError;
use crate::io::{SeekSource, Source};

/// Maximum lacing-table entries in one Ogg page (one byte's worth).
const MAX_SEGMENTS: usize = 255;

/// How much of the file's tail [`OggPackets::end_granule`] pulls into a stack
/// buffer when scanning backward for the last page's granule. An Ogg page
/// tops out around 65307 bytes (255 lacing segments, each up to 255 bytes),
/// so 64 KiB comfortably covers even a maximal final page while staying a
/// bounded, stack-sized read rather than an unbounded whole-file scan.
const GRANULE_SCAN_WINDOW: usize = 65536;

/// Hard cap on [`OggPackets::seek_to_granule`]'s bisection steps — a safety
/// net (not a normal-case bound: `log2` of any realistic file size converges
/// in well under this) guaranteeing the loop terminates even against an
/// adversarial/malformed stream.
const BISECT_MAX_ITERS: usize = 64;

/// Streaming Ogg packet reader over a [`Source`]. Preserves packet
/// boundaries and the granule position of each page (Vorbis needs both).
/// Holds only the current page's lacing table (bounded, [`MAX_SEGMENTS`]
/// bytes) — never buffers page or packet payload bytes itself.
pub struct OggPackets<S> {
    src: S,
    /// Current page's lacing (segment-length) table.
    lacing: [u8; MAX_SEGMENTS],
    /// Number of valid entries in `lacing`.
    seg_count: usize,
    /// Index of the next unconsumed entry in `lacing`.
    seg_idx: usize,
    /// Logical bitstream serial number, captured from the first page.
    serial: u32,
    /// Granule position of the most recently loaded page.
    last_granule: u64,
    /// Absolute granule of the page immediately preceding the page
    /// [`Self::seek_to_granule`] most recently landed on. See
    /// [`Self::landed_floor_granule`]'s doc comment. `0` until the first
    /// [`Self::seek_to_granule`] call (matching "no preceding page").
    floor_granule: u64,
}

impl<S: Source> OggPackets<S> {
    /// Parse the first Ogg page and position at its first packet.
    ///
    /// # Errors
    /// [`DecodeError::Malformed`] if the stream has no pages at all, or the
    /// first page's capture pattern doesn't match `OggS`; propagates I/O and
    /// truncation errors from the first page's header read.
    pub fn new(src: S) -> Result<Self, DecodeError> {
        let mut r = Self {
            src,
            lacing: [0u8; MAX_SEGMENTS],
            seg_count: 0,
            seg_idx: 0,
            serial: 0,
            last_granule: 0,
            floor_granule: 0,
        };
        if !r.load_page()? {
            return Err(DecodeError::Malformed);
        }
        Ok(r)
    }

    /// Read one Ogg page's fixed header + lacing table, leaving `src`
    /// positioned at the start of the page's segment data. `Ok(false)` at a
    /// *clean* end of stream (0 bytes where a page header was expected);
    /// anything short of a full header is [`DecodeError::UnexpectedEof`].
    fn load_page(&mut self) -> Result<bool, DecodeError> {
        let mut header = [0u8; 27];
        let mut got = 0usize;
        while got < header.len() {
            match self.src.read(&mut header[got..]) {
                Ok(0) => {
                    return if got == 0 {
                        Ok(false)
                    } else {
                        Err(DecodeError::UnexpectedEof)
                    };
                }
                Ok(k) => got += k,
                Err(_) => return Err(DecodeError::Io),
            }
        }
        if &header[0..4] != b"OggS" {
            return Err(DecodeError::Malformed);
        }
        let granule = u64::from_le_bytes(header[6..14].try_into().unwrap());
        let serial = u32::from_le_bytes(header[14..18].try_into().unwrap());
        let seg_count = usize::from(header[26]);
        read_exact(&mut self.src, &mut self.lacing[..seg_count])?;
        self.seg_count = seg_count;
        self.seg_idx = 0;
        self.serial = serial;
        self.last_granule = granule;
        Ok(true)
    }

    /// Read the next whole packet into `buf`, crossing page boundaries as
    /// needed (a lacing value of 255 continues the packet — into the next
    /// segment, or, if it's the page's last segment, into the next page).
    /// Returns the number of bytes copied, or `Ok(0)` at a clean end of
    /// stream (no more pages).
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] if the packet is larger than `buf`
    /// (rather than silently truncating it, unlike the tags crate's
    /// text-field reader — a Vorbis packet must be bit-exact for the codec,
    /// so a partial copy would corrupt decode instead of merely losing a
    /// tag). [`DecodeError::UnexpectedEof`] if the stream ends mid-packet
    /// (after at least one segment was consumed but before a terminating
    /// segment `< 255` is found).
    pub fn next_packet(&mut self, buf: &mut [u8]) -> Result<usize, DecodeError> {
        self.read_packet(buf, true)
    }

    /// Read the next packet's leading bytes into `buf` (up to `buf.len()`) and
    /// **discard the remainder** of that packet, unlike [`Self::next_packet`],
    /// which rejects an over-long packet as [`DecodeError::Unsupported`].
    /// Returns the number of prefix bytes copied, or `Ok(0)` at a clean end of
    /// stream.
    ///
    /// This is for the Vorbis **comment header**, which is validated only by
    /// its leading type byte + `"vorbis"` magic but may be arbitrarily large
    /// (it can carry a base64 `METADATA_BLOCK_PICTURE` cover image). Buffering
    /// the whole thing would force a large scratch buffer or spuriously reject
    /// a perfectly playable file; the decoder only needs the prefix, so the
    /// tail is skipped over.
    ///
    /// # Errors
    /// Same truncation/I-O errors as [`Self::next_packet`]:
    /// [`DecodeError::UnexpectedEof`] if the stream ends mid-packet.
    pub fn read_packet_prefix(&mut self, buf: &mut [u8]) -> Result<usize, DecodeError> {
        self.read_packet(buf, false)
    }

    /// Shared packet reassembly for [`Self::next_packet`] (`strict`) and
    /// [`Self::read_packet_prefix`] (non-`strict`). In `strict` mode a packet
    /// larger than `buf` is [`DecodeError::Unsupported`]; otherwise the
    /// overflow is silently skipped and the truncated prefix returned.
    fn read_packet(&mut self, buf: &mut [u8], strict: bool) -> Result<usize, DecodeError> {
        let mut copied = 0usize;
        let mut started = false;
        let mut overflowed = false;
        loop {
            while self.seg_idx >= self.seg_count {
                if !self.load_page()? {
                    return if started {
                        Err(DecodeError::UnexpectedEof)
                    } else {
                        Ok(0)
                    };
                }
            }
            let seg_len = usize::from(self.lacing[self.seg_idx]);
            self.seg_idx += 1;
            started = true;

            let space = buf.len().saturating_sub(copied);
            let take = seg_len.min(space);
            if take > 0 {
                read_exact(&mut self.src, &mut buf[copied..copied + take])?;
            }
            if seg_len > take {
                skip(&mut self.src, (seg_len - take) as u64)?;
                overflowed = true;
            }
            copied += take;

            if seg_len < MAX_SEGMENTS {
                // A lacing byte < 255 marks the end of the packet.
                break;
            }
        }
        if strict && overflowed {
            return Err(DecodeError::Unsupported);
        }
        Ok(copied)
    }

    /// Granule position of the most recently loaded page (for
    /// `total_frames`/position tracking while decoding forward).
    #[must_use]
    pub fn last_page_granule(&self) -> u64 {
        self.last_granule
    }

    /// Absolute granule of the page immediately preceding the page
    /// [`Self::seek_to_granule`] most recently landed on — i.e. the total
    /// per-channel sample count already accounted for as of the **start** of
    /// the landed page's audio, as opposed to [`Self::last_page_granule`]
    /// (that page's own granule: the count through its **end**). `0` if the
    /// landed page was the very first page in the stream (no predecessor).
    ///
    /// Lets a caller — the Vorbis decoder's `seek_to` — initialize an
    /// absolute running sample counter at the correct baseline after a seek,
    /// so a later granule end-trim comparison (`ceiling - counter`) stays
    /// exact all the way to the true final block instead of drifting by the
    /// landed page's whole sample span (the Task 10 review finding this
    /// fixes: `samples_emitted` reset to `0` after a seek while the ceiling
    /// stayed absolute).
    #[must_use]
    pub fn landed_floor_granule(&self) -> u64 {
        self.floor_granule
    }

    /// The logical bitstream serial number, from the first (BOS) page.
    #[must_use]
    pub fn serial(&self) -> u32 {
        self.serial
    }
}

impl<S: SeekSource> OggPackets<S> {
    /// Seek to the first page whose granule position is `>= target`, found
    /// by bisecting over byte offsets (never a linear scan). Leaves the
    /// reader positioned at that page's first packet. Returns the landed
    /// page's granule; [`Self::landed_floor_granule`] exposes the
    /// PRECEDING page's granule (the absolute sample count as of the start
    /// of the landed page), needed to keep an absolute running counter
    /// exact across a seek.
    ///
    /// A page whose granule is `u64::MAX` ("no packet finished on this
    /// page") carries no usable ordering information and is skipped
    /// leftward. If no page with `granule >= target` exists (`target`
    /// beyond the stream's last real granule), lands on the last page found
    /// by the bisection instead of failing — the caller gets the closest
    /// available position rather than an error.
    ///
    /// This is a byte-sync scan (matching `OggS` and trusting the header),
    /// not a CRC-verified page scan — like every other Ogg reader in this
    /// codebase, it does not validate the page checksum.
    ///
    /// # Errors
    /// [`DecodeError::Io`] on a seek/read failure; [`DecodeError::Malformed`]
    /// if no page can be found at all.
    pub fn seek_to_granule(&mut self, target: u64) -> Result<u64, DecodeError> {
        let file_len = self
            .src
            .seek(SeekFrom::End(0))
            .map_err(|_| DecodeError::Io)?;
        let mut lo: u64 = 0;
        let mut hi: u64 = file_len;
        let mut best: Option<u64> = None; // byte offset of the best candidate page
        // The last page actually observed during the bisection, regardless
        // of which way it steered `lo`/`hi` — the fallback landing spot when
        // no page with `granule >= target` exists (target beyond the
        // stream's end).
        let mut last_seen: Option<u64> = None;
        let mut iters = 0usize;

        while lo < hi && iters < BISECT_MAX_ITERS {
            iters += 1;
            let mid = lo + (hi - lo) / 2;
            match self.scan_page_at_or_after(mid)? {
                None => {
                    if mid == lo {
                        break;
                    }
                    hi = mid;
                }
                Some((page_start, granule, page_len)) => {
                    if page_start >= hi {
                        // No page boundary between mid and hi at all.
                        if mid == lo {
                            break;
                        }
                        hi = mid;
                        continue;
                    }
                    last_seen = Some(page_start);
                    if granule == u64::MAX {
                        if page_start <= lo {
                            break;
                        }
                        hi = page_start;
                    } else if granule < target {
                        let next = page_start + page_len;
                        if next <= lo {
                            break;
                        }
                        lo = next;
                    } else {
                        best = Some(page_start);
                        if page_start <= lo {
                            break;
                        }
                        hi = page_start;
                    }
                }
            }
        }

        let landing = match best.or(last_seen) {
            Some(p) => p,
            None => self
                .scan_page_at_or_after(lo)?
                .map(|(page_start, _, _)| page_start)
                .ok_or(DecodeError::Malformed)?,
        };

        // The bisection above only ever needs to VISIT a page whose granule
        // brackets `target` — it can (and, for a page bundling much of the
        // file, routinely does) jump straight past other real pages that sit
        // strictly between the old `lo` and the page it lands on. So no
        // running state kept *during* the bisection can be trusted as "the
        // granule of the page immediately before `landing`"; that floor is
        // instead recovered by a dedicated backward scan below, once
        // `landing` itself is settled.
        let floor = self.floor_before(landing)?;

        self.src
            .seek(SeekFrom::Start(landing))
            .map_err(|_| DecodeError::Io)?;
        if !self.load_page()? {
            return Err(DecodeError::Malformed);
        }
        self.floor_granule = floor;
        Ok(self.last_granule)
    }

    /// Absolute granule of the real (non-`u64::MAX`) page immediately
    /// preceding byte offset `landing` — the "floor" [`Self::seek_to_granule`]
    /// stores for [`Self::landed_floor_granule`]. `landing == 0` (no
    /// predecessor possible) trivially returns `0`; otherwise scans backward
    /// via [`Self::scan_page_before`], hopping across any `u64::MAX` ("no
    /// packet finished on this page") pages found along the way — bounded to
    /// a small fixed number of hops, each a bounded [`GRANULE_SCAN_WINDOW`]
    /// backward scan, so this stays a bounded operation, never a linear scan
    /// of the whole file. `0` (best-effort, matching `seek_to_granule`'s own
    /// "closest available" philosophy) if no predecessor is found within
    /// that budget rather than erroring the whole seek.
    fn floor_before(&mut self, mut landing: u64) -> Result<u64, DecodeError> {
        const MAX_FLOOR_HOPS: usize = 8;
        for _ in 0..MAX_FLOOR_HOPS {
            if landing == 0 {
                return Ok(0);
            }
            match self.scan_page_before(landing)? {
                None => return Ok(0),
                Some((page_start, granule, _page_len)) => {
                    if granule != u64::MAX {
                        return Ok(granule);
                    }
                    landing = page_start; // hop across the MAX-granule page.
                }
            }
        }
        Ok(0)
    }

    /// Scan backward (bounded, [`GRANULE_SCAN_WINDOW`]) from byte offset
    /// `end` for the page that ends **exactly** there, returning
    /// `(page_start, granule, page_len)`. Unlike
    /// [`Self::scan_page_at_or_after`] (a byte-sync forward scan trusted
    /// as-is), a raw backward byte scan can false-positive on an
    /// `OggS`-looking byte sequence inside packet payload data, so every
    /// backward `OggS` candidate is verified by fully parsing it forward
    /// (via [`Self::scan_page_at_or_after`]) and checking it actually chains
    /// up to `end` (`page_start + page_len == end`) before being accepted;
    /// a false match is skipped and the backward scan continues. `Ok(None)`
    /// if no such page is found within the window.
    fn scan_page_before(&mut self, end: u64) -> Result<Option<(u64, u64, u64)>, DecodeError> {
        let window = end.min(GRANULE_SCAN_WINDOW as u64);
        let win_start = end - window;
        let wlen = window as usize;
        if wlen < 4 {
            return Ok(None);
        }
        self.src
            .seek(SeekFrom::Start(win_start))
            .map_err(|_| DecodeError::Io)?;
        let mut buf = [0u8; GRANULE_SCAN_WINDOW];
        let mut got = 0usize;
        while got < wlen {
            match self.src.read(&mut buf[got..wlen]) {
                Ok(0) => break,
                Ok(k) => got += k,
                Err(_) => return Err(DecodeError::Io),
            }
        }
        if got < 4 {
            return Ok(None);
        }
        let mut idx = got - 4;
        loop {
            if &buf[idx..idx + 4] == b"OggS" {
                let cand_start = win_start + idx as u64;
                if let Some((page_start, granule, page_len)) =
                    self.scan_page_at_or_after(cand_start)?
                {
                    if page_start == cand_start && page_start + page_len == end {
                        return Ok(Some((page_start, granule, page_len)));
                    }
                }
            }
            if idx == 0 {
                break;
            }
            idx -= 1;
        }
        Ok(None)
    }

    /// Scan forward from byte offset `start` for the next `OggS` page sync,
    /// returning `(page_start, granule, page_len)`. `Ok(None)` at end of
    /// stream before a sync is found. Used by [`Self::seek_to_granule`]'s
    /// bisection; does not touch `self`'s page/lacing state.
    fn scan_page_at_or_after(
        &mut self,
        start: u64,
    ) -> Result<Option<(u64, u64, u64)>, DecodeError> {
        self.src
            .seek(SeekFrom::Start(start))
            .map_err(|_| DecodeError::Io)?;
        let mut window = [0u8; 4];
        let mut filled = 0usize;
        let mut pos = start;
        loop {
            let mut b = [0u8; 1];
            match self.src.read(&mut b) {
                Ok(0) => return Ok(None),
                Ok(_) => {}
                Err(_) => return Err(DecodeError::Io),
            }
            if filled < 4 {
                window[filled] = b[0];
                filled += 1;
            } else {
                window.copy_within(1..4, 0);
                window[3] = b[0];
            }
            pos += 1;
            if filled == 4 && &window == b"OggS" {
                let page_start = pos - 4;
                let mut rest = [0u8; 23];
                read_exact(&mut self.src, &mut rest)?;
                let granule = u64::from_le_bytes(rest[2..10].try_into().unwrap());
                let seg_count = usize::from(rest[22]);
                let mut lacing = [0u8; MAX_SEGMENTS];
                read_exact(&mut self.src, &mut lacing[..seg_count])?;
                let body_len: u64 = lacing[..seg_count].iter().map(|&v| u64::from(v)).sum();
                let page_len = 27 + seg_count as u64 + body_len;
                return Ok(Some((page_start, granule, page_len)));
            }
        }
    }

    /// Scan backward (bounded, [`GRANULE_SCAN_WINDOW`]) from end-of-stream
    /// for the last page with a real granule (i.e. not `u64::MAX` — "no
    /// packet finished on this page") and return it.
    ///
    /// **Non-destructive:** the scan seeks to the tail internally but snapshots
    /// and restores the full reader state (byte cursor + page/lacing/granule
    /// fields) before returning, so a caller may query the duration
    /// mid-stream — e.g. [`super::super`]'s Vorbis `total_frames` before or
    /// between `decode_into` calls — and keep reading packets exactly where it
    /// left off. (The Vorbis decoder drives this over unbuffered seekable
    /// sources, for which `SeekFrom::Current(0)` is the exact byte cursor.)
    ///
    /// # Errors
    /// [`DecodeError::Io`] on a seek/read failure; [`DecodeError::Malformed`]
    /// if the tail window is too short or no page with a real granule is
    /// found within it.
    pub fn end_granule(&mut self) -> Result<u64, DecodeError> {
        // Snapshot the resumable reader state (see the doc comment) so the
        // tail scan below can never leave the reader mispositioned.
        let saved_pos = self
            .src
            .seek(SeekFrom::Current(0))
            .map_err(|_| DecodeError::Io)?;
        let saved_lacing = self.lacing;
        let saved_seg_count = self.seg_count;
        let saved_seg_idx = self.seg_idx;
        let saved_granule = self.last_granule;

        let result = self.scan_end_granule();

        // Restore to exactly the pre-scan position + page state, whatever the
        // scan returned, so sequential decoding can resume uncorrupted.
        self.src
            .seek(SeekFrom::Start(saved_pos))
            .map_err(|_| DecodeError::Io)?;
        self.lacing = saved_lacing;
        self.seg_count = saved_seg_count;
        self.seg_idx = saved_seg_idx;
        self.last_granule = saved_granule;

        result
    }

    /// The raw tail scan for [`Self::end_granule`] (which wraps it in a
    /// save/restore of the reader state). Moves `src`'s cursor to the tail and
    /// parses the last real granule out of a bounded window; on its own it
    /// leaves the reader repositioned, which is why the public method restores.
    fn scan_end_granule(&mut self) -> Result<u64, DecodeError> {
        let len = self
            .src
            .seek(SeekFrom::End(0))
            .map_err(|_| DecodeError::Io)?;
        let window = len.min(GRANULE_SCAN_WINDOW as u64) as usize;
        if window < 14 {
            return Err(DecodeError::Malformed);
        }
        self.src
            .seek(SeekFrom::Start(len - window as u64))
            .map_err(|_| DecodeError::Io)?;
        let mut buf = [0u8; GRANULE_SCAN_WINDOW];
        let mut got = 0usize;
        while got < window {
            match self.src.read(&mut buf[got..window]) {
                Ok(0) => break,
                Ok(k) => got += k,
                Err(_) => return Err(DecodeError::Io),
            }
        }
        granule_from_tail(&buf[..got]).ok_or(DecodeError::Malformed)
    }
}

/// Scan `buf` (the tail of an Ogg file) backward for the last `OggS` capture
/// pattern whose granule position is a real end-of-packet marker rather than
/// `u64::MAX`. Never panics: bails out (`None`) once the search runs off the
/// front of `buf` without finding one.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceSource;

    /// Standard Ogg lacing encoding for a packet of `len` bytes.
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

    /// Build one Ogg page (model: `soul-audio-tags`'s `ogg_page` test helper).
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

    #[test]
    fn packet_spanning_a_page_boundary_reassembles() {
        // Packet 1 (page 1, BOS): small, fits entirely on its own page.
        let packet1 = b"identification-header".to_vec();

        // Packet 2: long enough (>255, <510 bytes) that its lacing table is
        // exactly two entries: [255, remainder]. Split its raw bytes across
        // two pages at that boundary.
        let packet2: Vec<u8> = (0..400u32).map(|i| (i % 256) as u8).collect();
        let lacing2 = lacing_for(packet2.len());
        assert_eq!(lacing2.len(), 2, "fixture needs a two-segment packet");
        assert_eq!(lacing2[0], 255);
        let split_at = 255usize;

        let mut page1 = Vec::new();
        page1.extend_from_slice(b"OggS");
        page1.push(0);
        page1.push(0x02); // BOS
        page1.extend_from_slice(&0u64.to_le_bytes());
        page1.extend_from_slice(&42u32.to_le_bytes());
        page1.extend_from_slice(&0u32.to_le_bytes());
        page1.extend_from_slice(&[0, 0, 0, 0]);
        let lacing1 = lacing_for(packet1.len());
        page1.push(lacing1.len() as u8);
        page1.extend_from_slice(&lacing1);
        page1.extend_from_slice(&packet1);

        let mut page2 = Vec::new();
        page2.extend_from_slice(b"OggS");
        page2.push(0);
        page2.push(0x00);
        page2.extend_from_slice(&0u64.to_le_bytes()); // no packet finishes on this page
        page2.extend_from_slice(&42u32.to_le_bytes());
        page2.extend_from_slice(&1u32.to_le_bytes());
        page2.extend_from_slice(&[0, 0, 0, 0]);
        let seg_a = &lacing2[..1]; // [255] — continuation
        page2.push(seg_a.len() as u8);
        page2.extend_from_slice(seg_a);
        page2.extend_from_slice(&packet2[..split_at]);

        let granule_page3: u64 = 12345;
        let mut page3 = Vec::new();
        page3.extend_from_slice(b"OggS");
        page3.push(0);
        page3.push(0x00);
        page3.extend_from_slice(&granule_page3.to_le_bytes());
        page3.extend_from_slice(&42u32.to_le_bytes());
        page3.extend_from_slice(&2u32.to_le_bytes());
        page3.extend_from_slice(&[0, 0, 0, 0]);
        let seg_b = &lacing2[1..]; // [remainder] — terminates packet 2
        // A third packet on page 3: a small one, fully contained. The lacing
        // table lists ALL segments (both packets) before ANY segment data.
        let packet3 = b"third-packet".to_vec();
        let lacing3 = lacing_for(packet3.len());
        page3.push((seg_b.len() + lacing3.len()) as u8); // page_segments
        page3.extend_from_slice(seg_b);
        page3.extend_from_slice(&lacing3);
        page3.extend_from_slice(&packet2[split_at..]);
        page3.extend_from_slice(&packet3);

        let mut bytes = Vec::new();
        bytes.extend(page1);
        bytes.extend(page2);
        bytes.extend(page3);

        let mut r = OggPackets::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(r.serial(), 42);

        let mut buf = [0u8; 1024];

        let n1 = r.next_packet(&mut buf).unwrap();
        assert_eq!(&buf[..n1], &packet1[..]);

        let n2 = r.next_packet(&mut buf).unwrap();
        assert_eq!(
            &buf[..n2],
            &packet2[..],
            "cross-page packet must reassemble"
        );
        assert_eq!(r.last_page_granule(), granule_page3);

        let n3 = r.next_packet(&mut buf).unwrap();
        assert_eq!(&buf[..n3], &packet3[..]);

        assert_eq!(
            r.next_packet(&mut buf).unwrap(),
            0,
            "Ok(0) at end of stream"
        );
    }

    #[test]
    fn single_page_two_packets_then_clean_eof() {
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, 0, 7, 0, b"hello"));
        bytes.extend(ogg_page(0x04, 999, 7, 1, b"world"));

        let mut r = OggPackets::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(r.serial(), 7);

        let mut buf = [0u8; 64];
        let n = r.next_packet(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");

        let n = r.next_packet(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"world");
        assert_eq!(r.last_page_granule(), 999);

        assert_eq!(r.next_packet(&mut buf).unwrap(), 0);
        // Reading past clean EOF again is still a clean Ok(0), not an error.
        assert_eq!(r.next_packet(&mut buf).unwrap(), 0);
    }

    #[test]
    fn oversized_packet_is_unsupported_not_truncated() {
        let big = vec![0xABu8; 600];
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, 0, 1, 0, &big));

        let mut r = OggPackets::new(SliceSource::new(&bytes)).unwrap();
        let mut buf = [0u8; 64]; // far smaller than the 600-byte packet
        assert_eq!(
            r.next_packet(&mut buf).unwrap_err(),
            DecodeError::Unsupported
        );
    }

    #[test]
    fn read_packet_prefix_returns_the_prefix_and_skips_the_rest() {
        // A packet far larger than the caller's buffer: read_packet_prefix
        // copies the leading bytes and skips the tail (returning the prefix
        // length), where next_packet would reject it as Unsupported.
        let big = vec![0xABu8; 600];
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, 0, 1, 0, &big));
        bytes.extend(ogg_page(0x04, 7, 1, 1, b"after"));

        let mut r = OggPackets::new(SliceSource::new(&bytes)).unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(r.read_packet_prefix(&mut buf).unwrap(), 8);
        assert_eq!(&buf, &[0xAB; 8]);
        // The skip left the reader correctly positioned at the next packet.
        let n = r.next_packet(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"after");
    }

    #[test]
    fn truncated_page_header_is_an_error_not_a_panic() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"OggS");
        bytes.push(0);
        bytes.push(0x02);
        bytes.extend_from_slice(&[0u8; 8]); // granule
        bytes.extend_from_slice(&[0u8; 4]); // serial
        bytes.extend_from_slice(&[0u8; 4]); // seq
        bytes.extend_from_slice(&[0u8; 4]); // checksum
        bytes.push(5); // claims 5 lacing bytes but none follow

        // Truncated even before a full page header/lacing table is
        // available: `new` itself must error, not panic.
        assert!(matches!(
            OggPackets::new(SliceSource::new(&bytes)).map(|_| ()),
            Err(DecodeError::UnexpectedEof)
        ));
    }

    #[test]
    fn truncated_packet_body_mid_stream_errors_not_panics() {
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, 0, 1, 0, b"first"));
        // A second page header claiming a packet, but its body is cut short.
        bytes.extend_from_slice(b"OggS");
        bytes.push(0);
        bytes.push(0x00);
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.push(1);
        bytes.push(10); // declares a 10-byte segment
        bytes.extend_from_slice(&[1, 2, 3]); // only 3 bytes actually present

        let mut r = OggPackets::new(SliceSource::new(&bytes)).unwrap();
        let mut buf = [0u8; 64];
        let n = r.next_packet(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"first");
        assert_eq!(
            r.next_packet(&mut buf).unwrap_err(),
            DecodeError::UnexpectedEof
        );
    }

    #[test]
    fn non_ogg_stream_is_malformed() {
        let bytes = b"not an ogg stream at all...".to_vec();
        assert!(matches!(
            OggPackets::new(SliceSource::new(&bytes)).map(|_| ()),
            Err(DecodeError::Malformed)
        ));
    }

    #[test]
    fn empty_stream_is_malformed() {
        let bytes: Vec<u8> = Vec::new();
        assert!(matches!(
            OggPackets::new(SliceSource::new(&bytes)).map(|_| ()),
            Err(DecodeError::Malformed)
        ));
    }

    /// Build a multi-page stream with monotonically increasing granules, one
    /// small packet per page, for the seek tests below.
    fn multi_page_stream(granules: &[u64]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (i, &g) in granules.iter().enumerate() {
            let header_type = if i == 0 {
                0x02
            } else if i == granules.len() - 1 {
                0x04
            } else {
                0x00
            };
            let packet = alloc_packet(i);
            bytes.extend(ogg_page(header_type, g, 55, i as u32, &packet));
        }
        bytes
    }

    /// A small, distinct packet payload per page index (so a landed page can
    /// be identified by its contents).
    fn alloc_packet(i: usize) -> Vec<u8> {
        vec![i as u8; 4]
    }

    #[test]
    fn seek_to_granule_lands_on_first_page_at_or_after_target() {
        let granules = [10u64, 20, 30, 40, 50];
        let bytes = multi_page_stream(&granules);

        let mut r = OggPackets::new(SliceSource::new(&bytes)).unwrap();
        let landed = r.seek_to_granule(25).unwrap();
        assert_eq!(
            landed, 30,
            "must land on the first page with granule >= target"
        );
        assert_eq!(
            r.landed_floor_granule(),
            20,
            "floor must be the PRECEDING page's granule, not the landed page's own"
        );

        // The reader must be positioned at that page's packet, not garbage.
        let mut buf = [0u8; 16];
        let n = r.next_packet(&mut buf).unwrap();
        assert_eq!(&buf[..n], &alloc_packet(2)[..]); // page index 2 has granule 30
    }

    #[test]
    fn seek_to_granule_exact_match() {
        let granules = [10u64, 20, 30, 40, 50];
        let bytes = multi_page_stream(&granules);

        let mut r = OggPackets::new(SliceSource::new(&bytes)).unwrap();
        let landed = r.seek_to_granule(20).unwrap();
        assert_eq!(landed, 20);
        assert_eq!(r.landed_floor_granule(), 10);
    }

    #[test]
    fn seek_to_granule_beyond_end_lands_on_last_page() {
        let granules = [10u64, 20, 30];
        let bytes = multi_page_stream(&granules);

        let mut r = OggPackets::new(SliceSource::new(&bytes)).unwrap();
        let landed = r.seek_to_granule(1_000_000).unwrap();
        assert_eq!(landed, 30);
        // The "beyond end" fallback lands ON the last real page itself; its
        // floor must still be the page BEFORE it (20), not its own granule
        // (30) — a case the bisection's own internal state can't answer
        // (see `seek_to_granule`'s comment above the `floor_before` call),
        // exercising the dedicated backward-scan lookup on this landing path.
        assert_eq!(r.landed_floor_granule(), 20);
    }

    #[test]
    fn seek_to_granule_zero_lands_on_first_page() {
        let granules = [10u64, 20, 30];
        let bytes = multi_page_stream(&granules);

        let mut r = OggPackets::new(SliceSource::new(&bytes)).unwrap();
        let landed = r.seek_to_granule(0).unwrap();
        assert_eq!(landed, 10);
        assert_eq!(
            r.landed_floor_granule(),
            0,
            "the very first page has no predecessor"
        );
    }

    #[test]
    fn end_granule_scans_tail_for_last_real_granule() {
        let granules = [10u64, 20, 30];
        let bytes = multi_page_stream(&granules);

        let mut r = OggPackets::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(r.end_granule().unwrap(), 30);
    }

    #[test]
    fn end_granule_is_non_destructive_for_sequential_decode() {
        // Reading packets, querying end_granule mid-stream, then continuing must
        // yield the same packets as never calling end_granule — the Task-7
        // total_frames-coexists-with-decode guarantee at the reader level.
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, 0, 5, 0, b"first-packet"));
        bytes.extend(ogg_page(0x00, 10, 5, 1, b"second-packet"));
        bytes.extend(ogg_page(0x04, 20, 5, 2, b"third-packet"));

        let mut r = OggPackets::new(SliceSource::new(&bytes)).unwrap();
        let mut buf = [0u8; 64];
        let n1 = r.next_packet(&mut buf).unwrap();
        assert_eq!(&buf[..n1], b"first-packet");
        // Query the duration in the middle of decoding.
        assert_eq!(r.end_granule().unwrap(), 20);
        // The next packets must still read correctly (position was restored).
        let n2 = r.next_packet(&mut buf).unwrap();
        assert_eq!(&buf[..n2], b"second-packet");
        let n3 = r.next_packet(&mut buf).unwrap();
        assert_eq!(&buf[..n3], b"third-packet");
        assert_eq!(r.next_packet(&mut buf).unwrap(), 0); // clean end of stream
    }

    #[test]
    fn end_granule_skips_trailing_page_with_no_finished_packet() {
        // A granule of `u64::MAX` means "no packet finished on this page";
        // the backward scan must keep looking for an earlier real one.
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, 0, 9, 0, b"a"));
        bytes.extend(ogg_page(0x00, 42, 9, 1, b"b"));
        bytes.extend(ogg_page(0x04, u64::MAX, 9, 2, b"c"));

        let mut r = OggPackets::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(r.end_granule().unwrap(), 42);
    }
}
