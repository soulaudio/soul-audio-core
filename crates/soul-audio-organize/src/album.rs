//! Album grouping for the planner — the importer's identity rule (album root
//! folder + album title + MusicBrainz release id; a disc folder folds into its
//! parent via `soul_audio_tags::disc_folder`), re-stated here because organize
//! must not depend on import. An organize run and a later re-import therefore
//! agree on what an album is (`tests/multi_disc_e2e.rs` asserts it).

use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;
use std::path::{Path, PathBuf};

use soul_audio_tags::{Tags, disc_folder};

use crate::paths;
use crate::tags::read_tags;

/// One input track after grouping.
pub(crate) struct TrackInfo {
    /// The file's tags with `disc` set to the effective disc number (tag, else
    /// the disc folder's number, else 1) and `disc_total` set to the album's
    /// `disc_count` — so the disc tokens render with the album's context.
    pub(crate) tags: Tags,
    /// Index into the album list returned alongside.
    pub(crate) album: usize,
}

/// One album: its source root, disc count, and member tracks (input indices).
pub(crate) struct AlbumGroup {
    pub(crate) root: PathBuf,
    pub(crate) disc_count: u32,
    pub(crate) tracks: Vec<usize>,
    /// Some track's disc number came only from its disc-folder name (no disc
    /// tag) — a layout without a `{DiscDir}` directory segment loses it on
    /// re-import.
    pub(crate) disc_from_folder: bool,
}

/// `(album root, disc-folder number)` for `src`: its parent folder, or the
/// grandparent when the parent's name is a disc folder that is neither
/// `source_root` nor an ancestor of it (compared host-aware, see
/// [`paths::key`]). Mirrors `soul-audio-import`'s `album_root`.
pub(crate) fn album_root(src: &Path, source_root: Option<&Path>) -> (PathBuf, Option<u32>) {
    let parent = src.parent().unwrap_or(Path::new(""));
    let may_climb = source_root.is_none_or(|r| !paths::starts_with(r, parent));
    if may_climb
        && let Some(df) = parent
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(disc_folder::parse)
        && let Some(grandparent) = parent.parent()
    {
        return (grandparent.to_path_buf(), Some(u32::from(df.number)));
    }
    (parent.to_path_buf(), None)
}

/// One input's tags plus the album-identity facts derived from them.
struct Read {
    tags: Tags,
    root: PathBuf,
    folder_disc: Option<u32>,
    title: String,
    mb: String,
}

/// Read every input's tags and group them into albums (first-seen order).
///
/// Mirrors `soul-audio-import`'s `build_catalog` exactly:
/// - key = (album root, album title — `"Unknown Album"` when untagged, effective
///   MusicBrainz release id);
/// - an **untagged** track's effective release id is the first non-empty id seen
///   (input order) in its (album root, title) group, so a partially tagged
///   release stays one album while two tagged releases in one folder split;
/// - disc number: tag (> 0, fits `u16`) → disc-folder number (`CD0` counts as
///   absent) → 1; `disc_count = max(disc_total tag, disc numbers, 1)`.
pub(crate) fn group(
    files: &[PathBuf],
    source_root: Option<&Path>,
) -> (Vec<TrackInfo>, Vec<AlbumGroup>) {
    let reads: Vec<Read> = files
        .iter()
        .map(|src| {
            let tags = read_tags(src);
            let (root, folder_disc) = album_root(src, source_root);
            let title = tags
                .album
                .clone()
                .unwrap_or_else(|| "Unknown Album".to_owned());
            let mb = tags
                .musicbrainz_album_id
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .to_owned();
            Read {
                tags,
                root,
                folder_disc,
                title,
                mb,
            }
        })
        .collect();
    // Pre-pass (input order): the first non-empty release id per (root, title).
    let mut first_mb: HashMap<(&Path, &str), &str> = HashMap::new();
    for r in reads.iter().filter(|r| !r.mb.is_empty()) {
        first_mb
            .entry((r.root.as_path(), r.title.as_str()))
            .or_insert(r.mb.as_str());
    }
    let effective_mb: Vec<String> = reads
        .iter()
        .map(|r| {
            if r.mb.is_empty() {
                first_mb
                    .get(&(r.root.as_path(), r.title.as_str()))
                    .copied()
                    .unwrap_or("")
                    .to_owned()
            } else {
                r.mb.clone()
            }
        })
        .collect();

    let mut albums: Vec<AlbumGroup> = Vec::new();
    let mut ids: HashMap<(PathBuf, String, String), usize> = HashMap::new();
    let mut infos: Vec<TrackInfo> = Vec::with_capacity(files.len());
    for (i, (r, mb)) in reads.into_iter().zip(effective_mb).enumerate() {
        let Read {
            mut tags,
            root,
            folder_disc,
            title,
            ..
        } = r;
        let key = (root.clone(), title, mb);
        let album = match ids.get(&key) {
            Some(&a) => a,
            None => {
                albums.push(AlbumGroup {
                    root,
                    disc_count: 1,
                    tracks: Vec::new(),
                    disc_from_folder: false,
                });
                ids.insert(key, albums.len() - 1);
                albums.len() - 1
            }
        };
        // `u16` bounds like the importer: a bogus 65536 is no disc at all.
        let disc_tag = tags.disc.filter(|&d| d > 0 && d <= u32::from(u16::MAX));
        let disc_total = tags
            .disc_total
            .filter(|&d| d <= u32::from(u16::MAX))
            .unwrap_or(0);
        let from_folder = folder_disc.filter(|&n| n > 0 && disc_tag.is_none());
        let disc = disc_tag.or(from_folder).unwrap_or(1);
        let group = &mut albums[album];
        group.disc_from_folder |= from_folder.is_some();
        group.disc_count = group.disc_count.max(disc).max(disc_total);
        group.tracks.push(i);
        tags.disc = Some(disc);
        infos.push(TrackInfo { tags, album });
    }
    for info in &mut infos {
        info.tags.disc_total = Some(albums[info.album].disc_count);
    }
    (infos, albums)
}

/// Album roots, for the side-file rule "the album root holds exactly one album".
pub(crate) struct RootIndex {
    counts: BTreeMap<PathBuf, usize>,
}

impl RootIndex {
    pub(crate) fn new(albums: &[AlbumGroup]) -> Self {
        let mut counts = BTreeMap::new();
        for a in albums {
            *counts.entry(a.root.clone()).or_insert(0) += 1;
        }
        Self { counts }
    }

    /// Whether `root` is ambiguous: another album shares it, or another album's
    /// root is nested inside it. `Path` orders component-wise, so a nested root
    /// sorts immediately after `root`.
    pub(crate) fn is_shared(&self, root: &Path) -> bool {
        if self.counts.get(root).is_some_and(|&n| n > 1) {
            return true;
        }
        self.counts
            .range::<Path, _>((Bound::Excluded(root), Bound::Unbounded))
            .next()
            .is_some_and(|(next, _)| next.starts_with(root))
    }
}

#[cfg(test)]
mod tests {
    use super::{AlbumGroup, RootIndex, album_root};
    use std::path::{Path, PathBuf};

    #[test]
    fn disc_folder_parent_climbs_to_the_album_root() {
        let lib = Some(Path::new("lib"));
        assert_eq!(
            album_root(Path::new("lib/Box/CD2/01.flac"), lib),
            (PathBuf::from("lib/Box"), Some(2))
        );
        assert_eq!(
            album_root(Path::new("lib/Box/01.flac"), lib),
            (PathBuf::from("lib/Box"), None)
        );
        assert_eq!(
            album_root(Path::new("x/CD1/01.flac"), None).0,
            PathBuf::from("x"),
            "no source root: the rule still applies"
        );
    }

    #[test]
    fn never_climbs_out_of_the_source_root() {
        assert_eq!(
            album_root(Path::new("CD1/01.flac"), Some(Path::new("CD1"))),
            (PathBuf::from("CD1"), None)
        );
        // A disc folder that is an ANCESTOR of the source root stays too.
        assert_eq!(
            album_root(Path::new("CD1/01.flac"), Some(Path::new("CD1/sub"))),
            (PathBuf::from("CD1"), None)
        );
        // Outside the source root the disc-folder rule still applies (identity
        // only; side files and pruning stay off there).
        assert_eq!(
            album_root(Path::new("x/CD2/01.flac"), Some(Path::new("lib"))),
            (PathBuf::from("x"), Some(2))
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_verbatim_or_differently_cased_source_root_still_folds_disc_folders() {
        for root in [r"\\?\D:\lib", r"d:\LIB"] {
            assert_eq!(
                album_root(Path::new(r"D:\lib\Box\CD2\01.flac"), Some(Path::new(root))),
                (PathBuf::from(r"D:\lib\Box"), Some(2)),
                "{root}"
            );
            assert_eq!(
                album_root(Path::new(r"D:\lib\CD1\01.flac"), Some(Path::new(root))).1,
                Some(1),
                "{root}: CD1 is strictly below the root"
            );
            assert_eq!(
                album_root(Path::new(r"D:\lib\01.flac"), Some(Path::new(root))),
                (PathBuf::from(r"D:\lib"), None)
            );
        }
        assert_eq!(
            album_root(
                Path::new(r"D:\lib\CD1\01.flac"),
                Some(Path::new(r"\\?\D:\LIB\cd1"))
            ),
            (PathBuf::from(r"D:\lib\CD1"), None),
            "never climbs out of a (verbatim, re-cased) source root"
        );
    }

    #[test]
    fn shared_and_nested_roots_are_ambiguous() {
        let g = |r: &str| AlbumGroup {
            root: PathBuf::from(r),
            disc_count: 1,
            tracks: Vec::new(),
            disc_from_folder: false,
        };
        let idx = RootIndex::new(&[
            g("a/Dump"),
            g("a/Dump"),
            g("a/Artist"),
            g("a/Artist/Album"),
            g("a/Artist Two"),
        ]);
        assert!(idx.is_shared(Path::new("a/Dump")));
        assert!(idx.is_shared(Path::new("a/Artist")), "a nested album root");
        assert!(!idx.is_shared(Path::new("a/Artist/Album")));
        assert!(!idx.is_shared(Path::new("a/Artist Two")));
    }
}
