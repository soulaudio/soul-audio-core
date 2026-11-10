//! Per-album cover-art discovery (desktop-only; the `.soulidx` baker never
//! looks at art — it just stores whatever bytes the importer hands it).
//!
//! Ported from `soul-player-ng`'s `art` module, with one deliberate deviation
//! for the desktop importer's per-*album* call site (rather than per-*track*):
//! "largest file wins" rather than "smallest wins" (we want the best-looking
//! cover, not the smallest front scan).
//!
//! Resolution order: an embedded front-cover picture on a representative
//! track of the album, else the best sidecar image in the album's own folder,
//! else the largest `Front*` scan in an album-local `Scans/` subfolder (the
//! EAC/dBpoweramp full-scan convention, matched case-insensitively), else the
//! best sidecar image in that folder's immediate parent — but only when that
//! parent is still inside the scanned library root; tracks sitting directly in
//! the library root must never cause a read outside it.

use std::path::{Path, PathBuf};

use soul_audio_tags::{PictureKind, read_owned, read_picture_bytes};

/// Sidecar stems we recognize (lower-cased, exact match only).
const COVER_STEMS: &[&str] = &["cover", "folder", "front", "album", "artwork"];
/// Image extensions we accept as cover art (lower-cased comparison).
const COVER_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp"];

/// Resolve one cover for an album: the embedded front-cover picture of
/// `a_track_path` (any one track belonging to the album) if present, else the
/// best sidecar image found in `album_dir` or (failing that) its immediate
/// parent folder, as long as that parent is still inside `root`. `None` when
/// neither source yields one, or a read fails — cover art is best-effort and
/// never fails the surrounding scan.
#[must_use]
pub(crate) fn find_album_cover(
    album_dir: &Path,
    a_track_path: &Path,
    root: &Path,
) -> Option<Vec<u8>> {
    embedded_cover(a_track_path).or_else(|| sidecar_cover(album_dir, root))
}

/// Read the representative track's embedded front-cover picture, falling back
/// to the first picture of any kind when none is explicitly tagged as the
/// front cover.
fn embedded_cover(track_path: &Path) -> Option<Vec<u8>> {
    let file = std::fs::File::open(track_path).ok()?;
    let mut src = embedded_io_adapters::std::FromStd::new(file);
    let (tags, _facts) = read_owned(&mut src).ok()?;
    let pic = tags
        .pictures
        .iter()
        .find(|p| matches!(p.usage, PictureKind::FrontCover))
        .or_else(|| tags.pictures.first())?;
    let mut src2 = embedded_io_adapters::std::FromStd::new(std::fs::File::open(track_path).ok()?);
    read_picture_bytes(&mut src2, pic).ok()
}

/// Best sidecar image for an album folder: its own directory first, then an
/// album-local `Scans/Front*` scan, then (only if those yield nothing) its
/// immediate parent — but only when the parent is still inside `root`. An
/// album folder sitting directly at `root` (or outside it) never falls back to
/// a read above the library.
fn sidecar_cover(album_dir: &Path, root: &Path) -> Option<Vec<u8>> {
    best_in_dir(album_dir)
        .or_else(|| scans_front_cover(album_dir))
        .or_else(|| {
            let parent = album_dir.parent()?;
            if parent.starts_with(root) {
                best_in_dir(parent)
            } else {
                None
            }
        })
}

/// The largest `Front*` image inside an album-local `Scans/` subfolder (matched
/// case-insensitively, so `Scans`/`scans`/`SCANS` all count). This honors the
/// full-scan rip convention where the cover lives at `<album>/Scans/Front.jpg`
/// or `Front-01.jpg`. Only `front`-stemmed files qualify, so booklet/back/disc
/// scans in the same folder are never mistaken for the cover. A `Scans/` folder
/// is always album-local, so the parent is never consulted here.
fn scans_front_cover(album_dir: &Path) -> Option<Vec<u8>> {
    let scans = std::fs::read_dir(album_dir).ok()?.flatten().find_map(|e| {
        let p = e.path();
        let name = p.file_name()?.to_str()?;
        (p.is_dir() && name.eq_ignore_ascii_case("scans")).then_some(p)
    })?;
    best_in_dir_matching(&scans, |stem, ext| {
        stem.starts_with("front") && COVER_EXTS.contains(&ext)
    })
}

/// The largest cover-named image directly inside `dir`, read to bytes.
/// `None` if the directory can't be read or holds no recognized sidecar.
fn best_in_dir(dir: &Path) -> Option<Vec<u8>> {
    best_in_dir_matching(dir, |stem, ext| {
        COVER_STEMS.contains(&stem) && COVER_EXTS.contains(&ext)
    })
}

/// The largest image directly inside `dir` whose lower-cased `(stem, ext)` pass
/// `matches`, read to bytes. `None` if the directory can't be read or nothing
/// matches. Hidden `.`-prefixed entries (macOS `._*` AppleDouble sidecars) and
/// subdirectories are skipped.
fn best_in_dir_matching(dir: &Path, matches: impl Fn(&str, &str) -> bool) -> Option<Vec<u8>> {
    let rd = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in rd.flatten() {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue; // macOS `._*` AppleDouble sidecars
        }
        let lower = name.to_ascii_lowercase();
        let Some((stem, ext)) = lower.rsplit_once('.') else {
            continue;
        };
        if !matches(stem, ext) {
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        if best.as_ref().is_none_or(|(best_size, _)| size > *best_size) {
            best = Some((size, p));
        }
    }
    best.and_then(|(_, p)| std::fs::read(p).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sai_cover_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn sidecar_in_own_folder_wins_over_parent() {
        let parent = tmpdir("own_vs_parent");
        std::fs::write(parent.join("folder.jpg"), vec![0u8; 10]).unwrap();
        let album = parent.join("Album");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("cover.png"), vec![1u8; 5]).unwrap();

        let got = sidecar_cover(&album, &parent).unwrap();
        assert_eq!(got, vec![1u8; 5]);
    }

    #[test]
    fn sidecar_falls_back_to_parent_when_own_folder_empty() {
        let parent = tmpdir("parent_fallback");
        std::fs::write(parent.join("folder.jpg"), vec![2u8; 7]).unwrap();
        let album = parent.join("Disc 1");
        std::fs::create_dir_all(&album).unwrap();

        let got = sidecar_cover(&album, &parent).unwrap();
        assert_eq!(got, vec![2u8; 7]);
    }

    #[test]
    fn sidecar_does_not_fall_back_above_scan_root() {
        // `album` sits directly at the scan root: its parent (`base`) is
        // OUTSIDE the selected library, so the fallback must not read from
        // it even though it holds a matching sidecar image.
        let base = tmpdir("root_bound");
        std::fs::write(base.join("folder.jpg"), vec![3u8; 11]).unwrap();
        let root = base.join("Library");
        std::fs::create_dir_all(&root).unwrap();
        let album = root.clone();

        assert_eq!(sidecar_cover(&album, &root), None);
    }

    #[test]
    fn sidecar_picks_largest_match() {
        let album = tmpdir("largest");
        std::fs::write(album.join("front.jpg"), vec![0u8; 100]).unwrap();
        std::fs::write(album.join("cover.jpg"), vec![0u8; 500]).unwrap();
        std::fs::write(album.join("artwork.png"), vec![0u8; 50]).unwrap();

        let got = sidecar_cover(&album, &album).unwrap();
        assert_eq!(got.len(), 500);
    }

    #[test]
    fn sidecar_skips_apple_double_and_unrecognized_names() {
        let album = tmpdir("skip_junk");
        std::fs::write(album.join("._cover.jpg"), vec![0u8; 9999]).unwrap();
        std::fs::write(album.join("notes.txt"), b"hi").unwrap();
        std::fs::write(album.join("back.jpg"), vec![0u8; 9999]).unwrap();

        assert_eq!(sidecar_cover(&album, &album), None);
    }

    #[test]
    fn none_when_nothing_present() {
        let album = tmpdir("none");
        assert_eq!(
            find_album_cover(&album, &album.join("missing.flac"), &album),
            None
        );
    }

    #[test]
    fn scans_front_found_when_no_folder_sidecar() {
        let album = tmpdir("scans_only");
        let scans = album.join("Scans");
        std::fs::create_dir_all(&scans).unwrap();
        std::fs::write(scans.join("Front-01.jpg"), vec![7u8; 42]).unwrap();

        let got = sidecar_cover(&album, &album).unwrap();
        assert_eq!(got, vec![7u8; 42]);
    }

    #[test]
    fn folder_sidecar_wins_over_scans_front() {
        let album = tmpdir("folder_vs_scans");
        std::fs::write(album.join("cover.jpg"), vec![1u8; 8]).unwrap();
        let scans = album.join("Scans");
        std::fs::create_dir_all(&scans).unwrap();
        std::fs::write(scans.join("Front.jpg"), vec![9u8; 9999]).unwrap();

        // The curated album cover wins over the raw Scans front, even though the
        // scan is far larger.
        let got = sidecar_cover(&album, &album).unwrap();
        assert_eq!(got, vec![1u8; 8]);
    }

    #[test]
    fn scans_front_wins_over_parent_sidecar() {
        let parent = tmpdir("scans_vs_parent");
        std::fs::write(parent.join("folder.jpg"), vec![3u8; 50]).unwrap();
        let album = parent.join("Disc 1");
        std::fs::create_dir_all(&album).unwrap();
        let scans = album.join("Scans");
        std::fs::create_dir_all(&scans).unwrap();
        std::fs::write(scans.join("Front.png"), vec![4u8; 20]).unwrap();

        // The album-local scan is more specific than the shared parent cover.
        let got = sidecar_cover(&album, &parent).unwrap();
        assert_eq!(got, vec![4u8; 20]);
    }

    #[test]
    fn scans_ignores_non_front_scans() {
        let album = tmpdir("scans_no_front");
        let scans = album.join("Scans");
        std::fs::create_dir_all(&scans).unwrap();
        std::fs::write(scans.join("Booklet-01.jpg"), vec![0u8; 9999]).unwrap();
        std::fs::write(scans.join("Back.jpg"), vec![0u8; 9999]).unwrap();

        // No `Front*` in Scans and nothing in the album folder → no cover.
        assert_eq!(sidecar_cover(&album, &album), None);
    }

    #[test]
    fn scans_subdir_matched_case_insensitively() {
        let album = tmpdir("scans_lower");
        let scans = album.join("scans"); // lower-case
        std::fs::create_dir_all(&scans).unwrap();
        std::fs::write(scans.join("front.jpeg"), vec![5u8; 13]).unwrap();

        let got = sidecar_cover(&album, &album).unwrap();
        assert_eq!(got, vec![5u8; 13]);
    }

    #[test]
    fn scans_picks_largest_front() {
        let album = tmpdir("scans_largest");
        let scans = album.join("Scans");
        std::fs::create_dir_all(&scans).unwrap();
        std::fs::write(scans.join("Front-01.jpg"), vec![0u8; 100]).unwrap();
        std::fs::write(scans.join("Front-02.jpg"), vec![0u8; 800]).unwrap();

        let got = sidecar_cover(&album, &album).unwrap();
        assert_eq!(got.len(), 800);
    }
}
