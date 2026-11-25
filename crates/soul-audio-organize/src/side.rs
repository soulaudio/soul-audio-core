//! Side files: the non-audio files that travel with an album — rip logs and cue
//! sheets, covers, `Scans/`, lyrics, `.txt`/`.nfo`. Discovery walks the album
//! root; placement decides each file's destination in the album's new folder.
//! Side files are only copied/moved, never modified (a `.log`'s checksum must
//! stay valid) — except a `.cue` sheet, whose `FILE` lines the planner points
//! at the renamed tracks (`crate::cue`).

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use soul_audio_tags::disc_folder;

use crate::paths;
use crate::sanitize::{sanitize_file_name, sanitize_segment};
use crate::template::{disc_dir_name, disc_label};

/// Audio extensions (lower-case): `soul-audio-import`'s `SUPPORTED` +
/// `KNOWN_AUDIO_WE_DO_NOT_DECODE`, kept in sync by hand (organize must not
/// depend on import). A file with one of these is never a side file.
pub(crate) const AUDIO_EXTS: &[&str] = &[
    "wav", "flac", "mp3", "mp2", "mp1", "m4a", "aac", "ogg", "oga", "opus", "aiff", "aif", "dsf",
    "dff", "mka", "webm", "wv", "caf", "wma", "ape", "mpc", "tta", "shn", "ra", "m4b", "m4p",
    "aifc", "au", "ac3", "dts", "amr",
];

/// Recursion guard for a pathological tree.
const MAX_DEPTH: usize = 16;

/// The longest file/folder name (in bytes) a side-file destination component
/// may have — the NTFS / ext4 / APFS limit.
const MAX_NAME_BYTES: usize = 255;

pub(crate) fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTS.iter().any(|a| a.eq_ignore_ascii_case(e)))
}

/// Hidden (`.`-prefixed: `.DS_Store`, AppleDouble `._x`) or OS junk.
pub(crate) fn is_junk(name: &str) -> bool {
    name.starts_with('.')
        || name.eq_ignore_ascii_case("Thumbs.db")
        || name.eq_ignore_ascii_case("desktop.ini")
}

/// What a folder walk saw besides side files.
#[derive(Default)]
struct Seen {
    /// Audio that is not one of the planner's inputs (in this folder or below).
    foreign_audio: bool,
    /// One of the planner's inputs, directly in this folder.
    input: bool,
    /// Set at depth 0 only: the album root itself, or one of its disc folders,
    /// holds audio that is not an input — only part of the album was selected.
    partial: bool,
}

/// An album root's side files, and whether its album was only partly selected.
pub(crate) struct Collected {
    /// Every side file under the root, sorted (see [`collect`]).
    pub(crate) files: Vec<PathBuf>,
    /// The album root, or a direct child named like a disc folder, holds audio
    /// that is not an input.
    pub(crate) partial: bool,
}

/// Every side file under `album_root`, recursively, sorted. Skips junk/hidden
/// entries, audio files, symlinks, the planner's `inputs` (keyed by
/// [`paths::key`]; an input is never also a side file), and the `skip`
/// directory (the destination root, when it sits inside a source album root).
///
/// A sub-folder holding audio that is **not** an input — another album nested
/// in this one — is skipped whole: its files belong with that audio. The
/// exception is one of this album's own disc folders: a direct child of the
/// album root named like a disc folder ([`disc_folder::parse`]) that directly
/// holds an input. Non-input audio in the album root itself or in **any** of
/// its disc folders sets [`Collected::partial`] (the planner then moves no
/// side file at all). That is conservative by design: audio that can never be
/// an input (a format the importer does not decode, e.g. `.wma`/`.ape`), or a
/// disc folder with none of its tracks selected, counts too.
///
/// Non-UTF-8 names are side files like any other (never renamed lossily — see
/// [`place`]).
pub(crate) fn collect(album_root: &Path, skip: &Path, inputs: &HashSet<PathBuf>) -> Collected {
    let mut files = Vec::new();
    let seen = walk(album_root, &paths::key(skip), inputs, 0, &mut files);
    files.sort();
    Collected {
        files,
        partial: seen.partial,
    }
}

fn walk(
    dir: &Path,
    skip: &Path,
    inputs: &HashSet<PathBuf>,
    depth: usize,
    out: &mut Vec<PathBuf>,
) -> Seen {
    let mut seen = Seen::default();
    if depth > MAX_DEPTH || paths::key(dir) == skip {
        return seen;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return seen;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name() else {
            continue;
        };
        // Junk names are ASCII, so a lossy view classifies them exactly.
        if is_junk(&name.to_string_lossy()) {
            continue;
        }
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            let mark = out.len();
            let sub = walk(&path, skip, inputs, depth + 1, out);
            let disc = depth == 0 && name.to_str().and_then(disc_folder::parse).is_some();
            seen.partial |= disc && sub.foreign_audio;
            let own_disc = disc && sub.input;
            if sub.foreign_audio && !own_disc {
                out.truncate(mark); // another album's folder: none of it is ours
                seen.foreign_audio = true;
            }
        } else if ft.is_file() {
            if inputs.contains(&paths::key(&path)) {
                seen.input = true;
            } else if is_audio(&path) {
                seen.foreign_audio = true;
                seen.partial |= depth == 0;
            } else {
                out.push(path);
            }
        }
    }
    seen
}

/// The side files directly in `album_root` (no recursion), sorted — what is
/// reported for an album whose side files are **not** planned. One
/// `read_dir`, so an album root that is an arbitrary folder (`~/Downloads`,
/// a drive root) is never walked.
pub(crate) fn direct(album_root: &Path, inputs: &HashSet<PathBuf>) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(album_root) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| !is_junk(&n.to_string_lossy()))
                && !is_audio(p)
                && !inputs.contains(&paths::key(p))
        })
        .collect();
    out.sort();
    out
}

/// Where one album's side files go.
pub(crate) struct AlbumPlacement<'a> {
    /// The album's source root.
    pub(crate) root: &'a Path,
    /// The album's destination folder (absolute).
    pub(crate) dir: &'a Path,
    /// The destination lays discs out as folders (`{DiscDir}` and > 1 disc).
    pub(crate) disc_dirs: bool,
    pub(crate) disc_count: u32,
    /// [`paths::key`] of a source track path without extension → that track's
    /// planned destination.
    pub(crate) stems: &'a HashMap<PathBuf, PathBuf>,
}

/// One side file's destination before the uniqueness pass.
struct Candidate {
    dst: PathBuf,
    /// The disc of a rule-2 file in the flat layout (prefix on collision).
    disc: Option<u32>,
    /// Rule 1: it follows a track's new name.
    followed: bool,
}

/// Plan a destination for each side file, returned in `side`'s order:
/// 1. same stem as a track in the same folder (`01 Liyue.lrc`) → the track's
///    new name with the side file's own extension (keeps lyrics attached);
/// 2. inside a disc folder → `<dir>/Disc N/<rest>` with per-disc folders, else
///    `<dir>/<rest>`, prefixed `Disc N - ` only when that destination collides
///    (with another side file or with a `taken` path);
/// 3. anything else → `<dir>/<path relative to the album root>`.
///
/// Every component below `dir` is sanitized like a template segment (so names
/// legal on APFS/ext4 — `:`, `?` — copy to NTFS) and kept to 255 bytes by
/// trimming the stem, never the extension.
///
/// Finally every destination is made unique: one already `taken` (every
/// earlier entry's destination, as [`paths::key`]s) or given to an earlier
/// side file gets ` (2)`, ` (3)`, … before its extension, so two distinct files
/// never share a destination (one would be skipped as a duplicate and silently
/// left behind). Rule-1 files are assigned first: a lyric that follows its
/// track wins the track's name over an unrelated file already carrying it.
/// Collisions compare [`paths::key`]s (case-insensitive on Windows/macOS).
/// Non-UTF-8 names are never renamed lossily (only numbered).
pub(crate) fn place(
    p: &AlbumPlacement<'_>,
    side: &[PathBuf],
    taken: &HashSet<PathBuf>,
) -> Vec<(PathBuf, PathBuf)> {
    // Pass 1: candidate destinations.
    let cands: Vec<Candidate> = side.iter().map(|src| candidate(p, src)).collect();
    // Pass 2: a disc-folder file whose flat destination collides gets `Disc N - `.
    let mut count: HashMap<PathBuf, usize> = HashMap::new();
    for c in &cands {
        *count.entry(paths::key(&c.dst)).or_insert(0) += 1;
    }
    // Pass 3: unique final destinations — stem-followed files first.
    let mut used: HashSet<PathBuf> = HashSet::new();
    let mut dsts: Vec<Option<PathBuf>> = vec![None; cands.len()];
    let order = (0..cands.len())
        .filter(|&i| cands[i].followed)
        .chain((0..cands.len()).filter(|&i| !cands[i].followed));
    for i in order {
        let c = &cands[i];
        let k = paths::key(&c.dst);
        let collides = count.get(&k).is_some_and(|&n| n > 1) || taken.contains(&k);
        let dst = match c.disc {
            Some(n) if collides => {
                let mut name = OsString::from(format!("Disc {} - ", disc_label(n, p.disc_count)));
                name.push(c.dst.file_name().unwrap_or_default());
                fit_name(&c.dst.with_file_name(name))
            }
            _ => c.dst.clone(),
        };
        let free = |d: &PathBuf| {
            let k = paths::key(d);
            !taken.contains(&k) && !used.contains(&k)
        };
        let dst = if free(&dst) {
            dst
        } else {
            (2u32..)
                .map(|n| numbered(&dst, n))
                .find(free)
                .unwrap_or(dst)
        };
        used.insert(paths::key(&dst));
        dsts[i] = Some(dst);
    }
    side.iter()
        .cloned()
        .zip(dsts.into_iter().map(Option::unwrap_or_default))
        .collect()
}

/// Rules 1–3 for one side file (before the uniqueness pass), sanitized.
fn candidate(p: &AlbumPlacement<'_>, src: &Path) -> Candidate {
    if let Some(ext) = src.extension()
        && let Some(track_dst) = p.stems.get(&paths::key(&src.with_extension("")))
    {
        return Candidate {
            dst: sanitize_below(p.dir, track_dst.with_extension(ext)),
            disc: None,
            followed: true,
        };
    }
    let rel = src.strip_prefix(p.root).unwrap_or(src);
    let mut comps = rel.components();
    let first = comps.next();
    let rest = comps.as_path();
    let disc = match first {
        Some(c) if !rest.as_os_str().is_empty() => c
            .as_os_str()
            .to_str()
            .and_then(disc_folder::parse)
            // `CD0`'s tracks fall back to disc 1, so its files go with disc 1.
            .map(|d| u32::from(d.number).max(1)),
        _ => None,
    };
    let (dst, disc) = match disc {
        Some(n) if p.disc_dirs => (p.dir.join(disc_dir_name(n, p.disc_count)).join(rest), None),
        Some(n) => (p.dir.join(rest), Some(n)),
        None => (p.dir.join(rel), None),
    };
    Candidate {
        dst: sanitize_below(p.dir, dst),
        disc,
        followed: false,
    }
}

/// `dst` with every UTF-8 component below `dir` sanitized (folders like a
/// template segment, the file name keeping its extension) and fitted to
/// [`MAX_NAME_BYTES`]. A path not below `dir` is returned unchanged.
fn sanitize_below(dir: &Path, dst: PathBuf) -> PathBuf {
    let Ok(rel) = dst.strip_prefix(dir) else {
        return dst;
    };
    let mut out = dir.to_path_buf();
    let mut comps = rel.components().peekable();
    while let Some(c) = comps.next() {
        let is_file_name = comps.peek().is_none();
        match c {
            Component::Normal(os) => match os.to_str() {
                Some(s) if is_file_name => out.push(fit(&sanitize_file_name(s), "")),
                Some(s) => out.push(fit(&sanitize_segment(s), "")),
                None => out.push(os),
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `path` with its (UTF-8) file name fitted to [`MAX_NAME_BYTES`].
fn fit_name(path: &Path) -> PathBuf {
    match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => path.with_file_name(fit(name, "")),
        None => path.to_path_buf(),
    }
}

/// `name` with `suffix` inserted before its extension, trimming the stem so
/// the result fits [`MAX_NAME_BYTES`] (the extension is kept whole unless it
/// alone is too long).
fn fit(name: &str, suffix: &str) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], Some(&name[i + 1..])),
        _ => (name, None),
    };
    let tail = suffix.len() + ext.map_or(0, |e| e.len() + 1);
    let mut stem = stem;
    if stem.len() + tail > MAX_NAME_BYTES {
        stem = floor_str(stem, MAX_NAME_BYTES.saturating_sub(tail)).trim_end_matches([' ', '.']);
        if stem.is_empty() {
            stem = "_";
        }
    }
    let mut out = format!("{stem}{suffix}");
    if let Some(e) = ext {
        out.push('.');
        out.push_str(e);
    }
    if out.len() > MAX_NAME_BYTES {
        out = floor_str(&out, MAX_NAME_BYTES).to_owned();
    }
    out
}

/// The longest prefix of `s` of at most `max` bytes, on a char boundary.
fn floor_str(s: &str, max: usize) -> &str {
    let mut cut = max.min(s.len());
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

/// `dir/name (n).ext` for `dir/name.ext` (`dir/name (n)` without an extension),
/// within [`MAX_NAME_BYTES`].
fn numbered(path: &Path, n: u32) -> PathBuf {
    let suffix = format!(" ({n})");
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        return path.with_file_name(fit(name, &suffix));
    }
    let mut name = path.file_stem().unwrap_or_default().to_os_string();
    name.push(suffix);
    if let Some(ext) = path.extension() {
        name.push(".");
        name.push(ext);
    }
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::{AlbumPlacement, MAX_NAME_BYTES, is_audio, is_junk, numbered, place};
    use crate::paths::key;
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};

    #[test]
    fn audio_and_junk_classification() {
        assert!(is_audio(Path::new("a/01.FLAC")));
        assert!(
            is_audio(Path::new("a/01.wma")),
            "undecodable audio is still audio"
        );
        assert!(!is_audio(Path::new("a/Box.cue")));
        assert!(is_junk("Thumbs.db") && is_junk("DESKTOP.INI") && is_junk(".DS_Store"));
        assert!(!is_junk("cover.jpg"));
    }

    /// `stems` keyed the way the planner keys them.
    fn stems(pairs: &[(&str, &str)]) -> HashMap<PathBuf, PathBuf> {
        pairs
            .iter()
            .map(|(s, d)| (key(Path::new(s)), PathBuf::from(d)))
            .collect()
    }

    #[test]
    fn placement_rules_in_both_layouts() {
        let stems = stems(&[("s/Box/CD1/01 Intro", "d/Box/1-01 - Intro.flac")]);
        let side: Vec<PathBuf> = [
            "s/Box/CD1/01 Intro.lrc",
            "s/Box/CD1/Box.log",
            "s/Box/CD2/Box.log",
            "s/Box/CD2/notes.txt",
            "s/Box/Scans/Front.jpg",
            "s/Box/cover.jpg",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect();
        let flat = AlbumPlacement {
            root: Path::new("s/Box"),
            dir: Path::new("d/Box"),
            disc_dirs: false,
            disc_count: 2,
            stems: &stems,
        };
        let got: Vec<PathBuf> = place(&flat, &side, &HashSet::new())
            .into_iter()
            .map(|(_, d)| d)
            .collect();
        assert_eq!(
            got,
            [
                "d/Box/1-01 - Intro.lrc",
                "d/Box/Disc 1 - Box.log",
                "d/Box/Disc 2 - Box.log",
                "d/Box/notes.txt",
                "d/Box/Scans/Front.jpg",
                "d/Box/cover.jpg",
            ]
            .map(PathBuf::from)
        );
        let folders = AlbumPlacement {
            disc_dirs: true,
            ..flat
        };
        let got: Vec<PathBuf> = place(&folders, &side, &HashSet::new())
            .into_iter()
            .map(|(_, d)| d)
            .collect();
        assert_eq!(
            got,
            [
                "d/Box/1-01 - Intro.lrc",
                "d/Box/Disc 1/Box.log",
                "d/Box/Disc 2/Box.log",
                "d/Box/Disc 2/notes.txt",
                "d/Box/Scans/Front.jpg",
                "d/Box/cover.jpg",
            ]
            .map(PathBuf::from)
        );
    }

    fn dsts(p: &AlbumPlacement<'_>, side: &[&str], taken: &[&str]) -> Vec<PathBuf> {
        let side: Vec<PathBuf> = side.iter().map(PathBuf::from).collect();
        let taken: HashSet<PathBuf> = taken.iter().map(|t| key(Path::new(t))).collect();
        place(p, &side, &taken)
            .into_iter()
            .map(|(_, d)| d)
            .collect()
    }

    fn flat_box(stems: &HashMap<PathBuf, PathBuf>) -> AlbumPlacement<'_> {
        AlbumPlacement {
            root: Path::new("s/Box"),
            dir: Path::new("d/Box"),
            disc_dirs: false,
            disc_count: 2,
            stems,
        }
    }

    #[test]
    fn distinct_side_files_never_share_a_destination() {
        let stems = stems(&[("s/Box/101 Intro", "d/Box/1-01 - Intro.flac")]);
        let flat = flat_box(&stems);
        // A stem-followed lyric wins its track's name; the unrelated file that
        // already carried that name is numbered instead.
        assert_eq!(
            dsts(
                &flat,
                &["s/Box/1-01 - Intro.lrc", "s/Box/101 Intro.lrc"],
                &[]
            ),
            ["d/Box/1-01 - Intro (2).lrc", "d/Box/1-01 - Intro.lrc"].map(PathBuf::from)
        );
        // The `Disc N - ` prefix lands on a root file of that very name.
        assert_eq!(
            dsts(
                &flat,
                &[
                    "s/Box/CD1/Box.log",
                    "s/Box/CD2/Box.log",
                    "s/Box/Disc 1 - Box.log"
                ],
                &[]
            ),
            [
                "d/Box/Disc 1 - Box.log",
                "d/Box/Disc 2 - Box.log",
                "d/Box/Disc 1 - Box (2).log"
            ]
            .map(PathBuf::from)
        );
        // A plain side file on a path the batch already claims (an audio dst).
        assert_eq!(
            dsts(
                &flat,
                &["s/Box/README"],
                &["d/Box/README", "d/Box/README (2)"]
            ),
            ["d/Box/README (3)"].map(PathBuf::from)
        );
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn names_differing_only_by_case_collide_on_case_insensitive_hosts() {
        let stems = HashMap::new();
        let flat = flat_box(&stems);
        assert_eq!(
            dsts(&flat, &["s/Box/CD1/cover.jpg", "s/Box/Cover.jpg"], &[]),
            ["d/Box/Disc 1 - cover.jpg", "d/Box/Cover.jpg"].map(PathBuf::from)
        );
        assert_eq!(
            dsts(&flat, &["s/Box/readme.txt"], &["d/box/README.TXT"]),
            ["d/Box/readme (2).txt"].map(PathBuf::from)
        );
    }

    #[test]
    fn names_are_sanitized_and_fit_255_bytes() {
        let stems = HashMap::new();
        let flat = flat_box(&stems);
        assert_eq!(
            dsts(&flat, &["s/Box/Scans?/a:b?.txt", "s/Box/con.txt"], &[]),
            ["d/Box/Scans_/a_b_.txt", "d/Box/con_.txt"].map(PathBuf::from)
        );
        let long = format!("s/Box/{}.log", "é".repeat(200));
        let got = dsts(&flat, &[long.as_str(), long.as_str()], &[]);
        for (d, tail) in got.iter().zip([".log", " (2).log"]) {
            let name = d.file_name().unwrap().to_str().unwrap();
            assert!(name.len() <= MAX_NAME_BYTES, "{} bytes", name.len());
            assert!(name.ends_with(tail), "{name}");
            assert!(name.starts_with("éé"));
        }
        assert_ne!(got[0], got[1]);
    }

    #[test]
    fn numbered_goes_before_the_extension() {
        assert_eq!(
            numbered(Path::new("d/a.b.log"), 2),
            PathBuf::from("d/a.b (2).log")
        );
        assert_eq!(
            numbered(Path::new("d/README"), 3),
            PathBuf::from("d/README (3)")
        );
    }
}
