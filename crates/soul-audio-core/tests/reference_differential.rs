//! **Reference-differential conformance**: each lossless decoder must produce
//! **bit-exact** PCM against that format's *own* canonical reference decoder —
//! a stronger guarantee than an ffmpeg cross-check (ffmpeg's decoders are
//! themselves re-implementations). We decode a file through our `open()` path
//! and through the reference (into a temp WAV, parsed to interleaved `i32`), and
//! assert every sample matches.
//!
//! `#[ignore]`d: the reference binaries are not on CI. Point each format at its
//! reference via an env var (or put it on PATH):
//!
//! | format  | env var        | reference (how to get it)                         |
//! |---------|----------------|---------------------------------------------------|
//! | FLAC    | `FLAC_BIN`     | Xiph `flac` (github.com/xiph/flac releases)       |
//! | WavPack | `WVUNPACK_BIN` | `wvunpack` (github.com/dbry/WavPack releases)      |
//!
//! Committed `testdata/` fixtures are checked for every format whose reference
//! is available; a real-music corpus is opt-in via `CONFORMANCE_CORPUS_DIR`
//! (any `.flac`/`.wv` files in it are matched). Run:
//!
//! ```text
//! FLAC_BIN=/path/flac.exe WVUNPACK_BIN=/path/wvunpack.exe \
//!   CONFORMANCE_CORPUS_DIR=/path/library \
//!   cargo test -p soul-audio-core --test reference_differential -- --ignored --nocapture
//! ```
//!
//! ALAC (`refalac`/`alacconvert`) and the lossy formats (Vorbis/Opus/AAC via an
//! SNR bound, since those are not bit-exact) extend this same table.

use std::path::{Path, PathBuf};
use std::process::Command;

use soul_audio_core::{Decoder, SliceSource, open};

/// A lossless format and how to invoke its reference decoder.
struct Fmt {
    name: &'static str,
    ext: &'static str,
    bin_env: &'static str,
    default_bin: &'static str,
    /// Decode `inp` to a WAV at `out` with the reference binary `bin`.
    decode_ref: fn(bin: &str, inp: &Path, out: &Path),
}

const FORMATS: &[Fmt] = &[
    Fmt {
        name: "FLAC",
        ext: "flac",
        bin_env: "FLAC_BIN",
        default_bin: "flac",
        decode_ref: ref_flac,
    },
    Fmt {
        name: "WavPack",
        ext: "wv",
        bin_env: "WVUNPACK_BIN",
        default_bin: "wvunpack",
        decode_ref: ref_wvunpack,
    },
];

fn ref_flac(bin: &str, inp: &Path, out: &Path) {
    let st = Command::new(bin)
        .args(["-d", "-f", "-s", "-o"])
        .arg(out)
        .arg(inp)
        .status()
        .expect("spawn reference flac");
    assert!(st.success(), "reference flac failed on {}", inp.display());
}

fn ref_wvunpack(bin: &str, inp: &Path, out: &Path) {
    let st = Command::new(bin)
        .args(["-y", "-q"])
        .arg(inp)
        .arg(out)
        .status()
        .expect("spawn reference wvunpack");
    assert!(
        st.success(),
        "reference wvunpack failed on {}",
        inp.display()
    );
}

/// Resolve a format's reference binary: `$<bin_env>`, else the default name if
/// it responds to `--version` on PATH, else `None` (skip the format).
fn resolve_bin(f: &Fmt) -> Option<String> {
    if let Some(p) = std::env::var_os(f.bin_env) {
        return Some(p.to_string_lossy().into_owned());
    }
    Command::new(f.default_bin)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|_| f.default_bin.to_string())
}

/// Decode every interleaved sample of `bytes` through OUR decoder.
fn decode_ours(bytes: &[u8]) -> Vec<i32> {
    let mut dec = open(SliceSource::new(bytes)).expect("probe + construct decoder");
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
}

/// Parse a canonical PCM WAV into interleaved `i32` samples (16/24/32-bit int,
/// sign-extended). Panics on float WAV (reference wrote a float stream).
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
            // WAVE_FORMAT_EXTENSIBLE carries the real tag in its subformat GUID.
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

/// Assert our decode of `path` is bit-exact with the format's reference.
fn assert_bit_exact(f: &Fmt, bin: &str, path: &Path) {
    let bytes = std::fs::read(path).expect("read fixture");
    let ours = decode_ours(&bytes);
    let out_wav = std::env::temp_dir().join(format!(
        "refdiff-{}.wav",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("x")
    ));
    (f.decode_ref)(bin, path, &out_wav);
    let theirs = parse_wav_samples(&std::fs::read(&out_wav).expect("read reference wav"));
    let _ = std::fs::remove_file(&out_wav);
    assert_eq!(
        ours.len(),
        theirs.len(),
        "{}: sample count ours={} ref={}",
        path.display(),
        ours.len(),
        theirs.len()
    );
    if let Some((i, (a, b))) = ours
        .iter()
        .zip(&theirs)
        .enumerate()
        .find(|(_, (a, b))| a != b)
    {
        panic!(
            "{}: first mismatch at sample {i}: ours={a} ref={b}",
            path.display()
        );
    }
    println!(
        "  OK {} bit-exact: {} ({} samples)",
        f.name,
        path.display(),
        ours.len()
    );
}

/// `.<ext>` files directly under `dir`, sorted.
fn files_with_ext(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some(ext))
        .collect();
    v.sort();
    v
}

/// Every committed `testdata/` fixture, per format whose reference is available.
#[test]
#[ignore = "needs reference decoders (FLAC_BIN/WVUNPACK_BIN or on PATH); run with --ignored --nocapture"]
fn committed_fixtures_are_bit_exact_vs_reference() {
    let testdata = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata");
    let mut checked = 0;
    for f in FORMATS {
        let Some(bin) = resolve_bin(f) else {
            eprintln!("SKIP {}: no reference ({} or PATH)", f.name, f.bin_env);
            continue;
        };
        let fixtures = files_with_ext(&testdata, f.ext);
        if fixtures.is_empty() {
            continue;
        }
        println!(
            "\n{} reference-differential ({} committed fixtures):",
            f.name,
            fixtures.len()
        );
        for p in &fixtures {
            assert_bit_exact(f, &bin, p);
            checked += 1;
        }
    }
    assert!(
        checked > 0,
        "no fixtures checked — no reference decoder was available"
    );
}

/// Opt-in real-music corpus (`$CONFORMANCE_CORPUS_DIR`), per available format.
#[test]
#[ignore = "needs reference decoders + CONFORMANCE_CORPUS_DIR; run with --ignored --nocapture"]
fn corpus_is_bit_exact_vs_reference() {
    let Some(dir) = std::env::var_os("CONFORMANCE_CORPUS_DIR") else {
        eprintln!("SKIP: set CONFORMANCE_CORPUS_DIR to a folder of lossless audio");
        return;
    };
    let dir = PathBuf::from(dir);
    let mut checked = 0;
    for f in FORMATS {
        let files = files_with_ext(&dir, f.ext);
        if files.is_empty() {
            continue;
        }
        let Some(bin) = resolve_bin(f) else {
            eprintln!("SKIP {}: no reference", f.name);
            continue;
        };
        println!("\n{} corpus ({} files):", f.name, files.len());
        for p in &files {
            assert_bit_exact(f, &bin, p);
            checked += 1;
        }
    }
    assert!(checked > 0, "no corpus files matched an available format");
}
