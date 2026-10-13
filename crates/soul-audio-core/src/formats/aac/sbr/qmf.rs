//! `sbr-reference.md` §7 — the complex-exponential-modulated QMF filterbank:
//! the **analysis** bank (§7.1) that splits the AAC-LC core time signal into
//! 32 complex QMF subbands per time-slot, and the dual-rate 2× **synthesis**
//! bank (§7.2) that reassembles 64 complex bands per slot into 128 real output
//! samples (64 per slot at 2× rate). This is the SBR analogue of the LC
//! IMDCT and the heaviest DSP unit: the `(2n−0.5)`/`(2n+1)` modulation
//! offsets, the prototype fold, and the overlap-add strides are easy to get
//! subtly wrong, so the two localization tests below (a tone must land in its
//! expected band) are the verify-first gate everything downstream trusts.
//!
//! Both banks run as an **`N`-point FFT with pre/post rotations** (the standard
//! MDCT→FFT reduction), not the direct O(N²) modulation matrix: the complex-exp
//! modulation `Σ_k X[k]·e^{iπ(k+½)(2n±…)/…}` factors into a pre-twiddle, an
//! `e^{+iθ}`-kernel FFT ([`fft`]), and a post-twiddle (derived + validated
//! against the direct form in the tests — `fast_*_reduction_*` and
//! `fast_*_matches_direct`). Synthesis = a 128-pt FFT of the 64 pre-rotated
//! bands (zero-padded); analysis = a 64-pt FFT of the 64 pre-rotated `u[n]`.
//! Tables (`tables::{QMF_PROTO, QMF_FFT_TW, QMF_SYN_PRE, QMF_ANA_PRE,
//! QMF_ANA_POST}`, plus `QMF_SYN_COS/SIN` column 0 reused as the synthesis
//! post-twiddle) are `xtask`-generated. No `libm`, no `unsafe`, no alloc: every
//! buffer is a fixed-size array and every index is provably in range.

use super::tables::{
    QMF_ANA_POST, QMF_ANA_PRE, QMF_FFT_TW, QMF_PROTO, QMF_SYN_COS, QMF_SYN_PRE, QMF_SYN_SIN,
};

/// Complex QMF subband element, reused crate-wide (HF gen/adjust/assemble and,
/// in Phase 3, PS). Kept a plain `re`/`im` pair so it is trivially `Copy` and
/// zero-initialisable (`Default` == the additive identity used to zero the
/// subband buffers).
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub(crate) struct Cpx {
    pub re: f32,
    pub im: f32,
}

impl Cpx {
    /// Additive identity — used to zero the FFT scratch buffers.
    const ZERO: Cpx = Cpx { re: 0.0, im: 0.0 };
}

/// Analysis subbands (the analysis bank is always 32-band, §7.1).
pub(super) const N_ANA: usize = 32;
/// Synthesis subbands = width of the assembled complex matrix (§7.2).
pub(super) const N_SYN: usize = 64;
/// `numTimeSlotsRate` at 2× (dual-rate HE-AAC): QMF slots carrying data per
/// 1024-sample core frame (§0).
pub(super) const TIME_SLOTS_RATE: usize = 32;
/// LPC lookback slots the HF patcher needs before the current frame (§0).
pub(super) const T_HFGEN: usize = 8;
/// Slot offset into the X buffer where the adjusted region starts (§0).
pub(super) const T_HFADJ: usize = 2;
/// `numTimeSlotsRate + tHFGen` — max slot span of the low-band buffer (§0).
pub(super) const MAX_SLOTS: usize = TIME_SLOTS_RATE + T_HFGEN;
/// Width of the per-channel adjusted-high-band `Y` buffer (§10 / FFmpeg's
/// `Y[2][38][64]`): a single envelope's RATE-scaled span `2·t_env[e+1]` can
/// reach `2·19 = 38` when a FIXVAR/VARVAR trailing border overshoots the
/// nominal 16-slot frame, so `Y` holds all 38 slots even though synthesis
/// only emits the first [`TIME_SLOTS_RATE`] (=32). Slots `32..38` are the
/// frame-boundary overlap the driver carries into the next frame's splice
/// (the `i_Temp`/`Ypos` ping-pong, `sbr-reference.md` §10 / `aacsbr_template.c`
/// `sbr_x_gen`).
pub(super) const MAX_Y_SLOTS: usize = TIME_SLOTS_RATE + 6;

/// Analysis-bank input delay line: 320 reals of the core time signal, shifted
/// up by 32 each slot (§7.1 step 1 / §7.4). Carried across frames per channel.
#[derive(Clone)]
pub(super) struct QmfAnalysisState {
    pub x: [f32; 320],
}

impl Default for QmfAnalysisState {
    fn default() -> Self {
        Self { x: [0.0; 320] }
    }
}

/// Synthesis-bank delay line: 1280 reals, shifted up by 128 each slot (§7.2
/// step 2 / §7.4). Carried across frames per channel.
#[derive(Clone)]
pub(super) struct QmfSynthesisState {
    pub v: [f32; 1280],
}

impl Default for QmfSynthesisState {
    fn default() -> Self {
        Self { v: [0.0; 1280] }
    }
}

/// `v`-buffer tap offsets for the 10-tap synthesis overlap-add (§7.2 step 3,
/// FFmpeg `sbr_qmf_synthesis` / faad2 `sbr_qmf_synthesis_64`). Paired with the
/// prototype offsets `k + 64j` (j = 0..9).
const VOFF: [usize; 10] = [0, 192, 256, 448, 512, 704, 768, 960, 1024, 1216];

/// Analysis QMF (§7.1): 32 time-slots, each consuming 32 core samples,
/// producing 32 complex subbands per slot. Writes `x_low[slot][band]`,
/// `slot = 0..32`.
pub(super) fn qmf_analysis(
    core_pcm: &[f32; 1024],
    st: &mut QmfAnalysisState,
    x_low: &mut [[Cpx; N_ANA]; TIME_SLOTS_RATE],
) {
    for (slot, x_low_slot) in x_low.iter_mut().enumerate() {
        // 1. Delay line: shift x up by 32 (x[32..320] <- x[0..288]), then
        //    write the 32 new core samples time-reversed into x[0..32]
        //    (x[31] = oldest new, x[0] = newest new).
        st.x.copy_within(0..288, 32);
        let base = slot * 32;
        for n in 0..32 {
            st.x[n] = core_pcm[base + (31 - n)];
        }

        // 2. Window with the 320-tap even-index prototype subset, fold to 64.
        let mut u = [0.0f32; 64];
        for (n, u_n) in u.iter_mut().enumerate() {
            let mut acc = 0.0f32;
            for j in 0..5 {
                let m = n + 64 * j;
                acc += st.x[m] * QMF_PROTO[2 * m];
            }
            *u_n = acc;
        }

        // 3. Modulate to 32 complex subbands via the FFT reduction (same result
        //    as the direct `X[k] = 2·Σ_n u[n]·e^{+iπ(k+½)(2n−0.5)/64}` — the
        //    FFmpeg/faad2 absolute-phase convention — at O(N log N)):
        //      u'[n] = u[n]·e^{+iπ(2n−0.5)/128};  T = FFT₆₄(u');
        //      X[k]  = 2·e^{−iπk/128}·T[k],  k = 0..32.
        //    The absolute per-band phase matters because SBR HF-gen copies
        //    X_low[p] up to band k; see `xtask/src/sbr_tables.rs`.
        let mut t = [Cpx::ZERO; 64];
        for (n, slot) in t.iter_mut().enumerate() {
            let pre = QMF_ANA_PRE[n];
            *slot = Cpx {
                re: u[n] * pre[0],
                im: u[n] * pre[1],
            };
        }
        fft(&mut t);
        for (k, out_band) in x_low_slot.iter_mut().enumerate().take(N_ANA) {
            let post = QMF_ANA_POST[k]; // 2·e^{−iπk/128}
            *out_band = cmul(t[k], post[0], post[1]);
        }
    }
}

/// Complex multiply `z·(c + i·s)`.
#[inline]
fn cmul(z: Cpx, c: f32, s: f32) -> Cpx {
    Cpx {
        re: z.re * c - z.im * s,
        im: z.re * s + z.im * c,
    }
}

/// In-place radix-2 DIT complex FFT with the `e^{+iθ}` (inverse) kernel and **no**
/// `1/N` scaling — the convention the QMF reduction folds its scale into
/// elsewhere. `a.len()` is a power of two in `{64, 128}`; twiddles come from the
/// shared base-128 [`QMF_FFT_TW`] (`cos`,`sin`), where a length-`len` sub-DFT
/// reads stride `128/len`, an index always `< 64` and so provably in range.
#[allow(clippy::needless_range_loop)]
fn fft(a: &mut [Cpx]) {
    let n = a.len();
    if n < 2 {
        return;
    }
    // Bit-reversal permutation.
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            a.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let half = len / 2;
        let stride = 128 / len;
        let mut start = 0;
        while start < n {
            for k in 0..half {
                let tw = QMF_FFT_TW[k * stride];
                let b = a[start + k + half];
                let wr = b.re * tw[0] - b.im * tw[1];
                let wi = b.re * tw[1] + b.im * tw[0];
                let u = a[start + k];
                a[start + k] = Cpx {
                    re: u.re + wr,
                    im: u.im + wi,
                };
                a[start + k + half] = Cpx {
                    re: u.re - wr,
                    im: u.im - wi,
                };
            }
            start += len;
        }
        len <<= 1;
    }
}

/// Synthesis QMF (§7.2, dual-rate 2×): from 32 assembled 64-band complex slots
/// → 2048 real time samples (64 per slot). `x[slot][band]`.
pub(super) fn qmf_synthesis(
    x: &[[Cpx; N_SYN]; TIME_SLOTS_RATE],
    st: &mut QmfSynthesisState,
    out: &mut [f32; 2048],
) {
    for (slot, bands) in x.iter().enumerate() {
        // 1. Modulate the 64 complex bands to 128 real via the FFT reduction
        //    (same result as the direct `w[n] = (1/64)·Σ_k re·SYN_COS − im·SYN_SIN`,
        //    i.e. `−(1/64)·Re{ Σ_k X[k]·e^{+iπ(k+½)(2n+1)/128} }`, at O(N log N)):
        //      Y[k] = X[k]·e^{+iπk/128}  (k = 0..64, zero-padded to 128);
        //      S    = FFT₁₂₈(Y);
        //      w[n] = −(1/64)·Re{ e^{+iπ(2n+1)/256}·S[n] }.
        //    The post-twiddle e^{+iπ(2n+1)/256} == (−SYN_COS[n][0], −SYN_SIN[n][0]).
        let mut y = [Cpx::ZERO; 128];
        for (k, y_k) in y.iter_mut().enumerate().take(N_SYN) {
            let pre = QMF_SYN_PRE[k];
            *y_k = cmul(bands[k], pre[0], pre[1]);
        }
        fft(&mut y);
        let mut w = [0.0f32; 128];
        for (n, w_n) in w.iter_mut().enumerate() {
            // Re{ P·S }, P = (−SYN_COS[n][0], −SYN_SIN[n][0]); scale −1/64.
            let (pre, pim) = (-QMF_SYN_COS[n][0], -QMF_SYN_SIN[n][0]);
            let re_ps = y[n].re * pre - y[n].im * pim;
            *w_n = -re_ps * (1.0 / 64.0);
        }

        // 2. Shift v up by 128 (v[128..1280] <- v[0..1152]), write w into
        //    v[0..128].
        st.v.copy_within(0..1152, 128);
        st.v[0..128].copy_from_slice(&w);

        // 3. Windowed overlap-add: out[k] = Σ_{j<10} v[k+Voff[j]]·PROTO[k+64j].
        let out_base = slot * 64;
        for k in 0..64 {
            let mut acc = 0.0f32;
            for j in 0..10 {
                acc += st.v[k + VOFF[j]] * QMF_PROTO[k + 64 * j];
            }
            out[out_base + k] = acc;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The direct-form modulation tables — the FFT path no longer imports these
    // (analysis) in production; the oracles + reduction tests below still need them.
    use super::super::tables::{QMF_ANA_COS, QMF_ANA_SIN};

    /// Direct O(N²) analysis modulation — the reference the fast [`qmf_analysis`]
    /// is validated against (the `imdct_naive` pattern). Byte-for-byte the former
    /// production body: delay-line + fold + `X[k] = 2·Σ_n u[n]·(COS+iSIN)`.
    #[allow(clippy::needless_range_loop)] // `n` indexes both `u` and the table row
    fn qmf_analysis_direct(
        core_pcm: &[f32; 1024],
        st: &mut QmfAnalysisState,
        x_low: &mut [[Cpx; N_ANA]; TIME_SLOTS_RATE],
    ) {
        for (slot, x_low_slot) in x_low.iter_mut().enumerate() {
            st.x.copy_within(0..288, 32);
            let base = slot * 32;
            for n in 0..32 {
                st.x[n] = core_pcm[base + (31 - n)];
            }
            let mut u = [0.0f32; 64];
            for (n, u_n) in u.iter_mut().enumerate() {
                let mut acc = 0.0f32;
                for j in 0..5 {
                    let m = n + 64 * j;
                    acc += st.x[m] * QMF_PROTO[2 * m];
                }
                *u_n = acc;
            }
            for k in 0..N_ANA {
                let (mut re, mut im) = (0.0f32, 0.0f32);
                for n in 0..64 {
                    re += u[n] * QMF_ANA_COS[k][n];
                    im += u[n] * QMF_ANA_SIN[k][n];
                }
                x_low_slot[k] = Cpx {
                    re: 2.0 * re,
                    im: 2.0 * im,
                };
            }
        }
    }

    /// Direct O(N²) synthesis modulation — the reference for the fast
    /// [`qmf_synthesis`]. Former production body (modulation + overlap-add).
    fn qmf_synthesis_direct(
        x: &[[Cpx; N_SYN]; TIME_SLOTS_RATE],
        st: &mut QmfSynthesisState,
        out: &mut [f32; 2048],
    ) {
        for (slot, bands) in x.iter().enumerate() {
            let mut w = [0.0f32; 128];
            for (n, w_n) in w.iter_mut().enumerate() {
                let mut acc = 0.0f32;
                for k in 0..N_SYN {
                    acc += bands[k].re * QMF_SYN_COS[n][k] - bands[k].im * QMF_SYN_SIN[n][k];
                }
                *w_n = acc * (1.0 / 64.0);
            }
            st.v.copy_within(0..1152, 128);
            st.v[0..128].copy_from_slice(&w);
            let out_base = slot * 64;
            for k in 0..64 {
                let mut acc = 0.0f32;
                for j in 0..10 {
                    acc += st.v[k + VOFF[j]] * QMF_PROTO[k + 64 * j];
                }
                out[out_base + k] = acc;
            }
        }
    }

    /// The fast FFT analysis must reproduce the direct form end-to-end (all 32
    /// slots × 32 bands), within f32 FFT-vs-direct-sum rounding.
    #[test]
    fn fast_analysis_matches_direct() {
        let mut s = 0x2468_ace0u32;
        let mut rnd = || {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        let core: [f32; 1024] = core::array::from_fn(|_| rnd());
        let mut xf = [[Cpx::ZERO; N_ANA]; TIME_SLOTS_RATE];
        let mut xd = [[Cpx::ZERO; N_ANA]; TIME_SLOTS_RATE];
        qmf_analysis(&core, &mut QmfAnalysisState::default(), &mut xf);
        qmf_analysis_direct(&core, &mut QmfAnalysisState::default(), &mut xd);
        let (mut maxerr, mut maxabs) = (0.0f32, 0.0f32);
        for (sf, sd) in xf.iter().zip(&xd) {
            for (cf, cd) in sf.iter().zip(sd) {
                maxerr = maxerr.max((cf.re - cd.re).abs()).max((cf.im - cd.im).abs());
                maxabs = maxabs.max(cd.re.abs()).max(cd.im.abs());
            }
        }
        assert!(
            maxerr <= 1e-3 * (maxabs + 1e-6),
            "analysis fast vs direct maxerr {maxerr} (maxabs {maxabs})"
        );
    }

    /// The fast FFT synthesis must reproduce the direct form end-to-end (2048
    /// output samples), within f32 FFT-vs-direct-sum rounding.
    #[test]
    fn fast_synthesis_matches_direct() {
        let mut s = 0x1357_9bdfu32;
        let mut rnd = || {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        let mut x = [[Cpx::ZERO; N_SYN]; TIME_SLOTS_RATE];
        for slot in x.iter_mut() {
            for b in slot.iter_mut() {
                *b = Cpx {
                    re: rnd(),
                    im: rnd(),
                };
            }
        }
        let mut of = [0.0f32; 2048];
        let mut od = [0.0f32; 2048];
        qmf_synthesis(&x, &mut QmfSynthesisState::default(), &mut of);
        qmf_synthesis_direct(&x, &mut QmfSynthesisState::default(), &mut od);
        let (mut maxerr, mut maxabs) = (0.0f32, 0.0f32);
        for (f, d) in of.iter().zip(&od) {
            maxerr = maxerr.max((f - d).abs());
            maxabs = maxabs.max(d.abs());
        }
        assert!(
            maxerr <= 1e-3 * (maxabs + 1e-6),
            "synthesis fast vs direct maxerr {maxerr} (maxabs {maxabs})"
        );
    }

    #[test]
    fn qmf_modulation_matches_ffmpeg_absolute_phase_convention() {
        // Regression for the SBR HF-reconstruction fidelity bug. Both QMF banks
        // must use FFmpeg's/faad2's EXACT absolute-phase convention (derived
        // from faad2's dct4_kernel form on the identical windowed/folded u[n]):
        //   analysis  X[k] = 2·Σ_n u[n]·exp(+i·π·(k+½)·(2n−0.5)/64)
        //   synthesis  E[n][k] = −exp(+i·π·(k+½)·(2n+1)/128)
        // The prior self-consistent pair (analysis 2n−96.5, synthesis +（2n−63))
        // round-trips and matches FFmpeg on the low-band passthrough (the
        // analysis/synthesis per-band phases cancel there — and the localization
        // tests below never check absolute phase), but SBR HF generation copies
        // X_low[p] up to band k. The patch is only correct in FFmpeg's phase
        // convention, so a mismatched convention corrupts the high band by
        // phi_k^syn·phi_p^ana — the "high-band shape wrong, error grows with
        // frequency" failure. Pin phase-sensitive cells that distinguish the
        // correct convention from the old self-consistent one.
        use core::f64::consts::PI;
        for &(k, n) in &[(3usize, 7usize), (10, 0), (20, 40), (31, 63)] {
            let want = PI * (k as f64 + 0.5) * (2.0 * n as f64 - 0.5) / 64.0;
            let old = PI * (k as f64 + 0.5) * (2.0 * n as f64 - 96.5) / 64.0;
            assert!(
                (f64::from(QMF_ANA_COS[k][n]) - want.cos()).abs() < 1e-5
                    && (f64::from(QMF_ANA_SIN[k][n]) - want.sin()).abs() < 1e-5,
                "QMF_ANA[{k}][{n}] must use FFmpeg's (2n-0.5) analysis offset"
            );
            assert!(
                (want.cos() - old.cos()).abs() > 0.05 || (want.sin() - old.sin()).abs() > 0.05,
                "analysis cell ({k},{n}) must distinguish (2n-0.5) from the old (2n-96.5)"
            );
        }
        for &(n, k) in &[(0usize, 0usize), (1, 10), (40, 20), (127, 63)] {
            let want = PI * (k as f64 + 0.5) * (2.0 * n as f64 + 1.0) / 128.0;
            let old = PI * (k as f64 + 0.5) * (2.0 * n as f64 - 63.0) / 128.0;
            // Synthesis carries a global sign flip (−exp): tables are −cos/−sin.
            assert!(
                (f64::from(QMF_SYN_COS[n][k]) - (-want.cos())).abs() < 1e-5
                    && (f64::from(QMF_SYN_SIN[n][k]) - (-want.sin())).abs() < 1e-5,
                "QMF_SYN[{n}][{k}] must use FFmpeg's −(2n+1) synthesis convention"
            );
            assert!(
                (-want.cos() - old.cos()).abs() > 0.05 || (-want.sin() - old.sin()).abs() > 0.05,
                "synthesis cell ({n},{k}) must distinguish −(2n+1) from the old +(2n−63)"
            );
        }
    }

    /// Validate the fast-QMF **reduction math** (not yet an FFT, just the
    /// pre/post twiddles + DFT sum) against the production direct-form tables,
    /// for both banks. This is the correctness gate for the fast rewrite: if
    /// these formulas reproduce the direct modulation, an ordinary FFT computing
    /// the same DFT will too. Twiddles here use std trig (test-only); production
    /// will read them from xtask-generated tables.
    #[test]
    fn fast_synthesis_reduction_matches_direct_modulation() {
        use core::f64::consts::PI;
        let cs = |a: f64| (a.cos(), a.sin());
        // Deterministic pseudo-random 64-band complex input.
        let mut x = 0x1234_5678u32;
        let mut rnd = || {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 8) as f64 / (1u32 << 24) as f64 - 0.5
        };
        let re: [f64; 64] = core::array::from_fn(|_| rnd());
        let im: [f64; 64] = core::array::from_fn(|_| rnd());

        // Direct production modulation: w[n] = (1/64) Σ_k re·SYN_COS - im·SYN_SIN.
        let mut direct = [0.0f64; 128];
        for (n, dn) in direct.iter_mut().enumerate() {
            let mut acc = 0.0;
            for k in 0..64 {
                acc += re[k] * f64::from(QMF_SYN_COS[n][k]) - im[k] * f64::from(QMF_SYN_SIN[n][k]);
            }
            *dn = acc / 64.0;
        }

        // Fast reduction: Y[k] = X[k]·e^{iπk/128}; S[n] = Σ_k Y[k]·e^{i2πkn/128};
        // w[n] = -(1/64)·Re{ e^{iπ(2n+1)/256}·S[n] }.
        let mut fast = [0.0f64; 128];
        for (n, fnv) in fast.iter_mut().enumerate() {
            let (mut sre, mut sim) = (0.0f64, 0.0f64);
            for k in 0..64 {
                // Y[k] = (re + i·im)·e^{iπk/128}
                let (wc, ws) = cs(PI * k as f64 / 128.0);
                let yre = re[k] * wc - im[k] * ws;
                let yim = re[k] * ws + im[k] * wc;
                // ·e^{i2πkn/128}
                let (ec, es) = cs(2.0 * PI * (k * n) as f64 / 128.0);
                sre += yre * ec - yim * es;
                sim += yre * es + yim * ec;
            }
            // post P[n] = e^{iπ(2n+1)/256}
            let (pc, ps) = cs(PI * (2 * n + 1) as f64 / 256.0);
            let re_ps = sre * pc - sim * ps;
            *fnv = -re_ps / 64.0;
        }

        let maxerr = direct
            .iter()
            .zip(&fast)
            .fold(0.0f64, |m, (a, b)| m.max((a - b).abs()));
        // Floor is the f32 table precision (~1e-7); a derivation bug would be O(0.1).
        assert!(
            maxerr < 1e-5,
            "synthesis reduction vs direct maxerr {maxerr}"
        );
    }

    #[test]
    #[allow(clippy::needless_range_loop)] // `n` indexes both `u` and the table row
    fn fast_analysis_reduction_matches_direct_modulation() {
        use core::f64::consts::PI;
        let cs = |a: f64| (a.cos(), a.sin());
        let mut x = 0x9e37_79b9u32;
        let mut rnd = || {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 8) as f64 / (1u32 << 24) as f64 - 0.5
        };
        let u: [f64; 64] = core::array::from_fn(|_| rnd());

        // Direct production modulation: X[k] = 2·(Σ u·ANA_COS, Σ u·ANA_SIN).
        let mut dre = [0.0f64; 32];
        let mut dim = [0.0f64; 32];
        for k in 0..32 {
            let (mut r, mut i) = (0.0f64, 0.0f64);
            for n in 0..64 {
                r += u[n] * f64::from(QMF_ANA_COS[k][n]);
                i += u[n] * f64::from(QMF_ANA_SIN[k][n]);
            }
            dre[k] = 2.0 * r;
            dim[k] = 2.0 * i;
        }

        // Fast reduction: u'[n] = u[n]·e^{iπ(2n-0.5)/128}; T[k] = Σ u'·e^{i2πnk/64};
        // X[k] = 2·e^{-iπk/128}·T[k].
        for k in 0..32 {
            let (mut tre, mut tim) = (0.0f64, 0.0f64);
            for n in 0..64 {
                let (pc, ps) = cs(PI * (2.0 * n as f64 - 0.5) / 128.0);
                let upre = u[n] * pc;
                let upim = u[n] * ps;
                let (ec, es) = cs(2.0 * PI * (n * k) as f64 / 64.0);
                tre += upre * ec - upim * es;
                tim += upre * es + upim * ec;
            }
            let (qc, qs) = cs(-PI * k as f64 / 128.0);
            let xre = 2.0 * (tre * qc - tim * qs);
            let xim = 2.0 * (tre * qs + tim * qc);
            // Floor is the f32 table precision (~1e-7); a derivation bug is O(0.1).
            assert!(
                (xre - dre[k]).abs() < 1e-5 && (xim - dim[k]).abs() < 1e-5,
                "analysis reduction vs direct mismatch at k={k}: ({xre},{xim}) vs ({},{})",
                dre[k],
                dim[k]
            );
        }
    }

    fn dominant_bin(sig: &[f32]) -> usize {
        // naive DFT magnitude argmax over sig.len() bins (std math ok in tests).
        let n = sig.len();
        let mut best = (0usize, -1.0f64);
        for k in 0..n / 2 {
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for (i, &s) in sig.iter().enumerate() {
                let a = -2.0 * core::f64::consts::PI * (k * i) as f64 / n as f64;
                re += f64::from(s) * a.cos();
                im += f64::from(s) * a.sin();
            }
            let m = re * re + im * im;
            if m > best.1 {
                best = (k, m);
            }
        }
        best.0
    }
    #[test]
    fn analysis_zero_in_zero_out() {
        let mut st = QmfAnalysisState::default();
        let mut xl = [[Cpx::default(); N_ANA]; TIME_SLOTS_RATE];
        qmf_analysis(&[0.0; 1024], &mut st, &mut xl);
        for slot in &xl {
            for c in slot {
                assert_eq!(*c, Cpx::default());
            }
        }
    }
    #[test]
    fn analysis_localizes_tone_to_expected_band() {
        // A tone at the centre of analysis band kb → that band should dominate.
        let kb = 5usize;
        let mut core = [0.0f32; 1024];
        for (i, s) in core.iter_mut().enumerate() {
            // band centre (normalised): (kb+0.5)/(2*N_ANA) cycles/sample.
            let f = (kb as f64 + 0.5) / (2.0 * N_ANA as f64);
            *s = (2.0 * core::f64::consts::PI * f * i as f64).sin() as f32;
        }
        let mut st = QmfAnalysisState::default();
        let mut xl = [[Cpx::default(); N_ANA]; TIME_SLOTS_RATE];
        qmf_analysis(&core, &mut st, &mut xl);
        // Steady-state slot (after the delay line fills): magnitude peaks at kb.
        let slot = &xl[TIME_SLOTS_RATE - 1];
        let mag = |c: &Cpx| c.re * c.re + c.im * c.im;
        let peak = (0..N_ANA)
            .max_by(|&a, &b| mag(&slot[a]).partial_cmp(&mag(&slot[b])).unwrap())
            .unwrap();
        assert_eq!(peak, kb);
    }
    #[test]
    fn synthesis_single_band_produces_tone() {
        // Drive synthesis band kb with a unit-magnitude signal across all slots
        // → the 2048-sample output is a sinusoid whose dominant bin matches the
        // band centre (kb+0.5)/128.
        //
        // The stimulus MUST be the actual band-kb subband signal: a unit-
        // magnitude phasor rotating one quarter-turn per slot (X[l][kb] = i^l),
        // not a DC constant. This is the SBR analogue of "excite one bin". A
        // *constant* (im=0) drive is a DC-subband, which every complex-QMF
        // synthesis (faad2 `sbr_qmf_synthesis_64`, FFmpeg, ISO 4.6.18.4 — none
        // apply a per-slot phase) reconstructs at the band *edge* kb/128, not
        // the centre — the +0.5 half-band is carried by the subband rotation,
        // exactly as a real band-centre tone produces after analysis (see
        // `synthesis_roundtrip_localizes_core_tone`). i^l has unit magnitude in
        // every slot, so "unit magnitude across all slots" still holds.
        let kb = 10usize;
        let phasor = [
            Cpx { re: 1.0, im: 0.0 },
            Cpx { re: 0.0, im: 1.0 },
            Cpx { re: -1.0, im: 0.0 },
            Cpx { re: 0.0, im: -1.0 },
        ];
        let mut x = [[Cpx::default(); N_SYN]; TIME_SLOTS_RATE];
        for (l, slot) in x.iter_mut().enumerate() {
            slot[kb] = phasor[l % 4];
        }
        let mut st = QmfSynthesisState::default();
        let mut out = [0.0f32; 2048];
        qmf_synthesis(&x, &mut st, &mut out);
        // Expected output frequency = (kb+0.5)/128 cycles/sample → bin over 2048.
        let expect_bin = (((kb as f64 + 0.5) / 128.0) * 2048.0).round() as usize;
        let got = dominant_bin(&out[512..1536]); // steady-state window
        let scaled = ((got as f64) * 2048.0 / 1024.0).round() as usize;
        assert!(
            (scaled as i64 - expect_bin as i64).abs() <= 2,
            "got {scaled}, expect {expect_bin}"
        );
    }

    #[test]
    fn synthesis_roundtrip_localizes_core_tone() {
        // Stimulus-free localization proof: analyse a real core tone at the
        // centre of analysis band kb, splice the 32 analysis bands into the low
        // 32 synthesis bands (zero-pad the high 32), synthesise, and confirm the
        // 2× output lands on the same physical frequency — band centre
        // (kb+0.5)/128 at the doubled rate. This exercises analysis and
        // synthesis together with the real (rotating) subband samples, so it
        // pins the modulation offsets of BOTH banks with no hand-chosen phase.
        let kb = 5usize;
        let mut core = [0.0f32; 1024];
        for (i, s) in core.iter_mut().enumerate() {
            let f = (kb as f64 + 0.5) / (2.0 * N_ANA as f64);
            *s = (2.0 * core::f64::consts::PI * f * i as f64).sin() as f32;
        }
        let mut ast = QmfAnalysisState::default();
        let mut xl = [[Cpx::default(); N_ANA]; TIME_SLOTS_RATE];
        qmf_analysis(&core, &mut ast, &mut xl);

        let mut xs = [[Cpx::default(); N_SYN]; TIME_SLOTS_RATE];
        for (l, slot) in xs.iter_mut().enumerate() {
            slot[..N_ANA].copy_from_slice(&xl[l]);
        }
        let mut sst = QmfSynthesisState::default();
        let mut out = [0.0f32; 2048];
        qmf_synthesis(&xs, &mut sst, &mut out);

        let expect_bin = (((kb as f64 + 0.5) / 128.0) * 2048.0).round() as usize;
        let got = dominant_bin(&out[512..1536]); // steady-state window
        let scaled = ((got as f64) * 2048.0 / 1024.0).round() as usize;
        assert!(
            (scaled as i64 - expect_bin as i64).abs() <= 2,
            "got {scaled}, expect {expect_bin}"
        );
    }

    #[test]
    fn synthesis_roundtrip_preserves_unit_amplitude() {
        // Gain-pinning proof (the localization tests above are gain-blind: they
        // only check the peak band/bin, never amplitude). A unit-amplitude core
        // tone at the centre of analysis band kb, round-tripped through
        // analysis → synthesis (same splice as `synthesis_roundtrip_localizes_
        // core_tone`), must come back out at ~unit amplitude. faad2 scales
        // analysis output by 2.0 (`QMF_RE`/`QMF_IM`) and synthesis output by
        // 1.0/64.0; without both factors applied stage-for-stage the round-trip
        // gain is off by a constant (net ~32x too loud pre-fix, since the two
        // banks' un-normalised matrices contribute the missing 2*64 factor
        // between them).
        let kb = 5usize;
        let mut core = [0.0f32; 1024];
        for (i, s) in core.iter_mut().enumerate() {
            let f = (kb as f64 + 0.5) / (2.0 * N_ANA as f64);
            *s = (2.0 * core::f64::consts::PI * f * i as f64).sin() as f32;
        }
        let mut ast = QmfAnalysisState::default();
        let mut xl = [[Cpx::default(); N_ANA]; TIME_SLOTS_RATE];
        qmf_analysis(&core, &mut ast, &mut xl);

        let mut xs = [[Cpx::default(); N_SYN]; TIME_SLOTS_RATE];
        for (l, slot) in xs.iter_mut().enumerate() {
            slot[..N_ANA].copy_from_slice(&xl[l]);
        }
        let mut sst = QmfSynthesisState::default();
        let mut out = [0.0f32; 2048];
        qmf_synthesis(&xs, &mut sst, &mut out);

        // Steady-state window (delay lines filled, boundary-free).
        let window = &out[512..1536];
        let peak = window.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        // Input core tone has unit amplitude (a unit-magnitude sine); the
        // round-trip through analysis+synthesis at the 2x dual rate should
        // reproduce it within a generous tolerance (a single band-centre
        // tone's low-band passthrough is ~unity but not exact due to the
        // prototype filter's frequency response).
        assert!(
            (0.85..=1.15).contains(&peak),
            "round-trip peak amplitude {peak}, expected ~1.0 (unity gain)"
        );
    }
}
