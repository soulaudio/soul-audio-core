//! `std`-only writer side of the `art.blob` format: an `image`-based
//! Floyd-Steinberg encoder (`encode_cover`) and a deduping writer
//! (`ArtBlobWriter`) that lays out the header + table + bitmaps consumed by
//! [`crate::ArtBlobReader`].

use std::collections::BTreeMap;

use image::imageops::FilterType;

use crate::{ENTRY_LEN, FORMAT_VERSION, HEADER_LEN, MAGIC, row_bytes};

/// Errors from [`encode_cover`].
#[derive(Debug)]
pub enum ArtError {
    /// The input bytes could not be decoded as an image.
    Decode,
}

/// Decode `bytes` as an image, convert to grayscale, resize to
/// `width`x`height`, and Floyd-Steinberg dither to a packed 1bpp bitmap.
///
/// A set bit (`1`) means ink/dark; a clear bit (`0`) means blank/light. The
/// returned buffer is exactly `row_bytes(width) * height` bytes.
pub fn encode_cover(bytes: &[u8], width: u16, height: u16) -> Result<Vec<u8>, ArtError> {
    let img = image::load_from_memory(bytes).map_err(|_| ArtError::Decode)?;
    let gray = img.to_luma8();
    let small = image::imageops::resize(
        &gray,
        u32::from(width),
        u32::from(height),
        FilterType::Triangle,
    );

    // Floyd-Steinberg over an i16 error buffer.
    let (w, h) = (width as usize, height as usize);
    let mut px: Vec<i16> = small.as_raw().iter().map(|&p| i16::from(p)).collect();
    let rb = row_bytes(width);
    let mut out = vec![0u8; rb * h];
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            let old = px[i];
            let new = if old < 128 { 0 } else { 255 };
            if new == 0 {
                out[y * rb + (x >> 3)] |= 0x80 >> (x & 7); // 1 = ink (dark)
            }
            let err = old - new;
            let mut diffuse = |xx: isize, yy: isize, f: i16| {
                if xx >= 0 && xx < w as isize && yy < h as isize {
                    let j = yy as usize * w + xx as usize;
                    px[j] = (px[j] + err * f / 16).clamp(0, 255);
                }
            };
            diffuse(x as isize + 1, y as isize, 7);
            diffuse(x as isize - 1, y as isize + 1, 3);
            diffuse(x as isize, y as isize + 1, 5);
            diffuse(x as isize + 1, y as isize + 1, 1);
        }
    }
    Ok(out)
}

/// Builds an `art.blob`: accumulates packed 1bpp bitmaps (deduped by content
/// hash) and emits the header + offset table + bitmap bytes expected by
/// [`crate::ArtBlobReader`].
pub struct ArtBlobWriter {
    width: u16,
    height: u16,
    entries: Vec<Vec<u8>>,
    seen: BTreeMap<u64, u32>,
}

impl ArtBlobWriter {
    #[must_use]
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            entries: Vec::new(),
            seen: BTreeMap::new(),
        }
    }

    /// Adds a packed bitmap, deduping by content hash. Returns the 1-based
    /// `art_id`; adding bytes identical to a previous entry returns that
    /// entry's existing id without storing a duplicate copy.
    pub fn add(&mut self, bitmap: Vec<u8>) -> u32 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for &b in &bitmap {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0000_0001_0000_01b3);
        }
        if let Some(&id) = self.seen.get(&hash) {
            return id;
        }
        self.entries.push(bitmap);
        let id = self.entries.len() as u32; // 1-based art_id
        self.seen.insert(hash, id);
        id
    }

    /// Consumes the writer, emitting the complete `art.blob` bytes: header,
    /// then one `(offset, len)` table entry per bitmap, then the bitmap
    /// bytes themselves in insertion order.
    #[must_use]
    pub fn finish(self) -> Vec<u8> {
        let count = self.entries.len();
        let table = HEADER_LEN + count * ENTRY_LEN;
        let mut out = Vec::with_capacity(table + self.entries.iter().map(Vec::len).sum::<usize>());
        out.extend_from_slice(&MAGIC);
        out.push(FORMAT_VERSION);
        out.push(0);
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.extend_from_slice(&(count as u32).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        let mut off = table as u32;
        for e in &self.entries {
            out.extend_from_slice(&off.to_le_bytes());
            out.extend_from_slice(&(e.len() as u32).to_le_bytes());
            off += e.len() as u32;
        }
        for e in &self.entries {
            out.extend_from_slice(e);
        }
        out
    }
}
