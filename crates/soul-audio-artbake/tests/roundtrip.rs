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
