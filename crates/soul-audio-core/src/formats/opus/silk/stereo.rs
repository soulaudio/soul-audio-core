//! SILK stereo mid/side prediction (RFC 6716 §4.2.7.1) and the mid/side →
//! left/right reconstruction (§4.2.8).
//!
//! Transcribed line-for-line from libopus **v1.5.2**: `silk/stereo_decode_pred.c`
//! (`silk_stereo_decode_pred`, `silk_stereo_decode_mid_only`) and
//! `silk/stereo_MS_to_LR.c` (`silk_stereo_MS_to_LR`). Pure fixed-point integer
//! math — `no_std` / no-alloc / `#![forbid(unsafe_code)]`.
//!
//! The cross-frame [`StereoState`] carries the 1-sample-history buffers
//! (`sMid`/`sSide`, 2 samples each) and the previous frame's prediction weights
//! (`pred_prev_Q13`) that the first `STEREO_INTERP_LEN_MS * fs_kHz` output
//! samples interpolate from — the three named bit-exactness bug sources.

use super::silk_tables::{
    STEREO_ONLY_CODE_MID_ICDF, STEREO_PRED_JOINT_ICDF, STEREO_PRED_QUANT_Q13, UNIFORM3_ICDF,
    UNIFORM5_ICDF,
};
use super::{add_lshift32, fix_const, lshift, rshift_round, sat16, smlabb, smlawb, smulbb, smulwb};
use crate::formats::opus::range::RangeDecoder;

/// `STEREO_QUANT_SUB_STEPS` (silk/define.h).
const STEREO_QUANT_SUB_STEPS: i32 = 5;
/// `STEREO_INTERP_LEN_MS` (silk/define.h) — the weight-interpolation ramp length
/// in ms (must be even).
const STEREO_INTERP_LEN_MS: usize = 8;
/// `SILK_FIX_CONST( 0.5 / STEREO_QUANT_SUB_STEPS, 16 )` = `SILK_FIX_CONST(0.1,16)`.
const HALF_SUB_STEP_Q16: i32 = fix_const(0.5 / STEREO_QUANT_SUB_STEPS as f64, 16);

/// Cross-frame stereo state (`stereo_dec_state`): the 1-sample delay history and
/// the previous frame's prediction weights (all init zero on reset).
#[derive(Clone, Default)]
pub(crate) struct StereoState {
    /// `sMid[2]` — the last two mid samples of the previous frame (the 1-sample
    /// output delay's history; also used for the mono buffering path).
    pub s_mid: [i16; 2],
    /// `sSide[2]` — the last two side samples of the previous frame.
    pub s_side: [i16; 2],
    /// `pred_prev_Q13[2]` — the previous frame's stereo prediction weights, the
    /// start of the first-samples interpolation ramp.
    pub pred_prev_q13: [i32; 2],
}

/// `silk_stereo_decode_pred` (silk/stereo_decode_pred.c): decode the two stereo
/// prediction weights (`pred_Q13[2]`) — the joint index via
/// `silk_stereo_pred_joint_iCDF`, then per-weight `uniform3`/`uniform5`
/// refinements dequantized against `silk_stereo_pred_quant_Q13`.
pub(crate) fn stereo_decode_pred(rd: &mut RangeDecoder) -> [i32; 2] {
    let mut ix = [[0i32; 3]; 2];
    let n = rd.decode_icdf(&STEREO_PRED_JOINT_ICDF, 8) as i32;
    ix[0][2] = n / 5; // silk_DIV32_16 (truncating)
    ix[1][2] = n - 5 * ix[0][2];
    for row in ix.iter_mut() {
        row[0] = rd.decode_icdf(&UNIFORM3_ICDF, 8) as i32;
        row[1] = rd.decode_icdf(&UNIFORM5_ICDF, 8) as i32;
    }

    let mut pred_q13 = [0i32; 2];
    for (n, row) in ix.iter_mut().enumerate() {
        row[0] += 3 * row[2];
        // Index range: row[0] ∈ 0..=14 (uniform3 0..2 + 3*(0..4)); the `+1`
        // reaches 15 — the last of the 16-entry table. Clamp defensively.
        let i0 = (row[0] as usize).min(STEREO_PRED_QUANT_Q13.len() - 2);
        let low_q13 = i32::from(STEREO_PRED_QUANT_Q13[i0]);
        let step_q13 = smulwb(
            i32::from(STEREO_PRED_QUANT_Q13[i0 + 1]) - low_q13,
            HALF_SUB_STEP_Q16,
        );
        pred_q13[n] = smlabb(low_q13, step_q13, 2 * row[1] + 1);
    }

    // Subtract second from first predictor (helps when actually applying these).
    pred_q13[0] -= pred_q13[1];
    pred_q13
}

/// `silk_stereo_decode_mid_only` (silk/stereo_decode_pred.c): decode the flag
/// that only the mid channel has been coded.
pub(crate) fn stereo_decode_mid_only(rd: &mut RangeDecoder) -> bool {
    rd.decode_icdf(&STEREO_ONLY_CODE_MID_ICDF, 8) != 0
}

/// `silk_stereo_MS_to_LR` (silk/stereo_MS_to_LR.c): convert the adaptive
/// mid/side representation to a left/right stereo signal, in place.
///
/// `x1` (mid, becomes left) and `x2` (side, becomes right) are each
/// `frame_length + 2` samples long, with the frame's decoded samples at indices
/// `[2..frame_length+2]` (matching libopus's `&samplesOut1_tmp[n][2]` write).
/// This routine copies the 2-sample history into `[0..2]`, saves the new history
/// for the next frame, interpolates the prediction weights over the first
/// `STEREO_INTERP_LEN_MS * fs_kHz` samples, adds the prediction to the side
/// channel, and finally forms L = mid+side / R = mid−side into `[1..frame_length+1]`
/// — the slice the per-channel resampler then reads from index `1`.
pub(crate) fn ms_to_lr(
    state: &mut StereoState,
    x1: &mut [i16],
    x2: &mut [i16],
    pred_q13: &[i32; 2],
    fs_khz: usize,
    frame_length: usize,
) {
    // Buffering: prepend the previous frame's 2-sample history, then save this
    // frame's last two samples for the next call.
    x1[0] = state.s_mid[0];
    x1[1] = state.s_mid[1];
    x2[0] = state.s_side[0];
    x2[1] = state.s_side[1];
    state.s_mid[0] = x1[frame_length];
    state.s_mid[1] = x1[frame_length + 1];
    state.s_side[0] = x2[frame_length];
    state.s_side[1] = x2[frame_length + 1];

    // Interpolate predictors and add prediction to the side channel.
    let mut pred0_q13 = state.pred_prev_q13[0];
    let mut pred1_q13 = state.pred_prev_q13[1];
    let ramp = STEREO_INTERP_LEN_MS * fs_khz;
    let denom_q16 = (1i32 << 16) / (ramp as i32); // silk_DIV32_16
    let delta0_q13 = rshift_round(smulbb(pred_q13[0] - state.pred_prev_q13[0], denom_q16), 16);
    let delta1_q13 = rshift_round(smulbb(pred_q13[1] - state.pred_prev_q13[1], denom_q16), 16);
    for n in 0..ramp {
        pred0_q13 += delta0_q13;
        pred1_q13 += delta1_q13;
        x2[n + 1] = ms_side_sample(x1, x2, n, pred0_q13, pred1_q13);
    }
    pred0_q13 = pred_q13[0];
    pred1_q13 = pred_q13[1];
    for n in ramp..frame_length {
        x2[n + 1] = ms_side_sample(x1, x2, n, pred0_q13, pred1_q13);
    }
    state.pred_prev_q13[0] = pred_q13[0];
    state.pred_prev_q13[1] = pred_q13[1];

    // Convert to left/right signals.
    for n in 0..frame_length {
        let mid = i32::from(x1[n + 1]);
        let side = i32::from(x2[n + 1]);
        x1[n + 1] = sat16(mid + side) as i16;
        x2[n + 1] = sat16(mid - side) as i16;
    }
}

/// One iteration of the side-channel prediction (the shared inner body of the
/// two `silk_stereo_MS_to_LR` loops): a low-pass predictor from the mid channel
/// plus the direct mid-channel weight, in the exact fixed-point of libopus.
#[inline]
fn ms_side_sample(x1: &[i16], x2: &[i16], n: usize, pred0_q13: i32, pred1_q13: i32) -> i16 {
    // sum = LSHIFT( ADD_LSHIFT32( x1[n] + x1[n+2], x1[n+1], 1 ), 9 )   /* Q11 */
    let sum = lshift(
        add_lshift32(
            i32::from(x1[n]) + i32::from(x1[n + 2]),
            i32::from(x1[n + 1]),
            1,
        ),
        9,
    );
    // sum = SMLAWB( LSHIFT( x2[n+1], 8 ), sum, pred0_Q13 )             /* Q8  */
    let sum = smlawb(lshift(i32::from(x2[n + 1]), 8), sum, pred0_q13);
    // sum = SMLAWB( sum, LSHIFT( x1[n+1], 11 ), pred1_Q13 )           /* Q8  */
    let sum = smlawb(sum, lshift(i32::from(x1[n + 1]), 11), pred1_q13);
    sat16(rshift_round(sum, 8)) as i16
}
