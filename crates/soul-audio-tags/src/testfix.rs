//! Shared test-only fixtures: minimal ID3v2.3 tag/frame builders and a
//! generic [`TagSink`] collector. Used by `id3v2`'s own tests and by
//! `riff`/`dsf` (both embed an ID3v2 tag inside their own container), so the
//! builders live in one place instead of being duplicated across modules.
//! Gated behind `#[cfg(test)]` at the `mod testfix;` declaration in `lib.rs`.

extern crate alloc;
use alloc::{borrow::ToOwned, string::String, vec::Vec};

use crate::{Field, Picture, TagSink};

/// Encode a 4-byte synchsafe integer (7 bits per byte, MSB first) — the
/// ID3v2 tag-size encoding.
pub(crate) fn synchsafe(n: u32) -> [u8; 4] {
    [
        ((n >> 21) & 0x7F) as u8,
        ((n >> 14) & 0x7F) as u8,
        ((n >> 7) & 0x7F) as u8,
        (n & 0x7F) as u8,
    ]
}

/// Build a v2.3 UTF-8 text frame (`id` + plain big-endian size + no flags +
/// encoding byte 0x03 + UTF-8 bytes).
pub(crate) fn text_frame(id: &[u8; 4], s: &str) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(id);
    let body_len = 1 + s.len() as u32; // enc byte + utf8
    b.extend_from_slice(&body_len.to_be_bytes()); // v2.3 plain size
    b.extend_from_slice(&[0, 0]); // flags
    b.push(0x03); // UTF-8
    b.extend_from_slice(s.as_bytes());
    b
}

/// Build an `APIC` frame (UTF-8 encoding, MIME string, picture type, empty
/// description, then raw image bytes).
pub(crate) fn apic(mime: &str, ptype: u8, data: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(0x03); // enc utf-8
    body.extend_from_slice(mime.as_bytes());
    body.push(0); // mime + nul
    body.push(ptype);
    body.push(0); // empty description + nul
    body.extend_from_slice(data);
    let mut b = Vec::new();
    b.extend_from_slice(b"APIC");
    b.extend_from_slice(&(body.len() as u32).to_be_bytes());
    b.extend_from_slice(&[0, 0]);
    b.extend_from_slice(&body);
    b
}

/// Wrap `frames` in a minimal ID3v2.3 tag header (no flags, synchsafe size).
pub(crate) fn id3v23(frames: &[Vec<u8>]) -> Vec<u8> {
    let mut body = Vec::new();
    for f in frames {
        body.extend_from_slice(f);
    }
    let mut v = Vec::new();
    v.extend_from_slice(b"ID3");
    v.extend_from_slice(&[3, 0, 0]); // v2.3, no flags
    v.extend_from_slice(&synchsafe(body.len() as u32));
    v.extend_from_slice(&body);
    v
}

/// A [`TagSink`] that just records every call, for assertions.
#[derive(Default)]
pub(crate) struct Collect {
    pub(crate) fields: Vec<(Option<Field>, String, String)>,
    pub(crate) pics: Vec<Picture>,
}
impl TagSink for Collect {
    fn text(&mut self, f: Option<Field>, k: &str, val: &str) {
        self.fields.push((f, k.to_owned(), val.to_owned()));
    }
    fn picture(&mut self, p: &Picture) {
        self.pics.push(*p);
    }
}
