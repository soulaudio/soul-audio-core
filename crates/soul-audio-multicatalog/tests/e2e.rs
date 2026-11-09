//! End-to-end across sources: scan **real** on-disk libraries through the
//! importer, bake each to a `.soulidx`, aggregate the readers with
//! `MultiCatalog`, and prove the whole chain on actually-scanned data —
//!
//! - aggregated counts and the cross-source **collation** browse order,
//! - cross-source **same-album grouping** (the same tagged file dropped into two
//!   sources collates to one album title, and each source's copy stays grouped —
//!   the real-data analogue of the unit-test grouping repros), and
//! - the playback seam: every track **locates to a real file on its OWN source**
//!   (the compound id's source picks the physical root — the per-source
//!   `path_of` the desktop wiring needs) and **decodes**, with the decoder's own
//!   `sample_rate`/`channels` equal to the baked record, so the file we located
//!   across sources is provably the file the catalog described.
//!
//! Two formats (FLAC + WAV) run through the real decode dispatch, so this covers
//! the locate→decode seam, not a single codec. Host-side, no board.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use soul_audio_catalog::{BrowseAxis, Catalog, StrRef};
use soul_audio_core::{Decoder, SliceSource, open};
use soul_audio_index::IndexReader;
use soul_audio_multicatalog::{MultiCatalog, unpack};

fn testdata(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(name)
}

/// Build a unique temp source library, copying each `(relative_dir, fixture,
/// dest_name)` fixture into place. Returns the source root.
fn make_source(tag: &str, files: &[(&str, &str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!("soul_mc_e2e_{}_{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for (reldir, fixture, dest) in files {
        let dir = root.join(reldir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(testdata(fixture), dir.join(dest))
            .unwrap_or_else(|e| panic!("copy fixture {fixture}: {e}"));
    }
    root
}

/// Scan + bake `root` through the real importer, leak the bytes for a `'static`
/// reader, and box it as a `Catalog` member.
fn baked(root: &Path) -> Box<dyn Catalog + Send> {
    let bytes: &'static [u8] = Vec::leak(soul_audio_import::bake(root).expect("bake"));
    Box::new(IndexReader::new(SliceSource::new(bytes)))
}

fn resolve(cat: &dyn Catalog, s: StrRef) -> String {
    let mut buf = [0u8; 256];
    let n = cat.resolve_str(s, &mut buf).expect("resolve_str");
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

fn title_of(cat: &dyn Catalog, id: u32) -> String {
    resolve(cat, cat.track(id).expect("track").title)
}

fn album_title_of(cat: &dyn Catalog, id: u32) -> String {
    let t = cat.track(id).expect("track");
    resolve(cat, cat.album(t.album).expect("album").title)
}

/// Reconstruct a track's physical path the way the desktop must for an aggregate:
/// the compound id's **source** selects the physical root, then the member's
/// `folder.path` + `file_name` (both resolved through the routing `MultiCatalog`).
fn physical_path(cat: &dyn Catalog, roots: &[PathBuf], id: u32) -> PathBuf {
    let (src, _) = unpack(id);
    let t = cat.track(id).expect("track");
    let folder = cat.folder(t.folder).expect("folder");
    let rel = resolve(cat, folder.path);
    let name = resolve(cat, t.file_name);
    let mut p = roots[src as usize].clone();
    if !rel.is_empty() {
        for seg in rel.split('/') {
            p.push(seg);
        }
    }
    p.push(&name);
    p
}

/// Drain a decoder to EOF; returns total decoded samples (channels interleaved).
fn drain(dec: &mut impl Decoder) -> u64 {
    let mut buf = [0i32; 1024];
    let mut total = 0u64;
    for _ in 0..5_000_000u32 {
        let got = dec.decode_into(&mut buf).expect("decode_into");
        if got == 0 {
            return total;
        }
        total += got as u64;
    }
    panic!("decode did not reach EOF");
}

#[test]
fn scan_bake_aggregate_locate_decode_across_sources() {
    // Source A ("local disk") and Source B ("server") are independently scanned
    // libraries. `tagged.flac` is in BOTH (identical tags -> album "Test Album"
    // on each), giving a real cross-source same-album case; the other files are a
    // WAV (A) and a different FLAC (B) for multi-format, multi-title coverage.
    let root_a = make_source(
        "a",
        &[
            ("Jazz/Album", "tagged.flac", "01 - track.flac"),
            ("Signals", "chirp.wav", "chirp.wav"),
        ],
    );
    let root_b = make_source(
        "b",
        &[
            ("Jazz/Album", "tagged.flac", "01 - track.flac"),
            ("Misc", "arted.flac", "arted.flac"),
        ],
    );
    let roots = vec![root_a.clone(), root_b.clone()];

    let multi = MultiCatalog::new(vec![baked(&root_a), baked(&root_b)]);

    // --- aggregated counts: 2 tracks per source, folder-isolated albums ---
    assert_eq!(
        multi.track_count(),
        4,
        "all four files scanned across sources"
    );
    assert_eq!(
        multi.album_count(),
        4,
        "folder isolation: 2 'Test Album' copies + 2 untagged, all distinct"
    );

    // --- the playback seam, across sources: locate on the OWNING source + decode ---
    let n = multi.order_len(BrowseAxis::Title);
    assert_eq!(n, 4);
    let mut decoded_formats = BTreeSet::new();
    for i in 0..n {
        let id = multi.order(BrowseAxis::Title, i).unwrap();
        let rec = multi.track(id).unwrap();
        let path = physical_path(&multi, &roots, id);
        assert!(
            path.is_file(),
            "located path must exist on its source: {path:?}"
        );

        let bytes = std::fs::read(&path).expect("read located file");
        let mut dec =
            open(SliceSource::new(&bytes)).unwrap_or_else(|e| panic!("open {path:?}: {e:?}"));
        let info = dec.stream_info();
        assert_ne!(
            rec.sample_rate, 0,
            "scan captured stream facts for {path:?}"
        );
        assert_eq!(
            info.sample_rate, rec.sample_rate,
            "sample rate decoder vs catalog for {path:?}"
        );
        assert_eq!(
            info.channels,
            u16::from(rec.channels),
            "channels decoder vs catalog for {path:?}"
        );
        assert!(drain(&mut dec) > 0, "decoded non-empty PCM for {path:?}");

        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            decoded_formats.insert(ext.to_owned());
        }
    }
    assert!(
        decoded_formats.contains("flac") && decoded_formats.contains("wav"),
        "both FLAC and WAV were located and decoded, got {decoded_formats:?}"
    );

    // --- cross-source collation + same-title tiebreak on real scanned data ---
    // "Arted Track" collates first; the two identical "Soul Test Track" titles
    // tie and break by source (A before B), landing adjacent at the end.
    let title_order: Vec<(String, u32)> = (0..n)
        .map(|i| {
            let id = multi.order(BrowseAxis::Title, i).unwrap();
            (title_of(&multi, id), unpack(id).0)
        })
        .collect();
    assert_eq!(
        title_order[0],
        ("Arted Track".to_owned(), 1),
        "arted collates first, from source B"
    );
    let soul: Vec<(usize, u32)> = title_order
        .iter()
        .enumerate()
        .filter(|(_, (t, _))| t == "Soul Test Track")
        .map(|(i, (_, s))| (i, *s))
        .collect();
    assert_eq!(soul.len(), 2, "both sources' copies present");
    assert_eq!(soul[0].0 + 1, soul[1].0, "the two copies are adjacent");
    assert_eq!(
        (soul[0].1, soul[1].1),
        (0, 1),
        "same-title tiebreak orders source A before source B"
    );

    // --- cross-source same-album grouping (the bug-fix invariant, on real data) ---
    // On the Album axis "Test Album" (< "Unknown Album") leads, and its two
    // cross-source tracks stay grouped: source A's copy, then source B's — not
    // interleaved with the other album.
    let album_order: Vec<(String, u32)> = (0..multi.order_len(BrowseAxis::Album))
        .map(|i| {
            let id = multi.order(BrowseAxis::Album, i).unwrap();
            (album_title_of(&multi, id), unpack(id).0)
        })
        .collect();
    assert_eq!(
        &album_order[0..2],
        &[("Test Album".to_owned(), 0), ("Test Album".to_owned(), 1),],
        "both 'Test Album' tracks lead and stay grouped A-then-B"
    );

    let _ = std::fs::remove_dir_all(&root_a);
    let _ = std::fs::remove_dir_all(&root_b);
}
