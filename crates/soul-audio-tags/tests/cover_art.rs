//! Embedded cover-art (image metadata) extraction across the three
//! Picture-emitting readers: ID3 `APIC` (MP3), MP4 `covr`, and FLAC `PICTURE`.
//! Each fixture carries the same tiny 16x16 PNG. We assert the descriptor
//! (`Picture`) the reader emits and that `read_picture_bytes` returns the exact
//! image payload (PNG magic + declared length).
//!
//! `std`-only (file I/O via `embedded-io-adapters`), same gating as the other
//! integration suites here.
#![cfg(feature = "std")]

use std::fs::File;
use std::path::{Path, PathBuf};

use embedded_io_adapters::std::FromStd;
use soul_audio_tags::{Field, MimeType, Picture, PictureKind, TagSink, read, read_picture_bytes};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(name)
}

/// Collects the pictures a reader emits (the descriptor is `Copy`, no borrow).
#[derive(Default)]
struct PicSink {
    pics: Vec<Picture>,
}

impl TagSink for PicSink {
    fn text(&mut self, _field: Option<Field>, _raw_key: &str, _value: &str) {}
    fn picture(&mut self, pic: &Picture) {
        self.pics.push(*pic);
    }
}

/// Read the first embedded picture of `file` plus its decoded image bytes.
fn first_picture(file: &str) -> (Picture, Vec<u8>) {
    let mut src = FromStd::new(File::open(fixture(file)).expect("open fixture"));
    let mut scratch = [0u8; 64 * 1024];
    let mut sink = PicSink::default();
    read(&mut src, &mut scratch, &mut sink).expect("read tags");
    assert!(!sink.pics.is_empty(), "{file}: no embedded picture found");
    let pic = sink.pics[0];
    let bytes = read_picture_bytes(&mut src, &pic).expect("read picture bytes");
    (pic, bytes)
}

/// The extracted bytes must be the exact PNG payload the descriptor points at.
fn assert_png_payload(pic: &Picture, bytes: &[u8]) {
    assert_eq!(pic.mime, MimeType::Png, "mime");
    assert!(bytes.len() >= 8, "picture too short: {}", bytes.len());
    assert_eq!(&bytes[..4], b"\x89PNG", "not PNG data");
    // Raw-encoded descriptors (ID3/MP4/FLAC) carry an exact byte length.
    assert_eq!(bytes.len() as u32, pic.data_len, "data_len mismatch");
}

#[test]
fn id3_apic_cover_extracts() {
    let (pic, bytes) = first_picture("art.mp3");
    assert_png_payload(&pic, &bytes);
}

#[test]
fn mp4_covr_cover_extracts() {
    let (pic, bytes) = first_picture("art.m4a");
    assert_png_payload(&pic, &bytes);
    // The MP4 `covr` reader classifies every cover as the front cover.
    assert_eq!(pic.usage, PictureKind::FrontCover);
}

#[test]
fn flac_picture_cover_extracts() {
    let (pic, bytes) = first_picture("arted.flac");
    assert_png_payload(&pic, &bytes);
}
