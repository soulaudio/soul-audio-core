//! CELT PVQ vector dequantisation + spreading rotation — RFC 6716 §4.3.4.
//!
//! Turns the decoded integer pulse vector into the band's **unit-norm** MDCT
//! shape: [`alg_unquant`] reads the pulses ([`super::cwrs::decode_pulses`]),
//! scales them to unit norm × gain ([`normalise_residual`]), then undoes the
//! encoder's spreading rotation ([`exp_rotation`]).
//!
//! # Source correspondence (libopus **v1.5.2**, float build)
//!
//! - [`exp_rotation1`] / [`exp_rotation`] ← `celt/vq.c` (the Givens-rotation
//!   spreading; the fixed-point `PSHR32(MAC16_16(...),15)` collapses to plain
//!   `f32` multiply-add in the float build, per `celt/arch.h`).
//! - [`normalise_residual`] / [`renormalise_vector`] / [`extract_collapse_mask`]
//!   / [`alg_unquant`] ← `celt/vq.c` (decoder path only — `alg_quant`,
//!   `op_pvq_search`, `stereo_itheta` are encoder-only and omitted).
//! - [`celt_cos_norm`] ← the **fixed-point** `celt/mathops.c`
//!   `celt_cos_norm`/`_celt_cos_pi_2` polynomial (`L1..L4`), *not* the float
//!   build's `cos()` — this core forbids `libm`. Legitimate because
//!   [`exp_rotation`] runs *after* the range reads in [`super::cwrs::decode_pulses`],
//!   so its precision affects only audio quality (a high golden SNR), never coder
//!   sync. See the module-level note in the task report.
//! - [`celt_sqrt`] / [`celt_rsqrt`] ← the float `celt_sqrt`/`celt_rsqrt` (`1/√`),
//!   implemented via a bit-trick seed + Newton-Raphson (no `libm`), mirroring the
//!   existing `sbr::hfadj::sqrtf` precedent in this crate.

use crate::formats::opus::range::RangeDecoder;

use super::cwrs::{MAX_BAND_N, decode_pulses};

/// `SPREAD_NONE` (libopus `celt/bands.h`): no spreading rotation.
pub(crate) const SPREAD_NONE: u32 = 0;

/// Square root without `libm`: an exponent-halving seed refined by three
/// Newton-Raphson steps (`y = ½(y + x/y)`), the same construction as this
/// crate's `sbr::hfadj::sqrtf`. `x <= 0` / NaN return `0.0` (guarded so a
/// malformed band energy can't produce a NaN that propagates).
#[allow(clippy::neg_cmp_op_on_partial_ord)]
pub(crate) fn celt_sqrt(x: f32) -> f32 {
    if !(x > 0.0) {
        return 0.0;
    }
    let bits = x.to_bits();
    let exp = ((bits >> 23) & 0xFF) as i32 - 127;
    let half_exp = exp >> 1;
    let remainder = exp - 2 * half_exp;
    let pow2_half = f32::from_bits(((half_exp + 127) as u32) << 23);
    let mut y = if remainder != 0 {
        pow2_half * core::f32::consts::SQRT_2
    } else {
        pow2_half
    };
    y = 0.5 * (y + x / y);
    y = 0.5 * (y + x / y);
    y = 0.5 * (y + x / y);
    y
}

/// `1/√x` (libopus float `celt_rsqrt`/`celt_rsqrt_norm`). Returns `0.0` for a
/// non-positive argument (guarded, as [`celt_sqrt`]).
pub(crate) fn celt_rsqrt(x: f32) -> f32 {
    let s = celt_sqrt(x);
    if s > 0.0 { 1.0 / s } else { 0.0 }
}

// --- Fixed-point cos polynomial (celt/mathops.c) -------------------------

const L1: i32 = 32767;
const L2: i32 = -7651;
const L3: i32 = 8277;
const L4: i32 = -626;

/// libopus `celt/arch.h` `MULT16_16_P15` (fixed): `(16384 + a*b) >> 15`, with
/// `a`/`b` taken as 16-bit (the macro casts to `opus_int16`).
fn mult16_16_p15(a: i32, b: i32) -> i32 {
    (16384 + (a as i16 as i32) * (b as i16 as i32)) >> 15
}

/// libopus `celt/mathops.c` `_celt_cos_pi_2` — `cos(π/2 · x)` for a Q15 input
/// `x` (0..32768), result in Q15.
fn celt_cos_pi_2(x: i32) -> i32 {
    let x2 = mult16_16_p15(x, x);
    let inner = L3 + mult16_16_p15(L4, x2);
    let inner = L2 + mult16_16_p15(x2, inner);
    let val = (L1 - x2) + mult16_16_p15(x2, inner);
    1 + val.min(32766)
}

/// libopus `celt/mathops.c` `celt_cos_norm` (fixed-point) — `cos(π/2 · x)` for a
/// Q16 phase `x`, result in Q15 (`[-32767, 32767]`). Full-period reduction +
/// the exact-quadrant special cases are kept so any argument is valid.
fn celt_cos_norm_i(mut x: i32) -> i32 {
    x &= 0x0001_ffff;
    if x > (1 << 16) {
        x = (1 << 17) - x;
    }
    if x & 0x0000_7fff != 0 {
        if x < (1 << 15) {
            celt_cos_pi_2(x)
        } else {
            -celt_cos_pi_2(65536 - x)
        }
    } else if x & 0x0000_ffff != 0 {
        0
    } else if x & 0x0001_ffff != 0 {
        -32767
    } else {
        32767
    }
}

/// `cos(π/2 · x)` for `x` in `[0, 1]` as an `f32`, via the fixed-point Q15
/// polynomial ([`celt_cos_norm_i`]) — the `libm`-free substitute for the float
/// build's `celt_cos_norm(x) = cos(.5·π·x)` (see module docs). `x` is rounded to
/// Q15 before the polynomial, matching the fixed build's Q15 `theta`.
fn celt_cos_norm(x: f32) -> f32 {
    let xq = (x * 32768.0 + 0.5) as i32;
    celt_cos_norm_i(xq) as f32 * (1.0 / 32768.0)
}

/// libopus `celt/vq.c` `exp_rotation1` — one pass of the paired Givens rotations
/// (`stride` apart) that spread/gather pulse energy. Float build: the
/// `PSHR32(MAC16_16(...),15)` is a plain `f32` multiply-add.
fn exp_rotation1(x: &mut [f32], len: usize, stride: usize, c: f32, s: f32) {
    let ms = -s;
    // Forward: i in 0..len-stride.
    let fwd = len.saturating_sub(stride);
    for i in 0..fwd {
        let x1 = x[i];
        let x2 = x[i + stride];
        x[i + stride] = c * x2 + s * x1;
        x[i] = c * x1 + ms * x2;
    }
    // Backward: i from len-2*stride-1 down to 0 (skipped if that is negative).
    if len > 2 * stride {
        for i in (0..=(len - 2 * stride - 1)).rev() {
            let x1 = x[i];
            let x2 = x[i + stride];
            x[i + stride] = c * x2 + s * x1;
            x[i] = c * x1 + ms * x2;
        }
    }
}

/// libopus `celt/vq.c` `exp_rotation` — apply the spreading rotation to band `x`
/// of size `len`, split into `stride` interleaved MDCTs, with `k` pulses and
/// `spread` decision. `dir < 0` is the decode (inverse) direction. Returns
/// early for `2*K >= len` or `spread == SPREAD_NONE` (no rotation), exactly like
/// libopus.
fn exp_rotation(x: &mut [f32], mut len: usize, dir: i32, stride: usize, k: i32, spread: u32) {
    const SPREAD_FACTOR: [i32; 3] = [15, 10, 5];
    if k <= 0 || 2 * (k as usize) >= len || spread == SPREAD_NONE {
        return;
    }
    let factor = SPREAD_FACTOR[(spread - 1) as usize];
    let gain = (len as f32) / ((len as i32 + factor * k) as f32);
    let theta = 0.5 * (gain * gain);
    let c = celt_cos_norm(theta);
    let s = celt_cos_norm(1.0 - theta); // sin(theta·π/2)

    let mut stride2 = 0usize;
    if len >= 8 * stride {
        stride2 = 1;
        // sqrt(len/stride) with rounding: increment while (stride2+0.5)^2 < len/stride.
        while (stride2 * stride2 + stride2) * stride + (stride >> 2) < len {
            stride2 += 1;
        }
    }
    len /= stride;
    for i in 0..stride {
        let base = i * len;
        let seg = &mut x[base..base + len];
        if dir < 0 {
            if stride2 != 0 {
                exp_rotation1(seg, len, stride2, s, c);
            }
            exp_rotation1(seg, len, 1, c, s);
        } else {
            exp_rotation1(seg, len, 1, c, -s);
            if stride2 != 0 {
                exp_rotation1(seg, len, stride2, s, -c);
            }
        }
    }
}

/// libopus `celt/vq.c` `normalise_residual` — scale the integer pulse vector
/// `iy` to unit norm × `gain` into `x` (float build: `g = gain/√Ryy`,
/// `x[i] = g·iy[i]`).
fn normalise_residual(iy: &[i32], x: &mut [f32], n: usize, ryy: f32, gain: f32) {
    let g = celt_rsqrt(ryy) * gain;
    for i in 0..n {
        x[i] = g * (iy[i] as f32);
    }
}

/// libopus `celt/vq.c` `renormalise_vector` — rescale `x[0..n]` to norm `gain`
/// (float build). Used to normalise folded / LCG-noise-filled bands.
pub(crate) fn renormalise_vector(x: &mut [f32], n: usize, gain: f32) {
    let mut e = 1e-15f32; // EPSILON
    for &v in x.iter().take(n) {
        e += v * v;
    }
    let g = celt_rsqrt(e) * gain;
    for v in x.iter_mut().take(n) {
        *v *= g;
    }
}

/// libopus `celt/vq.c` `extract_collapse_mask` — one bit per interleaved MDCT
/// sub-block indicating whether it received any pulse (for Task-8 anti-collapse).
fn extract_collapse_mask(iy: &[i32], n: usize, b: usize) -> u32 {
    if b <= 1 {
        return 1;
    }
    let n0 = n / b;
    let mut collapse_mask = 0u32;
    for i in 0..b {
        let mut tmp = 0i32;
        for j in 0..n0 {
            tmp |= iy[i * n0 + j];
        }
        collapse_mask |= u32::from(tmp != 0) << i;
    }
    collapse_mask
}

/// libopus `celt/vq.c` `alg_unquant` — decode one band's normalised PVQ shape
/// into `x[0..n]` and return its collapse mask. `k` pulses, `b` interleaved
/// MDCTs, spreading `spread`, output scaled by `gain`. This is the per-band
/// shape decode ("`decode_band_shape`") the [`super::bands`] loop calls at each
/// leaf.
pub(crate) fn alg_unquant(
    x: &mut [f32],
    n: usize,
    k: i32,
    spread: u32,
    b: usize,
    rd: &mut RangeDecoder,
    gain: f32,
) -> u32 {
    let mut iy = [0i32; MAX_BAND_N];
    let ryy = decode_pulses(&mut iy[..n], n, k, rd);
    normalise_residual(&iy, x, n, ryy, gain);
    exp_rotation(x, n, -1, b, k, spread);
    extract_collapse_mask(&iy, n, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn celt_sqrt_is_accurate() {
        for &v in &[1.0f32, 2.0, 4.0, 9.0, 176.0, 1e-6, 1e6, 0.25, 123.456] {
            let got = celt_sqrt(v);
            let want = libm_like_sqrt(v);
            assert!(
                (got - want).abs() <= 1e-4 * want.max(1.0),
                "sqrt({v}): {got} vs {want}"
            );
        }
        assert_eq!(celt_sqrt(0.0), 0.0);
        assert_eq!(celt_sqrt(-1.0), 0.0);
        assert_eq!(celt_sqrt(f32::NAN), 0.0);
    }

    /// f64 reference sqrt via Newton (test-only oracle, avoids std `f32::sqrt`
    /// dependency debates — this is a plain numeric check).
    fn libm_like_sqrt(x: f32) -> f32 {
        if x <= 0.0 {
            return 0.0;
        }
        let mut y = x as f64;
        for _ in 0..60 {
            y = 0.5 * (y + x as f64 / y);
        }
        y as f32
    }

    #[test]
    fn celt_cos_norm_matches_cosine() {
        // cos(π/2 · x) for x in [0,1] via the Q15 polynomial, checked against a
        // Taylor-series cosine oracle (no libm). Fixed-point ~Q15 accuracy.
        for i in 0..=20 {
            let x = i as f32 / 20.0;
            let got = celt_cos_norm(x);
            let want = cos_taylor(core::f32::consts::FRAC_PI_2 * x);
            assert!((got - want).abs() < 3e-3, "cos_norm({x}): {got} vs {want}");
        }
    }

    /// cos via range-reduced Taylor series (test oracle, no libm).
    fn cos_taylor(a: f32) -> f32 {
        let a = a as f64;
        let mut term = 1.0f64;
        let mut sum = 1.0f64;
        let x2 = a * a;
        for n in 1..=12 {
            term *= -x2 / (((2 * n - 1) * (2 * n)) as f64);
            sum += term;
        }
        sum as f32
    }

    #[test]
    fn normalise_residual_gives_unit_norm() {
        // A pulse vector normalised with gain=1 has ||x|| == 1.
        let iy = [3i32, -1, 0, 2, -1];
        let n = iy.len();
        let ryy: f32 = iy.iter().map(|&v| (v * v) as f32).sum();
        let mut x = [0.0f32; 5];
        normalise_residual(&iy, &mut x, n, ryy, 1.0);
        let norm: f32 = x.iter().map(|&v| v * v).sum::<f32>();
        assert!((norm - 1.0).abs() < 1e-5, "norm={norm}");
    }

    #[test]
    fn exp_rotation_preserves_norm() {
        // The spreading rotation is orthonormal in exact arithmetic; with the
        // Q15 cos polynomial (c^2+s^2 != 1 exactly) it preserves ||x|| only
        // approximately — the polynomial's ~Q15 precision (a few 1e-4), which is
        // the same tradeoff libopus's fixed-point build makes and is well within
        // the golden-SNR budget (see module docs).
        let mut x = [0.4f32, -0.3, 0.5, 0.1, -0.6, 0.2, 0.15, -0.25];
        let before: f32 = x.iter().map(|&v| v * v).sum();
        exp_rotation(&mut x, 8, -1, 1, 2, super::super::allocation::SPREAD_NORMAL);
        let after: f32 = x.iter().map(|&v| v * v).sum();
        assert!(
            (before - after).abs() < 3e-3,
            "norm changed: {before} -> {after}"
        );
    }

    #[test]
    fn exp_rotation_noop_when_2k_ge_len() {
        let mut x = [0.5f32, 0.5, 0.5, 0.5];
        let orig = x;
        exp_rotation(&mut x, 4, -1, 1, 2, super::super::allocation::SPREAD_NORMAL); // 2*2>=4
        assert_eq!(x, orig);
    }

    #[test]
    fn extract_collapse_mask_flags_nonzero_subblocks() {
        // B=4, N=8 -> 2 samples/subblock. Pulses only in subblocks 0 and 2.
        let iy = [1i32, 0, 0, 0, -2, 0, 0, 0];
        assert_eq!(extract_collapse_mask(&iy, 8, 4), 0b0101);
        // B<=1 always returns 1.
        assert_eq!(extract_collapse_mask(&iy, 8, 1), 1);
    }

    #[test]
    fn alg_unquant_produces_unit_norm_finite_shape() {
        let bytes = [0x9au8, 0x3c, 0xf1, 0x05, 0xd7, 0x2b, 0x88, 0x60];
        let mut rd = RangeDecoder::new(&bytes);
        let mut x = [0.0f32; MAX_BAND_N];
        let n = 16;
        alg_unquant(
            &mut x[..n],
            n,
            5,
            super::super::allocation::SPREAD_NORMAL,
            1,
            &mut rd,
            1.0,
        );
        let norm: f32 = x[..n].iter().map(|&v| v * v).sum();
        assert!(x[..n].iter().all(|v| v.is_finite()));
        assert!((norm - 1.0).abs() < 1e-4, "||x||^2 = {norm}");
    }
}
