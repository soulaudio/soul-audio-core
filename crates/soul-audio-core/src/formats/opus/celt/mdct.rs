//! CELT inverse MDCT + overlap-add (RFC 6716 §4.3.7).
//!
//! [`imdct_block`] is a **direct** (O(N²)) reproduction of libopus **v1.5.2**
//! `celt/mdct.c` `clt_mdct_backward`: pre-rotation, an unnormalised DFT (the raw
//! `opus_fft_impl`, which carries no `1/N` scaling — exactly why the decoder's
//! absolute output amplitude is fixed by the algorithm and matches ffmpeg's own
//! libopus decode), post-rotation, then the time-domain-aliasing-cancellation
//! (TDAC) window mirror. Replacing the fast FFT with a direct DFT keeps the
//! *exact same linear transform* (hence the exact same scale — no `n/2`-class
//! normalisation ambiguity), only slower; a split-radix FFT is a later
//! perf-only task, gated by this task's golden. This mirrors the project's
//! Vorbis precedent (a naïve direct IMDCT as the correctness oracle first).
//!
//! # Twiddles without `libm`
//!
//! The pre/post-rotation twiddles `cos(2π(j+1/8)/N)` and the DFT twiddles
//! `cos/sin(2πk/N4)` are evaluated at runtime by [`cos_sin_turn`] — a quadrant
//! reduction plus a degree-11 Taylor pair in `f64` (≈1e-7, i.e. ~130 dB, far
//! above the golden bar). Precision here is post-range-coder, so it affects only
//! audio quality, never bitstream sync (see the module note in the Task-8
//! report). No `libm`, no `std`, no runtime table generation.

use crate::formats::opus::opus_tables::MDCT_WINDOW;

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

/// One CELT inverse-MDCT block (libopus `clt_mdct_backward`), computed directly.
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

    // Pre-rotation: fin[i] = (freq[2i] + j·freq[N2-1-2i])·e^{-jθ_i}, stored (per
    // libopus's re/imag swap for using an FFT as an IFFT) as re=yi, im=yr.
    let mut fin_re = [0.0f32; MAX_N4];
    let mut fin_im = [0.0f32; MAX_N4];
    for i in 0..n4 {
        let (cos_t, sin_t) = cos_sin_turn((i as f32 + 0.125) / n as f32); // θ_i
        let a = freq[(2 * i) * stride];
        let b = freq[(n2 - 1 - 2 * i) * stride];
        fin_re[i] = a * cos_t + b * sin_t; // yi
        fin_im[i] = b * cos_t - a * sin_t; // yr
    }

    // Unnormalised forward DFT of size N4 (== opus_fft_impl, no 1/N scaling):
    // G[m] = Σ_i fin[i]·e^{-j2πim/N4}. Twiddles indexed by (i*m) mod N4, stepped
    // incrementally to avoid a per-element multiply+modulo.
    let mut wc = [0.0f32; MAX_N4];
    let mut ws = [0.0f32; MAX_N4];
    for (k, (c, s)) in wc[..n4].iter_mut().zip(ws[..n4].iter_mut()).enumerate() {
        let (cc, ss) = cos_sin_turn(k as f32 / n4 as f32);
        *c = cc;
        *s = ss;
    }
    let mut g_re = [0.0f32; MAX_N4];
    let mut g_im = [0.0f32; MAX_N4];
    for m in 0..n4 {
        let mut acc_re = 0.0f32;
        let mut acc_im = 0.0f32;
        let mut idx = 0usize; // (i*m) mod n4
        for i in 0..n4 {
            let c = wc[idx];
            let s = ws[idx];
            acc_re += fin_re[i] * c + fin_im[i] * s;
            acc_im += fin_im[i] * c - fin_re[i] * s;
            idx += m;
            if idx >= n4 {
                idx -= n4;
            }
        }
        g_re[m] = acc_re;
        g_im[m] = acc_im;
    }

    // Post-rotation + de-shuffle from both ends (libopus writes into
    // out[OVERLAP/2 ..]). t[j] = cos(2π(j+1/8)/N); t[N4+i] = -sin(θ_i).
    let base = OVERLAP / 2;
    let half = n4.div_ceil(2);
    for i in 0..half {
        // Front (uses G[i]); re=G[i].im, im=G[i].re (undo the input swap).
        let (c0, s0) = cos_sin_turn((i as f32 + 0.125) / n as f32); // t[i]=c0, t[N4+i]=-s0
        let re = g_im[i];
        let im = g_re[i];
        let yr_f = re * c0 - im * s0; //  re*t[i] + im*t[N4+i]
        let yi_f = -re * s0 - im * c0; //  re*t[N4+i] - im*t[i]
        out[base + 2 * i] = yr_f;
        out[base + (n2 - 1 - 2 * i)] = yi_f;

        // Back (uses G[N4-1-i]); t[j] here is a pure cosine (libopus stores only
        // cosines — the sines come from the N4/N2 index offsets, already folded
        // into the front terms above).
        let ci = n4 - i - 1;
        let t0b = t_cos(n4 - i - 1, n); // t[N4-i-1]
        let t1b = t_cos(n2 - i - 1, n); // t[N2-i-1]
        let re2 = g_im[ci];
        let im2 = g_re[ci];
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

    /// Cross-check `imdct_block` against an independent `f64` re-implementation
    /// of the *same* three-stage algorithm (pre-rotate → unnormalised DFT →
    /// post-rotate → TDAC mirror) built with `std` trig. This validates the
    /// no-`libm` `cos_sin_turn` precision and every index/sign in the transcription
    /// (a typo would diverge); the transform's *convention & absolute scale* vs
    /// libopus are pinned separately by the end-to-end golden in tests/decode.rs.
    #[test]
    fn imdct_block_matches_f64_reference_algorithm() {
        for &n2 in &[120usize, 240, 480] {
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

            // --- f64 reference of the identical algorithm ---
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

            // --- implementation under test ---
            let mut buf = vec![0.0f32; OVERLAP / 2 + n2];
            buf[..OVERLAP / 2].copy_from_slice(&prev_tail);
            imdct_block(&freq, n2, 1, &mut buf);

            let scale: f64 = refbuf.iter().map(|v| v.abs()).fold(0.0, f64::max).max(1e-6);
            for (i, (&got, &want)) in buf.iter().zip(refbuf.iter()).enumerate() {
                assert!(
                    (got as f64 - want).abs() < 1e-3 * scale,
                    "n2={n2} idx={i}: {got} vs {want}"
                );
            }
        }
    }
}
