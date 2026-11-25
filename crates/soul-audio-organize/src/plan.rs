//! Planning: group the inputs into albums (the importer's identity rule), read
//! each file's tags, compute its destination via the [`Template`], add each
//! album's side files (rewriting a `.cue`'s `FILE` lines in memory), and
//! classify every [`Action`] — **without** mutating the filesystem. `plan` only
//! *reads* tags and `.cue` side files, *lists* album folders and *stats* (for a
//! rewritten cue: reads) any existing destination; it never creates directories
//! or copies bytes.

use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::album::{self, AlbumGroup, RootIndex};
use crate::cue::{self, CueNotRewritten};
use crate::paths;
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
    /// already-organized — see the size heuristic on [`plan_with`]; a rewritten
    /// `.cue` must match its rewritten bytes exactly).
    SkipDuplicate,
    /// Skip: a **different** file already occupies the destination (same path,
    /// different byte length). We never overwrite.
    SkipConflict,
}

/// What kind of file a [`PlanEntry`] places.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EntryKind {
    /// An input audio file, renamed per the template.
    Audio,
    /// A non-audio file travelling with its album (`.log`, `.cue`, covers,
    /// `Scans/…`, lyrics, `.txt`/`.nfo`). Never modified — only copied/moved —
    /// except a `.cue` whose `FILE` lines are pointed at the renamed tracks
    /// ([`PlanEntry::rewritten`]); renamed only when it shares a track's stem
    /// (it follows that track) or its name must be made unique /
    /// filesystem-safe.
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
    /// A `.cue` side file whose `FILE` lines name audio this plan renames:
    /// the cue with those lines pointed at the new names (see
    /// [`CueNotRewritten`] for when a cue is left as-is). When `Some`,
    /// [`apply`](crate::apply) writes these bytes at `dst` instead of copying
    /// `src` (a Move removes `src` only after the write succeeded). `None` for
    /// every other entry.
    pub rewritten: Option<Vec<u8>>,
}

/// Why an album's side files were left where they are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SideFilesLeft {
    /// The source album root holds more than one album (another album shares
    /// the folder, or sits in a sub-folder of it): ownership is ambiguous.
    SharedAlbumRoot,
    /// The template gives the album no folder of its own (e.g. `flat`,
    /// `{Artist}/{Title}` — no `{Album}` folder — or its tracks render under
    /// different folders).
    NoAlbumFolder,
    /// The album's tracks sit loose in the source root itself — its other files
    /// are not album material to sweep along.
    AtSourceRoot,
    /// No [`PlanOptions::source_root`] contains the album root (none was given —
    /// [`plan`](fn@plan) — or the album lies outside it, or an input path has a
    /// `..` component): the folder may be an arbitrary user folder
    /// (`~/Downloads`), so nothing in it is swept along.
    NoSourceRoot,
    /// One of the album's audio files is [`Action::SkipConflict`] (a different
    /// file already sits at its destination): the album does not move, so its
    /// side files stay with it.
    AudioConflict,
    /// Only part of the album was selected: its root, or one of its disc
    /// folders (`CD2/`), still holds audio that is not among the inputs. The
    /// side files belong with the whole album, so none of them move.
    ///
    /// Conservative by design: audio that can never be an input (a format the
    /// importer does not decode, e.g. `.wma`/`.ape`) also counts, so an album
    /// folder holding such files never sweeps its side files.
    PartialSelection,
}

/// An album root whose side files were **not** planned (reported, never guessed).
///
/// For [`SideFilesLeft::AudioConflict`] and [`SideFilesLeft::PartialSelection`]
/// these are all the album's side files (its root lies in the source root and
/// holds only this album, so it is walked); for every other reason only the
/// album root's **own** files are counted — the planner never walks a folder
/// whose files it will not plan.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LeftInPlace {
    /// The source album root.
    pub album_root: PathBuf,
    /// Why.
    pub reason: SideFilesLeft,
    /// How many side files stay there (exact).
    pub files: usize,
    /// Those files, sorted — at most [`LeftInPlace::MAX_PATHS`] of them
    /// (`files` stays exact).
    pub paths: Vec<PathBuf>,
}

impl LeftInPlace {
    /// The cap on [`LeftInPlace::paths`].
    pub const MAX_PATHS: usize = 1000;

    fn new(album_root: &Path, reason: SideFilesLeft, mut files: Vec<PathBuf>) -> Self {
        let count = files.len();
        files.truncate(Self::MAX_PATHS);
        Self {
            album_root: album_root.to_path_buf(),
            reason,
            files: count,
            paths: files,
        }
    }
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
    /// **if empty** after the moves, deepest first — only folders strictly
    /// inside [`PlanOptions::source_root`]. Empty for Copy and without a source
    /// root.
    pub prune_dirs: Vec<PathBuf>,
    /// Album roots (each once, first-seen order) of multi-disc albums where some
    /// track's disc number came **only** from its disc-folder name (`CD2/`, no
    /// disc tag) while the template has no `{DiscDir}`: the new layout puts all
    /// discs in one folder, so a later re-import reads them as one disc. A UI
    /// should warn, or suggest a `+disc-folders` preset or tagging the discs.
    pub folder_inferred_discs: Vec<PathBuf>,
    /// `.cue` side files planned **unchanged** although they may name audio
    /// this plan renames, with the reason (source path, first-seen order). A
    /// UI should warn that their `FILE` lines may point at missing files.
    pub cues_not_rewritten: Vec<(PathBuf, CueNotRewritten)>,
}

/// Optional planning context. Construct with [`PlanOptions::new`] (or
/// `Default` for none).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PlanOptions {
    /// The folder the inputs were collected from (the scan/staging root). Album
    /// roots never climb to or above it; an album whose tracks sit loose in it
    /// keeps its side files in place; Move cleanup may prune emptied folders up
    /// to — never including — it. `None`: **no** side files are planned (each
    /// album's are reported as [`SideFilesLeft::NoSourceRoot`]) and nothing is
    /// pruned.
    ///
    /// **Form:** the same form as the input paths — both absolute (preferred),
    /// or both relative to the same working directory. A Windows verbatim
    /// prefix on either side (`\\?\D:\…`, `\\?\UNC\…`, as `canonicalize`
    /// returns) and, on Windows/macOS, letter-case differences are tolerated.
    /// A root containing `..` is not trusted: no side files, no pruning.
    pub source_root: Option<PathBuf>,
}

impl PlanOptions {
    /// Options with [`source_root`](Self::source_root) set.
    pub fn new(source_root: impl Into<PathBuf>) -> Self {
        Self {
            source_root: Some(source_root.into()),
        }
    }
}

/// [`plan_with`] with default [`PlanOptions`] (no source root): audio only.
///
/// Without a source root an album root could be any user folder, so `plan`
/// plans **no side files** (they are reported in
/// [`OrganizePlan::side_files_left`] as [`SideFilesLeft::NoSourceRoot`]) and
/// **no** Move cleanup ([`OrganizePlan::prune_dirs`] stays empty). Side files
/// and pruning require [`plan_with`] with [`PlanOptions::source_root`].
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
/// **Side files**: for every album root inside the source root that holds
/// exactly one album with a folder of its own, each non-audio, non-junk file
/// under it (recursively; not into a sub-folder holding another album's audio)
/// is planned too — see [`EntryKind::SideFile`] — unless only part of the
/// album was selected (its root or a disc folder holds non-input audio:
/// [`SideFilesLeft::PartialSelection`]) or one of the album's audio files is a
/// [`SkipConflict`](Action::SkipConflict). Otherwise the root is reported in
/// [`OrganizePlan::side_files_left`].
///
/// **Cue sheets**: a planned `.cue` side file whose `FILE` lines name inputs
/// of this plan (relative to the cue's folder; case-insensitive on
/// Windows/macOS) gets those lines pointed at the inputs' new destinations,
/// relative to the cue's own destination folder, in
/// [`PlanEntry::rewritten`]; everything else in it (BOM, line endings, other
/// commands, `REM` lines) is kept byte for byte, and a `FILE` line naming
/// anything else is kept as-is. A cue that is not UTF-8 is rewritten only when
/// the names involved are ASCII; otherwise — or when a new name cannot be
/// written — it is planned unchanged and listed in
/// [`OrganizePlan::cues_not_rewritten`]. Every other side file is never
/// modified.
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
/// Destinations compare case-insensitively on Windows/macOS (see the crate
/// docs), so `Abc` and `ABC` album folders are told apart with ` (2)` there.
///
/// # Duplicate heuristic
/// The already-exists check compares **sizes only**, not a content hash — except
/// for a rewritten `.cue` ([`PlanEntry::rewritten`]), which must hold exactly
/// the rewritten bytes; the existing destination is **never overwritten**.
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
    // Side files and pruning trust only a source root without `..`, and only
    // albums inside it none of whose inputs has `..` (it may escape the root).
    let trusted_root = source_root.filter(|r| !paths::has_parent_dir(r));
    let trusted: Vec<bool> = albums
        .iter()
        .map(|a| {
            trusted_root.is_some_and(|sr| paths::starts_with(&a.root, sr))
                && !a.tracks.iter().any(|&i| paths::has_parent_dir(&files[i]))
        })
        .collect();

    let mut entries: Vec<PlanEntry> = Vec::with_capacity(files.len());
    // Parallel to `entries`: may a Move of this entry prune its source folders.
    let mut prunable: Vec<bool> = Vec::with_capacity(files.len());
    let mut dsts = Dsts::default();

    // Each track's destination and album folder (relative), then distinct
    // albums rendering to one folder are pulled apart (` (2)`, ` (3)`, …).
    let mut rel_dsts: Vec<PathBuf> = Vec::with_capacity(files.len());
    let mut album_dirs: Vec<PathBuf> = Vec::with_capacity(files.len());
    let mut album_folders: Vec<PathBuf> = Vec::with_capacity(files.len());
    for (src, info) in files.iter().zip(&tracks) {
        rel_dsts.push(template.render(&info.tags, src));
        album_dirs.push(template.render_album_dir(&info.tags, src));
        album_folders.push(
            template
                .render_album_folder(&info.tags, src)
                .unwrap_or_default(),
        );
    }
    if template.has_album_dir() {
        separate_albums(&albums, &mut rel_dsts, &mut album_dirs, &album_folders);
    }

    // 1. Audio, in input order (entry `i` is input `i`).
    let mut audio_dst: Vec<PathBuf> = Vec::with_capacity(files.len());
    for ((src, info), rel_dst) in files.iter().zip(&tracks).zip(&rel_dsts) {
        let dst = dest_root.join(rel_dst);
        audio_dst.push(dst.clone());
        dsts.push(&mut entries, src, dst, strategy, EntryKind::Audio, None);
        prunable.push(trusted[info.album]);
    }
    // Each audio source (as a key) → where it lands: the names a cue's FILE
    // lines move to. A conflicting track does not move (its album's side
    // files stay too); the first entry for a source wins. Keyed by
    // `paths::key` of the input path — the same form a cue's FILE names are
    // resolved in (its folder is a walk of the album root, itself derived
    // from the inputs), so a verbatim prefix or a case difference between
    // the two never misses.
    let mut audio_to: HashMap<PathBuf, PathBuf> = HashMap::new();
    for e in entries.iter().filter(|e| e.action != Action::SkipConflict) {
        audio_to
            .entry(paths::key(&e.src))
            .or_insert_with(|| e.dst.clone());
    }
    let mut cues_not_rewritten: Vec<(PathBuf, CueNotRewritten)> = Vec::new();

    // 2. Side files, album by album; each album root is handled once.
    let roots = RootIndex::new(&albums);
    let mut side_files_left: Vec<LeftInPlace> = Vec::new();
    let mut seen_roots: HashSet<&Path> = HashSet::new();
    // An input is never also a side file (e.g. one with a non-audio extension).
    let inputs: HashSet<PathBuf> = files.iter().map(|f| paths::key(f)).collect();
    for (a, album) in albums.iter().enumerate() {
        if !seen_roots.insert(album.root.as_path()) {
            continue;
        }
        // Decided BEFORE any walk: an untrusted root may be any folder (a
        // drive root, `~/Downloads`) and is never recursed into.
        let reason = if !trusted[a] {
            Some(SideFilesLeft::NoSourceRoot)
        } else if trusted_root.is_some_and(|sr| paths::same(&album.root, sr)) {
            Some(SideFilesLeft::AtSourceRoot)
        } else if roots.is_shared(&album.root) {
            Some(SideFilesLeft::SharedAlbumRoot)
        } else if !template.has_album_dir() {
            Some(SideFilesLeft::NoAlbumFolder)
        } else {
            None
        };
        let rel_dir = match reason {
            None => album_dest_dir(&album_dirs, album),
            Some(_) => None,
        };
        let Some(rel_dir) = rel_dir else {
            let own = side::direct(&album.root, &inputs);
            if !own.is_empty() {
                let reason = reason.unwrap_or(SideFilesLeft::NoAlbumFolder);
                side_files_left.push(LeftInPlace::new(&album.root, reason, own));
            }
            continue;
        };
        let side::Collected {
            files: side,
            partial,
        } = side::collect(&album.root, dest_root, &inputs);
        if side.is_empty() {
            continue;
        }
        if partial {
            side_files_left.push(LeftInPlace::new(
                &album.root,
                SideFilesLeft::PartialSelection,
                side,
            ));
            continue;
        }
        if album
            .tracks
            .iter()
            .any(|&i| entries[i].action == Action::SkipConflict)
        {
            side_files_left.push(LeftInPlace::new(
                &album.root,
                SideFilesLeft::AudioConflict,
                side,
            ));
            continue;
        }
        let album_dir = dest_root.join(rel_dir);
        let stems: HashMap<PathBuf, PathBuf> = album
            .tracks
            .iter()
            .filter(|&&i| files[i].extension().is_some())
            .map(|&i| {
                (
                    paths::key(&files[i].with_extension("")),
                    audio_dst[i].clone(),
                )
            })
            .collect();
        let placement = AlbumPlacement {
            root: &album.root,
            dir: &album_dir,
            disc_dirs: template.has_disc_dir() && album.disc_count > 1,
            disc_count: album.disc_count,
            stems: &stems,
        };
        for (src, dst) in side::place(&placement, &side, &dsts.taken) {
            let rewritten = if cue::is_cue(&src) {
                cue::plan_rewrite(&src, &dst, &audio_to).unwrap_or_else(|why| {
                    cues_not_rewritten.push((src.clone(), why));
                    None
                })
            } else {
                None
            };
            dsts.push(
                &mut entries,
                &src,
                dst,
                strategy,
                EntryKind::SideFile,
                rewritten,
            );
            prunable.push(true);
        }
    }

    let prune_dirs = match (strategy, trusted_root) {
        (Strategy::Move, Some(sr)) => prune_candidates(&entries, &prunable, sr),
        _ => Vec::new(),
    };
    OrganizePlan {
        dest_root: dest_root.to_path_buf(),
        entries,
        side_files_left,
        prune_dirs,
        folder_inferred_discs: folder_inferred_discs(&albums, template),
        cues_not_rewritten,
    }
}

/// The destinations planned so far, as [`paths::key`]s.
#[derive(Default)]
struct Dsts {
    /// Destinations an earlier Copy/Move entry will write — a later entry
    /// mapping to one is a duplicate of that pending write.
    claimed: HashSet<PathBuf>,
    /// Every earlier entry's destination, whatever its action — the names a
    /// side file must avoid. Independent of what is already on disk, so a
    /// re-plan picks the same side-file names.
    taken: HashSet<PathBuf>,
}

impl Dsts {
    /// Classify and append one entry, claiming its destination when it will write.
    fn push(
        &mut self,
        entries: &mut Vec<PlanEntry>,
        src: &Path,
        dst: PathBuf,
        strategy: Strategy,
        kind: EntryKind,
        rewritten: Option<Vec<u8>>,
    ) {
        let key = paths::key(&dst);
        let action = if self.claimed.contains(&key) {
            Action::SkipDuplicate
        } else {
            classify(src, &dst, strategy, rewritten.as_deref())
        };
        if matches!(action, Action::Copy | Action::Move) {
            self.claimed.insert(key.clone());
        }
        self.taken.insert(key);
        entries.push(PlanEntry {
            src: src.to_path_buf(),
            dst,
            action,
            kind,
            rewritten,
        });
    }
}

/// [`OrganizePlan::folder_inferred_discs`]: album roots, each once.
fn folder_inferred_discs(albums: &[AlbumGroup], template: &Template) -> Vec<PathBuf> {
    if template.has_disc_dir() {
        return Vec::new();
    }
    let mut seen: HashSet<&Path> = HashSet::new();
    albums
        .iter()
        .filter(|a| a.disc_from_folder && a.disc_count > 1 && seen.insert(&a.root))
        .map(|a| a.root.clone())
        .collect()
}

/// Give every **distinct** album its own album folder within this batch.
///
/// Albums are walked in plan (first-seen input) order. The first album to
/// render into an album dir keeps it; a later album (a different identity
/// key) rendering into an already-claimed dir gets its **`{Album}` folder**
/// (`album_folders[i]`, see [`Template::render_album_folder`]) suffixed
/// ` (2)`, ` (3)`, … — the lowest `n` whose resulting album dir no album in
/// this batch claims or renders to natively — and its destinations are
/// re-rooted under it (`{AlbumArtist}/{Album}/{Year}/…` gives
/// `Box (2)/2020`, never `Box/2020 (2)`). When the `{Album}` folder is not a
/// prefix of the album dir (`{DiscDir}` before `{Album}`) the dir's last
/// folder is suffixed instead. Dirs compare as [`paths::key`]s (`Abc` and
/// `ABC` are one folder on Windows/macOS). Folders already on disk are not
/// considered: a re-run plans the same names, and its files classify as
/// `SkipDuplicate` as before. A track with no album dir (it renders into the
/// destination root) is left alone.
fn separate_albums(
    albums: &[AlbumGroup],
    rel_dsts: &mut [PathBuf],
    album_dirs: &mut [PathBuf],
    album_folders: &[PathBuf],
) {
    let native: HashSet<PathBuf> = album_dirs
        .iter()
        .filter(|d| d.components().next().is_some())
        .map(|d| paths::key(d))
        .collect();
    let mut owner: HashMap<PathBuf, usize> = HashMap::new();
    for (a, album) in albums.iter().enumerate() {
        // This album's native dir (as a key) → its final `{Album}` folder.
        let mut mine: HashMap<PathBuf, PathBuf> = HashMap::new();
        for &i in &album.tracks {
            let dir = album_dirs[i].clone();
            let folder = Some(&album_folders[i])
                .filter(|f| f.components().next().is_some() && dir.starts_with(f))
                .unwrap_or(&dir)
                .clone();
            let Some(name) = folder.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            let below = dir
                .strip_prefix(&folder)
                .map(Path::to_path_buf)
                .unwrap_or_default();
            let fin = mine
                .entry(paths::key(&dir))
                .or_insert_with(|| {
                    let fin = match owner.get(&paths::key(&dir)) {
                        Some(&o) if o != a => (2u32..)
                            .map(|n| folder.with_file_name(format!("{name} ({n})")))
                            .find(|c| {
                                let k = paths::key(&under(c, &below));
                                !owner.contains_key(&k) && !native.contains(&k)
                            })
                            .unwrap_or_else(|| folder.clone()),
                        _ => folder.clone(),
                    };
                    owner.insert(paths::key(&under(&fin, &below)), a);
                    fin
                })
                .clone();
            if fin != folder
                && let Ok(rest) = rel_dsts[i].strip_prefix(&folder)
            {
                rel_dsts[i] = fin.join(rest);
                album_dirs[i] = under(&fin, &below);
            }
        }
    }
}

/// `base/rest`, or `base` itself when `rest` is empty (no trailing separator).
fn under(base: &Path, rest: &Path) -> PathBuf {
    if rest.as_os_str().is_empty() {
        base.to_path_buf()
    } else {
        base.join(rest)
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

/// Every source folder a Move may empty: for each moved, `prunable` entry, its
/// parent and upward — up to but excluding `source_root` (compared as
/// [`paths::key`]s). Never the source root or an ancestor of it. Deepest
/// first, deterministic.
fn prune_candidates(entries: &[PlanEntry], prunable: &[bool], source_root: &Path) -> Vec<PathBuf> {
    let mut set: BTreeSet<PathBuf> = BTreeSet::new();
    for (e, &ok) in entries.iter().zip(prunable) {
        if e.action != Action::Move || !ok {
            continue;
        }
        let mut dir = e.src.parent();
        while let Some(d) = dir {
            if paths::same(d, source_root) || !paths::starts_with(d, source_root) {
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

/// Classify one source against whatever currently sits on disk at `dst` (the
/// batch's own claims are checked by the caller). A `rewritten` cue is a
/// duplicate only when `dst` holds exactly those bytes (cues are small, so the
/// content is compared); any other file only by size.
fn classify(src: &Path, dst: &Path, strategy: Strategy, rewritten: Option<&[u8]>) -> Action {
    if let Ok(dst_meta) = std::fs::metadata(dst) {
        // Something already lives at `dst`: the same = treat as already
        // organized (SkipDuplicate); different = a distinct file we refuse to
        // clobber (SkipConflict).
        let same = match rewritten {
            Some(bytes) => {
                dst_meta.len() == bytes.len() as u64 && std::fs::read(dst).is_ok_and(|d| d == bytes)
            }
            None => std::fs::metadata(src).map(|m| m.len()).ok() == Some(dst_meta.len()),
        };
        return if same {
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
