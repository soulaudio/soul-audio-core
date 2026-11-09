#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

#[cfg(feature = "build")]
pub mod build;

use embedded_io::SeekFrom;
use soul_audio_catalog::{ArtStore, CatalogError};
use soul_audio_core::SeekSource;

pub const MAGIC: [u8; 4] = *b"SART";
pub const FORMAT_VERSION: u8 = 1;
pub const HEADER_LEN: usize = 16;
pub const ENTRY_LEN: usize = 8;

#[must_use]
pub fn row_bytes(width: u16) -> usize {
    (width as usize).div_ceil(8)
}

fn le_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

pub struct ArtBlobReader<S> {
    src: core::cell::RefCell<S>,
    width: u16,
    height: u16,
    count: u32,
}

impl<S: SeekSource> ArtBlobReader<S> {
    #[must_use]
    pub fn new(mut src: S) -> Self {
        let mut h = [0u8; HEADER_LEN];
        let _ = src.seek(SeekFrom::Start(0));
        let _ = src.read(&mut h); // best-effort; a bad/absent blob → count 0
        let ok = h[..4] == MAGIC;
        Self {
            src: core::cell::RefCell::new(src),
            width: if ok { le_u16(&h, 6) } else { 0 },
            height: if ok { le_u16(&h, 8) } else { 0 },
            count: if ok { le_u32(&h, 10) } else { 0 },
        }
    }
    #[must_use]
    pub fn dimensions(&self) -> (u16, u16) {
        (self.width, self.height)
    }
    #[must_use]
    pub fn count(&self) -> u32 {
        self.count
    }
}

impl<S: SeekSource> ArtStore for ArtBlobReader<S> {
    fn art(&self, art_id: u32, buf: &mut [u8]) -> Result<usize, CatalogError> {
        if art_id == 0 {
            return Ok(0);
        }
        if art_id > self.count {
            return Err(CatalogError::OutOfRange);
        }
        let mut s = self.src.borrow_mut();
        let ent = HEADER_LEN as u64 + u64::from(art_id - 1) * ENTRY_LEN as u64;
        let mut e = [0u8; ENTRY_LEN];
        s.seek(SeekFrom::Start(ent))
            .map_err(|_| CatalogError::Corrupt)?;
        s.read_exact(&mut e).map_err(|_| CatalogError::Corrupt)?;
        let (off, len) = (le_u32(&e, 0), le_u32(&e, 4) as usize);
        if buf.len() < len {
            return Err(CatalogError::Truncated);
        }
        s.seek(SeekFrom::Start(u64::from(off)))
            .map_err(|_| CatalogError::Corrupt)?;
        s.read_exact(&mut buf[..len])
            .map_err(|_| CatalogError::Corrupt)?;
        Ok(len)
    }
}
