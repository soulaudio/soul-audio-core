//! `sbr-reference.md` §9 — HF adjustment: mapping envelope/noise/sine data
//! onto QMF bins (§9.1 `sbr_mapping`), estimating the current (pre-gain)
//! high-band energy from `X_high` (§9.2 `sbr_env_estimate`), computing
//! per-band gain/noise/sine levels with the limiter + boost (§9.3
//! `sbr_gain_calc`), and assembling the final adjusted high band `Y` with
//! 5-tap smoothing and the pseudo-random noise/sine injection (§9.4
//! `sbr_hf_assemble`).
//!
//! Uses FFmpeg's linear-float form (not faad2's log2-domain one) per the
//! task brief — cross-checked against the **live** FFmpeg source (fetched
//! via `curl`, not the summarizing WebFetch tool, per the brief's explicit
//! instruction that WebFetch has been unreliable on SBR details):
//! `libavcodec/aacsbr_template.c` (`sbr_mapping`, `sbr_env_estimate`) and
//! `libavcodec/aacsbr.c` (`sbr_gain_calc`, `sbr_hf_assemble`, which for the
//! *float* build live in `aacsbr.c` itself, included **before**
//! `aacsbr_template.c` — not in the template file, which only holds the
//! fixed/float-shared code); the per-band sine phase/gain-apply/noise-apply
//! primitives (`sbr_hf_g_filt_c`, `sbr_hf_apply_noise`) live in
//! `libavcodec/sbrdsp.c` + `sbrdsp_template.c`. See `task-10-report.md` for
//! the full derivation and the exact cross-checked formulas; its Concerns
//! section originally documented two simplifications relative to FFmpeg's
//! multi-frame-continuous `SBRData` state (the sine-map prev-frame carry,
//! the `g_temp`/`q_temp` ring re-prime) — both since implemented in full
//! (`HfAdjState::s_index_carry`/`t_env_num_env_old`) per a Task 10 review
//! pass; see the "Task 10 fix" section appended to that report for the
//! before/after and the two remaining Critical fixes from the same review
//! (`index_noise`'s transient-envelope advance, smoothing-off noise).

use super::envelope::EnvLinear;
use super::freq_tables::FreqTables;
use super::grid::SbrGrid;
use super::header::SbrHeader;
use super::qmf::{Cpx, MAX_SLOTS, MAX_Y_SLOTS, N_SYN, T_HFADJ};
use super::tables::V_NOISE;
use crate::decoder::DecodeError;

/// Max QMF high-band count (`M`, §3.4: `f_high[n_high] - f_high[0]` is
/// capped `1..=49`) — local per-band scratch arrays are sized to this, one
/// wider than `HfAdjState::g_temp`/`q_temp`'s 48 columns (see
/// `hf_assemble`'s `RING_COLS` note for why that one stays 48).
const MAX_M: usize = 49;
/// Max envelopes per frame (§4, `L_E` 1..=5).
const MAX_ENV: usize = 5;
/// `HfAdjState::g_temp`/`q_temp` ring depth: worst case is
/// `h_SL(4) + 2*(t_env[L_E] - t_env[0])` with `t_env[L_E]` up to
/// `numTimeSlots+3 = 19` (FIXVAR/VARVAR's trailing-border read can exceed
/// the nominal 16-slot frame, `grid.rs`), i.e. `4 + 2*19 = 42`.
const RING_LEN: usize = 42;
/// `HfAdjState::g_temp`/`q_temp` column width — matches the task brief's
/// literal `[f32; 48]` (one short of `MAX_M`=49's theoretical worst case);
/// the one-band overrun at `M==49` is bounded via `.get()`/slice truncation
/// below rather than widening the brief-mandated struct shape.
const RING_COLS: usize = 48;

/// No-libm `f32` square root: extract the IEEE-754 exponent and build a
/// coarse seed `2^floor(exp/2) · (√2 if exp is odd else 1)` — this ignores
/// the mantissa entirely (one constant seed per whole octave of `x`), so
/// it's accurate only to within the octave's spread: at the top of an
/// even-exponent octave the seed undershoots by a factor of `1/√2`
/// (`≈29.3%` relative error), which is the worst case across the whole
/// domain. Three Newton-Raphson refinements (`y = 0.5·(y + x/y)`, which for
/// this iteration roughly *squares* the relative error each step:
/// `e_{n+1} ≈ e_n²/2`) take that `≈0.29` down through `≈0.042 → ≈0.00088 →
/// ≈2e-7` — well past `f32` precision, hence "2–3" iterations sufficing per
/// the brief. `x<0`/NaN (`x > 0.0` is false for both, since NaN comparisons
/// are always false) and `x == 0.0` (`0.0 > 0.0` is false too) all return
/// exactly `0.0` — the exponent-seed path would only asymptotically
/// approach `0.0` through the iteration for a genuine `0.0` input, never
/// hit it bit-exact, which the brief's test requires.
// The negated `!(x > 0.0)` (rather than `x <= 0.0`) is deliberate: it also
// catches NaN (every comparison against NaN, including `<=`, is false, so
// `x <= 0.0` would NOT catch a NaN input, but `!(x > 0.0)` does since
// `x > 0.0` is false for NaN too) -- exactly the guard this fn's doc comment
// and the brief's test (`sqrtf(-1.0) == 0.0`, "guarded") require.
#[allow(clippy::neg_cmp_op_on_partial_ord)]
pub(crate) fn sqrtf(x: f32) -> f32 {
    if !(x > 0.0) {
        return 0.0;
    }
    let bits = x.to_bits();
    let exp = ((bits >> 23) & 0xFF) as i32 - 127;
    let half_exp = exp >> 1; // arithmetic (floor) shift -- correct for negative exp too
    let remainder = exp - 2 * half_exp; // 0 or 1
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

/// §9.3 per-band base gain (pre limiter/boost), FFmpeg `sbr_gain_calc`'s
/// per-`m` gain formula transcribed exactly (both branches), including the
/// unconditional `+= FLT_MIN` FFmpeg applies after either branch (keeps the
/// value strictly positive so later `q_m_max = q_m·gain_max/gain` divisions
/// in the limiter never divide by exactly zero).
pub(super) fn base_gain(
    e_orig: f32,
    e_curr: f32,
    q_mapped: f32,
    s_mapped: bool,
    delta: bool,
) -> f32 {
    let g = if s_mapped {
        sqrtf(e_orig * q_mapped / ((1.0 + e_curr) * (1.0 + q_mapped)))
    } else {
        let d = if delta { q_mapped } else { 0.0 };
        sqrtf(e_orig / ((1.0 + e_curr) * (1.0 + d)))
    };
    g + f32::MIN_POSITIVE
}

/// Per-channel HF-adjustment state carried across frames.
///
/// `[T; N]: Default` isn't implemented in `core` for `N > 32` (same
/// limitation `freq_tables::FreqTables`/`envelope::EnvQuant` already
/// document) — `g_temp`/`q_temp` at `48`/`42` both exceed it, so `Default`
/// is written by hand below instead of derived, matching this crate's
/// established convention for this exact situation.
#[derive(Clone, Copy)]
pub(super) struct HfAdjState {
    pub g_temp: [[f32; 48]; 42],
    pub q_temp: [[f32; 48]; 42],
    pub index_noise: u16, // 0..511, carried
    pub index_sine: u8,   // 0..3, carried
    pub l_a_prev: i8,
    /// FFmpeg `SBRData.t_env_num_env_old` (`aacsbr_template.c`: `dst->t_env_num_env_old
    /// = dst->t_env[dst->bs_num_env]`, set at the end of every `sbr_hf_assemble` call):
    /// the *previous* frame's trailing `t_env` border, in raw (non-RATE-scaled) slot
    /// units. Used by [`assemble`]'s ring re-prime step to locate the previous frame's
    /// last `h_SL` `g_temp`/`q_temp` rows (FIX #2).
    pub t_env_num_env_old: u8,
    /// FFmpeg `SBRData.s_indexmapped[0]` (`aacsbr_template.c` `sbr_mapping`, persisted
    /// via `memcpy(ch_data->s_indexmapped[0], ch_data->s_indexmapped[ch_data->bs_num_env], ...)`
    /// at the end of every call): whether the *previous* frame's LAST envelope had a
    /// sine mapped at each bin (`0..48`, relative to `kx`). Read by [`map_bands`] as the
    /// OR-term in the sine-mapping condition so a sine carries continuity across the
    /// frame boundary even for a bin whose *current*-frame envelope index is below
    /// `l_a` (FIX #1).
    pub s_index_carry: [u8; 48],
    /// Set by the frame driver ([`super`]) on a header-reset frame (or the
    /// very first SBR frame). Triggers [`assemble`]'s reset ring re-prime
    /// (FFmpeg `aacsbr.c` `sbr_hf_assemble`'s `if (sbr->reset)` branch,
    /// ~L309-313): the smoothing ring's low positions are primed from *this*
    /// frame's `gain[0]`/`q_m[0]` (there is no previous frame to continue
    /// from) instead of the normal carry from the previous frame's tail.
    /// Cleared once per [`hf_adjustment`] call after it is consumed.
    pub reset: bool,
}

impl Default for HfAdjState {
    fn default() -> Self {
        Self {
            g_temp: [[0.0; 48]; 42],
            q_temp: [[0.0; 48]; 42],
            index_noise: 0,
            index_sine: 0,
            // `-1` (FFmpeg's "no match" sentinel, same convention
            // `grid::SbrGrid::l_a` uses): on the very first call ever
            // (state fresh from `Default`), there is no previous frame, so
            // no envelope should be treated as a carried-over transient.
            l_a_prev: -1,
            t_env_num_env_old: 0,
            s_index_carry: [0u8; 48],
            reset: false,
        }
    }
}

/// §9.1 output: per-envelope, per-QMF-bin (relative to `kx`, `0..M`) mapped
/// values. Function-local scratch — sized past the `[T;N]:Default`-for-
/// `N<=32` limitation (`MAX_M`=49 > 32), but since this is never a `struct`
/// field on persisted state, a plain array-literal build (not `derive`)
/// sidesteps that limitation entirely.
struct Mapped {
    e_orig: [[f32; MAX_M]; MAX_ENV],
    q_mapped: [[f32; MAX_M]; MAX_ENV],
    /// Bin-level 0.0/1.0 flag (FFmpeg `s_indexmapped[e+1][m]`, used directly
    /// as a number in `s_m`'s `sqrt(temp·s_indexmapped)`) — distinct from
    /// `s_mapped` below, the per-*band* broadcast boolean the gain-branch
    /// select and `sqrtf` argument for `s_m` are computed from separately.
    s_index: [[f32; MAX_M]; MAX_ENV],
    s_mapped: [[bool; MAX_M]; MAX_ENV],
}

/// §9.1 `sbr_mapping`: spreads this frame's per-envelope-band energies onto
/// QMF bins. `s_index_carry` is FFmpeg's `s_indexmapped[0]` (see
/// [`HfAdjState::s_index_carry`]'s doc): the previous frame's LAST
/// envelope's per-bin sine map, ORed into every envelope's sine-mapping
/// condition here (Task 10 review FIX #1 — `task-10-report.md` Concern 1
/// previously simplified this term away as "always false").
#[allow(clippy::too_many_arguments)]
fn map_bands(
    grid: &SbrGrid,
    ft: &FreqTables,
    env: &EnvLinear,
    add_harmonic: &[u8; 48],
    s_index_carry: &[u8; 48],
    num_env: usize,
    kx: usize,
    m: usize,
) -> Result<Mapped, DecodeError> {
    let mut mp = Mapped {
        e_orig: [[0.0; MAX_M]; MAX_ENV],
        q_mapped: [[0.0; MAX_M]; MAX_ENV],
        s_index: [[0.0; MAX_M]; MAX_ENV],
        s_mapped: [[false; MAX_M]; MAX_ENV],
    };
    let n_q = ft.n_q as usize;
    if n_q == 0 || n_q > 5 {
        return Err(DecodeError::Malformed);
    }

    for e in 0..num_env {
        let res = grid.freq_res.get(e).copied().unwrap_or(0);
        let ilim = ft.n(res);
        if ilim == 0 || ilim > 48 {
            return Err(DecodeError::Malformed);
        }
        let table: &[u8; 64] = if res != 0 { &ft.f_high } else { &ft.f_low };

        // Envelope energy spread (§9.1 e_origmapped).
        for i in 0..ilim {
            let lo = *table.get(i).ok_or(DecodeError::Malformed)? as usize;
            let hi = *table.get(i + 1).ok_or(DecodeError::Malformed)? as usize;
            let val = env
                .env
                .get(e)
                .and_then(|row| row.get(i))
                .copied()
                .unwrap_or(0.0);
            for bin in lo..hi {
                if let Some(rel) = bin.checked_sub(kx) {
                    if rel < m {
                        mp.e_orig[e][rel] = val;
                    }
                }
            }
        }

        // Noise floor selection (§9.1: which of the up-to-2 decoded noise
        // envelopes covers this envelope's time span) + spread.
        let k_noise = usize::from(grid.num_noise > 1 && grid.t_env[e] >= grid.t_noise[1]);
        for i in 0..n_q {
            let lo = *ft.f_noise.get(i).ok_or(DecodeError::Malformed)? as usize;
            let hi = *ft.f_noise.get(i + 1).ok_or(DecodeError::Malformed)? as usize;
            let val = env
                .noise
                .get(k_noise)
                .and_then(|row| row.get(i))
                .copied()
                .unwrap_or(0.0);
            for bin in lo..hi {
                if let Some(rel) = bin.checked_sub(kx) {
                    if rel < m {
                        mp.q_mapped[e][rel] = val;
                    }
                }
            }
        }

        // Sine bin mapping — always computed on the HI-RES grid's midpoints
        // (§9.1), regardless of this envelope's own resolution.
        let n_high = (ft.n_high as usize).min(48);
        for i in 0..n_high {
            let f0 = *ft.f_high.get(i).ok_or(DecodeError::Malformed)? as usize;
            let f1 = *ft.f_high.get(i + 1).ok_or(DecodeError::Malformed)? as usize;
            let mid = (f0 + f1) >> 1;
            if let Some(rel) = mid.checked_sub(kx) {
                if rel < m {
                    let harmonic = add_harmonic.get(i).copied().unwrap_or(0);
                    // FIX #1: `e >= l_A` OR the previous frame's last
                    // envelope had a sine at this exact bin (FFmpeg
                    // `s_indexmapped[0][bin]==1`) — matches
                    // `aacsbr_template.c` `sbr_mapping`'s exact condition.
                    let carried = s_index_carry.get(rel).copied().unwrap_or(0) == 1;
                    if harmonic != 0 && (e as i8 >= grid.l_a || carried) {
                        mp.s_index[e][rel] = 1.0;
                    }
                }
            }
        }

        // Band-broadcast s_mapped (§9.1): 1 for every bin in a band (of
        // this envelope's OWN resolution table) if ANY bin in that band has
        // a mapped sine.
        for i in 0..ilim {
            let lo = *table.get(i).ok_or(DecodeError::Malformed)? as usize;
            let hi = *table.get(i + 1).ok_or(DecodeError::Malformed)? as usize;
            let mut any = false;
            for bin in lo..hi {
                if let Some(rel) = bin.checked_sub(kx) {
                    if rel < m && mp.s_index[e][rel] != 0.0 {
                        any = true;
                        break;
                    }
                }
            }
            for bin in lo..hi {
                if let Some(rel) = bin.checked_sub(kx) {
                    if rel < m {
                        mp.s_mapped[e][rel] = any;
                    }
                }
            }
        }
    }
    Ok(mp)
}

/// `Σ |X_high[band][slot]|²` for `slot` in `[ilb, iub)` (§9.2 `sum_square`).
fn sum_square(x_high: &[[Cpx; N_SYN]; MAX_SLOTS], band: usize, ilb: usize, iub: usize) -> f32 {
    let mut sum = 0.0f32;
    for slot in ilb..iub {
        if let Some(c) = x_high.get(slot).and_then(|row| row.get(band)) {
            sum += c.re * c.re + c.im * c.im;
        }
    }
    sum
}

/// §9.2 `sbr_env_estimate`: current (pre-gain) high-band energy per
/// envelope, either per QMF bin (`bs_interpol_freq` set) or per resolution
/// band, averaged over the band's bins and broadcast back to every bin in
/// it (`bs_interpol_freq` clear).
// `e`/`band` each index `e_curr` (a local 2D array, not an iterable this
// fn owns end-to-end) alongside several OTHER arrays (`grid.t_env`,
// `ft`/`table`, `x_high`) in lockstep -- clippy's `.iter_mut().enumerate()`
// suggestion only fits a single-array index, so plain range loops stay
// clearer here (same convention `envelope::read_env_noise` already uses).
#[allow(clippy::too_many_arguments, clippy::needless_range_loop)]
fn estimate_env(
    x_high: &[[Cpx; N_SYN]; MAX_SLOTS],
    grid: &SbrGrid,
    ft: &FreqTables,
    hdr: &SbrHeader,
    num_env: usize,
    kx: usize,
    m: usize,
) -> [[f32; MAX_M]; MAX_ENV] {
    let mut e_curr = [[0.0f32; MAX_M]; MAX_ENV];
    for e in 0..num_env {
        let t0 = grid.t_env[e] as usize;
        let t1 = grid.t_env.get(e + 1).copied().unwrap_or(0) as usize;
        if t1 <= t0 {
            continue;
        }
        let ilb = t0 * 2 + T_HFADJ;
        if ilb >= MAX_SLOTS {
            continue; // matches FFmpeg's early-return guard for this envelope
        }
        let iub = (t1 * 2 + T_HFADJ).min(MAX_SLOTS);

        if hdr.interpol_freq != 0 {
            let recip = 0.5 / (t1 - t0) as f32;
            for band in 0..m.min(MAX_M) {
                let sum = sum_square(x_high, kx + band, ilb, iub);
                e_curr[e][band] = sum * recip;
            }
        } else {
            let env_size = 2 * (t1 - t0);
            let res = grid.freq_res.get(e).copied().unwrap_or(0);
            let ilim = ft.n(res).min(48);
            let table: &[u8; 64] = if res != 0 { &ft.f_high } else { &ft.f_low };
            for p in 0..ilim {
                let lo = table.get(p).copied().unwrap_or(0) as usize;
                let hi = table.get(p + 1).copied().unwrap_or(0) as usize;
                if hi <= lo {
                    continue;
                }
                let den = (env_size * (hi - lo)) as f32;
                if den <= 0.0 {
                    continue;
                }
                let mut sum = 0.0f32;
                for k in lo..hi {
                    sum += sum_square(x_high, k, ilb, iub);
                }
                sum /= den;
                for k in lo..hi {
                    if let Some(rel) = k.checked_sub(kx) {
                        if rel < m {
                            e_curr[e][rel] = sum;
                        }
                    }
                }
            }
        }
    }
    e_curr
}

/// §9.3 output: per-envelope, per-bin gain + noise/sine levels.
struct GainBands {
    gain: [[f32; MAX_M]; MAX_ENV],
    q_m: [[f32; MAX_M]; MAX_ENV],
    s_m: [[f32; MAX_M]; MAX_ENV],
}

/// `limgain[bs_limiter_gains]` (§9.3): −3dB / 0dB / +3dB / limiter off.
const LIMGAIN: [f32; 4] = [0.707_95, 1.0, 1.412_54, 1.0e10];

/// §9.3 `sbr_gain_calc`: per limiter band, computes `q_m`/`s_m`/`gain` per
/// bin, then the `gain_max` limiter clamp, then `gain_boost` (energy-
/// matching boost, capped `1.584893192`), applied to `gain`/`q_m`/`s_m`
/// alike — transcribed field-for-field from live FFmpeg `aacsbr.c`
/// `sbr_gain_calc` (see module doc for the fetch/citation).
// See `estimate_env`'s doc comment for why the `band`-indexed loops here
// (`e_curr`/`mp.*`/`gb.*` all indexed in lockstep by the SAME `band`) keep
// plain range loops instead of clippy's single-array `.enumerate()` form.
#[allow(clippy::too_many_arguments, clippy::needless_range_loop)]
fn calc_gain(
    mp: &Mapped,
    e_curr: &[[f32; MAX_M]; MAX_ENV],
    grid: &SbrGrid,
    ft: &FreqTables,
    hdr: &SbrHeader,
    num_env: usize,
    m: usize,
    l_a_prev: i8,
) -> Result<GainBands, DecodeError> {
    let mut gb = GainBands {
        gain: [[0.0; MAX_M]; MAX_ENV],
        q_m: [[0.0; MAX_M]; MAX_ENV],
        s_m: [[0.0; MAX_M]; MAX_ENV],
    };
    let n_l = ft.n_l as usize;
    if n_l == 0 || n_l >= ft.f_lim.len() {
        return Err(DecodeError::Malformed);
    }
    let limgain = LIMGAIN[(hdr.limiter_gains as usize).min(3)];

    for e in 0..num_env {
        let delta = !(e as i8 == grid.l_a || e as i8 == l_a_prev);
        for k in 0..n_l {
            let lo = (*ft.f_lim.get(k).ok_or(DecodeError::Malformed)? as usize).min(m);
            let hi = (*ft.f_lim.get(k + 1).ok_or(DecodeError::Malformed)? as usize).min(m);
            if hi <= lo {
                continue;
            }

            // Pass 1: per-bin q_m/s_m/gain (§9.3).
            for band in lo..hi {
                let eo = mp.e_orig[e][band];
                let qmp = mp.q_mapped[e][band];
                let temp = eo / (1.0 + qmp);
                gb.q_m[e][band] = sqrtf(temp * qmp);
                gb.s_m[e][band] = sqrtf(temp * mp.s_index[e][band]);
                gb.gain[e][band] = base_gain(eo, e_curr[e][band], qmp, mp.s_mapped[e][band], delta);
            }

            // Pass 2: gain_max (limiter-band energy match).
            let mut sum0 = 0.0f32;
            let mut sum1 = 0.0f32;
            for band in lo..hi {
                sum0 += mp.e_orig[e][band];
                sum1 += e_curr[e][band];
            }
            let gain_max =
                (limgain * sqrtf((f32::EPSILON + sum0) / (f32::EPSILON + sum1))).min(1.0e5);

            // Pass 3: apply the limiter clamp.
            for band in lo..hi {
                let g = gb.gain[e][band];
                let q_m_max = gb.q_m[e][band] * gain_max / g;
                gb.q_m[e][band] = gb.q_m[e][band].min(q_m_max);
                gb.gain[e][band] = g.min(gain_max);
            }

            // Pass 4: gain_boost (re-matches energy after the clamp).
            sum0 = 0.0;
            sum1 = 0.0;
            for band in lo..hi {
                sum0 += mp.e_orig[e][band];
                let g = gb.gain[e][band];
                let s = gb.s_m[e][band];
                let q = gb.q_m[e][band];
                sum1 +=
                    e_curr[e][band] * g * g + s * s + if delta && s == 0.0 { q * q } else { 0.0 };
            }
            let gain_boost = sqrtf((f32::EPSILON + sum0) / (f32::EPSILON + sum1)).min(1.584_893_2);

            // Pass 5: apply the boost.
            for band in lo..hi {
                gb.gain[e][band] *= gain_boost;
                gb.q_m[e][band] *= gain_boost;
                gb.s_m[e][band] *= gain_boost;
            }
        }
    }
    Ok(gb)
}

/// §9.4 `h_smooth[5]` (faad2 lists it reversed; this is FFmpeg's order,
/// `h_smooth[0]` the current-slot tap).
const H_SMOOTH: [f32; 5] = [
    0.333_333_33,
    0.301_502_83,
    0.218_169_5,
    0.115_163_83,
    0.031_830_5,
];

/// The sine contribution `(Δre, Δim)` at absolute QMF band `kx+band` for the
/// current `index_sine` (0..3, masked by caller). Derived from live FFmpeg
/// `sbrdsp.c` `sbr_hf_apply_noise`/`sbrdsp_template.c`'s 4 `phi_sign0/1`
/// dispatch variants plus the `phi_sign1 = -phi_sign1` per-band flip (see
/// `task-10-report.md` for the full derivation): `index_sine` 0/2 add a
/// *constant* (over the whole band range) `±s` to the REAL component;
/// 1/3 add `±s` to the IMAGINARY component, with the sign alternating by
/// `(kx+band)` parity (`+1` when even, `-1` when odd, negated again for
/// `index_sine==3`) — independent of `kx`'s own parity once expressed this
/// way (verified algebraically: `phi_sign·(-1)^band` collapses to a
/// function of `(kx+band)` parity alone for both `kx` parities).
fn sine_delta(index_sine: u8, kx_even: bool, band: usize, s: f32) -> (f32, f32) {
    match index_sine & 3 {
        0 => (s, 0.0),
        2 => (-s, 0.0),
        idx => {
            let band_even = band % 2 == 0;
            let abs_even = kx_even == band_even; // (kx+band) even iff same parity
            let base_sign = if abs_even { 1.0 } else { -1.0 };
            let sign = if idx == 1 { base_sign } else { -base_sign };
            (0.0, s * sign)
        }
    }
}

/// §9.4 `sbr_hf_assemble`: re-primes the `g_temp`/`q_temp` smoothing rings'
/// low positions from the previous frame's tail (FIX #2), pushes this
/// frame's `gain`/`q_m` into those rings, then per slot/band applies the
/// (optionally 5-tap-smoothed) gain to `X_high`, adds pseudo-random noise
/// (`V_NOISE[index_noise]`) or the sine contribution ([`sine_delta`]),
/// following the `no_noise`/transient-envelope rule, and writes the result
/// into `y`. `index_noise`/`index_sine`/`t_env_num_env_old` carry across
/// frames via `st`.
///
/// **Ring re-prime** (Task 10 review FIX #2 — `task-10-report.md` Concern
/// 2 previously left this ring un-primed, permanently stuck at
/// `Default`-zero in the low rows): live FFmpeg copies the previous
/// frame's last `h_SL` `g_temp`/`q_temp` rows (keyed by
/// `st.t_env_num_env_old`, the previous frame's trailing `t_env` border)
/// down into this frame's low ring positions *before* the push loop below
/// overwrites the rest of the ring, so the smoothing filter's lookback
/// taps at the very start of a frame see genuine continuation instead of
/// zero.
///
/// **`y` is [`MAX_Y_SLOTS`] (=38) slots wide**, matching FFmpeg's own 38-wide
/// `Y1` buffer: a single envelope's raw span (`2·t_env[e]..2·t_env[e+1]`) can
/// reach up to 38 (FIXVAR/VARVAR's trailing border can exceed the nominal
/// 16-slot frame). Synthesis only emits the first 32 slots; slots `32..38`
/// are the frame-boundary overlap the T11 driver stitches into the *next*
/// frame's splice (§10 `i_Temp`/`Ypos` ping-pong) — every slot is written
/// here so none of that overlap is lost.
#[allow(clippy::too_many_arguments)]
fn assemble(
    x_high: &[[Cpx; N_SYN]; MAX_SLOTS],
    grid: &SbrGrid,
    hdr: &SbrHeader,
    gb: &GainBands,
    num_env: usize,
    kx: usize,
    m: usize,
    l_a_prev: i8,
    st: &mut HfAdjState,
    y: &mut [[Cpx; N_SYN]; MAX_Y_SLOTS],
) {
    let h_sl: usize = if hdr.smoothing_mode != 0 { 0 } else { 4 };
    let m_cols = m.min(RING_COLS);

    // Ring re-prime BEFORE the push loop writes this frame's own data, so the
    // smoothing FIR's lookback taps at the start of the frame see genuine
    // continuation rather than the `Default`-zero the low rows would otherwise
    // never be written to. Two cases, matching FFmpeg `aacsbr.c`
    // `sbr_hf_assemble` ~L309-323:
    //   * RESET frame (or the first SBR frame): there is no previous frame, so
    //     prime the `h_sl` low rows from THIS frame's `gain[0]`/`q_m[0]`
    //     (FFmpeg's `if (sbr->reset)` branch, ~L309-313).
    //   * otherwise (FIX #2): copy the previous frame's last `h_sl` rows
    //     (keyed by `t_env_num_env_old`) down into this frame's low positions
    //     (`else if (h_SL)`, ~L314-323).
    // Every index is bounds-checked against `RING_LEN` (worst case
    // `2*19 + 3 = 41 < 42`, see `RING_LEN`'s doc) before either side is touched.
    if h_sl != 0 {
        let t_new = grid.t_env[0] as usize;
        if st.reset {
            for j in 0..h_sl {
                let dst = 2 * t_new + j;
                if dst < RING_LEN {
                    st.g_temp[dst][..m_cols].copy_from_slice(&gb.gain[0][..m_cols]);
                    st.q_temp[dst][..m_cols].copy_from_slice(&gb.q_m[0][..m_cols]);
                }
            }
        } else {
            let t_old = st.t_env_num_env_old as usize;
            for j in 0..h_sl {
                let src = 2 * t_old + j;
                let dst = 2 * t_new + j;
                if src < RING_LEN && dst < RING_LEN {
                    let g_row = st.g_temp[src];
                    let q_row = st.q_temp[src];
                    st.g_temp[dst] = g_row;
                    st.q_temp[dst] = q_row;
                }
            }
        }
    }

    // Push this frame's raw gain/q_m into the ring at physical index h_sl+i.
    for e in 0..num_env {
        let t0 = grid.t_env[e] as usize;
        let t1 = grid.t_env.get(e + 1).copied().unwrap_or(t0 as u8) as usize;
        for i in (2 * t0)..(2 * t1) {
            let idx = h_sl + i;
            if idx >= RING_LEN {
                continue;
            }
            st.g_temp[idx][..m_cols].copy_from_slice(&gb.gain[e][..m_cols]);
            st.q_temp[idx][..m_cols].copy_from_slice(&gb.q_m[e][..m_cols]);
        }
    }

    let mut index_noise = st.index_noise;
    let mut index_sine = st.index_sine;
    let kx_even = kx % 2 == 0;

    for e in 0..num_env {
        let is_transient = e as i8 == grid.l_a || e as i8 == l_a_prev;
        let smooth = h_sl != 0 && !is_transient;
        let t0 = grid.t_env[e] as usize;
        let t1 = grid.t_env.get(e + 1).copied().unwrap_or(t0 as u8) as usize;

        for i in (2 * t0)..(2 * t1) {
            let idx1 = h_sl + i;
            // `Y` now holds all `MAX_Y_SLOTS` (=38) slots (was truncated at 32
            // pre-T11): slots `32..38` are the frame-boundary overlap the driver
            // stitches into the next frame's splice (§10 `i_Temp`/`Ypos`).
            let write_slot = i < MAX_Y_SLOTS;
            let phys = i + T_HFADJ;

            for band in 0..m.min(MAX_M) {
                let (g_filt, q_filt) = if smooth {
                    let mut gf = 0.0f32;
                    let mut qf = 0.0f32;
                    for (j, &h) in H_SMOOTH.iter().enumerate() {
                        if idx1 < j {
                            continue;
                        }
                        let src = idx1 - j;
                        if let Some(row) = st.g_temp.get(src) {
                            if let Some(&v) = row.get(band) {
                                gf += v * h;
                            }
                        }
                        if let Some(row) = st.q_temp.get(src) {
                            if let Some(&v) = row.get(band) {
                                qf += v * h;
                            }
                        }
                    }
                    (gf, qf)
                } else {
                    let g = st
                        .g_temp
                        .get(idx1)
                        .and_then(|row| row.get(band))
                        .copied()
                        .unwrap_or(0.0);
                    // FIX C2: a transient envelope replaces noise with the
                    // sine entirely (`q_filt` unused, so 0.0 is fine), but a
                    // NON-transient envelope with smoothing off (`h_sl==0`,
                    // so `idx1 == i` here) must still carry its real noise
                    // floor -- FFmpeg's non-smoothing branch is
                    // `q_filt = q_temp[i]` (`aacsbr.c` `sbr_hf_assemble`
                    // ~L328/352), not an unconditional 0. Forcing 0.0 here
                    // unconditionally silently dropped the noise floor on
                    // every `bs_smoothing_mode=1` non-transient frame.
                    let q = if is_transient {
                        0.0
                    } else {
                        st.q_temp
                            .get(idx1)
                            .and_then(|row| row.get(band))
                            .copied()
                            .unwrap_or(0.0)
                    };
                    (g, q)
                };

                // FIX C1: the noise index advances once per (slot, band)
                // for EVERY envelope, transient or not -- matches live
                // FFmpeg's unconditional `indexnoise = (indexnoise +
                // m_max) & 0x1ff;` per slot (`aacsbr.c` `sbr_hf_assemble`
                // ~L375, outside the transient if/else), which decomposed
                // per-band (this file's per-band loop, rather than
                // FFmpeg's single per-slot `+= m_max`) is exactly a `+1`
                // advance for every band regardless of transient status.
                // Previously gating this behind `!is_transient` meant a
                // transient envelope's slots advanced the index by 0
                // instead of `m_max`, permanently desyncing the
                // frame-to-frame PRNG noise walk (`st.index_noise`) from
                // FFmpeg's for every frame containing a transient.
                index_noise = (index_noise + 1) & 0x1ff;

                if write_slot && phys < MAX_SLOTS {
                    let abs_band = kx + band;
                    if let Some(xh) = x_high.get(phys).and_then(|row| row.get(abs_band)) {
                        let mut re = g_filt * xh.re;
                        let mut im = g_filt * xh.im;

                        let s = gb.s_m[e][band];
                        if is_transient || s != 0.0 {
                            let (dre, dim) = sine_delta(index_sine, kx_even, band, s);
                            re += dre;
                            im += dim;
                        } else {
                            let idx = (index_noise as usize) & 511;
                            if let Some(&[nre, nim]) = V_NOISE.get(idx) {
                                re += q_filt * nre;
                                im += q_filt * nim;
                            }
                        }

                        if let Some(yc) = y.get_mut(i).and_then(|row| row.get_mut(abs_band)) {
                            yc.re = re;
                            yc.im = im;
                        }
                    }
                }
            }
            index_sine = (index_sine + 1) & 3;
        }
    }

    st.index_noise = index_noise;
    st.index_sine = index_sine;
    // Next frame's l_a_prev (FFmpeg `e_a[0] = -(e_a[1] != bs_num_env_old)`):
    // 0 (matches this frame's envelope 0) iff this frame's l_a pointed
    // exactly at its own num_env (the transient ran to the frame boundary),
    // else -1 (no match, matching `grid::SbrGrid::l_a`'s own sentinel).
    st.l_a_prev = if grid.l_a == grid.num_env as i8 {
        0
    } else {
        -1
    };
    // FIX #2: this frame's trailing `t_env` border becomes next frame's
    // re-prime source key (FFmpeg `dst->t_env_num_env_old =
    // dst->t_env[dst->bs_num_env]`, `aacsbr_template.c`).
    st.t_env_num_env_old = grid.t_env[num_env];
}

/// Full HF adjustment (§9.1–§9.4): map envelope/noise onto bins, estimate
/// the current envelope, compute gains + limiter, and assemble `Y` (gain +
/// smoothed noise + sine) for the current frame. Fills `y[slot][band]`,
/// band `kx..kx+M-1`. Validates the shared bounds every §9 stage relies on
/// once up front (`M`, `num_env`, the limiter table, `f_high[0]==f_low[0]
/// ==kx` — the last mirrors FFmpeg `sbr_mapping`'s own explicit sanity
/// check on the frequency tables) rather than re-checking in each stage.
// 8 parameters: the task brief's mandated interface (this is the exact
// signature it specifies) -- not something to restructure into a params
// struct here, matching `hfgen::hf_generation`'s identical precedent.
#[allow(clippy::too_many_arguments)]
pub(super) fn hf_adjustment(
    x_high: &[[Cpx; N_SYN]; MAX_SLOTS],
    env: &EnvLinear,
    grid: &SbrGrid,
    ft: &FreqTables,
    hdr: &SbrHeader,
    add_harmonic: &[u8; 48],
    st: &mut HfAdjState,
    y: &mut [[Cpx; N_SYN]; MAX_Y_SLOTS],
) -> Result<(), DecodeError> {
    let kx = ft.kx as usize;
    let m = ft.m as usize;
    if m == 0 || m > MAX_M || kx + m > 64 {
        return Err(DecodeError::Malformed);
    }
    if ft.f_high[0] as usize != kx || ft.f_low[0] as usize != kx {
        return Err(DecodeError::Malformed);
    }
    let num_env = grid.num_env as usize;
    if num_env == 0 || num_env > MAX_ENV {
        return Err(DecodeError::Malformed);
    }

    // Read before this call's own update below (§9.3/§9.4 both compare
    // against the value carried IN from the previous frame).
    let l_a_prev_entry = st.l_a_prev;
    let s_index_carry_entry = st.s_index_carry;

    let mp = map_bands(
        grid,
        ft,
        env,
        add_harmonic,
        &s_index_carry_entry,
        num_env,
        kx,
        m,
    )?;
    // FIX #1: persist the LAST envelope's per-bin sine map for the next
    // frame's carry-in (FFmpeg `memcpy(ch_data->s_indexmapped[0],
    // ch_data->s_indexmapped[ch_data->bs_num_env], ...)`, `aacsbr_template.c`
    // `sbr_mapping` tail). `num_env >= 1` is already validated above.
    for (dst, &v) in st
        .s_index_carry
        .iter_mut()
        .zip(mp.s_index[num_env - 1].iter())
    {
        *dst = u8::from(v != 0.0);
    }
    let e_curr = estimate_env(x_high, grid, ft, hdr, num_env, kx, m);
    let gb = calc_gain(&mp, &e_curr, grid, ft, hdr, num_env, m, l_a_prev_entry)?;
    assemble(
        x_high,
        grid,
        hdr,
        &gb,
        num_env,
        kx,
        m,
        l_a_prev_entry,
        st,
        y,
    );

    // The reset ring re-prime is a one-shot: consumed by this frame's
    // `assemble` above, cleared so the next frame uses the normal FIX #2 carry.
    st.reset = false;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // The brief's own literal pinned value (`1.4142135`, one digit short of
    // `core::f32::consts::SQRT_2`'s full precision) -- clippy would rather
    // this used the named constant, but the test is transcribed verbatim
    // from the task brief's mandated Step-1 text.
    #[allow(clippy::approx_constant)]
    fn sqrtf_known_values() {
        assert!((sqrtf(4.0) - 2.0).abs() < 1e-4);
        assert!((sqrtf(2.0) - 1.4142135).abs() < 1e-4);
        assert_eq!(sqrtf(0.0), 0.0);
        assert_eq!(sqrtf(-1.0), 0.0); // guarded
    }

    #[test]
    fn base_gain_no_noise_no_sine() {
        // q_mapped=0, s_mapped=false, delta=true, e_curr=e_orig=3
        // -> sqrt(3 / ((1+3)(1+0))) = sqrt(0.75) ~= 0.8660254.
        assert!((base_gain(3.0, 3.0, 0.0, false, true) - 0.8660254).abs() < 1e-4);
    }

    #[test]
    fn base_gain_with_sine() {
        // s_mapped=true: sqrt(e_orig*q_mapped / ((1+e_curr)(1+q_mapped)))
        // e_orig=4, e_curr=1, q_mapped=1 -> sqrt(4*1/((2)(2))) = sqrt(1) = 1.
        assert!((base_gain(4.0, 1.0, 1.0, true, false) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn sqrtf_matches_std_across_a_wide_range() {
        // Broader regression beyond the brief's 2 pinned values: every
        // power-of-two-ish magnitude across the practical f32 domain, to
        // catch a seed/iteration-count regression the two pinned values
        // alone might not exercise (both 2.0 and 4.0 happen to converge in
        // zero extra NR steps -- see the fn doc's worked derivation).
        let cases = [
            0.001_f32, 0.1, 0.5, 1.0, 3.0, 10.0, 123.456, 1.0e6, 1.0e10, 1.0e20,
        ];
        for &x in &cases {
            let got = sqrtf(x);
            let want = (x as f64).sqrt() as f32;
            let rel = ((got - want) / want).abs();
            assert!(
                rel < 1e-5,
                "sqrtf({x}) = {got}, want ~{want} (rel err {rel})"
            );
        }
    }

    #[test]
    fn sine_delta_matches_expected_phase_pattern() {
        // index_sine=0: purely real +s, no kx/band dependence.
        assert_eq!(sine_delta(0, true, 0, 2.0), (2.0, 0.0));
        assert_eq!(sine_delta(0, false, 3, 2.0), (2.0, 0.0));
        // index_sine=2: purely real -s.
        assert_eq!(sine_delta(2, true, 1, 3.0), (-3.0, 0.0));
        // index_sine=1: purely imaginary, sign = +1 if (kx+band) even else -1.
        assert_eq!(sine_delta(1, true, 0, 1.0), (0.0, 1.0)); // kx even, band even
        assert_eq!(sine_delta(1, true, 1, 1.0), (0.0, -1.0)); // kx even, band odd
        assert_eq!(sine_delta(1, false, 0, 1.0), (0.0, -1.0)); // kx odd, band even
        assert_eq!(sine_delta(1, false, 1, 1.0), (0.0, 1.0)); // kx odd, band odd
        // index_sine=3: exact negation of index_sine=1.
        assert_eq!(sine_delta(3, true, 0, 1.0), (0.0, -1.0));
        assert_eq!(sine_delta(3, true, 1, 1.0), (0.0, 1.0));
    }

    fn test_header() -> SbrHeader {
        SbrHeader {
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
        }
    }

    fn synthetic_x_high() -> [[Cpx; N_SYN]; MAX_SLOTS] {
        let mut x = [[Cpx::default(); N_SYN]; MAX_SLOTS];
        for (phys, slot) in x.iter_mut().enumerate() {
            for (band, c) in slot.iter_mut().enumerate() {
                let t = phys as f64;
                let b = band as f64;
                c.re = (0.31 * t + 0.11 * b).sin() as f32 * 50.0;
                c.im = (0.19 * t - 0.07 * b).cos() as f32 * 50.0;
            }
        }
        x
    }

    #[test]
    fn hf_adjustment_no_panic_fills_y_and_advances_state() {
        use crate::formats::aac::sbr::freq_tables::{derive, derive_limiter};
        use crate::formats::aac::sbr::grid::FIXFIX;

        let hdr = test_header();
        let mut ft = derive(&hdr, 44100).unwrap();
        let patch_borders = [ft.kx, ft.kx + 9, ft.kx + ft.m];
        derive_limiter(&mut ft, &patch_borders).unwrap();
        assert!(ft.n_l >= 1);

        let grid = SbrGrid {
            frame_class: FIXFIX,
            num_env: 1,
            num_noise: 1,
            t_env: [0, 16, 0, 0, 0, 0],
            t_noise: [0, 16, 0],
            freq_res: [1, 0, 0, 0, 0, 0], // HI-RES
            pointer: 0,
            l_a: -1,
        };

        let mut env = EnvLinear::default();
        for i in 0..(ft.n_high as usize) {
            env.env[0][i] = 100.0;
        }
        for i in 0..(ft.n_q as usize) {
            env.noise[0][i] = 1.0;
        }

        let x_high = synthetic_x_high();
        let add_harmonic = [0u8; 48];
        let mut st = HfAdjState::default();
        let mut y = [[Cpx::default(); N_SYN]; MAX_Y_SLOTS];

        hf_adjustment(
            &x_high,
            &env,
            &grid,
            &ft,
            &hdr,
            &add_harmonic,
            &mut st,
            &mut y,
        )
        .unwrap();

        let mut any_nonzero = false;
        for slot in &y {
            for &c in &slot[ft.kx as usize..(ft.kx + ft.m) as usize] {
                if c != Cpx::default() {
                    any_nonzero = true;
                }
            }
        }
        assert!(any_nonzero, "hf_adjustment produced no output in kx..kx+M");

        let noise_after_1 = st.index_noise;
        let sine_after_1 = st.index_sine;
        assert!(noise_after_1 > 0, "index_noise must advance");

        // Second call: state must keep advancing (carried across frames),
        // and must still not panic on a repeat call reusing `st`.
        let mut y2 = [[Cpx::default(); N_SYN]; MAX_Y_SLOTS];
        hf_adjustment(
            &x_high,
            &env,
            &grid,
            &ft,
            &hdr,
            &add_harmonic,
            &mut st,
            &mut y2,
        )
        .unwrap();
        assert_ne!(
            (st.index_noise, st.index_sine),
            (noise_after_1, sine_after_1),
            "index_noise/index_sine must keep advancing across calls"
        );
    }

    #[test]
    fn hf_adjustment_rejects_inconsistent_freq_tables() {
        // f_high[0] != kx must be rejected (mirrors FFmpeg's own sanity
        // check in sbr_mapping) rather than silently mis-indexing.
        let hdr = test_header();
        let mut ft = FreqTables {
            kx: 17,
            m: 20,
            n_l: 1,
            ..FreqTables::default()
        };
        ft.f_lim[1] = 20;
        // f_high[0] left at 0 (default), which != kx(17).
        let grid = SbrGrid {
            frame_class: 0,
            num_env: 1,
            num_noise: 1,
            t_env: [0, 16, 0, 0, 0, 0],
            t_noise: [0, 16, 0],
            freq_res: [1, 0, 0, 0, 0, 0],
            pointer: 0,
            l_a: -1,
        };
        let env = EnvLinear::default();
        let x_high = [[Cpx::default(); N_SYN]; MAX_SLOTS];
        let add_harmonic = [0u8; 48];
        let mut st = HfAdjState::default();
        let mut y = [[Cpx::default(); N_SYN]; MAX_Y_SLOTS];
        let r = hf_adjustment(
            &x_high,
            &env,
            &grid,
            &ft,
            &hdr,
            &add_harmonic,
            &mut st,
            &mut y,
        );
        assert!(r.is_err());
    }

    // --- Task 10 review regression tests (FIX C1/C2/#2/#1) -----------------
    //
    // The pre-existing integration test above uses `smoothing_mode=1` +
    // `l_a=-1` (non-transient throughout), which dodges every one of the 4
    // bugs fixed below: with `l_a=-1` there's no transient envelope for
    // FIX C1 to matter on, and while it DOES exercise `smoothing_mode=1`
    // (so it happens to touch the FIX C2 code path), it only ever asserts
    // "`y` has some nonzero cell" -- true from gain alone, so it can't
    // distinguish "noise was added" from "noise was silently dropped".
    // Each test below isolates exactly one fix.

    #[test]
    fn c2_smoothing_off_still_adds_noise_floor() {
        // FIX C2: with `bs_smoothing_mode=1` (h_sl=0) and a non-transient
        // envelope, the noise floor (`q_temp`) must still be added, not
        // forced to 0. Pre-fix, `assemble`'s non-smoothing else-branch
        // hard-coded `q_filt=0.0` for every envelope (transient or not),
        // so `actual_re` below would equal `expected_gain_only_re` exactly
        // -- this test's `assert_ne!` is precisely what catches that.
        use crate::formats::aac::sbr::freq_tables::{derive, derive_limiter};
        use crate::formats::aac::sbr::grid::FIXFIX;

        let hdr = test_header(); // smoothing_mode = 1 (off)
        let mut ft = derive(&hdr, 44100).unwrap();
        let patch_borders = [ft.kx, ft.kx + 9, ft.kx + ft.m];
        derive_limiter(&mut ft, &patch_borders).unwrap();

        let grid = SbrGrid {
            frame_class: FIXFIX,
            num_env: 1,
            num_noise: 1,
            t_env: [0, 16, 0, 0, 0, 0],
            t_noise: [0, 16, 0],
            freq_res: [1, 0, 0, 0, 0, 0], // HI-RES
            pointer: 0,
            l_a: -1, // non-transient
        };

        let mut env = EnvLinear::default();
        for i in 0..(ft.n_high as usize) {
            env.env[0][i] = 100.0;
        }
        for i in 0..(ft.n_q as usize) {
            env.noise[0][i] = 5.0; // nonzero noise floor
        }

        let x_high = synthetic_x_high();
        let add_harmonic = [0u8; 48]; // no sine anywhere -> noise path taken
        let mut st = HfAdjState::default();
        let mut y = [[Cpx::default(); N_SYN]; MAX_Y_SLOTS];

        hf_adjustment(
            &x_high,
            &env,
            &grid,
            &ft,
            &hdr,
            &add_harmonic,
            &mut st,
            &mut y,
        )
        .unwrap();

        // Slot 0 / band 0: with h_sl=0, idx1==i==0, so the ring row `st.
        // g_temp[0]`/`st.q_temp[0]` (written by this same call's push loop)
        // holds exactly the `gain`/`q_m` used to produce y[0][kx].
        let kx = ft.kx as usize;
        let xh = x_high[T_HFADJ][kx];
        let gain0 = st.g_temp[0][0];
        let q0 = st.q_temp[0][0];
        assert!(
            q0 > 0.0,
            "test setup must have a nonzero noise floor (q_m) to be meaningful"
        );

        let expected_gain_only_re = gain0 * xh.re;
        let expected_gain_only_im = gain0 * xh.im;
        let actual = y[0][kx];

        assert_ne!(
            (actual.re, actual.im),
            (expected_gain_only_re, expected_gain_only_im),
            "smoothing-off (h_sl=0), non-transient slot must still add the \
             noise floor (q_filt*V_NOISE), not just gain*X_high -- FIX C2"
        );
    }

    #[test]
    fn c1_transient_envelope_still_advances_index_noise() {
        // FIX C1: a transient envelope (`e == l_a`) must still advance
        // `index_noise` by `m_max` per slot, exactly like a non-transient
        // one -- FFmpeg's `indexnoise = (indexnoise + m_max) & 0x1ff;` runs
        // unconditionally, outside the transient if/else. Pre-fix, the
        // advance was gated behind `if !is_transient`, so a fully-transient
        // frame (l_a=0, single envelope) left `index_noise` at 0.
        use crate::formats::aac::sbr::freq_tables::{derive, derive_limiter};
        use crate::formats::aac::sbr::grid::FIXFIX;

        let hdr = test_header();
        let mut ft = derive(&hdr, 44100).unwrap();
        let patch_borders = [ft.kx, ft.kx + 9, ft.kx + ft.m];
        derive_limiter(&mut ft, &patch_borders).unwrap();

        let grid = SbrGrid {
            frame_class: FIXFIX,
            num_env: 1,
            num_noise: 1,
            t_env: [0, 16, 0, 0, 0, 0],
            t_noise: [0, 16, 0],
            freq_res: [1, 0, 0, 0, 0, 0],
            pointer: 0,
            l_a: 0, // envelope 0 IS the transient
        };

        let mut env = EnvLinear::default();
        for i in 0..(ft.n_high as usize) {
            env.env[0][i] = 100.0;
        }
        for i in 0..(ft.n_q as usize) {
            env.noise[0][i] = 1.0;
        }

        let x_high = synthetic_x_high();
        let add_harmonic = [0u8; 48];
        let mut st = HfAdjState::default();
        let mut y = [[Cpx::default(); N_SYN]; MAX_Y_SLOTS];

        hf_adjustment(
            &x_high,
            &env,
            &grid,
            &ft,
            &hdr,
            &add_harmonic,
            &mut st,
            &mut y,
        )
        .unwrap();

        let m = ft.m as usize;
        let slots = 2 * (grid.t_env[1] as usize - grid.t_env[0] as usize); // 32
        let expected = ((m * slots) as u32 & 0x1ff) as u16;
        assert!(
            expected > 0,
            "test setup must produce a nonzero expected advance"
        );
        assert_eq!(
            st.index_noise, expected,
            "a fully-transient envelope must still advance index_noise by \
             m_max per slot -- FIX C1 (pre-fix this stayed 0)"
        );
    }

    #[test]
    fn ring_reprime_second_frame_low_rows_not_left_at_zero() {
        // FIX #2: smoothing ON (h_sl=4) across two consecutive frames with
        // an otherwise-identical grid/envelope/X_high (so both frames'
        // computed gain is the same) must re-prime the ring's low rows
        // (`g_temp[0..4]`) from the previous frame's tail before the
        // second frame's smoothing FIR reads them. Pre-fix, `g_temp[0..4]`
        // was NEVER written by any push loop (every frame's push starts at
        // `h_sl + 2*t_env[0] = 4`), so it stayed at `Default`-zero forever,
        // even on the second (or 100th) frame -- this test's per-row
        // `assert_eq!` against the real (nonzero) gain row is exactly what
        // catches that.
        use crate::formats::aac::sbr::freq_tables::{derive, derive_limiter};
        use crate::formats::aac::sbr::grid::FIXFIX;

        let mut hdr = test_header();
        hdr.smoothing_mode = 0; // smoothing ON -> h_sl = 4
        let mut ft = derive(&hdr, 44100).unwrap();
        let patch_borders = [ft.kx, ft.kx + 9, ft.kx + ft.m];
        derive_limiter(&mut ft, &patch_borders).unwrap();

        let grid = SbrGrid {
            frame_class: FIXFIX,
            num_env: 1,
            num_noise: 1,
            t_env: [0, 16, 0, 0, 0, 0],
            t_noise: [0, 16, 0],
            freq_res: [1, 0, 0, 0, 0, 0],
            pointer: 0,
            l_a: -1, // non-transient -> smoothing actually engages
        };

        let mut env = EnvLinear::default();
        for i in 0..(ft.n_high as usize) {
            env.env[0][i] = 100.0;
        }
        for i in 0..(ft.n_q as usize) {
            env.noise[0][i] = 1.0;
        }

        let x_high = synthetic_x_high();
        let add_harmonic = [0u8; 48];
        let mut st = HfAdjState::default();

        let mut y1 = [[Cpx::default(); N_SYN]; MAX_Y_SLOTS];
        hf_adjustment(
            &x_high,
            &env,
            &grid,
            &ft,
            &hdr,
            &add_harmonic,
            &mut st,
            &mut y1,
        )
        .unwrap(); // frame 1

        let mut y2 = [[Cpx::default(); N_SYN]; MAX_Y_SLOTS];
        hf_adjustment(
            &x_high,
            &env,
            &grid,
            &ft,
            &hdr,
            &add_harmonic,
            &mut st,
            &mut y2,
        )
        .unwrap(); // frame 2 -- identical inputs, so gain is unchanged

        let m_cols = (ft.m as usize).min(48);
        // Row `h_sl` (=4) is this (2nd) frame's own just-pushed gain row --
        // known-good, always written, and (since inputs are identical
        // across frames) numerically equal to what rows 0..4 SHOULD hold
        // once correctly re-primed from frame 1's tail.
        let real_gain_row = st.g_temp[4];
        assert!(
            real_gain_row[..m_cols].iter().any(|&v| v != 0.0),
            "test setup sanity: the real gain row must be nonzero"
        );
        for j in 0..4 {
            assert_eq!(
                st.g_temp[j][..m_cols],
                real_gain_row[..m_cols],
                "ring row {j} must be re-primed from the previous frame's \
                 tail after the 2nd frame, not left at the Default-zero \
                 value -- FIX #2"
            );
        }
    }

    #[test]
    fn sine_carry_lets_early_envelope_inherit_prev_frame_sine() {
        // FIX #1: `map_bands`'s sine-mapping condition must OR in
        // `s_index_carry` (the previous frame's LAST envelope's sine map),
        // not just `e >= l_a`. This only matters for an envelope BELOW a
        // mid-frame transient (`e < l_a`) whose bin also had a sine at the
        // end of the previous frame. Tested at two levels: the full
        // `hf_adjustment` entry point primes `st.s_index_carry` across two
        // real calls; `map_bands` is then re-invoked directly (with a
        // "control" no-carry variant) to inspect the otherwise-private
        // `s_index`/`s_m` intermediates the fix changes.
        use crate::formats::aac::sbr::freq_tables::{derive, derive_limiter};
        use crate::formats::aac::sbr::grid::FIXFIX;

        let hdr = test_header();
        let mut ft = derive(&hdr, 44100).unwrap();
        let patch_borders = [ft.kx, ft.kx + 9, ft.kx + ft.m];
        derive_limiter(&mut ft, &patch_borders).unwrap();
        let kx = ft.kx as usize;

        // The QMF bin under test: the midpoint of the first HI-RES band.
        let f0 = ft.f_high[0] as usize;
        let f1 = ft.f_high[1] as usize;
        let rel = ((f0 + f1) >> 1) - kx;

        let mut add_harmonic = [0u8; 48];
        add_harmonic[0] = 1;

        let mut env = EnvLinear::default();
        for i in 0..(ft.n_high as usize) {
            env.env[0][i] = 100.0;
            env.env[1][i] = 100.0;
        }

        let x_high = synthetic_x_high();
        let mut st = HfAdjState::default();

        // Frame 1: single envelope, l_a=0 -> e(0) >= l_a(0), so the sine
        // maps unconditionally (no carry needed yet) -- this primes
        // `st.s_index_carry` for frame 2.
        let grid1 = SbrGrid {
            frame_class: FIXFIX,
            num_env: 1,
            num_noise: 1,
            t_env: [0, 16, 0, 0, 0, 0],
            t_noise: [0, 16, 0],
            freq_res: [1, 0, 0, 0, 0, 0],
            pointer: 0,
            l_a: 0,
        };
        let mut y1 = [[Cpx::default(); N_SYN]; MAX_Y_SLOTS];
        hf_adjustment(
            &x_high,
            &env,
            &grid1,
            &ft,
            &hdr,
            &add_harmonic,
            &mut st,
            &mut y1,
        )
        .unwrap();
        assert_eq!(
            st.s_index_carry[rel], 1,
            "test setup sanity: frame 1's last envelope must have mapped \
             the sine at this bin"
        );

        // Frame 2: two envelopes, l_a=1 -> envelope 0 (e=0) is BELOW l_a,
        // so without the carry OR-term it would get no sine at all.
        let grid2 = SbrGrid {
            frame_class: FIXFIX,
            num_env: 2,
            num_noise: 1,
            t_env: [0, 8, 16, 0, 0, 0],
            t_noise: [0, 16, 0],
            freq_res: [1, 1, 0, 0, 0, 0],
            pointer: 0,
            l_a: 1,
        };

        let carry_after_frame1 = st.s_index_carry;
        let l_a_prev_after_frame1 = st.l_a_prev;
        let m = ft.m as usize;

        // Reconstructs exactly what `hf_adjustment` computed internally
        // for frame 2 (same pure functions, same inputs) so the test can
        // inspect `s_index`/`s_m`, which `hf_adjustment` doesn't expose.
        let mp_with_carry = map_bands(
            &grid2,
            &ft,
            &env,
            &add_harmonic,
            &carry_after_frame1,
            2,
            kx,
            m,
        )
        .unwrap();
        let e_curr2 = estimate_env(&x_high, &grid2, &ft, &hdr, 2, kx, m);
        let gb_with_carry = calc_gain(
            &mp_with_carry,
            &e_curr2,
            &grid2,
            &ft,
            &hdr,
            2,
            m,
            l_a_prev_after_frame1,
        )
        .unwrap();

        // Control: same frame 2, but with NO carry (as if FIX #1 weren't
        // applied) -- isolates the OR-term's effect from any other
        // per-frame variation.
        let no_carry = [0u8; 48];
        let mp_no_carry = map_bands(&grid2, &ft, &env, &add_harmonic, &no_carry, 2, kx, m).unwrap();
        let gb_no_carry = calc_gain(
            &mp_no_carry,
            &e_curr2,
            &grid2,
            &ft,
            &hdr,
            2,
            m,
            l_a_prev_after_frame1,
        )
        .unwrap();

        assert_eq!(
            mp_no_carry.s_index[0][rel], 0.0,
            "control sanity: without the carry, envelope 0 (below l_a) gets no sine at this bin"
        );
        assert_eq!(
            mp_with_carry.s_index[0][rel], 1.0,
            "FIX #1: the carried-over prev-frame sine must still map onto \
             envelope 0 even though e < l_a"
        );
        assert_eq!(
            gb_no_carry.s_m[0][rel], 0.0,
            "control sanity: without the carry, s_m for envelope 0 stays zero"
        );
        assert!(
            gb_with_carry.s_m[0][rel] > 0.0,
            "FIX #1: s_m (sine level) for envelope 0 must be nonzero once \
             the carry is honored"
        );
    }
}
