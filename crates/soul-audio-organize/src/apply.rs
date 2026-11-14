//! Execution: the one mutating layer. [`apply`] walks an
//! [`OrganizePlan`](crate::OrganizePlan), copying or moving each file into
//! place, reporting [`Progress`] per entry and tallying a [`Summary`]. It is
//! **idempotent** — a destination that already exists is skipped, never
//! overwritten — so re-running a plan is safe, and a per-file error never aborts
//! the batch (it is recorded in [`Summary::failed`] and the walk continues).

use std::path::PathBuf;

use crate::{Action, OrganizePlan, PlanEntry};

/// Progress for one processed entry, delivered to the `on_progress` callback
/// **after** that entry is handled (so `done` counts entries already finished).
#[derive(Clone, Debug)]
pub struct Progress {
    /// Entries finished so far, `1..=total`.
    pub done: usize,
    /// Total entries in the plan.
    pub total: usize,
    /// The source path of the entry just processed.
    pub current: PathBuf,
}

/// Outcome tally of an [`apply`] run. `copied + moved + skipped + failed.len()`
/// equals the number of plan entries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Entries copied into place.
    pub copied: usize,
    /// Entries moved into place.
    pub moved: usize,
    /// Entries skipped — a planned skip (duplicate/conflict) or an
    /// already-present destination encountered at apply time (idempotency).
    pub skipped: usize,
    /// Per-entry failures as `(source, error message)`; these never abort the run.
    pub failed: Vec<(PathBuf, String)>,
}

/// Execute `plan` — the only call in this crate that mutates the filesystem.
///
/// Entries are processed in order. For each, `on_progress` is invoked **after**
/// the entry is handled, with `done` incremented (final call has `done == total`)
/// and `current` set to the entry's source. Returns the accumulated [`Summary`].
///
/// # Idempotency & safety
/// A destination that already exists is left untouched and counted as skipped, so
/// applying the same plan twice is a no-op the second time. A per-entry IO error
/// is recorded in [`Summary::failed`] and the batch continues. The outer
/// `io::Result` is reserved for a catastrophic failure; in practice this always
/// returns `Ok(summary)`.
pub fn apply(
    plan: &OrganizePlan,
    mut on_progress: impl FnMut(Progress),
) -> std::io::Result<Summary> {
    let total = plan.entries.len();
    let mut summary = Summary::default();
    for (i, entry) in plan.entries.iter().enumerate() {
        apply_one(entry, &mut summary);
        on_progress(Progress {
            done: i + 1,
            total,
            current: entry.src.clone(),
        });
    }
    Ok(summary)
}

/// Apply a single entry, folding its outcome into `summary`.
fn apply_one(entry: &PlanEntry, summary: &mut Summary) {
    match entry.action {
        Action::Copy => do_copy(entry, summary),
        Action::Move => do_move(entry, summary),
        Action::SkipDuplicate | Action::SkipConflict => summary.skipped += 1,
    }
}

/// Ensure `dst`'s parent exists; on failure record it and signal the caller to
/// stop processing this entry. Returns `true` when it is safe to proceed.
fn ensure_parent(entry: &PlanEntry, summary: &mut Summary) -> bool {
    if let Some(parent) = entry.dst.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        summary.failed.push((entry.src.clone(), e.to_string()));
        return false;
    }
    true
}

fn do_copy(entry: &PlanEntry, summary: &mut Summary) {
    if !ensure_parent(entry, summary) {
        return;
    }
    if entry.dst.exists() {
        // Idempotent: never overwrite an existing destination.
        summary.skipped += 1;
        return;
    }
    match std::fs::copy(&entry.src, &entry.dst) {
        Ok(_) => summary.copied += 1,
        Err(e) => summary.failed.push((entry.src.clone(), e.to_string())),
    }
}

fn do_move(entry: &PlanEntry, summary: &mut Summary) {
    if !ensure_parent(entry, summary) {
        return;
    }
    if entry.dst.exists() {
        summary.skipped += 1;
        return;
    }
    match std::fs::rename(&entry.src, &entry.dst) {
        Ok(()) => summary.moved += 1,
        // Source genuinely missing: a real failure, not a cross-device rename.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            summary.failed.push((entry.src.clone(), e.to_string()));
        }
        // Cross-device (or other) rename failure: fall back to copy + remove.
        Err(_) => {
            match std::fs::copy(&entry.src, &entry.dst)
                .and_then(|_| std::fs::remove_file(&entry.src))
            {
                Ok(()) => summary.moved += 1,
                Err(e) => summary.failed.push((entry.src.clone(), e.to_string())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Progress, apply};
    use crate::{Strategy, Template, plan, presets};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    fn testdata(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata")
            .join(name)
    }

    fn unique_dir(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("organize-test-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn artist_album() -> Template {
        Template::new(presets()[0].1)
    }

    fn join(root: &Path, parts: &[&str]) -> PathBuf {
        let mut p = root.to_path_buf();
        for s in parts {
            p.push(s);
        }
        p
    }

    /// Copy a tagged + an untagged fixture into `src`, returning the two paths.
    fn two_fixtures(src: &Path) -> (PathBuf, PathBuf) {
        let a = src.join("a.flac");
        let b = src.join("b.flac");
        std::fs::copy(testdata("tagged.flac"), &a).unwrap();
        std::fs::copy(testdata("arted.flac"), &b).unwrap();
        (a, b)
    }

    #[test]
    fn copy_plan_places_files_and_keeps_originals() {
        let src = unique_dir("src-copy");
        let dest = unique_dir("dest-copy");
        let (a, b) = two_fixtures(&src);

        let p = plan(
            &[a.clone(), b.clone()],
            &dest,
            &artist_album(),
            Strategy::Copy,
        );
        let summary = apply(&p, |_| {}).unwrap();

        assert_eq!(summary.copied, 2);
        assert_eq!(summary.moved, 0);
        assert_eq!(summary.skipped, 0);
        assert!(summary.failed.is_empty());
        // Destinations exist under {AlbumArtist}/{Album}/..
        assert!(
            join(
                &dest,
                &["Test Artist", "Test Album", "00 - Soul Test Track.flac"]
            )
            .exists()
        );
        assert!(
            join(
                &dest,
                &["Unknown Artist", "Unknown Album", "00 - Arted Track.flac"]
            )
            .exists()
        );
        // Originals untouched.
        assert!(a.exists());
        assert!(b.exists());

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn reapplying_a_copy_plan_is_idempotent_all_skipped() {
        let src = unique_dir("src-idem");
        let dest = unique_dir("dest-idem");
        let (a, b) = two_fixtures(&src);

        let p = plan(&[a, b], &dest, &artist_album(), Strategy::Copy);
        let first = apply(&p, |_| {}).unwrap();
        assert_eq!(first.copied, 2);

        // Second application of the SAME plan: nothing new copied, all skipped.
        let second = apply(&p, |_| {}).unwrap();
        assert_eq!(second.copied, 0);
        assert_eq!(second.moved, 0);
        assert_eq!(second.skipped, 2);
        assert!(second.failed.is_empty());

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn move_plan_places_files_and_removes_originals() {
        let src = unique_dir("src-move");
        let dest = unique_dir("dest-move");
        let (a, b) = two_fixtures(&src);

        let p = plan(
            &[a.clone(), b.clone()],
            &dest,
            &artist_album(),
            Strategy::Move,
        );
        let summary = apply(&p, |_| {}).unwrap();

        assert_eq!(summary.moved, 2);
        assert_eq!(summary.copied, 0);
        assert!(summary.failed.is_empty());
        assert!(
            join(
                &dest,
                &["Test Artist", "Test Album", "00 - Soul Test Track.flac"]
            )
            .exists()
        );
        assert!(
            join(
                &dest,
                &["Unknown Artist", "Unknown Album", "00 - Arted Track.flac"]
            )
            .exists()
        );
        // Originals are gone after a move.
        assert!(!a.exists());
        assert!(!b.exists());

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn progress_fires_once_per_entry_ending_at_total() {
        let src = unique_dir("src-prog");
        let dest = unique_dir("dest-prog");
        let (a, b) = two_fixtures(&src);

        let p = plan(
            &[a.clone(), b.clone()],
            &dest,
            &artist_album(),
            Strategy::Copy,
        );
        let mut events: Vec<Progress> = Vec::new();
        apply(&p, |ev| events.push(ev)).unwrap();

        assert_eq!(events.len(), p.entries.len());
        assert_eq!(events.last().unwrap().done, events.last().unwrap().total);
        assert_eq!(events[0].done, 1);
        assert_eq!(events[0].current, a);
        assert_eq!(events[1].done, 2);
        assert_eq!(events[1].current, b);

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn duplicate_in_batch_copies_once_and_skips_the_rest() {
        let src = unique_dir("src-batchdup");
        let dest = unique_dir("dest-batchdup");
        let a = src.join("a.flac");
        std::fs::copy(testdata("tagged.flac"), &a).unwrap();

        // Same file twice → plan = [Copy, SkipDuplicate].
        let p = plan(
            &[a.clone(), a.clone()],
            &dest,
            &artist_album(),
            Strategy::Copy,
        );
        let summary = apply(&p, |_| {}).unwrap();

        assert_eq!(summary.copied, 1);
        assert_eq!(summary.skipped, 1);
        assert!(summary.failed.is_empty());

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dest);
    }
}
