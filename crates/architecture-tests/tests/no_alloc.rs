//! Prove `soul-audio-core` allocates nothing at runtime by installing a
//! counting global allocator and asserting the counter does not move while core
//! code runs.
//!
//! TEMPLATE: decoders are stubs, so this currently measures the source-model
//! plumbing (`SliceSource` read/seek + a caller-buffered `BufReader`). Once you
//! implement a decoder, wrap a full `decode_into` loop in the measured region —
//! the assertion is what enforces the no-alloc contract at runtime (the
//! `thumbv7em` build enforces it at compile time).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

// The counting allocator is a single global; two tests measuring it in parallel
// would corrupt each other's before/after. Serialize the measured regions.
static MEASURE: Mutex<()> = Mutex::new(());

use embedded_io::{Read, Seek, SeekFrom};
use soul_audio_core::{BufReader, SliceSource};

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: forwarding to the system allocator with the same layout.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarding to the system allocator with the same ptr/layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GA: Counting = Counting;

#[test]
fn core_io_does_not_allocate() {
    // Buffers are set up BEFORE measuring — that setup may allocate.
    let data = [0u8; 256];
    let mut out = [0u8; 64];
    let mut scratch = [0u8; 16];

    let _guard = MEASURE.lock().unwrap();
    let before = ALLOCS.load(Ordering::SeqCst);

    // Pure no-alloc plumbing across the whole capability ladder:
    // 1) SliceSource: read the slice in chunks, then seek and read again.
    let mut slice = SliceSource::new(&data);
    while slice.read(&mut out).unwrap() != 0 {}
    slice.seek(SeekFrom::Start(0)).unwrap();

    // 2) BufReader over a Source, buffering through a caller-owned scratch buffer.
    let mut buffered = BufReader::new(SliceSource::new(&data), &mut scratch);
    while buffered.read(&mut out).unwrap() != 0 {}

    let after = ALLOCS.load(Ordering::SeqCst);

    assert_eq!(before, after, "soul-audio-core io plumbing allocated");
}

/// The `.soulidx` READER path must not allocate — it reads records/strings into
/// caller buffers over a seekable source. Skeleton stage: the reader stubs do
/// no work and no alloc; once the codec lands (and actually seeks/reads), this
/// is what keeps it heap-free on the DAP. (The baker is `build`-gated std and is
/// deliberately not exercised here.)
#[test]
fn index_reader_does_not_allocate() {
    use soul_audio_catalog::{BrowseAxis, Catalog, StrRef};
    use soul_audio_core::SliceSource;
    use soul_audio_index::IndexReader;

    let bytes = *b"SIDX\x01\x00"; // magic + format version
    let reader = IndexReader::new(SliceSource::new(&bytes));
    let mut buf = [0u8; 64];

    let _guard = MEASURE.lock().unwrap();
    let before = ALLOCS.load(Ordering::SeqCst);

    let _ = reader.track_count();
    let _ = reader.album_count();
    let _ = reader.artist_count();
    let _ = reader.track(0);
    let _ = reader.album(0);
    let _ = reader.artist_name(0, &mut buf);
    let _ = reader.resolve_str(StrRef::default(), &mut buf);
    let _ = reader.order_len(BrowseAxis::Title);
    let _ = reader.order(BrowseAxis::ArtistAlbumTrack, 0);
    let _ = reader.album_track(0, 0);
    let _ = reader.folder_count();
    let _ = reader.folder(0);
    let _ = reader.folder_path(0, &mut buf);
    let _ = reader.playlist_track(0, 0);
    let _ = reader.playlist_name(0, &mut buf);

    let after = ALLOCS.load(Ordering::SeqCst);

    assert_eq!(before, after, "the .soulidx reader path allocated");
}
