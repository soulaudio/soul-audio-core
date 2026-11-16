//! Opt-in real-audio validation of our loudness metrics against ffmpeg's
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
//! It cross-checks **integrated LUFS**, **LRA**, and **true-peak (dBTP)** against
//! ffmpeg's `ebur128=peak=true` summary, and **DR** against an exact (sorted
//! order-statistics) `dr14_t.meter` reference computed over the same decoded PCM —
//! ffmpeg has no DR scanner, so we carry the authoritative algorithm here and check
//! our bounded-memory histogram DR against it. Also prints **PLR**. Asserts
//! per-metric tolerances for every PCM file it finds; if the corpus or ffmpeg is
//! absent it skips cleanly.

use std::path::{Path, PathBuf};
use std::process::Command;

use soul_audio_core::decoder::Decoder;
use soul_audio_desktop::open_track;
use soul_audio_loudness::{LoudnessMetrics, R128Analyzer};

const LIB: &str = r"D:\dev\soulaudio\testing-audio-library";
const TOL_LUFS: f32 = 0.5; // integrated
const TOL_LRA: f32 = 1.2; // LRA (histogram quantization vs ffmpeg's exact queue)
const TOL_TP: f32 = 1.0; // true-peak (interpolator differences)
const TOL_DR: i32 = 1; // DR14 integer (histogram bin quantization vs exact sort)
/// PCM-decodable extensions we cross-check (DSD is excluded — the decimator's
/// reported format is 1-bit, so canonical scaling doesn't apply the same way).
const EXTS: &[&str] = &[
    "wav", "flac", "mp3", "m4a", "aac", "ogg", "opus", "wv", "aiff", "caf",
];
const MAX_FILES: usize = 8;

/// Our analysis of a track: the analyzer's metrics plus the canonicalized PCM it
/// saw (so the test can also compute the EXACT DR14 reference over the same samples).
struct Ours {
    metrics: LoudnessMetrics,
    sample_rate: u32,
    channels: usize,
    samples: Vec<i32>,
}

/// Decode `path` through the desktop stack, canonicalize to signed 24-bit, feed the
/// analyzer, and keep the canon PCM (or `None` if unsupported).
fn our_metrics(path: &Path) -> Option<Ours> {
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
    let mut samples: Vec<i32> = Vec::new();
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
        samples.extend_from_slice(&canon[..n]);
    }
    Some(Ours {
        metrics: a.metrics(),
        sample_rate: si.sample_rate,
        channels: ch,
        samples,
    })
}

/// Exact Pleasurize/TT **DR14** from interleaved canonical-24-bit PCM — sorted
/// order-statistics (no histogram), the authoritative `dr14_t.meter` algorithm the
/// analyzer's bounded-memory histogram DR is checked against on real audio.
fn exact_dr14(samples: &[i32], channels: usize, sample_rate: u32) -> i32 {
    const FULL_SCALE: f64 = 8_388_608.0;
    let delta = if sample_rate == 44_100 { 60 } else { 0 };
    let bs = 3 * (sample_rate + delta) as usize;
    let frames: Vec<&[i32]> = samples.chunks_exact(channels).collect();
    let n = frames.len();
    let (mut rms2, mut peak): (Vec<Vec<f64>>, Vec<Vec<f64>>) =
        (vec![Vec::new(); channels], vec![Vec::new(); channels]);
    let mut i = 0;
    while i < n {
        let end = (i + bs).min(n);
        let m = (end - i) as f64;
        for c in 0..channels {
            let (mut sumsq, mut pk) = (0.0f64, 0.0f64);
            for f in &frames[i..end] {
                let x = f64::from(f[c]) / FULL_SCALE;
                sumsq += x * x;
                pk = pk.max(x.abs());
            }
            rms2[c].push(2.0 * sumsq / m);
            peak[c].push(pk);
        }
        i = end;
    }
    let mm = rms2[0].len();
    let n_blk = ((0.2 * mm as f64).floor() as usize).max(1);
    let mut sum = 0.0;
    for c in 0..channels {
        rms2[c].sort_by(|a, b| a.partial_cmp(b).unwrap());
        peak[c].sort_by(|a, b| a.partial_cmp(b).unwrap());
        let rms_top2: f64 = rms2[c][mm - n_blk..].iter().sum::<f64>() / n_blk as f64;
        sum += 20.0 * peak[c][mm - 2].log10() - 10.0 * rms_top2.log10();
    }
    (sum / channels as f64).round() as i32
}

/// Integrated LUFS, LRA (LU), and true-peak (dBFS) from `ffmpeg -af ebur128=peak=true`.
fn ffmpeg_metrics(path: &Path) -> Option<(f32, f32, f32)> {
    let out = Command::new("ffmpeg")
        .args(["-nostats", "-hide_banner", "-i"])
        .arg(path)
        .args(["-af", "ebur128=peak=true", "-f", "null", "-"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stderr);
    // Take the LAST occurrence of each label so we read the end-of-stream summary
    // (the per-frame log repeats `I:`/`LRA:`; only the summary has `Peak:`).
    let (mut i, mut lra, mut tp) = (None, None, None);
    let num_after = |line: &str, key: &str| -> Option<f32> {
        line.trim()
            .strip_prefix(key)?
            .split_whitespace()
            .next()?
            .parse::<f32>()
            .ok()
    };
    for line in text.lines() {
        if let Some(v) = num_after(line, "I:") {
            i = Some(v);
        }
        if let Some(v) = num_after(line, "LRA:") {
            lra = Some(v);
        }
        if let Some(v) = num_after(line, "Peak:") {
            tp = Some(v);
        }
    }
    Some((i?, lra?, tp?))
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
fn loudness_metrics_match_ffmpeg() {
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

    println!(
        "\n  file                   I (ours/ff)       LRA (ours/ff)     TP (ours/ff)     DR (ours/exact)  PLR"
    );
    println!(
        "  ----                   -----------       -------------     ------------     ---------------  ---"
    );
    let mut checked = 0;
    for path in corpus_files(&lib) {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        let (Some(o), Some((fi, flra, ftp))) = (our_metrics(&path), ffmpeg_metrics(&path)) else {
            println!("  {name:<22}  (skipped)");
            continue;
        };
        let m = o.metrics;
        // Our histogram DR vs the exact sorted-order-statistics DR14 over the SAME PCM.
        let exact = exact_dr14(&o.samples, o.channels, o.sample_rate);
        println!(
            "  {name:<22} {:>6.2}/{:>6.2}    {:>5.2}/{:>5.2}    {:>6.2}/{:>6.2}     {:>3.0} / {:<3}      {:>4.1}",
            m.integrated_lufs, fi, m.lra, flra, m.true_peak_dbtp, ftp, m.dr, exact, m.plr
        );
        checked += 1;
        let di = m.integrated_lufs - fi;
        assert!(
            di.abs() <= TOL_LUFS,
            "{name}: integrated ours {:.2} vs ffmpeg {fi:.2} (Δ {di:+.2}) exceeds ±{TOL_LUFS}",
            m.integrated_lufs
        );
        let dlra = m.lra - flra;
        assert!(
            dlra.abs() <= TOL_LRA,
            "{name}: LRA ours {:.2} vs ffmpeg {flra:.2} (Δ {dlra:+.2}) exceeds ±{TOL_LRA}",
            m.lra
        );
        let dtp = m.true_peak_dbtp - ftp;
        assert!(
            dtp.abs() <= TOL_TP,
            "{name}: true-peak ours {:.2} vs ffmpeg {ftp:.2} (Δ {dtp:+.2}) exceeds ±{TOL_TP}",
            m.true_peak_dbtp
        );
        let ddr = m.dr as i32 - exact;
        assert!(
            ddr.abs() <= TOL_DR,
            "{name}: DR ours {} vs exact dr14_t.meter {exact} (Δ {ddr:+}) exceeds ±{TOL_DR}",
            m.dr
        );
    }
    assert!(checked > 0, "no PCM files were cross-checked");
    println!("  all {checked} file(s) within tolerance\n");
}
