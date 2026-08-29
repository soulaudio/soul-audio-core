//! Parameter Huffman decode + delta accumulation + clip/wrap + dequant +
//! resolution-expand (`ps-reference.md` §3/§4): IID, ICC, IPD, OPD.
//!
//! Per parameter type, per envelope: read `dt` (1 bit), Huffman-decode
//! `nr_par` signed deltas (V1.a binary tree), accumulate against either the
//! previous band (`dt==0`, delta-freq) or the previous envelope / previous
//! frame's `*_index_prev` (`dt==1`, delta-time), then clip (IID) or wrap
//! mod 8 (IPD/OPD) or clip (ICC) so the result always safely indexes the V2
//! dequant tables `[NOPANIC]`. [`decode_param`] does exactly that step,
//! staying at the *transmitted* resolution (10/20/34 for IID/ICC, 5/11/17
//! for IPD/OPD, §2).
//!
//! [`read_iid`]/[`read_icc`]/[`read_ipd`]/[`read_opd`] wrap `decode_param`
//! with the §4 resolution-expand (still index-domain -- faad2
//! `map20indexto34`/FFmpeg `map_idx_*`/`remap34`: expanding *before*
//! dequant, since the dequant tables are non-linear and expanding
//! post-dequant floats would not reproduce the reference decoders' output)
//! and the §3 dequant tables, writing directly into [`PsParams`]'s
//! per-envelope, per-WORKING-band float arrays that [`super::data::ps_data`]
//! fills and Task 8's mixing consumes.

use super::MAX_PS_ENVELOPES;
use super::data::clamped_read;
use super::frame::PsFrameLayout;
use super::tables::{
    COS_ALPHAS, F_HUFF_ICC, F_HUFF_IID_DEF, F_HUFF_IID_FINE, F_HUFF_IPD, F_HUFF_OPD, IPDOPD_COS,
    IPDOPD_SIN, SF_IID_FINE, SF_IID_NORMAL, SIN_ALPHAS, T_HUFF_ICC, T_HUFF_IID_DEF,
    T_HUFF_IID_FINE, T_HUFF_IPD, T_HUFF_OPD,
};
use crate::decoder::DecodeError;
use crate::formats::aac::bits::BitReader;

/// IID/ICC parameter bands at the widest (`is34`) WORKING resolution (§2/§4)
/// -- also the fixed backing width of `decode_param`'s `raw` buffer,
/// regardless of parameter type (IPD/OPD only ever use the low
/// `MAX_IPDOPD_BANDS` of it).
const MAX_IIDICC_BANDS: usize = 34;
/// IPD/OPD parameter bands top out at `nr_iidopd_par_tab`'s largest entry
/// (§2: `17`, tracking the 34-band IID mode).
const MAX_IPDOPD_BANDS: usize = 17;

/// One frame's dequantized PS parameters, per envelope, at the WORKING
/// resolution (`ps-reference.md` §3/§4): `iid_c`/`icc_cos`/`icc_sin` use
/// indices `0..34` (`0..20` when `!is34`); `ipd_*`/`opd_*` use `0..17`
/// (`0..11` when `!is34`) -- Task 8 indexes these directly via
/// `bands::k_to_i`, no further resolution bookkeeping needed downstream.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PsParams {
    /// IID channel-level ratio `c = sf_iid[no_iid_steps + q]` (§3).
    pub(crate) iid_c: [[f32; MAX_IIDICC_BANDS]; MAX_PS_ENVELOPES],
    /// ICC mixing-angle cosine `cos_alphas[q]` (§3).
    pub(crate) icc_cos: [[f32; MAX_IIDICC_BANDS]; MAX_PS_ENVELOPES],
    /// ICC mixing-angle sine `sin_alphas[q]` (§3).
    pub(crate) icc_sin: [[f32; MAX_IIDICC_BANDS]; MAX_PS_ENVELOPES],
    /// IPD phase cosine `ipdopd_cos[q]` (§3), lowest `nr_ipdopd_par` bands.
    pub(crate) ipd_cos: [[f32; MAX_IPDOPD_BANDS]; MAX_PS_ENVELOPES],
    /// IPD phase sine `ipdopd_sin[q]` (§3).
    pub(crate) ipd_sin: [[f32; MAX_IPDOPD_BANDS]; MAX_PS_ENVELOPES],
    /// OPD phase cosine `ipdopd_cos[q]` (§3).
    pub(crate) opd_cos: [[f32; MAX_IPDOPD_BANDS]; MAX_PS_ENVELOPES],
    /// OPD phase sine `ipdopd_sin[q]` (§3).
    pub(crate) opd_sin: [[f32; MAX_IPDOPD_BANDS]; MAX_PS_ENVELOPES],
    /// Set once this frame's `enable_ext` extension actually carried IPD/OPD
    /// symbols (§1 `ps_extension()`'s own `enable_ipdopd` bit) -- Task 8's
    /// gate for whether to apply the IPD/OPD phase rotation at all.
    pub(crate) enable_ipdopd: bool,
}

impl Default for PsParams {
    fn default() -> Self {
        // `#[derive(Default)]` doesn't reach array widths > 32 (no blanket
        // `[T; N]: Default` for arbitrary const N in this toolchain) --
        // manual impl, all-zero/false, matching a frame whose parameter
        // body hasn't been parsed yet.
        Self {
            iid_c: [[0.0; MAX_IIDICC_BANDS]; MAX_PS_ENVELOPES],
            icc_cos: [[0.0; MAX_IIDICC_BANDS]; MAX_PS_ENVELOPES],
            icc_sin: [[0.0; MAX_IIDICC_BANDS]; MAX_PS_ENVELOPES],
            ipd_cos: [[0.0; MAX_IPDOPD_BANDS]; MAX_PS_ENVELOPES],
            ipd_sin: [[0.0; MAX_IPDOPD_BANDS]; MAX_PS_ENVELOPES],
            opd_cos: [[0.0; MAX_IPDOPD_BANDS]; MAX_PS_ENVELOPES],
            opd_sin: [[0.0; MAX_IPDOPD_BANDS]; MAX_PS_ENVELOPES],
            enable_ipdopd: false,
        }
    }
}

/// Selects the Huffman table pair + clip/wrap policy + dequant table (§3).
#[derive(Clone, Copy)]
pub(crate) enum ParamKind {
    /// 7-bit-symmetric (`±7`) IID, `iid_mode < 3`.
    IidCoarse,
    /// 15-bit-symmetric (`±15`, ½-dB steps) IID, `iid_mode >= 3`.
    IidFine,
    /// `[0,7]`-clipped ICC.
    Icc,
    /// mod-8-wrapped IPD.
    Ipd,
    /// mod-8-wrapped OPD.
    Opd,
}

impl ParamKind {
    /// `(delta-freq table, delta-time table)` (V1.a).
    fn huff_tabs(self) -> (&'static [[i8; 2]], &'static [[i8; 2]]) {
        match self {
            ParamKind::IidCoarse => (&F_HUFF_IID_DEF[..], &T_HUFF_IID_DEF[..]),
            ParamKind::IidFine => (&F_HUFF_IID_FINE[..], &T_HUFF_IID_FINE[..]),
            ParamKind::Icc => (&F_HUFF_ICC[..], &T_HUFF_ICC[..]),
            ParamKind::Ipd => (&F_HUFF_IPD[..], &T_HUFF_IPD[..]),
            ParamKind::Opd => (&F_HUFF_OPD[..], &T_HUFF_OPD[..]),
        }
    }

    /// Post-accumulation bound (§3 `[NOPANIC]`): IID clips `±7`/`±15`, ICC
    /// clips `[0,7]`, IPD/OPD wrap mod 8 into `[0,7]` (phase is cyclic).
    /// This is also the array-index guard for the §3 dequant tables below
    /// -- a malformed delta stream can overshoot arbitrarily far, but the
    /// result is always brought back into the table's valid range here.
    fn clip_or_wrap(self, v: i32) -> i8 {
        match self {
            ParamKind::IidCoarse => v.clamp(-7, 7) as i8,
            ParamKind::IidFine => v.clamp(-15, 15) as i8,
            ParamKind::Icc => v.clamp(0, 7) as i8,
            ParamKind::Ipd | ParamKind::Opd => v.rem_euclid(8) as i8,
        }
    }
}

/// Walk a V1.a binary tree: `i=0; while i>=0 { i = t[i][bit] }; return
/// i + 31`. Bounded two ways `[NOPANIC]`: a bit read past `end_bit` errors
/// (via `clamped_read`), and the walk cannot take more steps than the table
/// has rows (guards a hypothetically malformed/cyclic table -- the real V1
/// trees are well-formed and always terminate well within this bound).
pub(crate) fn ps_huff_dec(
    bits: &mut BitReader,
    end_bit: usize,
    t: &[[i8; 2]],
) -> Result<i32, DecodeError> {
    let bound = t.len();
    let mut index: i32 = 0;
    let mut steps = 0usize;
    while index >= 0 {
        if steps > bound {
            // A well-formed tree always reaches a leaf in at most `bound`
            // steps; more than that means a cyclic/malformed table.
            return Err(DecodeError::Malformed);
        }
        steps += 1;
        let node = index as usize;
        if node >= bound {
            return Err(DecodeError::Malformed);
        }
        let bit = clamped_read(bits, end_bit, 1).ok_or(DecodeError::Malformed)? as usize;
        index = t[node][bit] as i32;
    }
    Ok(index + 31)
}

/// Decode one parameter type across all `num_env` envelopes (§3): read `dt`
/// per envelope, Huffman-decode `nr_par` deltas, accumulate (delta-freq vs
/// delta-time against `index_prev`/the previous envelope), clip/wrap
/// `[NOPANIC]`. Fills `raw[env][0..nr_par]` (post clip/wrap indices, still
/// at the *transmitted* resolution) and updates `index_prev[0..nr_par]` to
/// the last envelope's indices (the persistent delta-time base for the next
/// frame's envelope 0).
pub(crate) fn decode_param(
    bits: &mut BitReader,
    end_bit: usize,
    kind: ParamKind,
    num_env: u8,
    nr_par: u8,
    index_prev: &mut [i8],
    raw: &mut [[i8; MAX_IIDICC_BANDS]; MAX_PS_ENVELOPES],
) -> Result<(), DecodeError> {
    let nr = nr_par as usize;
    let envs = num_env as usize;
    if nr > MAX_IIDICC_BANDS || envs > MAX_PS_ENVELOPES || index_prev.len() < nr {
        return Err(DecodeError::Malformed);
    }
    let (f_huff, t_huff) = kind.huff_tabs();
    for env in 0..envs {
        let dt = clamped_read(bits, end_bit, 1).ok_or(DecodeError::Malformed)? == 1;
        if dt {
            // delta-time: par[b] = par_prev[b] + delta[b], where par_prev is
            // the same band in the previous envelope, or (env==0) the
            // persistent `index_prev` from the previous frame's last
            // envelope (§3).
            for b in 0..nr {
                let delta = ps_huff_dec(bits, end_bit, t_huff)?;
                let base = if env == 0 {
                    index_prev[b] as i32
                } else {
                    raw[env - 1][b] as i32
                };
                raw[env][b] = kind.clip_or_wrap(base + delta);
            }
        } else {
            // delta-freq: par[b] = par[b-1] + delta[b], par[-1] = 0. Each
            // band's base is the PREVIOUS band's already clip/wrapped value
            // (matching faad2's in-place `index[i] = index[i-1] + index[i]`
            // where `index[i-1]` was clipped on the prior iteration).
            let mut prev = 0i32;
            for slot in raw[env][..nr].iter_mut() {
                let delta = ps_huff_dec(bits, end_bit, f_huff)?;
                let v = kind.clip_or_wrap(prev + delta);
                *slot = v;
                prev = v as i32;
            }
        }
    }
    if envs > 0 {
        index_prev[..nr].copy_from_slice(&raw[envs - 1][..nr]);
    }
    Ok(())
}

/// `map_idx_10_to_20` (§4; FFmpeg `aacps.c`): duplicate each of the low
/// `nr_par` transmitted bands into 2 working bands. `full=true` is the
/// IID/ICC form (10 -> 20, all of `dst[0..20]`); `full=false` is the
/// IPD/OPD form (5 -> 11, `dst[0..10]` duplicated, `dst[10]` forced 0 since
/// there is no 11th source band).
fn map_idx_10_to_20(dst: &mut [i8; MAX_IIDICC_BANDS], src: &[i8; MAX_IIDICC_BANDS], full: bool) {
    let b_max = if full {
        9
    } else {
        dst[10] = 0;
        4
    };
    for b in 0..=b_max {
        dst[2 * b] = src[b];
        dst[2 * b + 1] = src[b];
    }
}

/// `map_idx_20_to_34` (§4; FFmpeg `aacps.c`): the fixed 20->34 index map,
/// duplicating some bands and averaging adjacent pairs of others.
/// `full=true` is the IID/ICC form (20 -> 34, all of `dst[0..34]`);
/// `full=false` is the IPD/OPD form (11 -> 17, only `dst[0..17]`, reading
/// only `src[0..11]`).
fn map_idx_20_to_34(dst: &mut [i8; MAX_IIDICC_BANDS], src: &[i8; MAX_IIDICC_BANDS], full: bool) {
    if full {
        dst[33] = src[19];
        dst[32] = src[19];
        dst[31] = src[18];
        dst[30] = src[18];
        dst[29] = src[18];
        dst[28] = src[18];
        dst[27] = src[17];
        dst[26] = src[17];
        dst[25] = src[16];
        dst[24] = src[16];
        dst[23] = src[15];
        dst[22] = src[15];
        dst[21] = src[14];
        dst[20] = src[14];
        dst[19] = src[13];
        dst[18] = src[12];
        dst[17] = src[11];
    }
    dst[16] = src[10];
    dst[15] = src[9];
    dst[14] = src[9];
    dst[13] = src[8];
    dst[12] = src[8];
    dst[11] = src[7];
    dst[10] = src[6];
    dst[9] = src[5];
    dst[8] = src[5];
    dst[7] = src[4];
    dst[6] = src[4];
    dst[5] = src[3];
    dst[4] = (src[2] + src[3]) / 2;
    dst[3] = src[2];
    dst[2] = src[1];
    dst[1] = (src[0] + src[1]) / 2;
    dst[0] = src[0];
}

/// `map_idx_10_to_34` (§4; FFmpeg `aacps.c`): the fixed 10->34 index map.
/// `full=true` is the IID/ICC form (10 -> 34, all of `dst[0..34]`);
/// `full=false` is the IPD/OPD form (5 -> 17, `dst[0..16]` from
/// `src[0..5]`, `dst[16]` forced 0).
fn map_idx_10_to_34(dst: &mut [i8; MAX_IIDICC_BANDS], src: &[i8; MAX_IIDICC_BANDS], full: bool) {
    if full {
        dst[33] = src[9];
        dst[32] = src[9];
        dst[31] = src[9];
        dst[30] = src[9];
        dst[29] = src[9];
        dst[28] = src[9];
        dst[27] = src[8];
        dst[26] = src[8];
        dst[25] = src[8];
        dst[24] = src[8];
        dst[23] = src[7];
        dst[22] = src[7];
        dst[21] = src[7];
        dst[20] = src[7];
        dst[19] = src[6];
        dst[18] = src[6];
        dst[17] = src[5];
        dst[16] = src[5];
    } else {
        dst[16] = 0;
    }
    dst[15] = src[4];
    dst[14] = src[4];
    dst[13] = src[4];
    dst[12] = src[4];
    dst[11] = src[3];
    dst[10] = src[3];
    dst[9] = src[2];
    dst[8] = src[2];
    dst[7] = src[2];
    dst[6] = src[2];
    dst[5] = src[1];
    dst[4] = src[1];
    dst[3] = src[1];
    dst[2] = src[0];
    dst[1] = src[0];
    dst[0] = src[0];
}

/// Resolution-expand `src[0..nr_par]` (transmitted resolution) into
/// `dst[0..target]` (working resolution), §4. `nr_par == target` is a
/// direct copy (already at the working resolution); the six upward
/// transmitted/target pairs V6 ever produces (10/20, 10/34, 20/34 for
/// IID/ICC's `full=true`; 5/11, 5/17, 11/17 for IPD/OPD's `full=false`) go
/// through the matching `map_idx_*` above. `[NOPANIC]`: any other pairing
/// (unreachable given `frame::read_frame`'s V6-sourced band counts) falls
/// back to an identity copy over the smaller of the two spans rather than
/// leaving `dst` uninitialized.
fn expand_par(
    nr_par: u8,
    target: u8,
    full: bool,
    src: &[i8; MAX_IIDICC_BANDS],
    dst: &mut [i8; MAX_IIDICC_BANDS],
) {
    if nr_par == target {
        dst[..target as usize].copy_from_slice(&src[..target as usize]);
        return;
    }
    match (nr_par, target) {
        (10, 20) | (5, 11) => map_idx_10_to_20(dst, src, full),
        (10, 34) | (5, 17) => map_idx_10_to_34(dst, src, full),
        (20, 34) | (11, 17) => map_idx_20_to_34(dst, src, full),
        _ => {
            let n = nr_par.min(target) as usize;
            dst[..n].copy_from_slice(&src[..n]);
        }
    }
}

/// IID (§3 main body): Huffman-delta decode at the transmitted resolution,
/// resolution-expand to the working resolution (34 if `is34` else 20, §4),
/// dequantize (`c = sf_iid[no_iid_steps + q]`) into `params.iid_c`.
pub(super) fn read_iid(
    bits: &mut BitReader,
    end_bit: usize,
    frame: &PsFrameLayout,
    index_prev: &mut [i8],
    params: &mut PsParams,
) -> Result<(), DecodeError> {
    let fine = frame.iid_quant;
    let kind = if fine {
        ParamKind::IidFine
    } else {
        ParamKind::IidCoarse
    };
    let nr_par = frame.nr_iid_par;
    let target: u8 = if frame.is34 { 34 } else { 20 };
    let mut raw = [[0i8; MAX_IIDICC_BANDS]; MAX_PS_ENVELOPES];
    decode_param(
        bits,
        end_bit,
        kind,
        frame.num_env,
        nr_par,
        index_prev,
        &mut raw,
    )?;
    let no_iid_steps: i32 = if fine { 15 } else { 7 };
    let sf: &[f32] = if fine { &SF_IID_FINE } else { &SF_IID_NORMAL };
    let mut expanded = [0i8; MAX_IIDICC_BANDS];
    for (env, raw_env) in raw.iter().enumerate().take(frame.num_env as usize) {
        expand_par(nr_par, target, true, raw_env, &mut expanded);
        for (b, &q) in expanded.iter().enumerate().take(target as usize) {
            let idx = (no_iid_steps + q as i32) as usize;
            params.iid_c[env][b] = sf[idx];
        }
    }
    Ok(())
}

/// IID disabled this frame (no `enable_iid` header bit): reset to the
/// neutral dequantized value (index 0 -> unity gain, no panning) across all
/// working bands/envelopes, mirroring FFmpeg's `memset(iid_par, 0, ...)` --
/// a frame that turns IID off must not keep showing the previous frame's
/// panning.
pub(super) fn reset_iid(index_prev: &mut [i8], frame: &PsFrameLayout, params: &mut PsParams) {
    let target = if frame.is34 { 34 } else { 20 };
    for e in 0..frame.num_env as usize {
        params.iid_c[e][..target].fill(SF_IID_NORMAL[7]);
    }
    index_prev.fill(0);
}

/// ICC (§3 main body): Huffman-delta decode + resolution-expand + dequant
/// (`(cos, sin) = (cos_alphas[q], sin_alphas[q])`) into `params.icc_*`.
pub(super) fn read_icc(
    bits: &mut BitReader,
    end_bit: usize,
    frame: &PsFrameLayout,
    index_prev: &mut [i8],
    params: &mut PsParams,
) -> Result<(), DecodeError> {
    let nr_par = frame.nr_icc_par;
    let target: u8 = if frame.is34 { 34 } else { 20 };
    let mut raw = [[0i8; MAX_IIDICC_BANDS]; MAX_PS_ENVELOPES];
    decode_param(
        bits,
        end_bit,
        ParamKind::Icc,
        frame.num_env,
        nr_par,
        index_prev,
        &mut raw,
    )?;
    let mut expanded = [0i8; MAX_IIDICC_BANDS];
    for (env, raw_env) in raw.iter().enumerate().take(frame.num_env as usize) {
        expand_par(nr_par, target, true, raw_env, &mut expanded);
        for (b, &q) in expanded.iter().enumerate().take(target as usize) {
            let idx = q as usize;
            params.icc_cos[env][b] = COS_ALPHAS[idx];
            params.icc_sin[env][b] = SIN_ALPHAS[idx];
        }
    }
    Ok(())
}

/// ICC disabled this frame: reset to the neutral dequantized value (index 0
/// -> fully coherent, `alpha=0`) -- see [`reset_iid`].
pub(super) fn reset_icc(index_prev: &mut [i8], frame: &PsFrameLayout, params: &mut PsParams) {
    let target = if frame.is34 { 34 } else { 20 };
    for e in 0..frame.num_env as usize {
        params.icc_cos[e][..target].fill(COS_ALPHAS[0]);
        params.icc_sin[e][..target].fill(SIN_ALPHAS[0]);
    }
    index_prev.fill(0);
}

/// Shared IPD/OPD decode+expand+dequant body (§3 `ps_extension()`): `kind`
/// is `Ipd` or `Opd`; `out_cos`/`out_sin` are the matching `params` fields.
fn decode_ipdopd(
    bits: &mut BitReader,
    end_bit: usize,
    kind: ParamKind,
    frame: &PsFrameLayout,
    index_prev: &mut [i8],
    out_cos: &mut [[f32; MAX_IPDOPD_BANDS]; MAX_PS_ENVELOPES],
    out_sin: &mut [[f32; MAX_IPDOPD_BANDS]; MAX_PS_ENVELOPES],
) -> Result<(), DecodeError> {
    let nr_par = frame.nr_ipdopd_par;
    let target: u8 = if frame.is34 { 17 } else { 11 };
    let mut raw = [[0i8; MAX_IIDICC_BANDS]; MAX_PS_ENVELOPES];
    decode_param(
        bits,
        end_bit,
        kind,
        frame.num_env,
        nr_par,
        index_prev,
        &mut raw,
    )?;
    let mut expanded = [0i8; MAX_IIDICC_BANDS];
    for (env, raw_env) in raw.iter().enumerate().take(frame.num_env as usize) {
        expand_par(nr_par, target, false, raw_env, &mut expanded);
        for (b, &q) in expanded.iter().enumerate().take(target as usize) {
            let idx = q as usize;
            out_cos[env][b] = IPDOPD_COS[idx];
            out_sin[env][b] = IPDOPD_SIN[idx];
        }
    }
    Ok(())
}

/// Neutral (zero phase difference) reset for IPD or OPD -- see
/// [`reset_iid`].
fn reset_ipdopd(
    index_prev: &mut [i8],
    frame: &PsFrameLayout,
    out_cos: &mut [[f32; MAX_IPDOPD_BANDS]; MAX_PS_ENVELOPES],
    out_sin: &mut [[f32; MAX_IPDOPD_BANDS]; MAX_PS_ENVELOPES],
) {
    let target = if frame.is34 { 17 } else { 11 };
    for e in 0..frame.num_env as usize {
        out_cos[e][..target].fill(IPDOPD_COS[0]);
        out_sin[e][..target].fill(IPDOPD_SIN[0]);
    }
    index_prev.fill(0);
}

/// IPD (§3, inside `enable_ext`'s `ps_extension()`).
pub(super) fn read_ipd(
    bits: &mut BitReader,
    end_bit: usize,
    frame: &PsFrameLayout,
    index_prev: &mut [i8],
    params: &mut PsParams,
) -> Result<(), DecodeError> {
    decode_ipdopd(
        bits,
        end_bit,
        ParamKind::Ipd,
        frame,
        index_prev,
        &mut params.ipd_cos,
        &mut params.ipd_sin,
    )
}

/// IPD not carried this frame (either `!enable_ext` or the extension's own
/// `enable_ipdopd` bit was 0): reset to neutral -- see [`reset_iid`].
pub(super) fn reset_ipd(index_prev: &mut [i8], frame: &PsFrameLayout, params: &mut PsParams) {
    reset_ipdopd(index_prev, frame, &mut params.ipd_cos, &mut params.ipd_sin);
}

/// OPD (§3, inside `enable_ext`'s `ps_extension()`).
pub(super) fn read_opd(
    bits: &mut BitReader,
    end_bit: usize,
    frame: &PsFrameLayout,
    index_prev: &mut [i8],
    params: &mut PsParams,
) -> Result<(), DecodeError> {
    decode_ipdopd(
        bits,
        end_bit,
        ParamKind::Opd,
        frame,
        index_prev,
        &mut params.opd_cos,
        &mut params.opd_sin,
    )
}

/// OPD not carried this frame: reset to neutral -- see [`reset_iid`].
pub(super) fn reset_opd(index_prev: &mut [i8], frame: &PsFrameLayout, params: &mut PsParams) {
    reset_ipdopd(index_prev, frame, &mut params.opd_cos, &mut params.opd_sin);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::aac::bits::BitReader;
    use crate::formats::aac::ps::tables::{F_HUFF_ICC, SF_IID_NORMAL};

    #[test]
    fn huff_root_leaf_decodes_zero_delta() {
        // Every tree's root has a -31 leaf -> the shortest codeword decodes
        // to delta 0 (-31 + 31). F_HUFF_ICC row0 = {-31,1}: bit0=0 -> leaf
        // -31 -> value 0.
        let buf = [0u8; 2];
        let mut b = BitReader::new(&buf);
        assert_eq!(ps_huff_dec(&mut b, 16, &F_HUFF_ICC).unwrap(), 0);
    }

    #[test]
    fn iid_delta_freq_accumulate_and_dequant() {
        // dt=0 (delta-freq), nr_par=2. band0 codeword "0" (F_HUFF_IID_DEF
        // row0={-31,1}, bit0) -> leaf -31 -> delta 0. band1 codeword "100"
        // (row0 bit1->1, row1={2,3} bit0->2, row2={-30,-32} bit0-> leaf -30)
        // -> delta +1. Stream: dt(0) band0(0) band1(100) = "00100", padded.
        let buf = [0b0010_0000u8, 0];
        let mut b = BitReader::new(&buf);
        let mut raw = [[0i8; 34]; 5];
        let mut prev = [0i8; 20];
        decode_param(&mut b, 16, ParamKind::IidCoarse, 1, 2, &mut prev, &mut raw).unwrap();
        assert_eq!(raw[0][0], 0);
        assert_eq!(raw[0][1], 1);
        assert!((SF_IID_NORMAL[7 + raw[0][0] as usize] - 1.0).abs() < 1e-6);
        assert!((SF_IID_NORMAL[(7 + raw[0][1]) as usize] - 0.87961715).abs() < 1e-6);
        assert_eq!(&prev[..2], &[0, 1]); // index_prev updated to env 0 (the last envelope)
    }

    #[test]
    fn icc_clips_to_0_7() {
        // Two ICC delta-freq codewords, each the max ICC delta (+7,
        // F_HUFF_ICC leaf -24, 13-bit codeword "1111111111110"): band0 =
        // 0+7=7 (in range); band1 = 7+7=14, clipped to 7. The clipped index
        // must stay a safe COS_ALPHAS/SIN_ALPHAS index (no OOB) regardless
        // of how far the raw accumulation overshoots [NOPANIC].
        let buf = [0x7Fu8, 0xFB, 0xFF, 0xC0];
        let mut b = BitReader::new(&buf);
        let mut raw = [[0i8; 34]; 5];
        let mut prev = [0i8; 34];
        decode_param(&mut b, 32, ParamKind::Icc, 1, 2, &mut prev, &mut raw).unwrap();
        assert_eq!(raw[0][0], 7);
        assert_eq!(raw[0][1], 7); // clipped down from 14
        assert!((0..=7).contains(&raw[0][1]));
        let _ = COS_ALPHAS[raw[0][1] as usize]; // must not be OOB
    }

    #[test]
    fn ipd_wraps_mod_8() {
        // band0 delta=+7 (F_HUFF_IPD leaf -24, codeword "0111") -> index 7
        // (0+7, no wrap needed). band1 delta=+2 (leaf -29, codeword "0110")
        // -> 7+2=9, wraps mod 8 -> 1.
        let buf = [0x3Bu8, 0x00];
        let mut b = BitReader::new(&buf);
        let mut raw = [[0i8; 34]; 5];
        let mut prev = [0i8; 17];
        decode_param(&mut b, 16, ParamKind::Ipd, 1, 2, &mut prev, &mut raw).unwrap();
        assert_eq!(raw[0][0], 7);
        assert_eq!(raw[0][1], 1); // wrapped from 9
    }

    #[test]
    fn icc_delta_time_uses_previous_envelope() {
        // env0: dt=0 (freq), band0 delta=+3 (F_HUFF_ICC leaf -28, codeword
        // "111110") -> raw[0][0]=3.
        // env1: dt=1 (time), band0 delta=+2 (T_HUFF_ICC leaf -29, codeword
        // "1110") -> raw[1][0] = raw[0][0] + 2 = 5: the base is the
        // PREVIOUS ENVELOPE's value (env>0), not `index_prev`.
        let buf = [0x7Du8, 0xE0];
        let mut b = BitReader::new(&buf);
        let mut raw = [[0i8; 34]; 5];
        let mut prev = [0i8; 34];
        decode_param(&mut b, 16, ParamKind::Icc, 2, 1, &mut prev, &mut raw).unwrap();
        assert_eq!(raw[0][0], 3);
        assert_eq!(raw[1][0], 5);
        assert_eq!(prev[0], 5); // index_prev updated to the LAST envelope's index
    }

    #[test]
    fn truncated_stream_is_malformed_not_panic() {
        // dt bit present, but no room for even one huffman bit: end_bit
        // stops right after it. ps_huff_dec must Err (via clamped_read)
        // rather than reading OOB / panicking.
        let buf = [0u8; 1];
        let mut b = BitReader::new(&buf);
        let mut raw = [[0i8; 34]; 5];
        let mut prev = [0i8; 34];
        let result = decode_param(&mut b, 1, ParamKind::IidCoarse, 1, 1, &mut prev, &mut raw);
        assert!(result.is_err());
    }

    #[test]
    fn huff_dec_bounds_a_cyclic_table_no_infinite_loop() {
        // A pathological table where bit0 self-loops forever; ps_huff_dec
        // must bound the walk and Err rather than spin (defence in depth
        // beyond the real V1 trees, which are well-formed and terminate).
        let bad: [[i8; 2]; 1] = [[0, -31]];
        let buf = [0u8; 4]; // all-zero bits -> always takes the bit0 self-loop
        let mut b = BitReader::new(&buf);
        assert!(ps_huff_dec(&mut b, 32, &bad).is_err());
    }
}
