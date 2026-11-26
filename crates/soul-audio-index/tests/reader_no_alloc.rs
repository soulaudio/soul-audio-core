//! The v8 reader path over REAL baked bytes allocates nothing. The
//! architecture-tests probe only sees an empty header, so this one exercises the
//! new disc fields end to end (bake first, then measure the reads).
#![cfg(feature = "build")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use soul_audio_catalog::{Catalog, NO_FOLDER, PackedDate};
use soul_audio_core::SliceSource;
use soul_audio_index::IndexReader;
use soul_audio_index::build::{AlbumMeta, CatalogInput, FolderInput, TrackInput, write_index};

thread_local! {
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

struct Counting;
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.with(|c| c.set(c.get().wrapping_add(1)));
        // SAFETY: forwarding to the system allocator with the same layout.
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: forwarding to the system allocator with the same ptr/layout.
        unsafe { System.dealloc(p, l) }
    }
}
#[global_allocator]
static GA: Counting = Counting;

#[test]
fn v8_reader_reads_disc_fields_without_allocating() {
    let input = CatalogInput {
        tracks: vec![TrackInput {
            title: "Dawn".into(),
            artist: 0,
            album: 0,
            album_artist: 0,
            genre: 0,
            folder: 1,
            track_no: 1,
            disc_no: 2,
            duration_ms: 1000,
            sample_rate: 48_000,
            channels: 2,
            file_name: "01 Dawn.flac".into(),
            mbid: String::new(),
            disc_subtitle: "Mondstadt".into(),
            loudness: None,
            cue_range: None,
        }],
        artists: vec!["HOYO-MiX".into()],
        artist_sorts: Vec::new(),
        genres: vec!["Game".into()],
        albums: vec![AlbumMeta {
            title: "Suite".into(),
            album_artist: 0,
            original_date: PackedDate::UNKNOWN,
            release_date: PackedDate::UNKNOWN,
            folder: 1,
            disc_count: 2,
            mb_album_id: "0b1e5b0a-1111-4222-8333-444455556666".into(),
        }],
        folders: vec![
            FolderInput {
                path: String::new(),
                parent: NO_FOLDER,
            },
            FolderInput {
                path: "Suite".into(),
                parent: 0,
            },
        ],
        playlists: Vec::new(),
        covers: Vec::new(),
    };
    let bytes = write_index(&input);
    let rdr = IndexReader::new(SliceSource::new(&bytes));
    let mut buf = [0u8; 64];

    let before = ALLOCS.with(Cell::get);
    let t = rdr.track(0).unwrap();
    let n = rdr.resolve_str(t.disc_subtitle, &mut buf).unwrap();
    let a = rdr.album(t.album).unwrap();
    let m = rdr.resolve_str(a.mb_album_id, &mut buf).unwrap();
    let first = rdr.album_track(0, 0).unwrap();
    let after = ALLOCS.with(Cell::get);

    assert_eq!(before, after, "the v8 reader path allocated");
    assert_eq!((n, a.disc_count, m, first), (9, 2, 36, 0));
}
