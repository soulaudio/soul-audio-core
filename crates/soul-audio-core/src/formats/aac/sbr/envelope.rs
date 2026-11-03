//! SBR envelope/noise decode: Huffman table selection (§6.1), the read
//! procedure (§6.2/§6.3), delta resolution (incl. the HI<->LO
//! frequency-resolution remap), and dequant to linear energy (§6.4).
//!
//! **Table selection (§6.1)** — the effective `amp_res` for this
//! channel/frame is forced to 0 (1.5 dB) whenever the frame has exactly one
//! envelope *and* is `FIXFIX` framed, overriding the header's `bs_amp_res`
//! (confirmed against FFmpeg `read_sbr_grid`: `if (bs_num_env == 1)
//! bs_amp_res = 0;`, FIXFIX-only). `coupling && ch==1` selects the `_bal_`
//! ("balance"/pan) table variants and doubles every decoded value (`delta`
//! shift of 1 bit) — confirmed against FFmpeg `aacsbr_template.c`
//! `read_sbr_envelope`/`read_sbr_noise`: `delta = (ch==1 &&
//! bs_coupling) + 1`, multiplied into *every* Huffman-decoded value (not
//! just the start value) for both delta-time and delta-freq. Noise
//! delta-freq has no table of its own: it **reuses** `f_huffman_env_3_0dB` /
//! `f_huffman_env_bal_3_0dB` — noise never uses a 1.5 dB table regardless of
//! this frame's `amp_res` (`sbr-reference-verbatim.md` §3's own note; the
//! real trap this task calls out).
//!
//! **Delta resolution + the HI<->LO remap (§6.2)** — delta-freq bands
//! accumulate horizontally (`E[k] = E[k-1] + huff`) from an absolute start
//! value; delta-time bands add the co-located band from the *source*
//! envelope (this env's own previous envelope within the frame, or
//! `EnvPrev` carried from the previous frame's last envelope, for env 0).
//! When the source's frequency resolution differs from this envelope's own,
//! the co-located band index must be remapped across the LO/HI grids. This
//! file uses the **O(1) index-formula form** (FFmpeg `read_sbr_envelope`:
//! `k = (j + odd) >> 1` mapping HI band `j` to its containing LO band, `k =
//! j ? 2*j - odd : 0` for the inverse), not faad2's border-value search
//! loop — `odd` is exactly `freq_tables::derive`'s own `n_high & 1` (`minus`
//! in that file), so the two are the same relationship the frequency tables
//! were already built from, verified by direct construction (see this
//! file's `hi_lo_remap_matches_worked_example_freq_tables` test, which
//! cross-checks both formulas against `freq_tables.rs`'s own §3.5 worked
//! example arrays). Every accumulated value is range-checked
//! (`0..=127`/`0..=30`) at each step; out-of-range (this file rejects
//! negative sums too, matching FFmpeg's `> 127U`/`> 30U` unsigned-comparison
//! trick, which also catches negative values) is `Err(Malformed)`. The one
//! exception: the noise delta-freq band-0 *absolute start* value is written
//! unvalidated (`0..=30` is not checked there) — confirmed against live
//! FFmpeg `aacsbr_template.c::read_sbr_noise`, which never range-checks that
//! read, only the deltas that follow it; coupled ch1 (`delta_shift == 1`)
//! legally produces a start value up to 62 (raw 16..=31, doubled), which
//! `dequant_coupled` handles fine.
//!
//! **Dequant (§6.4)** transcribed from the actual FFmpeg `aacsbr.c`
//! `sbr_dequant`/`exp2fi` source (fetched and quoted in the task report) —
//! not just the summarized reference doc — because the coupled envelope
//! path has a real subtlety the summary glossed over: when `amp_res==0`,
//! *both* `temp1` (left/level) and `temp2` (pan) go through the
//! `(q>>1)+ … · (q&1?√2:1)` half-step split, not just `temp1`.

use super::bitstream::clamped_read;
use super::freq_tables::FreqTables;
use super::grid::{FIXFIX, SbrGrid};
use super::huffman::{
    F_HUFF_ENV_1_5DB, F_HUFF_ENV_3_0DB, F_HUFF_ENV_BAL_1_5DB, F_HUFF_ENV_BAL_3_0DB, SbrHuff,
    T_HUFF_ENV_1_5DB, T_HUFF_ENV_3_0DB, T_HUFF_ENV_BAL_1_5DB, T_HUFF_ENV_BAL_3_0DB,
    T_HUFF_NOISE_3_0DB, T_HUFF_NOISE_BAL_3_0DB, sbr_huff_dec,
};
use crate::decoder::DecodeError;
use crate::formats::aac::bits::BitReader;

/// `NOISE_FLOOR_OFFSET` (§6.4 / `sbr-reference.md` §0).
const NOISE_FLOOR_OFFSET: i32 = 6;

/// Resolved envelope/noise quantized levels for one channel's frame
/// (§6.1-§6.3's output): `env_facs_q[env][band]` (0..=127),
/// `noise_facs_q[noise_env][band]` (0..=30), and the effective `amp_res`
/// this frame/channel actually used (post §6.1 FIXFIX-override).
#[derive(Clone, Copy)]
pub(super) struct EnvQuant {
    pub env_q: [[u8; 48]; 6],
    pub noise_q: [[u8; 5]; 6],
    pub amp_res: u8,
}

// `#[derive(Default)]` doesn't reach the `[u8; 48]`/`[u8; 5]` row types --
// `[T; N]: Default` is only implemented in `core` for `N <= 32` (same
// caveat `freq_tables::FreqTables` documents) -- so `Default` is written by
// hand, matching that file's convention.
impl Default for EnvQuant {
    fn default() -> Self {
        Self {
            env_q: [[0; 48]; 6],
            noise_q: [[0; 5]; 6],
            amp_res: 0,
        }
    }
}

/// Dequantized linear energy (§6.4 output), same shape as [`EnvQuant`].
#[derive(Clone, Copy)]
pub(super) struct EnvLinear {
    pub env: [[f32; 48]; 6],
    pub noise: [[f32; 5]; 6],
}

impl Default for EnvLinear {
    fn default() -> Self {
        Self {
            env: [[0.0; 48]; 6],
            noise: [[0.0; 5]; 6],
        }
    }
}

/// Previous-frame delta-time base, per channel (state carried across
/// frames): the last envelope's/noise-env's absolute quantized values plus
/// the frequency resolution they were computed in (needed for the HI<->LO
/// remap when the next frame's env 0 is delta-time-coded against it).
#[derive(Clone, Copy)]
pub(super) struct EnvPrev {
    pub e_prev: [u8; 64],
    pub q_prev: [u8; 64],
    pub freq_res_prev: u8,
}

impl Default for EnvPrev {
    fn default() -> Self {
        Self {
            e_prev: [0; 64],
            q_prev: [0; 64],
            freq_res_prev: 0,
        }
    }
}

/// Read `n` bits, clamped to `end_bit`. Mirrors the other SBR sub-parsers'
/// thin wrapper around the shared [`clamped_read`] clamp.
fn read_bits(bits: &mut BitReader, end_bit: usize, n: u32) -> Result<u32, DecodeError> {
    clamped_read(bits, end_bit, n).ok_or(DecodeError::Malformed)
}

/// §6.1 table selection: `(t_env, f_env, start_bits)` for the envelope
/// Huffman pair, and `(t_noise, f_noise)` for noise — where `f_noise`
/// **reuses** the envelope 3.0dB table (the trap this task calls out; see
/// module docs).
fn select_tables(amp_res1: bool, coupled_ch1: bool) -> (SbrHuff, SbrHuff, u32, SbrHuff, SbrHuff) {
    let (t_env, f_env, start_bits): (SbrHuff, SbrHuff, u32) = match (amp_res1, coupled_ch1) {
        (true, true) => (&T_HUFF_ENV_BAL_3_0DB, &F_HUFF_ENV_BAL_3_0DB, 5),
        (false, true) => (&T_HUFF_ENV_BAL_1_5DB, &F_HUFF_ENV_BAL_1_5DB, 6),
        (true, false) => (&T_HUFF_ENV_3_0DB, &F_HUFF_ENV_3_0DB, 6),
        (false, false) => (&T_HUFF_ENV_1_5DB, &F_HUFF_ENV_1_5DB, 7),
    };
    let (t_noise, f_noise): (SbrHuff, SbrHuff) = if coupled_ch1 {
        (&T_HUFF_NOISE_BAL_3_0DB, &F_HUFF_ENV_BAL_3_0DB)
    } else {
        (&T_HUFF_NOISE_3_0DB, &F_HUFF_ENV_3_0DB)
    };
    (t_env, f_env, start_bits, t_noise, f_noise)
}

/// Store a resolved envelope value, validating `0..=127` (§6.2 "FFmpeg
/// validates env_facs_q <= 127 each step") *at this step* rather than in a
/// bulk pass — a negative `val` (e.g. an over-large negative Huffman delta)
/// is rejected here too, matching FFmpeg's `> 127U` unsigned-comparison
/// trick which implicitly also rejects negative sums.
fn store_env(q: &mut EnvQuant, env: usize, band: usize, val: i32) -> Result<(), DecodeError> {
    if !(0..=127).contains(&val) {
        return Err(DecodeError::Malformed);
    }
    let slot = q
        .env_q
        .get_mut(env)
        .and_then(|row| row.get_mut(band))
        .ok_or(DecodeError::Malformed)?;
    *slot = val as u8;
    Ok(())
}

/// Store a resolved noise value, validating `0..=30` (§6.3) at this step.
fn store_noise(
    q: &mut EnvQuant,
    noise_env: usize,
    band: usize,
    val: i32,
) -> Result<(), DecodeError> {
    if !(0..=30).contains(&val) {
        return Err(DecodeError::Malformed);
    }
    let slot = q
        .noise_q
        .get_mut(noise_env)
        .and_then(|row| row.get_mut(band))
        .ok_or(DecodeError::Malformed)?;
    *slot = val as u8;
    Ok(())
}

/// The delta-time source value for envelope `env`'s band `band`, given this
/// envelope's own resolution `res` and the *source* envelope's resolution
/// `g` (the previous envelope within this frame, or `EnvPrev` for `env==0`)
/// — the §6.2 HI<->LO remap (see module docs for the index-formula
/// derivation + where it's cross-checked).
fn env_delta_time_source(
    q: &EnvQuant,
    prev: &EnvPrev,
    env: usize,
    res: u8,
    g: u8,
    band: usize,
    n_high: u8,
) -> Result<i32, DecodeError> {
    let odd = (n_high & 1) as usize;
    let k = if res == g {
        band
    } else if res == 1 {
        // current HI, source LO: containing LO band = (band + odd) >> 1.
        (band + odd) >> 1
    } else {
        // current LO, source HI: exact-match HI band = 2*band - odd (band 0 -> 0).
        if band == 0 { 0 } else { 2 * band - odd }
    };
    if env == 0 {
        Ok(*prev.e_prev.get(k).ok_or(DecodeError::Malformed)? as i32)
    } else {
        Ok(*q
            .env_q
            .get(env - 1)
            .and_then(|row| row.get(k))
            .ok_or(DecodeError::Malformed)? as i32)
    }
}

/// Read + delta-resolve envelope & noise quantized levels (§6.1-§6.3).
/// `coupling` + `ch` select the Huffman table set + the `delta` shift;
/// updates `prev` with this frame's last envelope/noise-env for the next
/// frame's delta-time base.
// 10 parameters is over clippy's default `too_many_arguments` threshold
// (7), but this signature is the task-7 brief's mandated interface --
// bundling into a params struct would just move the same fields one level
// out for no clarity gain at this call count.
// `env`/`noise_env` each index several unrelated arrays in lockstep
// (`df_env`/`grid.freq_res`/`q.env_q`, and `df_noise`/`q.noise_q`) --
// clippy's `.iter().enumerate()` suggestion only fits a single-array index,
// so a plain range loop stays clearer here.
#[allow(clippy::too_many_arguments, clippy::needless_range_loop)]
pub(super) fn read_env_noise(
    bits: &mut BitReader,
    end_bit: usize,
    grid: &SbrGrid,
    df_env: &[u8; 5],
    df_noise: &[u8; 2],
    ft: &FreqTables,
    hdr_amp_res: u8,
    coupling: bool,
    ch: usize,
    prev: &mut EnvPrev,
) -> Result<EnvQuant, DecodeError> {
    let mut q = read_envelope(
        bits,
        end_bit,
        grid,
        df_env,
        ft,
        hdr_amp_res,
        coupling,
        ch,
        prev,
    )?;
    read_noise(
        bits, end_bit, grid, df_noise, ft, coupling, ch, &mut q, prev,
    )?;
    Ok(q)
}

/// §6.1-§6.2 envelope-only half of [`read_env_noise`]: reads + delta-resolves
/// this channel's envelope scalefactors and carries the last envelope into
/// `prev.e_prev`/`prev.freq_res_prev`. Split out so the **non-coupled CPE**
/// bitstream order (env0, env1, *then* noise0, noise1 — FFmpeg
/// `read_sbr_channel_pair_element`'s `bs_coupling == 0` branch) can be honoured
/// by the T11 driver, which reads both channels' envelopes before either
/// channel's noise. SCE and coupled CPE go through [`read_env_noise`] (env then
/// noise per channel), so the existing Task-7 tests are unaffected.
#[allow(clippy::too_many_arguments, clippy::needless_range_loop)]
pub(super) fn read_envelope(
    bits: &mut BitReader,
    end_bit: usize,
    grid: &SbrGrid,
    df_env: &[u8; 5],
    ft: &FreqTables,
    hdr_amp_res: u8,
    coupling: bool,
    ch: usize,
    prev: &mut EnvPrev,
) -> Result<EnvQuant, DecodeError> {
    let num_env = grid.num_env as usize;
    if num_env == 0 || num_env > 5 {
        return Err(DecodeError::Malformed);
    }

    // §6.1: FIXFIX + a single envelope forces 1.5dB regardless of the
    // header (FFmpeg `read_sbr_grid`: `if (bs_num_env == 1) bs_amp_res = 0`).
    let amp_res: u8 = if num_env == 1 && grid.frame_class == FIXFIX {
        0
    } else {
        hdr_amp_res
    };
    let amp_res1 = amp_res == 1;
    let coupled_ch1 = coupling && ch == 1;
    let delta_shift: i32 = i32::from(coupled_ch1); // 0 or 1 -- "coupled ch1 codes are x2"

    let (t_env, f_env, env_start_bits, _t_noise, _f_noise) = select_tables(amp_res1, coupled_ch1);

    let mut q = EnvQuant {
        amp_res,
        ..EnvQuant::default()
    };

    for env in 0..num_env {
        let res = grid.freq_res[env];
        let n = ft.n(res);
        if n == 0 || n > 48 {
            return Err(DecodeError::Malformed);
        }
        if df_env[env] == 0 {
            // delta-freq: absolute start value + horizontal deltas.
            let raw = read_bits(bits, end_bit, env_start_bits)? as i32;
            let mut val = raw << delta_shift;
            store_env(&mut q, env, 0, val)?;
            for band in 1..n {
                let d = sbr_huff_dec(bits, end_bit, f_env)? << delta_shift;
                val += d;
                store_env(&mut q, env, band, val)?;
            }
        } else {
            // delta-time: vertical deltas vs the co-located source band,
            // remapped across LO<->HI resolution when it differs.
            let g = if env == 0 {
                prev.freq_res_prev
            } else {
                grid.freq_res[env - 1]
            };
            for band in 0..n {
                let d = sbr_huff_dec(bits, end_bit, t_env)? << delta_shift;
                let src = env_delta_time_source(&q, prev, env, res, g, band, ft.n_high)?;
                store_env(&mut q, env, band, src + d)?;
            }
        }
    }

    // Carry the last envelope into `prev` for the next frame's delta-time base
    // (§6.2's "previous frame's last envelope").
    let last_env = num_env - 1;
    let n_last = ft.n(grid.freq_res[last_env]);
    let mut e_prev = [0u8; 64];
    e_prev[..n_last].copy_from_slice(&q.env_q[last_env][..n_last]);
    prev.e_prev = e_prev;
    prev.freq_res_prev = grid.freq_res[last_env];

    Ok(q)
}

/// §6.3 noise-only half of [`read_env_noise`]: reads + delta-resolves this
/// channel's noise-floor scalefactors into `q.noise_q` and carries the last
/// noise-env into `prev.q_prev`. See [`read_envelope`] for why the two halves
/// are separable (non-coupled CPE ordering). `q` is the [`EnvQuant`]
/// [`read_envelope`] already filled for this channel.
#[allow(clippy::too_many_arguments, clippy::needless_range_loop)]
pub(super) fn read_noise(
    bits: &mut BitReader,
    end_bit: usize,
    grid: &SbrGrid,
    df_noise: &[u8; 2],
    ft: &FreqTables,
    coupling: bool,
    ch: usize,
    q: &mut EnvQuant,
    prev: &mut EnvPrev,
) -> Result<(), DecodeError> {
    let num_noise = grid.num_noise as usize;
    if num_noise == 0 || num_noise > 2 {
        return Err(DecodeError::Malformed);
    }
    let coupled_ch1 = coupling && ch == 1;
    let delta_shift: i32 = i32::from(coupled_ch1);
    // Only the noise tables matter here; `amp_res1` never affects them
    // (`f_noise` reuses the 3.0dB envelope table regardless), so pass `false`.
    let (_t_env, _f_env, _start, t_noise, f_noise) = select_tables(false, coupled_ch1);

    let n_q = ft.n_q as usize;
    if n_q == 0 || n_q > 5 {
        return Err(DecodeError::Malformed);
    }
    for noise_env in 0..num_noise {
        if df_noise[noise_env] == 0 {
            let raw = read_bits(bits, end_bit, 5)? as i32;
            let mut val = raw << delta_shift;
            // Band 0 is the *absolute* noise start value -- write it
            // directly (still `.get_mut()`-bounds-checked for panic-safety)
            // WITHOUT `store_noise`'s `0..=30` semantic clamp. Coupled ch1
            // (`delta_shift == 1`) legally produces `val` up to 62 (raw
            // 16..=31, doubled) -- confirmed against live FFmpeg
            // `aacsbr_template.c::read_sbr_noise`: the absolute start read
            // is never range-checked, only the subsequent Huffman-decoded
            // deltas are (`store_noise`'s `0..=30` check stays below, for
            // bands `1..n_q`). `raw` is a 5-bit field (0..=31) and
            // `delta_shift` is 0 or 1, so `val` is always `0..=62`, well
            // within `u8`'s range -- the `as u8` cast is exact, and
            // `dequant_coupled` handles `q1` up to 62 fine (a small pan
            // factor, no overflow).
            let slot = q
                .noise_q
                .get_mut(noise_env)
                .and_then(|row| row.get_mut(0))
                .ok_or(DecodeError::Malformed)?;
            *slot = val as u8;
            for band in 1..n_q {
                let d = sbr_huff_dec(bits, end_bit, f_noise)? << delta_shift;
                val += d;
                store_noise(q, noise_env, band, val)?;
            }
        } else {
            // Noise has no LO/HI split (always N_Q bands) -- no remap needed.
            for band in 0..n_q {
                let d = sbr_huff_dec(bits, end_bit, t_noise)? << delta_shift;
                let base = if noise_env == 0 {
                    *prev.q_prev.get(band).ok_or(DecodeError::Malformed)? as i32
                } else {
                    *q.noise_q
                        .get(noise_env - 1)
                        .and_then(|row| row.get(band))
                        .ok_or(DecodeError::Malformed)? as i32
                };
                store_noise(q, noise_env, band, base + d)?;
            }
        }
    }

    // Carry the last noise-env into `prev` for the next frame's delta-time
    // base (§6.3's "previous frame's last noise envelope").
    let last_noise = num_noise - 1;
    let mut q_prev = [0u8; 64];
    q_prev[..n_q].copy_from_slice(&q.noise_q[last_noise][..n_q]);
    prev.q_prev = q_prev;

    Ok(())
}

/// `2^x` for integer `x`, via IEEE-754 exponent bit-pack (no libm/`powf`) --
/// mirrors FFmpeg `aacsbr.c`'s `exp2fi`. Clamping `x` to `[-126, 127]` keeps
/// the bit-pack itself well-defined for this module's whole input domain
/// (worst case, per §6.4: an amp_res=1 env exponent `q+7` with `q<=127` ->
/// 134, or a coupled pan exponent as low as `12-127=-115`); a value that
/// would need `x>127` already exceeds `2^127 ~= 1.7e38 > 1e20`, so it still
/// trips each call site's own `>1e20` overflow guard below, exactly
/// matching FFmpeg `sbr_dequant`'s `if (temp1 > 1E20) temp1 = 1;`.
fn exp2i(x: i32) -> f32 {
    let xc = x.clamp(-126, 127);
    f32::from_bits(((xc + 127) as u32) << 23)
}

/// `{1, sqrt(2)}`, the amp_res=0 "half dB step" factor selected by the
/// quantized value's low bit (§6.4).
const SQRT2_TAB: [f32; 2] = [1.0, core::f32::consts::SQRT_2];

/// Dequant to linear energy (§6.4). SCE / non-coupled CPE — transcribed
/// from FFmpeg `aacsbr.c` `sbr_dequant`'s `else` (non-coupled) branch.
pub(super) fn dequant_sce(q: &EnvQuant) -> EnvLinear {
    let mut lin = EnvLinear::default();
    let amp_res1 = q.amp_res != 0;
    for e in 0..6 {
        for k in 0..48 {
            let qv = q.env_q[e][k] as i32;
            let mut val = if amp_res1 {
                exp2i(qv + 6)
            } else {
                exp2i((qv >> 1) + 6) * SQRT2_TAB[(qv & 1) as usize]
            };
            if val > 1.0e20 {
                val = 1.0;
            }
            lin.env[e][k] = val;
        }
    }
    for e in 0..6 {
        for k in 0..5 {
            let qv = q.noise_q[e][k] as i32;
            lin.noise[e][k] = exp2i(NOISE_FLOOR_OFFSET - qv);
        }
    }
    lin
}

/// Coupled CPE: un-map ch0(left level) + ch1(pan) quantized values to L/R
/// energies (§6.4) — transcribed from FFmpeg `aacsbr.c` `sbr_dequant`'s
/// `if (bs_coupling)` branch, INCLUDING the amp_res==0 subtlety the summary
/// reference doc glossed over: `temp2` (the pan factor) goes through the
/// same `(q>>1)+.. * (q&1?sqrt2:1)` half-step split as `temp1` when
/// `amp_res==0`, not a plain `2^(panOffset-q1)`.
pub(super) fn dequant_coupled(q0: &EnvQuant, q1: &EnvQuant) -> (EnvLinear, EnvLinear) {
    let mut left = EnvLinear::default();
    let mut right = EnvLinear::default();
    // The coupled pair shares ch0's grid/amp_res (§6.1: only ch0 carries the
    // grid; ch1's amp_res field, if any, is never independently consulted).
    let amp_res1 = q0.amp_res != 0;
    let pan_offset: i32 = if amp_res1 { 12 } else { 24 };

    for e in 0..6 {
        for k in 0..48 {
            let qv0 = q0.env_q[e][k] as i32;
            let qv1 = q1.env_q[e][k] as i32;
            let (mut temp1, temp2) = if amp_res1 {
                (exp2i(qv0 + 7), exp2i(pan_offset - qv1))
            } else {
                let t1 = exp2i((qv0 >> 1) + 7) * SQRT2_TAB[(qv0 & 1) as usize];
                let d = pan_offset - qv1;
                let t2 = exp2i(d >> 1) * SQRT2_TAB[(d & 1) as usize];
                (t1, t2)
            };
            if temp1 > 1.0e20 {
                temp1 = 1.0;
            }
            let fac = temp1 / (1.0 + temp2);
            left.env[e][k] = fac;
            right.env[e][k] = fac * temp2;
        }
    }

    for e in 0..6 {
        for k in 0..5 {
            let qv0 = q0.noise_q[e][k] as i32;
            let qv1 = q1.noise_q[e][k] as i32;
            let temp1 = exp2i(NOISE_FLOOR_OFFSET - qv0 + 1);
            let temp2 = exp2i(12 - qv1);
            let fac = temp1 / (1.0 + temp2);
            left.noise[e][k] = fac;
            right.noise[e][k] = fac * temp2;
        }
    }

    (left, right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::aac::bits::BitReader;
    use crate::formats::aac::sbr::freq_tables;
    use crate::formats::aac::sbr::header::SbrHeader;

    #[test]
    fn dequant_sce_amp_res_1() {
        // amp_res=1 -> E = 2^(q+6). q=1 -> 128.
        let mut q = EnvQuant {
            amp_res: 1,
            ..EnvQuant::default()
        };
        q.env_q[0][0] = 1;
        let lin = dequant_sce(&q);
        assert!((lin.env[0][0] - 128.0).abs() < 1e-3);
    }

    #[test]
    fn dequant_sce_noise() {
        // Q = 2^(6 - q). q=6 -> 1.0.
        let mut q = EnvQuant::default();
        q.noise_q[0][0] = 6;
        let lin = dequant_sce(&q);
        assert!((lin.noise[0][0] - 1.0).abs() < 1e-4);
    }

    #[test]
    fn dequant_sce_amp_res_0_half_step() {
        // amp_res=0: q=0 -> 2^6=64 (no sqrt2); q=1 -> 2^6*sqrt2 (odd bit).
        let mut q = EnvQuant::default();
        q.env_q[0][0] = 0;
        q.env_q[0][1] = 1;
        let lin = dequant_sce(&q);
        assert!((lin.env[0][0] - 64.0).abs() < 1e-3);
        assert!((lin.env[0][1] - 64.0 * core::f32::consts::SQRT_2).abs() < 1e-2);
    }

    #[test]
    fn dequant_coupled_matches_ffmpeg_formula_amp_res_1() {
        // amp_res=1: temp1=2^(q0+7), temp2=2^(panOffset(12)-q1);
        // env_L=temp1/(1+temp2), env_R=env_L*temp2. Pick q0=0 (temp1=128),
        // q1=12 (temp2=2^0=1) -> env_L=64, env_R=64.
        let mut q0 = EnvQuant {
            amp_res: 1,
            ..EnvQuant::default()
        };
        q0.env_q[0][0] = 0;
        let mut q1 = EnvQuant::default();
        q1.env_q[0][0] = 12;
        let (l, r) = dequant_coupled(&q0, &q1);
        assert!((l.env[0][0] - 64.0).abs() < 1e-2);
        assert!((r.env[0][0] - 64.0).abs() < 1e-2);
    }

    /// Cross-check the O(1) HI<->LO remap index formulas against a real
    /// `FreqTables`' `f_high`/`f_low` arrays, by tagging every source band
    /// with a distinct sentinel and confirming each target band resolves to
    /// the sentinel of the LO/HI band that actually contains/matches it --
    /// this is the part of §6.2 the brief calls out as the untested risk.
    /// Shared by both `n_high` parities below (`odd = n_high & 1` selects a
    /// different `k` formula branch, so both must be exercised).
    fn assert_hi_lo_remap_matches(ft: &FreqTables) {
        // LO(source) -> HI(current): sentinel LO band i is e_prev[i]=100+i.
        let mut prev = EnvPrev::default();
        for i in 0..=(ft.n_low as usize) {
            prev.e_prev[i] = 100 + i as u8;
        }
        prev.freq_res_prev = 0; // LO
        let q = EnvQuant::default();
        for band in 0..(ft.n_high as usize) {
            let hi_border = ft.f_high[band];
            // Brute-force "which LO band contains this HI border" (the
            // ground truth faad2's search loop computes) from the actual
            // derived f_low table.
            let mut expect_k = 0usize;
            for i in 0..(ft.n_low as usize) {
                if ft.f_low[i] <= hi_border && hi_border < ft.f_low[i + 1] {
                    expect_k = i;
                }
            }
            let got = env_delta_time_source(&q, &prev, 0, 1, 0, band, ft.n_high).unwrap();
            assert_eq!(
                got,
                100 + expect_k as i32,
                "HI band {band} (border {hi_border}) should source LO band {expect_k}"
            );
        }

        // HI(source) -> LO(current): sentinel HI band i is e_prev[i]=200+i.
        let mut prev2 = EnvPrev::default();
        for i in 0..=(ft.n_high as usize) {
            prev2.e_prev[i] = 200 + i as u8;
        }
        prev2.freq_res_prev = 1; // HI
        for band in 0..(ft.n_low as usize) {
            let lo_border = ft.f_low[band];
            let mut expect_k = None;
            for i in 0..(ft.n_high as usize) {
                if ft.f_high[i] == lo_border {
                    expect_k = Some(i);
                }
            }
            let expect_k = expect_k.expect("every LO border must land exactly on a HI border");
            let got = env_delta_time_source(&q, &prev2, 0, 0, 1, band, ft.n_high).unwrap();
            assert_eq!(
                got,
                200 + expect_k as i32,
                "LO band {band} (border {lo_border}) should source HI band {expect_k}"
            );
        }
    }

    #[test]
    fn hi_lo_remap_matches_worked_example_odd_n_high() {
        // §3.5 first worked example: fs=44100, start=5/stop=6/scale=0/
        // alter=0/xover=3/noise=2 -> n_high=19 (ODD, odd=1: exercises the
        // `2*band-odd` / `(band+1)>>1` formula branch with a singleton
        // first LO band).
        let hdr = SbrHeader {
            amp_res: 1,
            start_freq: 5,
            stop_freq: 6,
            xover_band: 3,
            freq_scale: 0,
            alter_scale: 0,
            noise_bands: 2,
            limiter_bands: 2,
            limiter_gains: 2,
            interpol_freq: 1,
            smoothing_mode: 1,
        };
        let ft = freq_tables::derive(&hdr, 44100).unwrap();
        assert_eq!(ft.n_high, 19);
        assert_eq!(ft.n_low, 10);
        assert_eq!(ft.n_high & 1, 1, "this example must exercise odd=1");
        assert_hi_lo_remap_matches(&ft);
    }

    #[test]
    fn hi_lo_remap_matches_worked_example_even_n_high() {
        // §3.5 second worked example: fs=48000, start=5/stop=3/scale=2/
        // alter=1/xover=4/noise=2 -> n_high=6 (EVEN, odd=0: every LO band
        // pairs exactly 2 HI bands, no singleton -- the other parity of the
        // `k` formula's `odd` term).
        let hdr = SbrHeader {
            amp_res: 1,
            start_freq: 5,
            stop_freq: 3,
            xover_band: 4,
            freq_scale: 2,
            alter_scale: 1,
            noise_bands: 2,
            limiter_bands: 2,
            limiter_gains: 2,
            interpol_freq: 1,
            smoothing_mode: 1,
        };
        let ft = freq_tables::derive(&hdr, 48000).unwrap();
        assert_eq!(ft.n_high, 6);
        assert_eq!(ft.n_low, 3);
        assert_eq!(ft.n_high & 1, 0, "this example must exercise odd=0");
        assert_hi_lo_remap_matches(&ft);
    }

    /// Minimal delta-freq smoke test (single-band env/noise so no Huffman
    /// symbol needs decoding -- just the start-value read path): FIXFIX,
    /// num_env=1 forces amp_res effective 0 regardless of the header's 1;
    /// asserts the decoded values, the FIXFIX amp_res override, and the
    /// `EnvPrev` carry, with no panic.
    #[test]
    fn read_env_noise_delta_freq_smoke() {
        let ft = FreqTables {
            n_low: 1,
            n_high: 1,
            n_q: 1,
            ..FreqTables::default()
        };

        let grid = SbrGrid {
            frame_class: FIXFIX,
            num_env: 1,
            num_noise: 1,
            t_env: [0, 16, 0, 0, 0, 0],
            t_noise: [0, 16, 0],
            freq_res: [0, 0, 0, 0, 0, 0],
            pointer: 0,
            l_a: -1,
        };
        let df_env = [0u8; 5]; // delta-freq
        let df_noise = [0u8; 2]; // delta-freq

        // env start=42 (7 bits, amp_res effective=0 forced by FIXFIX+L_E=1):
        // 0101010. noise start=10 (5 bits): 01010. Concatenated + zero pad:
        // 0101010 01010 000 -> 01010100 10100000 = 0x54, 0xA0.
        let buf = [0x54u8, 0xA0u8];
        let mut b = BitReader::new(&buf);
        let mut prev = EnvPrev::default();
        let q = read_env_noise(
            &mut b, 12, &grid, &df_env, &df_noise, &ft, 1, false, 0, &mut prev,
        )
        .unwrap();

        assert_eq!(q.amp_res, 0, "FIXFIX + L_E==1 must force amp_res=0");
        assert_eq!(q.env_q[0][0], 42);
        assert_eq!(q.noise_q[0][0], 10);
        assert_eq!(prev.e_prev[0], 42);
        assert_eq!(prev.q_prev[0], 10);
        assert_eq!(prev.freq_res_prev, 0);
    }

    #[test]
    fn read_env_noise_accepts_wide_noise_delta_freq_start_value() {
        // Task 7 review fix (Important finding): the noise delta-freq
        // *absolute start* value (band 0) is NOT range-checked against
        // 0..=30 -- confirmed against live FFmpeg
        // `aacsbr_template.c::read_sbr_noise`: the start read is never
        // validated, only the Huffman-decoded deltas that follow it are.
        // This test exercises the raw 5-bit field's max (31) in the
        // *non-coupled* case (delta_shift=0, so val=raw=31); it used to be
        // (wrongly) routed through `store_noise` and rejected here -- see
        // `read_env_noise_coupled_ch1_noise_delta_freq_wide_start_is_accepted`
        // below for the coupled-ch1 case (delta_shift=1, val up to 62) the
        // review finding actually called out.
        let ft = FreqTables {
            n_low: 1,
            n_high: 1,
            n_q: 1,
            ..FreqTables::default()
        };
        let grid = SbrGrid {
            frame_class: FIXFIX,
            num_env: 1,
            num_noise: 1,
            t_env: [0, 16, 0, 0, 0, 0],
            t_noise: [0, 16, 0],
            freq_res: [0, 0, 0, 0, 0, 0],
            pointer: 0,
            l_a: -1,
        };
        let df_env = [0u8; 5];
        let df_noise = [0u8; 2];
        // env start=0 (7 bits: 0000000), noise start=31 (5 bits: 11111).
        let buf = [0b0000_0001, 0b1111_0000];
        let mut b = BitReader::new(&buf);
        let mut prev = EnvPrev::default();
        let q = read_env_noise(
            &mut b, 12, &grid, &df_env, &df_noise, &ft, 1, false, 0, &mut prev,
        )
        .unwrap();
        assert_eq!(
            q.noise_q[0][0], 31,
            "band-0 absolute noise start is unvalidated, matching ffmpeg"
        );
    }

    #[test]
    fn read_env_noise_coupled_ch1_noise_delta_freq_wide_start_is_accepted() {
        // Task 7 review fix (Important finding), the actual bug scenario:
        // coupled ch1 (`coupling=true, ch=1`) noise delta-freq with a raw
        // 5-bit start value >=16 -- `val = raw << 1` lands in 32..=62,
        // which the pre-fix code rejected (`store_noise`'s `0..=30` check
        // on the absolute start) even though FFmpeg `read_sbr_noise` and
        // faad2 both accept it (only the subsequent deltas are validated),
        // and `dequant_coupled` handles `q1` up to 62 fine. This is
        // confirmed to fail against the pre-fix code (manually verified:
        // pre-fix, `store_noise(&mut q, 0, 0, 40)` hits the `0..=30` guard
        // and returns `Err(DecodeError::Malformed)`, so this test's
        // `.unwrap()` would panic).
        //
        // raw noise start = 20 (10100) -> val = 20 << 1 = 40.
        let ft = FreqTables {
            n_low: 1,
            n_high: 1,
            n_q: 1,
            ..FreqTables::default()
        };
        let grid = SbrGrid {
            frame_class: FIXFIX,
            num_env: 1,
            num_noise: 1,
            t_env: [0, 16, 0, 0, 0, 0],
            t_noise: [0, 16, 0],
            freq_res: [0, 0, 0, 0, 0, 0],
            pointer: 0,
            l_a: -1,
        };
        let df_env = [0u8; 5]; // delta-freq
        let df_noise = [0u8; 2]; // delta-freq
        // coupled ch1 -> delta_shift=1 -> env start_bits=6 (the "_bal_"
        // table). env start=0 (6 bits: 000000), noise start=20 (5 bits:
        // 10100). Concatenated + zero pad: 000000 10100 000000 ->
        // 00000010 10000000 = 0x02, 0x80.
        let buf = [0x02u8, 0x80u8];
        let mut b = BitReader::new(&buf);
        let mut prev = EnvPrev::default();
        let q = read_env_noise(
            &mut b, 11, &grid, &df_env, &df_noise, &ft, 1, true, 1, &mut prev,
        )
        .unwrap();
        assert_eq!(
            q.noise_q[0][0], 40,
            "coupled ch1 noise delta-freq start 32..=62 must be accepted (matches ffmpeg)"
        );
    }

    #[test]
    fn store_noise_still_rejects_out_of_range_accumulated_band() {
        // The noise `0..=30` check itself (§6.3) is unchanged by the Task 7
        // review fix -- it was only removed from the band-0 *absolute
        // start* write in `read_env_noise`. Bands 1..n_q (accumulated
        // horizontal deltas) still go through `store_noise` and must still
        // reject anything outside 0..=30.
        let mut q = EnvQuant::default();
        assert!(store_noise(&mut q, 0, 1, 30).is_ok());
        assert!(store_noise(&mut q, 0, 1, 31).is_err());
        assert!(store_noise(&mut q, 0, 1, -1).is_err());
    }

    #[test]
    fn read_env_noise_read_past_end_bit_is_malformed_not_panic() {
        let ft = FreqTables {
            n_low: 1,
            n_high: 1,
            n_q: 1,
            ..FreqTables::default()
        };
        let grid = SbrGrid {
            frame_class: FIXFIX,
            num_env: 1,
            num_noise: 1,
            t_env: [0, 16, 0, 0, 0, 0],
            t_noise: [0, 16, 0],
            freq_res: [0, 0, 0, 0, 0, 0],
            pointer: 0,
            l_a: -1,
        };
        let df_env = [0u8; 5];
        let df_noise = [0u8; 2];
        let buf = [0xFFu8, 0xFFu8];
        let mut b = BitReader::new(&buf);
        let mut prev = EnvPrev::default();
        // end_bit=4 leaves no room for the 7-bit env start read.
        let r = read_env_noise(
            &mut b, 4, &grid, &df_env, &df_noise, &ft, 1, false, 0, &mut prev,
        );
        assert!(r.is_err());
    }
}
