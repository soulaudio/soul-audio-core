//! CELT inverse MDCT + overlap-add (RFC 6716 §4.3.7).
//!
//! [`imdct_block`] is a faithful reproduction of libopus **v1.5.2**
//! `celt/mdct.c` `clt_mdct_backward`: pre-rotation → a **mixed-radix kiss-FFT**
//! ([`opus_fft_impl`], `O(N log N)`) → post-rotation → the
//! time-domain-aliasing-cancellation (TDAC) window mirror. Earlier the middle
//! stage was a direct `O(N²)` DFT (the correctness oracle); this replaces it
//! with libopus's own FFT — the *exact same linear transform* (hence the exact
//! same unnormalised scale — `opus_fft_impl` carries no `1/N` factor, matching
//! ffmpeg's libopus decode), only far cheaper. A correct FFT equals the DFT up
//! to float rounding, so the golden / conformance gates are unchanged (issue
//! #38); the in-file f64 cross-check pins every index/sign against an
//! independent direct-DFT reference.
//!
//! # The FFT
//!
//! The CELT MDCT sizes are `N4 = N/4 ∈ {60,120,240,480}` (`= 2^k·3·5`), exactly
//! the mixed-radix sizes libopus kiss-FFT factors as radix 4/2/3/5. Per size,
//! the stage factors, the bit-reversal permutation, and the forward twiddles
//! `exp(-j2πk/N4)` are generated host-side into [`crate::formats::opus::opus_tables`]
//! (`cargo xtask gen-opus-tables`); the `no_std` core runs the butterflies
//! ([`kf_bfly2`]/[`kf_bfly3`]/[`kf_bfly4`]/[`kf_bfly5`]) over those `const`
//! tables with no runtime trig, no `libm`, no alloc, no `unsafe`. The FFT
//! scratch is a fixed `MAX_N4`-complex stack buffer. libopus uses a *forward*
//! FFT for the *inverse* MDCT by swapping re/imag on the FFT input and output
//! (`opus_fft_impl` is the forward kernel); the pre/post-rotation below do that
//! swap, so we transcribe `opus_fft_impl`, not `opus_ifft`.
//!
//! # Twiddles without `libm`
//!
//! The pre/post-rotation twiddles `cos(2π(j+1/8)/N)` (and `sin`) are evaluated
//! at runtime by [`cos_sin_turn`] — a quadrant reduction plus a degree-11 Taylor
//! pair in `f64` (≈1e-7, i.e. ~130 dB, far above the golden bar). Precision here
//! is post-range-coder, so it affects only audio quality, never bitstream sync.
//! The FFT twiddles are pretabulated (no trig at all).

use crate::formats::opus::opus_tables::{
    MDCT_WINDOW, OPUS_FFT_BITREV_60, OPUS_FFT_BITREV_120, OPUS_FFT_BITREV_240, OPUS_FFT_BITREV_480,
    OPUS_FFT_FACTORS_60, OPUS_FFT_FACTORS_120, OPUS_FFT_FACTORS_240, OPUS_FFT_FACTORS_480,
    OPUS_FFT_TWIDDLES_60, OPUS_FFT_TWIDDLES_120, OPUS_FFT_TWIDDLES_240, OPUS_FFT_TWIDDLES_480,
};

/// The CELT MDCT overlap window — 120 samples, rising `0 → 1` (libopus
/// `static_modes_float.h` `window120`). All four frame sizes share this single
/// 120-sample low-overlap window.
pub(crate) const WINDOW: &[f32; 120] = &MDCT_WINDOW;

/// CELT's fixed MDCT overlap length (libopus `mode->overlap`), independent of
/// the frame size — the low-overlap MDCT tapers only these edge samples.
pub(crate) const OVERLAP: usize = 120;

/// Largest per-block half-size `N4 = N2/2` any standard-mode block reaches: the
/// non-transient 20 ms long block has `N2 = 960`, so `N4 = 480`.
const MAX_N4: usize = 480;

/// `(cos(2π·t), sin(2π·t))` for `t ∈ [0, 1)`, without `libm`. Quadrant reduction
/// to `[0, π/2)` then a Taylor pair evaluated in `f64` for headroom. Inputs
/// outside `[0, 1)` are folded by their fractional turn (all call sites already
/// pass `[0, 1)`).
fn cos_sin_turn(t: f32) -> (f32, f32) {
    // Fold to [0,1) via an integer-cast floor (no libm `floor`); t >= 0 here.
    let mut tt = t as f64;
    if !(0.0..1.0).contains(&tt) {
        tt -= (tt as i64) as f64;
        if tt < 0.0 {
            tt += 1.0;
        }
    }
    let scaled = tt * 4.0; // [0, 4)
    let qi = scaled as i32; // floor, since scaled >= 0
    let q = qi & 3;
    let r = scaled - f64::from(qi); // [0, 1) — fraction of a quarter turn
    let a = core::f64::consts::FRAC_PI_2 * r; // [0, π/2)
    let (c, s) = cos_sin_small(a);
    let (cos, sin) = match q {
        0 => (c, s),
        1 => (-s, c),
        2 => (-c, -s),
        _ => (s, -c),
    };
    (cos as f32, sin as f32)
}

/// `(cos a, sin a)` for `a ∈ [0, π/2)` via Taylor series (6 cos / 6 sin terms →
/// ≈1e-7 over the range).
fn cos_sin_small(a: f64) -> (f64, f64) {
    let a2 = a * a;
    let cos = 1.0
        + a2 * (-1.0 / 2.0
            + a2 * (1.0 / 24.0
                + a2 * (-1.0 / 720.0 + a2 * (1.0 / 40320.0 + a2 * (-1.0 / 3628800.0)))));
    let sin = a
        * (1.0
            + a2 * (-1.0 / 6.0
                + a2 * (1.0 / 120.0
                    + a2 * (-1.0 / 5040.0 + a2 * (1.0 / 362880.0 + a2 * (-1.0 / 39916800.0))))));
    (cos, sin)
}

// ---------------------------------------------------------------------------
// Mixed-radix kiss-FFT (libopus celt/kiss_fft.c `opus_fft_impl`). Operates
// in-place on a complex array whose input was already scattered by the
// bit-reversal permutation; produces the natural-order forward DFT. Radices
// 2/3/4/5 cover every CELT MDCT size. Pure f32 arithmetic over pretabulated
// twiddles — no trig, no alloc, no unsafe.
// ---------------------------------------------------------------------------

/// A complex sample (`kiss_fft_cpx`).
#[derive(Clone, Copy)]
struct Cpx {
    r: f32,
    i: f32,
}

impl Cpx {
    const ZERO: Cpx = Cpx { r: 0.0, i: 0.0 };
}

#[inline]
fn c_add(a: Cpx, b: Cpx) -> Cpx {
    Cpx {
        r: a.r + b.r,
        i: a.i + b.i,
    }
}

#[inline]
fn c_sub(a: Cpx, b: Cpx) -> Cpx {
    Cpx {
        r: a.r - b.r,
        i: a.i - b.i,
    }
}

/// Complex multiply (`C_MUL`): `a·b`.
#[inline]
fn c_mul(a: Cpx, b: Cpx) -> Cpx {
    Cpx {
        r: a.r * b.r - a.i * b.i,
        i: a.r * b.i + a.i * b.r,
    }
}

/// Read twiddle `k` from the interleaved `[re, im, …]` table.
#[inline]
fn tw(t: &[f32], k: usize) -> Cpx {
    Cpx {
        r: t[2 * k],
        i: t[2 * k + 1],
    }
}

/// libopus `kf_bfly2` (non-`CUSTOM_MODES` path): a radix-2 stage specialised for
/// `m == 4`, so its four twiddles are the hardcoded eighth-roots (kf_factor
/// guarantees any lone radix-2 stage lands here). `n = fstride[i]` groups.
fn kf_bfly2(fout: &mut [Cpx], m: usize, n: usize) {
    debug_assert_eq!(m, 4);
    let tw8 = core::f32::consts::FRAC_1_SQRT_2; // cos(π/4) = sin(π/4)
    for grp in 0..n {
        let base = 2 * m * grp; // Fout advances by 2m == 8
        // k = 0: twiddle 1.
        let t = fout[base + m];
        fout[base + m] = c_sub(fout[base], t);
        fout[base] = c_add(fout[base], t);
        // k = 1: twiddle exp(-jπ/4).
        let f = fout[base + m + 1];
        let t = Cpx {
            r: (f.r + f.i) * tw8,
            i: (f.i - f.r) * tw8,
        };
        fout[base + m + 1] = c_sub(fout[base + 1], t);
        fout[base + 1] = c_add(fout[base + 1], t);
        // k = 2: twiddle exp(-jπ/2) = -j.
        let f = fout[base + m + 2];
        let t = Cpx { r: f.i, i: -f.r };
        fout[base + m + 2] = c_sub(fout[base + 2], t);
        fout[base + 2] = c_add(fout[base + 2], t);
        // k = 3: twiddle exp(-j3π/4).
        let f = fout[base + m + 3];
        let t = Cpx {
            r: (f.i - f.r) * tw8,
            i: -(f.i + f.r) * tw8,
        };
        fout[base + m + 3] = c_sub(fout[base + 3], t);
        fout[base + 3] = c_add(fout[base + 3], t);
    }
}

/// libopus `kf_bfly4` — radix-4 stage. `m == 1` is the twiddle-free base case;
/// otherwise each of the `m` sub-transforms multiplies by twiddles strided by
/// `fstride` (1×/2×/3×).
fn kf_bfly4(fout: &mut [Cpx], twiddles: &[f32], m: usize, fstride: usize, n: usize, mm: usize) {
    if m == 1 {
        for grp in 0..n {
            let base = 4 * grp;
            let scratch0 = c_sub(fout[base], fout[base + 2]);
            fout[base] = c_add(fout[base], fout[base + 2]);
            let scratch1 = c_add(fout[base + 1], fout[base + 3]);
            fout[base + 2] = c_sub(fout[base], scratch1);
            fout[base] = c_add(fout[base], scratch1);
            let scratch1 = c_sub(fout[base + 1], fout[base + 3]);
            fout[base + 1] = Cpx {
                r: scratch0.r + scratch1.i,
                i: scratch0.i - scratch1.r,
            };
            fout[base + 3] = Cpx {
                r: scratch0.r - scratch1.i,
                i: scratch0.i + scratch1.r,
            };
        }
    } else {
        let m2 = 2 * m;
        let m3 = 3 * m;
        for grp in 0..n {
            let fbeg = grp * mm;
            let mut tw1 = 0usize;
            let mut tw2 = 0usize;
            let mut tw3 = 0usize;
            for j in 0..m {
                let idx = fbeg + j;
                let s0 = c_mul(fout[idx + m], tw(twiddles, tw1));
                let s1 = c_mul(fout[idx + m2], tw(twiddles, tw2));
                let s2 = c_mul(fout[idx + m3], tw(twiddles, tw3));
                let s5 = c_sub(fout[idx], s1);
                fout[idx] = c_add(fout[idx], s1);
                let s3 = c_add(s0, s2);
                let s4 = c_sub(s0, s2);
                fout[idx + m2] = c_sub(fout[idx], s3);
                tw1 += fstride;
                tw2 += 2 * fstride;
                tw3 += 3 * fstride;
                fout[idx] = c_add(fout[idx], s3);
                fout[idx + m] = Cpx {
                    r: s5.r + s4.i,
                    i: s5.i - s4.r,
                };
                fout[idx + m3] = Cpx {
                    r: s5.r - s4.i,
                    i: s5.i + s4.r,
                };
            }
        }
    }
}

/// libopus `kf_bfly3` — radix-3 stage. `epi3 = twiddles[fstride·m]` is the
/// primitive cube root `exp(-j2π/3)`; only its imaginary part is used.
fn kf_bfly3(fout: &mut [Cpx], twiddles: &[f32], m: usize, fstride: usize, n: usize, mm: usize) {
    let m2 = 2 * m;
    let epi3_i = tw(twiddles, fstride * m).i;
    for grp in 0..n {
        let fbeg = grp * mm;
        let mut tw1 = 0usize;
        let mut tw2 = 0usize;
        for j in 0..m {
            let idx = fbeg + j;
            let s1 = c_mul(fout[idx + m], tw(twiddles, tw1));
            let s2 = c_mul(fout[idx + m2], tw(twiddles, tw2));
            let s3 = c_add(s1, s2);
            let s0 = c_sub(s1, s2);
            tw1 += fstride;
            tw2 += 2 * fstride;
            let fm_r = fout[idx].r - 0.5 * s3.r;
            let fm_i = fout[idx].i - 0.5 * s3.i;
            let s0 = Cpx {
                r: s0.r * epi3_i,
                i: s0.i * epi3_i,
            };
            fout[idx] = c_add(fout[idx], s3);
            fout[idx + m2] = Cpx {
                r: fm_r + s0.i,
                i: fm_i - s0.r,
            };
            fout[idx + m] = Cpx {
                r: fm_r - s0.i,
                i: fm_i + s0.r,
            };
        }
    }
}

/// libopus `kf_bfly5` — radix-5 stage. `ya = twiddles[fstride·m]` and
/// `yb = twiddles[fstride·2m]` are `exp(-j2π/5)` and `exp(-j4π/5)`.
fn kf_bfly5(fout: &mut [Cpx], twiddles: &[f32], m: usize, fstride: usize, n: usize, mm: usize) {
    let ya = tw(twiddles, fstride * m);
    let yb = tw(twiddles, fstride * 2 * m);
    for grp in 0..n {
        let fbeg = grp * mm;
        for u in 0..m {
            let i0 = fbeg + u;
            let i1 = i0 + m;
            let i2 = i0 + 2 * m;
            let i3 = i0 + 3 * m;
            let i4 = i0 + 4 * m;
            let s0 = fout[i0];
            let s1 = c_mul(fout[i1], tw(twiddles, u * fstride));
            let s2 = c_mul(fout[i2], tw(twiddles, 2 * u * fstride));
            let s3 = c_mul(fout[i3], tw(twiddles, 3 * u * fstride));
            let s4 = c_mul(fout[i4], tw(twiddles, 4 * u * fstride));

            let s7 = c_add(s1, s4);
            let s10 = c_sub(s1, s4);
            let s8 = c_add(s2, s3);
            let s9 = c_sub(s2, s3);

            fout[i0] = Cpx {
                r: s0.r + s7.r + s8.r,
                i: s0.i + s7.i + s8.i,
            };

            let s5 = Cpx {
                r: s0.r + s7.r * ya.r + s8.r * yb.r,
                i: s0.i + s7.i * ya.r + s8.i * yb.r,
            };
            let s6 = Cpx {
                r: s10.i * ya.i + s9.i * yb.i,
                i: -(s10.r * ya.i + s9.r * yb.i),
            };
            fout[i1] = c_sub(s5, s6);
            fout[i4] = c_add(s5, s6);

            let s11 = Cpx {
                r: s0.r + s7.r * yb.r + s8.r * ya.r,
                i: s0.i + s7.i * yb.r + s8.i * ya.r,
            };
            let s12 = Cpx {
                r: s9.i * ya.i - s10.i * yb.i,
                i: s10.r * yb.i - s9.r * ya.i,
            };
            fout[i2] = c_add(s11, s12);
            fout[i3] = c_sub(s11, s12);
        }
    }
}

/// libopus `opus_fft_impl` — the in-place mixed-radix FFT driver. Reads the
/// stage `factors` (`[p, m, …]`), computes the per-stage decimation stride, and
/// dispatches each stage to its radix butterfly. `st->shift` is 0 for these
/// standalone per-size states, so twiddle strides are just `fstride[i]`.
fn opus_fft_impl(factors: &[i16], twiddles: &[f32], fout: &mut [Cpx]) {
    let mut fstride = [0usize; MAXFACTORS];
    fstride[0] = 1;
    let mut l = 0usize;
    let mut m;
    loop {
        let p = factors[2 * l] as usize;
        m = factors[2 * l + 1] as usize;
        fstride[l + 1] = fstride[l] * p;
        l += 1;
        if m == 1 {
            break;
        }
    }
    let mut m = factors[2 * l - 1] as usize;
    let mut i = l as isize - 1;
    while i >= 0 {
        let ii = i as usize;
        let m2 = if ii != 0 {
            factors[2 * ii - 1] as usize
        } else {
            1
        };
        match factors[2 * ii] {
            2 => kf_bfly2(fout, m, fstride[ii]),
            4 => kf_bfly4(fout, twiddles, m, fstride[ii], fstride[ii], m2),
            3 => kf_bfly3(fout, twiddles, m, fstride[ii], fstride[ii], m2),
            5 => kf_bfly5(fout, twiddles, m, fstride[ii], fstride[ii], m2),
            _ => {}
        }
        m = m2;
        i -= 1;
    }
}

/// `MAXFACTORS` (libopus `_kiss_fft_guts.h`) — our sizes need at most 5 stages.
const MAXFACTORS: usize = 8;

/// The kiss-FFT config `(factors, bitrev, twiddles)` for a CELT MDCT half-size
/// `n4`. Only the four standard-mode sizes exist; anything else returns `None`
/// (a malformed size is then a no-op, matching the guards in [`imdct_block`]).
fn fft_config(n4: usize) -> Option<(&'static [i16], &'static [i16], &'static [f32])> {
    match n4 {
        60 => Some((
            &OPUS_FFT_FACTORS_60,
            &OPUS_FFT_BITREV_60,
            &OPUS_FFT_TWIDDLES_60,
        )),
        120 => Some((
            &OPUS_FFT_FACTORS_120,
            &OPUS_FFT_BITREV_120,
            &OPUS_FFT_TWIDDLES_120,
        )),
        240 => Some((
            &OPUS_FFT_FACTORS_240,
            &OPUS_FFT_BITREV_240,
            &OPUS_FFT_TWIDDLES_240,
        )),
        480 => Some((
            &OPUS_FFT_FACTORS_480,
            &OPUS_FFT_BITREV_480,
            &OPUS_FFT_TWIDDLES_480,
        )),
        _ => None,
    }
}

/// One CELT inverse-MDCT block (libopus `clt_mdct_backward`) via the fast FFT.
///
/// - `freq`: the `n2` frequency lines for this block, read with `stride` (the
///   interleave factor `B`: 1 for a long block, `M` per short block).
/// - `n2`: the number of frequency lines (`= mode->mdct.n >> shift >> 1`).
/// - `out`: the destination sub-slice (the channel's decode buffer at this
///   block's offset). On entry `out[0 .. OVERLAP/2)` holds the previous block's
///   overlap tail; on return `out[0 .. OVERLAP/2 + n2)` holds the windowed,
///   overlap-added time samples (the TDAC mirror folds the first `OVERLAP`).
///
/// Guarded so a malformed `n2`/short buffer is a no-op rather than a panic.
pub(crate) fn imdct_block(freq: &[f32], n2: usize, stride: usize, out: &mut [f32]) {
    let n4 = n2 / 2;
    if n4 == 0 || n4 > MAX_N4 || stride == 0 {
        return;
    }
    let n = 2 * n2; // full MDCT size N (twiddle denominator)
    // The raw region needs OVERLAP/2 + n2; the TDAC mirror spans the first
    // OVERLAP samples — for every real CELT block (n2 >= 120) the former covers
    // the latter, but guard both so a malformed size can't index out of bounds.
    if out.len() < core::cmp::max(OVERLAP, OVERLAP / 2 + n2) || freq.len() < (n2 - 1) * stride + 1 {
        return;
    }
    let Some((factors, bitrev, twiddles)) = fft_config(n4) else {
        return; // not a standard CELT MDCT size
    };

    // Pre-rotation, scattered by the FFT bit-reversal (libopus fuses these):
    // fin[i] = (freq[2i] + j·freq[N2-1-2i])·e^{-jθ_i}, stored with re/imag
    // swapped (re=yi, im=yr) so the *forward* FFT computes the *inverse* MDCT.
    let mut fx = [Cpx::ZERO; MAX_N4];
    for i in 0..n4 {
        let (cos_t, sin_t) = cos_sin_turn((i as f32 + 0.125) / n as f32); // θ_i
        let a = freq[(2 * i) * stride];
        let b = freq[(n2 - 1 - 2 * i) * stride];
        let re = a * cos_t + b * sin_t; // libopus yi (real lane)
        let im = b * cos_t - a * sin_t; // libopus yr (imag lane)
        let rev = bitrev[i] as usize;
        if rev < n4 {
            fx[rev] = Cpx { r: re, i: im };
        }
    }

    // Unnormalised forward FFT of size N4 (== opus_fft_impl, no 1/N scaling):
    // fx[m] = Σ_i fin[i]·e^{-j2πim/N4}, in natural order.
    opus_fft_impl(factors, twiddles, &mut fx[..n4]);

    // Post-rotation + de-shuffle from both ends (libopus writes into
    // out[OVERLAP/2 ..]). t[j] = cos(2π(j+1/8)/N); t[N4+i] = -sin(θ_i). The FFT
    // output lane swap (re=fx.im, im=fx.re) undoes the input swap above.
    let base = OVERLAP / 2;
    let half = n4.div_ceil(2);
    for i in 0..half {
        // Front (uses fx[i]); re=fx[i].im, im=fx[i].re.
        let (c0, s0) = cos_sin_turn((i as f32 + 0.125) / n as f32); // t[i]=c0, t[N4+i]=-s0
        let re = fx[i].i;
        let im = fx[i].r;
        let yr_f = re * c0 - im * s0; //  re*t[i] + im*t[N4+i]
        let yi_f = -re * s0 - im * c0; //  re*t[N4+i] - im*t[i]
        out[base + 2 * i] = yr_f;
        out[base + (n2 - 1 - 2 * i)] = yi_f;

        // Back (uses fx[N4-1-i]); t[j] here is a pure cosine (libopus stores only
        // cosines — the sines come from the N4/N2 index offsets, already folded
        // into the front terms above).
        let ci = n4 - i - 1;
        let t0b = t_cos(n4 - i - 1, n); // t[N4-i-1]
        let t1b = t_cos(n2 - i - 1, n); // t[N2-i-1]
        let re2 = fx[ci].i;
        let im2 = fx[ci].r;
        let yr_b = re2 * t0b + im2 * t1b;
        let yi_b = re2 * t1b - im2 * t0b;
        out[base + (n2 - 2 - 2 * i)] = yr_b;
        out[base + 2 * i + 1] = yi_b;
    }

    // TDAC window mirror over the first OVERLAP samples: fold this block's
    // leading edge with the previous block's stored tail (out[0..OVERLAP/2)).
    for i in 0..OVERLAP / 2 {
        let x1 = out[OVERLAP - 1 - i]; // this block's new leading edge
        let x2 = out[i]; // previous overlap tail
        let w_i = WINDOW[i];
        let w_hi = WINDOW[OVERLAP - 1 - i];
        out[i] = w_hi * x2 - w_i * x1;
        out[OVERLAP - 1 - i] = w_i * x2 + w_hi * x1;
    }
}

/// `t[j] = cos(2π(j+1/8)/N)` — the CELT MDCT post-rotation twiddle (cosine only).
#[inline]
fn t_cos(j: usize, n: usize) -> f32 {
    cos_sin_turn((j as f32 + 0.125) / n as f32).0
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::f64::consts::PI;

    #[test]
    fn cos_sin_turn_matches_reference() {
        // Compare against an independent f64 Taylor cosine/sine oracle.
        for k in 0..1000 {
            let t = k as f32 / 1000.0;
            let (c, s) = cos_sin_turn(t);
            let ang = 2.0 * PI * t as f64;
            let want_c = ang.cos();
            let want_s = ang.sin();
            assert!((c as f64 - want_c).abs() < 1e-5, "cos({t}) {c} vs {want_c}");
            assert!((s as f64 - want_s).abs() < 1e-5, "sin({t}) {s} vs {want_s}");
        }
    }

    /// Cross-check `imdct_block` (the fast FFT path) against an independent `f64`
    /// re-implementation of the *same* three-stage algorithm (pre-rotate →
    /// unnormalised **direct DFT** → post-rotate → TDAC mirror) built with `std`
    /// trig. A correct FFT equals the DFT up to float rounding, so this pins the
    /// FFT (bit-reversal, twiddles, radix-3/5 butterflies), the no-`libm`
    /// `cos_sin_turn` precision, and every index/sign in the transcription (any
    /// typo diverges). The transform's convention & absolute scale vs libopus are
    /// pinned separately by the end-to-end golden in tests/decode.rs.
    #[test]
    fn imdct_block_matches_f64_reference_algorithm() {
        for &n2 in &[120usize, 240, 480, 960] {
            let n4 = n2 / 2;
            let n = 2 * n2;
            // Deterministic frequency input.
            let freq: Vec<f32> = (0..n2)
                .map(|k| {
                    (0.3 * (0.7 * k as f64).sin() - 0.2 * (0.13 * k as f64 + 1.0).cos()) as f32
                })
                .collect();
            // Random-ish previous overlap tail.
            let prev_tail: Vec<f32> = (0..OVERLAP / 2)
                .map(|i| (0.11 * i as f64).sin() as f32 * 0.4)
                .collect();

            // --- f64 reference of the identical algorithm (direct DFT core) ---
            let tc = |j: usize| ((2.0 * PI * (j as f64 + 0.125)) / n as f64).cos();
            let ts = |j: usize| ((2.0 * PI * (j as f64 + 0.125)) / n as f64).sin();
            let mut fin_re = vec![0.0f64; n4];
            let mut fin_im = vec![0.0f64; n4];
            for i in 0..n4 {
                let ct = tc(i);
                let st = ts(i);
                let a = freq[2 * i] as f64;
                let b = freq[n2 - 1 - 2 * i] as f64;
                fin_re[i] = a * ct + b * st;
                fin_im[i] = b * ct - a * st;
            }
            let mut g_re = vec![0.0f64; n4];
            let mut g_im = vec![0.0f64; n4];
            for m in 0..n4 {
                for i in 0..n4 {
                    let w = 2.0 * PI * ((i * m) % n4) as f64 / n4 as f64;
                    let (c, s) = (w.cos(), w.sin());
                    g_re[m] += fin_re[i] * c + fin_im[i] * s;
                    g_im[m] += fin_im[i] * c - fin_re[i] * s;
                }
            }
            let mut refbuf = vec![0.0f64; OVERLAP / 2 + n2];
            for (i, &v) in prev_tail.iter().enumerate() {
                refbuf[i] = v as f64;
            }
            let base = OVERLAP / 2;
            for i in 0..n4.div_ceil(2) {
                let (c0, s0) = (tc(i), ts(i));
                let re = g_im[i];
                let im = g_re[i];
                refbuf[base + 2 * i] = re * c0 - im * s0;
                refbuf[base + (n2 - 1 - 2 * i)] = -re * s0 - im * c0;
                let ci = n4 - i - 1;
                let t0b = tc(n4 - i - 1);
                let t1b = tc(n2 - i - 1);
                let re2 = g_im[ci];
                let im2 = g_re[ci];
                refbuf[base + (n2 - 2 - 2 * i)] = re2 * t0b + im2 * t1b;
                refbuf[base + 2 * i + 1] = re2 * t1b - im2 * t0b;
            }
            for i in 0..OVERLAP / 2 {
                let x1 = refbuf[OVERLAP - 1 - i];
                let x2 = refbuf[i];
                let w_i = WINDOW[i] as f64;
                let w_hi = WINDOW[OVERLAP - 1 - i] as f64;
                refbuf[i] = w_hi * x2 - w_i * x1;
                refbuf[OVERLAP - 1 - i] = w_i * x2 + w_hi * x1;
            }

            // --- implementation under test (fast FFT) ---
            let mut buf = vec![0.0f32; OVERLAP / 2 + n2];
            buf[..OVERLAP / 2].copy_from_slice(&prev_tail);
            imdct_block(&freq, n2, 1, &mut buf);

            let scale: f64 = refbuf.iter().map(|v| v.abs()).fold(0.0, f64::max).max(1e-6);
            for (i, (&got, &want)) in buf.iter().zip(refbuf.iter()).enumerate() {
                assert!(
                    (got as f64 - want).abs() < 1e-5 * scale,
                    "n2={n2} idx={i}: {got} vs {want}"
                );
            }
        }
    }
}
