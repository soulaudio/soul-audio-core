//! Local music library: a recursive folder scan that reads tags with `lofty`.
//! Track ids are the scan order (stable for the session); the loader maps
//! `id → path` when opening a track.

use std::path::{Path, PathBuf};

use crate::types::TrackView;

/// One scanned track. `id` is its index in `Library::tracks`.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub id: u32,
    pub path: PathBuf,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_secs: f64,
}

/// An ordered set of tracks with stable session ids.
#[derive(Debug, Clone, Default)]
pub struct Library {
    pub tracks: Vec<Track>,
}

/// Extensions the decoders support (lower-cased comparison).
const SUPPORTED: &[&str] = &["wav", "flac", "mp3", "m4a", "aac", "ogg", "dsf", "dff"];

/// Hard cap on folder nesting: a symlink cycle or pathological tree cannot
/// recurse past this many levels (belt-and-braces on top of the visited-set).
const MAX_DEPTH: usize = 64;

fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| SUPPORTED.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Recursively collect supported files under `dir` into `out`, sorted for a
/// deterministic id assignment.
///
/// Robustness policy:
/// - **Symlink cycles / duplicates**: directory targets are canonicalized into
///   a visited set, so a symlink loop (or the same tree reachable twice) is
///   traversed once, not infinitely.
/// - **Unreadable subtrees**: a `read_dir` failure below the root is skipped
///   with a warning instead of failing the whole scan (only the root's failure
///   propagates to [`Library::scan`]'s caller).
/// - **Depth**: recursion stops at [`MAX_DEPTH`] as a last-resort guard.
fn walk(
    dir: &Path,
    out: &mut Vec<PathBuf>,
    visited: &mut std::collections::HashSet<PathBuf>,
    depth: usize,
) -> std::io::Result<()> {
    if depth >= MAX_DEPTH {
        eprintln!(
            "scan: max depth {MAX_DEPTH} reached at {:?}; skipping deeper files",
            dir
        );
        return Ok(());
    }
    // Cycle guard: canonicalize the dir we are about to descend into. A
    // symlink back to an ancestor canonicalizes to an already-visited path,
    // so the recursion stops instead of looping.
    let canon = std::fs::canonicalize(dir)?;
    if !visited.insert(canon) {
        return Ok(()); // already traversed (symlink cycle / duplicate target)
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            // An unreadable subdirectory must not sink the whole scan: warn and
            // keep going. Only the ROOT's own read failure propagates (the `?`
            // above), so `scan` still errors when the requested folder itself is
            // unreadable.
            if let Err(e) = walk(&path, out, visited, depth + 1) {
                eprintln!("scan: skipping unreadable dir {:?}: {e}", path);
            }
        } else if is_supported(&path) {
            out.push(path);
        }
    }
    Ok(())
}

impl Library {
    /// Scan `root` recursively for supported audio files, reading tags. Runs off
    /// the UI thread by the caller. Robustness: unreadable tags degrade to
    /// `None`/`0.0` (never fail the scan), an unreadable *subdirectory* is
    /// skipped with a warning, symlink cycles terminate, and only an unreadable
    /// `root` itself errors.
    pub fn scan(root: &Path) -> anyhow::Result<Library> {
        let mut paths = Vec::new();
        let mut visited = std::collections::HashSet::new();
        walk(root, &mut paths, &mut visited, 0)?;
        let tracks = paths
            .into_iter()
            .enumerate()
            .map(|(i, path)| {
                let (title, artist, album, duration_secs) = read_tags(&path);
                Track {
                    id: i as u32,
                    path,
                    title,
                    artist,
                    album,
                    duration_secs,
                }
            })
            .collect();
        Ok(Library { tracks })
    }

    /// The file path for `id`, if in range.
    #[must_use]
    pub fn path_of(&self, id: u32) -> Option<&Path> {
        self.tracks.get(id as usize).map(|t| t.path.as_path())
    }

    /// The UI-facing view of every track (no paths).
    #[must_use]
    pub fn views(&self) -> Vec<TrackView> {
        self.tracks
            .iter()
            .map(|t| TrackView {
                id: t.id,
                title: t.title.clone(),
                artist: t.artist.clone(),
                album: t.album.clone(),
                duration_secs: t.duration_secs,
            })
            .collect()
    }
}

/// Read (title, artist, album, duration_secs) with lofty; all-`None`/`0.0` on
/// any read error (a corrupt tag must not sink the scan).
fn read_tags(path: &Path) -> (Option<String>, Option<String>, Option<String>, f64) {
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::tag::Accessor;

    match lofty::read_from_path(path) {
        Ok(tagged) => {
            let duration_secs = tagged.properties().duration().as_secs_f64();
            let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
            let s = |f: Option<std::borrow::Cow<'_, str>>| f.map(|c| c.into_owned());
            match tag {
                Some(t) => (s(t.title()), s(t.artist()), s(t.album()), duration_secs),
                None => (None, None, None, duration_secs),
            }
        }
        Err(_) => (None, None, None, 0.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid PCM WAV of `frames` stereo frames at `rate` (silence).
    fn wav(rate: u32, frames: u32) -> Vec<u8> {
        let channels: u16 = 2;
        let bits: u16 = 16;
        let block_align = channels * bits / 8;
        let byte_rate = rate * u32::from(block_align);
        let data_len = frames * u32::from(block_align);
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data_len).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&byte_rate.to_le_bytes());
        v.extend_from_slice(&block_align.to_le_bytes());
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        v.resize(v.len() + data_len as usize, 0);
        v
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sas_lib_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn scan_finds_supported_files_skips_others_and_assigns_index_ids() {
        let root = tmpdir("scan");
        std::fs::write(root.join("b.wav"), wav(48_000, 48_000)).unwrap(); // 1.0 s
        std::fs::write(root.join("a.wav"), wav(48_000, 24_000)).unwrap(); // 0.5 s
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/c.wav"), wav(48_000, 48_000)).unwrap();
        std::fs::write(root.join("notes.txt"), b"not audio").unwrap();

        let lib = Library::scan(&root).unwrap();
        // Sorted walk: a.wav, b.wav, sub/c.wav — .txt skipped.
        assert_eq!(lib.tracks.len(), 3);
        assert_eq!(lib.tracks[0].id, 0);
        assert!(lib.tracks[0].path.ends_with("a.wav"));
        assert_eq!(lib.tracks[2].id, 2);
        assert!(lib.tracks[2].path.ends_with("c.wav"));
        assert_eq!(lib.path_of(1), Some(lib.tracks[1].path.as_path()));
        assert_eq!(lib.path_of(9), None);
    }

    #[test]
    fn scan_reads_duration() {
        let root = tmpdir("dur");
        std::fs::write(root.join("x.wav"), wav(48_000, 48_000)).unwrap(); // 1.0 s
        let lib = Library::scan(&root).unwrap();
        assert_eq!(lib.tracks.len(), 1);
        assert!(
            (lib.tracks[0].duration_secs - 1.0).abs() < 0.05,
            "duration {} not ~1.0 s",
            lib.tracks[0].duration_secs
        );
    }

    #[test]
    fn views_match_tracks_without_paths() {
        let root = tmpdir("views");
        std::fs::write(root.join("x.wav"), wav(48_000, 24_000)).unwrap();
        let lib = Library::scan(&root).unwrap();
        let views = lib.views();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].id, lib.tracks[0].id);
        assert_eq!(views[0].duration_secs, lib.tracks[0].duration_secs);
    }
    /// Path to a committed `testdata/` fixture (relative to the workspace).
    fn fixture(name: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata")
            .join(name)
    }

    #[test]
    fn scan_reads_tags_from_a_tagged_file() {
        // Exercises the `Some(tag)` accessor path (title/artist/album) via the
        // committed `tagged.flac` fixture.
        let root = tmpdir("tags");
        std::fs::copy(fixture("tagged.flac"), root.join("t.flac")).unwrap();
        let lib = Library::scan(&root).unwrap();
        assert_eq!(lib.tracks.len(), 1);
        let t = &lib.tracks[0];
        assert_eq!(t.title.as_deref(), Some("Soul Test Track"));
        assert_eq!(t.artist.as_deref(), Some("Test Artist"));
        assert_eq!(t.album.as_deref(), Some("Test Album"));
        assert!(
            t.duration_secs > 0.0,
            "tagged fixture has a real duration, got {}",
            t.duration_secs
        );
    }

    #[test]
    fn scan_survives_a_corrupt_supported_file() {
        // A file with a supported extension but garbage bytes degrades to an
        // empty track rather than failing the whole scan.
        let root = tmpdir("corrupt");
        std::fs::write(root.join("ok.wav"), wav(48_000, 48_000)).unwrap(); // 1.0 s
        std::fs::write(root.join("bad.flac"), b"this is not a flac").unwrap();
        let lib = Library::scan(&root).unwrap();
        assert_eq!(lib.tracks.len(), 2, "corrupt file still listed");
        let bad = lib
            .tracks
            .iter()
            .find(|t| t.path.ends_with("bad.flac"))
            .unwrap();
        assert_eq!(bad.title, None);
        assert_eq!(bad.artist, None);
        assert_eq!(bad.album, None);
        assert_eq!(bad.duration_secs, 0.0);
        let ok = lib
            .tracks
            .iter()
            .find(|t| t.path.ends_with("ok.wav"))
            .unwrap();
        assert!((ok.duration_secs - 1.0).abs() < 0.05);
    }

    #[test]
    fn scan_errors_when_root_is_missing() {
        // Only the ROOT read failure propagates (subdir failures are skipped).
        let missing = std::env::temp_dir().join("sas_lib_definitely_missing");
        let _ = std::fs::remove_dir_all(&missing);
        assert!(Library::scan(&missing).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn scan_skips_unreadable_subdir_instead_of_failing() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmpdir("deny");
        std::fs::write(root.join("a.wav"), wav(48_000, 48_000)).unwrap();
        std::fs::create_dir(root.join("locked")).unwrap();
        std::fs::write(root.join("locked/b.wav"), wav(48_000, 48_000)).unwrap();
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let lib = Library::scan(&root).unwrap(); // must not error
        assert_eq!(lib.tracks.len(), 1);
        assert!(lib.tracks[0].path.ends_with("a.wav"));
        // Restore so the temp dir can be removed.
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn scan_stops_at_a_symlink_cycle() {
        use std::os::unix::fs::symlink;
        let root = tmpdir("cycle");
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/a.wav"), wav(48_000, 48_000)).unwrap();
        // sub/loop -> root: a cycle. Without the canonical visited-set the
        // walk would recurse forever; it must terminate with just the one file.
        symlink(&root, root.join("sub/loop")).unwrap();
        let lib = Library::scan(&root).unwrap();
        assert_eq!(lib.tracks.len(), 1);
        assert!(lib.tracks[0].path.ends_with("a.wav"));
    }

    #[test]
    fn scan_stops_at_max_depth() {
        // Belt-and-braces guard: pathological nesting cannot recurse past
        // MAX_DEPTH even without symlinks.
        let root = tmpdir("deep");
        let mut cur = root.clone();
        for _ in 0..(MAX_DEPTH + 8) {
            cur = cur.join("d");
            std::fs::create_dir(&cur).unwrap();
        }
        std::fs::write(cur.join("deep.wav"), wav(48_000, 48_000)).unwrap();
        let lib = Library::scan(&root).unwrap(); // must terminate
        assert!(
            lib.tracks.is_empty(),
            "file beyond MAX_DEPTH is not scanned"
        );
        // And a file just within the cap is still found.
        let root2 = tmpdir("deep2");
        let mut cur2 = root2.clone();
        for _ in 0..(MAX_DEPTH - 2) {
            cur2 = cur2.join("d");
            std::fs::create_dir(&cur2).unwrap();
        }
        std::fs::write(cur2.join("ok.wav"), wav(48_000, 48_000)).unwrap();
        let lib2 = Library::scan(&root2).unwrap();
        assert_eq!(lib2.tracks.len(), 1, "file within MAX_DEPTH is scanned");
    }
}
