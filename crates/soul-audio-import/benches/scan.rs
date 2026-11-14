//! Importer **scan** benchmark — the performance signal for the two S+ wins:
//! the **incremental** cache (a warm rescan skips all tag reads) and the
//! **parallel** extraction pool (the I/O-bound per-file reads fan out).
//! Dependency-free (no criterion), matching the repo's other benches.
//!
//! Two parts, both over a REAL temp library (the only honest way to measure a
//! filesystem scanner):
//!
//! 1. **Cold vs warm** — a full `scan_incremental` (empty cache, extracts every
//!    file) vs an immediate rescan with the returned cache (extracts nothing).
//!    The cold/warm ratio is the incremental win and is the robust headline
//!    number (warm = a directory walk + `stat`s, no file opens).
//!
//! 2. **Extraction scaling** — a cold scan at `threads ∈ {1,2,4,8,16}`, reporting
//!    wall-time and speedup vs 1 thread. NOTE: the fixtures were just written, so
//!    the OS page cache is warm — this measures parse + syscall parallelism and
//!    UNDERSTATES the real win, which comes from hiding *cold-disk* read latency
//!    (where the research shows small random reads scale ~up to 40x). Treat the
//!    ratios as a conservative lower bound.
//!
//! Run: `cargo bench -p soul-audio-import --bench scan`

use std::path::{Path, PathBuf};
use std::time::Instant;

use soul_audio_import::{ScanCache, ScanProgress, scan_incremental};

fn testdata(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(name)
}

/// Build a synthetic library: `n` copies of a ~73 KB tagged fixture spread across
/// `folders` album directories. Returns the root (caller removes it).
fn build_library(n: usize, folders: usize) -> PathBuf {
    let root = std::env::temp_dir().join(format!("import-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let src = testdata("complex.mp3");
    for i in 0..n {
        let dir = root.join(format!("album{:03}", i % folders));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(&src, dir.join(format!("{i:04}.mp3"))).unwrap();
    }
    root
}

fn noop(_: ScanProgress) {}

fn time_scan(root: &Path, cache: &ScanCache, threads: usize) -> (std::time::Duration, ScanCache) {
    let t = Instant::now();
    let out = scan_incremental(root, cache, threads, &mut noop).unwrap();
    (t.elapsed(), out.cache)
}

fn main() {
    const N: usize = 600;
    const FOLDERS: usize = 30;
    let threads_hw = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    println!("# importer scan benchmark — N={N} files, {FOLDERS} folders, {threads_hw} cores\n");

    let root = build_library(N, FOLDERS);

    // ── 1. Cold vs warm (incremental win) ────────────────────────────────────
    let (cold, cache) = time_scan(&root, &ScanCache::default(), threads_hw);
    let (warm, _) = time_scan(&root, &cache, threads_hw);
    println!("## incremental (cache) win");
    println!("  cold full scan : {:>8.2?}  ({N} files extracted)", cold);
    println!("  warm rescan    : {:>8.2?}  (0 files extracted)", warm);
    println!(
        "  → {:.0}x faster when nothing changed (walk + stat only)\n",
        cold.as_secs_f64() / warm.as_secs_f64().max(1e-9)
    );

    // ── 2. Extraction scaling (parallel win) ─────────────────────────────────
    println!("## extraction scaling (cold scan at N threads; page cache warm → lower bound)");
    let mut base = None;
    for &t in &[1usize, 2, 4, 8, 16] {
        // Fresh cold scan each time (empty cache → extract all N).
        let (dur, _) = time_scan(&root, &ScanCache::default(), t);
        let b = *base.get_or_insert(dur.as_secs_f64());
        println!(
            "  {:>2} threads : {:>8.2?}   {:.2}x vs 1",
            t,
            dur,
            b / dur.as_secs_f64().max(1e-9)
        );
    }

    let _ = std::fs::remove_dir_all(&root);
    println!("\n(absolute times are machine/disk-dependent; the ratios are the signal.)");
}
