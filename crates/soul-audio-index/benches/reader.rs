//! `.soulidx` **reader** benchmark — the performance baseline and regression
//! gate for the no-alloc read path the DAP and desktop browse over. Follows the
//! repo's dependency-free bench style (no criterion): a hand-rolled,
//! auto-calibrating best-of-N timer reporting **ns/op** per read primitive, at
//! several catalog scales, so the O(1)/O(log n) design is visible in the numbers
//! (a flat ns/op as N grows = the seek-based format working as intended).
//!
//! Reads run over an in-memory `SliceSource`, isolating the *format's* CPU cost
//! (seek + fixed-record parse + string copy) from any SD/file I/O. On real media
//! the I/O dominates, and the whole point of the format is to need **few**
//! seeks — which this measures by timing the per-op work.
//!
//! Run: `cargo bench -p soul-audio-index --bench reader --features build`

use std::cell::Cell;
use std::hint::black_box;
use std::time::{Duration, Instant};

use soul_audio_catalog::{BrowseAxis, Catalog};
use soul_audio_core::SliceSource;
use soul_audio_index::IndexReader;
use soul_audio_index::build::{AlbumMeta, CatalogInput, FolderInput, TrackInput, write_index};

/// Each timed batch is calibrated to at least this wall time for a stable number.
const MIN_WALL: Duration = Duration::from_millis(250);
/// Timed batches per benchmark; the fastest is reported (system noise only adds
/// wall time, so the min is the least-contended, most representative run).
const REPEAT: usize = 7;

/// Time `f` (one read primitive, returning a `u64` to defeat dead-code
/// elimination) and print ns/op + ops/s. Auto-calibrates the batch size.
fn bench(label: &str, f: impl Fn() -> u64) {
    let _ = black_box(f()); // warm up

    let mut iters = 1024u64;
    loop {
        let t = Instant::now();
        let mut acc = 0u64;
        for _ in 0..iters {
            acc = acc.wrapping_add(f());
        }
        black_box(acc);
        if t.elapsed() >= MIN_WALL {
            break;
        }
        iters = iters.saturating_mul(2);
    }

    let mut best = Duration::MAX;
    for _ in 0..REPEAT {
        let t = Instant::now();
        let mut acc = 0u64;
        for _ in 0..iters {
            acc = acc.wrapping_add(f());
        }
        black_box(acc);
        best = best.min(t.elapsed());
    }
    let ns = best.as_nanos() as f64 / iters as f64;
    println!(
        "    {label:<26} {ns:>8.1} ns/op   {:>12.0} ops/s",
        1e9 / ns.max(1e-9)
    );
}

/// A deterministic strided id generator in `0..n` (prime stride → semi-random
/// access across the record table, so cache behaviour is realistic rather than
/// purely sequential).
fn strider(n: u32) -> impl Fn() -> u32 {
    let ctr = Cell::new(0u32);
    move || {
        let v = ctr.get();
        ctr.set((v.wrapping_add(2_654_435_761 % n.max(1))) % n.max(1));
        v % n.max(1)
    }
}

/// Synthesize a realistic catalog of `n` tracks: ~12 tracks/album, ~4
/// albums/artist, one folder per album. Names are generated, not read from disk.
fn synth(n: u32) -> CatalogInput {
    let albums = (n / 12).max(1);
    let artists = (albums / 4).max(1);
    let folders = albums; // one folder per album

    let artist_names = (0..artists).map(|a| format!("Artist {a:05}")).collect();
    let folder_inputs = (0..folders)
        .map(|f| FolderInput {
            path: format!("Music/Artist {:05}/Album {f:05}", f % artists),
            parent: soul_audio_catalog::NO_FOLDER,
        })
        .collect();
    let album_metas = (0..albums)
        .map(|a| AlbumMeta {
            title: format!("Album {a:05}"),
            album_artist: a % artists,
            year: 1960 + (a % 60) as u16,
            folder: a,
        })
        .collect();
    let tracks = (0..n)
        .map(|i| {
            let album = i / 12;
            TrackInput {
                title: format!("Track {i:06} - a representative title"),
                artist: album % artists,
                album,
                album_artist: album % artists,
                folder: album,
                track_no: (i % 12 + 1) as u16,
                disc_no: 1,
                duration_ms: 180_000 + (i % 120_000),
                sample_rate: 44_100,
                channels: 2,
                file_name: format!("{:02} - Track {i:06}.flac", i % 12 + 1),
            }
        })
        .collect();

    CatalogInput {
        tracks,
        artists: artist_names,
        albums: album_metas,
        folders: folder_inputs,
        playlists: Vec::new(),
    }
}

fn main() {
    println!("\n.soulidx reader — ns/op by catalog scale (best of {REPEAT}, in-memory source)\n");

    for &n in &[1_000u32, 10_000, 100_000] {
        let bytes = write_index(&synth(n));
        let reader = IndexReader::new(SliceSource::new(&bytes));
        let album_count = reader.album_count();
        let order_len = reader.order_len(BrowseAxis::Title);

        println!(
            "  N = {n:>7} tracks   index = {:.2} MiB   ({:.1} B/track)",
            bytes.len() as f64 / (1 << 20) as f64,
            bytes.len() as f64 / n as f64,
        );

        // O(1): direct record seek + fixed-width parse.
        let id = strider(n);
        bench("track(id)  [O(1) seek]", || {
            reader.track(id()).map(|t| u64::from(t.folder)).unwrap_or(0)
        });

        // Order-table read: one u32 at a computed offset.
        let pos = strider(order_len);
        bench("order(Title, i)", || {
            u64::from(reader.order(BrowseAxis::Title, pos()).unwrap_or(0))
        });

        // String heap copy into a caller buffer (the no-alloc resolve path).
        let id2 = strider(n);
        bench("resolve_str(title)", || {
            let mut buf = [0u8; 128];
            let t = reader.track(id2()).unwrap();
            reader.resolve_str(t.title, &mut buf).unwrap_or(0) as u64
        });

        // Album -> its tracks (grouping read).
        let alb = strider(album_count);
        bench("album_track(a, 0)", || {
            u64::from(reader.album_track(alb(), 0).unwrap_or(0))
        });

        // O(log n): binary-search the Title order for a target title, resolving
        // strings per probe — the "jump to letter / find" cost.
        let target = format!("Track {:06}", n / 2);
        bench("binary_search title [O(log n)]", || {
            let (mut lo, mut hi) = (0u32, order_len);
            let mut buf = [0u8; 128];
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let tid = reader.order(BrowseAxis::Title, mid).unwrap();
                let t = reader.track(tid).unwrap();
                let len = reader.resolve_str(t.title, &mut buf).unwrap();
                if &buf[..len.min(target.len())] < target.as_bytes() {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            u64::from(lo)
        });

        println!();
    }
}
