//! SILK long-term prediction (voiced frames): per-subframe pitch-lag
//! reconstruction and the 5-tap LTP filter + scaling dequantization.
//!
//! Transcribed from libopus **v1.5.2** `silk/decode_pitch.c` (pitch lags) and
//! `silk/decode_parameters.c` (LTP gain filter `Q7 << 7 → Q14` + LTP scaling).
//! Pure integer math — `no_std` / no-alloc / `#![forbid(unsafe_code)]` clean.
//!
//! The pitch/LTP *indices* themselves are read from the range decoder in
//! [`super::decoder`] (matching libopus's `silk_decode_indices` bitstream
//! order); this module turns those indices into the lags/taps Task 11's
//! synthesis filter consumes.

use super::silk_tables::{
    CB_LAGS_STAGE2, CB_LAGS_STAGE2_10MS, CB_LAGS_STAGE3, CB_LAGS_STAGE3_10MS, LTP_GAIN_VQ_0,
    LTP_GAIN_VQ_1, LTP_GAIN_VQ_2, LTP_SCALES_Q14,
};
use super::{LTP_ORDER, MAX_NB_SUBFR, lshift, smulbb};

/// `PE_MIN_LAG_MS` / `PE_MAX_LAG_MS` (silk/pitch_est_defines.h).
const PE_MIN_LAG_MS: i32 = 2;
const PE_MAX_LAG_MS: i32 = 18;

/// Decoded LTP parameters for one voiced SILK frame (what Task 11 needs to run
/// the long-term-prediction synthesis filter).
#[derive(Clone, Copy)]
pub(crate) struct LtpParams {
    /// `pitchL[k]` — integer pitch lag (samples at the internal rate) per
    /// subframe.
    pub pitch_l: [i32; MAX_NB_SUBFR],
    /// `LTPCoef_Q14[k][tap]` — the 5-tap Q14 long-term filter per subframe.
    pub coef_q14: [[i16; LTP_ORDER]; MAX_NB_SUBFR],
    /// `LTP_scale_Q14` — inter-frame LTP state downscaling factor (Q14).
    pub scale_q14: i32,
}

impl Default for LtpParams {
    fn default() -> Self {
        Self {
            pitch_l: [0; MAX_NB_SUBFR],
            coef_q14: [[0; LTP_ORDER]; MAX_NB_SUBFR],
            scale_q14: 0,
        }
    }
}

/// `silk_decode_pitch` (silk/decode_pitch.c): reconstruct the `nb_subfr`
/// per-subframe pitch lags from the absolute/relative lag index and the
/// contour codebook index. NB uses the stage-2 lag codebooks, MB/WB the
/// stage-3 ones; each has a 10 ms (2-subframe) variant.
fn decode_pitch(
    lag_index: i32,
    contour_index: i32,
    fs_khz: i32,
    nb_subfr: usize,
    pitch_l: &mut [i32; MAX_NB_SUBFR],
) {
    let min_lag = smulbb(PE_MIN_LAG_MS, fs_khz);
    let max_lag = smulbb(PE_MAX_LAG_MS, fs_khz);
    let lag = min_lag + lag_index;
    let ci = contour_index.max(0) as usize;

    // `matrix_ptr(Lag_CB_ptr, k, contourIndex, cbk_size)` selects the codebook
    // entry for subframe `k`. Indexing is guarded so malformed indices can't
    // panic (out-of-range contributes 0 offset).
    for (k, out) in pitch_l.iter_mut().enumerate().take(nb_subfr) {
        let off = if fs_khz == 8 {
            if nb_subfr == MAX_NB_SUBFR {
                CB_LAGS_STAGE2.get(k).and_then(|r| r.get(ci)).copied()
            } else {
                CB_LAGS_STAGE2_10MS.get(k).and_then(|r| r.get(ci)).copied()
            }
        } else if nb_subfr == MAX_NB_SUBFR {
            CB_LAGS_STAGE3.get(k).and_then(|r| r.get(ci)).copied()
        } else {
            CB_LAGS_STAGE3_10MS.get(k).and_then(|r| r.get(ci)).copied()
        };
        let v = lag + i32::from(off.unwrap_or(0));
        // silk_LIMIT( pitch_lags[k], min_lag, max_lag ) — min_lag < max_lag here.
        *out = v.clamp(min_lag, max_lag);
    }
}

/// Reconstruct the LTP gain filter for subframe `k` (silk/decode_parameters.c):
/// `LTPCoef_Q14 = silk_LTP_vq_ptrs_Q7[ PERIndex ][ LTPIndex ] << 7`.
fn ltp_gain_row(per_index: i32, ltp_index: i32) -> [i16; LTP_ORDER] {
    let ix = ltp_index.max(0) as usize;
    let row: Option<&[i8; LTP_ORDER]> = match per_index {
        0 => LTP_GAIN_VQ_0.get(ix),
        1 => LTP_GAIN_VQ_1.get(ix),
        _ => LTP_GAIN_VQ_2.get(ix),
    };
    let mut out = [0i16; LTP_ORDER];
    if let Some(r) = row {
        for (o, &c) in out.iter_mut().zip(r.iter()) {
            *o = lshift(i32::from(c), 7) as i16;
        }
    }
    out
}

/// Reconstruct all voiced LTP parameters for a frame: per-subframe pitch lags,
/// the 5-tap Q14 filters, and the LTP scaling factor.
#[allow(clippy::too_many_arguments)]
pub(super) fn decode_ltp(
    lag_index: i32,
    contour_index: i32,
    per_index: i32,
    ltp_index: &[i8; MAX_NB_SUBFR],
    ltp_scale_index: i32,
    fs_khz: i32,
    nb_subfr: usize,
) -> LtpParams {
    let mut p = LtpParams::default();
    decode_pitch(lag_index, contour_index, fs_khz, nb_subfr, &mut p.pitch_l);
    for (out, &ix) in p.coef_q14.iter_mut().zip(ltp_index.iter()).take(nb_subfr) {
        *out = ltp_gain_row(per_index, i32::from(ix));
    }
    // silk_LTPScales_table_Q14[ LTP_scaleIndex ].
    p.scale_q14 = LTP_SCALES_Q14
        .get(ltp_scale_index.max(0) as usize)
        .copied()
        .map(i32::from)
        .unwrap_or_else(|| i32::from(LTP_SCALES_Q14[0]));
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pitch_lags_match_libopus_voiced_frame() {
        // Golden from the verbatim libopus v1.5.2 `silk_decode_pitch` oracle on
        // the first voiced frame of testdata/voice-silk-mono.opus (WB, 20 ms):
        // lagIndex=68, contourIndex=0 => lag=32+68=100, contour offsets 0.
        let mut lags = [0i32; MAX_NB_SUBFR];
        decode_pitch(68, 0, 16, 4, &mut lags);
        assert_eq!(lags, [100, 100, 100, 100]);
    }

    #[test]
    fn pitch_lags_are_clamped_to_the_valid_range() {
        // A huge lag index saturates at max_lag = 18 * fs_kHz; a tiny one floors
        // at min_lag = 2 * fs_kHz. Guarantees no out-of-range lag reaches Task 11.
        let mut lags = [0i32; MAX_NB_SUBFR];
        decode_pitch(100_000, 0, 16, 4, &mut lags);
        assert!(lags.iter().all(|&l| l == 18 * 16));
        decode_pitch(-100_000, 0, 16, 4, &mut lags);
        assert!(lags.iter().all(|&l| l == 2 * 16));
    }

    #[test]
    fn ltp_taps_match_libopus_q7_shifted_to_q14() {
        // PERIndex=0, LTPIndex[0]=2 -> silk_LTP_gain_vq_0[2] = {12,28,41,13,-4},
        // each << 7. Golden from the libopus oracle on the voiced frame.
        assert_eq!(ltp_gain_row(0, 2), [1536, 3584, 5248, 1664, -512]);
        // LTPIndex=0 -> vq_0[0] = {4,6,24,7,5} << 7.
        assert_eq!(ltp_gain_row(0, 0), [512, 768, 3072, 896, 640]);
    }

    #[test]
    fn out_of_range_indices_do_not_panic() {
        // Malformed indices must degrade to zeros, never index out of bounds.
        assert_eq!(ltp_gain_row(2, 999), [0; LTP_ORDER]);
        let p = decode_ltp(0, 999, 9, &[9, 9, 9, 9], 9, 16, 4);
        assert_eq!(p.scale_q14, i32::from(LTP_SCALES_Q14[0]));
    }
}
