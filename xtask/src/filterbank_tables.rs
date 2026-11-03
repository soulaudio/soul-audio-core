//! Generates `crates/soul-audio-core/src/formats/aac/filterbank_tables.rs` —
//! the AAC-LC filterbank's precomputed trig tables: the radix-2 FFT twiddles,
//! the IMDCT pre/post twiddles (long N=2048 / short N=256), and the sine + KBD
//! analysis/synthesis window halves (long + short). Run via
//! `cargo xtask gen-filterbank-tables`; the output is checked in and CI-guarded
//! by [`check`] (wired into `xtask verify`), exactly like the resampler tables.
//!
//! The core crate is `#![no_std]`, no-alloc, and its dependency allowlist forbids
//! `libm`, so it cannot call `sin`/`cos`/Bessel `i0` at runtime — every trig value
//! the filterbank needs is baked here (host-side, where `f64` trig + an `i0`
//! series are fine) and read from `const` tables. A runtime self-check test in
//! `filterbank.rs` re-derives the closed-form properties (window Princen–Bradley
//! reconstruction, twiddle unit-modulus) so the tables can't silently rot.
//!
//! # Layout / conventions (must match `filterbank.rs`)
//! - `FFT_TW_COS/SIN[m] = cos/sin(2π·m/512)`, `m = 0..256`. The radix-2 FFT of
//!   size `L ∈ {512, 64}` reads the twiddle for a length-`len` sub-DFT at column
//!   `j` from index `j·(512/len)` (always `< 256`): forward uses `(cos, -sin)`,
//!   inverse `(cos, +sin)`. One table serves both sizes (64 = 512 via stride 8).
//! - `IMDCT_TW_{LONG,SHORT}_COS/SIN[k] = cos/sin(2π·(k+1/8)/N)`, `N = 2048`/`256`,
//!   `k = 0..N/4`.
//! - `SINE_{LONG,SHORT}[n] = sin((π/N)(n+1/2))`, the rising half (`n = 0..N/2`),
//!   `N = 2048`/`256`.
//! - `KBD_{LONG,SHORT}` — Kaiser–Bessel-derived rising half, `α = 4`/`6`, via the
//!   cumulative-sum-of-Kaiser normalization (ISO 14496-3 / FFmpeg `kbdwin.c`).

use std::f64::consts::PI;
use std::fmt::Write as _;

const TABLES_PATH: &str = "crates/soul-audio-core/src/formats/aac/filterbank_tables.rs";

/// Base size of the shared FFT twiddle table (largest FFT is N/4 = 512).
const FFT_TW_N: usize = 512;

/// Modified Bessel function of the first kind, order 0 — same power series the
/// resampler generator uses (host-only; the core crate never calls this).
fn i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half = x / 2.0;
    for k in 1..100 {
        term *= (half / k as f64) * (half / k as f64);
        sum += term;
        if term < 1e-18 * sum {
            break;
        }
    }
    sum
}

/// Shared radix-2 FFT twiddles `cos/sin(2π·m/512)`, `m = 0..256`.
fn fft_twiddles() -> (Vec<f32>, Vec<f32>) {
    let n = FFT_TW_N / 2; // 256
    let mut c = vec![0f32; n];
    let mut s = vec![0f32; n];
    for m in 0..n {
        let a = 2.0 * PI * m as f64 / FFT_TW_N as f64;
        c[m] = a.cos() as f32;
        s[m] = a.sin() as f32;
    }
    (c, s)
}

/// IMDCT pre/post twiddles `cos/sin(2π·(k+1/8)/n)`, `k = 0..n/4`.
fn imdct_twiddles(n: usize) -> (Vec<f32>, Vec<f32>) {
    let l = n / 4;
    let mut c = vec![0f32; l];
    let mut s = vec![0f32; l];
    for k in 0..l {
        let a = 2.0 * PI * (k as f64 + 0.125) / n as f64;
        c[k] = a.cos() as f32;
        s[k] = a.sin() as f32;
    }
    (c, s)
}

/// Sine rising half `sin((π/n)(i+1/2))`, `i = 0..n/2` (full window length `n`).
fn sine_half(n: usize) -> Vec<f32> {
    let nh = n / 2;
    (0..nh)
        .map(|i| (PI / n as f64 * (i as f64 + 0.5)).sin() as f32)
        .collect()
}

/// KBD rising half, `nh = n/2` samples, Kaiser parameter `alpha`. The Kaiser
/// window (length `nh+1`) is cumulatively summed and square-rooted — the
/// standard KBD construction whose reflection guarantees Princen–Bradley
/// perfect reconstruction (`w[i]²+w[nh-1-i]² = 1`). `alpha2 = 4·(α·π/nh)²`
/// matches ISO 14496-3 / FFmpeg `ff_kbd_window_init(w, α, nh)`.
fn kbd_half(n: usize, alpha: f64) -> Vec<f32> {
    let nh = n / 2;
    let alpha2 = 4.0 * (alpha * PI / nh as f64) * (alpha * PI / nh as f64);
    let temp: Vec<f64> = (0..=nh)
        .map(|i| i0(((i as f64) * ((nh - i) as f64) * alpha2).sqrt()))
        .collect();
    let total: f64 = temp.iter().sum();
    let mut cum = 0.0;
    (0..nh)
        .map(|i| {
            cum += temp[i];
            (cum / total).sqrt() as f32
        })
        .collect()
}

/// Shortest round-tripping f32 literal, guaranteed to parse in an `[f32; _]`
/// array — `{v}` renders whole values as bare integers (`1`, `0`), which Rust
/// rejects as float elements, so append `.0` when there's no `.`/`e` already.
fn lit(v: f32) -> String {
    let s = format!("{v}");
    if s.contains(['.', 'e', 'E']) {
        s
    } else {
        format!("{s}.0")
    }
}

fn emit(name: &str, t: &[f32], out: &mut String) {
    writeln!(out, "#[rustfmt::skip]").unwrap();
    writeln!(out, "pub(super) static {name}: [f32; {}] = [", t.len()).unwrap();
    for chunk in t.chunks(8) {
        out.push_str("    ");
        for v in chunk {
            write!(out, "{},", lit(*v)).unwrap();
        }
        out.push('\n');
    }
    out.push_str("];\n");
}

/// Build the checked-in table source content.
fn generate() -> String {
    let mut out = String::new();
    out.push_str("// @generated by `cargo xtask gen-filterbank-tables` — do not edit.\n");
    out.push_str("// AAC-LC filterbank trig tables (FFT + IMDCT twiddles, sine/KBD windows).\n");
    out.push_str("// See xtask/src/filterbank_tables.rs for layout/conventions.\n");
    writeln!(out, "pub(super) const FFT_TW_N: usize = {FFT_TW_N};").unwrap();

    let (fc, fs) = fft_twiddles();
    emit("FFT_TW_COS", &fc, &mut out);
    emit("FFT_TW_SIN", &fs, &mut out);

    let (lc, ls) = imdct_twiddles(2048);
    emit("IMDCT_TW_LONG_COS", &lc, &mut out);
    emit("IMDCT_TW_LONG_SIN", &ls, &mut out);
    let (sc, ss) = imdct_twiddles(256);
    emit("IMDCT_TW_SHORT_COS", &sc, &mut out);
    emit("IMDCT_TW_SHORT_SIN", &ss, &mut out);

    emit("SINE_LONG", &sine_half(2048), &mut out);
    emit("SINE_SHORT", &sine_half(256), &mut out);
    emit("KBD_LONG", &kbd_half(2048, 4.0), &mut out);
    emit("KBD_SHORT", &kbd_half(256, 6.0), &mut out);
    out
}

/// Regenerate the checked-in table source file.
pub fn run() -> Result<(), String> {
    let content = generate();
    std::fs::write(TABLES_PATH, content).map_err(|e| format!("writing {TABLES_PATH}: {e}"))
}

/// CI guard: fail if the committed tables differ from a fresh generation.
pub fn check() -> Result<(), String> {
    let want = generate();
    let have =
        std::fs::read_to_string(TABLES_PATH).map_err(|e| format!("reading {TABLES_PATH}: {e}"))?;
    if want == have {
        Ok(())
    } else {
        Err(format!(
            "{TABLES_PATH} is stale — run `cargo xtask gen-filterbank-tables`"
        ))
    }
}
