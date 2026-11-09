#![cfg(feature = "build")]
use soul_audio_artbake::{ArtBlobReader, ENTRY_LEN, HEADER_LEN};
use soul_audio_catalog::ArtStore;
use soul_audio_core::SliceSource;

fn blob() -> Vec<u8> {
    let bitmaps: [[u8; 2]; 2] = [[0b1010_1010, 0b0101_0101], [0xFF, 0x00]];
    let mut v = Vec::new();
    v.extend_from_slice(b"SART"); // magic
    v.push(1);
    v.push(0); // version, flags
    v.extend_from_slice(&8u16.to_le_bytes()); // width
    v.extend_from_slice(&2u16.to_le_bytes()); // height
    v.extend_from_slice(&2u32.to_le_bytes()); // count
    v.extend_from_slice(&0u16.to_le_bytes()); // pad
    let first = (HEADER_LEN + 2 * ENTRY_LEN) as u32;
    for (i, _) in bitmaps.iter().enumerate() {
        v.extend_from_slice(&(first + i as u32 * 2).to_le_bytes()); // offset
        v.extend_from_slice(&2u32.to_le_bytes()); // len
    }
    for b in bitmaps {
        v.extend_from_slice(&b);
    }
    v
}

#[test]
fn reads_dimensions_and_bitmaps_by_art_id() {
    let bytes = blob();
    let r = ArtBlobReader::new(SliceSource::new(&bytes));
    assert_eq!(r.dimensions(), (8, 2));
    assert_eq!(r.count(), 2);
    let mut buf = [0u8; 8];
    assert_eq!(r.art(0, &mut buf).unwrap(), 0, "art_id 0 = no art");
    assert_eq!(r.art(1, &mut buf).unwrap(), 2);
    assert_eq!(&buf[..2], &[0b1010_1010, 0b0101_0101]);
    assert_eq!(r.art(2, &mut buf).unwrap(), 2);
    assert_eq!(&buf[..2], &[0xFF, 0x00]);
    assert_eq!(
        r.art(3, &mut buf),
        Err(soul_audio_catalog::CatalogError::OutOfRange)
    );
}

use image::{GrayImage, ImageEncoder, Luma};
use soul_audio_artbake::build::{ArtBlobWriter, encode_cover};

fn png_of(w: u32, h: u32, f: impl Fn(u32, u32) -> u8) -> Vec<u8> {
    let img = GrayImage::from_fn(w, h, |x, y| Luma([f(x, y)]));
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(img.as_raw(), w, h, image::ExtendedColorType::L8)
        .unwrap();
    out
}

#[test]
fn solid_black_is_all_ink_white_is_all_blank() {
    let black = encode_cover(&png_of(16, 16, |_, _| 0), 16, 16).unwrap();
    assert!(black.iter().all(|&b| b == 0xFF), "black → all ink bits");
    let white = encode_cover(&png_of(16, 16, |_, _| 255), 16, 16).unwrap();
    assert!(white.iter().all(|&b| b == 0x00), "white → all blank");
    assert_eq!(black.len(), 2 * 16); // row_bytes(16)=2 * height 16
}

#[test]
fn encode_is_deterministic() {
    let src = png_of(32, 24, |x, y| ((x * 7 + y * 3) % 256) as u8);
    assert_eq!(
        encode_cover(&src, 20, 20).unwrap(),
        encode_cover(&src, 20, 20).unwrap()
    );
}

#[test]
fn unsupported_bytes_error() {
    assert!(encode_cover(b"not an image", 8, 8).is_err());
}

#[test]
fn writer_dedups_and_round_trips_through_reader() {
    let a = encode_cover(&png_of(16, 16, |x, _| if x < 8 { 0 } else { 255 }), 16, 16).unwrap();
    let b = encode_cover(&png_of(16, 16, |_, _| 0), 16, 16).unwrap();
    let mut w = ArtBlobWriter::new(16, 16);
    let id_a = w.add(a.clone());
    let id_b = w.add(b.clone());
    let id_a2 = w.add(a.clone()); // identical → same id
    assert_eq!(id_a, 1);
    assert_eq!(id_b, 2);
    assert_eq!(id_a2, id_a);
    let bytes = w.finish();
    let r = ArtBlobReader::new(SliceSource::new(&bytes));
    assert_eq!(r.count(), 2);
    let mut buf = vec![0u8; a.len()];
    assert_eq!(r.art(id_a, &mut buf).unwrap(), a.len());
    assert_eq!(buf, a);
    r.art(id_b, &mut buf).unwrap();
    assert_eq!(buf, b);
}
