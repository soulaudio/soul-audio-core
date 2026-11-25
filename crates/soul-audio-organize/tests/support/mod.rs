//! Shared helpers for the organize integration tests: unique temp dirs, tiny
//! REAL tag writers (FLAC STREAMINFO + VORBIS_COMMENT; ID3v2.3 MP3) and tree
//! listing. (`soul-audio-tags`' `testfix` is crate-private, so these live here.)
#![allow(dead_code)] // each test binary uses a subset

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A fresh, unique temp dir (manual, per repo convention — no `tempfile`).
pub fn unique_dir(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("organize-it-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `root` joined with a `/`-separated relative path, component by component
/// (no mixed separators on Windows).
pub fn at(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    for c in rel.split('/') {
        p.push(c);
    }
    p
}

/// Write `bytes` at `root/rel`, creating parent folders. Returns the path.
pub fn put(root: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let p = at(root, rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, bytes).unwrap();
    p
}

/// `path` relative to `base`, `/`-separated.
pub fn rel(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or_else(|_| panic!("{path:?} is not under {base:?}"))
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Every file under `root` (hidden ones included), relative and `/`-separated.
pub fn tree(root: &Path) -> BTreeSet<String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                out.insert(rel(root, &p));
            }
        }
    }
    let mut out = BTreeSet::new();
    if root.exists() {
        walk(root, root, &mut out);
    }
    out
}

/// The `.flac`/`.mp3` files under `root` (not hidden), sorted — the organizer's
/// input list.
pub fn audio_files(root: &Path) -> Vec<PathBuf> {
    tree(root)
        .into_iter()
        .filter(|r| {
            let name = r.rsplit('/').next().unwrap_or(r);
            !name.starts_with('.') && (r.ends_with(".flac") || r.ends_with(".mp3"))
        })
        .map(|r| at(root, &r))
        .collect()
}

/// A minimal real FLAC: `fLaC` + STREAMINFO (44.1 kHz stereo, 2 s) + a LAST
/// VORBIS_COMMENT block with `comments` (`KEY=VALUE`). No audio frames — enough
/// for every tag reader in the workspace.
pub fn flac(comments: &[&str]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"fLaC");
    v.push(0x00); // STREAMINFO, not last
    v.extend_from_slice(&[0, 0, 34]);
    let mut si = [0u8; 34];
    let (sr, ch, bps, total): (u32, u32, u32, u64) = (44_100, 2, 16, 88_200);
    si[10] = (sr >> 12) as u8;
    si[11] = (sr >> 4) as u8;
    si[12] = (((sr & 0xF) as u8) << 4) | ((((ch - 1) & 0x7) as u8) << 1) | (((bps - 1) >> 4) as u8);
    si[13] = ((((bps - 1) & 0xF) as u8) << 4) | (((total >> 32) & 0xF) as u8);
    si[14] = (total >> 24) as u8;
    si[15] = (total >> 16) as u8;
    si[16] = (total >> 8) as u8;
    si[17] = total as u8;
    v.extend_from_slice(&si);
    let mut vc = Vec::new();
    vc.extend_from_slice(&6u32.to_le_bytes());
    vc.extend_from_slice(b"vendor");
    vc.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in comments {
        vc.extend_from_slice(&(c.len() as u32).to_le_bytes());
        vc.extend_from_slice(c.as_bytes());
    }
    v.push(0x84); // VORBIS_COMMENT, last block
    v.extend_from_slice(&(vc.len() as u32).to_be_bytes()[1..]);
    v.extend_from_slice(&vc);
    v
}

/// A minimal real MP3: an ID3v2.3 tag of UTF-8 text frames `(id, value)` (e.g.
/// `("TIT2", "Solo")`, `("TRCK", "1/1")`) followed by 64 zero bytes standing in
/// for audio (no MPEG sync, so the facts scan finds nothing).
pub fn mp3(frames: &[(&str, &str)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (id, value) in frames {
        assert_eq!(id.len(), 4, "ID3v2.3 frame ids are 4 chars");
        body.extend_from_slice(id.as_bytes());
        body.extend_from_slice(&(1 + value.len() as u32).to_be_bytes());
        body.extend_from_slice(&[0, 0]); // flags
        body.push(0x03); // UTF-8
        body.extend_from_slice(value.as_bytes());
    }
    let n = body.len() as u32;
    let mut v = b"ID3".to_vec();
    v.extend_from_slice(&[3, 0, 0]); // v2.3, no flags
    v.extend_from_slice(&[
        ((n >> 21) & 0x7F) as u8,
        ((n >> 14) & 0x7F) as u8,
        ((n >> 7) & 0x7F) as u8,
        (n & 0x7F) as u8,
    ]);
    v.extend_from_slice(&body);
    v.extend_from_slice(&[0u8; 64]);
    v
}
