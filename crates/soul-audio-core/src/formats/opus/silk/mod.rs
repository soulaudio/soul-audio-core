//! SILK front-end (RFC 6716 §4.2.7): frame type + gains + NLSF (VQ →
//! stabilization → NLSF→LPC) → per-subframe LPC coefficients.
//!
//! This is the LSF/LPC front-end of the SILK speech codec — a codec entirely
//! separate from CELT, but sharing the already-verified range decoder
//! ([`super::range::RangeDecoder`]). Everything here is transcribed line-for-line
//! from libopus **v1.5.2** fixed-point (`silk/*.c`); the algorithms are integer
//! math with **no** transcendentals (the LSF→cos conversion uses a table, not
//! trig), so this stays `no_std` + no-alloc + `#![forbid(unsafe_code)]` with no
//! `libm` dependency.
//!
//! # Scope (Task 9)
//! Per SILK frame this decodes, in bitstream order: the packet's VAD/LBRR
//! header flags → **frame type + quantization-offset type** → **subframe gains**
//! (log-domain, dequantized to linear Q16) → **NLSF** (stage-1 VQ, stage-2
//! backward-predicted residual, reconstruction, stabilization, NLSF→LPC, and
//! LSF interpolation for the first two subframes). It **stops** after producing
//! per-subframe LPC `a_Q12[]`, leaving the range decoder positioned exactly
//! where the LTP/pitch/excitation indices begin (Task 10). In-band FEC (LBRR)
//! frames are skipped (their data is stepped over to keep the coder aligned).
//!
//! Stereo (Task 13b): [`decoder::SilkDecoder`] holds **two** per-channel states
//! plus the mid/side prediction state in [`stereo`], and decodes a stereo packet
//! (mid + side channels → `silk_stereo_MS_to_LR` → resample each of L/R).
//!
//! # Layout
//! - [`decoder`]: cross-frame [`decoder::SilkDecoder`] state (two
//!   [`decoder::SilkChannel`]s + [`stereo::StereoState`]), the header-flag +
//!   side-info index decode (`silk_decode_indices` subset), gain dequant
//!   (`silk_gains_dequant`/`silk_log2lin`), and the mono **and** stereo
//!   orchestration (`silk_Decode`) producing interleaved 48 kHz PCM.
//! - [`stereo`]: the stereo mid/side predictor decode
//!   (`silk_stereo_decode_pred` + mid-only flag) and the mid/side → left/right
//!   reconstruction (`silk_stereo_MS_to_LR`).
//! - [`nlsf`]: the NLSF vector decoder (`silk_NLSF_decode`/`_unpack`),
//!   stabilizer (`silk_NLSF_stabilize`), NLSF→LPC (`silk_NLSF2A` + `silk_LPC_fit`
//!   + `silk_bwexpander_32` + `silk_LPC_inverse_pred_gain`), and LSF interp.
//! - [`silk_tables`]: the checked-in, generator-emitted static tables (NLSF
//!   codebooks, gain/type/interp iCDFs, LSF cos table). See
//!   `xtask/src/silk_tables.rs`.
//!
//! This front-end feeds the shipping decode path: [`decoder::SilkFrontEnd`]'s
//! output (per-subframe LPC `a_Q12[]`) is consumed by the LTP/excitation decode
//! (Task 10) and LPC synthesis (Task 11) that follow it in [`decoder::SilkDecoder`].
pub(crate) mod decoder;
mod excitation;
mod ltp;
mod nlsf;
mod resampler;
mod silk_tables;
mod stereo;

// ---------------------------------------------------------------------------
// Constants (libopus silk/define.h).
// ---------------------------------------------------------------------------

/// `MAX_LPC_ORDER` — the widest LPC order (WB); NB/MB use 10.
pub(super) const MAX_LPC_ORDER: usize = 16;
/// `MAX_NB_SUBFR` — subframes in a 20 ms SILK frame (10 ms uses 2).
pub(super) const MAX_NB_SUBFR: usize = 4;
/// `SUB_FRAME_LENGTH_MS` — 5 ms per subframe (silk/define.h).
pub(super) const SUB_FRAME_LENGTH_MS: usize = 5;
/// `MAX_SUB_FRAME_LENGTH` = `SUB_FRAME_LENGTH_MS * MAX_FS_KHZ` (16) = 80.
pub(super) const MAX_SUB_FRAME_LENGTH: usize = SUB_FRAME_LENGTH_MS * 16;
/// `LTP_MEM_LENGTH_MS` — length of the LTP re-whitening memory (silk/define.h).
pub(super) const LTP_MEM_LENGTH_MS: usize = 20;
/// `MAX_LTP_MEM_LENGTH` = `LTP_MEM_LENGTH_MS * MAX_FS_KHZ` (16) = 320.
pub(super) const MAX_LTP_MEM_LENGTH: usize = LTP_MEM_LENGTH_MS * 16;
/// `outBuf` capacity (silk/structs.h): `MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH`.
pub(super) const OUT_BUF_LEN: usize = MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH;

/// `TYPE_NO_VOICE_ACTIVITY` / `TYPE_UNVOICED` / `TYPE_VOICED` (silk/define.h).
pub(super) const TYPE_NO_VOICE_ACTIVITY: i32 = 0;
pub(super) const TYPE_VOICED: i32 = 2;

/// `LTP_ORDER` — the 5-tap long-term-prediction filter length (silk/define.h).
pub(super) const LTP_ORDER: usize = 5;
/// `SHELL_CODEC_FRAME_LENGTH` — samples per shell-coded excitation block.
pub(super) const SHELL_CODEC_FRAME_LENGTH: usize = 16;
/// `LOG2_SHELL_CODEC_FRAME_LENGTH` — `log2(SHELL_CODEC_FRAME_LENGTH)`.
pub(super) const LOG2_SHELL_CODEC_FRAME_LENGTH: u32 = 4;
/// `MAX_NB_SHELL_BLOCKS` = `MAX_FRAME_LENGTH / SHELL_CODEC_FRAME_LENGTH` = 320/16.
pub(super) const MAX_NB_SHELL_BLOCKS: usize = 20;
/// `MAX_FRAME_LENGTH` — widest SILK internal frame (20 ms @ 16 kHz = 320 samples).
pub(super) const MAX_FRAME_LENGTH: usize = 320;
/// `SILK_MAX_PULSES` — the pulse-count value `+1` of which signals an LSB escape.
pub(super) const SILK_MAX_PULSES: i32 = 16;
/// `N_RATE_LEVELS` — number of pulse-count-iCDF rate levels (the last is the
/// escape / LSB-count table).
pub(super) const N_RATE_LEVELS: usize = 10;
/// `QUANT_LEVEL_ADJUST_Q10` — excitation dead-zone adjustment (silk/define.h).
pub(super) const QUANT_LEVEL_ADJUST_Q10: i32 = 80;

/// `RAND_MULTIPLIER` / `RAND_INCREMENT` — the SILK excitation LCG constants
/// (silk/SigProc_FIX.h `silk_RAND`). Intentionally modular over `u32`.
pub(super) const RAND_MULTIPLIER: u32 = 196_314_165;
pub(super) const RAND_INCREMENT: u32 = 907_633_515;

/// `silk_RAND(seed)` = `silk_MLA_ovflw(RAND_INCREMENT, seed, RAND_MULTIPLIER)`
/// = `RAND_INCREMENT + seed*RAND_MULTIPLIER` (modular `u32`, kept signed).
#[inline]
pub(super) fn silk_rand(seed: i32) -> i32 {
    (RAND_INCREMENT.wrapping_add((seed as u32).wrapping_mul(RAND_MULTIPLIER))) as i32
}

/// `NLSF_QUANT_MAX_AMPLITUDE` — stage-2 residual amplitude clamp (silk/define.h).
pub(super) const NLSF_QUANT_MAX_AMPLITUDE: i32 = 4;
/// `SILK_FIX_CONST( NLSF_QUANT_LEVEL_ADJ, 10 )` with `NLSF_QUANT_LEVEL_ADJ=0.1`.
pub(super) const NLSF_QUANT_LEVEL_ADJ_Q10: i32 = fix_const(0.1, 10);

/// Gain-quantizer geometry (silk/define.h + silk/gain_quant.c macros).
const MIN_QGAIN_DB: i32 = 2;
const MAX_QGAIN_DB: i32 = 88;
pub(super) const N_LEVELS_QGAIN: i32 = 64;
pub(super) const MIN_DELTA_GAIN_QUANT: i32 = -4;
pub(super) const MAX_DELTA_GAIN_QUANT: i32 = 36;
/// `OFFSET = (MIN_QGAIN_DB*128)/6 + 16*128` (silk/gain_quant.c).
pub(super) const GAIN_OFFSET_Q7: i32 = (MIN_QGAIN_DB * 128) / 6 + 16 * 128;
/// `INV_SCALE_Q16 = 65536*(((MAX_QGAIN_DB-MIN_QGAIN_DB)*128)/6)/(N_LEVELS_QGAIN-1)`.
pub(super) const GAIN_INV_SCALE_Q16: i32 =
    (65536 * (((MAX_QGAIN_DB - MIN_QGAIN_DB) * 128) / 6)) / (N_LEVELS_QGAIN - 1);

/// `MAX_LPC_STABILIZE_ITERATIONS` (silk/define.h).
pub(super) const MAX_LPC_STABILIZE_ITERATIONS: i32 = 16;
/// `SILK_FIX_CONST( 1.0 / MAX_PREDICTION_POWER_GAIN, 30 )`, `MAX_PREDICTION_POWER_GAIN=1e4`.
pub(super) const MIN_INV_GAIN_Q30: i32 = fix_const(1.0 / 1e4, 30);

/// `SILK_FIX_CONST( C, Q )` = round-to-nearest of `C * 2^Q` (silk/SigProc_FIX.h).
/// A `const fn` so every fixed-point constant is computed at compile time from
/// the same float literal libopus uses.
pub(super) const fn fix_const(c: f64, q: u32) -> i32 {
    (c * (1i64 << q) as f64 + 0.5) as i32
}

// ---------------------------------------------------------------------------
// Fixed-point helpers (libopus silk/macros.h + silk/SigProc_FIX.h + Inlines.h).
//
// Transcribed to match the C integer semantics exactly, including where libopus
// relies on modular (wrapping) 32-bit arithmetic. `i64` intermediates reproduce
// the widening-multiply macros; arithmetic right shifts on `i32`/`i64` match C's
// signed `>>`.
// ---------------------------------------------------------------------------

/// `silk_LSHIFT32(a, s)` — `((opus_uint32)a << s)` cast back to signed, so a
/// negative operand shifts its bit pattern (no overflow panic).
#[inline]
pub(super) fn lshift(a: i32, s: u32) -> i32 {
    ((a as u32) << s) as i32
}

/// `silk_RSHIFT(a, s)` — arithmetic right shift (matches C signed `>>`).
#[inline]
pub(super) fn rshift(a: i32, s: u32) -> i32 {
    a >> s
}

/// `silk_SMULWB(a, b)` = `(a * (int64)(int16)b) >> 16`.
#[inline]
pub(super) fn smulwb(a: i32, b: i32) -> i32 {
    ((a as i64 * (b as i16 as i64)) >> 16) as i32
}

/// `silk_SMULWW(a, b)` = `((int64)a * b) >> 16`.
#[inline]
pub(super) fn smulww(a: i32, b: i32) -> i32 {
    ((a as i64 * b as i64) >> 16) as i32
}

/// `silk_SMLAWB(a, b, c)` = `a + ((b * (int64)(int16)c) >> 16)` (wrapping add).
#[inline]
pub(super) fn smlawb(a: i32, b: i32, c: i32) -> i32 {
    a.wrapping_add(((b as i64 * (c as i16 as i64)) >> 16) as i32)
}

/// `silk_SMULBB(a, b)` = `(int32)(int16)a * (int32)(int16)b` (wrapping mul).
#[inline]
pub(super) fn smulbb(a: i32, b: i32) -> i32 {
    (a as i16 as i32).wrapping_mul(b as i16 as i32)
}

/// `silk_MLA(a, b, c)` = `a + b*c` (wrapping, C `int` semantics).
#[inline]
pub(super) fn mla(a: i32, b: i32, c: i32) -> i32 {
    a.wrapping_add(b.wrapping_mul(c))
}

/// `silk_SMULL(a, b)` = `(int64)a * b`.
#[inline]
pub(super) fn smull(a: i32, b: i32) -> i64 {
    a as i64 * b as i64
}

/// `silk_SMMUL(a, b)` = `((int64)a * b) >> 32`.
#[inline]
pub(super) fn smmul(a: i32, b: i32) -> i32 {
    ((a as i64 * b as i64) >> 32) as i32
}

/// `silk_MUL(a, b)` = `a * b` (wrapping, C `int` semantics).
#[inline]
pub(super) fn mul(a: i32, b: i32) -> i32 {
    a.wrapping_mul(b)
}

/// `silk_ADD_RSHIFT32(a, b, s)` = `a + (b >> s)`.
#[inline]
pub(super) fn add_rshift32(a: i32, b: i32, s: u32) -> i32 {
    a.wrapping_add(b >> s)
}

/// `silk_RSHIFT_ROUND(a, s)` (silk/SigProc_FIX.h). `s >= 1`.
#[inline]
pub(super) fn rshift_round(a: i32, s: u32) -> i32 {
    if s == 1 {
        (a >> 1) + (a & 1)
    } else {
        ((a >> (s - 1)) + 1) >> 1
    }
}

/// `silk_RSHIFT_ROUND64(a, s)` (silk/SigProc_FIX.h). `s >= 1`.
#[inline]
pub(super) fn rshift_round64(a: i64, s: u32) -> i64 {
    if s == 1 {
        (a >> 1) + (a & 1)
    } else {
        ((a >> (s - 1)) + 1) >> 1
    }
}

/// `silk_DIV32_16(a, b)` = `a / (int16)b` (truncating toward zero, as C).
#[inline]
pub(super) fn div32_16(a: i32, b: i32) -> i32 {
    a / (b as i16 as i32)
}

/// `silk_DIV32(a, b)` = `a / b` (truncating toward zero, as C).
#[inline]
pub(super) fn div32(a: i32, b: i32) -> i32 {
    a / b
}

/// `silk_LIMIT( a, l1, l2 )` (silk/SigProc_FIX.h) — the exact asymmetric form
/// (handles `l1 > l2` the way libopus does), used by both `silk_LIMIT_int` and
/// `silk_LIMIT_32`.
#[inline]
pub(super) fn limit(a: i32, l1: i32, l2: i32) -> i32 {
    if l1 > l2 {
        if a > l1 {
            l1
        } else if a < l2 {
            l2
        } else {
            a
        }
    } else if a > l2 {
        l2
    } else if a < l1 {
        l1
    } else {
        a
    }
}

/// `silk_SAT16(a)` — clamp to `int16` range (result kept as `i32`).
#[inline]
pub(super) fn sat16(a: i32) -> i32 {
    a.clamp(i16::MIN as i32, i16::MAX as i32)
}

/// `silk_ADD_SAT16(a, b)` = `(int16)silk_SAT16(a + b)` (saturating add in i16).
#[inline]
pub(super) fn add_sat16(a: i32, b: i32) -> i32 {
    sat16(a.wrapping_add(b))
}

/// `silk_CLZ32(x)` — count leading zeros (`32` for `x == 0`, as the builtin).
#[inline]
pub(super) fn clz32(x: i32) -> i32 {
    (x as u32).leading_zeros() as i32
}

/// `silk_abs(a)` — note libopus's caveat that `silk_intXX_MIN` misbehaves; not
/// reached with those inputs here.
#[inline]
pub(super) fn abs(a: i32) -> i32 {
    if a > 0 { a } else { -a }
}

/// `silk_LSHIFT_SAT32(a, s)` = `LSHIFT32( LIMIT(a, INT32_MIN>>s, INT32_MAX>>s), s )`.
#[inline]
pub(super) fn lshift_sat32(a: i32, s: u32) -> i32 {
    lshift(limit(a, i32::MIN >> s, i32::MAX >> s), s)
}

/// `silk_INVERSE32_varQ( b32, Qres )` (silk/Inlines.h) — integer reciprocal to
/// `1 << Qres` precision. Only reached with `b32 > 0` and `Qres > 0`.
pub(super) fn inverse32_var_q(b32: i32, qres: i32) -> i32 {
    debug_assert!(b32 != 0 && qres > 0);
    let b_headrm = clz32(abs(b32)) - 1;
    let b32_nrm = lshift(b32, b_headrm as u32);
    let b32_inv = div32_16(i32::MAX >> 2, rshift(b32_nrm, 16));
    let mut result = lshift(b32_inv, 16);
    let err_q32 = lshift((1i32 << 29) - smulwb(b32_nrm, b32_inv), 3);
    // silk_SMLAWW(result, err_Q32, b32_inv)
    result = result.wrapping_add(((err_q32 as i64 * b32_inv as i64) >> 16) as i32);
    let lshift_amt = 61 - b_headrm - qres;
    if lshift_amt <= 0 {
        lshift_sat32(result, (-lshift_amt) as u32)
    } else if lshift_amt < 32 {
        rshift(result, lshift_amt as u32)
    } else {
        0
    }
}

/// `silk_ADD32`/`silk_ADD32_ovflw` — modular (wrapping) 32-bit add. libopus's
/// plain `silk_ADD32` can overflow on invalid streams; wrapping matches C and
/// never panics in debug.
#[inline]
pub(super) fn add32(a: i32, b: i32) -> i32 {
    a.wrapping_add(b)
}

/// `silk_SUB32`/`silk_SUB32_ovflw` — modular (wrapping) 32-bit subtract.
#[inline]
pub(super) fn sub32(a: i32, b: i32) -> i32 {
    a.wrapping_sub(b)
}

/// `silk_ADD_LSHIFT32(a, b, s)` = `a + (b << s)` (wrapping).
#[inline]
pub(super) fn add_lshift32(a: i32, b: i32, s: u32) -> i32 {
    a.wrapping_add(lshift(b, s))
}

/// `silk_ADD_SAT32(a, b)` — saturating 32-bit add.
#[inline]
pub(super) fn add_sat32(a: i32, b: i32) -> i32 {
    a.saturating_add(b)
}

/// `silk_SMLABB(a, b, c)` = `a + (int16)b * (int16)c` (wrapping, C `int`
/// semantics — matches `silk_SMLABB_ovflw`).
#[inline]
pub(super) fn smlabb(a: i32, b: i32, c: i32) -> i32 {
    a.wrapping_add((b as i16 as i32).wrapping_mul(c as i16 as i32))
}

/// `silk_DIV32_varQ(a32, b32, Qres)` (silk/Inlines.h) — a good approximation of
/// `(a32 << Qres) / b32`. Only reached with `b32 > 0` and `Qres >= 0`.
pub(super) fn div32_var_q(a32: i32, b32: i32, qres: i32) -> i32 {
    debug_assert!(b32 != 0 && qres >= 0);
    let a_headrm = clz32(abs(a32)) - 1;
    let a32_nrm = lshift(a32, a_headrm as u32);
    let b_headrm = clz32(abs(b32)) - 1;
    let b32_nrm = lshift(b32, b_headrm as u32);
    let b32_inv = div32_16(i32::MAX >> 2, rshift(b32_nrm, 16));
    let mut result = smulwb(a32_nrm, b32_inv);
    // a32_nrm = silk_SUB32_ovflw( a32_nrm, silk_LSHIFT_ovflw( silk_SMMUL(b32_nrm, result), 3 ) )
    let a32_nrm = sub32(a32_nrm, lshift(smmul(b32_nrm, result), 3));
    result = smlawb(result, a32_nrm, b32_inv);
    let lshift_amt = 29 + a_headrm - b_headrm - qres;
    if lshift_amt < 0 {
        lshift_sat32(result, (-lshift_amt) as u32)
    } else if lshift_amt < 32 {
        rshift(result, lshift_amt as u32)
    } else {
        0
    }
}
