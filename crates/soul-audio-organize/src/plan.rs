//! Planning: group the inputs into albums (the importer's identity rule), read
//! each file's tags, compute its destination via the [`Template`], add each
//! album's side files, and classify every [`Action`] — **without** mutating the
//! filesystem. `plan` only *reads* tags, *lists* album folders and *stats* any
//! existing destination; it never creates directories or copies bytes.

use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::album::{self, AlbumGroup, RootIndex};
use crate::side::{self, AlbumPlacement};
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

/// What kind of file a [`PlanEntry`] places.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// An input audio file, renamed per the template.
    Audio,
    /// A non-audio file travelling with its album (`.log`, `.cue`, covers,
    /// `Scans/…`, lyrics, `.txt`/`.nfo`). Never modified — only copied/moved,
    /// renamed only when it shares a track's stem (it follows that track).
    SideFile,
}

/// One planned source → destination placement with its classified [`Action`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanEntry {
    /// The source file to copy/move.
    pub src: PathBuf,
    /// Its computed destination.
    pub dst: PathBuf,
    /// How [`apply`](crate::apply) will treat this entry.
    pub action: Action,
    /// Audio input or album side file.
    pub kind: EntryKind,
}

/// Why an album's side files were left where they are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SideFilesLeft {
    /// The source album root holds more than one album (another album shares
    /// the folder, or sits in a sub-folder of it): ownership is ambiguous.
    SharedAlbumRoot,
    /// The template gives the album no folder of its own (e.g. `flat`, or its
    /// tracks render under different folders).
    NoAlbumFolder,
    /// The album's tracks sit loose in the source root itself — its other files
    /// are not album material to sweep along.
    AtSourceRoot,
}

/// An album root whose side files were **not** planned (reported, never guessed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeftInPlace {
    /// The source album root.
    pub album_root: PathBuf,
    /// Why.
    pub reason: SideFilesLeft,
    /// How many side files stay there.
    pub files: usize,
}

/// A computed, not-yet-executed organize plan.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct OrganizePlan {
    /// The root every entry's destination lives under.
    pub dest_root: PathBuf,
    /// Audio entries in input order, then side-file entries album by album.
    pub entries: Vec<PlanEntry>,
    /// Album roots whose side files stay in place, with the reason (each root at
    /// most once).
    pub side_files_left: Vec<LeftInPlace>,
    /// [`Strategy::Move`] only: source folders [`apply`](crate::apply) removes
    /// **if empty** after the moves, deepest first. Never the source root or
    /// above it; without a source root, never above a moved file's album root.
    /// Empty for Copy.
    pub prune_dirs: Vec<PathBuf>,
}

/// Optional planning context.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanOptions {
    /// The folder the inputs were collected from (the scan/staging root). Album
    /// roots never climb to or above it; an album whose tracks sit loose in it
    /// keeps its side files in place; Move cleanup may prune emptied folders up
    /// to — never including — it. `None`: cleanup stops at each album root
    /// (inclusive).
    pub source_root: Option<PathBuf>,
}

/// [`plan_with`] with default [`PlanOptions`] (no source root).
pub fn plan(
    files: &[PathBuf],
    dest_root: &Path,
    template: &Template,
    strategy: Strategy,
) -> OrganizePlan {
    plan_with(
        files,
        dest_root,
        template,
        strategy,
        &PlanOptions::default(),
    )
}

/// Compute an [`OrganizePlan`] for `files` against `dest_root`.
///
/// Inputs are first grouped into **albums** exactly as the importer does
/// (album root folder — a `CD1`/`Disc 2 - …` folder folds into its parent —
/// plus album title plus MusicBrainz release id), so every track renders with
/// its album's disc context (`{DiscPrefix}`, `{DiscDir}`, `{DiscTotal}`; a disc
/// number missing from the tags comes from the disc folder). Each audio file's
/// destination is `dest_root.join(template.render(..))`.
///
/// **Distinct same-title albums** (e.g. two editions in different source
/// folders) never share an album folder: when a template with an `{Album}`
/// folder renders a later album into a folder an earlier album of this batch
/// already claims, the later one gets `<folder> (2)`, `(3)`, … (stable across
/// re-runs, since input order is deterministic).
///
/// **Side files**: for every album root that holds exactly one album, each
/// non-audio, non-junk file under it (recursively) is planned too — see
/// [`EntryKind::SideFile`]; otherwise the root is reported in
/// [`OrganizePlan::side_files_left`].
///
/// Each entry's [`Action`] is classified as:
/// - **[`SkipDuplicate`](Action::SkipDuplicate)** if an *earlier* entry in this
///   batch already claims the identical destination, **or** the destination
///   already exists on disk with the **same byte length** as the source.
/// - **[`SkipConflict`](Action::SkipConflict)** if the destination already exists
///   with a *different* byte length (a distinct file we must not overwrite).
/// - otherwise **[`Copy`](Action::Copy)** or **[`Move`](Action::Move)** per
///   `strategy`.
///
/// # Duplicate heuristic
/// The already-exists check compares **sizes only**, not a content hash; the
/// existing destination is **never overwritten**.
///
/// `plan_with` performs **no** filesystem mutation.
pub fn plan_with(
    files: &[PathBuf],
    dest_root: &Path,
    template: &Template,
    strategy: Strategy,
    opts: &PlanOptions,
) -> OrganizePlan {
    let source_root = opts.source_root.as_deref();
    let (tracks, albums) = album::group(files, source_root);
    let mut entries: Vec<PlanEntry> = Vec::with_capacity(files.len());
    // Parallel to `entries`: each entry's source album root (the Move-cleanup bound).
    let mut entry_roots: Vec<&Path> = Vec::with_capacity(files.len());
    // Destinations an earlier Copy/Move entry will write — later entries mapping
    // to the same path are duplicates of that pending write.
    let mut claimed: HashSet<PathBuf> = HashSet::new();

    // Each track's destination and album folder (relative), then distinct
    // albums rendering to one folder are pulled apart (` (2)`, ` (3)`, …).
    let mut rel_dsts: Vec<PathBuf> = Vec::with_capacity(files.len());
    let mut album_dirs: Vec<PathBuf> = Vec::with_capacity(files.len());
    for (src, info) in files.iter().zip(&tracks) {
        rel_dsts.push(template.render(&info.tags, src));
        album_dirs.push(template.render_album_dir(&info.tags, src));
    }
    if template.has_album_dir() {
        separate_albums(&albums, &mut rel_dsts, &mut album_dirs);
    }

    // 1. Audio, in input order.
    let mut audio_dst: Vec<PathBuf> = Vec::with_capacity(files.len());
    for ((src, info), rel_dst) in files.iter().zip(&tracks).zip(&rel_dsts) {
        let dst = dest_root.join(rel_dst);
        audio_dst.push(dst.clone());
        push(
            &mut entries,
            &mut claimed,
            src,
            dst,
            strategy,
            EntryKind::Audio,
        );
        entry_roots.push(&albums[info.album].root);
    }

    // 2. Side files, album by album; each album root is handled once.
    let roots = RootIndex::new(&albums);
    let mut side_files_left: Vec<LeftInPlace> = Vec::new();
    let mut seen_roots: HashSet<&Path> = HashSet::new();
    for album in &albums {
        if !seen_roots.insert(album.root.as_path()) {
            continue;
        }
        let side = side::collect(&album.root, dest_root);
        if side.is_empty() {
            continue;
        }
        let reason = if source_root == Some(album.root.as_path()) {
            Some(SideFilesLeft::AtSourceRoot)
        } else if roots.is_shared(&album.root) {
            Some(SideFilesLeft::SharedAlbumRoot)
        } else {
            None
        };
        let rel_dir = match reason {
            None => album_dest_dir(&album_dirs, album),
            Some(_) => None,
        };
        let Some(rel_dir) = rel_dir else {
            side_files_left.push(LeftInPlace {
                album_root: album.root.clone(),
                reason: reason.unwrap_or(SideFilesLeft::NoAlbumFolder),
                files: side.len(),
            });
            continue;
        };
        let album_dir = dest_root.join(rel_dir);
        let stems: HashMap<PathBuf, PathBuf> = album
            .tracks
            .iter()
            .filter(|&&i| files[i].extension().is_some())
            .map(|&i| (files[i].with_extension(""), audio_dst[i].clone()))
            .collect();
        let placement = AlbumPlacement {
            root: &album.root,
            dir: &album_dir,
            disc_dirs: template.has_disc_dir() && album.disc_count > 1,
            disc_count: album.disc_count,
            stems: &stems,
        };
        for (src, dst) in side::place(&placement, &side, &claimed) {
            push(
                &mut entries,
                &mut claimed,
                &src,
                dst,
                strategy,
                EntryKind::SideFile,
            );
            entry_roots.push(&album.root);
        }
    }

    let prune_dirs = match strategy {
        Strategy::Move => prune_candidates(&entries, &entry_roots, source_root),
        Strategy::Copy => Vec::new(),
    };
    OrganizePlan {
        dest_root: dest_root.to_path_buf(),
        entries,
        side_files_left,
        prune_dirs,
    }
}

/// Classify and append one entry, claiming its destination when it will write.
fn push(
    entries: &mut Vec<PlanEntry>,
    claimed: &mut HashSet<PathBuf>,
    src: &Path,
    dst: PathBuf,
    strategy: Strategy,
    kind: EntryKind,
) {
    let action = classify(src, &dst, strategy, claimed);
    if matches!(action, Action::Copy | Action::Move) {
        claimed.insert(dst.clone());
    }
    entries.push(PlanEntry {
        src: src.to_path_buf(),
        dst,
        action,
        kind,
    });
}

/// Give every **distinct** album its own album folder within this batch.
///
/// Albums are walked in plan (first-seen input) order. The first album to
/// render into an album folder keeps it; a later album (a different identity
/// key) rendering into an already-claimed folder gets that folder's last
/// component suffixed ` (2)`, ` (3)`, … — the lowest `n` whose name no album in
/// this batch claims or renders to natively — and its destinations are
/// re-rooted under it (the path below the album folder is unchanged). Folders
/// already on disk are not considered: a re-run plans the same names, and its
/// files classify as `SkipDuplicate` as before. A track with no album folder
/// (it renders into the destination root) is left alone.
fn separate_albums(albums: &[AlbumGroup], rel_dsts: &mut [PathBuf], album_dirs: &mut [PathBuf]) {
    let native: HashSet<PathBuf> = album_dirs
        .iter()
        .filter(|d| d.components().next().is_some())
        .cloned()
        .collect();
    let mut owner: HashMap<PathBuf, usize> = HashMap::new();
    for (a, album) in albums.iter().enumerate() {
        // This album's native folder → its final folder.
        let mut mine: HashMap<PathBuf, PathBuf> = HashMap::new();
        for &i in &album.tracks {
            let dir = album_dirs[i].clone();
            let Some(name) = dir.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            let fin = mine
                .entry(dir.clone())
                .or_insert_with(|| {
                    let fin = match owner.get(&dir) {
                        Some(&o) if o != a => (2u32..)
                            .map(|n| dir.with_file_name(format!("{name} ({n})")))
                            .find(|c| !owner.contains_key(c) && !native.contains(c))
                            .unwrap_or_else(|| dir.clone()),
                        _ => dir.clone(),
                    };
                    owner.insert(fin.clone(), a);
                    fin
                })
                .clone();
            if fin != dir
                && let Ok(below) = rel_dsts[i].strip_prefix(&dir)
            {
                rel_dsts[i] = fin.join(below);
                album_dirs[i] = fin;
            }
        }
    }
}

/// The album's own destination folder, relative to the destination root: the
/// common ancestor of its tracks' album folders (rendered with `{DiscDir}`
/// neutralized, after [`separate_albums`]). `None` when that is the destination
/// root itself (e.g. `flat`, or tracks of one album rendering under different
/// artist folders).
fn album_dest_dir(album_dirs: &[PathBuf], album: &AlbumGroup) -> Option<PathBuf> {
    let mut common: Option<PathBuf> = None;
    for &i in &album.tracks {
        let dir = &album_dirs[i];
        common = Some(match common {
            None => dir.clone(),
            Some(c) => c
                .components()
                .zip(dir.components())
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| a)
                .collect(),
        });
    }
    common.filter(|c| c.components().next().is_some())
}

/// Every source folder a Move may empty: for each moved entry, its parent and
/// upward — up to and including the album root, or (with a source root above
/// the album root) up to but excluding the source root. Never the source root
/// or an ancestor of it. Deepest first, deterministic.
fn prune_candidates(
    entries: &[PlanEntry],
    roots: &[&Path],
    source_root: Option<&Path>,
) -> Vec<PathBuf> {
    let mut set: BTreeSet<PathBuf> = BTreeSet::new();
    for (e, root) in entries.iter().zip(roots) {
        if e.action != Action::Move {
            continue;
        }
        let limit = match source_root {
            Some(sr) if root.starts_with(sr) => Some(sr),
            _ => root.parent(),
        };
        let Some(limit) = limit else {
            continue;
        };
        let mut dir = e.src.parent();
        while let Some(d) = dir {
            if d == limit || !d.starts_with(limit) {
                break;
            }
            if source_root.is_some_and(|sr| sr.starts_with(d)) {
                break;
            }
            set.insert(d.to_path_buf());
            dir = d.parent();
        }
    }
    let mut dirs: Vec<PathBuf> = set.into_iter().collect();
    dirs.sort_by_key(|d| Reverse(d.components().count()));
    dirs
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
