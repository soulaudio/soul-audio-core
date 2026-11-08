//! End-to-end desktop pipeline: scan a real folder of tagged files → bake
//! `.soulidx` → read it back through the no-alloc reader, exactly as the DAP
//! will. Proves scan + tags + intern + bake + read are 1:1.

use std::path::{Path, PathBuf};

use soul_audio_catalog::{BrowseAxis, Catalog};
use soul_audio_core::SliceSource;
use soul_audio_index::IndexReader;

fn testdata(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(name)
}

/// A unique temp dir with two album folders holding committed tagged fixtures.
fn fixture_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!("soul_import_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let a = root.join("Album A");
    let b = root.join("Album B");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    // "Album A/..." sorts before "Album B/...", so the walk yields A then B.
    std::fs::copy(testdata("tagged.flac"), a.join("01 - song.flac")).unwrap();
    std::fs::copy(testdata("arted.flac"), b.join("01 - song.flac")).unwrap();
    root
}

fn resolve<'a>(
    rdr: &impl Catalog,
    s: soul_audio_catalog::StrRef,
    buf: &'a mut [u8; 128],
) -> &'a str {
    let n = rdr.resolve_str(s, buf).expect("resolve_str");
    core::str::from_utf8(&buf[..n]).expect("utf8")
}

fn artist<'a>(rdr: &impl Catalog, id: u32, buf: &'a mut [u8; 128]) -> &'a str {
    let n = rdr.artist_name(id, buf).expect("artist_name");
    core::str::from_utf8(&buf[..n]).expect("utf8")
}

#[test]
fn scan_tags_intern_bake_read_is_1to1() {
    let root = fixture_root();
    let bytes = soul_audio_import::bake(&root).expect("bake");
    let rdr = IndexReader::new(SliceSource::new(&bytes));
    let mut buf = [0u8; 128];

    assert_eq!(rdr.track_count(), 2, "both files scanned");

    // Sorted walk: Album A (tagged.flac) = track 0, Album B (arted.flac) = track 1.
    let t0 = rdr.track(0).unwrap();
    assert_eq!(resolve(&rdr, t0.title, &mut buf), "Soul Test Track");
    assert_eq!(artist(&rdr, t0.artist, &mut buf), "Test Artist");
    let a0 = rdr.album(t0.album).unwrap();
    assert_eq!(resolve(&rdr, a0.title, &mut buf), "Test Album");

    let t1 = rdr.track(1).unwrap();
    assert_eq!(resolve(&rdr, t1.title, &mut buf), "Arted Track");
    // No artist/album tags → importer defaults.
    assert_eq!(artist(&rdr, t1.artist, &mut buf), "Unknown Artist");
    let a1 = rdr.album(t1.album).unwrap();
    assert_eq!(resolve(&rdr, a1.title, &mut buf), "Unknown Album");

    // Folder isolation: the two tracks live in different folders → different albums.
    assert_ne!(t0.album, t1.album, "distinct folders → distinct albums");
    assert_eq!(rdr.album_count(), 2);
    assert_ne!(t0.folder, t1.folder);

    // Desktop-baked Title browse order: "Arted Track" < "Soul Test Track".
    assert_eq!(rdr.order_len(BrowseAxis::Title), 2);
    assert_eq!(rdr.order(BrowseAxis::Title, 0).unwrap(), 1, "Arted first");
    assert_eq!(rdr.order(BrowseAxis::Title, 1).unwrap(), 0);

    let _ = std::fs::remove_dir_all(&root);
}
