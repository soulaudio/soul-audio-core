//! The `.soulidx` contract, as a host-side round-trip (no board needed):
//! bake an index from a fixture catalog, read it back with the no-alloc reader
//! over a `SliceSource`, and assert the browse order + fields survive.
//!
//! This is written RED first: against the skeleton baker/reader it fails. The
//! codec implementation is "done" when this is green while the no-alloc gate
//! stays green.
#![cfg(feature = "build")]

use soul_audio_catalog::{BrowseAxis, Catalog, NO_FOLDER};
use soul_audio_core::SliceSource;
use soul_audio_index::IndexReader;
use soul_audio_index::build::{
    AlbumMeta, CatalogInput, FolderInput, PlaylistInput, TrackInput, write_index,
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
