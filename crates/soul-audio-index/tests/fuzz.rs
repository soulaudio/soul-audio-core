//! Deterministic mutation fuzzer for the **`.soulidx` reader**.
//!
//! Invariant under test: every [`Catalog`]/[`ArtStore`] method on
//! [`IndexReader`] must **never panic** — for any index bytes and any id,
//! including ids far beyond the counts the header declares. The only allowed
//! outcomes are `Err(OutOfRange/Io/Malformed)` or a bounded `Ok`.
//!
//! Why this surface needs a gate: the index is a file **on the user's SD card**.
//! It is not a trusted internal artifact — a card is removable, a half-written
//! bake is a real event (power loss mid-sync), FAT corruption is a real event,
//! and anyone can write whatever they like to a card and put it in the player.
//! The reader is also the most arithmetic-dense code in the repo after the
//! codecs: every access is `offset + id × RECORD_SIZE` on `u32`s read straight
//! out of the header, which is precisely the shape that produced the overflow
//! panics in the decode and tag paths (#150, #151, #153). On the DAP a panic
//! here is a dead player, and unlike a bad audio file there is nothing to skip
//! past — the index is the whole library.
//!
//! The reader is deliberately tolerant by design (a bad header reads as an
//! *empty* catalog, `is_current()` tells that apart from an empty library), so
//! "returns nothing" is a correct answer to garbage. This fuzzer asserts the
//! part that is not optional: it must not crash, and it must not report a
//! written length longer than the buffer it was handed.
//!
//! Gated on `feature = "build"` so the seeds come from the **real baker** —
//! fuzzing hand-rolled bytes would mostly test the header rejection, not the
//! record/heap/order-table arithmetic behind it. `cargo xtask verify` and CI
//! both run this crate with that feature.
//!
//! No hang detection, unlike the decode/tag fuzzers: those parse *chains*
//! (Ogg page → page, EBML element → child) where a zero-size element can mean
//! "advance by nothing, forever". Every read here is a single bounded
//! seek-and-fill at a computed offset, with no loop that a record's contents
//! can extend — so a panic, not a spin, is the failure mode.
#![cfg(feature = "build")]

use std::panic::{AssertUnwindSafe, catch_unwind};

use soul_audio_catalog::{BrowseAxis, Catalog, NO_FOLDER, PackedDate};
use soul_audio_core::SliceSource;
use soul_audio_index::IndexReader;
use soul_audio_index::build::{AlbumMeta, CatalogInput, FolderInput, PlaylistInput, TrackInput};

/// Fixed seed for the deterministic CI run.
const CI_SEED: u64 = 0x5349_4458_0001;

/// CI iterations. Each one bakes nothing (seeds are baked once) but drives
/// ~40 reader calls over a mutated index, so this stays a second or two.
const CI_ITERS: u64 = 4_000;

/// Deterministic PRNG (xorshift64*) — the house pattern.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn byte(&mut self) -> u8 {
        (self.next() >> 33) as u8
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo)
    }
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok()?.trim().parse().ok()
}

// ---------------------------------------------------------------------------
// Seed corpus: real baked indexes.
// ---------------------------------------------------------------------------

fn track(title: &str, artist: u32, album: u32, folder: u32) -> TrackInput {
    TrackInput {
        title: title.into(),
        artist,
        album,
        album_artist: artist,
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

/// An album record for the fixture (every field named: `AlbumMeta` has no
/// `Default`, deliberately — a baked album without a title is a bug, not a
/// default).
fn album(title: &str, folder: u32) -> AlbumMeta {
    AlbumMeta {
        title: title.into(),
        album_artist: 0,
        original_date: PackedDate::from_year(1955),
        release_date: PackedDate::new(2015, 5, 28),
        disc_count: 1,
        mb_album_id: String::new(),
        folder,
    }
}

/// A small but structurally complete catalog: several tracks across albums and
/// folders, a playlist, a cue-sliced track, and the unicode/empty strings that
/// exercise the heap.
fn catalog() -> CatalogInput {
    let mut tracks = vec![
        track("Aria", 0, 0, 0),
        track("Variatio 1", 0, 0, 0),
        track("Blue in Green", 1, 1, 1),
        track("", 1, 1, 1), // empty title: a valid StrRef of len 0
        track("Kalinychta", 2, 2, 2),
    ];
    // Multi-byte UTF-8 in the heap, and a cue-sliced track whose (start, len)
    // the reader must hand back.
    tracks[4].title = "\u{39a}\u{3b1}\u{3bb}\u{3b7}\u{3bd}\u{3cd}\u{3c7}\u{3c4}\u{3b1}".into();
    tracks[1].cue_range = Some((44_100, 88_200));
    CatalogInput {
        tracks,
        artists: vec!["Bach".into(), "Miles Davis".into(), "\u{3a3}".into()],
        artist_sorts: vec!["Bach".into(), "Davis, Miles".into(), String::new()],
        genres: vec!["Classical".into(), "Jazz".into(), String::new()],
        albums: vec![
            album("Goldberg Variations", 0),
            album("Kind of Blue", 1),
            album("", 2),
        ],
        folders: vec![
            FolderInput {
                path: "Bach".into(),
                parent: NO_FOLDER,
            },
            FolderInput {
                path: "Jazz/Miles".into(),
                parent: NO_FOLDER,
            },
            FolderInput {
                path: "\u{3a3}".into(),
                parent: 1,
            },
        ],
        playlists: vec![PlaylistInput {
            name: "Favourites".into(),
            tracks: vec![0, 2, 4],
        }],
        covers: Vec::new(),
    }
}

/// The seeds: a full index, an empty one, and a single-track one — plus the
/// degenerate "almost a header" shapes a mutation can grow from.
fn seeds() -> Vec<(&'static str, Vec<u8>)> {
    let empty = CatalogInput {
        tracks: vec![],
        artists: vec![],
        artist_sorts: vec![],
        genres: vec![],
        albums: vec![],
        folders: vec![],
        playlists: vec![],
        covers: Vec::new(),
    };
    let one = CatalogInput {
        tracks: vec![track("Solo", 0, 0, 0)],
        artists: vec!["A".into()],
        artist_sorts: vec!["A".into()],
        genres: vec!["C".into()],
        albums: vec![album("B", 0)],
        folders: vec![FolderInput {
            path: "D".into(),
            parent: NO_FOLDER,
        }],
        playlists: vec![],
        covers: Vec::new(),
    };
    vec![
        ("full", soul_audio_index::build::write_index(&catalog())),
        ("empty", soul_audio_index::build::write_index(&empty)),
        ("one-track", soul_audio_index::build::write_index(&one)),
        ("magic-only", b"SOULIDX\0".to_vec()),
        ("nothing", Vec::new()),
    ]
}

// ---------------------------------------------------------------------------
// Mutations. The header is the high-value target: every record access is
// `offset + id × RECORD_SIZE` over `u32`s taken straight from it.
// ---------------------------------------------------------------------------

/// Overwrite little-endian `u32`/`u16` windows with hostile values. Biased to
/// the first 80 bytes (the header's counts and section offsets), because that
/// is what every subsequent offset computation is built from — a count of
/// `u32::MAX` invites `id × RECORD_SIZE` to overflow, and an offset near
/// `u32::MAX` invites the add to.
fn corrupt_header(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut v = data.to_vec();
    if v.len() < 8 {
        return v;
    }
    for _ in 0..rng.range(1, 6) {
        let width = if rng.below(4) == 0 { 2 } else { 4 };
        if v.len() <= width {
            break;
        }
        // 80% of the time inside the header, else anywhere.
        let at = if rng.below(5) < 4 {
            rng.below(80.min(v.len() - width))
        } else {
            rng.below(v.len() - width)
        };
        let val: u32 = match rng.below(7) {
            0 => u32::MAX,
            1 => 0,
            2 => u32::MAX - 1,
            3 => 0x7FFF_FFFF,
            4 => 1,
            5 => 0xFFFF_0000,
            _ => rng.next() as u32,
        };
        let b = val.to_le_bytes();
        v[at..at + width].copy_from_slice(&b[..width]);
    }
    v
}

fn bit_flips(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut v = data.to_vec();
    if v.is_empty() {
        return v;
    }
    for _ in 0..rng.range(1, 20) {
        let pos = rng.below(v.len());
        v[pos] ^= 1 << rng.below(8);
    }
    v
}

fn byte_subst(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut v = data.to_vec();
    if v.is_empty() {
        return v;
    }
    for _ in 0..rng.range(1, 20) {
        let pos = rng.below(v.len());
        v[pos] = rng.byte();
    }
    v
}

fn truncate_at(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }
    data[..rng.below(data.len() + 1)].to_vec()
}

fn zero_run(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut v = data.to_vec();
    if v.is_empty() {
        return v;
    }
    let start = rng.below(v.len());
    let len = rng.range(1, (v.len() - start).max(1) + 1);
    for b in &mut v[start..start + len] {
        *b = 0;
    }
    v
}

fn splice(rng: &mut Rng, a: &[u8], b: &[u8]) -> Vec<u8> {
    let cut_a = rng.below(a.len() + 1);
    let cut_b = rng.below(b.len() + 1);
    let mut v = a[..cut_a].to_vec();
    v.extend_from_slice(&b[cut_b..]);
    v
}

const STRATEGIES: &[&str] = &[
    "header-fields",
    "bit-flips",
    "byte-subst",
    "truncate",
    "zero-run",
    "splice",
    "random",
];

fn generate(rng: &mut Rng, corpus: &[(&'static str, Vec<u8>)]) -> (&'static str, Vec<u8>) {
    let s = rng.below(STRATEGIES.len());
    let (_, base) = &corpus[rng.below(corpus.len())];
    match s {
        0 => (STRATEGIES[0], corrupt_header(rng, base)),
        1 => (STRATEGIES[1], bit_flips(rng, base)),
        2 => (STRATEGIES[2], byte_subst(rng, base)),
        3 => (STRATEGIES[3], truncate_at(rng, base)),
        4 => (STRATEGIES[4], zero_run(rng, base)),
        5 => {
            let (_, other) = &corpus[rng.below(corpus.len())];
            (STRATEGIES[5], splice(rng, base, other))
        }
        _ => {
            let n = rng.below(512);
            (STRATEGIES[6], (0..n).map(|_| rng.byte()).collect())
        }
    }
}

// ---------------------------------------------------------------------------
// Driving the whole reader surface.
// ---------------------------------------------------------------------------

/// Ids to try for every accessor: the first few, the boundary around the
/// declared counts, and the extremes where `id × RECORD_SIZE` overflows.
fn ids(count: u32) -> [u32; 8] {
    [
        0,
        1,
        count.saturating_sub(1),
        count,
        count.saturating_add(1),
        u32::MAX / 99, // RECORD_SIZE: the id where the multiply reaches u32::MAX
        u32::MAX / 2,
        u32::MAX,
    ]
}

/// A string accessor must never claim to have written more than the buffer
/// holds — a caller slices `&buf[..n]`, so an over-long `n` is a panic (or
/// worse, a read of unrelated bytes) in *their* code, not ours.
fn check_len(n: Result<usize, soul_audio_catalog::CatalogError>, cap: usize, what: &str) {
    if let Ok(n) = n {
        assert!(n <= cap, "{what} reported {n} bytes written into {cap}");
    }
}

/// Everything the browse UI and the player do with an index.
fn drive(bytes: &[u8]) {
    let r = IndexReader::new(SliceSource::new(bytes));
    let _ = (r.format_version(), r.is_current());

    let tc = r.track_count();
    let ac = r.album_count();
    let arc = r.artist_count();
    let gc = r.genre_count();
    let fc = r.folder_count();
    let pc = r.playlist_count();

    let mut buf = [0u8; 64];
    let mut tiny = [0u8; 1];

    for &id in &ids(tc) {
        if let Ok(t) = r.track(id) {
            // Every field, including the derived cue accessors.
            let _ = (t.title, t.artist, t.album, t.folder, t.genre);
            let _ = (t.duration_ms, t.sample_rate, t.channels, t.art_id);
            let _ = (t.file_name, t.mbid, t.loudness, t.flags);
            let _ = (t.cue_range(), t.is_cue_track());
            check_len(r.resolve_str(t.title, &mut buf), buf.len(), "resolve_str");
            check_len(r.resolve_str(t.title, &mut tiny), tiny.len(), "resolve_str");
        }
    }
    for &id in &ids(ac) {
        if let Ok(a) = r.album(id) {
            let _ = (a.title, a.album_artist, a.folder, a.art_id);
            let _ = (a.original_date, a.release_date, a.id);
            check_len(r.resolve_str(a.title, &mut buf), buf.len(), "album title");
        }
        let n = r.album_track_count(id);
        for &i in &[0u32, 1, n, n.saturating_add(1), u32::MAX] {
            let _ = r.album_track(id, i);
        }
    }
    for &id in &ids(arc) {
        check_len(r.artist_name(id, &mut buf), buf.len(), "artist_name");
        check_len(r.artist_name(id, &mut tiny), tiny.len(), "artist_name tiny");
        check_len(r.artist_sort_name(id, &mut buf), buf.len(), "artist_sort");
    }
    for &id in &ids(gc) {
        check_len(r.genre_name(id, &mut buf), buf.len(), "genre_name");
    }
    for &id in &ids(fc) {
        if let Ok(f) = r.folder(id) {
            let _ = (f.path, f.parent);
        }
        check_len(r.folder_path(id, &mut buf), buf.len(), "folder_path");
    }
    for &id in &ids(pc) {
        check_len(r.playlist_name(id, &mut buf), buf.len(), "playlist_name");
        let n = r.playlist_len(id);
        for &i in &[0u32, 1, n, n.saturating_add(1), u32::MAX] {
            let _ = r.playlist_track(id, i);
        }
    }
    // Every browse axis, at the boundary of its own length.
    for axis in [
        BrowseAxis::Title,
        BrowseAxis::ArtistAlbumTrack,
        BrowseAxis::Album,
        BrowseAxis::Folder,
        BrowseAxis::Genre,
    ] {
        let len = r.order_len(axis);
        for &i in &[
            0u32,
            1,
            len.saturating_sub(1),
            len,
            len.saturating_add(1),
            u32::MAX,
        ] {
            let _ = r.order(axis, i);
        }
    }
    // The locator lookup a resume-from-disk does. (Cover art lives in a
    // separate blob read by `soul-audio-artbake`, not in this reader.)
    for key in ["", "Bach/Aria.flac", "../../etc/passwd", "\u{feff}", "Σ"] {
        let _ = r.resolve_locator(key);
    }
}

#[test]
fn no_index_bytes_can_panic_the_reader() {
    let seed = env_u64("IDX_FUZZ_SEED").unwrap_or(CI_SEED);
    let iters = env_u64("IDX_FUZZ_ITERS").unwrap_or(CI_ITERS);
    let corpus = seeds();
    let mut rng = Rng::new(seed);

    // The unmutated seeds first: a baked index must of course read.
    for (name, bytes) in &corpus {
        if catch_unwind(AssertUnwindSafe(|| drive(bytes))).is_err() {
            panic!("seed {name} panicked ({} bytes)", bytes.len());
        }
    }

    for i in 0..iters {
        let (strat, bytes) = generate(&mut rng, &corpus);
        if catch_unwind(AssertUnwindSafe(|| drive(&bytes))).is_err() {
            // Print the input so the failure is reproducible by hand, not only
            // by replaying the seed.
            let hex: String = bytes.iter().take(160).map(|b| format!("{b:02x}")).collect();
            panic!(
                "iteration {i} (seed {seed:#x}, strategy {strat}) panicked on \
                 {} bytes: {hex}…",
                bytes.len()
            );
        }
    }
}

/// A baked index still reads correctly through the same driver the fuzzer uses
/// — so a "no panics" pass cannot be satisfied by a reader that rejects
/// everything, which is the way a totality gate quietly stops testing anything.
#[test]
fn the_unmutated_seed_is_a_working_catalog() {
    let bytes = soul_audio_index::build::write_index(&catalog());
    let r = IndexReader::new(SliceSource::new(&bytes));
    assert!(r.is_current(), "baked index must read as current");
    assert_eq!(r.track_count(), 5);
    assert_eq!(r.album_count(), 3);
    assert_eq!(r.playlist_count(), 1);

    let mut buf = [0u8; 64];
    let t = r.track(0).expect("track 0");
    let n = r.resolve_str(t.title, &mut buf).expect("title");
    assert!(!buf[..n].is_empty(), "a baked title must resolve");

    // The cue-sliced track round-trips as a slice, which is what makes the
    // `cue_range()` branch in `drive` reachable at all.
    let sliced = (0..r.track_count())
        .filter_map(|i| r.track(i).ok())
        .find(|t| t.is_cue_track())
        .expect("one cue-sliced track in the fixture");
    assert_eq!(sliced.cue_range(), Some((44_100, 88_200)));
}

/// Same determinism guard the other fuzzers carry: a reported failure must be
/// replayable from its seed.
#[test]
fn generator_is_deterministic() {
    let corpus = seeds();
    let fingerprint = |seed: u64| -> Vec<(usize, u64)> {
        let mut rng = Rng::new(seed);
        (0..128)
            .map(|_| {
                let (_, b) = generate(&mut rng, &corpus);
                let sum = b
                    .iter()
                    .fold(0u64, |a, &x| a.wrapping_mul(31).wrapping_add(x.into()));
                (b.len(), sum)
            })
            .collect()
    };
    assert_eq!(fingerprint(CI_SEED), fingerprint(CI_SEED));
    assert_ne!(fingerprint(CI_SEED), fingerprint(CI_SEED ^ 0xABCD));
}
