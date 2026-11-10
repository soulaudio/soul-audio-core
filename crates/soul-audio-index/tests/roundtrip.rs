//! The `.soulidx` contract, as a host-side round-trip (no board needed):
//! bake an index from a fixture catalog, read it back with the no-alloc reader
//! over a `SliceSource`, and assert the browse order + fields survive.
//!
//! This is written RED first: against the skeleton baker/reader it fails. The
//! codec implementation is "done" when this is green while the no-alloc gate
//! stays green.
#![cfg(feature = "build")]

use image::{GrayImage, ImageEncoder, Luma};
use soul_audio_artbake::ArtBlobReader;
use soul_audio_catalog::{ArtStore, BrowseAxis, Catalog, NO_FOLDER};
use soul_audio_core::SliceSource;
use soul_audio_index::IndexReader;
use soul_audio_index::build::{
    ART_H, ART_W, AlbumMeta, CatalogInput, FolderInput, PlaylistInput, TrackInput, bake_art,
    write_index, write_index_with_art,
};

fn fixture() -> CatalogInput {
    CatalogInput {
        tracks: vec![
            TrackInput {
                title: "Aria".into(),
                artist: 0,
                album: 0,
                album_artist: 0,
                folder: 1,
                track_no: 1,
                disc_no: 1,
                duration_ms: 299_520,
                sample_rate: 44_100,
                channels: 2,
                file_name: "01 - Aria.flac".into(),
            },
            TrackInput {
                title: "Variatio 1".into(),
                artist: 0,
                album: 0,
                album_artist: 0,
                folder: 1,
                track_no: 2,
                disc_no: 1,
                duration_ms: 110_000,
                sample_rate: 44_100,
                channels: 2,
                file_name: "02 - Variatio 1.flac".into(),
            },
        ],
        artists: vec!["Kimiko Ishizaka".into()],
        artist_sorts: vec!["Ishizaka, Kimiko".into()],
        albums: vec![AlbumMeta {
            title: "Open Goldberg Variations".into(),
            album_artist: 0,
            year: 2012,
            folder: 1,
        }],
        // Folder 0 = root (empty path, no parent); folder 1 = the album dir.
        folders: vec![
            FolderInput {
                path: "".into(),
                parent: NO_FOLDER,
            },
            FolderInput {
                path: "Bach/Open Goldberg".into(),
                parent: 0,
            },
        ],
        playlists: vec![PlaylistInput {
            name: "Favourites".into(),
            tracks: vec![1, 0],
        }],
        covers: Vec::new(),
    }
}

fn resolve<'a>(
    rdr: &impl Catalog,
    s: soul_audio_catalog::StrRef,
    buf: &'a mut [u8; 64],
) -> &'a str {
    let n = rdr.resolve_str(s, buf).expect("resolve_str");
    core::str::from_utf8(&buf[..n]).expect("utf8")
}

#[test]
fn soulidx_round_trips_tracks_order_and_playlists() {
    let bytes = write_index(&fixture());
    let rdr = IndexReader::new(SliceSource::new(&bytes));

    // Tier-1 track fields survive.
    assert_eq!(rdr.track_count(), 2, "track count");
    let t0 = rdr.track(0).expect("track 0");
    let mut buf = [0u8; 64];
    assert_eq!(resolve(&rdr, t0.title, &mut buf), "Aria");
    assert_eq!(t0.duration_ms, 299_520);
    assert_eq!(t0.track_no, 1);
    assert_eq!(t0.sample_rate, 44_100);
    // File name survives on the track record (the playback locator's leaf).
    assert_eq!(resolve(&rdr, t0.file_name, &mut buf), "01 - Aria.flac");

    // Folder table: the track's folder resolves to its root-relative path, and
    // folder.path + file_name is the physical location.
    assert_eq!(rdr.folder_count(), 2, "root + album dir");
    let f = rdr.folder(t0.folder).expect("folder");
    assert_eq!(f.id, t0.folder);
    assert_eq!(resolve(&rdr, f.path, &mut buf), "Bach/Open Goldberg");
    let n = rdr.folder_path(t0.folder, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"Bach/Open Goldberg");
    // Its parent is the root folder (empty path, no parent of its own).
    let root = rdr.folder(f.parent).expect("root folder");
    assert_eq!(root.path.len, 0);
    assert_eq!(root.parent, NO_FOLDER);

    // Artist entity + album title resolve through their id refs.
    assert_eq!(rdr.artist_count(), 1);
    let n = rdr.artist_name(t0.artist, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"Kimiko Ishizaka");
    assert_eq!(rdr.album_count(), 1);
    let album = rdr.album(t0.album).expect("album 0");
    assert_eq!(
        resolve(&rdr, album.title, &mut buf),
        "Open Goldberg Variations"
    );
    assert_eq!(album.year, 2012);

    // Desktop-baked browse order is readable.
    assert_eq!(rdr.order_len(BrowseAxis::Title), 2, "title axis len");
    // "Aria" sorts before "Variatio 1".
    assert_eq!(rdr.order(BrowseAxis::Title, 0).unwrap(), 0);
    assert_eq!(rdr.order(BrowseAxis::Title, 1).unwrap(), 1);

    // Playlist lives in the index and round-trips, order preserved.
    assert_eq!(rdr.playlist_count(), 1, "playlist count");
    assert_eq!(rdr.playlist_len(0), 2, "playlist len");
    assert_eq!(rdr.playlist_track(0, 0).unwrap(), 1);
    assert_eq!(rdr.playlist_track(0, 1).unwrap(), 0);
    assert_eq!(rdr.playlist_name(0, &mut buf).unwrap(), "Favourites".len());
}

#[test]
fn baked_title_order_is_collation_sorted() {
    // The baker sorts the Title axis by collation key: leading articles fold
    // ("The Kinks" files under K, after "kinks"), and numbers sort naturally
    // ("Track 2" before "Track 10"). The device reads this frozen order.
    let titles = ["The Kinks", "ABBA", "Track 10", "Track 2", "kinks"];
    let tracks = titles
        .iter()
        .map(|&t| TrackInput {
            title: t.to_string(),
            artist: 0,
            album: 0,
            album_artist: 0,
            folder: 0,
            track_no: 0,
            disc_no: 1,
            duration_ms: 0,
            sample_rate: 44_100,
            channels: 2,
            file_name: format!("{t}.flac"),
        })
        .collect();
    let input = CatalogInput {
        tracks,
        artists: vec!["A".to_string()],
        artist_sorts: Vec::new(),
        albums: vec![AlbumMeta {
            title: "A".to_string(),
            album_artist: 0,
            year: 0,
            folder: 0,
        }],
        folders: vec![FolderInput {
            path: String::new(),
            parent: NO_FOLDER,
        }],
        playlists: vec![],
        covers: Vec::new(),
    };

    let bytes = write_index(&input);
    let rdr = IndexReader::new(SliceSource::new(&bytes));
    let mut buf = [0u8; 128];
    let order: Vec<String> = (0..rdr.order_len(BrowseAxis::Title))
        .map(|i| {
            let tid = rdr.order(BrowseAxis::Title, i).unwrap();
            let t = rdr.track(tid).unwrap();
            let n = rdr.resolve_str(t.title, &mut buf).unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        })
        .collect();

    assert_eq!(
        order,
        vec!["ABBA", "kinks", "The Kinks", "Track 2", "Track 10"],
    );
}

/// A tiny gray PNG, generated in-test — same pattern as
/// `soul-audio-artbake/tests/roundtrip.rs`'s `png_of`.
fn png_of(w: u32, h: u32, f: impl Fn(u32, u32) -> u8) -> Vec<u8> {
    let img = GrayImage::from_fn(w, h, |x, y| Luma([f(x, y)]));
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(img.as_raw(), w, h, image::ExtendedColorType::L8)
        .unwrap();
    out
}

/// End-to-end: `bake_art` encodes the covered album's cover into `art.blob` and
/// assigns it a non-zero `art_id`; `write_index_with_art` threads that id into
/// both the album record and its track's record; the uncovered album/track
/// stay `art_id == 0`; and the id resolves back through `ArtBlobReader` to the
/// encoded bitmap.
#[test]
fn bakes_cover_art_into_blob_and_assigns_art_id() {
    let cover_png = png_of(32, 32, |x, y| (((x + y) * 7) % 256) as u8);
    let input = CatalogInput {
        tracks: vec![
            TrackInput {
                title: "Covered Track".into(),
                artist: 0,
                album: 0,
                album_artist: 0,
                folder: 0,
                track_no: 1,
                disc_no: 1,
                duration_ms: 1000,
                sample_rate: 44_100,
                channels: 2,
                file_name: "a.flac".into(),
            },
            TrackInput {
                title: "Uncovered Track".into(),
                artist: 0,
                album: 1,
                album_artist: 0,
                folder: 0,
                track_no: 1,
                disc_no: 1,
                duration_ms: 1000,
                sample_rate: 44_100,
                channels: 2,
                file_name: "b.flac".into(),
            },
        ],
        artists: vec!["Artist".into()],
        artist_sorts: Vec::new(),
        albums: vec![
            AlbumMeta {
                title: "Covered Album".into(),
                album_artist: 0,
                year: 2020,
                folder: 0,
            },
            AlbumMeta {
                title: "Uncovered Album".into(),
                album_artist: 0,
                year: 2021,
                folder: 0,
            },
        ],
        folders: vec![FolderInput {
            path: "".into(),
            parent: NO_FOLDER,
        }],
        playlists: vec![],
        covers: vec![Some(cover_png.clone()), None],
    };

    let (art_blob, album_art_id) = bake_art(&input, ART_W, ART_H);
    let bytes = write_index_with_art(&input, &album_art_id);
    let rdr = IndexReader::new(SliceSource::new(&bytes));

    // Covered album's record and its track share one non-zero art_id.
    let album0 = rdr.album(0).expect("album 0");
    assert_ne!(album0.art_id, 0, "covered album gets a non-zero art_id");
    let track0 = rdr.track(0).expect("track 0");
    assert_eq!(
        track0.art_id, album0.art_id,
        "track's art_id is its album's"
    );

    // Uncovered album's record and its track stay art_id == 0.
    let album1 = rdr.album(1).expect("album 1");
    assert_eq!(album1.art_id, 0, "uncovered album has art_id 0");
    let track1 = rdr.track(1).expect("track 1");
    assert_eq!(track1.art_id, 0, "uncovered album's track has art_id 0");

    // The id resolves back through the blob to the encoded bitmap.
    let art_rdr = ArtBlobReader::new(SliceSource::new(&art_blob));
    let want_len = soul_audio_artbake::row_bytes(ART_W) * ART_H as usize;
    let mut buf = vec![0u8; want_len];
    let n = art_rdr
        .art(album0.art_id, &mut buf)
        .expect("art_id resolves in art.blob");
    assert_eq!(n, want_len);
    assert_eq!(
        &buf[..n],
        soul_audio_artbake::build::encode_cover(&cover_png, ART_W, ART_H)
            .unwrap()
            .as_slice(),
        "resolved bytes match the encoder's output exactly, not just its length"
    );
}

#[test]
fn artist_album_track_orders_by_sort_name() {
    // Two artists whose SORT order differs from their DISPLAY order:
    //   "Brian Eno"    → sort "Eno, Brian"    (e…)
    //   "Eric Clapton" → sort "Clapton, Eric" (c…)
    // By display name "Brian Eno" (b) precedes "Eric Clapton" (e); by sort name
    // "Clapton, Eric" (c) precedes "Eno, Brian" (e) — the opposite order.
    let input = CatalogInput {
        tracks: vec![
            TrackInput {
                title: "eno_track".into(),
                artist: 0,
                album: 0,
                album_artist: 0,
                folder: 0,
                track_no: 1,
                disc_no: 1,
                duration_ms: 0,
                sample_rate: 44_100,
                channels: 2,
                file_name: "eno.flac".into(),
            },
            TrackInput {
                title: "clapton_track".into(),
                artist: 1,
                album: 1,
                album_artist: 1,
                folder: 0,
                track_no: 1,
                disc_no: 1,
                duration_ms: 0,
                sample_rate: 44_100,
                channels: 2,
                file_name: "clapton.flac".into(),
            },
        ],
        artists: vec!["Brian Eno".into(), "Eric Clapton".into()],
        artist_sorts: vec!["Eno, Brian".into(), "Clapton, Eric".into()],
        albums: vec![
            AlbumMeta {
                title: "Here Come the Warm Jets".into(),
                album_artist: 0,
                year: 1974,
                folder: 0,
            },
            AlbumMeta {
                title: "Slowhand".into(),
                album_artist: 1,
                year: 1977,
                folder: 0,
            },
        ],
        folders: vec![FolderInput {
            path: String::new(),
            parent: NO_FOLDER,
        }],
        playlists: vec![],
        covers: Vec::new(),
    };
    let bytes = write_index(&input);
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
    // Clapton (sorts under C) before Eno (sorts under E) — sort-name order,
    // the reverse of display-name order.
    assert_eq!(order, vec!["clapton_track", "eno_track"]);

    // Display names are preserved; the v2 sort-name table reads back.
    let n = rdr.artist_name(0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"Brian Eno");
    let n = rdr.artist_sort_name(0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"Eno, Brian");
    let n = rdr.artist_sort_name(1, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"Clapton, Eric");
}

#[test]
fn artist_sort_name_falls_back_to_display_when_untagged() {
    // Empty artist_sorts → the sort-name table stores the display-name ref, so
    // artist_sort_name mirrors artist_name (untagged libraries are unchanged).
    let input = CatalogInput {
        tracks: vec![TrackInput {
            title: "t".into(),
            artist: 0,
            album: 0,
            album_artist: 0,
            folder: 0,
            track_no: 1,
            disc_no: 1,
            duration_ms: 0,
            sample_rate: 44_100,
            channels: 2,
            file_name: "t.flac".into(),
        }],
        artists: vec!["Miles Davis".into()],
        artist_sorts: Vec::new(),
        albums: vec![AlbumMeta {
            title: "Kind of Blue".into(),
            album_artist: 0,
            year: 1959,
            folder: 0,
        }],
        folders: vec![FolderInput {
            path: String::new(),
            parent: NO_FOLDER,
        }],
        playlists: vec![],
        covers: Vec::new(),
    };
    let bytes = write_index(&input);
    let rdr = IndexReader::new(SliceSource::new(&bytes));
    let mut buf = [0u8; 128];
    let n = rdr.artist_sort_name(0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"Miles Davis");
}
