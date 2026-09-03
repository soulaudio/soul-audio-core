//! Verify-loop harness for cover-art resolution (embedded + external).
//!
//! Run: cargo run -p soul-audio-service --example art_check -- <file-or-dir>
//! Walks a directory (or checks one file) and reports, per track, whether cover
//! art resolves and how big it is — so a fix can be re-run and confirmed without
//! launching the GUI.

use std::path::Path;

use soul_audio_service::art::{cover_bytes, external_cover};

fn main() {
    let arg = std::env::args()
        .nth(1)
        .expect("usage: art_check <file-or-dir>");
    let root = Path::new(&arg);
    let mut total = 0usize;
    let mut with_art = 0usize;
    if root.is_dir() {
        walk(root, &mut |p| check(p, &mut total, &mut with_art));
        eprintln!("\n=== {total} tracks, {with_art} resolved cover art ===");
    } else {
        check(root, &mut total, &mut with_art);
    }
}

fn check(path: &Path, total: &mut usize, with_art: &mut usize) {
    *total += 1;
    match cover_bytes(path) {
        Some(cover) => {
            *with_art += 1;
            let src = if external_cover(path).is_some() && !embedded_present(path) {
                "external"
            } else {
                "embedded"
            };
            let head: Vec<String> = cover
                .bytes
                .iter()
                .take(4)
                .map(|b| format!("{b:02X}"))
                .collect();
            println!(
                "ART {src:>8}  {}  [{} {} bytes, head {}]",
                path.display(),
                cover.mime,
                cover.bytes.len(),
                head.join(" ")
            );
        }
        None => println!("NONE          {}", path.display()),
    }
}

/// Cheap check used only to label the source in the report.
fn embedded_present(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut src = embedded_io_adapters::std::FromStd::new(file);
    matches!(soul_audio_tags::read_owned(&mut src), Ok((tags, _)) if !tags.pictures.is_empty())
}

fn walk(dir: &Path, f: &mut dyn FnMut(&Path)) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = rd.filter_map(Result::ok).map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            walk(&p, f);
        } else if matches!(
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref(),
            Some("flac" | "mp3" | "wav" | "m4a" | "aac" | "ogg" | "opus" | "dsf" | "dff")
        ) {
            f(&p);
        }
    }
}
