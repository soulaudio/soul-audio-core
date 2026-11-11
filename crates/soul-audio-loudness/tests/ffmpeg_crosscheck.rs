//! Opt-in real-audio validation of integrated loudness against ffmpeg's
//! `ebur128` scanner, on the local `testing-audio-library` corpus.
//!
//! `#[ignore]`d — it needs the local corpus and `ffmpeg` on PATH, so it is not a
//! CI gate (the committed, always-run accuracy gate is the synthetic EBU
//! Tech 3341 calibration in `analyzer.rs`). Run it locally to confirm the whole
//! decode → analyze pipeline agrees with a reference on real music:
//!
//! ```text
//! cargo test -p soul-audio-loudness --test ffmpeg_crosscheck -- --ignored --nocapture
//! ```
//!
//! It prints an agreement table (ours vs ffmpeg `I:`) and asserts `|Δ| <= 0.5`
//! LU for every PCM file it finds. If the corpus or ffmpeg is absent it skips
//! cleanly.

use std::path::{Path, PathBuf};
use std::process::Command;

use soul_audio_core::decoder::Decoder;
use soul_audio_desktop::open_track;
use soul_audio_loudness::{NO_LOUDNESS, R128Analyzer};

const LIB: &str = r"D:\dev\soulaudio\testing-audio-library";
const TOL_LU: f32 = 0.5;
/// PCM-decodable extensions we cross-check (DSD is excluded — the decimator's
/// reported format is 1-bit, so canonical scaling doesn't apply the same way).
const EXTS: &[&str] = &[
    "wav", "flac", "mp3", "m4a", "aac", "ogg", "opus", "wv", "aiff", "caf",
];
const MAX_FILES: usize = 8;

/// Decode `path` through the desktop stack, canonicalize to signed 24-bit, and
/// return our integrated LUFS (or `None` if unsupported / below gate).
fn our_integrated(path: &Path) -> Option<f32> {
    let mut d = open_track(path).ok()?;
    let si = d.stream_info();
    if si.format.is_dsd() || si.channels == 0 || si.channels > 2 {
        return None;
    }
    let ch = si.channels as usize;
    let shift = 24i32 - i32::from(si.bits_per_sample());
    let mut a = R128Analyzer::new(si.sample_rate, si.channels as u8);
    let mut buf = vec![0i32; 8192 * ch];
    let mut canon = vec![0i32; 8192 * ch];
    loop {
        let n = d.decode_into(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        for i in 0..n {
            let s = buf[i];
            canon[i] = if shift >= 0 {
                s << shift
            } else {
                s >> (-shift)
            };
        }
        a.feed(&canon[..n]);
    }
    let l = a.integrated_lufs();
    (l != NO_LOUDNESS).then_some(l)
}

/// Parse the integrated loudness (`I:  -XX.X LUFS`) from `ffmpeg -af ebur128`.
fn ffmpeg_integrated(path: &Path) -> Option<f32> {
    let out = Command::new("ffmpeg")
        .args(["-nostats", "-hide_banner", "-i"])
        .arg(path)
        .args(["-af", "ebur128", "-f", "null", "-"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stderr);
    // The end-of-stream summary prints "    I:         -XX.X LUFS"; take the last.
    let mut found = None;
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("I:") {
            if let Some(num) = rest.split_whitespace().next() {
                if let Ok(v) = num.parse::<f32>() {
                    found = Some(v);
                }
            }
        }
    }
    found
}

/// Up to `MAX_FILES` PCM files in the corpus, sorted by name.
fn corpus_files(lib: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(lib)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| EXTS.contains(&e.to_ascii_lowercase().as_str()))
                .unwrap_or(false)
        })
        .collect();
    files.sort();
    files.truncate(MAX_FILES);
    files
}

#[test]
#[ignore = "needs local testing-audio-library + ffmpeg; run with --ignored --nocapture"]
fn integrated_lufs_matches_ffmpeg() {
    // `OwnedDecoder` is large by value (~0.3–0.7 MiB) and overflows the default
    // test stack in debug — run on a big-stack thread like the desktop decode
    // tests do, and propagate any assertion panic via join().
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(run_crosscheck)
        .expect("spawn big-stack thread")
        .join()
        .expect("cross-check thread panicked");
}

fn run_crosscheck() {
    let lib = PathBuf::from(LIB);
    if !lib.is_dir() {
        eprintln!("SKIP: corpus {LIB} not present");
        return;
    }
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("SKIP: ffmpeg not on PATH");
        return;
    }

    println!("\n  file                    ours      ffmpeg      Δ (LU)");
    println!("  ----                    ----      ------      ------");
    let mut checked = 0;
    let mut worst: f32 = 0.0;
    for path in corpus_files(&lib) {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        match (our_integrated(&path), ffmpeg_integrated(&path)) {
            (Some(o), Some(f)) => {
                let d = o - f;
                println!("  {name:<22}  {o:>7.2}    {f:>7.2}    {d:>+6.2}");
                worst = worst.max(d.abs());
                checked += 1;
                assert!(
                    d.abs() <= TOL_LU,
                    "{name}: ours {o:.2} vs ffmpeg {f:.2} (Δ {d:+.2} LU) exceeds ±{TOL_LU}"
                );
            }
            _ => println!("  {name:<22}  (skipped: no integrated value)"),
        }
    }
    println!("  worst |Δ| = {worst:.2} LU over {checked} file(s)\n");
    assert!(checked > 0, "no PCM files were cross-checked");
}
