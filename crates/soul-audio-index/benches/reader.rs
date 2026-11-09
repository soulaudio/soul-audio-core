//! `.soulidx` **reader** benchmark — the performance baseline + regression gate
//! for the no-alloc read path the DAP and desktop browse over. Dependency-free
//! (no criterion), matching soul-audio-core's `throughput` bench.
//!
//! Two parts:
//!
//! 1. **ns/op** — a hand-rolled best-of-N timer per read primitive, over an
//!    in-memory `SliceSource`. Measures the *format's CPU cost* (seek + fixed
//!    record parse + string copy), isolated from any I/O.
//!
//! 2. **DAP I/O model** — the number that actually decides whether browsing a
//!    big library feels "S+" on the device. A desktop RAM timing says nothing
//!    about an SD card on a Cortex-M7, where a random 512-byte read is ~10000x
//!    slower than RAM and *reads dominate*. So this wraps the source in a meter
//!    that counts **distinct 512-byte blocks touched** (the real SD read unit),
//!    models an LRU page cache, and runs **DAP-shaped workloads** (mount, render
//!    a 20-row screen, scroll, jump-to, open-a-track) at 100k/1M tracks —
//!    reporting cold block-reads and a modeled milliseconds-per-action.
//!
//! Run: `cargo bench -p soul-audio-index --bench reader --features build`

use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};
use std::hint::black_box;
use std::rc::Rc;
use std::time::{Duration, Instant};

use embedded_io::{ErrorType, Read, Seek, SeekFrom};
use soul_audio_catalog::{BrowseAxis, Catalog};
use soul_audio_core::SliceSource;
use soul_audio_index::IndexReader;
use soul_audio_index::build::{AlbumMeta, CatalogInput, FolderInput, TrackInput, write_index};

/// SD read granularity — the FS/card transfers at least a 512-byte block.
const BLOCK: u64 = 512;
/// Modeled latency of one cold (uncached) random block read on a decent microSD
/// over SDMMC. Sequential runs are cheaper, so this is a conservative upper
/// bound on the per-block cost.
const BLOCK_MS: f64 = 0.25;
/// A small page cache, in blocks (32 KiB) — the kind a constrained DAP can spare.
const CACHE_BLOCKS: usize = 64;

// ---------------------------------------------------------------------------
// Part 1 — ns/op CPU timing
// ---------------------------------------------------------------------------

const MIN_WALL: Duration = Duration::from_millis(250);
const REPEAT: usize = 7;

fn bench(label: &str, f: impl Fn() -> u64) {
    let _ = black_box(f());
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

fn strider(n: u32) -> impl Fn() -> u32 {
    let ctr = Cell::new(0u32);
    move || {
        let v = ctr.get();
        ctr.set((v.wrapping_add(2_654_435_761 % n.max(1))) % n.max(1));
        v % n.max(1)
    }
}

// ---------------------------------------------------------------------------
// Catalog synthesis
// ---------------------------------------------------------------------------

/// Synthesize a realistic catalog of `n` tracks: ~12 tracks/album, ~4
/// albums/artist, one folder per album. Generated, not read from disk.
fn synth(n: u32) -> CatalogInput {
    let albums = (n / 12).max(1);
    let artists = (albums / 4).max(1);

    let artist_names = (0..artists).map(|a| format!("Artist {a:05}")).collect();
    let folders = (0..albums)
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
        folders,
        playlists: Vec::new(),
        covers: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Part 2 — DAP I/O model: a block-counting, page-cached source
// ---------------------------------------------------------------------------

/// Shared counters + an LRU block cache. `cap == 0` means an unlimited cache, so
/// `misses` counts the *distinct* blocks an action touches (its cold SD reads);
/// `cap == K` models a real K-block page cache.
#[derive(Default)]
struct Meter {
    reads: Cell<u64>,
    misses: Cell<u64>,
    cap: Cell<usize>,
    lru: RefCell<VecDeque<u64>>,
    resident: RefCell<HashSet<u64>>,
}

impl Meter {
    fn reset(&self, cap: usize) {
        self.reads.set(0);
        self.misses.set(0);
        self.cap.set(cap);
        self.lru.borrow_mut().clear();
        self.resident.borrow_mut().clear();
    }

    fn touch(&self, off: u64, len: usize) {
        if len == 0 {
            return;
        }
        self.reads.set(self.reads.get() + 1);
        let cap = self.cap.get();
        let mut res = self.resident.borrow_mut();
        let mut lru = self.lru.borrow_mut();
        let (first, last) = (off / BLOCK, (off + len as u64 - 1) / BLOCK);
        for b in first..=last {
            if res.contains(&b) {
                if cap != 0 {
                    if let Some(i) = lru.iter().position(|&x| x == b) {
                        lru.remove(i);
                    }
                    lru.push_front(b);
                }
            } else {
                self.misses.set(self.misses.get() + 1);
                res.insert(b);
                if cap != 0 {
                    lru.push_front(b);
                    if lru.len() > cap {
                        if let Some(ev) = lru.pop_back() {
                            res.remove(&ev);
                        }
                    }
                }
            }
        }
    }
}

/// A `SeekSource` that forwards to an in-memory `SliceSource` while recording the
/// blocks each read touches — a stand-in for the DAP's SD card.
struct Metered<'a> {
    inner: SliceSource<'a>,
    pos: u64,
    m: Rc<Meter>,
}

impl<'a> Metered<'a> {
    fn new(bytes: &'a [u8], m: Rc<Meter>) -> Self {
        Self {
            inner: SliceSource::new(bytes),
            pos: 0,
            m,
        }
    }
}

impl<'a> ErrorType for Metered<'a> {
    type Error = <SliceSource<'a> as ErrorType>::Error;
}
impl Read for Metered<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        let n = self.inner.read(buf)?;
        self.m.touch(self.pos, n);
        self.pos += n as u64;
        Ok(n)
    }
}
impl Seek for Metered<'_> {
    fn seek(&mut self, pos: SeekFrom) -> Result<u64, Self::Error> {
        let p = self.inner.seek(pos)?;
        self.pos = p;
        Ok(p)
    }
}

type Reader<'a> = IndexReader<Metered<'a>>;

/// Read one on-screen row's displayed fields (title + artist) for position `i`.
fn read_row(r: &Reader, axis: BrowseAxis, i: u32) {
    let mut t = [0u8; 128];
    let mut a = [0u8; 128];
    let tid = r.order(axis, i).unwrap();
    let rec = r.track(tid).unwrap();
    let _ = r.resolve_str(rec.title, &mut t);
    let _ = r.artist_name(rec.artist, &mut a);
}

/// Render a screen of `rows` rows starting at `start`, in `axis` order.
fn render_screen(r: &Reader, axis: BrowseAxis, start: u32, rows: u32, len: u32) {
    for i in start..(start + rows).min(len) {
        read_row(r, axis, i);
    }
}

/// Everything needed to open a track for playback: its folder path + file name.
fn open_track(r: &Reader, tid: u32) {
    let mut p = [0u8; 256];
    let mut f = [0u8; 128];
    let rec = r.track(tid).unwrap();
    let _ = r.folder_path(rec.folder, &mut p);
    let _ = r.resolve_str(rec.file_name, &mut f);
}

/// Binary-search the Title order for `target`, resolving titles per probe.
fn jump_to(r: &Reader, len: u32, target: &str) -> u32 {
    let (mut lo, mut hi) = (0u32, len);
    let mut buf = [0u8; 128];
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let tid = r.order(BrowseAxis::Title, mid).unwrap();
        let rec = r.track(tid).unwrap();
        let k = r.resolve_str(rec.title, &mut buf).unwrap();
        if &buf[..k.min(target.len())] < target.as_bytes() {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

fn report(label: &str, m: &Meter, over: u64) {
    let misses = m.misses.get();
    let per = misses as f64 / over as f64;
    println!(
        "    {label:<34} {:>7} reads  {per:>8.1} cold blocks  {:>7.2} ms",
        m.reads.get(),
        per * BLOCK_MS,
    );
}

fn dap_io_model(n: u32) {
    const ROWS: u32 = 20; // a typical e-ink list screen
    let bytes = write_index(&synth(n));

    let m = Rc::new(Meter::default());
    m.reset(0);
    let r: Reader = IndexReader::new(Metered::new(&bytes, m.clone()));
    // Mount = open + read the header/counts the UI needs to show anything.
    let _ = r.track_count();
    let _ = r.album_count();
    let len = r.order_len(BrowseAxis::Album);

    println!(
        "  N = {n:>7} tracks   index = {:.1} MiB   (cache = {CACHE_BLOCKS} blocks / {} KiB, {BLOCK_MS} ms/block)",
        bytes.len() as f64 / (1 << 20) as f64,
        CACHE_BLOCKS * BLOCK as usize / 1024,
    );
    report("mount (open + header/counts)", &m, 1);

    m.reset(0);
    render_screen(&r, BrowseAxis::Album, 0, ROWS, len);
    report("render album screen (20 rows), cold", &m, 1);

    // Scroll 50 screens with the page cache warm — the real fast-scroll feel.
    m.reset(CACHE_BLOCKS);
    let screens = 50u32;
    for s in 0..screens {
        render_screen(&r, BrowseAxis::Album, s * ROWS, ROWS, len);
    }
    report(
        "scroll 50 screens (cached), per screen",
        &m,
        u64::from(screens),
    );

    m.reset(0);
    let _ = jump_to(&r, len, &format!("Track {:06}", n / 2));
    report("jump-to-track (binary search), cold", &m, 1);

    m.reset(0);
    let tid = r.order(BrowseAxis::Album, 0).unwrap();
    open_track(&r, tid);
    report("open track (locate for playback), cold", &m, 1);

    println!();
}

// ---------------------------------------------------------------------------

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

        let id = strider(n);
        bench("track(id)  [O(1) seek]", || {
            reader.track(id()).map(|t| u64::from(t.folder)).unwrap_or(0)
        });
        let pos = strider(order_len);
        bench("order(Title, i)", || {
            u64::from(reader.order(BrowseAxis::Title, pos()).unwrap_or(0))
        });
        let id2 = strider(n);
        bench("resolve_str(title)", || {
            let mut buf = [0u8; 128];
            let t = reader.track(id2()).unwrap();
            reader.resolve_str(t.title, &mut buf).unwrap_or(0) as u64
        });
        let alb = strider(album_count);
        bench("album_track(a, 0)", || {
            u64::from(reader.album_track(alb(), 0).unwrap_or(0))
        });
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

    println!("\n.soulidx reader — DAP I/O model (distinct 512B blocks = SD reads; modeled ms)\n");
    for &n in &[100_000u32, 1_000_000] {
        dap_io_model(n);
    }
}
