//! End-to-end `plan` + `apply` roundtrip on real, committed audio fixtures.
//!
//! The expected destination is **derived from the files' actual tags** (read
//! here via `soul_audio_tags::read_owned`, exactly as the organizer does
//! internally) and `Template::render` — nothing about the tag values is
//! hard-coded, so the test stays honest if a fixture's tags ever change.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use soul_audio_organize::{Strategy, Template, apply, plan, presets};
use soul_audio_tags::Tags;

/// A committed fixture under the repo's shared `testdata/` dir.
fn testdata(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(name)
}

/// A fresh, unique temp dir (manual, per repo convention — no `tempfile`).
fn unique_dir(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("organize-test-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Read a file's owned tags the same way the organizer does — used to *derive*
/// the expected destination rather than hard-coding it.
fn read_tags(path: &Path) -> Tags {
    let f = std::fs::File::open(path).unwrap();
    let mut src = embedded_io_adapters::std::FromStd::new(f);
    soul_audio_tags::read_owned(&mut src)
        .map(|(t, _facts)| t)
        .unwrap_or_default()
}

#[test]
fn plan_then_apply_places_fixtures_and_reapply_is_all_skipped() {
    let src = unique_dir("rt-src");
    let dest = unique_dir("rt-dest");

    // Two committed fixtures: one fully tagged, one tagged for title only (it
    // exercises the Unknown-Artist/Unknown-Album template fallbacks).
    let a = src.join("first.flac");
    let b = src.join("second.flac");
    std::fs::copy(testdata("tagged.flac"), &a).unwrap();
    std::fs::copy(testdata("arted.flac"), &b).unwrap();
    let files = vec![a.clone(), b.clone()];

    // Derive the expected destinations from the REAL tags + the same preset.
    let template = Template::new(presets()[0].1); // {AlbumArtist}/{Album}/{TrackNo} - {Title}
    let expected_a = dest.join(template.render(&read_tags(&a), &a));
    let expected_b = dest.join(template.render(&read_tags(&b), &b));
    assert_ne!(expected_a, expected_b, "fixtures must map to distinct dsts");

    // Plan is pure: it classifies but writes nothing.
    let p = plan(&files, &dest, &template, Strategy::Copy);
    assert_eq!(p.dest_root, dest);
    assert_eq!(p.entries.len(), 2);
    assert_eq!(p.entries[0].dst, expected_a);
    assert_eq!(p.entries[1].dst, expected_b);
    assert!(!expected_a.exists(), "plan() must not create files");
    assert!(!expected_b.exists(), "plan() must not create files");

    // Apply: the real mutation.
    let summary = apply(&p, |_| {}).unwrap();
    assert_eq!(summary.copied, 2, "both fixtures copied");
    assert_eq!(summary.moved, 0);
    assert_eq!(summary.skipped, 0);
    assert!(
        summary.failed.is_empty(),
        "no failures: {:?}",
        summary.failed
    );

    // The expected tree now exists on disk.
    assert!(expected_a.exists());
    assert!(expected_b.exists());

    // Structural check: each dst is a 3-level `artist/album/file` relative path
    // under the destination root (derived straight from the rendered template).
    let rel_a = expected_a.strip_prefix(&dest).unwrap();
    assert_eq!(
        rel_a.components().count(),
        3,
        "artist/album/file: {rel_a:?}"
    );

    // Re-applying the SAME plan is idempotent — nothing new copied, all skipped.
    let again = apply(&p, |_| {}).unwrap();
    assert_eq!(again.copied, 0);
    assert_eq!(again.skipped, 2);
    assert!(again.failed.is_empty());

    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&dest);
}
