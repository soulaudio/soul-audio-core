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

use embedded_io::{ErrorType, Read};

use crate::decoder::DecodeError;
use crate::io::Source;

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
