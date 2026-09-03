//! Cover-art resolution for a track: embedded first, then an external image
//! file next to the track (or in a `Scans/`-style sibling folder).
//!
//! Many rips — DSD/SACD in particular — carry only text tags and keep the
//! artwork as separate JPEGs (e.g. `Scans/Front-0.jpg`), so a player that only
//! reads embedded `APIC`/PICTURE blocks shows nothing. This module adds the
//! conventional external-file fallback.

use std::path::{Path, PathBuf};

/// An owned cover image ready to hand to the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cover {
    /// e.g. `"image/jpeg"` / `"image/png"`.
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// Cover-image file stems we recognize, **highest priority first**. An exact
/// stem match beats a `front*` prefix match (see [`external_cover`]).
const COVER_STEMS: &[&str] = &["cover", "folder", "front", "albumart", "album", "artwork"];
/// Image extensions we accept as cover art (lower-cased comparison).
const COVER_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "bmp"];
/// Sibling subdirectories that commonly hold scans/art (lower-cased).
const ART_SUBDIRS: &[&str] = &["scans", "scan", "artwork", "art", "covers", "cover"];
/// Never load an external image larger than this (avoid a multi-hundred-MB scan
/// blowing up memory / IPC). Front covers are comfortably under it.
pub const MAX_ART_BYTES: u64 = 40 * 1024 * 1024;

/// Resolve a track's cover: embedded art if present, else an external file.
/// `None` when neither exists (or a read fails — art is best-effort).
#[must_use]
pub fn cover_bytes(track_path: &Path) -> Option<Cover> {
    embedded_cover(track_path).or_else(|| external_cover(track_path).and_then(read_external))
}

/// Read the track's embedded front-cover (or first picture) via `soul-audio-tags`.
fn embedded_cover(track_path: &Path) -> Option<Cover> {
    use soul_audio_tags::{MimeType, PictureKind, read_owned, read_picture_bytes};

    let file = std::fs::File::open(track_path).ok()?;
    let mut src = embedded_io_adapters::std::FromStd::new(file);
    let (tags, _facts) = read_owned(&mut src).ok()?;
    let pic = tags
        .pictures
        .iter()
        .find(|p| matches!(p.usage, PictureKind::FrontCover))
        .or_else(|| tags.pictures.first())?;
    if u64::from(pic.data_len) > MAX_ART_BYTES {
        return None;
    }
    let mut src2 = embedded_io_adapters::std::FromStd::new(std::fs::File::open(track_path).ok()?);
    let bytes = read_picture_bytes(&mut src2, pic).ok()?;
    let mime = match pic.mime {
        MimeType::Png => "image/png",
        _ => "image/jpeg",
    }
    .to_string();
    Some(Cover { mime, bytes })
}

/// Find an external cover-image file for a track: search the track's own folder
/// and any `Scans/`-style sibling subfolder, preferring conventional cover names
/// and, among equals, the smallest file (front scans beat large inserts).
#[must_use]
pub fn external_cover(track_path: &Path) -> Option<PathBuf> {
    let dir = track_path.parent()?;
    let mut search_dirs = vec![dir.to_path_buf()];
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| ART_SUBDIRS.contains(&n.to_ascii_lowercase().as_str()))
            {
                search_dirs.push(p);
            }
        }
    }

    // (priority, size, path) — lower priority wins, then smaller size.
    let mut best: Option<(u32, u64, PathBuf)> = None;
    for d in &search_dirs {
        let Ok(rd) = std::fs::read_dir(d) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.starts_with('.') {
                continue; // AppleDouble / hidden junk
            }
            let lower = name.to_ascii_lowercase();
            let (stem, ext) = match lower.rsplit_once('.') {
                Some((s, e)) => (s, e),
                None => continue,
            };
            if !COVER_EXTS.contains(&ext) {
                continue;
            }
            let priority = match COVER_STEMS.iter().position(|s| *s == stem) {
                Some(i) => i as u32,
                None if stem.starts_with("front") => COVER_STEMS.len() as u32,
                None => continue,
            };
            let size = entry.metadata().map(|m| m.len()).unwrap_or(u64::MAX);
            if size > MAX_ART_BYTES {
                continue;
            }
            let better = match &best {
                None => true,
                Some((bp, bs, _)) => (priority, size) < (*bp, *bs),
            };
            if better {
                best = Some((priority, size, p));
            }
        }
    }
    best.map(|(_, _, p)| p)
}

fn read_external(path: PathBuf) -> Option<Cover> {
    let meta = std::fs::metadata(&path).ok()?;
    if meta.len() > MAX_ART_BYTES {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    Some(Cover {
        mime: mime_from_ext(&path).to_string(),
        bytes,
    })
}

fn mime_from_ext(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        _ => "image/jpeg",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sas_art_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn finds_cover_in_track_dir() {
        let d = tmpdir("root");
        std::fs::write(d.join("01 - song.dsf"), b"x").unwrap();
        std::fs::write(d.join("cover.jpg"), b"img").unwrap();
        std::fs::write(d.join("Rear.jpg"), b"back").unwrap();
        let got = external_cover(&d.join("01 - song.dsf")).unwrap();
        assert!(got.ends_with("cover.jpg"), "picked {got:?}");
    }

    #[test]
    fn finds_front_scan_in_scans_subdir_smallest_first() {
        let d = tmpdir("scans");
        std::fs::write(d.join("01 - song.dsf"), b"x").unwrap();
        let scans = d.join("Scans");
        std::fs::create_dir_all(&scans).unwrap();
        std::fs::write(scans.join("Front-0.jpg"), vec![0u8; 5000]).unwrap(); // larger
        std::fs::write(scans.join("Front-1.jpg"), vec![0u8; 1000]).unwrap(); // smaller
        std::fs::write(scans.join("Rear.jpg"), vec![0u8; 100]).unwrap(); // not a front
        std::fs::write(scans.join("._Front-0.jpg"), vec![0u8; 10]).unwrap(); // AppleDouble
        let got = external_cover(&d.join("01 - song.dsf")).unwrap();
        assert!(
            got.ends_with("Front-1.jpg"),
            "should pick the smallest front scan, got {got:?}"
        );
    }

    #[test]
    fn none_when_no_cover_present() {
        let d = tmpdir("empty");
        std::fs::write(d.join("01 - song.dsf"), b"x").unwrap();
        std::fs::write(d.join("notes.txt"), b"hi").unwrap();
        assert_eq!(external_cover(&d.join("01 - song.dsf")), None);
    }

    #[test]
    fn oversized_external_is_skipped() {
        let d = tmpdir("big");
        std::fs::write(d.join("01 - song.dsf"), b"x").unwrap();
        // A "cover" just over the cap must be ignored, not returned.
        let big = MAX_ART_BYTES + 1;
        let f = std::fs::File::create(d.join("cover.jpg")).unwrap();
        f.set_len(big).unwrap();
        assert_eq!(external_cover(&d.join("01 - song.dsf")), None);
    }
}
