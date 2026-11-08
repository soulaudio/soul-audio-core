//! The on-DISK export contract (the closest host-side proxy for the DAP's SD
//! read): scan a temp "card" of tagged files in **nested folders**, `export` the
//! `.soulidx` onto that same card, then open `card/.soulidx/index.sidx` through a
//! real `std::fs::File` wrapped as a `SeekSource` — exactly as the device reads
//! it — and prove the whole desktop→card→DAP loop:
//!
//! - track fields + file name survive,
//! - **`card_root / folder.path / file_name` reconstructs the real file path**
//!   (the playback locator — the point of this phase),
//! - browse order + album/artist resolution hold.
//!
//! `art.blob` / `extended.blob` are deferred, so only `index.sidx` is asserted.

use std::path::{Path, PathBuf};

use soul_audio_catalog::{BrowseAxis, Catalog, StrRef};
use soul_audio_index::IndexReader;

fn testdata(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(name)
}

/// A unique temp "card": two tagged fixtures in **nested** album folders.
/// Sorted walk ("Classical" < "Jazz") → Aria = track 0, Monk = track 1.
fn card_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!("soul_export_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let bach = root.join("Classical").join("Bach");
    let jazz = root.join("Jazz");
    std::fs::create_dir_all(&bach).unwrap();
    std::fs::create_dir_all(&jazz).unwrap();
    std::fs::copy(testdata("tagged.flac"), bach.join("01 - Aria.flac")).unwrap();
    std::fs::copy(testdata("arted.flac"), jazz.join("01 - Monk.flac")).unwrap();
    root
}

/// Open the exported index over a real on-disk `File` as a `SeekSource` — the
/// transport the DAP uses (SD), proven here on a host file.
fn open_on_disk(card: &Path) -> IndexReader<embedded_io_adapters::std::FromStd<std::fs::File>> {
    let path = card.join(".soulidx").join("index.sidx");
    let file = std::fs::File::open(&path).expect("open exported index.sidx");
    IndexReader::new(embedded_io_adapters::std::FromStd::new(file))
}

fn owned(rdr: &impl Catalog, s: StrRef) -> String {
    let mut buf = [0u8; 256];
    let n = rdr.resolve_str(s, &mut buf).expect("resolve_str");
    core::str::from_utf8(&buf[..n]).expect("utf8").to_owned()
}

fn artist(rdr: &impl Catalog, id: u32) -> String {
    let mut buf = [0u8; 256];
    let n = rdr.artist_name(id, &mut buf).expect("artist_name");
    core::str::from_utf8(&buf[..n]).expect("utf8").to_owned()
}

/// Reconstruct a track's physical path the way the DAP does:
/// `card_root / folder(track.folder).path / track.file_name`.
fn physical_path(rdr: &impl Catalog, card: &Path, track_id: u32) -> PathBuf {
    let t = rdr.track(track_id).expect("track");
    let folder = rdr.folder(t.folder).expect("folder");
    let rel = owned(rdr, folder.path);
    let name = owned(rdr, t.file_name);
    let mut p = card.to_path_buf();
    if !rel.is_empty() {
        p.push(&rel);
    }
    p.push(&name);
    p
}

#[test]
fn export_writes_disk_and_tracks_are_locatable() {
    let card = card_root();

    // The export step: desktop bakes the index onto the card.
    soul_audio_import::export(&card, &card).expect("export");
    let index_path = card.join(".soulidx").join("index.sidx");
    assert!(
        index_path.is_file(),
        "export wrote card/.soulidx/index.sidx"
    );

    // Read it back over a real File SeekSource (the on-disk path).
    let rdr = open_on_disk(&card);

    assert_eq!(rdr.track_count(), 2, "both nested files scanned");

    // Track 0 = Classical/Bach/01 - Aria.flac (tagged fixture).
    let t0 = rdr.track(0).unwrap();
    assert_eq!(owned(&rdr, t0.title), "Soul Test Track");
    assert_eq!(owned(&rdr, t0.file_name), "01 - Aria.flac");
    assert_eq!(artist(&rdr, t0.artist), "Test Artist");
    assert_eq!(
        owned(&rdr, rdr.album(t0.album).unwrap().title),
        "Test Album"
    );

    // Track 1 = Jazz/01 - Monk.flac (arted fixture: no artist/album tags).
    let t1 = rdr.track(1).unwrap();
    assert_eq!(owned(&rdr, t1.title), "Arted Track");
    assert_eq!(owned(&rdr, t1.file_name), "01 - Monk.flac");
    assert_eq!(artist(&rdr, t1.artist), "Unknown Artist");

    // THE POINT: folder path + file name reconstruct the real file, for BOTH
    // tracks, across nested folders. Canonicalize so the comparison is robust to
    // separator/`..` differences between host and the baked `/`-form.
    for (id, real) in [
        (
            0u32,
            card.join("Classical").join("Bach").join("01 - Aria.flac"),
        ),
        (1u32, card.join("Jazz").join("01 - Monk.flac")),
    ] {
        let reconstructed = physical_path(&rdr, &card, id);
        assert!(
            reconstructed.is_file(),
            "reconstructed path must exist: {reconstructed:?}"
        );
        assert_eq!(
            std::fs::canonicalize(&reconstructed).unwrap(),
            std::fs::canonicalize(&real).unwrap(),
            "root / folder.path / file_name must equal the real file path"
        );
    }

    // Folder table: nested paths are relative to the card root, `/`-separated,
    // and form a tree (each track's folder has a valid parent chain to root).
    assert_eq!(
        owned(&rdr, rdr.folder(t0.folder).unwrap().path),
        "Classical/Bach"
    );
    assert_eq!(owned(&rdr, rdr.folder(t1.folder).unwrap().path), "Jazz");
    // Walk t0's folder up to the root (empty path); the chain must terminate.
    let mut cur = t0.folder;
    let mut hops = 0;
    loop {
        let f = rdr.folder(cur).unwrap();
        if f.parent == soul_audio_catalog::NO_FOLDER {
            assert_eq!(
                owned(&rdr, f.path),
                "",
                "chain terminates at the root folder"
            );
            break;
        }
        cur = f.parent;
        hops += 1;
        assert!(hops < 64, "folder parent chain must terminate");
    }

    // Folder isolation + distinct albums, and the desktop-baked Title order.
    assert_ne!(t0.folder, t1.folder);
    assert_ne!(t0.album, t1.album);
    assert_eq!(rdr.album_count(), 2);
    assert_eq!(
        rdr.order(BrowseAxis::Title, 0).unwrap(),
        1,
        "Arted sorts first"
    );
    assert_eq!(rdr.order(BrowseAxis::Title, 1).unwrap(), 0);

    let _ = std::fs::remove_dir_all(&card);
}
