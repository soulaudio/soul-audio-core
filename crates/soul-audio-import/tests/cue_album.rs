//! A single-file cue album, end to end: scan a folder holding one audio file
//! plus its `.cue` → bake `.soulidx` → read it back through the no-alloc
//! reader, and get the album's **tracks**, each with its own metadata and its
//! slice of the shared file.
//!
//! This is the half of cue support the library sees. The playback half (the
//! slice actually playing, sample-accurately and gaplessly) is
//! `soul-audio-playlist/tests/cue_playback.rs`; what this proves is that the
//! numbers survive the scan, the intern, the bake and the read — and that the
//! whole-file entry is *replaced*, so the album does not also appear as one
//! 50-minute item.

use std::path::{Path, PathBuf};

use soul_audio_catalog::{BrowseAxis, Catalog};
use soul_audio_core::SliceSource;
use soul_audio_index::IndexReader;

fn testdata(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(name)
}

/// A temp library with one cue album (`tone.flac` + a sheet splitting it in
/// three) and one ordinary track, so the test can tell the two apart.
fn fixture_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("soul_cue_{}_{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let album = root.join("Cue Album");
    let plain = root.join("Plain Album");
    std::fs::create_dir_all(&album).unwrap();
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::copy(testdata("tone.flac"), album.join("album.flac")).unwrap();
    std::fs::copy(testdata("tagged.flac"), plain.join("01 - song.flac")).unwrap();
    // `tone.flac` is 2 s at 44.1 kHz: split at 0.4 s (30 CD frames) and 1.2 s
    // (90 frames), i.e. 00:00:30 and 00:01:15 in MM:SS:FF.
    let cue = "REM GENRE Test\r\n\
               PERFORMER \"Cue Performer\"\r\n\
               TITLE \"Cue Album Title\"\r\n\
               FILE \"album.flac\" WAVE\r\n\
               \x20 TRACK 01 AUDIO\r\n\
               \x20   TITLE \"First Track\"\r\n\
               \x20   INDEX 01 00:00:00\r\n\
               \x20 TRACK 02 AUDIO\r\n\
               \x20   TITLE \"Second Track\"\r\n\
               \x20   PERFORMER \"Guest Artist\"\r\n\
               \x20   INDEX 01 00:00:30\r\n\
               \x20 TRACK 03 AUDIO\r\n\
               \x20   TITLE \"Third Track\"\r\n\
               \x20   INDEX 01 00:01:15\r\n";
    std::fs::write(album.join("album.cue"), cue).unwrap();
    root
}

fn resolve(rdr: &impl Catalog, s: soul_audio_catalog::StrRef, buf: &mut [u8; 128]) -> String {
    let n = rdr.resolve_str(s, buf).expect("resolve_str");
    core::str::from_utf8(&buf[..n]).expect("utf8").to_owned()
}

/// Every track in the baked index, in title order, as
/// `(title, artist, file_name, track_no, cue_range, duration_ms)`.
type Row = (String, String, String, u16, Option<(u64, u64)>, u32);

fn rows(root: &Path) -> Vec<Row> {
    let bytes = soul_audio_import::bake(root).expect("bake");
    let rdr = IndexReader::new(SliceSource::new(&bytes));
    assert!(rdr.is_current(), "the baker and reader must agree on v9");
    let mut out = Vec::new();
    for i in 0..rdr.order_len(BrowseAxis::Title) {
        let id = rdr.order(BrowseAxis::Title, i).expect("order slot");
        let t = rdr.track(id).expect("track");
        let mut buf = [0u8; 128];
        let title = resolve(&rdr, t.title, &mut buf);
        let n = rdr.artist_name(t.artist, &mut buf).expect("artist_name");
        let artist = core::str::from_utf8(&buf[..n]).expect("utf8").to_owned();
        let file_name = resolve(&rdr, t.file_name, &mut buf);
        out.push((
            title,
            artist,
            file_name,
            t.track_no,
            t.cue_range(),
            t.duration_ms,
        ));
    }
    out
}

#[test]
fn a_cue_album_imports_as_its_tracks_each_with_its_own_slice() {
    let root = fixture_root("basic");
    let rows = rows(&root);

    // Three cue tracks + the one ordinary track. Crucially *not* five: the
    // whole-file entry is replaced, not added to.
    assert_eq!(
        rows.len(),
        4,
        "expected 3 cue tracks + 1 plain track, got {rows:?}"
    );

    let cue_rows: Vec<&Row> = rows.iter().filter(|r| r.4.is_some()).collect();
    assert_eq!(cue_rows.len(), 3, "{rows:?}");
    // They all point at the one real file — that is what makes it a single-file
    // album — and each carries a different slice of it.
    for r in &cue_rows {
        assert_eq!(r.2, "album.flac", "every cue track plays the shared file");
    }

    let by_no = |n: u16| -> &Row {
        cue_rows
            .iter()
            .copied()
            .find(|r| r.3 == n)
            .unwrap_or_else(|| panic!("no track {n} in {rows:?}"))
    };
    // Titles and the per-track performer come from the sheet; the album
    // performer fills in where a track does not name one.
    assert_eq!(by_no(1).0, "First Track");
    assert_eq!(by_no(1).1, "Cue Performer");
    assert_eq!(by_no(2).0, "Second Track");
    assert_eq!(by_no(2).1, "Guest Artist", "the track's own PERFORMER wins");
    assert_eq!(by_no(3).0, "Third Track");

    // The slices: 30 and 90 CD frames at 44.1 kHz are 588 samples each, and the
    // last track runs to the file's end. They must tile the file exactly.
    let f = |cd: u64| cd * 588;
    assert_eq!(by_no(1).4, Some((0, f(30))));
    assert_eq!(by_no(2).4, Some((f(30), f(90) - f(30))));
    let (start3, len3) = by_no(3).4.expect("track 3 slice");
    assert_eq!(start3, f(90));
    // `tone.flac` is the committed 2-second fixture (testdata/README.md), so
    // the last track runs from 90 CD frames to 88 200 — and the three slices
    // must add up to exactly the file, with no gap and no overlap.
    const FILE_FRAMES: u64 = 2 * 44_100;
    assert_eq!(
        len3,
        FILE_FRAMES - f(90),
        "the last track must run to the end of the file"
    );
    let covered: u64 = cue_rows.iter().map(|r| r.4.unwrap().1).sum();
    assert_eq!(
        covered, FILE_FRAMES,
        "the slices must tile the whole file: {cue_rows:?}"
    );
    for (a, b) in [(by_no(1), by_no(2)), (by_no(2), by_no(3))] {
        let (sa, la) = a.4.unwrap();
        let (sb, _) = b.4.unwrap();
        assert_eq!(sa + la, sb, "each track must abut the next");
    }

    // Durations are per track, not the album file's 2 seconds.
    assert_eq!(by_no(1).5, 400, "30 CD frames = 0.4 s");
    assert_eq!(by_no(2).5, 800, "60 CD frames = 0.8 s");

    // The ordinary track is untouched and is not a cue track.
    let plain: Vec<&Row> = rows.iter().filter(|r| r.4.is_none()).collect();
    assert_eq!(plain.len(), 1);
    assert_eq!(plain[0].2, "01 - song.flac");

    let _ = std::fs::remove_dir_all(&root);
}

/// A cue that does not line up with reality must leave the file importable as
/// itself: a sheet naming a file that is not there, and a sheet with a
/// malformed timestamp. Either way the user keeps their album — as one entry,
/// which is worse than tracks but far better than nothing.
#[test]
fn a_broken_or_mismatched_cue_leaves_the_plain_file() {
    for (tag, cue) in [
        (
            "missing_file",
            "FILE \"not-here.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n",
        ),
        (
            "bad_timestamp",
            "FILE \"album.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 99:99:99\n",
        ),
        ("no_tracks", "FILE \"album.flac\" WAVE\nREM nothing\n"),
        ("not_a_cue_at_all", "\u{0}\u{1}\u{2}garbage"),
    ] {
        let root = fixture_root(tag);
        std::fs::write(root.join("Cue Album").join("album.cue"), cue).unwrap();
        let rows = rows(&root);
        assert_eq!(rows.len(), 2, "{tag}: expected the 2 plain files, {rows:?}");
        assert!(
            rows.iter().all(|r| r.4.is_none()),
            "{tag}: nothing may be marked as a cue track: {rows:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// A cue sheet is **untrusted input**: it is a text file that names a path, and
/// it may name one outside its own folder — by accident (a rip moved from
/// another machine) or deliberately. Expansion must never reach outside the
/// cue's directory, so a sheet cannot make the importer read, index, or later
/// play a file the user did not put in their library.
///
/// The implementation takes only the *leaf* of a `FILE` name, which makes this
/// true by construction; this test is what keeps it true.
#[test]
fn a_cue_file_name_cannot_escape_its_folder() {
    for name in [
        "../Plain Album/01 - song.flac",
        "../../../../etc/passwd",
        "..\\..\\Plain Album\\01 - song.flac",
        "/etc/passwd",
        "C:\\Windows\\System32\\drivers\\etc\\hosts",
    ] {
        let root = fixture_root("escape");
        let cue = format!(
            "FILE \"{name}\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n\
             TRACK 02 AUDIO\nINDEX 01 00:00:30\n"
        );
        std::fs::write(root.join("Cue Album").join("album.cue"), cue).unwrap();
        let rows = rows(&root);
        // The two files the library actually contains, neither of them split:
        // the sheet pointed at something that is not beside it.
        assert_eq!(rows.len(), 2, "{name}: {rows:?}");
        assert!(
            rows.iter().all(|r| r.4.is_none()),
            "{name}: a sheet that escapes its folder must expand nothing: {rows:?}"
        );
        // And in particular the *other* album's track is not pulled into this
        // one as a cue track.
        assert!(
            rows.iter()
                .any(|r| r.2 == "01 - song.flac" && r.4.is_none()),
            "{name}: the sibling album must be untouched: {rows:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// A cue sheet in a legacy code page (Windows-1252, as EAC wrote for years)
/// still splits the album: the timestamps are ASCII, so only the titles are
/// affected by the decode.
#[test]
fn a_non_utf8_cue_still_splits_the_album() {
    let root = fixture_root("latin1");
    // "Caf\xE9" — 0xE9 is `é` in Windows-1252 and invalid UTF-8.
    let mut cue = Vec::new();
    cue.extend_from_slice(b"TITLE \"Caf\xE9 Album\"\nFILE \"album.flac\" WAVE\n");
    cue.extend_from_slice(b"TRACK 01 AUDIO\nTITLE \"Caf\xE9\"\nINDEX 01 00:00:00\n");
    cue.extend_from_slice(b"TRACK 02 AUDIO\nTITLE \"Second\"\nINDEX 01 00:00:30\n");
    std::fs::write(root.join("Cue Album").join("album.cue"), cue).unwrap();

    let rows = rows(&root);
    let cue_rows: Vec<&Row> = rows.iter().filter(|r| r.4.is_some()).collect();
    assert_eq!(cue_rows.len(), 2, "the splits must survive the encoding");
    assert_eq!(cue_rows.iter().filter(|r| r.3 == 1).count(), 1);
    // The accented title decoded as Latin-1, i.e. the right character.
    assert!(
        rows.iter().any(|r| r.0 == "Café"),
        "a Windows-1252 title must decode to its character: {rows:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The sheet's `FILE` name may differ in case from the file on disk (a rip
/// made on Windows, read on Linux) — the album must still split.
#[test]
fn a_cue_file_name_matches_case_insensitively() {
    let root = fixture_root("case");
    let cue = "FILE \"ALBUM.FLAC\" WAVE\n\
               TRACK 01 AUDIO\nINDEX 01 00:00:00\n\
               TRACK 02 AUDIO\nINDEX 01 00:00:30\n";
    std::fs::write(root.join("Cue Album").join("album.cue"), cue).unwrap();
    let rows = rows(&root);
    assert_eq!(
        rows.iter().filter(|r| r.4.is_some()).count(),
        2,
        "a case-mismatched FILE name must still resolve: {rows:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
