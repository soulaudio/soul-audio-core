//! **Vorbis reference-differential conformance** (desktop side).
//!
//! Core's no-alloc `open()` doesn't decode Vorbis — the arena-backed
//! `VorbisDecoder` lives in `soul-audio-desktop` and is reached through
//! `open_track`. So Vorbis conformance is checked here rather than in
//! `soul-audio-core`'s `reference_differential` suite: we decode a `.ogg`
//! through `open_track` and compare against ffmpeg. Vorbis is lossy, so the bar
//! is a high **scale-invariant SNR** (a real decode bug drops it off a cliff;
//! float-rounding between compliant decoders does not), matching the core
//! suite's AAC treatment.
//!
//! `#[ignore]`d (ffmpeg isn't on CI). `FFMPEG_BIN=/path/ffmpeg` (or on PATH),
//! optional `CONFORMANCE_CORPUS_DIR`. Run:
//!
//! ```text
//! FFMPEG_BIN=/path/ffmpeg cargo test -p soul-audio-desktop \
//!   --test vorbis_conformance -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use soul_audio_core::Decoder;
use soul_audio_desktop::open_track;

/// Compliant Vorbis decodes sit far above this vs ffmpeg; a defect collapses
/// toward 0 dB (same bound the core suite uses for AAC).
const SNR_MIN_DB: f64 = 50.0;

/// The ffmpeg binary: `$FFMPEG_BIN`, else `ffmpeg` if it answers `--version` on
/// PATH, else `None` (skip — the reference isn't available).
fn resolve_ffmpeg() -> Option<String> {
    if let Some(p) = std::env::var_os("FFMPEG_BIN") {
        return Some(p.to_string_lossy().into_owned());
    }
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| "ffmpeg".to_string())
}

/// The sibling `ffprobe` for an `ffmpeg` path (`.../ffmpeg[.exe]` →
/// `.../ffprobe[.exe]`), falling back to a bare `ffprobe` on PATH.
fn ffprobe_for(ffmpeg: &str) -> String {
    let p = Path::new(ffmpeg);
    if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
        let probe = name.replacen("ffmpeg", "ffprobe", 1);
        if probe != name {
            return p.with_file_name(probe).to_string_lossy().into_owned();
        }
    }
    "ffprobe".to_string()
}

/// `codec_name` of the first audio stream, lowercased.
fn ffprobe_codec(ffmpeg: &str, inp: &Path) -> Option<String> {
    let out = Command::new(ffprobe_for(ffmpeg))
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(inp)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_lowercase())
        .filter(|s| !s.is_empty())
}

/// Decode `inp` to a 24-bit integer-PCM WAV with ffmpeg (high-precision lossy
/// reference).
fn ref_ffmpeg_wav(bin: &str, inp: &Path, out: &Path) {
    let st = Command::new(bin)
        .args(["-v", "error", "-y", "-i"])
        .arg(inp)
        .args(["-map", "0:a:0", "-c:a", "pcm_s24le", "-f", "wav"])
        .arg(out)
        .status()
        .expect("spawn reference ffmpeg");
    assert!(st.success(), "reference ffmpeg failed on {}", inp.display());
}

/// Parse a canonical integer-PCM WAV (16/24/32-bit) into interleaved `i32`.
fn parse_wav_samples(wav: &[u8]) -> Vec<i32> {
    assert!(&wav[0..4] == b"RIFF" && &wav[8..12] == b"WAVE", "not a WAV");
    let (mut bits, mut fmt_tag) = (16u16, 1u16);
    let mut pos = 12;
    let mut data: &[u8] = &[];
    while pos + 8 <= wav.len() {
        let id = &wav[pos..pos + 4];
        let len = u32::from_le_bytes(wav[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = &wav[pos + 8..(pos + 8 + len).min(wav.len())];
        if id == b"fmt " {
            fmt_tag = u16::from_le_bytes(body[0..2].try_into().unwrap());
            bits = u16::from_le_bytes(body[14..16].try_into().unwrap());
            if fmt_tag == 0xFFFE && body.len() >= 26 {
                fmt_tag = u16::from_le_bytes(body[24..26].try_into().unwrap());
            }
        } else if id == b"data" {
            data = body;
        }
        pos += 8 + len + (len & 1);
    }
    assert_eq!(
        fmt_tag, 1,
        "reference WAV is not integer PCM (tag {fmt_tag})"
    );
    match bits {
        16 => data
            .chunks_exact(2)
            .map(|b| i32::from(i16::from_le_bytes([b[0], b[1]])))
            .collect(),
        24 => data
            .chunks_exact(3)
            .map(|b| {
                ((i32::from(b[0])) | (i32::from(b[1]) << 8) | (i32::from(b[2]) << 16)) << 8 >> 8
            })
            .collect(),
        32 => data
            .chunks_exact(4)
            .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect(),
        other => panic!("unexpected reference WAV bit depth: {other}"),
    }
}

/// Scale-invariant SNR (dB) of `ours` vs `reference` over their common length:
/// best-fit scalar absorbs any output-depth difference, then
/// `10·log10(‖reference‖² / ‖reference − k·ours‖²)`.
fn snr_db(reference: &[i32], ours: &[i32]) -> f64 {
    let n = reference.len().min(ours.len());
    if n == 0 {
        return f64::NEG_INFINITY;
    }
    let (mut rr, mut ro, mut oo) = (0f64, 0f64, 0f64);
    for i in 0..n {
        let (r, o) = (f64::from(reference[i]), f64::from(ours[i]));
        rr += r * r;
        ro += r * o;
        oo += o * o;
    }
    if oo == 0.0 {
        return if rr == 0.0 {
            f64::INFINITY
        } else {
            f64::NEG_INFINITY
        };
    }
    let k = ro / oo;
    let err: f64 = (0..n)
        .map(|i| {
            let d = f64::from(reference[i]) - k * f64::from(ours[i]);
            d * d
        })
        .sum();
    if err == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (rr / err).log10()
    }
}

/// Decode a file through the desktop `open_track` path (the real playback path —
/// the only one that decodes Vorbis). Runs on a 64 MiB-stack thread: the
/// arena-backed `OwnedDecoder` is a large by-value struct that overflows the
/// default test stack in a debug build.
fn decode_ours(path: &Path) -> Vec<i32> {
    let path = path.to_owned();
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            let mut dec = open_track(&path).expect("open_track");
            let mut out = Vec::new();
            let mut buf = [0i32; 8192];
            loop {
                let n = dec.decode_into(&mut buf).expect("decode_into");
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
            out
        })
        .expect("spawn decode thread")
        .join()
        .expect("decode thread panicked")
}

fn assert_vorbis_conforms(bin: &str, path: &Path) {
    let ours = decode_ours(path);
    let out_wav = std::env::temp_dir().join(format!(
        "vorbisdiff-{}.wav",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("x")
    ));
    ref_ffmpeg_wav(bin, path, &out_wav);
    let theirs = parse_wav_samples(&std::fs::read(&out_wav).expect("read reference wav"));
    let _ = std::fs::remove_file(&out_wav);

    let (lo, hi) = (ours.len().min(theirs.len()), ours.len().max(theirs.len()));
    assert!(
        hi - lo <= 4096,
        "{}: Vorbis length gap too large ours={} ref={}",
        path.display(),
        ours.len(),
        theirs.len()
    );
    let snr = snr_db(&theirs, &ours);
    assert!(
        snr >= SNR_MIN_DB,
        "{}: Vorbis SNR {snr:.1} dB < {SNR_MIN_DB} dB bound",
        path.display()
    );
    println!(
        "  OK Vorbis SNR {snr:.1} dB (≥{SNR_MIN_DB}): {} ({} samples)",
        path.display(),
        ours.len()
    );
}

/// `.ogg` files directly under `dir` that ffprobe reports as Vorbis, sorted.
fn vorbis_files(dir: &Path, bin: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("ogg"))
        .filter(|p| ffprobe_codec(bin, p).as_deref() == Some("vorbis"))
        .collect();
    v.sort();
    v
}

#[test]
#[ignore = "needs ffmpeg (FFMPEG_BIN or PATH); run with --ignored --nocapture"]
fn committed_vorbis_conforms_to_reference() {
    let Some(bin) = resolve_ffmpeg() else {
        eprintln!("SKIP Vorbis: no ffmpeg (FFMPEG_BIN or PATH)");
        return;
    };
    let testdata = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata");
    let files = vorbis_files(&testdata, &bin);
    assert!(!files.is_empty(), "no committed Vorbis fixtures found");
    println!(
        "\nVorbis reference-differential ({} committed fixtures):",
        files.len()
    );
    for p in &files {
        assert_vorbis_conforms(&bin, p);
    }
}

#[test]
#[ignore = "needs ffmpeg + CONFORMANCE_CORPUS_DIR; run with --ignored --nocapture"]
fn corpus_vorbis_conforms_to_reference() {
    let Some(bin) = resolve_ffmpeg() else {
        eprintln!("SKIP Vorbis: no ffmpeg");
        return;
    };
    let Some(dir) = std::env::var_os("CONFORMANCE_CORPUS_DIR") else {
        eprintln!("SKIP: set CONFORMANCE_CORPUS_DIR to a folder of audio files");
        return;
    };
    let files = vorbis_files(&PathBuf::from(dir), &bin);
    if files.is_empty() {
        eprintln!("NOTE: no Vorbis files in the corpus — nothing checked");
        return;
    }
    println!("\nVorbis corpus ({} files):", files.len());
    for p in &files {
        assert_vorbis_conforms(&bin, p);
    }
}
