//! Stereo reconstruction / mixing (`ps-reference.md` §7) — the final DSP
//! stage before the Task 9 seam.
//!
//! Per parameter band and envelope, build a 2×2 (real, or complex with
//! IPD/OPD) mixing matrix from the dequantized IID/ICC (+IPD/OPD)
//! parameters, linearly interpolate it across the envelope's QMF slots, and
//! form the stereo pair from the direct (`s`) and decorrelated (`d`)
//! hybrid-domain signals:
//!
//! ```text
//! L = H11·s + H12·d          R = H21·s + H22·d
//! ```
//!
//! (+ imaginary cross-terms when IPD/OPD is active). The four physical matrix
//! elements are held internally in **FFmpeg's `HA`/`HB` array order**
//! `[arr0, arr1, arr2, arr3]` and applied by FFmpeg's own mixing (the
//! conformance oracle, `libavcodec/aacps.c` `stereo_processing` +
//! `ps_stereo_interpolate*`), which fixes the faad2-vs-ISO `H12`/`H21`
//! transposition unambiguously:
//!
//! ```text
//! L = arr0·s + arr2·d        R = arr1·s + arr3·d
//! ```
//!
//! so the brief's ISO naming maps as `H11=arr0`, `H12=arr2`, `H21=arr1`,
//! `H22=arr3` (arr2 — not arr1 — is the element multiplying `d` into `L`).
//!
//! **Type A** (`icc_mode < 3`, faad2 `ps_mix_phase` ≡ FFmpeg `HA`): from IID
//! the two channel gains `c1 = sf_iid[steps+q]`, `c2 = sf_iid[steps−q]` (the
//! table already encodes `√2/√(1+c²)` — no `sqrt` at runtime); from ICC the
//! rotation `(cosα,sinα) = (cos_alphas[icc], sin_alphas[icc])` and the
//! IID×ICC skew `(cosβ,sinβ) = (cos_betas, sin_betas)[|q|][icc]` (`sinβ`
//! negated for `q < 0`); then
//! `arr0=c2·cos(α+β)`, `arr1=c1·cos(β−α)`, `arr2=c2·sin(α+β)`,
//! `arr3=c1·sin(β−α)` via the angle-addition identities. Verify-first
//! identity: ρ=1 ⇒ icc index 0 ⇒ α=β=0 ⇒ `L=c2·s`, `R=c1·s`.
//!
//! **Type B** (`icc_mode ≥ 3`): `arr0=√2·cosψ·cosγ`, `arr1=√2·sinψ·cosγ`,
//! `arr2=−√2·sinψ·sinγ`, `arr3=√2·cosψ·sinγ` (FFmpeg `HB`; ψ from
//! `sincos_alphas_B`, γ from `cos/sin_gammas`).
//!
//! **IPD/OPD** (`enable_ipdopd` and the param band `< NR_IPDOPD_BANDS`):
//! makes the matrix complex. The 3-frame `[¼,½,1]`-weighted, unit-normalized
//! phase smoothing is table-driven (no `atan2`) via `PD_RE_SMOOTH`/
//! `PD_IM_SMOOTH` (`[512] = 8·8·8`, 3-history): the per-band history byte
//! packs the previous two phase indices, and the lookup index is
//! `hist·8 + cur`, then `hist ← (hist·8 + cur) & 0x3F`. The left column is
//! rotated by OPD, the right column by OPD−IPD (`opd·conj(ipd)`).
//! `NEGATE_IPD_MASK` groups (FFmpeg's flattened-band `k` ranges) negate the
//! imaginary anchor.
//!
//! **Interpolation** per band over the envelope's slots
//! `[border[e], border[e+1])`: `dH = (H_now − H_prev)/L_env` per element
//! (`L_env > 0` guarded, `[NOPANIC §2]`), `H` stepped once per slot and
//! applied to every flattened sub-band of the band; the last envelope's
//! `H_now` is carried into [`MixState`] as the next frame/envelope's anchor.
//!
//! `PsParams` (Task 4) exposes only the *dequantized* trig/gain floats, so
//! the integer indices the 2-D beta/gamma tables, the `c2` mirror and the
//! `PD_*_SMOOTH` lookup need are recovered here by exact reverse-lookup in
//! the same const tables (no data duplication).
use super::bands::{NR_BANDS, NR_IPDOPD_BANDS, NR_PAR_BANDS, k_to_i};
use super::tables::{
    COS_ALPHAS, COS_BETAS_FINE, COS_BETAS_NORMAL, COS_GAMMAS_FINE, COS_GAMMAS_NORMAL, IPDOPD_COS,
    IPDOPD_SIN, PD_IM_SMOOTH, PD_RE_SMOOTH, SF_IID_FINE, SF_IID_NORMAL, SIN_BETAS_FINE,
    SIN_BETAS_NORMAL, SIN_GAMMAS_FINE, SIN_GAMMAS_NORMAL, SINCOS_ALPHAS_B_FINE,
    SINCOS_ALPHAS_B_NORMAL,
};
use crate::formats::aac::ps::{N_SLOTS, NR_BANDS_MAX};
use crate::formats::aac::ps::{frame::PsFrameLayout, params::PsParams};
use crate::formats::aac::sbr::qmf::Cpx;

/// Backing width of the per-parameter-band interpolation anchors: the
/// 34-config uses ≤34 working parameter bands, sized to 50 to match the
/// reference's worst-case buffers (`ps-reference.md` §8).
const MAX_PAR_BANDS: usize = 50;
/// Backing width of the IPD/OPD phase-history rings (≥ `NR_IPDOPD_BANDS`
/// max of 17).
const MAX_IPDOPD_BANDS: usize = 20;

/// Persistent mixing state, carried frame-to-frame (Task 9 threads it like
/// `HybridHistory`/`DecorrState`). `ps-reference.md` §7/§8.
///
/// Anchors are stored in FFmpeg `HA`/`HB` array order (`[arr0,arr1,arr2,arr3]`
/// = ISO `[H11,H21,H12,H22]`); the imaginary anchor is only non-zero while
/// IPD/OPD is active. `ipd_hist`/`opd_hist` are FFmpeg's packed 6-bit
/// phase-index rings (reconciled against the `[512]` 3-history
/// `PD_*_SMOOTH` tables — *not* the brief's original `[64]`/complex-ring
/// assumption; see the module docs).
#[derive(Clone, Copy)]
pub(crate) struct MixState {
    /// Real interpolation anchor per working parameter band (last envelope of
    /// the previous frame), FFmpeg array order.
    hr_prev: [[f32; 4]; MAX_PAR_BANDS],
    /// Imaginary interpolation anchor (non-zero only with IPD/OPD).
    hi_prev: [[f32; 4]; MAX_PAR_BANDS],
    /// Packed 6-bit IPD phase-index history: `idx = hist·8 + cur`,
    /// `hist ← idx & 0x3F` (bits `[5:3]` = 2-frames-ago, `[2:0]` = 1-frame-ago).
    ipd_hist: [u8; MAX_IPDOPD_BANDS],
    /// Packed 6-bit OPD phase-index history (see `ipd_hist`).
    opd_hist: [u8; MAX_IPDOPD_BANDS],
    /// Previous frame's `is34`, to reset the anchors/rings on a transition
    /// (FFmpeg `ipdopd_reset` + `map_val_*`; we reset conservatively).
    /// `None` until the first frame is mixed.
    is34_prev: Option<bool>,
}

impl Default for MixState {
    fn default() -> Self {
        // Manual: `[[f32; 4]; 50]` is wider than the toolchain's blanket
        // `[T; N]: Default`. All-zero anchors/rings == cold start (silence
        // before frame 0); `is34_prev = None` forces the first-frame reset
        // path (a no-op over already-zero state).
        Self {
            hr_prev: [[0.0; 4]; MAX_PAR_BANDS],
            hi_prev: [[0.0; 4]; MAX_PAR_BANDS],
            ipd_hist: [0; MAX_IPDOPD_BANDS],
            opd_hist: [0; MAX_IPDOPD_BANDS],
            is34_prev: None,
        }
    }
}

/// Nearest-entry reverse lookup: the index `i` minimizing `|table[i] − v|`.
/// Every value fed in came verbatim from `table`, so the match is exact; the
/// nearest fallback keeps a (hypothetically) off-table value in bounds
/// (`[NOPANIC]` — the result is always `< table.len()`).
#[inline]
fn nearest_index(table: &[f32], v: f32) -> usize {
    let mut best = 0usize;
    let mut best_d = f32::INFINITY;
    for (i, &t) in table.iter().enumerate() {
        let d = (t - v).abs();
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    best
}

/// Recover an IPD/OPD phase index `0..8` from its dequantized `(cos, sin)`
/// pair (the 8 phases are distinct as complex points; matching the pair
/// disambiguates the repeated cosines). `[NOPANIC]` — always `< 8`.
#[inline]
fn phase_index(cos: f32, sin: f32) -> usize {
    let mut best = 0usize;
    let mut best_d = f32::INFINITY;
    for q in 0..8 {
        let dc = IPDOPD_COS[q] - cos;
        let ds = IPDOPD_SIN[q] - sin;
        let d = dc * dc + ds * ds;
        if d < best_d {
            best_d = d;
            best = q;
        }
    }
    best
}

/// `(re, im)` of `(are + i·aim) · b`.
#[inline]
fn cmul_ri(are: f32, aim: f32, b: Cpx) -> Cpx {
    Cpx {
        re: are * b.re - aim * b.im,
        im: are * b.im + aim * b.re,
    }
}

/// Type A matrix (`icc_mode < 3`), FFmpeg `HA` array order
/// `[arr0,arr1,arr2,arr3]`. `c1 = iid_c` (`= sf_iid[steps+q]`), and the raw
/// index `q` is recovered to fetch `c2 = sf_iid[steps−q]`, `|q|` and `icc`
/// for the `(cos/sin)_betas[|q|][icc]` skew (`sinβ` negated for `q < 0`, so
/// the reconstructed matrix matches FFmpeg's signed-`iid` `HA`).
fn type_a(params: &PsParams, e: usize, b: usize, fine: bool) -> [f32; 4] {
    let c1 = params.iid_c[e][b];
    let (sf, steps): (&[f32], i32) = if fine {
        (&SF_IID_FINE, 15)
    } else {
        (&SF_IID_NORMAL, 7)
    };
    let i = nearest_index(sf, c1) as i32; // i = steps + q
    let q = i - steps;
    let abs_iid = q.unsigned_abs() as usize;
    let c2 = sf[(2 * steps - i).clamp(0, sf.len() as i32 - 1) as usize];

    let cosa = params.icc_cos[e][b];
    let sina = params.icc_sin[e][b];
    let icc = nearest_index(&COS_ALPHAS, cosa);

    let (cosb, mut sinb) = if fine {
        (COS_BETAS_FINE[abs_iid][icc], SIN_BETAS_FINE[abs_iid][icc])
    } else {
        (
            COS_BETAS_NORMAL[abs_iid][icc],
            SIN_BETAS_NORMAL[abs_iid][icc],
        )
    };
    if q < 0 {
        sinb = -sinb;
    }

    let ab1 = cosb * cosa; // cosβ·cosα
    let ab2 = sinb * sina; // sinβ·sinα
    let ab3 = sinb * cosa; // sinβ·cosα
    let ab4 = cosb * sina; // cosβ·sinα
    // arr0 = c2·cos(α+β), arr1 = c1·cos(β−α),
    // arr2 = c2·sin(α+β), arr3 = c1·sin(β−α).
    [
        c2 * (ab1 - ab2),
        c1 * (ab1 + ab2),
        c2 * (ab3 + ab4),
        c1 * (ab3 - ab4),
    ]
}

/// Type B matrix (`icc_mode ≥ 3`), FFmpeg `HB` array order. ψ (`sincos_alphas_B`,
/// signed index) rotates the level split, γ (`cos/sin_gammas[|q|][icc]`) the
/// enhanced-ICC rotation.
fn type_b(params: &PsParams, e: usize, b: usize, fine: bool) -> [f32; 4] {
    const SQRT2: f32 = core::f32::consts::SQRT_2;
    let c1 = params.iid_c[e][b];
    let (sf, steps): (&[f32], i32) = if fine {
        (&SF_IID_FINE, 15)
    } else {
        (&SF_IID_NORMAL, 7)
    };
    let i = nearest_index(sf, c1); // i = steps + q
    let q = i as i32 - steps;
    let abs_iid = q.unsigned_abs() as usize;
    let icc = nearest_index(&COS_ALPHAS, params.icc_cos[e][b]);

    let (cosa, sina, cosg, sing) = if fine {
        let mirror = 30usize.saturating_sub(i);
        (
            SINCOS_ALPHAS_B_FINE[i][icc],
            SINCOS_ALPHAS_B_FINE[mirror][icc],
            COS_GAMMAS_FINE[abs_iid][icc],
            SIN_GAMMAS_FINE[abs_iid][icc],
        )
    } else {
        let mirror = 14usize.saturating_sub(i);
        (
            SINCOS_ALPHAS_B_NORMAL[i][icc],
            SINCOS_ALPHAS_B_NORMAL[mirror][icc],
            COS_GAMMAS_NORMAL[abs_iid][icc],
            SIN_GAMMAS_NORMAL[abs_iid][icc],
        )
    };
    [
        SQRT2 * cosa * cosg,
        SQRT2 * sina * cosg,
        -SQRT2 * sina * sing,
        SQRT2 * cosa * sing,
    ]
}

/// Stereo mixing (§7): fill `xl`/`xr` (hybrid domain) from `s` (direct) and
/// `d` (decorrelated) using per-band/envelope interpolated 2×2 matrices.
/// Updates the interpolation anchors + IPD/OPD phase rings in `st`.
#[allow(clippy::too_many_arguments)] // the §7 mixing seam's fixed signature
pub(crate) fn stereo_mix(
    s: &[[Cpx; NR_BANDS_MAX]; N_SLOTS],
    d: &[[Cpx; NR_BANDS_MAX]; N_SLOTS],
    params: &PsParams,
    layout: &PsFrameLayout,
    icc_mode: u8,
    st: &mut MixState,
    xl: &mut [[Cpx; NR_BANDS_MAX]; N_SLOTS],
    xr: &mut [[Cpx; NR_BANDS_MAX]; N_SLOTS],
) {
    let is34 = layout.is34;
    let nr_par = NR_PAR_BANDS[is34 as usize].min(MAX_PAR_BANDS); // 20 / 34
    let nr_bands = NR_BANDS[is34 as usize].min(NR_BANDS_MAX); //   71 / 91
    let nr_ipdopd = NR_IPDOPD_BANDS[is34 as usize]; //             11 / 17
    let fine = layout.iid_quant;
    let type_b_mode = icc_mode >= 3;
    let ipdopd = params.enable_ipdopd;

    // is34-transition reset (FFmpeg's `ipdopd_reset` + anchor remap): the
    // anchors and phase rings are indexed by working parameter band, whose
    // meaning changes across a 20<->34 switch, so drop them rather than reuse
    // them at the wrong resolution. Also the cold-start path (`None`).
    if st.is34_prev != Some(is34) {
        st.hr_prev = [[0.0; 4]; MAX_PAR_BANDS];
        st.hi_prev = [[0.0; 4]; MAX_PAR_BANDS];
        st.ipd_hist = [0; MAX_IPDOPD_BANDS];
        st.opd_hist = [0; MAX_IPDOPD_BANDS];
        st.is34_prev = Some(is34);
    }

    let num_env = layout.num_env as usize;
    if num_env == 0 {
        // "Reuse previous parameters" sentinel (fixed class): no envelope to
        // mix this frame — leave `xl`/`xr` to the Task 9 seam.
        return;
    }

    // Interpolation anchors, carried from the previous frame's last envelope
    // and advanced envelope-to-envelope within this frame.
    let mut anc_r = st.hr_prev;
    let mut anc_i = st.hi_prev;

    for e in 0..num_env {
        // --- build H_now (+ IPD/OPD complex rotation) per parameter band ---
        let mut now_r = [[0.0f32; 4]; MAX_PAR_BANDS];
        let mut now_i = [[0.0f32; 4]; MAX_PAR_BANDS];
        for b in 0..nr_par {
            let mut r = if type_b_mode {
                type_b(params, e, b, fine)
            } else {
                type_a(params, e, b, fine)
            };
            let mut im = [0.0f32; 4];

            if ipdopd && b < nr_ipdopd {
                // 3-history phase smoothing, table-driven (no atan2). The
                // packed history byte holds the previous two indices; the
                // lookup is hist·8 + cur, then hist <- (hist·8 + cur) & 0x3F.
                let opd_cur = phase_index(params.opd_cos[e][b], params.opd_sin[e][b]);
                let ipd_cur = phase_index(params.ipd_cos[e][b], params.ipd_sin[e][b]);
                let opd_idx = (st.opd_hist[b] as usize) * 8 + opd_cur; // < 512
                let ipd_idx = (st.ipd_hist[b] as usize) * 8 + ipd_cur;
                let opd_re = PD_RE_SMOOTH[opd_idx];
                let opd_im = PD_IM_SMOOTH[opd_idx];
                let ipd_re = PD_RE_SMOOTH[ipd_idx];
                let ipd_im = PD_IM_SMOOTH[ipd_idx];
                st.opd_hist[b] = (opd_idx & 0x3F) as u8;
                st.ipd_hist[b] = (ipd_idx & 0x3F) as u8;

                // Right column phase = OPD − IPD = opd·conj(ipd).
                let ipd_adj_re = opd_re * ipd_re + opd_im * ipd_im;
                let ipd_adj_im = opd_im * ipd_re - opd_re * ipd_im;
                // arr0/arr2 (the left output row) rotated by OPD; arr1/arr3
                // (the right output row) by OPD − IPD.
                im[0] = r[0] * opd_im;
                r[0] *= opd_re;
                im[2] = r[2] * opd_im;
                r[2] *= opd_re;
                im[1] = r[1] * ipd_adj_im;
                r[1] *= ipd_adj_re;
                im[3] = r[3] * ipd_adj_im;
                r[3] *= ipd_adj_re;
            }
            now_r[b] = r;
            now_i[b] = im;
        }

        // --- interpolate the matrix across the envelope's slots + apply ---
        let start = layout.border_position[e] as usize;
        let stop = layout.border_position[e + 1] as usize;
        // [NOPANIC §2]: skip a non-positive / out-of-range envelope length
        // rather than divide by zero or index slots OOB. Anchors still carry.
        if stop > start && stop <= N_SLOTS {
            let inv = 1.0 / (stop - start) as f32;
            for k in 0..nr_bands {
                let b = k_to_i(k, is34);
                if b >= nr_par {
                    continue; // [NOPANIC] (V3 invariant already guarantees this)
                }
                let mut hr = anc_r[b];
                let mut hi = anc_i[b];
                // NEGATE_IPD_MASK groups: FFmpeg negates the imaginary anchor
                // (only) for these flattened bands.
                let negate = ipdopd && ((is34 && (9..=13).contains(&k)) || (!is34 && k <= 1));
                if negate {
                    hi = [-hi[0], -hi[1], -hi[2], -hi[3]];
                }
                let tr = now_r[b];
                let ti = now_i[b];
                let mut dr = [0.0f32; 4];
                let mut di = [0.0f32; 4];
                for j in 0..4 {
                    dr[j] = (tr[j] - hr[j]) * inv;
                    di[j] = (ti[j] - hi[j]) * inv;
                }
                for slot in start..stop {
                    for j in 0..4 {
                        hr[j] += dr[j];
                        hi[j] += di[j];
                    }
                    let sc = s[slot][k];
                    let dc = d[slot][k];
                    if ipdopd {
                        // L = arr0·s + arr2·d, R = arr1·s + arr3·d (complex).
                        let l0 = cmul_ri(hr[0], hi[0], sc);
                        let l2 = cmul_ri(hr[2], hi[2], dc);
                        let r1 = cmul_ri(hr[1], hi[1], sc);
                        let r3 = cmul_ri(hr[3], hi[3], dc);
                        xl[slot][k] = Cpx {
                            re: l0.re + l2.re,
                            im: l0.im + l2.im,
                        };
                        xr[slot][k] = Cpx {
                            re: r1.re + r3.re,
                            im: r1.im + r3.im,
                        };
                    } else {
                        xl[slot][k] = Cpx {
                            re: hr[0] * sc.re + hr[2] * dc.re,
                            im: hr[0] * sc.im + hr[2] * dc.im,
                        };
                        xr[slot][k] = Cpx {
                            re: hr[1] * sc.re + hr[3] * dc.re,
                            im: hr[1] * sc.im + hr[3] * dc.im,
                        };
                    }
                }
            }
        }

        // Carry this envelope's matrices as the next envelope's anchor.
        anc_r = now_r;
        anc_i = now_i;
    }

    // Persist the last envelope's matrices for the next frame's envelope 0.
    st.hr_prev = anc_r;
    st.hi_prev = anc_i;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::aac::ps::tables::{
        IPDOPD_COS as PCOS, IPDOPD_SIN as PSIN, PD_IM_SMOOTH as PDIM, PD_RE_SMOOTH as PDRE,
        SF_IID_NORMAL as SF, SIN_ALPHAS,
    };

    fn zero() -> [[Cpx; NR_BANDS_MAX]; N_SLOTS] {
        [[Cpx::default(); NR_BANDS_MAX]; N_SLOTS]
    }

    /// Fill band `k` of every slot with a constant value.
    fn fill_col(buf: &mut [[Cpx; NR_BANDS_MAX]; N_SLOTS], k: usize, v: Cpx) {
        for row in buf.iter_mut() {
            row[k] = v;
        }
    }

    /// 20-config, single fixed envelope over all 32 slots, IID/ICC neutral
    /// everywhere except as set by the caller.
    fn layout_1env_20() -> PsFrameLayout {
        PsFrameLayout {
            num_env: 1,
            border_position: [0, 32, 0, 0, 0, 0],
            nr_iid_par: 20,
            nr_icc_par: 20,
            nr_ipdopd_par: 11,
            is34: false,
            iid_quant: false,
        }
    }

    fn neutral_params_20() -> PsParams {
        let mut p = PsParams::default();
        for b in 0..20 {
            p.iid_c[0][b] = SF[7]; // q=0 -> c1=c2=1
            p.icc_cos[0][b] = COS_ALPHAS[0]; // rho=1 -> alpha=0
            p.icc_sin[0][b] = SIN_ALPHAS[0];
        }
        p
    }

    #[test]
    fn iid_only_icc_rho1_is_pure_pan() {
        // Verify-first gate. ICC rho=1 (icc index 0 -> cos_alpha=1,
        // sin_alpha=0) => alpha=beta=0 => L = c2·s, R = c1·s, with `d`
        // dropping out entirely (arr2 = arr3 = 0).
        let layout = layout_1env_20();
        let mut p = neutral_params_20();
        // IID index +2 at every band -> c1 = sf_iid[9], c2 = sf_iid[5].
        for b in 0..20 {
            p.iid_c[0][b] = SF[9];
        }
        let c1 = SF[9]; // 0.75464857
        let c2 = SF[5]; // 1.1960374

        let mut s = zero();
        let mut dd = zero();
        let k = 5usize; // k_to_i_20[5] = 3
        fill_col(&mut s, k, Cpx { re: 0.5, im: -0.3 });
        fill_col(&mut dd, k, Cpx { re: 0.9, im: 0.7 }); // arbitrary: must drop out
        let mut xl = zero();
        let mut xr = zero();
        let mut st = MixState::default();
        stereo_mix(&s, &dd, &p, &layout, 0, &mut st, &mut xl, &mut xr);

        // Last slot: interpolation from the cold (zero) anchor reaches H_now.
        let n = 31usize;
        assert!(
            (xl[n][k].re - c2 * 0.5).abs() < 1e-4,
            "xl.re={}",
            xl[n][k].re
        );
        assert!(
            (xl[n][k].im - c2 * -0.3).abs() < 1e-4,
            "xl.im={}",
            xl[n][k].im
        );
        assert!(
            (xr[n][k].re - c1 * 0.5).abs() < 1e-4,
            "xr.re={}",
            xr[n][k].re
        );
        assert!(
            (xr[n][k].im - c1 * -0.3).abs() < 1e-4,
            "xr.im={}",
            xr[n][k].im
        );
    }

    #[test]
    fn matrix_interpolates_across_slots() {
        // Two envelopes with different H (both IID-only, rho=1 => real
        // diagonal). Envelope 1's anchor is envelope 0's H_now, so across
        // its slot range the R gain ramps c1a -> c1b and L gain c2a -> c2b
        // linearly: slot (16+m) == H_prev + (m+1)·dH.
        let mut layout = layout_1env_20();
        layout.num_env = 2;
        layout.border_position = [0, 16, 32, 0, 0, 0];
        let mut p = neutral_params_20();
        // env0: iid +2 ; env1: iid -1.
        for b in 0..20 {
            p.iid_c[0][b] = SF[9]; // c1a=sf[9], c2a=sf[5]
            p.icc_cos[1][b] = COS_ALPHAS[0];
            p.icc_sin[1][b] = SIN_ALPHAS[0];
            p.iid_c[1][b] = SF[6]; // c1b=sf[6], c2b=sf[8]
        }
        let c1a = SF[9];
        let c2a = SF[5];
        let c1b = SF[6];
        let c2b = SF[8];

        let mut s = zero();
        let dd = zero();
        let k = 5usize; // param band 3
        fill_col(&mut s, k, Cpx { re: 1.0, im: 0.0 });
        let mut xl = zero();
        let mut xr = zero();
        let mut st = MixState::default();
        stereo_mix(&s, &dd, &p, &layout, 0, &mut st, &mut xl, &mut xr);

        // env1 spans slots 16..32; check m=7 (absolute slot 23).
        let m = 7usize;
        let n = 16 + m;
        let expect_r = c1a + ((m + 1) as f32) * (c1b - c1a) / 16.0;
        let expect_l = c2a + ((m + 1) as f32) * (c2b - c2a) / 16.0;
        assert!((xr[n][k].re - expect_r).abs() < 1e-4, "xr={}", xr[n][k].re);
        assert!((xl[n][k].re - expect_l).abs() < 1e-4, "xl={}", xl[n][k].re);
    }

    #[test]
    fn ipdopd_makes_matrix_complex() {
        // enable_ipdopd + non-zero IPD/OPD indices => complex matrix. With
        // IID/ICC neutral (arr = [1,1,0,0]), the left output carries the OPD
        // phase and the right output OPD-IPD = opd·conj(ipd), both via the
        // PD_*_SMOOTH tables. Band k=2 -> param band 0 (< 11), outside the
        // 20-cfg negate set {0,1}.
        let layout = layout_1env_20();
        let mut p = neutral_params_20();
        p.enable_ipdopd = true;
        let opd_q = 1usize; // (cos,sin) = pi/4
        let ipd_q = 2usize; // (cos,sin) = pi/2
        for b in 0..11 {
            p.opd_cos[0][b] = PCOS[opd_q];
            p.opd_sin[0][b] = PSIN[opd_q];
            p.ipd_cos[0][b] = PCOS[ipd_q];
            p.ipd_sin[0][b] = PSIN[ipd_q];
        }

        let mut s = zero();
        let dd = zero();
        let k = 2usize; // k_to_i_20[2] = 0
        fill_col(&mut s, k, Cpx { re: 1.0, im: 0.0 });
        let mut xl = zero();
        let mut xr = zero();
        let mut st = MixState::default();
        stereo_mix(&s, &dd, &p, &layout, 0, &mut st, &mut xl, &mut xr);

        // Hand-computed (opd_hist=ipd_hist=0 at frame 0): idx = phase index.
        let opd = Cpx {
            re: PDRE[opd_q],
            im: PDIM[opd_q],
        };
        let ipd = Cpx {
            re: PDRE[ipd_q],
            im: PDIM[ipd_q],
        };
        let ipd_adj_re = opd.re * ipd.re + opd.im * ipd.im;
        let ipd_adj_im = opd.im * ipd.re - opd.re * ipd.im;

        let n = 31usize; // last slot -> full H_now
        // xl == arr0·s == opd (since s=(1,0)); xr == arr1·s == opd·conj(ipd).
        assert!((xl[n][k].re - opd.re).abs() < 1e-4, "xl.re={}", xl[n][k].re);
        assert!((xl[n][k].im - opd.im).abs() < 1e-4, "xl.im={}", xl[n][k].im);
        assert!(
            (xr[n][k].re - ipd_adj_re).abs() < 1e-4,
            "xr.re={}",
            xr[n][k].re
        );
        assert!(
            (xr[n][k].im - ipd_adj_im).abs() < 1e-4,
            "xr.im={}",
            xr[n][k].im
        );
        // A genuinely non-zero imaginary cross-term was produced.
        assert!(xl[n][k].im.abs() > 0.1 && xr[n][k].im.abs() > 0.1);
    }

    #[test]
    fn type_a_matches_ffmpeg_ha_over_all_iid_icc() {
        // Conformance: the table-based Type A construction must reproduce
        // FFmpeg's `HA[iid][icc]` closed form (aacps_tablegen.h) for every
        // signed IID (±7) and ICC (0..7) — exercises the `sinβ` sign flip and
        // the `c2 = sf_iid[steps−q]` mirror. Reference computed here with f64
        // libm (host-only; the shipping code stays table-driven / no libm).
        const IID_PAR: [f64; 15] = [
            0.05623413251903,
            0.12589254117942,
            0.19952623149689,
            0.31622776601684,
            0.44668359215096,
            0.63095734448019,
            0.79432823472428,
            1.0,
            1.25892541179417,
            1.58489319246111,
            2.23872113856834,
            3.16227766016838,
            5.01187233627272,
            7.94328234724282,
            17.7827941003892,
        ];
        const ICC_INVQ: [f64; 8] = [1.0, 0.937, 0.84118, 0.60092, 0.36764, 0.0, -0.589, -1.0];
        for q in -7i32..=7 {
            for icc in 0..8usize {
                let c = IID_PAR[(7 + q) as usize];
                let c1 = 2f64.sqrt() / (1.0 + c * c).sqrt();
                let c2 = c * c1;
                let alpha = 0.5 * ICC_INVQ[icc].acos();
                let beta = alpha * (c1 - c2) * 0.5f64.sqrt();
                let ha = [
                    c2 * (beta + alpha).cos(),
                    c1 * (beta - alpha).cos(),
                    c2 * (beta + alpha).sin(),
                    c1 * (beta - alpha).sin(),
                ];
                let mut p = PsParams::default();
                p.iid_c[0][0] = SF[(7 + q) as usize];
                p.icc_cos[0][0] = COS_ALPHAS[icc];
                p.icc_sin[0][0] = SIN_ALPHAS[icc];
                let got = type_a(&p, 0, 0, false);
                for j in 0..4 {
                    assert!(
                        (got[j] as f64 - ha[j]).abs() < 1e-3,
                        "q={q} icc={icc} arr{j}: got {} want {}",
                        got[j],
                        ha[j]
                    );
                }
            }
        }
    }

    #[test]
    fn type_b_matches_ffmpeg_hb_over_all_iid_icc() {
        // Conformance (parallel to `type_a_matches_ffmpeg_ha_over_all_iid_icc`):
        // the table-based Type B construction (`icc_mode >= 3`) must reproduce
        // FFmpeg's `HB[iid][icc]` closed form (`aacps_tablegen.h`) for every
        // signed IID (±7) and ICC (0..7). Reference computed here with f64 libm
        // (host-only; shipping code stays table-driven / no libm), using the
        // exact ψ/γ derivation:
        //   rho   = max(icc_invq[icc], 0.05)
        //   alpha = 0.5·atan2(2·c·rho, c²−1)                (always in [0, π/2])
        //   mu    = sqrt(1 + (4·rho²−4)/(c + 1/c)²)
        //   gamma = atan(sqrt((1−mu)/(1+mu)))
        //   HB = [ √2·cosα·cosγ,  √2·sinα·cosγ, −√2·sinα·sinγ,  √2·cosα·sinγ ]
        // (our `type_b` stores cosα in `SINCOS_ALPHAS_B` and recovers sinα by
        // the exact mirror identity sinα(c) = cosα(1/c) — the `14−i` lookup.)
        const IID_PAR: [f64; 15] = [
            0.05623413251903,
            0.12589254117942,
            0.19952623149689,
            0.31622776601684,
            0.44668359215096,
            0.63095734448019,
            0.79432823472428,
            1.0,
            1.25892541179417,
            1.58489319246111,
            2.23872113856834,
            3.16227766016838,
            5.01187233627272,
            7.94328234724282,
            17.7827941003892,
        ];
        const ICC_INVQ: [f64; 8] = [1.0, 0.937, 0.84118, 0.60092, 0.36764, 0.0, -0.589, -1.0];
        let sqrt2 = 2f64.sqrt();
        for q in -7i32..=7 {
            for icc in 0..8usize {
                let c = IID_PAR[(7 + q) as usize];
                let rho = ICC_INVQ[icc].max(0.05);
                let alpha = 0.5 * (2.0 * c * rho).atan2(c * c - 1.0); // [0, π/2]
                let mu_lin = c + 1.0 / c;
                let mu = (1.0 + (4.0 * rho * rho - 4.0) / (mu_lin * mu_lin)).sqrt();
                let gamma = ((1.0 - mu) / (1.0 + mu)).sqrt().atan();
                let hb = [
                    sqrt2 * alpha.cos() * gamma.cos(),
                    sqrt2 * alpha.sin() * gamma.cos(),
                    -sqrt2 * alpha.sin() * gamma.sin(),
                    sqrt2 * alpha.cos() * gamma.sin(),
                ];
                let mut p = PsParams::default();
                p.iid_c[0][0] = SF[(7 + q) as usize];
                p.icc_cos[0][0] = COS_ALPHAS[icc];
                p.icc_sin[0][0] = SIN_ALPHAS[icc];
                let got = type_b(&p, 0, 0, false);
                for j in 0..4 {
                    assert!(
                        (got[j] as f64 - hb[j]).abs() < 1e-3,
                        "q={q} icc={icc} arr{j}: got {} want {}",
                        got[j],
                        hb[j]
                    );
                }
            }
        }
    }

    #[test]
    fn negate_ipd_mask_flips_imaginary_cross_term() {
        // NEGATE_IPD_MASK value path. FFmpeg's `stereo_processing` negates the
        // *imaginary interpolation anchor* (only) for the flattened bands
        // `k <= 1` in the 20-config (`aacps.c`). The negate acts on the START
        // anchor `H[1][e][b]`, not the target, so it is inert while that anchor
        // is zero — which is why the existing `ipdopd_makes_matrix_complex` test
        // (frame 0, envelope 0, cold anchor, band k=2 outside the negate set)
        // never exercises it.
        //
        // Here a 2-envelope layout makes envelope 1's imaginary START anchor the
        // (non-zero) matrix from envelope 0, and we compare two flattened bands
        // that map to the SAME parameter band (`k_to_i_20[1] == k_to_i_20[2] ==
        // 0`) but differ only in the negate: k=1 (negated, k<=1) vs k=2 (not
        // negated). At an intermediate slot the imaginary cross-term's sign
        // flips. Discriminating: drop the mask and the two bands are identical
        // (same param band, same input) — the sign-flip assertion fails.
        let mut layout = layout_1env_20();
        layout.num_env = 2;
        layout.border_position = [0, 16, 32, 0, 0, 0];
        let mut p = neutral_params_20(); // IID/ICC neutral -> real matrix [1,1,0,0]
        p.enable_ipdopd = true;
        let opd_q = 1usize; // (cos,sin) = π/4 -> non-zero opd_im
        let ipd_q = 2usize; // (cos,sin) = π/2
        for e in 0..2 {
            for b in 0..11 {
                p.opd_cos[e][b] = PCOS[opd_q];
                p.opd_sin[e][b] = PSIN[opd_q];
                p.ipd_cos[e][b] = PCOS[ipd_q];
                p.ipd_sin[e][b] = PSIN[ipd_q];
            }
        }
        // `neutral_params_20` only fills envelope 0 — give envelope 1 the same
        // neutral IID/ICC so its real matrix is also [1,1,0,0].
        for b in 0..20 {
            p.iid_c[1][b] = SF[7];
            p.icc_cos[1][b] = COS_ALPHAS[0];
            p.icc_sin[1][b] = SIN_ALPHAS[0];
        }

        let mut s = zero();
        let dd = zero();
        // k=1 and k=2 both map to param band 0; drive both columns identically.
        fill_col(&mut s, 1, Cpx { re: 1.0, im: 0.0 });
        fill_col(&mut s, 2, Cpx { re: 1.0, im: 0.0 });
        let mut xl = zero();
        let mut xr = zero();
        let mut st = MixState::default();
        stereo_mix(&s, &dd, &p, &layout, 0, &mut st, &mut xl, &mut xr);

        // Envelope 1 spans slots 16..32; sample slot 16 (m=0), where the anchor
        // (negated for k=1) still dominates the interpolation. With s=(1,0) and
        // d=0 the left output's imaginary part is exactly the interpolated
        // imaginary anchor hi[0].
        let slot = 16usize;
        let im_neg = xl[slot][1].im; // k=1: anchor negated
        let im_pos = xl[slot][2].im; // k=2: anchor not negated

        // Hand-derived: envelope 0 sets the imaginary anchor to opd_im at the
        // cold history (idx = opd_q); envelope 1's target opd_im uses the
        // advanced history (idx = opd_q·8 + opd_q). One interpolation step of
        // 1/16 from the (±) anchor toward the target.
        let v = PDIM[opd_q]; // env0 opd_im -> env1 imaginary START anchor
        let ti = PDIM[opd_q * 8 + opd_q]; // env1 opd_im target
        let inv = 1.0f32 / 16.0;
        let expect_pos = v + (ti - v) * inv; //  anchor +v -> ti
        let expect_neg = -v + (ti + v) * inv; // anchor -v -> ti
        assert!(
            (im_pos - expect_pos).abs() < 1e-4,
            "k=2 (non-negated) im={im_pos} want {expect_pos}"
        );
        assert!(
            (im_neg - expect_neg).abs() < 1e-4,
            "k=1 (negated) im={im_neg} want {expect_neg}"
        );
        // The mask genuinely flips the imaginary cross-term's sign.
        assert!(
            im_neg < 0.0 && im_pos > 0.0,
            "NEGATE_IPD_MASK must flip the imaginary cross-term sign: \
             k=1 im={im_neg}, k=2 im={im_pos}"
        );
    }

    #[test]
    fn no_panic_on_degenerate_and_transitioning_input() {
        // Variable-class layout whose last border stops short of 32 (leaves a
        // tail of unmixed slots), a zero-length middle envelope, an is34
        // toggle between calls (transition reset), and off-table garbage IID
        // — none may panic / index OOB / divide by zero.
        let mut st = MixState::default();
        let s = zero();
        let dd = zero();
        let mut xl = zero();
        let mut xr = zero();

        let mut l = layout_1env_20();
        l.num_env = 2;
        l.border_position = [0, 10, 10, 0, 0, 0]; // second envelope has zero length
        let mut p = neutral_params_20();
        p.iid_c[0][3] = 123.456; // off-table -> nearest fallback, no OOB
        stereo_mix(&s, &dd, &p, &l, 0, &mut st, &mut xl, &mut xr);

        // Now a 34-config (is34 transition) with IPD/OPD + Type B.
        let l2 = PsFrameLayout {
            num_env: 1,
            border_position: [0, 32, 0, 0, 0, 0],
            nr_iid_par: 34,
            nr_icc_par: 34,
            nr_ipdopd_par: 17,
            is34: true,
            iid_quant: true,
        };
        let mut p2 = PsParams {
            enable_ipdopd: true,
            ..Default::default()
        };
        for b in 0..34 {
            p2.iid_c[0][b] = SF_IID_FINE[15];
            p2.icc_cos[0][b] = COS_ALPHAS[4];
            p2.icc_sin[0][b] = SIN_ALPHAS[4];
        }
        stereo_mix(&s, &dd, &p2, &l2, 4, &mut st, &mut xl, &mut xr);
        // num_env == 0 (reuse-previous sentinel) is inert.
        let mut l3 = layout_1env_20();
        l3.num_env = 0;
        stereo_mix(&s, &dd, &p, &l3, 0, &mut st, &mut xl, &mut xr);
    }
}
