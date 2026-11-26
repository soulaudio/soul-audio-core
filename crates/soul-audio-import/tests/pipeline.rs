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

#[test]
fn covers_sidecar_found_aligns_with_albums() {
    let root = std::env::temp_dir().join(format!("soul_import_cover_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let album = root.join("Covered Album");
    std::fs::create_dir_all(&album).unwrap();
    std::fs::copy(testdata("tagged.flac"), album.join("01 - song.flac")).unwrap();
    // Discovery matches by NAME, not validity — arbitrary bytes are fine.
    let cover_bytes = b"not actually a jpeg, discovery matches by name".to_vec();
    std::fs::write(album.join("cover.jpg"), &cover_bytes).unwrap();

    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(
        input.covers.len(),
        input.albums.len(),
        "covers aligned to albums"
    );
    assert_eq!(input.albums.len(), 1);
    assert_eq!(input.covers[0].as_deref(), Some(cover_bytes.as_slice()));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn covers_none_when_no_cover_present() {
    let root = std::env::temp_dir().join(format!("soul_import_nocover_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::copy(testdata("tagged.flac"), root.join("01 - song.flac")).unwrap();

    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(
        input.covers.len(),
        input.albums.len(),
        "covers aligned to albums"
    );
    assert_eq!(input.albums.len(), 1);
    assert_eq!(input.covers[0], None);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn covers_prefers_embedded_art_in_multitrack_album() {
    let root = std::env::temp_dir().join(format!("soul_import_embed_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let album = root.join("Arted Album");
    std::fs::create_dir_all(&album).unwrap();
    // A two-track album whose representative (first-walked) track carries an
    // embedded front cover. A sidecar is ALSO present, so this exercises both
    // multi-track aggregation (two tracks → one album → one cover) AND the
    // resolution order: the embedded picture must win over the sidecar.
    std::fs::copy(testdata("arted.flac"), album.join("01 - first.flac")).unwrap();
    std::fs::copy(testdata("arted.flac"), album.join("02 - second.flac")).unwrap();
    let sidecar = b"SIDECAR not the embedded art".to_vec();
    std::fs::write(album.join("cover.jpg"), &sidecar).unwrap();

    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(input.tracks.len(), 2, "two tracks scanned");
    assert_eq!(input.albums.len(), 1, "both tracks → one album");
    assert_eq!(input.covers.len(), 1, "one cover per album");
    let cover = input.covers[0].as_deref().expect("a cover resolved");
    assert!(!cover.is_empty(), "embedded art bytes extracted");
    assert_ne!(
        cover,
        sidecar.as_slice(),
        "embedded art wins over the sidecar"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Minimal FLAC bytes: STREAMINFO + a VORBIS_COMMENT (last block) carrying the
/// given `KEY=VALUE` comments — enough for the importer's tag read, no audio.
fn flac_with_comments(comments: &[&str]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"fLaC");
    // STREAMINFO (type 0), 34-byte body, not last.
    v.push(0x00);
    v.extend_from_slice(&[0, 0, 34]);
    let mut si = [0u8; 34];
    let (sr, ch, bps, total): (u32, u32, u32, u64) = (44_100, 2, 16, 88_200);
    si[10] = (sr >> 12) as u8;
    si[11] = (sr >> 4) as u8;
    si[12] = (((sr & 0xF) as u8) << 4) | ((((ch - 1) & 0x7) as u8) << 1) | (((bps - 1) >> 4) as u8);
    si[13] = ((((bps - 1) & 0xF) as u8) << 4) | (((total >> 32) & 0xF) as u8);
    si[14] = (total >> 24) as u8;
    si[15] = (total >> 16) as u8;
    si[16] = (total >> 8) as u8;
    si[17] = total as u8;
    v.extend_from_slice(&si);
    // VORBIS_COMMENT (type 4), LAST block (0x80 | 4 = 0x84).
    let mut vc = Vec::new();
    vc.extend_from_slice(&6u32.to_le_bytes());
    vc.extend_from_slice(b"vendor");
    vc.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in comments {
        vc.extend_from_slice(&(c.len() as u32).to_le_bytes());
        vc.extend_from_slice(c.as_bytes());
    }
    v.push(0x84);
    v.extend_from_slice(&(vc.len() as u32).to_be_bytes()[1..]);
    v.extend_from_slice(&vc);
    v
}

#[test]
fn scan_interns_genre_from_tags_and_bakes_it() {
    let root = std::env::temp_dir().join(format!("soul_import_genre_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let a = root.join("Album A");
    let b = root.join("Album B");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    // Track 0 carries a real GENRE tag; track 1 has none → the importer's
    // "Unknown Genre" fallback. "Album A" < "Album B" fixes the walk order.
    std::fs::write(
        a.join("01.flac"),
        flac_with_comments(&["TITLE=Genred", "GENRE=Jazz"]),
    )
    .unwrap();
    std::fs::write(b.join("01.flac"), flac_with_comments(&["TITLE=Plain"])).unwrap();

    // The baked genre string table + per-track genre id survive the read-back.
    let bytes = soul_audio_import::bake(&root).expect("bake");
    let rdr = IndexReader::new(SliceSource::new(&bytes));
    let mut buf = [0u8; 128];

    assert_eq!(rdr.track_count(), 2);
    assert!(
        rdr.genre_count() >= 1,
        "the tagged genre interned into the index"
    );

    let genre_of = |id: u32, buf: &mut [u8; 128]| -> String {
        let g = rdr.track(id).unwrap().genre;
        let n = rdr.genre_name(g, buf).unwrap();
        String::from_utf8_lossy(&buf[..n]).into_owned()
    };
    assert_eq!(genre_of(0, &mut buf), "Jazz", "real tag interned");
    assert_eq!(
        genre_of(1, &mut buf),
        "Unknown Genre",
        "untagged track falls back"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn scan_bakes_the_musicbrainz_recording_id_from_tags() {
    let root = std::env::temp_dir().join(format!("soul_import_mbid_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let mbid = "d6d5e8e6-1234-4abc-9def-0123456789ab";
    let tagged = format!("MUSICBRAINZ_TRACKID={mbid}");
    std::fs::write(
        root.join("01.flac"),
        flac_with_comments(&["TITLE=Tagged", tagged.as_str()]),
    )
    .unwrap();
    std::fs::write(root.join("02.flac"), flac_with_comments(&["TITLE=Plain"])).unwrap();

    let bytes = soul_audio_import::bake(&root).expect("bake");
    let rdr = IndexReader::new(SliceSource::new(&bytes));
    let mut buf = [0u8; 128];
    let t0 = rdr.track(0).unwrap();
    let n = rdr.resolve_str(t0.mbid, &mut buf).unwrap();
    assert_eq!(
        &buf[..n],
        mbid.as_bytes(),
        "the tagged recording MBID flows tags → importer → baked index (v4)"
    );
    assert_eq!(
        rdr.track(1).unwrap().mbid.len,
        0,
        "the untagged track bakes an empty MBID"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn scan_collects_sort_names_and_bakes_sort_order() {
    let root = std::env::temp_dir().join(format!("soul_import_sort_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    // Two artists whose sort order REVERSES their display order:
    //   "Brian Eno"    → ALBUMARTISTSORT "Eno, Brian"    (e…)
    //   "Eric Clapton" → ALBUMARTISTSORT "Clapton, Eric" (c…)
    let eno = root.join("Eno");
    let clapton = root.join("Clapton");
    std::fs::create_dir_all(&eno).unwrap();
    std::fs::create_dir_all(&clapton).unwrap();
    std::fs::write(
        eno.join("01.flac"),
        flac_with_comments(&[
            "ARTIST=Brian Eno",
            "ALBUMARTIST=Brian Eno",
            "ALBUMARTISTSORT=Eno, Brian",
            "ALBUM=Here Come the Warm Jets",
            "TITLE=eno_track",
        ]),
    )
    .unwrap();
    std::fs::write(
        clapton.join("01.flac"),
        flac_with_comments(&[
            "ARTIST=Eric Clapton",
            "ALBUMARTIST=Eric Clapton",
            "ALBUMARTISTSORT=Clapton, Eric",
            "ALBUM=Slowhand",
            "TITLE=clapton_track",
        ]),
    )
    .unwrap();

    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(
        input.artists.len(),
        input.artist_sorts.len(),
        "sort-names aligned to artists"
    );
    let sort_of = |name: &str| -> String {
        let i = input.artists.iter().position(|a| a == name).unwrap();
        input.artist_sorts[i].clone()
    };
    assert_eq!(sort_of("Brian Eno"), "Eno, Brian");
    assert_eq!(sort_of("Eric Clapton"), "Clapton, Eric");

    // The baked ArtistAlbumTrack order files Clapton (sort C) before Eno (sort
    // E) — the reverse of display order, proving the sort-name drives ordering.
    let bytes = soul_audio_import::bake(&root).expect("bake");
    let rdr = IndexReader::new(SliceSource::new(&bytes));
    let mut buf = [0u8; 128];
    let order: Vec<String> = (0..rdr.order_len(BrowseAxis::ArtistAlbumTrack))
        .map(|i| {
            let tid = rdr.order(BrowseAxis::ArtistAlbumTrack, i).unwrap();
            let title = rdr.track(tid).unwrap().title;
            let n = rdr.resolve_str(title, &mut buf).unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        })
        .collect();
    assert_eq!(order, vec!["clapton_track", "eno_track"]);

    let _ = std::fs::remove_dir_all(&root);
}

// ── Multi-disc albums (spec 2026-11-25) ─────────────────────────────────────

/// A fresh, uniquely-named temp library root.
fn fresh(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("soul_import_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

/// Write a minimal tagged FLAC at `root/rel`, creating parent folders.
fn put(root: &Path, rel: &str, comments: &[&str]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, flac_with_comments(comments)).unwrap();
}

#[test]
fn flat_multi_disc_album_is_one_album_with_disc_count() {
    let root = fresh("md_flat");
    put(
        &root,
        "Box/101.flac",
        &["ALBUM=Box", "TITLE=d1t1", "TRACKNUMBER=1", "DISCNUMBER=1/2"],
    );
    put(
        &root,
        "Box/102.flac",
        &["ALBUM=Box", "TITLE=d1t2", "TRACKNUMBER=2", "DISCNUMBER=1/2"],
    );
    put(
        &root,
        "Box/201.flac",
        &["ALBUM=Box", "TITLE=d2t1", "TRACKNUMBER=1", "DISCNUMBER=2/2"],
    );

    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(input.albums.len(), 1);
    assert_eq!(input.albums[0].disc_count, 2);
    let discs: Vec<u16> = input.tracks.iter().map(|t| t.disc_no).collect();
    assert_eq!(
        discs,
        vec![1, 1, 2],
        "DISCNUMBER=n/total splits (was silently disc 1)"
    );

    // Baked v8: disc count on the album record, album run in (disc, track) order.
    let bytes = soul_audio_index::build::write_index(&input);
    let rdr = IndexReader::new(SliceSource::new(&bytes));
    assert_eq!(rdr.album(0).unwrap().disc_count, 2);
    let mut buf = [0u8; 128];
    let run: Vec<String> = (0..rdr.album_track_count(0))
        .map(|i| {
            let t = rdr.track(rdr.album_track(0, i).unwrap()).unwrap();
            resolve(&rdr, t.title, &mut buf).to_owned()
        })
        .collect();
    assert_eq!(run, ["d1t1", "d1t2", "d2t1"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn disc_subfolders_fold_into_the_album_root() {
    let root = fresh("md_cd");
    put(
        &root,
        "Box/CD1/01.flac",
        &["ALBUM=Box", "TITLE=a", "TRACKNUMBER=1", "DISCNUMBER=1"],
    );
    put(
        &root,
        "Box/CD2/01.flac",
        &["ALBUM=Box", "TITLE=b", "TRACKNUMBER=1", "DISCNUMBER=2"],
    );

    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(input.albums.len(), 1, "CD1 + CD2 = one album");
    assert_eq!(input.albums[0].disc_count, 2);
    let path = |f: u32| input.folders[f as usize].path.clone();
    assert_eq!(
        path(input.albums[0].folder),
        "Box",
        "album folder = album root"
    );
    assert_eq!(
        path(input.tracks[0].folder),
        "Box/CD1",
        "track folder = its real location"
    );
    assert_eq!(path(input.tracks[1].folder), "Box/CD2");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn untagged_disc_folders_infer_disc_numbers() {
    let root = fresh("md_infer");
    put(
        &root,
        "Box/Disc 1/01.flac",
        &["ALBUM=Box", "TITLE=a", "TRACKNUMBER=1"],
    );
    put(
        &root,
        "Box/Disc 2/01.flac",
        &["ALBUM=Box", "TITLE=b", "TRACKNUMBER=1"],
    );

    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(input.albums.len(), 1);
    assert_eq!(input.albums[0].disc_count, 2);
    let discs: Vec<u16> = input.tracks.iter().map(|t| t.disc_no).collect();
    assert_eq!(
        discs,
        vec![1, 2],
        "disc number from the folder when untagged"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn disc_folder_numbered_zero_still_folds_but_falls_back_to_disc_one() {
    let root = fresh("md_zero");
    put(
        &root,
        "Box/CD0/01.flac",
        &["ALBUM=Box", "TITLE=a", "TRACKNUMBER=1"],
    );
    put(
        &root,
        "Box/CD2/01.flac",
        &["ALBUM=Box", "TITLE=b", "TRACKNUMBER=1"],
    );

    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(
        input.albums.len(),
        1,
        "CD0 is still a disc folder for the album root"
    );
    assert_eq!(input.folders[input.albums[0].folder as usize].path, "Box");
    let discs: Vec<u16> = input.tracks.iter().map(|t| t.disc_no).collect();
    assert_eq!(discs, vec![1, 2], "folder number 0 is absent, so disc 1");
    assert_eq!(input.albums[0].disc_count, 2);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn disc_folder_subtitle_is_a_fallback_for_the_tag() {
    let root = fresh("md_sub");
    put(
        &root,
        "Suite/Disc 1 - Mondstadt/01.flac",
        &["ALBUM=Suite", "TITLE=a", "TRACKNUMBER=1"],
    );
    put(
        &root,
        "Suite/Disc 2 - Liyue/01.flac",
        &[
            "ALBUM=Suite",
            "TITLE=b",
            "TRACKNUMBER=1",
            "DISCSUBTITLE=Liyue Harbor",
        ],
    );

    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(input.albums.len(), 1);
    assert_eq!(
        input.tracks[0].disc_subtitle, "Mondstadt",
        "folder fallback"
    );
    assert_eq!(
        input.tracks[1].disc_subtitle, "Liyue Harbor",
        "the tag wins"
    );
    let bytes = soul_audio_index::build::write_index(&input);
    let rdr = IndexReader::new(SliceSource::new(&bytes));
    let mut buf = [0u8; 128];
    let t0 = rdr.track(0).unwrap();
    assert_eq!(resolve(&rdr, t0.disc_subtitle, &mut buf), "Mondstadt");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn doubled_prefix_disc_folders_fold_into_one_album() {
    let root = fresh("md_cdcd");
    for (n, sub) in [
        (1, "Sakura and Violet Thunder  稻光与雷樱的土地"),
        (2, "Stories of the Floating World 浮世之枯荣"),
        (3, "Battles of Inazuma 稻妻征战记"),
    ] {
        put(
            &root,
            &format!("Inazuma/Disc CD 0{n} {sub}/01.flac"),
            &["ALBUM=Inazuma", "TITLE=a", "TRACKNUMBER=1"],
        );
    }
    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(input.albums.len(), 1);
    assert_eq!(input.albums[0].disc_count, 3);
    assert_eq!(
        input.folders[input.albums[0].folder as usize].path,
        "Inazuma"
    );
    let subs: Vec<&str> = input
        .tracks
        .iter()
        .map(|t| t.disc_subtitle.as_str())
        .collect();
    assert_eq!(
        subs,
        [
            "Sakura and Violet Thunder  稻光与雷樱的土地",
            "Stories of the Floating World 浮世之枯荣",
            "Battles of Inazuma 稻妻征战记"
        ]
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn same_title_in_two_folders_is_two_albums() {
    let root = fresh("md_iso");
    put(&root, "A/01.flac", &["ALBUM=Greatest Hits", "TITLE=a"]);
    put(&root, "B/01.flac", &["ALBUM=Greatest Hits", "TITLE=b"]);
    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(input.albums.len(), 2, "folder isolation holds");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn two_musicbrainz_releases_in_one_folder_are_two_albums() {
    let root = fresh("md_mb");
    let (a, b) = (
        "aaaaaaaa-0000-4000-8000-000000000001",
        "bbbbbbbb-0000-4000-8000-000000000002",
    );
    let ma = format!("MUSICBRAINZ_ALBUMID={a}");
    let mb = format!("MUSICBRAINZ_ALBUMID={b}");
    put(
        &root,
        "Dump/01.flac",
        &["ALBUM=Footprints", "TITLE=x", ma.as_str()],
    );
    put(
        &root,
        "Dump/02.flac",
        &["ALBUM=Footprints", "TITLE=y", mb.as_str()],
    );

    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(
        input.albums.len(),
        2,
        "two releases of one title stay apart"
    );
    assert_eq!(input.albums[0].mb_album_id, a);
    let bytes = soul_audio_index::build::write_index(&input);
    let rdr = IndexReader::new(SliceSource::new(&bytes));
    let mut buf = [0u8; 128];
    assert_eq!(
        resolve(&rdr, rdr.album(1).unwrap().mb_album_id, &mut buf),
        b
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn various_artists_without_album_artist_is_one_album() {
    let root = fresh("md_va");
    put(
        &root,
        "VA/01.flac",
        &["ALBUM=Hits", "ARTIST=Alpha", "TITLE=a"],
    );
    put(
        &root,
        "VA/02.flac",
        &["ALBUM=Hits", "ARTIST=Beta", "TITLE=b"],
    );
    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(input.albums.len(), 1, "album artist is not part of the key");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cover_in_album_root_serves_disc_folder_tracks() {
    let root = fresh("md_cover");
    put(
        &root,
        "Box/CD1/01.flac",
        &["ALBUM=Box", "TITLE=a", "DISCNUMBER=1"],
    );
    put(
        &root,
        "Box/CD2/01.flac",
        &["ALBUM=Box", "TITLE=b", "DISCNUMBER=2"],
    );
    let cover = b"album-root cover bytes".to_vec();
    std::fs::write(root.join("Box/cover.jpg"), &cover).unwrap();

    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(input.albums.len(), 1);
    assert_eq!(input.covers.len(), 1);
    assert_eq!(input.covers[0].as_deref(), Some(cover.as_slice()));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_disc_named_scan_root_is_never_climbed() {
    let outer = fresh("md_cdroot");
    let root = outer.join("CD1");
    std::fs::create_dir_all(&root).unwrap();
    put(&root, "01.flac", &["ALBUM=X", "TITLE=a"]);
    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(
        input.folders[input.albums[0].folder as usize].path, "",
        "the album root never leaves the scan root"
    );
    assert_eq!(input.tracks[0].disc_no, 1);
    let _ = std::fs::remove_dir_all(&outer);
}

#[test]
fn per_disc_folder_cover_beats_the_parent_decoy() {
    let root = fresh("md_disc_cover");
    put(
        &root,
        "Artist/Box/CD1/a.flac",
        &["ALBUM=Box", "TITLE=a", "DISCNUMBER=1"],
    );
    put(
        &root,
        "Artist/Box/CD2/b.flac",
        &["ALBUM=Box", "TITLE=b", "DISCNUMBER=2"],
    );
    let img_a = b"per-disc folder.jpg".to_vec();
    std::fs::write(root.join("Artist/Box/CD1/folder.jpg"), &img_a).unwrap();
    std::fs::write(root.join("Artist/cover.jpg"), b"artist photo decoy").unwrap();

    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(input.albums.len(), 1);
    assert_eq!(input.covers[0].as_deref(), Some(img_a.as_slice()));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn partially_tagged_release_stays_one_album() {
    let root = fresh("md_mb_partial");
    let x = "aaaaaaaa-0000-4000-8000-00000000000a";
    let mx = format!("MUSICBRAINZ_ALBUMID={x}");
    put(&root, "Alb/01.flac", &["ALBUM=Alb", "TITLE=a", mx.as_str()]);
    put(&root, "Alb/02.flac", &["ALBUM=Alb", "TITLE=b"]);
    put(&root, "Alb/03.flac", &["ALBUM=Alb", "TITLE=c", mx.as_str()]);

    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(
        input.albums.len(),
        1,
        "an untagged track joins the tagged release"
    );
    assert_eq!(input.albums[0].mb_album_id, x);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn untagged_track_joins_the_first_seen_release() {
    let root = fresh("md_mb_first");
    let (a, b) = (
        "aaaaaaaa-0000-4000-8000-000000000001",
        "bbbbbbbb-0000-4000-8000-000000000002",
    );
    let ma = format!("MUSICBRAINZ_ALBUMID={a}");
    let mb = format!("MUSICBRAINZ_ALBUMID={b}");
    // The untagged track sorts first; it still joins release `a` (first seen with an id).
    put(&root, "Dump/00.flac", &["ALBUM=Footprints", "TITLE=u"]);
    put(
        &root,
        "Dump/01.flac",
        &["ALBUM=Footprints", "TITLE=x", ma.as_str()],
    );
    put(
        &root,
        "Dump/02.flac",
        &["ALBUM=Footprints", "TITLE=y", mb.as_str()],
    );

    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(input.albums.len(), 2);
    assert_eq!(input.albums[0].mb_album_id, a);
    assert_eq!(input.albums[1].mb_album_id, b);
    let albums: Vec<u32> = input.tracks.iter().map(|t| t.album).collect();
    assert_eq!(albums, vec![0, 0, 1]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn disc_folders_directly_under_the_scan_root_fold_into_it() {
    let root = fresh("md_rootdiscs");
    put(&root, "CD1/x.flac", &["ALBUM=R", "TITLE=x"]);
    put(&root, "CD2/y.flac", &["ALBUM=R", "TITLE=y"]);

    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(input.albums.len(), 1);
    assert_eq!(input.folders[input.albums[0].folder as usize].path, "");
    let discs: Vec<u16> = input.tracks.iter().map(|t| t.disc_no).collect();
    assert_eq!(discs, vec![1, 2]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn out_of_range_disc_tag_is_treated_as_untagged() {
    let root = fresh("md_disc_overflow");
    // 65536 would wrap to 0 under an `as u16` cast and bypass the "tag if > 0" rule.
    put(
        &root,
        "Box/CD2/01.flac",
        &["ALBUM=Box", "TITLE=a", "DISCNUMBER=65536/65536"],
    );

    let input = soul_audio_import::scan(&root).expect("scan");
    assert_eq!(
        input.tracks[0].disc_no, 2,
        "falls back to the disc-folder number"
    );
    assert_eq!(input.albums[0].disc_count, 2);
    let _ = std::fs::remove_dir_all(&root);
}
