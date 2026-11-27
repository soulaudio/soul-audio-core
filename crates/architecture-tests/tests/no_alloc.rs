//! Prove the DAP's code paths allocate nothing at runtime: install a counting
//! global allocator and assert the counter does not move while they run.
//!
//! # Why a runtime gate at all
//!
//! The `thumbv7em` build already enforces no-alloc at *compile* time — there is
//! no allocator to link against. But that build is `--no-default-features`,
//! so it never sees a path behind a default or `std` feature, and it cannot
//! catch a dependency that allocates only on some configurations. This gate
//! runs the same code the player runs, with the host's features on, and counts.
//!
//! # What is measured
//!
//! The whole audio path, not just the plumbing: the source ladder, a real
//! decode-and-seek loop over every container `open` can build, the full
//! deck pipeline (decode → resample → channel-map → sink), and the `.soulidx`
//! reader over a **real baked index**.
//!
//! Everything that may legitimately allocate — reading a fixture off disk,
//! baking the index, constructing the decoder or the deck — happens *before*
//! the measured region. That is the whole discipline of this file: if setup
//! leaked into the measurement the gate would be noise, and if the measurement
//! does not reach the real work the gate is a placeholder. It was a placeholder
//! until #162: the io test exercised no decoder at all, and the index test fed
//! a foreign-version header, so every accessor returned before touching the
//! source.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use embedded_io::{Read, Seek, SeekFrom};
use soul_audio_core::{BufReader, SliceSource};

thread_local! {
    // Count allocations PER THREAD, not globally: a global counter is corrupted
    // by allocations on the libtest harness's other threads during the measured
    // window (racy — it flaked on CI, 189 vs 190, while passing locally by
    // timing). Each test's measured region runs on its own thread, so a
    // per-thread counter measures exactly the plumbing under test and nothing
    // else. `const`-initialised so the thread-local never allocates on first
    // access — no recursion back into this allocator.
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.with(|c| c.set(c.get().wrapping_add(1)));
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

    let before = ALLOCS.with(Cell::get);

    // Pure no-alloc plumbing across the whole capability ladder:
    // 1) SliceSource: read the slice in chunks, then seek and read again.
    let mut slice = SliceSource::new(&data);
    while slice.read(&mut out).unwrap() != 0 {}
    slice.seek(SeekFrom::Start(0)).unwrap();

    // 2) BufReader over a Source, buffering through a caller-owned scratch buffer.
    let mut buffered = BufReader::new(SliceSource::new(&data), &mut scratch);
    while buffered.read(&mut out).unwrap() != 0 {}

    let after = ALLOCS.with(Cell::get);

    assert_eq!(before, after, "soul-audio-core io plumbing allocated");
}

/// The `.soulidx` READER path must not allocate — it reads records, strings,
/// order tables and playlist runs into caller buffers over a seekable source.
///
/// It is measured over a **real baked index**. The previous version passed a
/// foreign-version header (`SIDX` + version 8 while the format is at 9), which
/// the reader correctly treats as an empty catalog — so every accessor returned
/// `OutOfRange` immediately and the test proved nothing about the read path.
/// The baker is host-side `std` and runs before the measured region.
#[test]
fn index_reader_does_not_allocate() {
    use soul_audio_catalog::{BrowseAxis, Catalog, NO_FOLDER, PackedDate};
    use soul_audio_core::SliceSource;
    use soul_audio_index::IndexReader;
    use soul_audio_index::build::{
        AlbumMeta, CatalogInput, FolderInput, PlaylistInput, TrackInput, write_index,
    };

    fn track(title: &str, folder: u32) -> TrackInput {
        TrackInput {
            title: title.into(),
            artist: 0,
            album: 0,
            album_artist: 0,
            genre: 0,
            folder,
            track_no: 1,
            disc_no: 1,
            duration_ms: 180_000,
            sample_rate: 44_100,
            channels: 2,
            file_name: format!("{title}.flac"),
            mbid: String::new(),
            disc_subtitle: String::new(),
            loudness: None,
            cue_range: None,
        }
    }

    // Bake a real index: several tracks, a non-ASCII string in the heap (so the
    // reader walks a multi-byte value), and a playlist.
    let bytes = write_index(&CatalogInput {
        tracks: vec![
            track("Aria", 1),
            track("Variatio 1", 1),
            track("Καληνύχτα", 1),
        ],
        artists: vec!["Kimiko Ishizaka".into()],
        artist_sorts: vec!["Ishizaka, Kimiko".into()],
        genres: vec!["Classical".into()],
        albums: vec![AlbumMeta {
            title: "Open Goldberg Variations".into(),
            album_artist: 0,
            original_date: PackedDate::from_year(1741),
            release_date: PackedDate::from_year(2012),
            disc_count: 1,
            mb_album_id: String::new(),
            folder: 1,
        }],
        folders: vec![
            FolderInput {
                path: String::new(),
                parent: NO_FOLDER,
            },
            FolderInput {
                path: "Bach/Open Goldberg".into(),
                parent: 0,
            },
        ],
        playlists: vec![PlaylistInput {
            name: "Favourites".into(),
            tracks: vec![2, 0, 1],
        }],
        covers: Vec::new(),
    });

    let reader = IndexReader::new(SliceSource::new(&bytes));
    assert!(
        reader.is_current(),
        "the baked index must read as current, or this measures nothing"
    );
    let mut buf = [0u8; 256];

    let before = ALLOCS.with(Cell::get);

    // The full browse path: counts, records, every string accessor, each axis,
    // album/playlist runs, and the locator lookup a resume-from-disk does.
    let tracks = reader.track_count();
    let _ = (
        reader.album_count(),
        reader.artist_count(),
        reader.genre_count(),
    );
    for id in 0..tracks {
        let t = reader.track(id).expect("baked track reads back");
        let _ = reader
            .resolve_str(t.title, &mut buf)
            .expect("title resolves");
        let _ = reader.resolve_str(t.file_name, &mut buf);
    }
    let a = reader.album(0).expect("baked album reads back");
    let _ = reader.resolve_str(a.title, &mut buf);
    let _ = reader.artist_name(0, &mut buf).expect("artist resolves");
    let _ = reader.artist_sort_name(0, &mut buf);
    let _ = reader.genre_name(0, &mut buf);
    for axis in [
        BrowseAxis::Title,
        BrowseAxis::ArtistAlbumTrack,
        BrowseAxis::Album,
        BrowseAxis::Folder,
        BrowseAxis::Genre,
    ] {
        for i in 0..reader.order_len(axis) {
            let _ = reader.order(axis, i).expect("order slot reads back");
        }
    }
    for i in 0..reader.album_track_count(0) {
        let _ = reader.album_track(0, i).expect("album run reads back");
    }
    for id in 0..reader.folder_count() {
        let _ = reader.folder(id);
        let _ = reader.folder_path(id, &mut buf);
    }
    for p in 0..reader.playlist_count() {
        let _ = reader.playlist_name(p, &mut buf);
        for i in 0..reader.playlist_len(p) {
            let _ = reader
                .playlist_track(p, i)
                .expect("playlist run reads back");
        }
    }
    let _ = reader.resolve_locator("Bach/Open Goldberg/Aria.flac");

    let after = ALLOCS.with(Cell::get);

    assert_eq!(before, after, "the .soulidx reader path allocated");
}

/// The real decode path must not allocate: for every container `open` can
/// build, decode the whole fixture and then seek back and decode again — all
/// inside the measured region, with the decoder constructed outside it.
///
/// This is the gate's point and it was missing: before #162 nothing here
/// touched a decoder, so the no-alloc contract for the DAP's audio path was
/// only enforced by the `thumbv7em` build — which is `--no-default-features`
/// and so never sees a path behind a host feature.
#[test]
fn decoding_every_container_does_not_allocate() {
    use soul_audio_core::{Decoder, decoder_scratch_len, open, open_seekable};

    // One fixture per container the default feature set can decode, with the
    // entry point it needs. (Vorbis needs a caller-provided arena that only the
    // host crate owns, and AAC is patent-gated off by default — both are
    // excluded deliberately rather than silently missing.)
    //
    // `seekable` is not a fallback: an MP4 whose `moov` follows its `mdat`
    // genuinely requires random access, and that is what `open_seekable` is
    // for. Stating it per fixture keeps a future regression in the streaming
    // path from being papered over by a retry.
    //
    // Paths are relative to the workspace root, which is why the ALAC entry
    // reaches into `soul-audio-core`'s own fixtures: the root-level
    // `*-alac.m4a` files are for the desktop differential suite and this
    // crate's `open` declines them, while `alac.m4a` is the one core itself
    // decodes in its conformance tests.
    const FIXTURES: &[(&str, bool)] = &[
        ("testdata/tone.wav", false),
        ("testdata/tone-aiff.aiff", false),
        ("testdata/tone.flac", false),
        ("testdata/tone.mp3", false),
        ("testdata/tone.caf", false),
        ("testdata/tone.wv", false),
        ("testdata/tone.opus", false),
        ("testdata/tone-flac.mka", false),
        ("crates/soul-audio-core/testdata/alac.m4a", false),
        // An MP4 whose `moov` follows its `mdat`: genuinely needs random
        // access, which is what `open_seekable` is for.
        (
            "crates/soul-audio-core/testdata/alac-nonfaststart.m4a",
            true,
        ),
        ("crates/soul-audio-core/testdata/flac-in-ogg.ogg", false),
    ];

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("crates/architecture-tests -> workspace root");

    for &(name, seekable) in FIXTURES {
        let path = root.join(name);
        let Ok(bytes) = std::fs::read(&path) else {
            panic!("fixture {name} missing — this gate must not silently skip a container");
        };

        // Everything that may allocate happens here, before the measurement:
        // reading the file, the scratch plane, and building the decoder.
        // `decoder_scratch_len()` is the crate's own answer for how big that
        // plane must be — a smaller one makes WavPack and ALAC decline as
        // `Unsupported`, which is how an earlier draft of this test "proved"
        // those containers unsupported rather than measuring them.
        let mut scratch = vec![0i32; decoder_scratch_len()];
        let mut out = vec![0i32; 8192];

        // Streaming decode (the DAP's normal path).
        let mut dec = if seekable {
            open_seekable(SliceSource::new(&bytes), scratch.as_mut_slice())
        } else {
            open(SliceSource::new(&bytes), scratch.as_mut_slice())
        }
        .unwrap_or_else(|e| panic!("open {name}: {e:?}"));

        let before = ALLOCS.with(Cell::get);
        let mut frames = 0usize;
        loop {
            let n = dec
                .decode_into(&mut out)
                .unwrap_or_else(|e| panic!("decode {name}: {e:?}"));
            if n == 0 {
                break;
            }
            frames += n;
        }
        let after = ALLOCS.with(Cell::get);
        assert_eq!(before, after, "decoding {name} allocated");
        assert!(
            frames > 0,
            "{name} decoded nothing — the gate measured idling"
        );

        // And the seek path, which resets codec state and refills buffers —
        // the likeliest place for a hidden allocation.
        let mut seekable = open_seekable(SliceSource::new(&bytes), scratch.as_mut_slice())
            .unwrap_or_else(|e| panic!("open_seekable {name}: {e:?}"));

        let before = ALLOCS.with(Cell::get);
        for target in [0u64, 1024, 64, 4096, 0] {
            let _ = seekable.seek_to(target);
            let _ = seekable.decode_into(&mut out);
        }
        let after = ALLOCS.with(Cell::get);
        assert_eq!(before, after, "seeking {name} allocated");
    }
}

/// The **whole deck pipeline** must not allocate while it plays: decode →
/// resample → channel-map → sink, which is exactly what runs on the DAP
/// between the SD card and the SAI DMA.
///
/// The fixture is 44.1 kHz mono and the sink 48 kHz stereo, so the resampler
/// and the channel map are both engaged — a passthrough configuration would
/// measure far less than it appears to.
#[test]
fn the_deck_pipeline_does_not_allocate_while_playing() {
    use soul_audio_core::{
        AnyDecoder, Deck, Progress, Quality, Sink, SinkError, StreamInfo, model::SampleFormat, open,
    };

    /// A sink that counts frames and keeps nothing — allocation-free by
    /// construction, so anything the counter sees came from the deck.
    struct CountingSink {
        frames: usize,
    }
    impl Sink for CountingSink {
        fn stream_info(&self) -> StreamInfo {
            StreamInfo {
                sample_rate: 48_000,
                channels: 2,
                format: SampleFormat::Int { bits: 24 },
            }
        }
        fn write(&mut self, frames: &[i32]) -> Result<(), SinkError> {
            self.frames += frames.len();
            Ok(())
        }
        fn flush(&mut self) -> Result<(), SinkError> {
            Ok(())
        }
    }

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .join("testdata/mp3-mono.mp3");
    let bytes = std::fs::read(&path).expect("mono fixture");

    // Setup outside the measurement: the scratch plane, the decoder, the deck
    // (which embeds large fixed buffers by value), and the load.
    let mut scratch = vec![0i32; 1 << 16];
    let decoder: AnyDecoder<_, _> =
        open(SliceSource::new(&bytes), scratch.as_mut_slice()).expect("open mono mp3");
    let mut deck: Deck<CountingSink, AnyDecoder<_, _>, 1> =
        Deck::new(CountingSink { frames: 0 }, Quality::Hq);
    deck.load(decoder).expect("load");
    // One pump before measuring, so first-block resampler/buffer setup (which
    // is allowed to allocate on the host) is not attributed to steady state.
    let _ = deck.pump().expect("first pump");

    let before = ALLOCS.with(Cell::get);
    let mut pumps = 0;
    while pumps < 400 {
        match deck.pump().expect("pump") {
            Progress::Played | Progress::TrackChanged => pumps += 1,
            // Ended / Idle: the fixture ran out, which is a fine place to stop.
            Progress::Ended | Progress::Idle => break,
        }
    }
    let after = ALLOCS.with(Cell::get);

    assert_eq!(before, after, "the deck pipeline allocated while playing");
    assert!(
        pumps > 0,
        "the deck produced nothing — the gate measured idling"
    );
    assert!(
        deck.sink().frames > 0,
        "no frames reached the sink — the pipeline was not actually running"
    );
}

/// The harness itself, pinned: a counter that never moves would make every
/// test above pass vacuously, which is the failure mode this whole file is
/// vulnerable to. One `Box` must be seen; a measured region that only touches
/// stack data must not be.
#[test]
fn the_allocation_counter_actually_counts() {
    let before = ALLOCS.with(Cell::get);
    let boxed = Box::new(0u64);
    let after = ALLOCS.with(Cell::get);
    assert!(
        after > before,
        "the counting allocator did not observe a Box — every no-alloc \
         assertion in this file would pass vacuously"
    );
    drop(boxed);

    // And it does not fire on stack-only work, or the gates would be flaky.
    let before = ALLOCS.with(Cell::get);
    let mut acc = [0u32; 64];
    for (i, a) in acc.iter_mut().enumerate() {
        *a = i as u32;
    }
    let after = ALLOCS.with(Cell::get);
    assert_eq!(
        before, after,
        "stack-only work must not count as allocation"
    );
    assert_eq!(acc[63], 63);
}
