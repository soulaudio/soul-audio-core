//! SILK front-end orchestration: cross-frame [`SilkDecoder`] state, the
//! side-information index decode (`silk_decode_indices` subset), gain dequant
//! (`silk_gains_dequant` + `silk_log2lin`), and the parameter decode
//! (`silk_decode_parameters` subset) that produces per-subframe LPC coefficients.
//!
//! Transcribed from libopus **v1.5.2**: `silk/dec_API.c` (`silk_Decode` — the
//! VAD/LBRR header flags and the mono **and** stereo per-frame orchestration),
//! `silk/decode_indices.c`, `silk/gain_quant.c`, `silk/log2lin.c`,
//! `silk/decode_parameters.c`, and `silk/decoder_set_fs.c` (reset state). The
//! stereo mid/side prediction + unmix live in [`super::stereo`].
//!
//! # Stereo structure ([`SilkDecoder`])
//! The decoder now holds **two** independent per-channel [`SilkChannel`] states
//! (mid + side) plus a cross-frame [`super::stereo::StereoState`]
//! (`sMid`/`sSide`/`pred_prev_Q13`). Per internal SILK frame of a **stereo**
//! packet it decodes the stereo prediction weights + mid-only flag, decodes the
//! mid channel (always) and side channel (unless mid-only), reconstructs L/R
//! with [`super::stereo::ms_to_lr`], **then** resamples each of L/R to 48 kHz —
//! the exact order of `silk_Decode`. Mono packets take the identical single-
//! channel path as before (byte-for-byte), optionally duplicated to two output
//! channels for a stereo-header stream carrying a mono packet.

use super::excitation::{decode_pulses, reconstruct_excitation};
use super::ltp::{LtpParams, decode_ltp};
use super::nlsf::{lsf_interpolate, nlsf_decode, nlsf2a, unpack};
use super::resampler::Resampler;
use super::silk_tables::{
    DELTA_GAIN_ICDF, GAIN_ICDF, LBRR_FLAGS_2_ICDF, LBRR_FLAGS_3_ICDF, LTP_GAIN_ICDF_0,
    LTP_GAIN_ICDF_1, LTP_GAIN_ICDF_2, LTP_PER_INDEX_ICDF, LTPSCALE_ICDF, NLSF_CB_NB_MB, NLSF_CB_WB,
    NLSF_EXT_ICDF, NLSF_INTERP_FACTOR_ICDF, NlsfCb, PITCH_CONTOUR_10MS_ICDF,
    PITCH_CONTOUR_10MS_NB_ICDF, PITCH_CONTOUR_ICDF, PITCH_CONTOUR_NB_ICDF, PITCH_DELTA_ICDF,
    PITCH_LAG_ICDF, TYPE_OFFSET_NO_VAD_ICDF, TYPE_OFFSET_VAD_ICDF, UNIFORM4_ICDF, UNIFORM6_ICDF,
    UNIFORM8_ICDF,
};
use super::stereo::{StereoState, ms_to_lr, stereo_decode_mid_only, stereo_decode_pred};
use super::{
    GAIN_INV_SCALE_Q16, GAIN_OFFSET_Q7, LTP_MEM_LENGTH_MS, LTP_ORDER, MAX_DELTA_GAIN_QUANT,
    MAX_FRAME_LENGTH, MAX_LPC_ORDER, MAX_LTP_MEM_LENGTH, MAX_NB_SUBFR, MAX_SUB_FRAME_LENGTH,
    MIN_DELTA_GAIN_QUANT, N_LEVELS_QGAIN, NLSF_QUANT_MAX_AMPLITUDE, OUT_BUF_LEN,
    SUB_FRAME_LENGTH_MS, TYPE_NO_VOICE_ACTIVITY, TYPE_VOICED, add_lshift32, add_rshift32,
    add_sat32, div32_var_q, inverse32_var_q, limit, lshift, lshift_sat32, mla, mul, rshift,
    rshift_round, sat16, smlabb, smlawb, smulbb, smulwb, smulww,
};
use crate::decoder::DecodeError;
use crate::formats::opus::range::RangeDecoder;
use crate::formats::opus::toc::{Bandwidth, Config, Mode};

/// `MAX_FRAMES_PER_PACKET` (silk/define.h): up to three 20 ms SILK frames
/// (60 ms) in one Opus packet.
const MAX_FRAMES_PER_PACKET: usize = 3;

/// 48 kHz output samples one internal SILK frame (20 ms) resamples to.
const SILK_OUT_48K_PER_FRAME: usize = 960;

/// `CODE_{INDEPENDENTLY,INDEPENDENTLY_NO_LTP_SCALING,CONDITIONALLY}`
/// (silk/define.h) — the conditional-coding mode of one internal SILK frame,
/// selecting delta-vs-absolute gains, relative-vs-absolute pitch, and whether
/// the LTP scaling factor is coded.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CondCoding {
    /// Fully independent: absolute gains, absolute pitch, LTP scale coded.
    Independently,
    /// Independent gains/pitch but the LTP scale is **not** coded (defaults to
    /// 0) — used for a side frame following a mid-only frame in the same stream.
    IndependentlyNoLtpScaling,
    /// Conditionally coded: delta gains, relative pitch, no LTP scale.
    Conditionally,
}

impl CondCoding {
    /// `condCoding == CODE_CONDITIONALLY` — the delta-gain / relative-pitch gate.
    #[inline]
    fn is_conditional(self) -> bool {
        self == CondCoding::Conditionally
    }
    /// `condCoding == CODE_INDEPENDENTLY` — the LTP-scale-is-coded gate.
    #[inline]
    fn is_fully_independent(self) -> bool {
        self == CondCoding::Independently
    }
}

/// Per-frame front-end output: the dequantized subframe gains (linear, Q16) and
/// per-half LPC coefficients (Q12) SILK synthesis ([`SilkChannel::decode_core`])
/// consumes. The bulky per-sample `pulses`/`exc_Q14` buffers live in
/// [`SilkChannel`] state (mirroring libopus's in-state `psDec->exc_Q14`), not
/// here — this struct stays small enough to return by value.
pub(crate) struct SilkFrontEnd {
    /// Linear subframe gains, `nb_subfr` valid (`silk_decode_parameters`
    /// `Gains_Q16`).
    pub gains_q16: [i32; MAX_NB_SUBFR],
    /// `PredCoef_Q12[half][coef]` — half 0 is the interpolated first-two-subframe
    /// filter, half 1 the frame's main filter. `lpc_order` coefficients valid.
    pub pred_coef_q12: [[i16; MAX_LPC_ORDER]; 2],
    /// Number of subframes (2 for a 10 ms frame, 4 otherwise).
    pub nb_subfr: usize,
    /// LPC order (10 for NB/MB, 16 for WB).
    pub lpc_order: usize,
    /// `TYPE_{NO_VOICE_ACTIVITY,UNVOICED,VOICED}`.
    pub signal_type: i32,
    /// `NLSF_interpolation_flag` (`NLSFInterpCoef_Q2 < 4`): drives the `k == 2`
    /// re-whitening in [`SilkChannel::decode_core`].
    pub nlsf_interp_flag: bool,
    /// Long-term-prediction parameters — only meaningful when
    /// `signal_type == TYPE_VOICED` (zeroed otherwise).
    pub ltp: LtpParams,
}

/// One SILK channel's cross-frame state (fixed-size, no heap): the previous
/// NLSF vector, running gain index, LTP/LPC filter memory, the output-signal
/// history, and the output resampler's delay lines. A stereo decoder holds two
/// of these (mid + side); a mono decoder uses only the first.
pub(crate) struct SilkChannel {
    /// Internal sample rate in kHz (8/12/16); `0` = uninitialized (forces reset).
    fs_khz: i32,
    /// `LastGainIndex` — the previous frame's final gain index (init 10).
    last_gain_index: i32,
    /// `prevNLSF_Q15` — previous frame's stabilized NLSF vector.
    prev_nlsf_q15: [i16; MAX_LPC_ORDER],
    /// `first_frame_after_reset` — forces `NLSFInterpCoef_Q2 = 4` (no interp).
    first_frame_after_reset: bool,
    /// `ec_prevSignalType` (conditional/delta pitch coding across frames).
    prev_signal_type: i32,
    /// `ec_prevLagIndex` — previous voiced frame's pitch-lag index (for the
    /// relative/delta lag path in conditionally-coded frames).
    ec_prev_lag_index: i32,
    /// `exc_Q14` — the reconstructed Q14 excitation residual for the current
    /// frame (`silk_decode_core`'s in-state buffer). `frame_length` valid.
    pub exc_q14: [i32; MAX_FRAME_LENGTH],
    /// Signed excitation pulse magnitudes (`silk_decode_pulses` output), a
    /// per-frame transient carried in-state to keep [`SilkFrontEnd`] small.
    pub pulses: [i16; MAX_FRAME_LENGTH],
    /// `sLPC_Q14_buf` — the short-term LPC synthesis filter memory, carried
    /// across subframes and frames.
    s_lpc_q14: [i32; MAX_LPC_ORDER],
    /// `outBuf` — decoded-output history for the voiced re-whitening
    /// (`silk_LPC_analysis_filter`), holding the last `ltp_mem_length` samples.
    out_buf: [i16; OUT_BUF_LEN],
    /// `prev_gain_Q16` — the previous subframe's gain (Q16), for the inter-frame
    /// gain-adjustment scaling of the filter state. Init `65536` (= 1.0).
    prev_gain_q16: i32,
    /// The SILK→48 kHz output resampler (delay lines carried across frames).
    resampler: Resampler,
}

impl Default for SilkChannel {
    fn default() -> Self {
        Self {
            fs_khz: 0,
            last_gain_index: 10,
            prev_nlsf_q15: [0; MAX_LPC_ORDER],
            first_frame_after_reset: true,
            prev_signal_type: TYPE_NO_VOICE_ACTIVITY,
            ec_prev_lag_index: 0,
            exc_q14: [0; MAX_FRAME_LENGTH],
            pulses: [0; MAX_FRAME_LENGTH],
            s_lpc_q14: [0; MAX_LPC_ORDER],
            out_buf: [0; OUT_BUF_LEN],
            prev_gain_q16: 65536,
            resampler: Resampler::default(),
        }
    }
}

/// Cross-frame SILK decoder state (fixed-size, no heap): two per-channel
/// [`SilkChannel`] states (mid + side) plus the stereo mid/side prediction +
/// delay state, mirroring libopus's `silk_decoder` (`channel_state[2]` +
/// `sStereo` + `prev_decode_only_middle`).
pub(crate) struct SilkDecoder {
    /// Per-channel states: `[0]` = mid (mono uses only this), `[1]` = side.
    pub channels: [SilkChannel; 2],
    /// `sStereo` — the stereo mid/side prediction + 1-sample-delay state,
    /// carried across frames (and shared with the mono buffering path).
    stereo: StereoState,
    /// `prev_decode_only_middle` — whether the previous frame coded only the mid
    /// channel (drives the side-reset + `CODE_INDEPENDENTLY_NO_LTP_SCALING`).
    prev_decode_only_middle: bool,
    /// `psDec->nChannelsInternal` from the previous packet (init 0), for the
    /// mono→stereo transition reset of the stereo state.
    prev_channels_internal: usize,
    /// `psDec->nChannelsAPI` from the previous packet (init 0), same transition.
    prev_channels_api: usize,
}

impl Default for SilkDecoder {
    // libopus's `silk_InitDecoder` zeroes nChannelsAPI/Internal, so the very
    // first stereo packet's mono→stereo transition reset does NOT fire (its
    // condition tests for a previous value of 1) — a no-op on a fresh decoder
    // anyway (pred/sSide already zero, both resamplers freshly equal).
    #[allow(clippy::derivable_impls)]
    fn default() -> Self {
        Self {
            channels: [SilkChannel::default(), SilkChannel::default()],
            stereo: StereoState::default(),
            prev_decode_only_middle: false,
            prev_channels_internal: 0,
            prev_channels_api: 0,
        }
    }
}

/// Decoded side-information indices for one SILK frame's front-end.
struct Indices {
    signal_type: i32,
    quant_offset_type: i32,
    /// Gain indices (`GainsIndices`), `nb_subfr` valid.
    gains: [i8; MAX_NB_SUBFR],
    /// NLSF codebook path (`NLSFIndices`): `[0]` = stage-1 index, `[1..=order]`
    /// = stage-2 residual indices.
    nlsf: [i8; MAX_LPC_ORDER + 1],
    /// `NLSFInterpCoef_Q2` (0..4).
    interp_q2: i32,
    /// `lagIndex` — absolute (or delta-reconstructed) pitch-lag index (voiced).
    lag_index: i32,
    /// `contourIndex` — pitch-contour codebook index (voiced).
    contour_index: i32,
    /// `PERIndex` — LTP periodicity index selecting one of 3 filter codebooks.
    per_index: i32,
    /// `LTPIndex[k]` — per-subframe LTP filter index (voiced).
    ltp_index: [i8; MAX_NB_SUBFR],
    /// `LTP_scaleIndex` — LTP scaling factor index (voiced, independent coding).
    ltp_scale_index: i32,
    /// `Seed` — 2-bit LCG seed for excitation reconstruction.
    seed: i32,
}

impl SilkChannel {
    /// `silk_decoder_set_fs` (reset subset): on an internal-sample-rate change,
    /// reset the cross-frame state the way libopus does (LSF interp disabled,
    /// gain index re-seeded to 10, NLSF + LTP/LPC + output history cleared) and
    /// re-initialize the output resampler for `fs_khz → 48 kHz`.
    fn set_fs(&mut self, fs_khz: i32) {
        if self.fs_khz != fs_khz {
            self.fs_khz = fs_khz;
            self.first_frame_after_reset = true;
            self.last_gain_index = 10;
            self.prev_signal_type = TYPE_NO_VOICE_ACTIVITY;
            self.prev_nlsf_q15 = [0; MAX_LPC_ORDER];
            self.ec_prev_lag_index = 0;
            // Synthesis state (silk_decoder_set_fs clears these on an fs change).
            self.s_lpc_q14 = [0; MAX_LPC_ORDER];
            self.out_buf = [0; OUT_BUF_LEN];
            self.prev_gain_q16 = 65536;
            self.resampler = Resampler::init(fs_khz);
        }
    }

    /// Reset the side channel's decoder prediction memory (`silk_Decode`: the
    /// "first frame with side coding" reset when transitioning out of a mid-only
    /// run). Mirrors libopus clearing `outBuf`/`sLPC_Q14_buf` and re-seeding
    /// `LastGainIndex`/`prevSignalType`/`first_frame_after_reset`.
    fn reset_side(&mut self) {
        self.out_buf = [0; OUT_BUF_LEN];
        self.s_lpc_q14 = [0; MAX_LPC_ORDER];
        self.last_gain_index = 10;
        self.prev_signal_type = TYPE_NO_VOICE_ACTIVITY;
        self.first_frame_after_reset = true;
    }

    /// Decode one internal SILK frame's side info + excitation (`silk_decode_
    /// indices` → `silk_decode_pulses` → `silk_decode_parameters`), writing the
    /// per-frame pulses/excitation into `self.pulses`/`self.exc_q14` and
    /// advancing the cross-frame state (NLSF history, gain index, ec_prev*,
    /// first_frame_after_reset).
    #[allow(clippy::too_many_arguments)]
    fn decode_one_frame(
        &mut self,
        rd: &mut RangeDecoder,
        fs_khz: i32,
        nb_subfr: usize,
        order: usize,
        cb: &NlsfCb,
        vad_flag: bool,
        cond: CondCoding,
    ) -> SilkFrontEnd {
        let idx = decode_indices(
            rd,
            vad_flag,
            cb,
            fs_khz,
            nb_subfr,
            order,
            cond,
            self.prev_signal_type,
            &mut self.ec_prev_lag_index,
        );

        // Dequant gains (silk_gains_dequant).
        let mut gains_q16 = [0i32; MAX_NB_SUBFR];
        gains_dequant(
            &mut gains_q16,
            &idx.gains,
            &mut self.last_gain_index,
            cond.is_conditional(),
            nb_subfr,
        );

        // Decode + stabilize NLSFs, convert to LPC (main filter, half 1).
        let mut nlsf_q15 = [0i16; MAX_LPC_ORDER];
        nlsf_decode(&mut nlsf_q15[..order], &idx.nlsf, cb);
        let mut pred = [[0i16; MAX_LPC_ORDER]; 2];
        nlsf2a(&mut pred[1][..order], &nlsf_q15[..order], order);

        // LSF interpolation for the first two subframes (half 0).
        let interp_q2 = if self.first_frame_after_reset {
            4
        } else {
            idx.interp_q2
        };
        let nlsf_interp_flag = interp_q2 < 4;
        if nlsf_interp_flag {
            let mut nlsf0 = [0i16; MAX_LPC_ORDER];
            lsf_interpolate(
                &mut nlsf0[..order],
                &self.prev_nlsf_q15[..order],
                &nlsf_q15[..order],
                interp_q2,
                order,
            );
            nlsf2a(&mut pred[0][..order], &nlsf0[..order], order);
        } else {
            pred[0] = pred[1];
        }

        self.prev_nlsf_q15[..order].copy_from_slice(&nlsf_q15[..order]);
        self.first_frame_after_reset = false;
        self.prev_signal_type = idx.signal_type;

        let frame_length = nb_subfr * SUB_FRAME_LENGTH_MS * (fs_khz as usize);

        // Long-term prediction parameters (voiced frames only).
        let ltp = if idx.signal_type == TYPE_VOICED {
            decode_ltp(
                idx.lag_index,
                idx.contour_index,
                idx.per_index,
                &idx.ltp_index,
                idx.ltp_scale_index,
                fs_khz,
                nb_subfr,
            )
        } else {
            LtpParams::default()
        };

        // Excitation: shell-coded pulses, then reconstruct the Q14 residual
        // (both into decoder state, matching libopus's in-state buffers).
        decode_pulses(
            rd,
            &mut self.pulses,
            idx.signal_type,
            idx.quant_offset_type,
            frame_length,
        );
        reconstruct_excitation(
            &mut self.exc_q14,
            &self.pulses,
            idx.seed,
            idx.signal_type,
            idx.quant_offset_type,
            frame_length,
        );

        SilkFrontEnd {
            gains_q16,
            pred_coef_q12: pred,
            nb_subfr,
            lpc_order: order,
            signal_type: idx.signal_type,
            nlsf_interp_flag,
            ltp,
        }
    }

    /// `silk_decode_core` (silk/decode_core.c) — the inverse NSQ: per subframe,
    /// LTP-synthesize the LPC residual from `self.exc_q14` (voiced) using the
    /// 5-tap `LTPCoef_Q14` over the `pitchL` lag from the re-whitened `sLTP`
    /// history, run the short-term LPC filter (`PredCoef_Q12`, order 10/16) with
    /// the `sLPC_Q14` state, and scale by the subframe gain — producing `xq`
    /// (int16, internal rate). Carries `s_lpc_q14`/`prev_gain_q16` across
    /// subframes and frames; reads the previous frame's `out_buf` history.
    #[allow(clippy::needless_range_loop)] // per-sample state/index arithmetic mirrors libopus
    fn decode_core(&mut self, fe: &SilkFrontEnd, fs_khz: usize, xq: &mut [i16]) {
        let nb_subfr = fe.nb_subfr;
        let order = fe.lpc_order;
        let subfr_length = SUB_FRAME_LENGTH_MS * fs_khz;
        let ltp_mem_length = LTP_MEM_LENGTH_MS * fs_khz;
        let signal_type = fe.signal_type;
        let voiced = signal_type == TYPE_VOICED;

        // Per-call scratch (sized to the widest configuration).
        let mut s_ltp = [0i16; MAX_LTP_MEM_LENGTH];
        let mut s_ltp_q15 = [0i32; MAX_LTP_MEM_LENGTH + MAX_FRAME_LENGTH];
        let mut res_q14 = [0i32; MAX_SUB_FRAME_LENGTH];
        let mut s_lpc_q14 = [0i32; MAX_SUB_FRAME_LENGTH + MAX_LPC_ORDER];

        // Copy the carried-over LPC state into the working buffer.
        s_lpc_q14[..MAX_LPC_ORDER].copy_from_slice(&self.s_lpc_q14);

        let mut s_ltp_buf_idx = ltp_mem_length;
        let mut lag = 0usize;

        for k in 0..nb_subfr {
            let a_q12 = &fe.pred_coef_q12[k >> 1];
            let b_q14 = &fe.ltp.coef_q14[k];
            let gain_q16 = fe.gains_q16[k];
            let gain_q10 = rshift(gain_q16, 6);
            let mut inv_gain_q31 = inverse32_var_q(gain_q16, 47);

            // Gain-adjustment factor + short-term state rescale.
            let gain_adj_q16 = if gain_q16 != self.prev_gain_q16 {
                let g = div32_var_q(self.prev_gain_q16, gain_q16, 16);
                for v in s_lpc_q14[..MAX_LPC_ORDER].iter_mut() {
                    *v = smulww(g, *v);
                }
                g
            } else {
                1 << 16
            };
            self.prev_gain_q16 = gain_q16;

            if voiced {
                lag = fe.ltp.pitch_l[k] as usize;
                if k == 0 || (k == 2 && fe.nlsf_interp_flag) {
                    // Re-whiten the output history with the current LPC coefs.
                    let start_idx =
                        (ltp_mem_length as i32 - lag as i32 - order as i32 - LTP_ORDER as i32 / 2)
                            .max(0) as usize;
                    if k == 2 {
                        self.out_buf[ltp_mem_length..ltp_mem_length + 2 * subfr_length]
                            .copy_from_slice(&xq[..2 * subfr_length]);
                    }
                    let filt_off = start_idx + k * subfr_length;
                    let filt_len = ltp_mem_length - start_idx;
                    lpc_analysis_filter(
                        &mut s_ltp[start_idx..ltp_mem_length],
                        &self.out_buf[filt_off..filt_off + filt_len],
                        &a_q12[..order],
                    );
                    if k == 0 {
                        // LTP downscaling to reduce inter-packet dependency.
                        inv_gain_q31 = lshift(smulwb(inv_gain_q31, fe.ltp.scale_q14), 2);
                    }
                    for i in 0..(lag + LTP_ORDER / 2) {
                        s_ltp_q15[s_ltp_buf_idx - i - 1] =
                            smulwb(inv_gain_q31, i32::from(s_ltp[ltp_mem_length - i - 1]));
                    }
                } else if gain_adj_q16 != 1 << 16 {
                    for i in 0..(lag + LTP_ORDER / 2) {
                        s_ltp_q15[s_ltp_buf_idx - i - 1] =
                            smulww(gain_adj_q16, s_ltp_q15[s_ltp_buf_idx - i - 1]);
                    }
                }
            }

            let base = k * subfr_length;

            // Long-term prediction → residual res_Q14 (voiced only).
            if voiced {
                let mut plp = s_ltp_buf_idx - lag + LTP_ORDER / 2;
                for i in 0..subfr_length {
                    let mut ltp_pred_q13 = 2i32;
                    ltp_pred_q13 = smlawb(ltp_pred_q13, s_ltp_q15[plp], i32::from(b_q14[0]));
                    ltp_pred_q13 = smlawb(ltp_pred_q13, s_ltp_q15[plp - 1], i32::from(b_q14[1]));
                    ltp_pred_q13 = smlawb(ltp_pred_q13, s_ltp_q15[plp - 2], i32::from(b_q14[2]));
                    ltp_pred_q13 = smlawb(ltp_pred_q13, s_ltp_q15[plp - 3], i32::from(b_q14[3]));
                    ltp_pred_q13 = smlawb(ltp_pred_q13, s_ltp_q15[plp - 4], i32::from(b_q14[4]));
                    plp += 1;
                    res_q14[i] = add_lshift32(self.exc_q14[base + i], ltp_pred_q13, 1);
                    s_ltp_q15[s_ltp_buf_idx] = lshift(res_q14[i], 1);
                    s_ltp_buf_idx += 1;
                }
            }

            // Short-term LPC prediction + gain scaling.
            for i in 0..subfr_length {
                let mut lpc_pred_q10 = rshift(order as i32, 1);
                for (j, &a) in a_q12[..order].iter().enumerate() {
                    lpc_pred_q10 = smlawb(
                        lpc_pred_q10,
                        s_lpc_q14[MAX_LPC_ORDER + i - 1 - j],
                        i32::from(a),
                    );
                }
                let pres_i = if voiced {
                    res_q14[i]
                } else {
                    self.exc_q14[base + i]
                };
                s_lpc_q14[MAX_LPC_ORDER + i] = add_sat32(pres_i, lshift_sat32(lpc_pred_q10, 4));
                xq[base + i] = sat16(rshift_round(
                    smulww(s_lpc_q14[MAX_LPC_ORDER + i], gain_q10),
                    8,
                )) as i16;
            }

            // Shift the LPC filter state down for the next subframe.
            s_lpc_q14.copy_within(subfr_length..subfr_length + MAX_LPC_ORDER, 0);
        }

        // Save the LPC filter state for the next frame.
        self.s_lpc_q14.copy_from_slice(&s_lpc_q14[..MAX_LPC_ORDER]);
    }

    /// Update the output-signal history after synthesizing one frame
    /// (`silk_decode_frame`): keep the last `ltp_mem_length` samples ending with
    /// this frame, for the next frame's voiced re-whitening.
    fn update_out_buf(&mut self, xq: &[i16], fs_khz: usize, frame_length: usize) {
        let ltp_mem_length = LTP_MEM_LENGTH_MS * fs_khz;
        let mv_len = ltp_mem_length - frame_length;
        self.out_buf.copy_within(frame_length..ltp_mem_length, 0);
        self.out_buf[mv_len..ltp_mem_length].copy_from_slice(&xq[..frame_length]);
    }
}

impl SilkDecoder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Decode the front-end of the **first** internal SILK frame of a mono Opus
    /// packet: the VAD/LBRR header flags, then the side-info indices, then
    /// dequantize to per-subframe gains + LPC coefficients + LTP + excitation.
    /// Used only by the front-end unit + oracle tests below (mono only); the
    /// shipping decode path goes through [`Self::decode_packet`] instead.
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] for a non-SILK config, a Hybrid bandwidth, or
    /// a packet that signals in-band FEC (LBRR). [`DecodeError::Malformed`] for
    /// an out-of-range frame size.
    #[cfg(test)]
    pub(crate) fn decode_frontend(
        &mut self,
        rd: &mut RangeDecoder,
        config: Config,
    ) -> Result<SilkFrontEnd, DecodeError> {
        let (fs_khz, nb_subfr, n_frames) = silk_geometry(config)?;
        self.channels[0].set_fs(fs_khz);
        let (order, cb): (usize, &NlsfCb) = if fs_khz == 16 {
            (16, &NLSF_CB_WB)
        } else {
            (10, &NLSF_CB_NB_MB)
        };

        // Packet header (silk/dec_API.c): a VAD flag per internal frame, then a
        // single LBRR flag — all before any frame's side info.
        let mut vad0 = false;
        for i in 0..n_frames {
            let v = rd.decode_bit_logp(1) != 0;
            if i == 0 {
                vad0 = v;
            }
        }
        let lbrr_flag = rd.decode_bit_logp(1) != 0;
        if lbrr_flag {
            // In-band FEC (LBRR) prepends extra coded frames the single-frame
            // front-end path doesn't skip; the full decoder handles them.
            return Err(DecodeError::Unsupported);
        }

        // First internal frame is always independently coded (FrameIndex 0).
        Ok(self.channels[0].decode_one_frame(
            rd,
            fs_khz,
            nb_subfr,
            order,
            cb,
            vad0,
            CondCoding::Independently,
        ))
    }

    /// Decode one **SILK-only or Hybrid** Opus packet end-to-end into `out48`
    /// (interleaved 16-bit PCM in `i32` slots, `api_channels` per sample),
    /// applying `output_gain`. Returns the number of `i32` written.
    ///
    /// Mirrors `silk/dec_API.c`'s `silk_Decode`: it handles a **mono** packet
    /// (byte-for-byte the previous mono path, optionally duplicated to two output
    /// channels for a stereo-header stream) and a **stereo** packet (two channel
    /// states → `silk_stereo_MS_to_LR` → resample each of L/R → interleave). The
    /// packet's internal channel count comes from the TOC `config.stereo` s-bit;
    /// `api_channels` is the stream's `OpusHead` channel count.
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] for a non-SILK / Hybrid config;
    /// [`DecodeError::Malformed`] for an out-of-range frame size.
    pub(crate) fn decode_packet(
        &mut self,
        rd: &mut RangeDecoder,
        config: Config,
        api_channels: usize,
        out48: &mut [i32],
        output_gain: f32,
    ) -> Result<usize, DecodeError> {
        // SILK-only and Hybrid both decode a SILK layer here; Hybrid additionally
        // decodes a CELT high band from the same range coder afterward (handled
        // by the caller). CELT-only never reaches this path.
        if !matches!(config.mode, Mode::SilkOnly | Mode::Hybrid) {
            return Err(DecodeError::Unsupported);
        }
        let api_channels = api_channels.clamp(1, 2);
        let internal_channels = if config.stereo { 2 } else { 1 };
        let (fs_khz, nb_subfr, n_frames) = silk_geometry(config)?;
        let (order, cb): (usize, &NlsfCb) = if fs_khz == 16 {
            (16, &NLSF_CB_WB)
        } else {
            (10, &NLSF_CB_NB_MB)
        };
        let frame_length = nb_subfr * SUB_FRAME_LENGTH_MS * (fs_khz as usize);

        // Set the internal sample rate on every internal channel (silk_Decode's
        // per-channel silk_decoder_set_fs), then — on a mono→stereo transition —
        // reset the stereo prediction/side state and seed the side resampler from
        // the mid channel's (silk_Decode lines guarding pred_prev/sSide/resampler).
        for ch in self.channels.iter_mut().take(internal_channels) {
            ch.set_fs(fs_khz);
        }
        if api_channels == 2
            && internal_channels == 2
            && (self.prev_channels_api == 1 || self.prev_channels_internal == 1)
        {
            self.stereo.pred_prev_q13 = [0; 2];
            self.stereo.s_side = [0; 2];
            self.channels[1].resampler = self.channels[0].resampler.clone();
        }

        // --- Packet header: per internal channel, VAD flags + LBRR flag ---
        let mut vad = [[false; MAX_FRAMES_PER_PACKET]; 2];
        let mut lbrr_flag = [false; 2];
        for n in 0..internal_channels {
            for v in vad[n].iter_mut().take(n_frames) {
                *v = rd.decode_bit_logp(1) != 0;
            }
            lbrr_flag[n] = rd.decode_bit_logp(1) != 0;
        }

        // --- LBRR present-flags (per internal channel) ---
        let mut lbrr_flags = [[false; MAX_FRAMES_PER_PACKET]; 2];
        for n in 0..internal_channels {
            if lbrr_flag[n] {
                if n_frames == 1 {
                    lbrr_flags[n][0] = true;
                } else {
                    let icdf: &[u8] = if n_frames == 2 {
                        &LBRR_FLAGS_2_ICDF
                    } else {
                        &LBRR_FLAGS_3_ICDF
                    };
                    let symbol = rd.decode_icdf(icdf, 8) as i32 + 1;
                    for (i, f) in lbrr_flags[n].iter_mut().enumerate().take(n_frames) {
                        *f = (symbol >> i) & 1 == 1;
                    }
                }
            }
        }

        // --- Skip all LBRR data (FLAG_DECODE_NORMAL) so the range coder stays
        // aligned. Decode indices + pulses and discard; this DOES advance the
        // shared per-channel ec_prev* state, exactly as libopus does. For stereo,
        // the mid channel additionally decodes the stereo predictor (+ mid-only
        // flag) ahead of its indices. ---
        for i in 0..n_frames {
            for n in 0..internal_channels {
                if lbrr_flags[n][i] {
                    if internal_channels == 2 && n == 0 {
                        let _ = stereo_decode_pred(rd);
                        if !lbrr_flags[1][i] {
                            let _ = stereo_decode_mid_only(rd);
                        }
                    }
                    let cond = if i > 0 && lbrr_flags[n][i - 1] {
                        CondCoding::Conditionally
                    } else {
                        CondCoding::Independently
                    };
                    let ch = &mut self.channels[n];
                    let idx = decode_indices(
                        rd,
                        true, // decode_LBRR forces the VAD (voiced) type iCDF
                        cb,
                        fs_khz,
                        nb_subfr,
                        order,
                        cond,
                        ch.prev_signal_type,
                        &mut ch.ec_prev_lag_index,
                    );
                    ch.prev_signal_type = idx.signal_type;
                    decode_pulses(
                        rd,
                        &mut ch.pulses,
                        idx.signal_type,
                        idx.quant_offset_type,
                        frame_length,
                    );
                }
            }
        }

        // --- Regular frames: decode both channels → unmix → resample ---
        let mut written = 0usize;
        // +2-offset internal-rate buffers for MS_to_LR (mid=buf0, side=buf1);
        // the decoded samples land at [2..], the L/R at [1..frame_length+1].
        let mut buf = [[0i16; 2 + MAX_FRAME_LENGTH]; 2];
        let mut out16 = [[0i16; SILK_OUT_48K_PER_FRAME]; 2];
        #[allow(clippy::needless_range_loop)] // frame_idx indexes vad + drives condCoding
        for frame_idx in 0..n_frames {
            // Stereo prediction weights + mid-only flag (mid channel only).
            let mut ms_pred_q13 = [0i32; 2];
            let mut decode_only_middle = false;
            if internal_channels == 2 {
                ms_pred_q13 = stereo_decode_pred(rd);
                if !vad[1][frame_idx] {
                    decode_only_middle = stereo_decode_mid_only(rd);
                }
            }

            // Reset the side channel when transitioning out of a mid-only run.
            if internal_channels == 2 && !decode_only_middle && self.prev_decode_only_middle {
                self.channels[1].reset_side();
            }

            let has_side = internal_channels == 2 && !decode_only_middle;

            // Decode each internal channel to the internal-rate signal at [2..].
            for n in 0..internal_channels {
                if n == 0 || has_side {
                    let cond = if frame_idx == 0 {
                        CondCoding::Independently
                    } else if n > 0 && self.prev_decode_only_middle {
                        CondCoding::IndependentlyNoLtpScaling
                    } else {
                        CondCoding::Conditionally
                    };
                    let ch = &mut self.channels[n];
                    let fe = ch.decode_one_frame(
                        rd,
                        fs_khz,
                        nb_subfr,
                        order,
                        cb,
                        vad[n][frame_idx],
                        cond,
                    );
                    ch.decode_core(&fe, fs_khz as usize, &mut buf[n][2..2 + frame_length]);
                    // Clone the frame slice out to satisfy the borrow checker,
                    // then push it into the output history.
                    let mut xq = [0i16; MAX_FRAME_LENGTH];
                    xq[..frame_length].copy_from_slice(&buf[n][2..2 + frame_length]);
                    ch.update_out_buf(&xq[..frame_length], fs_khz as usize, frame_length);
                } else {
                    // Mid-only: zero the (undecoded) side signal.
                    for s in buf[1][2..2 + frame_length].iter_mut() {
                        *s = 0;
                    }
                }
            }

            // Unmix + resample + interleave, per silk_Decode's output stage.
            let n_out = if api_channels == 2 && internal_channels == 2 {
                // Convert Mid/Side → Left/Right, then resample each of L/R.
                let (b0, b1) = buf.split_at_mut(1);
                ms_to_lr(
                    &mut self.stereo,
                    &mut b0[0][..frame_length + 2],
                    &mut b1[0][..frame_length + 2],
                    &ms_pred_q13,
                    fs_khz as usize,
                    frame_length,
                );
                let nl = self.channels[0]
                    .resampler
                    .process(&mut out16[0], &buf[0][1..1 + frame_length]);
                let nr = self.channels[1]
                    .resampler
                    .process(&mut out16[1], &buf[1][1..1 + frame_length]);
                nl.min(nr)
            } else {
                // Mono internal signal: the mid buffering path (silk_Decode's
                // `else` branch) — prepend sStereo.sMid, save the new history,
                // resample from index 1. Byte-for-byte the previous mono path.
                buf[0][0] = self.stereo.s_mid[0];
                buf[0][1] = self.stereo.s_mid[1];
                self.stereo.s_mid[0] = buf[0][frame_length];
                self.stereo.s_mid[1] = buf[0][frame_length + 1];
                self.channels[0]
                    .resampler
                    .process(&mut out16[0], &buf[0][1..1 + frame_length])
            };

            // Emit: interleave to `api_channels` (duplicating mono → stereo).
            written += emit_samples(
                out48,
                written,
                &out16,
                n_out,
                api_channels,
                internal_channels == 2,
                output_gain,
            );

            self.prev_decode_only_middle = decode_only_middle;
        }

        self.prev_channels_api = api_channels;
        self.prev_channels_internal = internal_channels;
        Ok(written)
    }
}

/// Interleave one frame's per-channel 48 kHz output into `out48` at `written`,
/// applying `output_gain`. `stereo_internal` selects the true-stereo (two
/// distinct `out16` rows) vs mono-duplicate (`out16[0]` to both) path. Returns
/// the number of `i32` slots written (bounded to `out48`'s capacity).
#[allow(clippy::needless_range_loop)] // i indexes both out16 rows + writes 2 out48 slots
fn emit_samples(
    out48: &mut [i32],
    written: usize,
    out16: &[[i16; SILK_OUT_48K_PER_FRAME]; 2],
    n_out: usize,
    api_channels: usize,
    stereo_internal: bool,
    output_gain: f32,
) -> usize {
    let mut w = written;
    if api_channels == 2 {
        for i in 0..n_out {
            if w + 1 >= out48.len() {
                break;
            }
            let l = out16[0][i];
            let r = if stereo_internal { out16[1][i] } else { l };
            out48[w] = apply_output_gain(l, output_gain);
            out48[w + 1] = apply_output_gain(r, output_gain);
            w += 2;
        }
    } else {
        for i in 0..n_out {
            if w >= out48.len() {
                break;
            }
            out48[w] = apply_output_gain(out16[0][i], output_gain);
            w += 1;
        }
    }
    w - written
}

/// Apply the linear `OpusHead` output gain to one int16 SILK sample and clamp to
/// the int16 range (returned in an `i32` slot). Gain `1.0` is an exact identity.
#[inline]
fn apply_output_gain(sample: i16, output_gain: f32) -> i32 {
    let v = f32::from(sample) * output_gain;
    let r = if v >= 0.0 { v + 0.5 } else { v - 0.5 } as i32;
    r.clamp(i16::MIN as i32, i16::MAX as i32)
}

/// `silk_LPC_analysis_filter` (silk/LPC_analysis_filter.c, non-CELT-FIR path):
/// whiten `in_` with the Q12 MA prediction coefficients `b` into `out`
/// (`out[d..len] = in[d..len] − Σ b·past`, saturated to int16; `out[0..d] = 0`).
/// `out` and `in_` share the same length; `b.len() == d` (the filter order).
fn lpc_analysis_filter(out: &mut [i16], in_: &[i16], b: &[i16]) {
    let d = b.len();
    let len = out.len().min(in_.len());
    for ix in d..len {
        // Σ_{j=0}^{d-1} in[ix-1-j] * b[j]  (wrapping, matches silk_SMLABB_ovflw).
        let mut acc = 0i32;
        for (j, &bc) in b.iter().enumerate() {
            acc = smlabb(acc, i32::from(in_[ix - 1 - j]), i32::from(bc));
        }
        // in[ix]<<12 − acc, rounded to Q0, saturated to int16.
        let out32_q12 = super::sub32(lshift(i32::from(in_[ix]), 12), acc);
        out[ix] = sat16(rshift_round(out32_q12, 12)) as i16;
    }
    for slot in out.iter_mut().take(d.min(len)) {
        *slot = 0;
    }
}

/// Map an Opus SILK/Hybrid [`Config`] to `(fs_kHz, nb_subfr, n_frames)`
/// (silk/dec_API.c: `payloadSize_ms` → subframe/frame counts, bandwidth →
/// internal sample rate).
fn silk_geometry(config: Config) -> Result<(i32, usize, usize), DecodeError> {
    let (nb_subfr, n_frames) = match config.frame_size_48k {
        480 => (2, 1),  // 10 ms
        960 => (4, 1),  // 20 ms
        1920 => (4, 2), // 40 ms
        2880 => (4, 3), // 60 ms
        _ => return Err(DecodeError::Malformed),
    };
    let fs_khz = match config.bandwidth {
        Bandwidth::Nb => 8,
        Bandwidth::Mb => 12,
        Bandwidth::Wb => 16,
        // Hybrid (SWB/FB) drives SILK at 16 kHz internally (WB, order-16 LPC);
        // the CELT high band above 8 kHz is decoded separately and summed by
        // [`crate::formats::opus::OpusDecoder`] (libopus `opus_decode_frame`).
        Bandwidth::Swb | Bandwidth::Fb => 16,
    };
    Ok((fs_khz, nb_subfr, n_frames))
}

/// `silk_decode_indices` (silk/decode_indices.c): frame type + quantization
/// offset, subframe gain indices, the NLSF codebook path + interpolation
/// factor, then — for voiced frames — the pitch lag/contour + LTP filter +
/// scaling indices, and finally the 2-bit LCG seed. This mirrors the exact
/// bitstream order the real decoder reads before `silk_decode_pulses`.
#[allow(clippy::too_many_arguments)]
fn decode_indices(
    rd: &mut RangeDecoder,
    vad_flag: bool,
    cb: &NlsfCb,
    fs_khz: i32,
    nb_subfr: usize,
    order: usize,
    cond: CondCoding,
    prev_signal_type: i32,
    ec_prev_lag_index: &mut i32,
) -> Indices {
    // Frame type + quantizer offset.
    let ix = if vad_flag {
        rd.decode_icdf(&TYPE_OFFSET_VAD_ICDF, 8) as i32 + 2
    } else {
        rd.decode_icdf(&TYPE_OFFSET_NO_VAD_ICDF, 8) as i32
    };
    let signal_type = rshift(ix, 1);
    let quant_offset_type = ix & 1;

    // Gains: subframe 0 independent (MSB iCDF + 3 LSBs) or delta; rest delta.
    let mut gains = [0i8; MAX_NB_SUBFR];
    if cond.is_conditional() {
        gains[0] = rd.decode_icdf(&DELTA_GAIN_ICDF, 8) as i8;
    } else {
        let sig = (signal_type as usize).min(GAIN_ICDF.len() - 1);
        let msb = rd.decode_icdf(&GAIN_ICDF[sig], 8) as i32;
        let lsb = rd.decode_icdf(&UNIFORM8_ICDF, 8) as i32;
        gains[0] = (lshift(msb, 3) + lsb) as i8;
    }
    for g in gains.iter_mut().take(nb_subfr).skip(1) {
        *g = rd.decode_icdf(&DELTA_GAIN_ICDF, 8) as i8;
    }

    // NLSF stage-1 index, then per-coefficient stage-2 residual indices.
    let mut nlsf = [0i8; MAX_LPC_ORDER + 1];
    let cb1_row = (rshift(signal_type, 1) as usize) * (cb.n_vectors as usize);
    nlsf[0] = rd.decode_icdf(&cb.cb1_icdf[cb1_row..], 8) as i8;

    let mut ec_ix = [0i16; MAX_LPC_ORDER];
    let mut pred_q8 = [0u8; MAX_LPC_ORDER];
    unpack(&mut ec_ix, &mut pred_q8, cb, nlsf[0] as usize);
    for i in 0..order {
        let col = (ec_ix[i] as usize).min(cb.ec_icdf.len().saturating_sub(1));
        let mut ixv = rd.decode_icdf(&cb.ec_icdf[col..], 8) as i32;
        if ixv == 0 {
            ixv -= rd.decode_icdf(&NLSF_EXT_ICDF, 8) as i32;
        } else if ixv == 2 * NLSF_QUANT_MAX_AMPLITUDE {
            ixv += rd.decode_icdf(&NLSF_EXT_ICDF, 8) as i32;
        }
        nlsf[i + 1] = (ixv - NLSF_QUANT_MAX_AMPLITUDE) as i8;
    }

    // LSF interpolation factor (only present for 4-subframe frames).
    let interp_q2 = if nb_subfr == MAX_NB_SUBFR {
        rd.decode_icdf(&NLSF_INTERP_FACTOR_ICDF, 8) as i32
    } else {
        4
    };

    // Pitch lags + LTP filter + scaling (voiced frames only).
    let mut lag_index = 0i32;
    let mut contour_index = 0i32;
    let mut per_index = 0i32;
    let mut ltp_index = [0i8; MAX_NB_SUBFR];
    let mut ltp_scale_index = 0i32;
    if signal_type == TYPE_VOICED {
        // Pitch lag: relative/delta from the previous voiced frame (only in
        // conditionally-coded frames), else absolute (high iCDF * (fs/2) + low).
        let mut decode_absolute = true;
        if cond.is_conditional() && prev_signal_type == TYPE_VOICED {
            let delta = rd.decode_icdf(&PITCH_DELTA_ICDF, 8) as i32;
            if delta > 0 {
                lag_index = *ec_prev_lag_index + (delta - 9);
                decode_absolute = false;
            }
        }
        if decode_absolute {
            lag_index = (rd.decode_icdf(&PITCH_LAG_ICDF, 8) as i32) * rshift(fs_khz, 1);
            lag_index += rd.decode_icdf(pitch_lag_low_bits_icdf(fs_khz), 8) as i32;
        }
        *ec_prev_lag_index = lag_index;

        // Pitch contour index (iCDF chosen by bandwidth + subframe count).
        contour_index = rd.decode_icdf(pitch_contour_icdf(fs_khz, nb_subfr), 8) as i32;

        // LTP filter: periodicity index selects one of 3 filter-index iCDFs,
        // then a per-subframe filter index.
        per_index = rd.decode_icdf(&LTP_PER_INDEX_ICDF, 8) as i32;
        let gain_icdf = ltp_gain_icdf(per_index);
        for g in ltp_index.iter_mut().take(nb_subfr) {
            *g = rd.decode_icdf(gain_icdf, 8) as i8;
        }

        // LTP scaling factor (coded only in fully-independent frames; else
        // default 0).
        if cond.is_fully_independent() {
            ltp_scale_index = rd.decode_icdf(&LTPSCALE_ICDF, 8) as i32;
        }
    }

    // 2-bit LCG seed (uniform over 4).
    let seed = rd.decode_icdf(&UNIFORM4_ICDF, 8) as i32;

    Indices {
        signal_type,
        quant_offset_type,
        gains,
        nlsf,
        interp_q2,
        lag_index,
        contour_index,
        per_index,
        ltp_index,
        ltp_scale_index,
        seed,
    }
}

/// `psDec->pitch_lag_low_bits_iCDF` (silk/decoder_set_fs.c): the uniform iCDF
/// for the pitch-lag low bits, sized to `fs_kHz / 2` (NB→4, MB→6, WB→8).
fn pitch_lag_low_bits_icdf(fs_khz: i32) -> &'static [u8] {
    match fs_khz {
        8 => &UNIFORM4_ICDF,
        12 => &UNIFORM6_ICDF,
        _ => &UNIFORM8_ICDF,
    }
}

/// `psDec->pitch_contour_iCDF` (silk/decoder_set_fs.c): the pitch-contour iCDF
/// selected by bandwidth (NB vs MB/WB) and frame length (20 ms vs 10 ms).
fn pitch_contour_icdf(fs_khz: i32, nb_subfr: usize) -> &'static [u8] {
    if fs_khz == 8 {
        if nb_subfr == MAX_NB_SUBFR {
            &PITCH_CONTOUR_NB_ICDF
        } else {
            &PITCH_CONTOUR_10MS_NB_ICDF
        }
    } else if nb_subfr == MAX_NB_SUBFR {
        &PITCH_CONTOUR_ICDF
    } else {
        &PITCH_CONTOUR_10MS_ICDF
    }
}

/// `silk_LTP_gain_iCDF_ptrs[ PERIndex ]` (silk/tables_LTP.c): the filter-index
/// iCDF for the given periodicity index (guarded to the 3 valid codebooks).
fn ltp_gain_icdf(per_index: i32) -> &'static [u8] {
    match per_index {
        0 => &LTP_GAIN_ICDF_0,
        1 => &LTP_GAIN_ICDF_1,
        _ => &LTP_GAIN_ICDF_2,
    }
}

/// `silk_gains_dequant` (silk/gain_quant.c): scalar dequantization of the log-
/// domain gain indices to linear Q16, with the delta accumulation + double-step
/// logic and the hysteresis floor for the first (independent) subframe.
fn gains_dequant(
    gains_q16: &mut [i32],
    ind: &[i8],
    prev_ind: &mut i32,
    conditional: bool,
    nb_subfr: usize,
) {
    for k in 0..nb_subfr {
        if k == 0 && !conditional {
            // Not allowed to drop more than 16 steps (~21.8 dB).
            *prev_ind = i32::from(ind[0]).max(*prev_ind - 16);
        } else {
            let ind_tmp = i32::from(ind[k]) + MIN_DELTA_GAIN_QUANT;
            let double_step_size_threshold = 2 * MAX_DELTA_GAIN_QUANT - N_LEVELS_QGAIN + *prev_ind;
            if ind_tmp > double_step_size_threshold {
                *prev_ind += lshift(ind_tmp, 1) - double_step_size_threshold;
            } else {
                *prev_ind += ind_tmp;
            }
        }
        *prev_ind = limit(*prev_ind, 0, N_LEVELS_QGAIN - 1);

        // Scale and convert to linear (3967 = 31 in Q7).
        let in_log_q7 = (smulwb(GAIN_INV_SCALE_Q16, *prev_ind) + GAIN_OFFSET_Q7).min(3967);
        gains_q16[k] = log2lin(in_log_q7);
    }
}

/// `silk_log2lin` (silk/log2lin.c): approximation of `2^(x)` converting a Q7 log
/// value to linear, via a piecewise-parabolic correction.
fn log2lin(in_log_q7: i32) -> i32 {
    if in_log_q7 < 0 {
        return 0;
    } else if in_log_q7 >= 3967 {
        return i32::MAX;
    }
    let mut out = lshift(1, rshift(in_log_q7, 7) as u32);
    let frac_q7 = in_log_q7 & 0x7F;
    // silk_SMLAWB( frac_Q7, silk_SMULBB( frac_Q7, 128 - frac_Q7 ), -174 )
    let correction = smlawb(frac_q7, smulbb(frac_q7, 128 - frac_q7), -174);
    if in_log_q7 < 2048 {
        out = add_rshift32(out, mul(out, correction), 7);
    } else {
        out = mla(out, rshift(out, 7), correction);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::nlsf::is_lpc_stable;
    use super::*;
    use crate::demux::ogg_packets::OggPackets;
    use crate::formats::opus::toc::{self, Mode};
    use crate::io::SliceSource;

    /// Pull the first audio packet's first frame (raw SILK bytes) + its TOC
    /// config out of an Ogg-Opus fixture, skipping the two header packets.
    fn first_silk_frame(bytes: &[u8]) -> (Config, Vec<u8>) {
        let mut ogg = OggPackets::new(SliceSource::new(bytes)).expect("ogg open");
        let mut buf = [0u8; 4096];
        let _ = ogg.next_packet(&mut buf).expect("OpusHead");
        let _ = ogg.next_packet(&mut buf).expect("OpusTags");
        let n = ogg.next_packet(&mut buf).expect("first audio packet");
        let mut frames: [&[u8]; toc::MAX_FRAMES] = [&[][..]; toc::MAX_FRAMES];
        let (config, count) = toc::frames(&buf[..n], &mut frames).expect("frame split");
        assert!(count >= 1);
        (config, frames[0].to_vec())
    }

    /// Pull the `idx`-th audio packet's first frame + config (0-based over the
    /// audio packets that follow the two Ogg header packets).
    fn nth_silk_frame(bytes: &[u8], idx: usize) -> (Config, Vec<u8>) {
        let mut ogg = OggPackets::new(SliceSource::new(bytes)).expect("ogg open");
        let mut buf = [0u8; 4096];
        let _ = ogg.next_packet(&mut buf).expect("OpusHead");
        let _ = ogg.next_packet(&mut buf).expect("OpusTags");
        for _ in 0..idx {
            let _ = ogg.next_packet(&mut buf).expect("skip audio packet");
        }
        let n = ogg.next_packet(&mut buf).expect("audio packet");
        let mut frames: [&[u8]; toc::MAX_FRAMES] = [&[][..]; toc::MAX_FRAMES];
        let (config, count) = toc::frames(&buf[..n], &mut frames).expect("frame split");
        assert!(count >= 1);
        (config, frames[0].to_vec())
    }

    /// FNV-1a over the little-endian `i32` stream — identical to the byte order
    /// the gcc oracle hashes, so its printed constants are directly comparable.
    fn fnv1a_i32(vals: &[i32]) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &v in vals {
            let u = v as u32;
            for b in 0..4 {
                h ^= u64::from((u >> (b * 8)) & 0xff);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        h
    }

    #[test]
    fn decodes_silk_frontend_of_first_voice_frame() {
        // Fixture: mono 16 kbps VoIP Opus (ffmpeg libopus). Its first audio
        // packet is TOC config 9 = SILK / WB / 20 ms (order-16 LPC, 4 subframes).
        let bytes = include_bytes!("../../../../testdata/voice-silk-mono.opus");
        let (config, frame) = first_silk_frame(bytes);
        assert_eq!(config.mode, Mode::SilkOnly, "fixture must be a SILK config");

        let mut rd = RangeDecoder::new(&frame);
        let mut dec = SilkDecoder::new();
        let fe = dec
            .decode_frontend(&mut rd, config)
            .expect("SILK front-end decode");

        assert_eq!(fe.lpc_order, 16, "WB → order 16");
        assert_eq!(fe.nb_subfr, 4, "20 ms → 4 subframes");

        // Gains are positive and within a sane linear-Q16 range.
        for k in 0..fe.nb_subfr {
            let g = fe.gains_q16[k];
            assert!(g > 0, "subframe {k} gain not positive: {g}");
            assert!(g < i32::MAX, "subframe {k} gain saturated: {g}");
        }

        // Both LPC halves are finite and pass libopus's stability check, and the
        // coefficients sit in the expected Q12 magnitude band.
        for half in 0..2 {
            let a = &fe.pred_coef_q12[half][..fe.lpc_order];
            assert!(is_lpc_stable(a, fe.lpc_order), "half {half} LPC unstable");
            for (i, &c) in a.iter().enumerate() {
                assert!(
                    (-16384..=16384).contains(&i32::from(c)),
                    "half {half} coef {i} out of Q12 range: {c}"
                );
            }
        }

        // First frame after reset ⇒ no interpolation ⇒ both halves identical.
        assert_eq!(fe.pred_coef_q12[0], fe.pred_coef_q12[1]);

        // --- Task 10: excitation of the first (unvoiced) SILK frame ---
        assert_eq!(fe.signal_type, 1, "fixture's first frame is unvoiced");
        // WB (16 kHz) 20 ms ⇒ 4 subframes * 5 ms * 16 = 320 samples (`fe.nb_subfr`
        // asserted above); `SilkFrontEnd` doesn't carry `frame_length` itself, so
        // this is the same literal the field used to assert against.
        let frame_length = 320;

        // Per-shell-block pulse counts are sane (0..=~ a few dozen).
        for (blk, chunk) in dec.channels[0].pulses[..frame_length]
            .chunks(16)
            .enumerate()
        {
            let sum: i32 = chunk.iter().map(|&p| i32::from(p).abs()).sum();
            assert!(sum <= 64, "shell block {blk} pulse sum implausible: {sum}");
        }

        // The reconstructed excitation is finite and in a sane magnitude band.
        for (i, &e) in dec.channels[0].exc_q14[..frame_length].iter().enumerate() {
            assert!(e.abs() < (1 << 24), "exc[{i}] magnitude implausible: {e}");
        }
        assert!(
            dec.channels[0].exc_q14[..frame_length]
                .iter()
                .any(|&e| e != 0),
            "excitation is all zero"
        );

        // Byte-for-byte match against the verbatim libopus v1.5.2 gcc oracle
        // (silk_decode_pulses + silk_decode_core) on the same 47-byte frame.
        assert_eq!(
            fnv1a_i32(
                &dec.channels[0].pulses[..frame_length]
                    .iter()
                    .map(|&p| i32::from(p))
                    .collect::<Vec<_>>()
            ),
            4_076_444_569_632_941_751,
            "pulse vector disagreed with the libopus oracle"
        );
        assert_eq!(
            fnv1a_i32(&dec.channels[0].exc_q14[..frame_length]),
            11_411_641_577_209_555_600,
            "excitation disagreed with the libopus oracle"
        );

        // Unvoiced ⇒ no LTP parameters.
        assert_eq!(fe.ltp.pitch_l, [0; MAX_NB_SUBFR]);
        assert_eq!(fe.ltp.scale_q14, 0);
    }

    #[test]
    fn decodes_ltp_and_excitation_of_first_voiced_frame() {
        // The 2nd audio packet of the fixture is a voiced WB 20 ms SILK frame;
        // this exercises the pitch-lag/contour + LTP filter + scaling path that
        // the (unvoiced) first frame does not.
        let bytes = include_bytes!("../../../../testdata/voice-silk-mono.opus");
        let (config, frame) = nth_silk_frame(bytes, 1);
        assert_eq!(config.mode, Mode::SilkOnly);

        let mut rd = RangeDecoder::new(&frame);
        let mut dec = SilkDecoder::new();
        let fe = dec.decode_frontend(&mut rd, config).expect("SILK decode");

        assert_eq!(fe.signal_type, 2, "voiced");
        // WB 20 ms ⇒ 320 samples (see the sibling unvoiced-frame test for why
        // this is a literal rather than `fe.frame_length`).
        let frame_length = 320;

        // Pitch lags + LTP taps + scale from the verbatim libopus oracle
        // (silk_decode_pitch + LTP `Q7 << 7`): lagIndex 68, contour 0,
        // PERIndex 0, LTPIndex [2,0,0,0], scale index 0.
        assert_eq!(fe.ltp.pitch_l, [100, 100, 100, 100]);
        assert_eq!(fe.ltp.coef_q14[0], [1536, 3584, 5248, 1664, -512]);
        assert_eq!(fe.ltp.coef_q14[1], [512, 768, 3072, 896, 640]);
        assert_eq!(fe.ltp.coef_q14[2], [512, 768, 3072, 896, 640]);
        assert_eq!(fe.ltp.coef_q14[3], [512, 768, 3072, 896, 640]);
        assert_eq!(fe.ltp.scale_q14, 15565);

        // Pitch lag is inside the valid [2*fs, 18*fs] range; taps are bounded.
        for &l in &fe.ltp.pitch_l {
            assert!(
                (2 * 16..=18 * 16).contains(&l),
                "pitch lag {l} out of range"
            );
        }
        for tap in fe.ltp.coef_q14.iter().flatten() {
            assert!(tap.unsigned_abs() < (1 << 14), "LTP tap {tap} implausible");
        }

        // Pulse vector + excitation match the libopus oracle byte-for-byte.
        assert_eq!(
            fnv1a_i32(
                &dec.channels[0].pulses[..frame_length]
                    .iter()
                    .map(|&p| i32::from(p))
                    .collect::<Vec<_>>()
            ),
            7_713_367_100_382_987_973,
            "voiced pulse vector disagreed with the libopus oracle"
        );
        assert_eq!(
            fnv1a_i32(&dec.channels[0].exc_q14[..frame_length]),
            10_834_452_417_747_915_079,
            "voiced excitation disagreed with the libopus oracle"
        );
    }
}
