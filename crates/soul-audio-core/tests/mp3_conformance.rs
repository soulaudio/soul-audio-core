//! ISO/IEC 11172-4 & 13818-4 **MPEG audio conformance** — the authoritative
//! standard vectors (not an ffmpeg cross-check), driven through the public
//! `open()` API against each vector's reference PCM.
//!
//! # Why the vectors are not committed
//!
//! The ISO compliance bitstreams are ISO-copyright. This project already keeps
//! license-restricted test data out of the tree (`ps-local.m4a`), so the vectors
//! live in a **gitignored** `testdata/mp3-conformance-local/` directory and this
//! test **skips** when they are absent (CI / a clean checkout). Fetch them with
//! `testdata/fetch-mp3-conformance.sh` (they are the set the minimp3 project —
//! which our Layer III decoder is ported from — ships for conformance).
//!
//! # What this establishes (measured)
//!
//! - **Layer III** (the real `.mp3`, MPEG-1 *and* MPEG-2): ISO **full accuracy** —
//!   the decode matches the reference to **max |Δ| ≤ 1 LSB** (PSNR 113–117 dB).
//!   This is the decisive, decoder-independent proof the core MP3 path is correct,
//!   beyond the ffmpeg SNR cross-checks in `decode.rs`.
//! - **Layer I / Layer II**: our decoder is a faithful (bit-exact) port of
//!   **kjmp2**, a compact *limited-accuracy* Layer II decoder (reduced-precision
//!   synthesis window), so it tracks the ISO reference only to ~42–64 dB. That is
//!   a documented, by-design property of kjmp2 — not a bug — and is gated here at
//!   a correspondingly honest bar so a real regression (a decode that fell apart)
//!   still fails loudly.

use std::path::PathBuf;

use soul_audio_core::io::SliceSource;
use soul_audio_core::{Decoder, open};

/// `(vector stem, max allowed |Δ| LSB, min PSNR dB)`. Layer III vectors demand
/// ISO full accuracy (±1 LSB); the kjmp2-based Layer I/II vectors are gated at
/// their measured limited-accuracy floor (so a real break still trips).
const VECTORS: &[(&str, i64, f64)] = &[
    // Layer III — ISO full accuracy.
    ("ILL2_layer3", 1, 96.0),
    ("M2L3_compl24", 1, 96.0), // MPEG-2 Layer III
    ("M2L3_noise", 1, 96.0),   // MPEG-2 Layer III
    // Layer I — kjmp2 limited accuracy.
    ("ILL2_layer1", 400, 55.0),
    // Layer II — kjmp2 limited accuracy.
    ("ILL2_center2", 4000, 40.0),
    ("ILL2_dual", 4000, 40.0),
    ("ILL2_mono", 4000, 40.0),
    ("ILL2_samples", 4000, 40.0),
    ("ILL2_scf63", 4000, 40.0),
    ("ILL2_multilingual", 4000, 55.0),
];

fn vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/mp3-conformance-local")
}

/// Decode a raw MPEG bitstream through `open()` on a big stack (`AnyDecoder` is
/// large by value), returning interleaved i32 samples clamped to 16-bit range.
fn decode(bytes: Vec<u8>) -> Vec<i32> {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let mut dec = open(SliceSource::new(&bytes)).expect("probe + construct MPEG decoder");
            let mut out = Vec::new();
            let mut buf = [0i32; 8192];
            loop {
                let n = dec.decode_into(&mut buf).expect("decode");
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
            out
        })
        .expect("spawn")
        .join()
        .expect("decode thread panicked")
}

fn load_ref_i16(path: &std::path::Path) -> Vec<i32> {
    std::fs::read(path)
        .unwrap()
        .chunks_exact(2)
        .map(|c| i32::from(i16::from_le_bytes([c[0], c[1]])))
        .collect()
}

#[test]
fn mpeg_audio_iso_conformance_vectors() {
    let dir = vectors_dir();
    if !dir.exists() {
        eprintln!(
            "SKIP: ISO MPEG conformance vectors absent ({}). \
             Run testdata/fetch-mp3-conformance.sh to enable (vectors are ISO-copyright, not committed).",
            dir.display()
        );
        return;
    }

    let mut ran = 0usize;
    for &(stem, max_diff_bar, psnr_bar) in VECTORS {
        let bit = dir.join(format!("{stem}.bit"));
        let pcm = dir.join(format!("{stem}.pcm"));
        if !bit.exists() || !pcm.exists() {
            continue;
        }
        ran += 1;

        let refr = load_ref_i16(&pcm);
        let ours = decode(std::fs::read(&bit).unwrap());
        let n = refr.len().min(ours.len());
        assert!(
            n >= refr.len().saturating_sub(16), // allow a sliver of trailing padding
            "{stem}: decoded {} samples vs reference {} — gross length mismatch",
            ours.len(),
            refr.len()
        );

        // Reference and decode are delay-aligned identically (both the minimp3
        // convention our Layer III path matches), so compare at offset 0.
        let mut max_diff = 0i64;
        let mut err = 0f64;
        let mut sig = 0f64;
        for i in 0..n {
            let d = i64::from(refr[i]) - i64::from(ours[i]);
            max_diff = max_diff.max(d.abs());
            err += (d * d) as f64;
            sig += f64::from(refr[i]).powi(2);
        }
        let _ = sig;
        let psnr = if err > 0.0 {
            10.0 * (32767.0f64.powi(2) * n as f64 / err).log10()
        } else {
            f64::INFINITY
        };
        assert!(
            max_diff <= max_diff_bar && psnr > psnr_bar,
            "{stem}: ISO conformance FAILED — max |Δ|={max_diff} (bar {max_diff_bar}), \
             PSNR={psnr:.1} dB (bar {psnr_bar})"
        );
    }

    assert!(
        ran > 0,
        "conformance dir {} exists but held no known vectors",
        dir.display()
    );
    eprintln!("MPEG ISO conformance: {ran} vectors passed");
}
