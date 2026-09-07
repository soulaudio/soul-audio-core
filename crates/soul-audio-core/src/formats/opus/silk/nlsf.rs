//! SILK NLSF vector decode + NLSF→LPC conversion + LSF interpolation.
//!
//! Transcribed from libopus **v1.5.2** fixed-point: `silk/NLSF_decode.c`,
//! `silk/NLSF_unpack.c`, `silk/NLSF_stabilize.c`, `silk/NLSF2A.c`,
//! `silk/LPC_fit.c`, `silk/bwexpander_32.c`, and `silk/LPC_inv_pred_gain.c`.
//! Every step is integer/fixed-point — no transcendentals — so the `no_std`,
//! `#![forbid(unsafe_code)]`, `libm`-free constraints hold.

use super::silk_tables::{LSF_COS_TAB_Q12, NlsfCb};
use super::{
    MAX_LPC_ORDER, MAX_LPC_STABILIZE_ITERATIONS, MIN_INV_GAIN_Q30, NLSF_QUANT_LEVEL_ADJ_Q10,
    NLSF_QUANT_MAX_AMPLITUDE, abs, add_sat16, clz32, div32, div32_16, fix_const, inverse32_var_q,
    limit, lshift, mul, rshift, rshift_round, rshift_round64, sat16, smlawb, smmul, smulbb, smull,
    smulww,
};

/// `QA` in `silk/NLSF2A.c` — the fixed-point domain of the intermediate cosine
/// values and polynomials.
const QA: u32 = 16;

/// libopus `silk/NLSF2A.c` `ordering10` — index permutation maximizing numerical
/// accuracy of the polynomial expansion for order-10 (NB/MB).
const ORDERING10: [usize; 10] = [0, 9, 6, 3, 4, 5, 8, 1, 2, 7];
/// libopus `silk/NLSF2A.c` `ordering16` — the order-16 (WB) permutation.
const ORDERING16: [usize; 16] = [0, 15, 8, 7, 4, 11, 12, 3, 2, 13, 10, 5, 6, 9, 14, 1];

/// `silk_NLSF_residual_dequant` (silk/NLSF_decode.c): predictive dequantizer for
/// the stage-2 NLSF residual. **Backward** predicted — processed from the
/// highest index down, so `res[i]` folds in the already-computed `res[i+1]`
/// through `pred_coef_Q8[i]`. Getting the direction (or the level-adjust sign)
/// wrong silently corrupts every reconstructed NLSF, so this mirrors the C
/// exactly.
fn residual_dequant(
    x_q10: &mut [i16],
    indices: &[i8],
    pred_coef_q8: &[u8],
    quant_step_size_q16: i32,
    order: usize,
) {
    let mut out_q10: i32 = 0;
    for i in (0..order).rev() {
        let pred_q10 = rshift(smulbb(out_q10, i32::from(pred_coef_q8[i])), 8);
        out_q10 = lshift(i32::from(indices[i]), 10);
        if out_q10 > 0 {
            out_q10 = out_q10.wrapping_sub(NLSF_QUANT_LEVEL_ADJ_Q10);
        } else if out_q10 < 0 {
            out_q10 = out_q10.wrapping_add(NLSF_QUANT_LEVEL_ADJ_Q10);
        }
        out_q10 = smlawb(pred_q10, out_q10, quant_step_size_q16);
        x_q10[i] = out_q10 as i16;
    }
}

/// `silk_NLSF_unpack` (silk/NLSF_unpack.c): unpack the per-coefficient entropy
/// table indices (`ec_ix`) and backward predictor coefficients (`pred_Q8`) for a
/// given stage-1 codebook index, from the packed `ec_sel` nibbles. Called both
/// by [`nlsf_decode`] (for the predictor) and by the side-info index decoder
/// (for `ec_ix`, which selects each residual coefficient's iCDF).
pub(super) fn unpack(ec_ix: &mut [i16], pred_q8: &mut [u8], cb: &NlsfCb, cb1_index: usize) {
    let order = cb.order as usize;
    let base = cb1_index * order / 2;
    let step = 2 * NLSF_QUANT_MAX_AMPLITUDE + 1;
    let mut i = 0;
    while i < order {
        let entry = i32::from(cb.ec_sel[base + i / 2]);
        ec_ix[i] = smulbb(rshift(entry, 1) & 7, step) as i16;
        pred_q8[i] = cb.pred_q8[i + (entry & 1) as usize * (order - 1)];
        ec_ix[i + 1] = smulbb(rshift(entry, 5) & 7, step) as i16;
        // The last odd coefficient's predictor index is `2*(order-1)` — one past
        // the `pred_q8` codebook — when its select bit is set. libopus reads the
        // adjacent global there; the value is only ever `pred_coef_Q8[order-1]`,
        // which `residual_dequant` multiplies by the (still-zero) `out_Q10`, so it
        // never affects the output. Guard with `unwrap_or(0)` to stay panic-free.
        let pi = i + (rshift(entry, 4) & 1) as usize * (order - 1) + 1;
        pred_q8[i + 1] = cb.pred_q8.get(pi).copied().unwrap_or(0);
        i += 2;
    }
}

/// `silk_NLSF_decode` (silk/NLSF_decode.c): reconstruct the quantized NLSF
/// vector (Q15) from the codebook path `nlsf_indices` (`[order + 1]`: stage-1
/// index then `order` stage-2 residual indices), then stabilize it. The stage-1
/// codebook vector `CB1_Q8` is added to the inverse-square-root-weighted stage-2
/// residual.
pub(super) fn nlsf_decode(nlsf_q15: &mut [i16], nlsf_indices: &[i8], cb: &NlsfCb) {
    let order = cb.order as usize;
    let mut pred_q8 = [0u8; MAX_LPC_ORDER];
    let mut ec_ix = [0i16; MAX_LPC_ORDER];
    let mut res_q10 = [0i16; MAX_LPC_ORDER];

    let cb1_index = nlsf_indices[0] as usize;
    unpack(&mut ec_ix, &mut pred_q8, cb, cb1_index);
    residual_dequant(
        &mut res_q10,
        &nlsf_indices[1..=order],
        &pred_q8,
        cb.quant_step_size_q16,
        order,
    );

    let cb_element = &cb.cb1_nlsf_q8[cb1_index * order..cb1_index * order + order];
    let cb_wght = &cb.cb1_wght_q9[cb1_index * order..cb1_index * order + order];
    for i in 0..order {
        // silk_ADD_LSHIFT32( silk_DIV32_16( res_Q10[i] << 14, Wght_Q9[i] ), CB1_Q8[i], 7 )
        let weighted = div32_16(lshift(i32::from(res_q10[i]), 14), i32::from(cb_wght[i]));
        let nlsf_tmp = weighted.wrapping_add(lshift(i32::from(cb_element[i]), 7));
        nlsf_q15[i] = limit(nlsf_tmp, 0, 32767) as i16;
    }

    stabilize(&mut nlsf_q15[..order], cb.delta_min_q15, order);
}

/// `silk_NLSF_stabilize` (silk/NLSF_stabilize.c): enforce the minimum spacing
/// `NDeltaMin_Q15` between consecutive NLSFs and against the `0` / `32768`
/// bounds. An iterative forward pass (up to `MAX_LOOPS`) that repeatedly widens
/// the single smallest gap, with a sort-and-clamp fallback if it doesn't
/// converge.
fn stabilize(nlsf_q15: &mut [i16], ndelta_min_q15: &[i16], l: usize) {
    const MAX_LOOPS: i32 = 20;
    debug_assert!(ndelta_min_q15[l] >= 1);

    for _ in 0..MAX_LOOPS {
        // Find the smallest distance.
        let mut min_diff = i32::from(nlsf_q15[0]) - i32::from(ndelta_min_q15[0]);
        let mut idx_i = 0usize;
        for i in 1..=l - 1 {
            let diff = i32::from(nlsf_q15[i])
                - (i32::from(nlsf_q15[i - 1]) + i32::from(ndelta_min_q15[i]));
            if diff < min_diff {
                min_diff = diff;
                idx_i = i;
            }
        }
        let diff = (1 << 15) - (i32::from(nlsf_q15[l - 1]) + i32::from(ndelta_min_q15[l]));
        if diff < min_diff {
            min_diff = diff;
            idx_i = l;
        }

        if min_diff >= 0 {
            return;
        }

        if idx_i == 0 {
            nlsf_q15[0] = ndelta_min_q15[0];
        } else if idx_i == l {
            // Compute in i32: `1i16 << 15` is `i16::MIN` (-32768), so the
            // subtraction below would overflow i16 in debug builds even
            // though release silently wraps to the intended `32768 -
            // ndelta_min_q15[l]` value (always <= 32767, so the final `as
            // i16` cast back is exact, not truncating).
            nlsf_q15[l - 1] = ((1i32 << 15) - i32::from(ndelta_min_q15[l])) as i16;
        } else {
            // Lower extreme for the location of the current center frequency.
            let mut min_center = 0i32;
            for &d in ndelta_min_q15.iter().take(idx_i) {
                min_center += i32::from(d);
            }
            min_center += rshift(i32::from(ndelta_min_q15[idx_i]), 1);
            // Upper extreme.
            let mut max_center = 1i32 << 15;
            for k in (idx_i + 1..=l).rev() {
                max_center -= i32::from(ndelta_min_q15[k]);
            }
            max_center -= rshift(i32::from(ndelta_min_q15[idx_i]), 1);

            let center = limit(
                rshift_round(
                    i32::from(nlsf_q15[idx_i - 1]) + i32::from(nlsf_q15[idx_i]),
                    1,
                ),
                min_center,
                max_center,
            );
            nlsf_q15[idx_i - 1] = (center - rshift(i32::from(ndelta_min_q15[idx_i]), 1)) as i16;
            nlsf_q15[idx_i] = nlsf_q15[idx_i - 1] + ndelta_min_q15[idx_i];
        }
    }

    // Fallback: insertion sort + clamp to keep spacing (less ideal).
    insertion_sort_increasing(&mut nlsf_q15[..l]);
    nlsf_q15[0] = nlsf_q15[0].max(ndelta_min_q15[0]);
    for i in 1..l {
        nlsf_q15[i] = nlsf_q15[i]
            .max(add_sat16(i32::from(nlsf_q15[i - 1]), i32::from(ndelta_min_q15[i])) as i16);
    }
    // Same i32-domain subtraction as the `idx_i == l` branch above, to avoid
    // an i16 overflow panic in debug builds (release wraps to the same
    // `32768 - ndelta_min_q15[l]` value that this computes exactly).
    nlsf_q15[l - 1] = nlsf_q15[l - 1].min(((1i32 << 15) - i32::from(ndelta_min_q15[l])) as i16);
    for i in (0..=l - 2).rev() {
        nlsf_q15[i] = nlsf_q15[i].min(nlsf_q15[i + 1] - ndelta_min_q15[i + 1]);
    }
}

/// `silk_insertion_sort_increasing_all_values_int16` (silk/sort.c).
fn insertion_sort_increasing(a: &mut [i16]) {
    for i in 1..a.len() {
        let value = a[i];
        let mut j = i as isize - 1;
        while j >= 0 && value < a[j as usize] {
            a[(j + 1) as usize] = a[j as usize];
            j -= 1;
        }
        a[(j + 1) as usize] = value;
    }
}

/// `silk_NLSF2A_find_poly` (silk/NLSF2A.c): build one of the two order-`dd`
/// polynomials by convolution from the interleaved `2*cos(LSF)` values (`QA`).
fn nlsf2a_find_poly(out: &mut [i32], clsf: &[i32], dd: usize) {
    out[0] = lshift(1, QA);
    out[1] = -clsf[0];
    for k in 1..dd {
        let ftmp = clsf[2 * k];
        out[k + 1] =
            lshift(out[k - 1], 1).wrapping_sub(rshift_round64(smull(ftmp, out[k]), QA) as i32);
        for n in (2..=k).rev() {
            out[n] = out[n]
                .wrapping_add(out[n - 2])
                .wrapping_sub(rshift_round64(smull(ftmp, out[n - 1]), QA) as i32);
        }
        out[1] -= ftmp;
    }
}

/// `silk_bwexpander_32` (silk/bwexpander_32.c): chirp (bandwidth-expand) an
/// int32 AR filter (without the leading 1).
fn bwexpander_32(ar: &mut [i32], d: usize, chirp_q16: i32) {
    let mut chirp = chirp_q16;
    let chirp_minus_one = chirp - 65536;
    for a in ar.iter_mut().take(d - 1) {
        *a = smulww(chirp, *a);
        chirp += rshift_round(mul(chirp, chirp_minus_one), 16);
    }
    ar[d - 1] = smulww(chirp, ar[d - 1]);
}

/// `silk_LPC_fit` (silk/LPC_fit.c): convert int32 coefficients (`QIN`) to int16
/// (`QOUT`), applying bandwidth expansion until they fit without wrap-around.
fn lpc_fit(a_qout: &mut [i16], a_qin: &mut [i32], qout: u32, qin: u32, d: usize) {
    let shift = qin - qout;
    let mut idx = 0usize;
    let mut i = 0i32;
    while i < 10 {
        let mut maxabs = 0i32;
        for (k, &v) in a_qin.iter().enumerate().take(d) {
            let absval = abs(v);
            if absval > maxabs {
                maxabs = absval;
                idx = k;
            }
        }
        maxabs = rshift_round(maxabs, shift);
        if maxabs > i32::from(i16::MAX) {
            // Reduce magnitude of prediction coefficients.
            maxabs = maxabs.min(163838); // (int32_MAX >> 14) + int16_MAX
            let chirp_q16 = fix_const(0.999, 16)
                - div32(
                    lshift(maxabs - i32::from(i16::MAX), 14),
                    rshift(mul(maxabs, idx as i32 + 1), 2),
                );
            bwexpander_32(a_qin, d, chirp_q16);
        } else {
            break;
        }
        i += 1;
    }

    if i == 10 {
        for (out, qin) in a_qout.iter_mut().zip(a_qin.iter_mut()).take(d) {
            *out = sat16(rshift_round(*qin, shift)) as i16;
            *qin = lshift(i32::from(*out), shift);
        }
    } else {
        for (out, &qin) in a_qout.iter_mut().zip(a_qin.iter()).take(d) {
            *out = rshift_round(qin, shift) as i16;
        }
    }
}

/// `MUL32_FRAC_Q(a, b, Q)` (silk/LPC_inv_pred_gain.c).
#[inline]
fn mul32_frac_q(a: i32, b: i32, q: u32) -> i32 {
    rshift_round64(smull(a, b), q) as i32
}

/// `LPC_inverse_pred_gain_QA_c` (silk/LPC_inv_pred_gain.c): returns the inverse
/// prediction gain (Q30), or `0` if the coefficients are unstable. Consumes a
/// mutable copy of the coefficients in the `QA=24` domain.
fn inverse_pred_gain_qa(a_qa: &mut [i32], order: usize) -> i32 {
    const QA_G: u32 = 24;
    let a_limit = fix_const(0.99975, QA_G);
    let mut inv_gain_q30 = 1i32 << 30;

    let mut k = order - 1;
    while k > 0 {
        if a_qa[k] > a_limit || a_qa[k] < -a_limit {
            return 0;
        }
        let rc_q31 = -lshift(a_qa[k], 31 - QA_G);
        let rc_mult1_q30 = (1i32 << 30) - smmul(rc_q31, rc_q31);
        inv_gain_q30 = lshift(smmul(inv_gain_q30, rc_mult1_q30), 2);
        if inv_gain_q30 < MIN_INV_GAIN_Q30 {
            return 0;
        }
        let mult2_q = 32 - clz32(abs(rc_mult1_q30));
        let rc_mult2 = inverse32_var_q(rc_mult1_q30, mult2_q + 30);

        for n in 0..(k + 1) >> 1 {
            let tmp1 = a_qa[n];
            let tmp2 = a_qa[k - n - 1];
            let t1 = smull(
                tmp1.saturating_sub(mul32_frac_q(tmp2, rc_q31, 31)),
                rc_mult2,
            );
            let tmp64 = rshift_round64(t1, mult2_q as u32);
            if tmp64 > i64::from(i32::MAX) || tmp64 < i64::from(i32::MIN) {
                return 0;
            }
            a_qa[n] = tmp64 as i32;
            let t2 = smull(
                tmp2.saturating_sub(mul32_frac_q(tmp1, rc_q31, 31)),
                rc_mult2,
            );
            let tmp64 = rshift_round64(t2, mult2_q as u32);
            if tmp64 > i64::from(i32::MAX) || tmp64 < i64::from(i32::MIN) {
                return 0;
            }
            a_qa[k - n - 1] = tmp64 as i32;
        }
        k -= 1;
    }

    if a_qa[0] > a_limit || a_qa[0] < -a_limit {
        return 0;
    }
    let rc_q31 = -lshift(a_qa[0], 31 - QA_G);
    let rc_mult1_q30 = (1i32 << 30) - smmul(rc_q31, rc_q31);
    inv_gain_q30 = lshift(smmul(inv_gain_q30, rc_mult1_q30), 2);
    if inv_gain_q30 < MIN_INV_GAIN_Q30 {
        return 0;
    }
    inv_gain_q30
}

/// Whether a Q12 LPC filter passes libopus's stability test (all poles inside
/// the unit circle). Used by the front-end tests to assert `silk_NLSF2A` yields
/// stable coefficients.
#[cfg(test)]
pub(super) fn is_lpc_stable(a_q12: &[i16], order: usize) -> bool {
    lpc_inverse_pred_gain(a_q12, order) != 0
}

/// `silk_LPC_inverse_pred_gain_c` (silk/LPC_inv_pred_gain.c) for Q12 input.
/// `0` means "unstable".
fn lpc_inverse_pred_gain(a_q12: &[i16], order: usize) -> i32 {
    const QA_G: u32 = 24;
    let mut atmp_qa = [0i32; MAX_LPC_ORDER];
    let mut dc_resp = 0i32;
    for k in 0..order {
        dc_resp += i32::from(a_q12[k]);
        atmp_qa[k] = lshift(i32::from(a_q12[k]), QA_G - 12);
    }
    if dc_resp >= 4096 {
        return 0;
    }
    inverse_pred_gain_qa(&mut atmp_qa[..order], order)
}

/// `silk_NLSF2A` (silk/NLSF2A.c): convert normalized line spectral frequencies
/// (Q15) into monic whitening-filter (LPC) coefficients (Q12), including the
/// stability-limiting bandwidth-expansion iterations.
pub(super) fn nlsf2a(a_q12: &mut [i16], nlsf: &[i16], d: usize) {
    let ordering: &[usize] = if d == 16 { &ORDERING16 } else { &ORDERING10 };

    // Convert LSFs to 2*cos(LSF) via the piecewise-linear cos table.
    let mut cos_lsf_qa = [0i32; MAX_LPC_ORDER];
    for k in 0..d {
        let f_int = rshift(i32::from(nlsf[k]), 15 - 7); // 0..127
        let f_frac = i32::from(nlsf[k]) - lshift(f_int, 15 - 7); // 0..255
        let fi = (f_int as usize).min(LSF_COS_TAB_Q12.len() - 2);
        let cos_val = i32::from(LSF_COS_TAB_Q12[fi]);
        let delta = i32::from(LSF_COS_TAB_Q12[fi + 1]) - cos_val;
        // silk_RSHIFT_ROUND( (cos_val << 8) + delta*f_frac, 20 - QA )
        cos_lsf_qa[ordering[k]] = rshift_round(lshift(cos_val, 8) + mul(delta, f_frac), 20 - QA);
    }

    let dd = d >> 1;
    let mut p = [0i32; MAX_LPC_ORDER / 2 + 1];
    let mut q = [0i32; MAX_LPC_ORDER / 2 + 1];
    nlsf2a_find_poly(&mut p, &cos_lsf_qa, dd);
    nlsf2a_find_poly(&mut q, &cos_lsf_qa[1..], dd);

    // Combine even/odd polynomials into QA+1 filter coefficients.
    let mut a32_qa1 = [0i32; MAX_LPC_ORDER];
    for k in 0..dd {
        let ptmp = p[k + 1].wrapping_add(p[k]);
        let qtmp = q[k + 1].wrapping_sub(q[k]);
        a32_qa1[k] = -qtmp - ptmp;
        a32_qa1[d - k - 1] = qtmp - ptmp;
    }

    // Convert to Q12 int16, then stabilize.
    lpc_fit(a_q12, &mut a32_qa1, 12, QA + 1, d);

    let mut i = 0i32;
    while lpc_inverse_pred_gain(a_q12, d) == 0 && i < MAX_LPC_STABILIZE_ITERATIONS {
        bwexpander_32(&mut a32_qa1, d, 65536 - lshift(2, i as u32));
        for k in 0..d {
            a_q12[k] = rshift_round(a32_qa1[k], QA + 1 - 12) as i16;
        }
        i += 1;
    }
}

/// LSF interpolation for the first two subframes (RFC 6716 §4.2.7.5.7 /
/// silk/decode_parameters.c): `nlsf0 = prev + (interp/4)*(cur - prev)` in Q15.
/// Caller guarantees `interp_q2 < 4` (a value of `4`, or a post-reset frame,
/// skips interpolation and reuses `cur`).
pub(super) fn lsf_interpolate(
    nlsf0_q15: &mut [i16],
    prev_nlsf_q15: &[i16],
    cur_nlsf_q15: &[i16],
    interp_q2: i32,
    order: usize,
) {
    for i in 0..order {
        let diff = i32::from(cur_nlsf_q15[i]) - i32::from(prev_nlsf_q15[i]);
        nlsf0_q15[i] = (i32::from(prev_nlsf_q15[i]) + rshift(mul(interp_q2, diff), 2)) as i16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Golden vectors from a self-contained oracle built out of the *verbatim*
    // libopus v1.5.2 `silk_NLSF2A` + `silk_LPC_fit` + `silk_bwexpander_32` +
    // `silk_LPC_inverse_pred_gain` functions (see the Task 9 report). Matching
    // them proves the fixed-point NLSF→LPC conversion is bit-exact.

    #[test]
    fn nlsf2a_order10_matches_libopus() {
        let nlsf: [i16; 10] = [
            2000, 5000, 8000, 11000, 14000, 17000, 20000, 23000, 26000, 29000,
        ];
        let mut a = [0i16; 10];
        nlsf2a(&mut a, &nlsf, 10);
        assert_eq!(a, [2398, -520, 788, -312, 424, -179, 231, -80, 98, -9]);
        assert!(is_lpc_stable(&a, 10));
    }

    #[test]
    fn nlsf2a_order16_matches_libopus() {
        let nlsf: [i16; 16] = [
            1500, 3000, 5000, 7000, 9000, 11000, 13000, 15000, 17000, 19000, 21000, 23000, 25000,
            27000, 29000, 31000,
        ];
        let mut a = [0i16; 16];
        nlsf2a(&mut a, &nlsf, 16);
        assert_eq!(
            a,
            [
                1551, 921, 36, 269, -42, 115, -59, 50, -64, 14, -60, -5, -48, -12, -30, -9
            ]
        );
        assert!(is_lpc_stable(&a, 16));
    }

    #[test]
    fn stabilize_enforces_minimum_spacing() {
        // NB/MB minimum-spacing vector (silk_NLSF_DELTA_MIN_NB_MB_Q15).
        let dmin: [i16; 11] = [250, 3, 6, 3, 3, 3, 4, 3, 3, 3, 461];
        // Deliberately-too-close / below-floor input.
        let mut x: [i16; 10] = [
            100, 120, 5000, 5001, 15000, 15002, 20000, 25000, 30000, 32000,
        ];
        stabilize(&mut x, &dmin, 10);
        // Golden from the verbatim libopus `silk_NLSF_stabilize` oracle.
        assert_eq!(
            x,
            [
                250, 253, 5000, 5003, 15000, 15003, 20000, 25000, 30000, 32000
            ]
        );
        // Invariants: floor, ceiling, and pairwise spacing all honored.
        assert!(x[0] >= dmin[0]);
        assert!(i32::from(x[9]) <= (1 << 15) - i32::from(dmin[10]));
        for i in 1..10 {
            assert!(i32::from(x[i]) - i32::from(x[i - 1]) >= i32::from(dmin[i]));
        }
    }

    #[test]
    fn stabilize_top_pin_branch_does_not_panic() {
        // Regression test for a debug-build overflow panic: the `idx_i == l`
        // ("I==L" / top-pinned) branch computed `(1 << 15) - ndelta_min_q15[l]`
        // in `i16`, and `1i16 << 15` is `i16::MIN` (-32768), so subtracting
        // any positive `ndelta_min_q15[l]` overflowed i16 ("attempt to
        // subtract with overflow") under `cargo test`'s overflow checks. This
        // input drives the top coefficient at/above the ceiling so that
        // branch executes on the very first loop iteration.
        let dmin: [i16; 3] = [100, 50, 461];
        let mut x: [i16; 2] = [16000, 32750];
        stabilize(&mut x, &dmin, 2);

        // Hand-traced against the (fixed) algorithm:
        //   iter 1: top gap `32768 - (32750 + 461) = -443` is the tightest
        //           (more negative than the floor/interior gaps) => I==L
        //           branch fires: x[1] = 32768 - 461 = 32307.
        //   iter 2: top gap `32768 - (32307 + 461) = 0` >= 0 => converged.
        // This is exactly the value release builds silently wrap to today
        // (`-32768i16 - 461` wraps to `32307`), so the fix is a no-op on the
        // non-overflowing (release) path.
        assert_eq!(x, [16000, 32307]);

        // Invariants: floor, ceiling, and pairwise spacing all honored.
        assert!(x[0] >= dmin[0]);
        assert!(i32::from(x[1]) <= (1 << 15) - i32::from(dmin[2]));
        assert!(i32::from(x[1]) - i32::from(x[0]) >= i32::from(dmin[1]));
    }

    #[test]
    fn lsf_interpolation_quarters_the_step() {
        // interp_q2 = 2 (i.e. 2/4 = halfway) → nlsf0 = midpoint of prev/cur.
        let prev: [i16; 4] = [1000, 2000, 3000, 4000];
        let cur: [i16; 4] = [2000, 4000, 6000, 8000];
        let mut out = [0i16; 4];
        lsf_interpolate(&mut out, &prev, &cur, 2, 4);
        assert_eq!(out, [1500, 3000, 4500, 6000]);
    }
}
