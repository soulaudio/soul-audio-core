//! Generates `crates/soul-audio-core/src/formats/vorbis/vorbis_tables.rs` — the
//! Vorbis decoder's precomputed trig tables: the Vorbis window rising halves for
//! every supported block size, and a shared cosine quarter-wave table the naïve
//! IMDCT (spec §4.3.7) indexes instead of calling runtime trig. Run via
//! `cargo xtask gen-vorbis-tables`; the output is checked in and CI-guarded by
//! [`check`] (wired into `xtask verify`), exactly like the filterbank tables.
//!
//! The core crate is `#![no_std]`, no-alloc, and its dependency allowlist forbids
//! `libm`, so it cannot call `sin`/`cos` at runtime — every trig value the
//! IMDCT/window needs is baked here (host-side, where `f64` trig is fine) and read
//! from `const`/`static` tables. A runtime self-check test in `imdct.rs`
//! re-derives the closed-form properties (window Princen–Bradley reconstruction,
//! cosine lookup vs the direct formula) so the tables can't silently rot.
//!
//! # Layout / conventions (must match `imdct.rs`)
//! - `WIN_<L>[i] = sin(π/2 · sin²((i+½)/L · π/2))`, `i = 0..L` — the Vorbis window
//!   **rising** half of length `L` (spec §4.3.8). `L = blocksize/2`, one table per
//!   supported block size (`blocksize = 2^e`, `e ∈ 6..=13`, so `L ∈ 32..=4096`).
//!   The falling half is the reverse of the rising half (the window is symmetric),
//!   so only the rising half is stored.
//! - `COS_QUARTER[m] = cos(2π·m / (4·MAX_BLOCKSIZE))`, `m = 0..=MAX_BLOCKSIZE`
//!   (`= cos(π·m / (2·MAX_BLOCKSIZE))`) — one quarter-period of a cosine at the
//!   finest resolution any block size needs. The IMDCT's cosine argument is
//!   `(π/(2·bs))·(2n+1+bs/2)·(2k+1) = 2π·J / (4·bs)`, an integer `J` scaled by
//!   `2π/(4·bs)`; a stride of `MAX_BLOCKSIZE/bs` maps any block size onto this one
//!   table, and the four quadrants are reflected from the stored quarter.

use std::f64::consts::PI;
use std::fmt::Write as _;

const TABLES_PATH: &str = "crates/soul-audio-core/src/formats/vorbis/vorbis_tables.rs";

/// Largest (long) block size the decoder supports — the Vorbis I ceiling `2^13`.
/// Must equal `formats::vorbis::MAX_BLOCKSIZE`. Sets the shared cosine table's
/// resolution (`4·MAX_BLOCKSIZE` samples per full period → a `MAX_BLOCKSIZE`-entry
/// quarter).
const MAX_BLOCKSIZE: usize = 8192;

/// Block-size exponents the identification header admits (`2^e`, spec §4.2.2).
const BLOCKSIZE_EXPONENTS: [u32; 8] = [6, 7, 8, 9, 10, 11, 12, 13];

/// One Vorbis window rising half of length `l` (spec §4.3.8):
/// `w[i] = sin(π/2 · sin²((i+½)/l · π/2))`.
fn window_half(l: usize) -> Vec<f32> {
    (0..l)
        .map(|i| {
            let inner = ((i as f64 + 0.5) / l as f64) * (PI / 2.0);
            let s = inner.sin();
            (PI / 2.0 * (s * s)).sin() as f32
        })
        .collect()
}

/// The shared cosine quarter-wave: `cos(2π·m / (4·MAX_BLOCKSIZE))`,
/// `m = 0..=MAX_BLOCKSIZE`.
fn cos_quarter() -> Vec<f32> {
    let period = (4 * MAX_BLOCKSIZE) as f64;
    (0..=MAX_BLOCKSIZE)
        .map(|m| (2.0 * PI * m as f64 / period).cos() as f32)
        .collect()
}

/// Shared FFT twiddle resolution for the fast IMDCT: the largest radix-2 FFT a
/// block needs is `N/4 = MAX_BLOCKSIZE/4` points, and a length-`len` sub-DFT's
/// column-`j` twiddle is `VFFT_*[j·(VFFT_TW_N/len)]`.
const VFFT_TW_N: usize = MAX_BLOCKSIZE / 4;

/// FFT twiddle half-period: `fn(i) = trig(2π·i / VFFT_TW_N)`, `i = 0..VFFT_TW_N/2`
/// (every sub-DFT index stays `< VFFT_TW_N/2`). `cos` and `sin` variants.
fn fft_twiddles(sin: bool) -> Vec<f32> {
    let period = VFFT_TW_N as f64;
    (0..VFFT_TW_N / 2)
        .map(|i| {
            let th = 2.0 * PI * i as f64 / period;
            (if sin { th.sin() } else { th.cos() }) as f32
        })
        .collect()
}

/// Per-block-size IMDCT pre/post-rotation twiddle `trig(2π·(k+⅛)/N)`,
/// `k = 0..N/4` (`N` = block size) — the rotation an `N/4`-point FFT IMDCT folds
/// around its transform (same convention as the AAC filterbank). `cos`/`sin`.
fn mdct_twiddles(bs: usize, sin: bool) -> Vec<f32> {
    let n = bs as f64;
    (0..bs / 4)
        .map(|k| {
            let th = 2.0 * PI * (k as f64 + 0.125) / n;
            (if sin { th.sin() } else { th.cos() }) as f32
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
    out.push_str("// @generated by `cargo xtask gen-vorbis-tables` — do not edit.\n");
    out.push_str("// Vorbis decoder trig tables (window rising halves + IMDCT cosine quarter).\n");
    out.push_str("// See xtask/src/vorbis_tables.rs for layout/conventions.\n");
    writeln!(
        out,
        "pub(super) const COS_QUARTER_N: usize = {MAX_BLOCKSIZE};"
    )
    .unwrap();

    for e in BLOCKSIZE_EXPONENTS {
        let l = 1usize << (e - 1); // half of 2^e
        emit(&format!("WIN_{l}"), &window_half(l), &mut out);
    }
    emit("COS_QUARTER", &cos_quarter(), &mut out);

    // Fast IMDCT (N/4-point FFT) tables: a shared FFT twiddle half-period + the
    // per-block-size pre/post rotation twiddles.
    writeln!(out, "pub(super) const VFFT_TW_N: usize = {VFFT_TW_N};").unwrap();
    emit("VFFT_COS", &fft_twiddles(false), &mut out);
    emit("VFFT_SIN", &fft_twiddles(true), &mut out);
    for e in BLOCKSIZE_EXPONENTS {
        let bs = 1usize << e;
        emit(
            &format!("VMDCT_COS_{bs}"),
            &mdct_twiddles(bs, false),
            &mut out,
        );
        emit(
            &format!("VMDCT_SIN_{bs}"),
            &mdct_twiddles(bs, true),
            &mut out,
        );
    }
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
            "{TABLES_PATH} is stale — run `cargo xtask gen-vorbis-tables`"
        ))
    }
}
