//! Incremental + parallel scan: change-detection, cache reuse, deletions,
//! determinism, and byte-identical bake vs the full `scan`.

use std::path::{Path, PathBuf};

use soul_audio_import::{ScanCache, ScanPhase, ScanProgress, bake, scan_incremental};
use soul_audio_index::build::write_index;

fn testdata(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(name)
}

/// A fresh, uniquely-named temp dir (per-test name → no cross-test collision).
fn fresh_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("import-incr-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn noop(_: ScanProgress) {}

#[test]
fn cold_then_warm_reuses_cache_and_extracts_nothing() {
    let root = fresh_dir("warm");
    std::fs::copy(testdata("tagged.flac"), root.join("a.flac")).unwrap();
    std::fs::copy(testdata("arted.flac"), root.join("b.flac")).unwrap();

    // Cold: empty cache → everything is "added".
    let cold = scan_incremental(&root, &ScanCache::default(), 4, &mut noop).unwrap();
    assert_eq!(cold.added.len(), 2, "cold scan sees both files as new");
    assert!(cold.changed.is_empty() && cold.removed.is_empty());
    assert_eq!(cold.cache.len(), 2);
    assert_eq!(cold.input.tracks.len(), 2);

    // Warm: same files untouched → nothing added/changed/removed. Since the ONLY
    // files extracted are `added ∪ changed`, empty both proves zero re-reads.
    let warm = scan_incremental(&root, &cold.cache, 4, &mut noop).unwrap();
    assert!(
        warm.added.is_empty() && warm.changed.is_empty() && warm.removed.is_empty(),
        "an unchanged library re-extracts nothing"
    );
    assert_eq!(
        warm.input.tracks.len(),
        2,
        "the library is still fully present"
    );
    assert_eq!(warm.cache.len(), 2);
}

#[test]
fn detects_added_changed_and_removed() {
    let root = fresh_dir("acr");
    std::fs::copy(testdata("tagged.flac"), root.join("a.flac")).unwrap();
    std::fs::copy(testdata("arted.flac"), root.join("b.flac")).unwrap();
    let base = scan_incremental(&root, &ScanCache::default(), 4, &mut noop).unwrap();

    // ADD a file.
    std::fs::copy(testdata("tagged.flac"), root.join("c.flac")).unwrap();
    let added = scan_incremental(&root, &base.cache, 4, &mut noop).unwrap();
    assert_eq!(added.added, vec![root.join("c.flac")]);
    assert!(added.changed.is_empty() && added.removed.is_empty());
    assert_eq!(added.input.tracks.len(), 3);

    // CHANGE a file (overwrite a.flac with a differently-sized fixture → stamp differs).
    std::fs::copy(testdata("complex.flac"), root.join("a.flac")).unwrap();
    let changed = scan_incremental(&root, &added.cache, 4, &mut noop).unwrap();
    assert_eq!(changed.changed, vec![root.join("a.flac")]);
    assert!(changed.added.is_empty() && changed.removed.is_empty());
    assert_eq!(changed.input.tracks.len(), 3);

    // REMOVE a file.
    std::fs::remove_file(root.join("b.flac")).unwrap();
    let removed = scan_incremental(&root, &changed.cache, 4, &mut noop).unwrap();
    assert_eq!(removed.removed, vec![root.join("b.flac")]);
    assert!(removed.added.is_empty() && removed.changed.is_empty());
    assert_eq!(removed.input.tracks.len(), 2);
}

#[test]
fn incremental_bakes_identical_index_to_full_scan() {
    let root = fresh_dir("identical");
    std::fs::create_dir_all(root.join("album")).unwrap();
    std::fs::copy(testdata("tagged.flac"), root.join("album/01.flac")).unwrap();
    std::fs::copy(testdata("arted.flac"), root.join("album/02.flac")).unwrap();
    std::fs::copy(testdata("complex.flac"), root.join("03.flac")).unwrap();

    let incremental = scan_incremental(&root, &ScanCache::default(), 8, &mut noop).unwrap();
    let from_incremental = write_index(&incremental.input);
    let from_full = bake(&root).unwrap();
    assert_eq!(
        from_incremental, from_full,
        "the incremental path must bake a byte-identical .soulidx to the full scan"
    );
}

#[test]
fn thread_count_does_not_change_the_result() {
    let root = fresh_dir("threads");
    for i in 0..6 {
        std::fs::copy(testdata("tagged.flac"), root.join(format!("{i:02}.flac"))).unwrap();
    }
    let one = scan_incremental(&root, &ScanCache::default(), 1, &mut noop).unwrap();
    let many = scan_incremental(&root, &ScanCache::default(), 16, &mut noop).unwrap();
    assert_eq!(
        write_index(&one.input),
        write_index(&many.input),
        "parallel extraction must be deterministic (results placed in walk order)"
    );
}

#[test]
fn progress_reports_discovering_then_extracting() {
    let root = fresh_dir("progress");
    std::fs::copy(testdata("tagged.flac"), root.join("a.flac")).unwrap();
    std::fs::copy(testdata("arted.flac"), root.join("b.flac")).unwrap();

    let mut events: Vec<ScanProgress> = Vec::new();
    scan_incremental(&root, &ScanCache::default(), 4, &mut |p| events.push(p)).unwrap();

    let first_extract = events
        .iter()
        .position(|e| e.phase == ScanPhase::Extracting)
        .expect("an Extracting phase is reported");
    assert!(
        events[..first_extract]
            .iter()
            .all(|e| e.phase == ScanPhase::Discovering),
        "Discovering is reported before Extracting"
    );
    let last = events.last().unwrap();
    assert_eq!(last.phase, ScanPhase::Extracting);
    assert_eq!(
        last.done, last.total,
        "extraction progress ends at done == total"
    );
    assert_eq!(last.total, 2, "both new files are extracted");
}
