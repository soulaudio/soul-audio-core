//! Debug/verify tool: dump tags + picture descriptors for a file (or every
//! audio file under a directory), and try to read each picture's bytes.
//!
//! Run: cargo run -p soul-audio-tags --features std --example dump -- <path>
//! With a directory, it walks recursively and prints one line per file plus a
//! summary — the "loop and verify" harness for cover-art work.

use std::path::Path;

use soul_audio_tags::{MimeType, PictureKind, read_owned, read_picture_bytes};

fn main() {
    let arg = std::env::args().nth(1).expect("usage: dump <file-or-dir>");
    let path = Path::new(&arg);
    let mut total = 0usize;
    let mut with_pic = 0usize;
    let mut pic_read_ok = 0usize;
    if path.is_dir() {
        walk(path, &mut |p| {
            dump_one(p, &mut total, &mut with_pic, &mut pic_read_ok, false)
        });
        eprintln!(
            "\n=== summary: {total} files, {with_pic} report a picture, {pic_read_ok} picture-bytes read OK ==="
        );
    } else {
        dump_one(path, &mut total, &mut with_pic, &mut pic_read_ok, true);
    }
}

fn dump_one(
    path: &Path,
    total: &mut usize,
    with_pic: &mut usize,
    pic_read_ok: &mut usize,
    verbose: bool,
) {
    *total += 1;
    let Ok(file) = std::fs::File::open(path) else {
        println!("OPEN-ERR  {}", path.display());
        return;
    };
    let mut src = embedded_io_adapters::std::FromStd::new(file);
    match read_owned(&mut src) {
        Ok((tags, facts)) => {
            if verbose {
                println!("file:    {}", path.display());
                println!("title:   {:?}", tags.title);
                println!("artist:  {:?}", tags.artist);
                println!("album:   {:?}", tags.album);
                println!("facts:   {facts:?}");
                println!("pictures: {}", tags.pictures.len());
            }
            if tags.pictures.is_empty() {
                println!("NO-PIC    {}", path.display());
            } else {
                *with_pic += 1;
                let p = tags
                    .pictures
                    .iter()
                    .find(|p| matches!(p.usage, PictureKind::FrontCover))
                    .or_else(|| tags.pictures.first())
                    .unwrap();
                let mime = match p.mime {
                    MimeType::Png => "png",
                    MimeType::Jpeg => "jpeg",
                    MimeType::Other => "other",
                };
                // Re-open for the picture read (mirrors the app's track_art).
                let mut src2 =
                    embedded_io_adapters::std::FromStd::new(std::fs::File::open(path).unwrap());
                match read_picture_bytes(&mut src2, p) {
                    Ok(bytes) => {
                        *pic_read_ok += 1;
                        let head: Vec<String> =
                            bytes.iter().take(4).map(|b| format!("{b:02X}")).collect();
                        println!(
                            "PIC-OK    {} [{mime} {:?} off={} len={} read={} head={}]",
                            path.display(),
                            p.usage,
                            p.data_offset,
                            p.data_len,
                            bytes.len(),
                            head.join(" ")
                        );
                    }
                    Err(e) => println!(
                        "PIC-READ-ERR {} [{mime} off={} len={} err={e:?}]",
                        path.display(),
                        p.data_offset,
                        p.data_len
                    ),
                }
            }
        }
        Err(e) => println!("READ-ERR  {} [{e:?}]", path.display()),
    }
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
