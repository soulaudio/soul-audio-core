//! Planning: read each file's tags, compute its destination via the
//! [`Template`](crate::Template), and classify the [`Action`] — **without**
//! mutating the filesystem. `plan` only *reads* tags and *stats* any existing
//! destination; it never creates directories or copies bytes.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::tags::read_tags;
use crate::{Strategy, Template};

/// What [`apply`](crate::apply) will do with one source file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Copy the source to the destination, keeping the original.
    Copy,
    /// Move the source to the destination, removing the original.
    Move,
    /// Skip: the destination is already claimed by an **earlier** entry in this
    /// batch, or already present on disk at the **same byte length** (treated as
    /// already-organized — see the size heuristic on [`plan`]).
    SkipDuplicate,
    /// Skip: a **different** file already occupies the destination (same path,
    /// different byte length). We never overwrite.
    SkipConflict,
}

/// One planned source → destination placement with its classified [`Action`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanEntry {
    /// The source file to copy/move.
    pub src: PathBuf,
    /// Its computed destination (`dest_root` joined with the rendered template).
    pub dst: PathBuf,
    /// How [`apply`](crate::apply) will treat this entry.
    pub action: Action,
}

/// A computed, not-yet-executed organize plan: the destination root plus one
/// [`PlanEntry`] per input file, in input order.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct OrganizePlan {
    /// The root every entry's destination lives under.
    pub dest_root: PathBuf,
    /// One entry per input file, preserving input order.
    pub entries: Vec<PlanEntry>,
}

/// Compute an [`OrganizePlan`] for `files` against `dest_root`.
///
/// For each source (in input order) the destination is
/// `dest_root.join(template.render(tags, src))`, where `tags` are read per file
/// (an unreadable or untagged file degrades to
/// [`Tags::default`](soul_audio_tags::Tags), never a panic). Each entry's
/// [`Action`] is classified as:
///
/// - **[`SkipDuplicate`](Action::SkipDuplicate)** if an *earlier* entry in this
///   same batch already claims the identical destination, **or** the destination
///   already exists on disk with the **same byte length** as the source.
/// - **[`SkipConflict`](Action::SkipConflict)** if the destination already exists
///   but with a *different* byte length (a distinct file we must not overwrite).
/// - otherwise **[`Copy`](Action::Copy)** or **[`Move`](Action::Move)** per
///   `strategy`.
///
/// # Duplicate heuristic
/// The already-exists check compares **sizes only** (`std::fs::metadata(..).len()`),
/// not a content hash. Two files of equal length are assumed identical and the
/// source is skipped; an unequal length is a conflict. In either case the
/// existing destination is **never overwritten**. This is deliberately cheap; a
/// caller needing byte-exact de-duplication must hash separately.
///
/// `plan` performs **no** filesystem mutation — it creates no directories and
/// copies nothing.
pub fn plan(
    files: &[PathBuf],
    dest_root: &Path,
    template: &Template,
    strategy: Strategy,
) -> OrganizePlan {
    let mut entries = Vec::with_capacity(files.len());
    // Destinations an earlier Copy/Move entry will write — later inputs mapping
    // to the same path are duplicates of that pending write.
    let mut claimed: HashSet<PathBuf> = HashSet::new();
    for src in files {
        let tags = read_tags(src);
        let dst = dest_root.join(template.render(&tags, src));
        let action = classify(src, &dst, strategy, &claimed);
        if matches!(action, Action::Copy | Action::Move) {
            claimed.insert(dst.clone());
        }
        entries.push(PlanEntry {
            src: src.clone(),
            dst,
            action,
        });
    }
    OrganizePlan {
        dest_root: dest_root.to_path_buf(),
        entries,
    }
}

/// Classify one source against the destinations already claimed this batch and
/// whatever currently sits on disk at `dst`.
fn classify(src: &Path, dst: &Path, strategy: Strategy, claimed: &HashSet<PathBuf>) -> Action {
    if claimed.contains(dst) {
        return Action::SkipDuplicate;
    }
    if let Ok(dst_meta) = std::fs::metadata(dst) {
        // Something already lives at `dst`. Equal size = treat as already
        // organized (SkipDuplicate); different size = a distinct file we refuse
        // to clobber (SkipConflict). Size-only, not a content hash.
        let src_len = std::fs::metadata(src).map(|m| m.len()).ok();
        return if src_len == Some(dst_meta.len()) {
            Action::SkipDuplicate
        } else {
            Action::SkipConflict
        };
    }
    match strategy {
        Strategy::Copy => Action::Copy,
        Strategy::Move => Action::Move,
    }
}

#[cfg(test)]
mod tests {
    use super::{Action, plan};
    use crate::{Strategy, Template, presets};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

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
        let dir =
            std::env::temp_dir().join(format!("organize-test-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The `artist-album` preset: `{AlbumArtist}/{Album}/{TrackNo} - {Title}`.
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

    #[test]
    fn two_distinct_tracks_plan_to_two_copies_with_expected_dsts() {
        let src = unique_dir("src-distinct");
        let dest = unique_dir("dest-distinct");
        // Verified tags: tagged.flac = Test Artist / Test Album / Soul Test Track;
        // arted.flac = untagged but for title → Unknown Artist / Unknown Album.
        let a = src.join("a.flac");
        let b = src.join("b.flac");
        std::fs::copy(testdata("tagged.flac"), &a).unwrap();
        std::fs::copy(testdata("arted.flac"), &b).unwrap();

        let out = plan(
            &[a.clone(), b.clone()],
            &dest,
            &artist_album(),
            Strategy::Copy,
        );

        assert_eq!(out.dest_root, dest);
        assert_eq!(out.entries.len(), 2);
        assert_eq!(out.entries[0].action, Action::Copy);
        assert_eq!(out.entries[1].action, Action::Copy);
        assert_eq!(
            out.entries[0].dst,
            join(
                &dest,
                &["Test Artist", "Test Album", "00 - Soul Test Track.flac"]
            )
        );
        assert_eq!(
            out.entries[1].dst,
            join(
                &dest,
                &["Unknown Artist", "Unknown Album", "00 - Arted Track.flac"]
            )
        );
        // No mutation: the plan must not have created the destination tree.
        assert!(!out.entries[0].dst.exists());
        assert!(!join(&dest, &["Test Artist"]).exists());

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn same_file_twice_is_copy_then_skip_duplicate() {
        let src = unique_dir("src-dup");
        let dest = unique_dir("dest-dup");
        let a = src.join("a.flac");
        std::fs::copy(testdata("tagged.flac"), &a).unwrap();

        let out = plan(
            &[a.clone(), a.clone()],
            &dest,
            &artist_album(),
            Strategy::Copy,
        );

        assert_eq!(out.entries.len(), 2);
        assert_eq!(out.entries[0].action, Action::Copy);
        assert_eq!(out.entries[1].action, Action::SkipDuplicate);
        assert_eq!(out.entries[0].dst, out.entries[1].dst);

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn preexisting_dst_equal_size_is_skip_duplicate() {
        let src = unique_dir("src-eq");
        let dest = unique_dir("dest-eq");
        let a = src.join("a.flac");
        std::fs::copy(testdata("tagged.flac"), &a).unwrap();

        // Pre-create the exact destination with the SAME bytes (equal size).
        let dst = join(
            &dest,
            &["Test Artist", "Test Album", "00 - Soul Test Track.flac"],
        );
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::fs::copy(testdata("tagged.flac"), &dst).unwrap();

        let out = plan(
            std::slice::from_ref(&a),
            &dest,
            &artist_album(),
            Strategy::Copy,
        );
        assert_eq!(out.entries.len(), 1);
        assert_eq!(out.entries[0].action, Action::SkipDuplicate);

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn preexisting_dst_different_size_is_skip_conflict() {
        let src = unique_dir("src-neq");
        let dest = unique_dir("dest-neq");
        let a = src.join("a.flac");
        std::fs::copy(testdata("tagged.flac"), &a).unwrap();

        // Pre-create the destination with DIFFERENT (smaller) content.
        let dst = join(
            &dest,
            &["Test Artist", "Test Album", "00 - Soul Test Track.flac"],
        );
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::fs::write(&dst, b"not the same file").unwrap();

        let out = plan(
            std::slice::from_ref(&a),
            &dest,
            &artist_album(),
            Strategy::Copy,
        );
        assert_eq!(out.entries.len(), 1);
        assert_eq!(out.entries[0].action, Action::SkipConflict);

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn move_strategy_yields_move_actions() {
        let src = unique_dir("src-move");
        let dest = unique_dir("dest-move");
        let a = src.join("a.flac");
        std::fs::copy(testdata("tagged.flac"), &a).unwrap();

        let out = plan(
            std::slice::from_ref(&a),
            &dest,
            &artist_album(),
            Strategy::Move,
        );
        assert_eq!(out.entries.len(), 1);
        assert_eq!(out.entries[0].action, Action::Move);

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dest);
    }
}
