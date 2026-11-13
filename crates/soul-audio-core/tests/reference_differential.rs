//! **Reference-differential conformance**: each decoder is checked against that
//! format's canonical reference decoder. For **lossless** formats (FLAC,
//! WavPack, ALAC) the guarantee is **bit-exact** PCM; for **lossy** formats
//! (Vorbis, AAC) a single bitstream can decode to slightly different floats in
//! different implementations, so the bar is a high **SNR** against the reference
//! instead (a real decode bug drops the SNR off a cliff; float-rounding noise
//! does not). We decode a file through our `open()` path and through the
//! reference (into a temp WAV, parsed to interleaved `i32`), and compare.
//!
//! `#[ignore]`d: the reference binaries are not on CI. Point each format at its
//! reference via an env var (or put it on PATH):
//!
//! | format  | env var        | reference (how to get it)                         |
//! |---------|----------------|---------------------------------------------------|
//! | FLAC    | `FLAC_BIN`     | Xiph `flac` (github.com/xiph/flac releases)       |
//! | WavPack | `WVUNPACK_BIN` | `wvunpack` (github.com/dbry/WavPack releases)      |
//! | ALAC    | `FFMPEG_BIN`   | `ffmpeg` (+`ffprobe`); lossless, so bit-exact     |
//! | Vorbis  | `FFMPEG_BIN`   | `ffmpeg`; lossy → SNR bound                        |
//! | AAC     | `FFMPEG_BIN`   | `ffmpeg`; lossy → SNR bound                        |
//!
//! ALAC and AAC share the `.m4a` container, so those two are told apart by the
//! reference's `codec_name` (`ffprobe`). Committed `testdata/` fixtures are
//! checked for every format whose reference is available; a real-music corpus is
//! opt-in via `CONFORMANCE_CORPUS_DIR`. Run:
//!
//! ```text
//! FLAC_BIN=/path/flac WVUNPACK_BIN=/path/wvunpack FFMPEG_BIN=/path/ffmpeg \
//!   CONFORMANCE_CORPUS_DIR=/path/library \
//!   cargo test -p soul-audio-core --test reference_differential -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use soul_audio_core::{Decoder, SliceSource, open_seekable};

/// How our decode is compared with the reference's.
#[derive(Clone, Copy)]
enum Exactness {
    /// Lossless: every sample must match.
    BitExact,
    /// Lossy: scale-invariant SNR (dB) must clear this bar. Different compliant
    /// decoders of the same lossy bitstream differ only by float-rounding noise,
    /// which sits far above this; a real bug does not.
    Snr(f64),
}

/// A format and how to invoke its reference decoder.
struct Fmt {
    name: &'static str,
    ext: &'static str,
    /// When set, only files the reference reports as this `codec_name` are taken
    /// — disambiguates ALAC vs AAC, which share `.m4a`.
    codec: Option<&'static str>,
    exact: Exactness,
    bin_env: &'static str,
    default_bin: &'static str,
    /// Decode `inp` to a canonical integer WAV at `out` with reference `bin`.
    decode_ref: fn(bin: &str, inp: &Path, out: &Path),
}

const FORMATS: &[Fmt] = &[
    Fmt {
        name: "FLAC",
        ext: "flac",
        codec: None,
        exact: Exactness::BitExact,
        bin_env: "FLAC_BIN",
        default_bin: "flac",
        decode_ref: ref_flac,
    },
    Fmt {
        name: "WavPack",
        ext: "wv",
        codec: None,
        exact: Exactness::BitExact,
        bin_env: "WVUNPACK_BIN",
        default_bin: "wvunpack",
        decode_ref: ref_wvunpack,
    },
    Fmt {
        name: "ALAC",
        ext: "m4a",
        codec: Some("alac"),
        exact: Exactness::BitExact,
        bin_env: "FFMPEG_BIN",
        default_bin: "ffmpeg",
        decode_ref: ref_ffmpeg,
    },
    Fmt {
        name: "Vorbis",
        ext: "ogg",
        codec: Some("vorbis"),
        // Compliant AAC/Vorbis decodes of these fixtures land at 60–92 dB SNR vs
        // ffmpeg (worst: dense real music); a real decode defect collapses toward
        // 0 dB. 50 dB sits in the wide gap — clear of content variance, far above
        // any bug.
        exact: Exactness::Snr(50.0),
        bin_env: "FFMPEG_BIN",
        default_bin: "ffmpeg",
        decode_ref: ref_ffmpeg,
    },
    Fmt {
        name: "AAC",
        ext: "m4a",
        codec: Some("aac"),
        exact: Exactness::Snr(50.0),
        bin_env: "FFMPEG_BIN",
        default_bin: "ffmpeg",
        decode_ref: ref_ffmpeg,
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

/// Decode `inp` to an integer-PCM WAV with ffmpeg, at the source's own bit depth
/// for lossless ALAC (so the comparison is bit-exact) or 24-bit for the lossy
/// formats (plenty of headroom for an SNR check). `bin` is the ffmpeg path;
/// `ffprobe` is assumed alongside it.
fn ref_ffmpeg(bin: &str, inp: &Path, out: &Path) {
    let pcm = match ffprobe_bits(bin, inp) {
        Some(16) => "pcm_s16le",
        Some(24) => "pcm_s24le",
        Some(32) => "pcm_s32le",
        _ => "pcm_s24le", // lossy (no raw bit depth) → high-precision reference
    };
    let st = Command::new(bin)
        .args(["-v", "error", "-y", "-i"])
        .arg(inp)
        .args(["-map", "0:a:0", "-c:a", pcm, "-f", "wav"])
        .arg(out)
        .status()
        .expect("spawn reference ffmpeg");
    assert!(st.success(), "reference ffmpeg failed on {}", inp.display());
}

/// The sibling `ffprobe` path for an `ffmpeg` path (`.../ffmpeg[.exe]` →
/// `.../ffprobe[.exe]`), falling back to a bare `ffprobe` on `PATH`.
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

/// `codec_name` of the first audio stream, lowercased (e.g. `"alac"`, `"aac"`,
/// `"vorbis"`), or `None` if ffprobe can't read it.
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

/// `bits_per_raw_sample` of the first audio stream (16/24/32 for lossless),
/// `None` for lossy streams that report no raw bit depth.
fn ffprobe_bits(ffmpeg: &str, inp: &Path) -> Option<u16> {
    let out = Command::new(ffprobe_for(ffmpeg))
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=bits_per_raw_sample",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(inp)
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
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

/// Decode every interleaved sample of `bytes` through OUR decoder, or `None` if
/// core can't construct a decoder for it. We use `open_seekable` (a `SliceSource`
/// is seekable) — the same entry point the desktop uses, so this exercises the
/// real playback decode path, including non-faststart MP4 (`moov` after `mdat`).
/// Some formats (notably Vorbis) are only wired up through the
/// `soul-audio-desktop` arena-backed path, not core's decoders, so the core
/// harness skips them rather than reporting a false failure — they belong in a
/// desktop-side harness.
fn decode_ours(bytes: &[u8]) -> Option<Vec<i32>> {
    let mut dec = open_seekable(SliceSource::new(bytes)).ok()?;
    let ch = usize::from(dec.stream_info().channels).max(1);
    // Trim to the PLAYABLE audio, matching what a reference's MP4 edit list
    // presents: drop the leading encoder-delay priming and cap at the playable
    // length (trailing padding removed). The deck does this for gapless; the raw
    // decoder reports the counts for us. A no-op for lossless (delay 0, no cap),
    // so ALAC/FLAC/WavPack stay bit-exact; it's what aligns lossy AAC with ffmpeg.
    let skip = dec.encoder_delay() as usize * ch;
    let cap = dec.playable_frames().map(|p| p as usize * ch);
    let mut all = Vec::new();
    let mut buf = [0i32; 8192];
    loop {
        let n = dec.decode_into(&mut buf).expect("decode_into");
        if n == 0 {
            break;
        }
        all.extend_from_slice(&buf[..n]);
    }
    let start = skip.min(all.len());
    let end = cap.map_or(all.len(), |c| (start + c).min(all.len()));
    Some(all[start..end].to_vec())
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

/// Scale-invariant SNR in dB of `ours` against `reference` over their common
/// length: fit the single best scalar `k` so `k·ours ≈ reference` (absorbing any
/// output-depth/scale difference between the two decoders), then
/// `10·log10(‖reference‖² / ‖reference − k·ours‖²)`. `+inf` when identical.
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

/// Assert our decode of `path` conforms to the format's reference, per `f.exact`.
/// Returns `false` (checked nothing) when core's `open()` can't decode the format
/// — the caller logs a skip; `true` when the conformance check actually ran.
#[must_use]
fn assert_conforms(f: &Fmt, bin: &str, path: &Path) -> bool {
    let bytes = std::fs::read(path).expect("read fixture");
    let Some(ours) = decode_ours(&bytes) else {
        eprintln!(
            "  SKIP {}: core can't decode this format via open_seekable ({})",
            f.name,
            path.display()
        );
        return false;
    };
    let out_wav = std::env::temp_dir().join(format!(
        "refdiff-{}.wav",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("x")
    ));
    (f.decode_ref)(bin, path, &out_wav);
    let theirs = parse_wav_samples(&std::fs::read(&out_wav).expect("read reference wav"));
    let _ = std::fs::remove_file(&out_wav);

    match f.exact {
        Exactness::BitExact => {
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
        Exactness::Snr(min_db) => {
            // Lossy decoders may differ by a few priming/padding frames; a large
            // length gap is itself a defect, a handful of frames is not.
            let (lo, hi) = (ours.len().min(theirs.len()), ours.len().max(theirs.len()));
            assert!(
                hi - lo <= 4096,
                "{}: lossy length gap too large ours={} ref={}",
                path.display(),
                ours.len(),
                theirs.len()
            );
            let snr = snr_db(&theirs, &ours);
            assert!(
                snr >= min_db,
                "{}: {} SNR {snr:.1} dB < {min_db} dB bound",
                path.display(),
                f.name
            );
            println!(
                "  OK {} SNR {snr:.1} dB (≥{min_db}): {} ({} samples)",
                f.name,
                path.display(),
                ours.len()
            );
        }
    }
    true
}

/// `.<ext>` files directly under `dir` that match `f.codec` (via ffprobe when
/// the format disambiguates by codec), sorted.
fn fixtures_for(f: &Fmt, dir: &Path, bin: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some(f.ext))
        .filter(|p| match f.codec {
            None => true,
            Some(c) => ffprobe_codec(bin, p).as_deref() == Some(c),
        })
        .collect();
    v.sort();
    v
}

/// Every committed `testdata/` fixture, per format whose reference is available.
#[test]
#[ignore = "needs reference decoders (FLAC_BIN/WVUNPACK_BIN/FFMPEG_BIN or on PATH); run with --ignored --nocapture"]
fn committed_fixtures_conform_to_reference() {
    let testdata = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata");
    let mut checked = 0;
    for f in FORMATS {
        let Some(bin) = resolve_bin(f) else {
            eprintln!("SKIP {}: no reference ({} or PATH)", f.name, f.bin_env);
            continue;
        };
        let fixtures = fixtures_for(f, &testdata, &bin);
        if fixtures.is_empty() {
            continue;
        }
        println!(
            "\n{} reference-differential ({} committed fixtures):",
            f.name,
            fixtures.len()
        );
        for p in &fixtures {
            if assert_conforms(f, &bin, p) {
                checked += 1;
            }
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
fn corpus_conforms_to_reference() {
    let Some(dir) = std::env::var_os("CONFORMANCE_CORPUS_DIR") else {
        eprintln!("SKIP: set CONFORMANCE_CORPUS_DIR to a folder of audio files");
        return;
    };
    let dir = PathBuf::from(dir);
    let mut checked = 0;
    for f in FORMATS {
        let Some(bin) = resolve_bin(f) else {
            eprintln!("SKIP {}: no reference", f.name);
            continue;
        };
        let files = fixtures_for(f, &dir, &bin);
        if files.is_empty() {
            continue;
        }
        println!("\n{} corpus ({} files):", f.name, files.len());
        for p in &files {
            if assert_conforms(f, &bin, p) {
                checked += 1;
            }
        }
    }
    // A file that opens but mis-decodes already panicked above. Reaching here with
    // nothing checked just means core couldn't decode any corpus file (e.g. a
    // Vorbis-only corpus) — a coverage gap, not a harness failure, so note it
    // rather than fail the run.
    if checked == 0 {
        eprintln!("NOTE: no corpus file was decodable by core open() — nothing checked");
    }
}
