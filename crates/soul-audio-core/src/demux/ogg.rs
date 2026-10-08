//! Minimal Ogg page reader that presents **FLAC-in-Ogg** as a native FLAC byte
//! stream, so [`FlacDecoder`](crate::formats::flac::FlacDecoder) decodes it
//! unchanged.
//!
//! Ogg framing is stripped on the fly: the reader reconstructs the native
//! `fLaC` + STREAMINFO prelude (with the last-metadata-block flag forced on) from
//! the first packet's `\x7FFLAC` mapping header, skips the remaining header
//! packets (VORBIS_COMMENT, …), then streams the audio pages' body bytes (the
//! FLAC frames). Since the FLAC codec re-syncs on the frame sync code, audio
//! packet boundaries need not be preserved.

use embedded_io::{ErrorType, Read, SeekFrom};

use crate::decoder::DecodeError;
use crate::io::{SeekSource, Source};

/// An [`embedded_io::Read`] adapter over an Ogg stream that yields the embedded
/// native FLAC bitstream.
pub struct OggReader<S> {
    inner: S,
    /// Current page's segment table and cursor.
    segs: [u8; 255],
    nsegs: usize,
    seg_i: usize,
    seg_off: usize,
    /// Reconstructed `fLaC` + STREAMINFO(last-flag) prelude.
    prelude: [u8; 42],
    prelude_pos: usize,
    /// Count of header packets after the mapping packet (VORBIS_COMMENT, …) —
    /// retained so [`Self::restart_audio`] can re-skip the same prefix on a seek.
    num_headers: u16,
}

impl<S: Source> OggReader<S> {
    /// Parse the Ogg headers and position at the first audio page.
    ///
    /// # Errors
    /// [`DecodeError::Malformed`]/`Unsupported` for a non-FLAC or malformed Ogg
    /// stream, or a read error.
    pub fn new(inner: S) -> Result<Self, DecodeError> {
        let mut r = Self {
            inner,
            segs: [0; 255],
            nsegs: 0,
            seg_i: 0,
            seg_off: 0,
            prelude: [0; 42],
            prelude_pos: 0,
            num_headers: 0,
        };
        // BOS page, then packet 0 = the FLAC-in-Ogg mapping header.
        if !r.next_page()? {
            return Err(DecodeError::Malformed);
        }
        let mut p0 = [0u8; 64];
        let mut n = 0usize;
        r.consume_packet(|b| {
            if n < p0.len() {
                p0[n] = b;
            }
            n += 1;
        })?;
        // \x7F "FLAC", major/minor version, header count (BE), then the native
        // stream: "fLaC" + STREAMINFO block (4-byte header + 34-byte body).
        if n < 51 || &p0[0..5] != b"\x7FFLAC" || &p0[9..13] != b"fLaC" {
            return Err(DecodeError::Unsupported);
        }
        let num_headers = u16::from_be_bytes([p0[7], p0[8]]);
        r.num_headers = num_headers;
        r.prelude[0..4].copy_from_slice(b"fLaC");
        r.prelude[4] = p0[13] | 0x80; // STREAMINFO header, force last-block flag
        r.prelude[5..42].copy_from_slice(&p0[14..51]); // length(3) + STREAMINFO(34)

        // `num_headers` counts the header packets *after* this mapping packet
        // (VORBIS_COMMENT, …); skip them so audio frames stream next.
        for _ in 0..num_headers {
            if !r.consume_packet(|_| {})? {
                break;
            }
        }
        Ok(r)
    }

    /// Rewind to the first **audio** page (past the mapping + header packets),
    /// so the next [`Read::read`] yields FLAC frame bytes from the stream start
    /// — the prelude is *not* re-emitted (the FLAC demuxer already consumed it).
    /// Used by [`crate::formats::flac::FlacDecoder`]'s FLAC-in-Ogg seek, which
    /// decode-discards from here.
    ///
    /// # Errors
    /// [`DecodeError::Io`] on seek failure; [`DecodeError::Malformed`]/
    /// `UnexpectedEof` if the header prefix no longer re-parses.
    pub fn restart_audio(&mut self) -> Result<(), DecodeError>
    where
        S: SeekSource,
    {
        self.inner
            .seek(SeekFrom::Start(0))
            .map_err(|_| DecodeError::Io)?;
        self.segs = [0; 255];
        self.nsegs = 0;
        self.seg_i = 0;
        self.seg_off = 0;
        self.prelude_pos = self.prelude.len(); // already delivered — don't re-emit
        if !self.next_page()? {
            return Err(DecodeError::Malformed);
        }
        // Re-skip the mapping packet (1) + the `num_headers` header packets,
        // matching `new`, to land on the first audio page.
        for _ in 0..=self.num_headers {
            if !self.consume_packet(|_| {})? {
                break;
            }
        }
        Ok(())
    }

    /// Reposition to an **audio** page whose granule is `< target` (O(log n)
    /// byte bisection on page granule), so a caller decoding forward reaches
    /// `target` after dropping at most one page of frames. In FLAC-in-Ogg the
    /// granule IS the per-channel sample number, so this is a direct sample
    /// bisection. Header pages (granule 0) are skipped; if `target` falls within
    /// the first audio page (no earlier audio page exists) it rewinds to the
    /// first audio page via [`Self::restart_audio`]. The prelude is not
    /// re-emitted. The caller (FLAC decoder) anchors its exact position from the
    /// first decoded frame's coded number, so landing slightly early — or on a
    /// page that starts mid-frame — stays sample-accurate.
    ///
    /// # Errors
    /// [`DecodeError::Io`] on seek/read failure; [`DecodeError::Malformed`] if a
    /// landing page can't be loaded.
    pub fn seek_to_granule(&mut self, target: u64) -> Result<(), DecodeError>
    where
        S: SeekSource,
    {
        if target == 0 {
            return self.restart_audio();
        }
        match bisect_last_audio_page_before(&mut self.inner, target)? {
            Some(page_start) => {
                self.inner
                    .seek(SeekFrom::Start(page_start))
                    .map_err(|_| DecodeError::Io)?;
                self.segs = [0; 255];
                self.nsegs = 0;
                self.seg_i = 0;
                self.seg_off = 0;
                self.prelude_pos = self.prelude.len(); // already delivered
                if !self.next_page()? {
                    return Err(DecodeError::Malformed);
                }
                Ok(())
            }
            // `target` is inside the first audio page — start from the top.
            None => self.restart_audio(),
        }
    }

    /// Read the next Ogg page header + segment table. `false` at a clean EOF.
    fn next_page(&mut self) -> Result<bool, DecodeError> {
        let mut h = [0u8; 27];
        let mut got = 0;
        while got < 27 {
            match self.inner.read(&mut h[got..]) {
                Ok(0) => {
                    return if got == 0 {
                        Ok(false) // clean end between pages
                    } else {
                        Err(DecodeError::UnexpectedEof)
                    };
                }
                Ok(k) => got += k,
                Err(_) => return Err(DecodeError::Io),
            }
        }
        if &h[0..4] != b"OggS" {
            return Err(DecodeError::Malformed);
        }
        let nsegs = usize::from(h[26]);
        let mut i = 0;
        while i < nsegs {
            match self.inner.read(&mut self.segs[i..nsegs]) {
                Ok(0) => return Err(DecodeError::UnexpectedEof),
                Ok(k) => i += k,
                Err(_) => return Err(DecodeError::Io),
            }
        }
        self.nsegs = nsegs;
        self.seg_i = 0;
        self.seg_off = 0;
        Ok(true)
    }

    /// Read one body byte from the inner stream.
    fn inner_byte(&mut self) -> Result<u8, DecodeError> {
        let mut b = [0u8; 1];
        match self.inner.read(&mut b) {
            Ok(0) => Err(DecodeError::UnexpectedEof),
            Ok(_) => Ok(b[0]),
            Err(_) => Err(DecodeError::Io),
        }
    }

    /// Consume one packet (segments until a `< 255`-length segment), feeding its
    /// body bytes to `sink`. `false` at end of stream before any segment.
    fn consume_packet(&mut self, mut sink: impl FnMut(u8)) -> Result<bool, DecodeError> {
        loop {
            if self.seg_i >= self.nsegs {
                if !self.next_page()? {
                    return Ok(false);
                }
                continue;
            }
            let seg_len = usize::from(self.segs[self.seg_i]);
            while self.seg_off < seg_len {
                let b = self.inner_byte()?;
                self.seg_off += 1;
                sink(b);
            }
            self.seg_i += 1;
            self.seg_off = 0;
            if seg_len < 255 {
                return Ok(true); // packet ended
            }
        }
    }

    /// Next audio body byte (packet boundaries ignored). `None` at end of stream.
    fn audio_byte(&mut self) -> Result<Option<u8>, DecodeError> {
        loop {
            if self.seg_i < self.nsegs {
                let seg_len = usize::from(self.segs[self.seg_i]);
                if self.seg_off < seg_len {
                    self.seg_off += 1;
                    return Ok(Some(self.inner_byte()?));
                }
                self.seg_i += 1;
                self.seg_off = 0;
                continue;
            }
            if !self.next_page()? {
                return Ok(None);
            }
        }
    }
}

/// Read exactly `buf.len()` bytes or error.
fn read_full<S: Source>(src: &mut S, buf: &mut [u8]) -> Result<(), DecodeError> {
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

/// Forward byte-sync scan from `start` for the next `OggS` page; returns
/// `(page_start, granule, page_len)`. `None` at end of stream before a sync.
/// A byte-sync scan (matching `OggS` + trusting the header), like every other
/// Ogg reader here — no page-CRC validation.
fn scan_page_at_or_after<S: SeekSource>(
    src: &mut S,
    start: u64,
) -> Result<Option<(u64, u64, u64)>, DecodeError> {
    src.seek(SeekFrom::Start(start))
        .map_err(|_| DecodeError::Io)?;
    let mut window = [0u8; 4];
    let mut filled = 0usize;
    let mut pos = start;
    loop {
        let mut b = [0u8; 1];
        match src.read(&mut b) {
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
            let mut rest = [0u8; 23]; // version..page_segments (granule at [2..10], LE)
            read_full(src, &mut rest)?;
            let granule = u64::from_le_bytes(rest[2..10].try_into().unwrap());
            let seg_count = usize::from(rest[22]);
            let mut lacing = [0u8; 255];
            read_full(src, &mut lacing[..seg_count])?;
            let body: u64 = lacing[..seg_count].iter().map(|&v| u64::from(v)).sum();
            return Ok(Some((page_start, granule, 27 + seg_count as u64 + body)));
        }
    }
}

/// Byte-bisect for the start of the last **audio** page (granule `> 0`) whose
/// granule is `< target`. Header pages (granule `0`) and `u64::MAX` ("no packet
/// finished") pages are stepped past, never chosen. `None` if no such page
/// exists (`target` is within the first audio page). Pure function of `src` —
/// deliberately independent of `OggReader`'s page/segment state and of the
/// `OggPackets` reader, so it can't regress the Vorbis/Opus seek path.
fn bisect_last_audio_page_before<S: SeekSource>(
    src: &mut S,
    target: u64,
) -> Result<Option<u64>, DecodeError> {
    let file_len = src.seek(SeekFrom::End(0)).map_err(|_| DecodeError::Io)?;
    let mut lo: u64 = 0;
    let mut hi: u64 = file_len;
    let mut best: Option<u64> = None;
    let mut iters = 0usize;
    while lo < hi && iters < 128 {
        iters += 1;
        let mid = lo + (hi - lo) / 2;
        match scan_page_at_or_after(src, mid)? {
            None => {
                if mid == lo {
                    break;
                }
                hi = mid;
            }
            Some((page_start, granule, page_len)) => {
                if page_start >= hi {
                    if mid == lo {
                        break;
                    }
                    hi = mid;
                    continue;
                }
                let next = page_start + page_len;
                if granule != u64::MAX && granule > 0 && granule < target {
                    best = Some(page_start); // audio page strictly before target
                    if next <= lo {
                        break;
                    }
                    lo = next;
                } else if granule == u64::MAX || granule == 0 || granule < target {
                    // header / no-packet / (defensive) — advance past it.
                    if next <= lo {
                        break;
                    }
                    lo = next;
                } else {
                    // granule >= target: target is at or before this page.
                    if page_start <= lo {
                        break;
                    }
                    hi = page_start;
                }
            }
        }
    }
    Ok(best)
}

impl<S: Source> ErrorType for OggReader<S> {
    type Error = embedded_io::ErrorKind;
}

impl<S: Source> Read for OggReader<S> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        let mut w = 0;
        // Emit the reconstructed native-FLAC prelude first.
        while w < buf.len() && self.prelude_pos < self.prelude.len() {
            buf[w] = self.prelude[self.prelude_pos];
            self.prelude_pos += 1;
            w += 1;
        }
        // Then the audio frame bytes.
        while w < buf.len() {
            match self
                .audio_byte()
                .map_err(|_| embedded_io::ErrorKind::Other)?
            {
                Some(b) => {
                    buf[w] = b;
                    w += 1;
                }
                None => break,
            }
        }
        Ok(w)
    }
}
