//! `sbr-reference.md` §8 — HF generation: the 2nd-order complex LPC inverse
//! filter per low QMF band (§8.1), the chirp (bandwidth-smoothing) factors
//! per noise band (§8.2), patch construction mapping high bands to source
//! low bands (§8.3), and the HF-generation MAC that actually fills
//! `X_high[kx..kx+M-1]` from `X_low` (§8.4).
//!
//! **Buffer-index convention** (verified against faad2 `sbr_hfgen.c` +
//! `sbr_dec.c` and FFmpeg `aacsbr_template.c` + `sbrdsp_fixed.c` live
//! source, not just the reference doc's prose — this is the exact detail
//! the task brief flagged as the highest-risk ambiguity):
//!
//! `x_low`/`x_high` are both `MAX_SLOTS` (=40) wide. A *different* task
//! (frame orchestration, T11) is responsible for writing the current
//! frame's 32 fresh QMF-analysis slots at physical indices
//! `[T_HFGEN, T_HFGEN+32)` = `[8,40)` (faad2 `sbr_qmf_analysis_32(...,
//! sbr->tHFGen, ...)` / FFmpeg `sbr_lf_gen`'s `X_low[k][i - t_HFGen]` for
//! `i` in `[8,40)`), carrying the previous frame's last 8 slots at `[0,8)`.
//!
//! *This* module never uses `T_HFGEN` directly — every read/write in here
//! (both the LPC autocorrelation window and the HF-gen MAC) is anchored at
//! `T_HFADJ` (=2), confirmed byte-for-byte against both reference decoders:
//! faad2's `hf_generation` sets `offset = sbr->tHFAdj` and uses it for
//! *both* `Xlow[l+offset]` and `Xhigh[l+offset]`; FFmpeg's `sbr_hf_gen`
//! passes `X_high[k] + ENVELOPE_ADJUSTMENT_OFFSET` / `X_low[p] +
//! ENVELOPE_ADJUSTMENT_OFFSET` (`ENVELOPE_ADJUSTMENT_OFFSET` == `T_HFADJ`)
//! into the DSP kernel, which then indexes `X_low[i]`/`X_low[i-1]`/
//! `X_low[i-2]` directly off that pre-offset pointer for `i` in
//! `[2*t_env[0], 2*t_env[L_E])` (`sbrdsp_fixed.c` `sbr_hf_gen_c`). So a
//! frame-relative slot `l` (from the ×RATE-scaled envelope grid, `l=0`
//! meaning the start of the *envelope's* covered span) maps to physical
//! buffer index `l + T_HFADJ`, and the 2-tap lookback `l-1`/`l-2` stays
//! in-bounds down to `l=0` (physical 0) — that's *why* `T_HFADJ=2` exists
//! as its own constant distinct from `T_HFGEN=8`: it's the minimum runway
//! a 2nd-order predictor needs, not the QMF analysis's full lookback depth.
//! The wider `T_HFGEN` window matters only to §8.1's autocorrelation, which
//! deliberately looks further back (`numTimeSlotsRate+6` = 38 samples,
//! physical `[2,40)`) than the 2-tap MAC needs, for a better-conditioned
//! covariance estimate — faad2 `auto_correlation`'s own loop is `for (j =
//! offset; j < len+offset; j++)` with the *same* `offset = sbr->tHFAdj`.
//!
//! Both `calc_prediction_coef` and `hf_generation` were cross-checked this
//! way against live faad2/FFmpeg source (not just this repo's own
//! `sbr-reference.md` paraphrase) because the brief specifically flagged
//! the LPC conjugate placement and the MAC lookback/bw-power as the
//! highest-risk detail in this task; see `task-9-report.md` for the full
//! derivation and cross-source citations.

use super::freq_tables::FreqTables;
use super::grid::SbrGrid;
use super::qmf::{Cpx, MAX_SLOTS, N_ANA, N_SYN, T_HFADJ};
use crate::decoder::DecodeError;

/// §8.3 patch table: `n_patches` consecutive-low-band-to-high-band copies,
/// `n_patches <= 6`. `num_subbands[p]`/`start_subband[p]` are faad2's
/// `patchNoSubbands`/`patchStartSubband`.
///
/// The cap is **6**, not 5: 14496-3 sp04 p205 sets the *requirement* at 5, but
/// FFmpeg's `sbr_hf_calc_npatches` places its `num_patches > 5` reject at the
/// TOP of the loop body (before a patch is recorded), so a *final* count of
/// exactly 6 is legal-in-practice — its own comment notes "the Coding
/// Technologies decoder check stream has a final count of 6 patches". Real
/// HE-AAC streams (linear freq scale, wide M) hit this; capping at 5 silently
/// discards the top patch, zeroing the highest QMF bands. Arrays are sized 6
/// to match FFmpeg's `patch_num_subbands[6]`/`patch_start_subband[6]`.
#[derive(Clone, Copy, Default)]
pub(super) struct Patches {
    pub n_patches: usize,
    pub num_subbands: [u8; 6],
    pub start_subband: [u8; 6],
}

impl Patches {
    /// Sorted **absolute** QMF-channel patch borders (`kx`, then `kx +`
    /// each patch's cumulative `num_subbands`, ending at `kx+M`) — the same
    /// absolute domain as `FreqTables::f_low`/`f_high`, which is what
    /// `freq_tables::derive_limiter` merges them against (it subtracts `kx`
    /// itself when building `f_lim`; passing already-`kx`-relative values
    /// here would double-subtract). `kx` is taken as a parameter precisely
    /// because these values are computed as `kx + cumsum`, not read off
    /// `self` directly. Cumulative sums over strictly-positive
    /// `num_subbands` entries (a patch is only ever recorded once
    /// `patchNoSubbands>0`, per §8.3) are automatically strictly
    /// increasing, so no separate sort step is needed. Returns `(borders,
    /// len)`, `len = n_patches+1 <= 7` (6 patches → 7 borders).
    pub fn borders(&self, kx: u8) -> ([u8; 7], usize) {
        let mut b = [0u8; 7];
        b[0] = kx;
        let mut acc: u32 = kx as u32;
        let mut n = 1usize;
        for i in 0..self.n_patches.min(6) {
            acc += self.num_subbands[i] as u32;
            if n >= b.len() {
                break;
            }
            b[n] = acc.min(255) as u8;
            n += 1;
        }
        (b, n)
    }
}

/// §8.3 `goalSb`: `round(2.048e6 / fs)`, matching faad2's precomputed
/// `goalSbTab[sr_index]` exactly (verified: e.g. `round(2048000/48000)=43`,
/// `round(2048000/44100)=46`, `round(2048000/32000)=64` — all match faad2's
/// table entries) — computed live via integer rounding (the same
/// `(num + den/2) / den` round-half-up convention `freq_tables::compute_k0`
/// already uses) instead of a second lookup table, since it's a single
/// division and avoids duplicating a 12-entry const table for one value.
fn goal_sb(output_rate: u32) -> Result<u32, DecodeError> {
    if output_rate == 0 {
        return Err(DecodeError::Malformed);
    }
    Ok((2_048_000u32 + output_rate / 2) / output_rate)
}

/// Patch construction (§8.3 / faad2 `patch_construction`). Maps high QMF
/// bands to source low bands by walking `f_master` backward from a `goalSb`
/// anchor, each iteration carving off the largest patch that keeps the
/// "odd" alignment consistent with `k0`; stops when the running border `sb`
/// reaches `kx+M`. Drops a too-small (`<3` subbands) final patch. The patch
/// count is capped at **6** exactly as FFmpeg does it: the `n_patches > 5`
/// reject sits at the top of the loop body, BEFORE a patch is recorded, so a
/// final count of 6 is kept (the Coding-Technologies check stream) while a 7th
/// patch returns `Err`. `output_rate` = 2×core (for `goalSb`).
pub(super) fn patch_construction(
    ft: &FreqTables,
    output_rate: u32,
) -> Result<Patches, DecodeError> {
    let n_master = ft.n_master as usize;
    if n_master == 0 {
        // faad2's own early-out: nothing to patch.
        return Ok(Patches::default());
    }
    if n_master >= ft.f_master.len() {
        return Err(DecodeError::Malformed);
    }

    let k0 = ft.k0 as i32;
    let kx = ft.kx as i32;
    let m = ft.m as i32;
    let target = kx + m;

    let goal = goal_sb(output_rate)? as i32;

    // Initial `k`: first f_master index whose value >= goalSb (or N_master
    // if goalSb doesn't undercut kx+M). Mirrors faad2's
    // `for (i=0,k=0; f_master[i]<goalSb; i++) k=i+1;` — bounded here by
    // `n_master` defensively (mathematically guaranteed to terminate by
    // then whenever `goal < target`, since f_master[n_master]=k2>=target).
    let mut k: usize = if goal < target {
        let mut found = n_master;
        for i in 0..n_master {
            if (ft.f_master[i] as i32) >= goal {
                found = i;
                break;
            }
        }
        found
    } else {
        n_master
    };

    let mut msb = k0;
    let mut usb = kx;
    let mut n_patches = 0usize;
    // Scratch sized to FFmpeg's `patch_num_subbands[6]`/`patch_start_subband[6]`.
    // The in-loop `n_patches > 5` guard below runs BEFORE every record, so a
    // patch is only ever written at index `n_patches <= 5` (a 6th patch, index
    // 5, is the last write; a 7th is rejected before its write) — index 6 is
    // never touched. Real spec-conformant headers reach at most 6 patches.
    const SCRATCH: usize = 6;
    let mut num_subbands = [0i32; SCRATCH];
    let mut start_subband = [0i32; SCRATCH];

    let max_iters = n_master + 2;
    let mut sb: i32;
    let mut iters = 0usize;
    loop {
        iters += 1;
        if iters > max_iters || k > n_master {
            return Err(DecodeError::Malformed);
        }

        // Inner loop: walk j down from k, looking for the largest f_master
        // border `sb` that still satisfies `sb <= k0-1+msb-odd`.
        let mut j = k as i32 + 1;
        let odd;
        loop {
            j -= 1;
            if j < 0 {
                return Err(DecodeError::Malformed);
            }
            let jf = *ft.f_master.get(j as usize).ok_or(DecodeError::Malformed)? as i32;
            let this_odd = (jf - 2 + k0).rem_euclid(2);
            if jf <= k0 - 1 + msb - this_odd {
                sb = jf;
                odd = this_odd;
                break;
            }
        }

        // FFmpeg `sbr_hf_calc_npatches`: the `num_patches > 5` reject sits
        // HERE, at the top of the loop body BEFORE the patch is recorded, so a
        // final count of exactly 6 is kept (14496-3 sp04 p205 caps the
        // *requirement* at 5, but the Coding-Technologies check stream legally
        // reaches 6) while a 7th patch — `n_patches` already 6 — returns `Err`.
        // Because this runs before every write, a patch is only recorded at
        // index `n_patches <= 5`, so `num_subbands`/`start_subband` (len 6) are
        // never indexed out of bounds.
        if n_patches > 5 {
            return Err(DecodeError::Malformed);
        }
        debug_assert!(n_patches < SCRATCH);
        let no_sub = (sb - usb).max(0);
        let start_sub = k0 - odd - no_sub;
        num_subbands[n_patches] = no_sub;
        start_subband[n_patches] = start_sub;

        if no_sub > 0 {
            usb = sb;
            msb = sb;
            n_patches += 1;
        } else {
            msb = kx;
        }

        let fk = *ft.f_master.get(k).ok_or(DecodeError::Malformed)? as i32;
        if fk - sb < 3 {
            k = n_master;
        }

        if sb == target {
            break;
        }
    }

    if n_patches > 1 && num_subbands[n_patches - 1] < 3 {
        n_patches -= 1;
    }
    // No final `min` cap here: FFmpeg has none, and the in-loop `n_patches > 5`
    // reject already bounds this to <= 6 (a 7th patch returns `Err` above). A
    // `.min(5)` here is the original bug — it silently discarded a legitimate
    // 6th patch. `n_patches` is now provably <= 6, so every index into the
    // len-6 `Patches` arrays below (and the len-7 `borders`) is in bounds.

    let mut patches = Patches {
        n_patches,
        ..Patches::default()
    };
    for i in 0..n_patches {
        let ns = num_subbands[i];
        let ss = start_subband[i];
        if !(0..=64).contains(&ns) || !(0..=64).contains(&ss) || ss + ns > 64 {
            return Err(DecodeError::Malformed);
        }
        patches.num_subbands[i] = ns as u8;
        patches.start_subband[i] = ss as u8;
    }
    Ok(patches)
}

/// Chirp state carried across frames (per channel): the previous frame's
/// smoothed bandwidth and `bs_invf_mode`, per noise band.
#[derive(Clone, Copy, Default)]
pub(super) struct ChirpState {
    pub bw_prev: [f32; 5],
    pub invf_prev: [u8; 5],
}

/// §8.2 `mapNewBw` (faad2 exact form — cross-checked against live source):
/// `HIGH(3)->0.98`, `MID(2)->0.9`, `LOW(1)->0.6 if prev==OFF else 0.75`,
/// `OFF(0)->0.6 if prev==LOW else 0.0`.
fn map_new_bw(invf: u8, invf_prev: u8) -> f32 {
    match invf {
        3 => 0.98,
        2 => 0.9,
        1 => {
            if invf_prev == 0 {
                0.6
            } else {
                0.75
            }
        }
        _ => {
            if invf_prev == 1 {
                0.6
            } else {
                0.0
            }
        }
    }
}

/// Chirp factors (§8.2 / faad2 `calc_chirp_factors`) per noise band:
/// `new_bw` from [`map_new_bw`], asymmetric one-pole smoothing against the
/// previous frame's bandwidth (slower attack when increasing: 0.90625/
/// 0.09375, faster when decreasing: 0.75/0.25), then the
/// `[0.015625, 0.99609375]` snap/clamp. Updates `st.bw_prev`/`st.invf_prev`
/// with this frame's values for the next call.
pub(super) fn calc_chirp_factors(
    invf: &[u8; 5],
    n_q: usize,
    st: &mut ChirpState,
    bw: &mut [f32; 5],
) {
    let n_q = n_q.min(5);
    for i in 0..n_q {
        let new_bw = map_new_bw(invf[i], st.invf_prev[i]);
        let bw_prev = st.bw_prev[i];
        let mut b = if new_bw < bw_prev {
            0.75 * new_bw + 0.25 * bw_prev
        } else {
            0.90625 * new_bw + 0.09375 * bw_prev
        };
        if b < 0.015625 {
            b = 0.0;
        } else if b >= 0.996_093_75 {
            b = 0.996_093_75;
        }
        bw[i] = b;
        st.bw_prev[i] = b;
        st.invf_prev[i] = invf[i];
    }
}

/// `a * conj(b)` (complex).
fn mul_conj(a: Cpx, b: Cpx) -> Cpx {
    Cpx {
        re: a.re * b.re + a.im * b.im,
        im: a.im * b.re - a.re * b.im,
    }
}

/// `a * b` (complex, no conjugate).
fn mul(a: Cpx, b: Cpx) -> Cpx {
    Cpx {
        re: a.re * b.re - a.im * b.im,
        im: a.re * b.im + a.im * b.re,
    }
}

/// 2nd-order complex LPC per low band (§8.1 / faad2 `calc_prediction_coef` +
/// `auto_correlation`), filling `alpha0[k]`/`alpha1[k]` for `k=0..k0`.
///
/// Covariance-method autocorrelation over the `numTimeSlotsRate+6` (=38)
/// most recent samples (physical buffer indices `T_HFADJ..MAX_SLOTS` =
/// `2..40` — see the module doc comment for why `T_HFADJ`, not `T_HFGEN`,
/// anchors this window):
/// ```text
/// φ01 = Σ X[l]·conj(X[l-1])     φ02 = Σ X[l]·conj(X[l-2])
/// φ11 = Σ X[l-1]·conj(X[l-1])   φ12 = Σ X[l-1]·conj(X[l-2])   φ22 = Σ X[l-2]·conj(X[l-2])
/// d  = Re(φ22)·Re(φ11) − |φ12|²/(1+1e-6)
/// α1 = (d==0) ? 0 : (φ01·φ12 − φ02·φ11)/d
/// α0 = (Re(φ11)==0) ? 0 : −(φ01 + α1·conj(φ12))/Re(φ11)
/// ```
/// (φ11/φ22 are exactly real by construction — `X·conj(X)` — so only their
/// real accumulators are tracked.) Stability fallback: `|α0|²>=16 ||
/// |α1|²>=16 => α0=α1=0`.
pub(super) fn calc_prediction_coef(
    x_low: &[[Cpx; N_ANA]; MAX_SLOTS],
    k0: usize,
    alpha0: &mut [Cpx; N_ANA],
    alpha1: &mut [Cpx; N_ANA],
) {
    let k0 = k0.min(N_ANA);
    const REL: f32 = 1.0 / (1.0 + 1e-6);

    for k in 0..k0 {
        let mut phi01 = Cpx::default();
        let mut phi02 = Cpx::default();
        let mut phi11 = 0.0f32;
        let mut phi12 = Cpx::default();
        let mut phi22 = 0.0f32;

        for j in T_HFADJ..MAX_SLOTS {
            let cur = x_low[j][k];
            let p1 = x_low[j - 1][k];
            let p2 = x_low[j - 2][k];

            let t01 = mul_conj(cur, p1);
            phi01.re += t01.re;
            phi01.im += t01.im;
            let t02 = mul_conj(cur, p2);
            phi02.re += t02.re;
            phi02.im += t02.im;
            phi11 += p1.re * p1.re + p1.im * p1.im;
            let t12 = mul_conj(p1, p2);
            phi12.re += t12.re;
            phi12.im += t12.im;
            phi22 += p2.re * p2.re + p2.im * p2.im;
        }

        let d = phi22 * phi11 - (phi12.re * phi12.re + phi12.im * phi12.im) * REL;

        let mut a1 = Cpx::default();
        if d != 0.0 {
            let t = mul(phi01, phi12);
            let re = t.re - phi02.re * phi11;
            let im = t.im - phi02.im * phi11;
            let inv_d = 1.0 / d;
            a1 = Cpx {
                re: re * inv_d,
                im: im * inv_d,
            };
        }

        let mut a0 = Cpx::default();
        if phi11 != 0.0 {
            let cross = mul_conj(a1, phi12); // a1 * conj(phi12)
            let inv11 = 1.0 / phi11;
            a0 = Cpx {
                re: -(phi01.re + cross.re) * inv11,
                im: -(phi01.im + cross.im) * inv11,
            };
        }

        let m0 = a0.re * a0.re + a0.im * a0.im;
        let m1 = a1.re * a1.re + a1.im * a1.im;
        if m0 >= 16.0 || m1 >= 16.0 {
            a0 = Cpx::default();
            a1 = Cpx::default();
        }

        alpha0[k] = a0;
        alpha1[k] = a1;
    }
}

/// HF generation MAC (§8.4 / faad2 `hf_generation`, FFmpeg `sbr_hf_gen` +
/// `sbr_hf_gen_c`): for each high band `k` sourced from low band `p` (via
/// the patch map), noise band `g = table_map_k_to_g[k-kx]`, `bw = bw[g]`:
/// ```text
/// X_high[l+T_HFADJ][k] = X_low[l+T_HFADJ][p]
///                       + (α0[p]·bw)  ·X_low[l+T_HFADJ-1][p]
///                       + (α1[p]·bw²)·X_low[l+T_HFADJ-2][p]
/// ```
/// for `l` in the current envelope's slot span `[t_env[0]·RATE,
/// t_env[L_E]·RATE)`. When `bw==0` the prediction terms vanish exactly
/// (0·anything=0), degenerating to a straight copy — matching faad2's
/// separate `bw2>0` branch without needing to special-case it here.
/// No-panic: every derived `k`/`p`/buffer index is bounds-checked before
/// use; an out-of-range `(patch, x)` pair is skipped (not written) rather
/// than causing an out-of-bounds access.
// 8 parameters: the brief's fixed interface (§8.4's MAC genuinely needs
// x_low/alpha0/alpha1/bw/ft/patches/grid as independent inputs plus the
// x_high out-param) — matches the signature the task brief specifies
// exactly, not something to restructure into a struct here.
#[allow(clippy::too_many_arguments)]
pub(super) fn hf_generation(
    x_low: &[[Cpx; N_ANA]; MAX_SLOTS],
    alpha0: &[Cpx; N_ANA],
    alpha1: &[Cpx; N_ANA],
    bw: &[f32; 5],
    ft: &FreqTables,
    patches: &Patches,
    grid: &SbrGrid,
    x_high: &mut [[Cpx; N_SYN]; MAX_SLOTS],
) {
    let kx = ft.kx as usize;
    let kx_plus_m = kx + ft.m as usize;

    let num_env = grid.num_env as usize;
    if num_env >= grid.t_env.len() {
        return;
    }
    let first = grid.t_env[0] as usize * 2;
    let last = grid.t_env[num_env] as usize * 2;

    let mut k = kx;
    for i in 0..patches.n_patches.min(6) {
        let num_sb = patches.num_subbands[i] as usize;
        let start_sb = patches.start_subband[i] as usize;
        for x in 0..num_sb {
            let p = start_sb + x;
            let cur_k = k;
            k += 1; // always advance, whether or not this (i,x) is usable

            if cur_k >= kx_plus_m || cur_k >= N_SYN || p >= N_ANA {
                continue;
            }
            let rel = cur_k - kx;
            let g = ft.table_map_k_to_g.get(rel).copied().unwrap_or(0) as usize;
            let b = bw.get(g).copied().unwrap_or(0.0);
            let bw2 = b * b;

            let a0p = Cpx {
                re: alpha0[p].re * b,
                im: alpha0[p].im * b,
            };
            let a1p = Cpx {
                re: alpha1[p].re * bw2,
                im: alpha1[p].im * bw2,
            };

            for l in first..last {
                let phys = l + T_HFADJ;
                if !(2..MAX_SLOTS).contains(&phys) {
                    continue;
                }
                let cur = x_low[phys][p];
                let prev1 = x_low[phys - 1][p];
                let prev2 = x_low[phys - 2][p];
                let t1 = mul(a0p, prev1);
                let t2 = mul(a1p, prev2);
                x_high[phys][cur_k] = Cpx {
                    re: cur.re + t1.re + t2.re,
                    im: cur.im + t1.im + t2.im,
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chirp_off_is_zero() {
        // invf OFF (0), prev OFF → new_bw=0 → bw < 0.015625 → 0.
        let mut st = ChirpState::default();
        let mut bw = [0.0f32; 5];
        calc_chirp_factors(&[0, 0, 0, 0, 0], 2, &mut st, &mut bw);
        assert_eq!(bw[0], 0.0);
        assert_eq!(bw[1], 0.0);
    }
    #[test]
    fn chirp_high_from_zero_prev() {
        // invf HIGH (3) → new_bw=0.98; new_bw ≥ bw_prev(0) → bw = 0.90625*0.98 + 0 = 0.888125.
        let mut st = ChirpState::default();
        let mut bw = [0.0f32; 5];
        calc_chirp_factors(&[3, 0, 0, 0, 0], 1, &mut st, &mut bw);
        assert!((bw[0] - 0.888125).abs() < 1e-5);
        assert!((st.bw_prev[0] - 0.888125).abs() < 1e-5);
    }
    #[test]
    fn patches_are_bounded() {
        use crate::formats::aac::sbr::{freq_tables::derive, header::SbrHeader};
        let h = SbrHeader {
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
        let ft = derive(&h, 44100).unwrap();
        let p = patch_construction(&ft, 44100).unwrap();
        assert!(p.n_patches >= 1 && p.n_patches <= 6);
        let mut total = 0u32;
        for i in 0..p.n_patches {
            assert!(p.start_subband[i] as usize + p.num_subbands[i] as usize <= 64);
            total += u32::from(p.num_subbands[i]);
        }
        assert_eq!(total, u32::from(ft.m)); // patches cover exactly M high bands

        // Hand-derived (see task-9-report.md): for this exact header/rate,
        // faad2's algorithm produces 2 patches [9,11] starting at [5,2],
        // giving absolute borders [kx, kx+9, kx+M] = [17, 26, 37].
        assert_eq!(p.n_patches, 2);
        assert_eq!(&p.num_subbands[..2], &[9, 11]);
        assert_eq!(&p.start_subband[..2], &[5, 2]);
        let (borders, len) = p.borders(ft.kx);
        assert_eq!(&borders[..len], &[17, 26, 37]);
    }

    #[test]
    fn patch_construction_six_patches_coding_technologies() {
        // Regression for the "capped at 5" bug (was: `n_patches.min(5)` silently
        // discarded a legitimate 6th patch, zeroing the top QMF bands → a broken
        // high band above the crossover). This is ps-local.m4a's derived SBR
        // config: k0=8, kx=8, M=35 (so k2=43), a *linear* master table
        // (bs_freq_scale=0, bs_alter_scale=1 ⇒ dk=2). The real fixture is
        // license-restricted / gitignored, so the derived tables are built
        // directly here (as `hf_generation_zero_bw_is_straight_copy` does),
        // keeping this CI-safe and fixture-free.
        //
        // Hand-traced against FFmpeg `sbr_hf_calc_npatches` — whose `num_patches
        // > 5` reject sits BEFORE the record, so a final count of exactly 6 is
        // kept: the six patches are num_subbands=[5,6,6,6,6,6], giving absolute
        // borders [8,13,19,25,31,37,43] that fully cover k0..k2 = 8..43. Against
        // the old `.min(5)` code this yields only 5 patches / borders
        // [8,13,19,25,31,37], dropping the top patch — so `n_patches == 6` is the
        // discriminating assertion.
        let mut ft = FreqTables {
            k0: 8,
            kx: 8,
            m: 35,
            n_master: 18,
            ..FreqTables::default()
        };
        ft.f_master[..19].copy_from_slice(&[
            8, 9, 11, 13, 15, 17, 19, 21, 23, 25, 27, 29, 31, 33, 35, 37, 39, 41, 43,
        ]);
        // goal_sb = round(2_048_000/44_100) = 46 ≥ kx+M(43) ⇒ k starts at n_master.
        let p = patch_construction(&ft, 44100).unwrap();
        assert_eq!(p.n_patches, 6, "6th (top) patch must not be dropped");
        assert_eq!(&p.num_subbands[..6], &[5, 6, 6, 6, 6, 6]);
        assert_eq!(&p.start_subband[..6], &[2, 1, 1, 1, 1, 1]);
        let (borders, len) = p.borders(ft.kx);
        assert_eq!(len, 7);
        assert_eq!(&borders[..len], &[8, 13, 19, 25, 31, 37, 43]);
        // Patches cover exactly M high bands (kx..kx+M).
        let total: u32 = p.num_subbands[..p.n_patches]
            .iter()
            .map(|&s| u32::from(s))
            .sum();
        assert_eq!(total, u32::from(ft.m));
    }

    #[test]
    fn patch_construction_seventh_patch_is_err_not_panic() {
        // Bound: a config that would need a 7th patch must return `Err` (mirroring
        // FFmpeg's `num_patches > 5` reject) — never a panic, an OOB write into
        // the len-6 arrays, or a 7-patch result. Same linear master as above,
        // extended to k2=49 (M=41): six 6-ish-wide patches reach 43, and a
        // seventh would be needed to cover 43..49, which the guard rejects.
        let mut ft = FreqTables {
            k0: 8,
            kx: 8,
            m: 41,
            n_master: 21,
            ..FreqTables::default()
        };
        ft.f_master[..22].copy_from_slice(&[
            8, 9, 11, 13, 15, 17, 19, 21, 23, 25, 27, 29, 31, 33, 35, 37, 39, 41, 43, 45, 47, 49,
        ]);
        assert!(patch_construction(&ft, 44100).is_err());
    }

    #[test]
    fn calc_prediction_coef_zero_input_is_zero_alpha() {
        // Degenerate/silent input: phi11==phi22==0 (real) => d==0 and
        // Re(phi11)==0, both explicit guards => alpha0=alpha1=0. Exercises
        // the d==0 / phi11==0 branches directly (a real signal essentially
        // never hits these exactly, but silence legitimately does).
        let x_low = [[Cpx::default(); N_ANA]; MAX_SLOTS];
        let mut a0 = [Cpx::default(); N_ANA];
        let mut a1 = [Cpx::default(); N_ANA];
        // Poison with nonzero sentinels so a no-op implementation wouldn't
        // accidentally pass.
        a0[0] = Cpx { re: 1.0, im: 1.0 };
        a1[0] = Cpx { re: 1.0, im: 1.0 };
        calc_prediction_coef(&x_low, 3, &mut a0, &mut a1);
        for k in 0..3 {
            assert_eq!(a0[k], Cpx::default());
            assert_eq!(a1[k], Cpx::default());
        }
    }

    #[test]
    fn hf_generation_zero_bw_is_straight_copy() {
        // bw==0 everywhere => the predictor terms vanish exactly, so
        // X_high[phys][k] must equal X_low[phys][p] bit-for-bit. This also
        // pins the T_HFADJ-anchored index convention: x_low is filled with
        // values that depend on (phys, p) independently, so any indexing
        // mistake (wrong offset, off-by-one lookback, band mixup) shows up
        // as an exact-equality failure.
        let mut x_low = [[Cpx::default(); N_ANA]; MAX_SLOTS];
        for (phys, slot) in x_low.iter_mut().enumerate() {
            for (p, c) in slot.iter_mut().enumerate() {
                *c = Cpx {
                    re: phys as f32,
                    im: p as f32,
                };
            }
        }
        // Deliberately large/nonzero alpha to prove bw==0 zeroes their
        // contribution rather than the values happening to be trivially 0.
        let mut alpha0 = [Cpx::default(); N_ANA];
        let mut alpha1 = [Cpx::default(); N_ANA];
        for p in 0..3 {
            alpha0[p] = Cpx { re: 3.0, im: -2.0 };
            alpha1[p] = Cpx { re: -1.5, im: 4.0 };
        }
        let bw = [0.0f32; 5];

        let mut ft = FreqTables {
            kx: 5,
            m: 3,
            ..FreqTables::default()
        };
        ft.table_map_k_to_g[..3].copy_from_slice(&[0, 0, 0]);

        let patches = Patches {
            n_patches: 1,
            num_subbands: [3, 0, 0, 0, 0, 0],
            start_subband: [0, 0, 0, 0, 0, 0],
        };

        let mut t_env = [0u8; 6];
        t_env[1] = 16; // full span: first=0, last=32
        let grid = SbrGrid {
            num_env: 1,
            t_env,
            ..SbrGrid::default()
        };

        let mut x_high = [[Cpx::default(); N_SYN]; MAX_SLOTS];
        hf_generation(
            &x_low,
            &alpha0,
            &alpha1,
            &bw,
            &ft,
            &patches,
            &grid,
            &mut x_high,
        );

        for (l, k) in [(0usize, 5usize), (15, 6), (31, 7)] {
            let phys = l + T_HFADJ;
            let p = k - 5;
            assert_eq!(x_high[phys][k], x_low[phys][p], "l={l} k={k}");
        }
        // Untouched: band below kx, and a slot before the written range.
        assert_eq!(x_high[2][4], Cpx::default());
        assert_eq!(x_high[0][5], Cpx::default());
    }

    #[test]
    fn hf_generation_no_panic_on_derived_tables() {
        use crate::formats::aac::sbr::{freq_tables::derive, header::SbrHeader};
        let h = SbrHeader {
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
        let ft = derive(&h, 44100).unwrap();
        let patches = patch_construction(&ft, 44100).unwrap();

        // Non-degenerate synthetic low-band signal (std sin/cos: test-only,
        // this crate is no_std outside cfg(test)).
        let mut x_low = [[Cpx::default(); N_ANA]; MAX_SLOTS];
        for (phys, slot) in x_low.iter_mut().enumerate() {
            for (band, c) in slot.iter_mut().enumerate() {
                let t = phys as f64;
                let b = band as f64;
                c.re = (0.37 * t + 0.13 * b).sin() as f32;
                c.im = (0.21 * t - 0.05 * b).cos() as f32;
            }
        }

        let mut alpha0 = [Cpx::default(); N_ANA];
        let mut alpha1 = [Cpx::default(); N_ANA];
        calc_prediction_coef(&x_low, ft.k0 as usize, &mut alpha0, &mut alpha1);

        let mut st = ChirpState::default();
        let mut bw = [0.0f32; 5];
        calc_chirp_factors(&[2, 3, 0, 0, 0], ft.n_q as usize, &mut st, &mut bw);

        let mut t_env = [0u8; 6];
        t_env[1] = 16;
        let grid = SbrGrid {
            num_env: 1,
            t_env,
            ..SbrGrid::default()
        };

        let mut x_high = [[Cpx::default(); N_SYN]; MAX_SLOTS];
        hf_generation(
            &x_low,
            &alpha0,
            &alpha1,
            &bw,
            &ft,
            &patches,
            &grid,
            &mut x_high,
        );

        let mut any_nonzero = false;
        for slot in &x_high {
            for &c in &slot[ft.kx as usize..(ft.kx + ft.m) as usize] {
                if c != Cpx::default() {
                    any_nonzero = true;
                }
            }
        }
        assert!(
            any_nonzero,
            "hf_generation produced no output in the patched band range"
        );
    }
}
