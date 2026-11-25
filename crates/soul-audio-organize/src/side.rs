//! Side files: the non-audio files that travel with an album — rip logs and cue
//! sheets, covers, `Scans/`, lyrics, `.txt`/`.nfo`. Discovery walks the album
//! root; placement decides each file's destination in the album's new folder.
//! Side files are never opened or modified, only copied/moved (a `.log`'s
//! checksum must stay valid). `.cue` `FILE` lines are NOT rewritten when tracks
//! are renamed (documented limitation).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use soul_audio_tags::disc_folder;

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

/// Every side file under `album_root`, recursively, sorted. Skips junk/hidden
/// entries, audio files, symlinks, and the `skip` directory (the destination
/// root, when it sits inside a source album root).
pub(crate) fn collect(album_root: &Path, skip: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, skip: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if depth > MAX_DEPTH || dir == skip {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if is_junk(name) {
                continue;
            }
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_dir() {
                walk(&path, skip, depth + 1, out);
            } else if ft.is_file() && !is_audio(&path) {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(album_root, skip, 0, &mut out);
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
    /// Source track path without extension → that track's planned destination.
    pub(crate) stems: &'a HashMap<PathBuf, PathBuf>,
}

/// Plan a destination for each side file, in `side`'s order:
/// 1. same stem as a track in the same folder (`01 Liyue.lrc`) → the track's
///    new name with the side file's own extension (keeps lyrics attached);
/// 2. inside a disc folder → `<dir>/Disc N/<rest>` with per-disc folders, else
///    `<dir>/<rest>`, prefixed `Disc N - ` only when that destination collides
///    (with another side file or with a `taken` path);
/// 3. anything else → `<dir>/<path relative to the album root>`.
pub(crate) fn place(
    p: &AlbumPlacement<'_>,
    side: &[PathBuf],
    taken: &HashSet<PathBuf>,
) -> Vec<(PathBuf, PathBuf)> {
    // Pass 1: candidate destinations (+ the disc number for rule-2 flat files).
    let mut cands: Vec<(PathBuf, PathBuf, Option<u32>)> = Vec::with_capacity(side.len());
    for src in side {
        if let Some(ext) = src.extension()
            && let Some(track_dst) = p.stems.get(&src.with_extension(""))
        {
            cands.push((src.clone(), track_dst.with_extension(ext), None));
            continue;
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
        let cand = match disc {
            Some(n) if p.disc_dirs => (
                src.clone(),
                p.dir.join(disc_dir_name(n, p.disc_count)).join(rest),
                None,
            ),
            Some(n) => (src.clone(), p.dir.join(rest), Some(n)),
            None => (src.clone(), p.dir.join(rel), None),
        };
        cands.push(cand);
    }
    // Pass 2: a disc-folder file whose flat destination collides gets `Disc N - `.
    let mut count: HashMap<PathBuf, usize> = HashMap::new();
    for (_, dst, _) in &cands {
        *count.entry(dst.clone()).or_insert(0) += 1;
    }
    cands
        .into_iter()
        .map(|(src, dst, disc)| {
            let collides = count.get(&dst).is_some_and(|&n| n > 1) || taken.contains(&dst);
            match disc {
                Some(n) if collides => {
                    let name = dst
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let renamed = dst
                        .with_file_name(format!("Disc {} - {name}", disc_label(n, p.disc_count)));
                    (src, renamed)
                }
                _ => (src, dst),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{AlbumPlacement, is_audio, is_junk, place};
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

    #[test]
    fn placement_rules_in_both_layouts() {
        let stems = HashMap::from([(
            PathBuf::from("s/Box/CD1/01 Intro"),
            PathBuf::from("d/Box/1-01 - Intro.flac"),
        )]);
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
}
