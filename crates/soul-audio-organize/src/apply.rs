//! Execution: the one mutating layer. [`apply`] walks an
//! [`OrganizePlan`](crate::OrganizePlan), copying or moving each file into
//! place — a rewritten `.cue` ([`PlanEntry::rewritten`](crate::PlanEntry::rewritten))
//! is written from the plan's bytes instead, its source removed only after the
//! write on a Move — reporting [`Progress`] per entry and tallying a
//! [`Summary`]. It is
//! **idempotent** — a destination that already exists is skipped, never
//! overwritten — so re-running a plan is safe, and a per-file error never aborts
//! the batch (it is recorded in [`Summary::failed`] and the walk continues).
//! After the entries it removes the plan's
//! [`prune_dirs`](crate::OrganizePlan::prune_dirs) (a Move plan's) that are now
//! empty (bottom-up; a non-empty folder is never touched) — also on a resumed
//! re-apply that had nothing left to move.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cue::SourceStamp;
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
    /// Source folders removed after a Move because they were left empty
    /// ([`OrganizePlan::prune_dirs`](crate::OrganizePlan::prune_dirs)). Not
    /// part of the per-entry tally.
    pub pruned_dirs: usize,
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
    // Move cleanup: `remove_dir` only ever removes an EMPTY folder, so a folder
    // still holding a file (junk, an ambiguous album's side files, an
    // unplanned file) survives. Errors are expected and ignored. Not gated on
    // this run having moved anything: `prune_dirs` is only ever set for a Move
    // plan, and a resumed re-apply (every move already done by an interrupted
    // run) must still clean up.
    for dir in &plan.prune_dirs {
        if std::fs::remove_dir(dir).is_ok() {
            summary.pruned_dirs += 1;
        }
    }
    Ok(summary)
}

/// Apply a single entry, folding its outcome into `summary`.
fn apply_one(entry: &PlanEntry, summary: &mut Summary) {
    match (entry.action, &entry.rewritten) {
        (Action::Copy | Action::Move, Some(bytes)) => do_write(entry, bytes, summary),
        (Action::Copy, None) => do_copy(entry, summary),
        (Action::Move, None) => do_move(entry, summary),
        (Action::SkipDuplicate | Action::SkipConflict, _) => summary.skipped += 1,
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

/// A rewritten cue ([`PlanEntry::rewritten`]): write `bytes` at `dst` — never
/// over an existing file — and, for a Move, remove the source only once the
/// write succeeded.
///
/// The source is re-read first and must still match what was planned (its
/// stamp: length + content hash): a cue edited, replaced or removed since
/// planning is a failure — nothing is written, nothing removed — so an edit
/// is never lost. A hand-built entry has no stamp: a Copy writes, a Move
/// fails (it would delete a source nobody verified). A failed write leaves
/// the source untouched (and no partial destination); a failed removal
/// leaves both files and is recorded.
///
/// An existing `dst` is skipped — except on a Move whose `dst` already holds
/// exactly `bytes` while the source is unchanged (a run stopped between the
/// write and the removal): the removal is finished and the entry counts as
/// moved.
fn do_write(entry: &PlanEntry, bytes: &[u8], summary: &mut Summary) {
    if !ensure_parent(entry, summary) {
        return;
    }
    let is_move = entry.action == Action::Move;
    if entry.dst.exists() {
        if is_move && std::fs::read(&entry.dst).is_ok_and(|d| d == bytes) && source_matches(entry) {
            remove_source(entry, summary);
        } else {
            summary.skipped += 1;
        }
        return;
    }
    match entry.source_stamp {
        Some(_) if !source_matches(entry) => {
            summary.failed.push((
                entry.src.clone(),
                "the cue changed or vanished since it was planned; re-plan".to_owned(),
            ));
            return;
        }
        None if is_move => {
            summary.failed.push((
                entry.src.clone(),
                "a rewritten cue without a plan-time source check is never moved".to_owned(),
            ));
            return;
        }
        _ => {}
    }
    match write_new(&entry.dst, bytes) {
        Ok(()) => {}
        // Created by someone else since the check: still never overwritten.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            summary.skipped += 1;
            return;
        }
        Err(e) => {
            summary.failed.push((entry.src.clone(), e.to_string()));
            return;
        }
    }
    if is_move {
        remove_source(entry, summary);
    } else {
        summary.copied += 1;
    }
}

/// Whether `entry`'s source still matches its plan-time stamp (`false`
/// without one, or when it cannot be read).
fn source_matches(entry: &PlanEntry) -> bool {
    let Some(stamp) = entry.source_stamp else {
        return false;
    };
    // Cheap length check first: a grown file is never read whole.
    std::fs::metadata(&entry.src).is_ok_and(|m| m.is_file() && m.len() == stamp.len())
        && std::fs::read(&entry.src).is_ok_and(|b| SourceStamp::of(&b) == stamp)
}

/// The rewritten cue is in place: remove the (verified) source of a Move.
fn remove_source(entry: &PlanEntry, summary: &mut Summary) {
    match std::fs::remove_file(&entry.src) {
        Ok(()) => summary.moved += 1,
        Err(e) => summary.failed.push((
            entry.src.clone(),
            format!(
                "wrote {} but could not remove the source: {e}",
                entry.dst.display()
            ),
        )),
    }
}

/// Create `dst` (failing if it exists) holding exactly `bytes`; on error the
/// partial file is removed.
fn write_new(dst: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;
    let res = f.write_all(bytes).and_then(|()| f.sync_all());
    if res.is_err() {
        drop(f);
        let _ = std::fs::remove_file(dst);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::{Progress, apply};
    use crate::cue::SourceStamp;
    use crate::{Action, EntryKind, OrganizePlan, PlanEntry, Strategy, Template, plan, presets};
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
        // `module_path!()` is part of the name on purpose. `plan::tests` and
        // `apply::tests` each have their OWN `static N`, but they live in the
        // same test binary (same pid) and both use the tags `src-move` and
        // `dest-move` — so without a per-module discriminator their independent
        // counters can mint the IDENTICAL path. Run in parallel, one test then
        // populates the other's destination directory, and the planner correctly
        // reports `SkipDuplicate` where the test expects `Move` (or an apply
        // test finds its originals already moved). Intermittent, because it
        // depends on the order the two counters happen to be advanced in.
        let dir = std::env::temp_dir().join(format!(
            "organize-test-{}-{}-{tag}-{n}",
            std::process::id(),
            module_path!().replace("::", "-")
        ));
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

    /// A one-entry plan writing `bytes` (a rewritten cue) for `src` → `dst`,
    /// planned from a source holding `OLD` (as the planner stamps it).
    fn cue_plan(src: &Path, dst: &Path, action: Action, bytes: &[u8]) -> OrganizePlan {
        let mut p = hand_cue_plan(src, dst, action, bytes);
        p.entries[0].source_stamp = Some(SourceStamp::of(OLD));
        p
    }

    /// [`cue_plan`] without a source stamp (a hand-built entry).
    fn hand_cue_plan(src: &Path, dst: &Path, action: Action, bytes: &[u8]) -> OrganizePlan {
        let mut e = PlanEntry::new(src, dst, action, EntryKind::SideFile);
        e.rewritten = Some(bytes.to_vec());
        OrganizePlan {
            entries: vec![e],
            ..Default::default()
        }
    }

    const OLD: &[u8] = b"FILE \"a.flac\" WAVE\r\n";
    const NEW: &[u8] = b"FILE \"01 - A.flac\" WAVE\r\n";

    #[test]
    fn a_rewritten_cue_copy_writes_the_new_bytes_and_keeps_the_source() {
        let dir = unique_dir("cue-copy");
        let src = dir.join("a.cue");
        std::fs::write(&src, OLD).unwrap();
        let dst = join(&dir, &["out", "a.cue"]);
        let s = apply(&cue_plan(&src, &dst, Action::Copy, NEW), |_| {}).unwrap();
        assert_eq!((s.copied, s.moved, s.skipped), (1, 0, 0));
        assert!(s.failed.is_empty(), "{:?}", s.failed);
        assert_eq!(std::fs::read(&dst).unwrap(), NEW);
        assert_eq!(std::fs::read(&src).unwrap(), OLD);
        // A hand-built Copy (no stamp) writes too: the source is never touched.
        let dst2 = join(&dir, &["out", "b.cue"]);
        let s = apply(&hand_cue_plan(&src, &dst2, Action::Copy, NEW), |_| {}).unwrap();
        assert_eq!((s.copied, s.failed.len()), (1, 0), "{:?}", s.failed);
        assert_eq!(std::fs::read(&dst2).unwrap(), NEW);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rewritten_cue_move_writes_then_removes_the_source() {
        let dir = unique_dir("cue-move");
        let src = dir.join("a.cue");
        std::fs::write(&src, OLD).unwrap();
        let dst = join(&dir, &["out", "a.cue"]);
        let p = cue_plan(&src, &dst, Action::Move, NEW);
        let s = apply(&p, |_| {}).unwrap();
        assert_eq!((s.copied, s.moved, s.skipped), (0, 1, 0));
        assert_eq!(std::fs::read(&dst).unwrap(), NEW);
        assert!(!src.exists());
        // Re-applying the same plan is a no-op.
        let again = apply(&p, |_| {}).unwrap();
        assert_eq!((again.moved, again.skipped), (0, 1));
        assert!(again.failed.is_empty(), "{:?}", again.failed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rewritten_cue_never_overwrites() {
        let dir = unique_dir("cue-exists");
        let src = dir.join("a.cue");
        std::fs::write(&src, OLD).unwrap();
        let dst = dir.join("b.cue");
        std::fs::write(&dst, b"someone else's").unwrap();
        let s = apply(&cue_plan(&src, &dst, Action::Move, NEW), |_| {}).unwrap();
        assert_eq!((s.moved, s.skipped), (0, 1));
        assert_eq!(std::fs::read(&dst).unwrap(), b"someone else's");
        assert_eq!(std::fs::read(&src).unwrap(), OLD, "the source stays");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_cue_write_keeps_the_source() {
        let dir = unique_dir("cue-fail");
        let src = dir.join("a.cue");
        std::fs::write(&src, OLD).unwrap();
        // The destination's parent is a FILE: the write cannot happen.
        std::fs::write(dir.join("blocker"), b"x").unwrap();
        let dst = join(&dir, &["blocker", "a.cue"]);
        let s = apply(&cue_plan(&src, &dst, Action::Move, NEW), |_| {}).unwrap();
        assert_eq!((s.moved, s.failed.len()), (0, 1));
        assert_eq!(std::fs::read(&src).unwrap(), OLD, "never both missing");
        assert!(!dst.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cue_edited_after_planning_is_never_moved() {
        // The user edits the cue between plan and apply: writing the planned
        // bytes and deleting the source would lose the edit.
        let dir = unique_dir("cue-edited");
        let src = dir.join("a.cue");
        std::fs::write(&src, OLD).unwrap();
        let dst = join(&dir, &["out", "a.cue"]);
        let p = cue_plan(&src, &dst, Action::Move, NEW);
        let edited: &[u8] = b"REM EDITED
FILE \"a.flac\" WAVE
";
        std::fs::write(&src, edited).unwrap();
        let s = apply(&p, |_| {}).unwrap();
        assert_eq!((s.moved, s.copied, s.skipped), (0, 0, 0));
        assert_eq!(s.failed.len(), 1, "{:?}", s.failed);
        assert_eq!(std::fs::read(&src).unwrap(), edited, "the edit survives");
        assert!(!dst.exists(), "nothing written");
        // Same length, different bytes: still caught (content hash).
        let same_len: Vec<u8> = OLD.iter().map(|b| b.to_ascii_lowercase()).collect();
        assert_eq!(same_len.len(), OLD.len());
        std::fs::write(&src, &same_len).unwrap();
        let s = apply(&p, |_| {}).unwrap();
        assert_eq!((s.moved, s.failed.len()), (0, 1), "{:?}", s.failed);
        assert_eq!(std::fs::read(&src).unwrap(), same_len);
        assert!(!dst.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_vanished_cue_source_writes_nothing() {
        for action in [Action::Move, Action::Copy] {
            let dir = unique_dir("cue-vanished");
            let src = dir.join("a.cue");
            let dst = join(&dir, &["out", "a.cue"]);
            // Planned from OLD, gone by apply time.
            let s = apply(&cue_plan(&src, &dst, action, NEW), |_| {}).unwrap();
            assert_eq!((s.moved, s.copied, s.failed.len()), (0, 0, 1), "{action:?}");
            assert!(!dst.exists(), "{action:?}: nothing written");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_hand_built_rewritten_move_never_deletes_the_source() {
        // No stamp: apply cannot tell whether the source changed since the
        // bytes were made, so a Move fails safe.
        let dir = unique_dir("cue-hand");
        let src = dir.join("a.cue");
        std::fs::write(&src, OLD).unwrap();
        let dst = join(&dir, &["out", "a.cue"]);
        let s = apply(&hand_cue_plan(&src, &dst, Action::Move, NEW), |_| {}).unwrap();
        assert_eq!((s.moved, s.failed.len()), (0, 1), "{:?}", s.failed);
        assert_eq!(std::fs::read(&src).unwrap(), OLD);
        assert!(!dst.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_interrupted_cue_move_is_completed_on_reapply() {
        // The rewritten cue was written but the source was not removed (the
        // run stopped in between): re-applying finishes the Move.
        let dir = unique_dir("cue-complete");
        let src = dir.join("a.cue");
        std::fs::write(&src, OLD).unwrap();
        let dst = join(&dir, &["out", "a.cue"]);
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::fs::write(&dst, NEW).unwrap();
        let s = apply(&cue_plan(&src, &dst, Action::Move, NEW), |_| {}).unwrap();
        assert!(s.failed.is_empty(), "{:?}", s.failed);
        assert_eq!((s.moved, s.skipped), (1, 0));
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst).unwrap(), NEW);

        // ...but never when the source was edited, or dst is someone else's.
        for (src_bytes, dst_bytes) in [(&b"edited"[..], NEW), (OLD, &b"other"[..])] {
            std::fs::write(&src, src_bytes).unwrap();
            std::fs::write(&dst, dst_bytes).unwrap();
            let s = apply(&cue_plan(&src, &dst, Action::Move, NEW), |_| {}).unwrap();
            assert_eq!((s.moved, s.skipped), (0, 1), "{:?}", s.failed);
            assert_eq!(std::fs::read(&src).unwrap(), src_bytes, "source kept");
            assert_eq!(std::fs::read(&dst).unwrap(), dst_bytes, "dst untouched");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
