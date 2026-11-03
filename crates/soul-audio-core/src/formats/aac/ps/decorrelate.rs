//! PS decorrelation (`ps-reference.md` §6) — turns the mono hybrid-domain
//! signal `s` into a maximally-decorrelated `d` (same energy envelope,
//! independent fine structure) that, alongside the direct `s`, feeds the
//! stereo mixer (Task 8). One decorrelator per flattened processing band:
//!
//! * **Below `NR_ALLPASS_BANDS` (`{30,50}`):** a 3-link serial allpass
//!   reverberator — base delay `z^-2` + fractional phase `Phi_Fract[k]`, then
//!   three links `(Q_Fract[k][m]·z^-d(m) − a(m)·g(k)) / (1 − a(m)·g(k)·
//!   Q_Fract[k][m]·z^-d(m))`, `d(m)={3,4,5}`, `a(m)=FILTER_A[m]`, decay
//!   `g(k)`.
//! * **At/above it:** a pure delay — 14 slots for flattened `k <
//!   SHORT_DELAY_BAND` (`{42,62}` ≡ QMF band 35), else 1.
//! * **Transient (peak-decay) attenuation** per parameter band ducks `d`
//!   during attacks so the reverb tail does not smear transients.
//!
//! ## Verified against the two ground-truth decoders
//! The allpass difference equation is the float kernel from FFmpeg
//! `libavcodec/aacpsdsp_template.c` `ps_decorrelate_c` (`in = delay·phi`; per
//! link `new = link_delay·Q − a·g·in`, store `in + a·g·new`), cross-checked
//! against faad2 `libfaad/ps_dec.c` `ps_decorrelate` (`tmp = tmp0·Q_Fract −
//! g_filt·R0`, `tmp2 = R0 + g_filt·tmp`). The flattened band walk
//! (`k = 0..NR_BANDS`, allpass / delay-14 / delay-1 split, transient indexed
//! by `k_to_i[k]`) mirrors FFmpeg `aacps.c` `decorrelation()`. The persistent
//! ring buffers + write indices are faad2's (`delay_Qmf`/`delay_SubQmf` +
//! `delay_buf_index_*`); see [`DecorrState`].
//!
//! ## `k` vs `k_to_i[k]` (the classic trap)
//! The allpass tables (`PHI_FRACT_*`, `Q_FRACT_ALLPASS_*`) and the decay
//! `g(k)` are indexed by the **flattened band `k`**; the transient
//! energy/gain is indexed by the **parameter band `i = k_to_i[k]`**. Mixing
//! the two silently corrupts the reverb, so they are kept distinct below.
//!
//! ## No panic / no OOB / no infinite loop (firmware invariant)
//! Every ring index is `(start + n) mod len` with `len ∈ {2,14,3,4,5,1}` — a
//! compile-time-bounded value that can never exceed its array. The transient
//! divide is guarded: the quotient is only taken when `gamma·P_diff > nrg ≥
//! 0`, so the denominator is provably `> 0` (`P_diff == 0 ⇒ G_trans = 1.0`,
//! no NaN). No `libm`, no `unsafe`, no alloc: all state is fixed by-value.
use crate::formats::aac::ps::{N_SLOTS, NR_BANDS_MAX};
use crate::formats::aac::sbr::qmf::Cpx;

use super::bands::{
    DECAY_CUTOFF, NR_ALLPASS_BANDS, NR_BANDS, NR_PAR_BANDS, SHORT_DELAY_BAND, hybrid_split, k_to_i,
};
use super::tables::{
    DELAY_D, FILTER_A, NUM_SAMPLE_DELAY_SER, PHI_FRACT_QMF, PHI_FRACT_SUBQMF20, PHI_FRACT_SUBQMF34,
    Q_FRACT_ALLPASS_QMF, Q_FRACT_ALLPASS_SUBQMF20, Q_FRACT_ALLPASS_SUBQMF34,
};

/// Number of serial allpass links (`PS_AP_LINKS` / `NO_ALLPASS_LINKS`, §6).
const N_LINKS: usize = 3;
/// Base-delay ring length — the allpass `z^-2` (base delay `D = 2`, §6).
const BASE_RING: usize = 2;
/// Longest pure-delay ring — `delay_D`'s 14-slot value (`delay_qmf` height).
const MAX_DELAY_RING: usize = 14;
/// Decay-slope per band above the cutoff (`DECAY_SLOPE`, §6/V6).
const DECAY_SLOPE: f32 = 0.05;
/// Transient peak-hold decay (`alpha_decay` / `peak_decay_factor`, §6/V6).
const ALPHA_DECAY: f32 = 0.765_928_3;
/// Transient smoothing coefficient (`alpha_smooth`, §6/V6).
const ALPHA_SMOOTH: f32 = 0.25;
/// Transient impact factor (`gamma` / `transient_impact`, §6/V6).
const GAMMA: f32 = 1.5;

// `ring` layout (the write indices faad2 persists as `saved_delay`,
// `delay_buf_index_delay` and `delay_buf_index_ser[3]`). All advance uniformly
// (once per slot, independent of band), so a single shared counter per role
// keeps every band in lockstep — the delay-1 bands collapse to index 0 via
// `mod delay_D`, the delay-14 bands share one `mod 14` counter, exactly as in
// faad2 where the per-band indices stay identical because every band processes
// the same 32 slots.
/// `ring[R_BASE]` — allpass base-delay write index (`mod 2`, faad2
/// `saved_delay`), shared by the QMF and hybrid-sub-band allpass rings.
const R_BASE: usize = 0;
/// `ring[R_DELAY]` — pure-delay write index (`mod delay_D[q]`, faad2
/// `delay_buf_index_delay`), QMF-only.
const R_DELAY: usize = 1;
/// `ring[R_SER + m]` — serial-link `m` write index (`mod {3,4,5}`, faad2
/// `delay_buf_index_ser[m]`), shared by QMF and hybrid-sub-band rings.
const R_SER: usize = 2;

/// Persistent decorrelation state (rings + transient), carried frame-to-frame
/// (Task 9 threads it like Task 6's `HybridHistory`). Sized to the 34-cfg
/// worst case, fixed by-value (`ps-reference.md` §6/§8 buffer inventory).
///
/// Ring dimensions (faad2 `ps_dec.h`, reconciled with §8):
/// * `delay_qmf[14][64]` — the shared QMF delay line: rows `0..2` hold the
///   allpass `z^-2` base delay, rows `0..delay_D[q]` (≤14) the pure delay.
/// * `delay_subqmf[2][32]` — the hybrid-sub-band allpass base delay (hybrid
///   sub-bands are always allpass, never pure-delay ⇒ 2 rows suffice).
/// * `delay_qmf_ser[3][5][64]` / `delay_subqmf_ser[3][5][32]` — the 3 serial
///   links × ≤5-sample link delay (`{3,4,5}`) × bands.
///
/// `ring: [usize; 5]` holds exactly the five write indices faad2 keeps:
/// `[base, pure-delay, ser0, ser1, ser2]` (see the `R_*` constants). A
/// zeroed default is the correct cold start (silence before frame 0).
#[derive(Clone, Copy)]
pub(crate) struct DecorrState {
    /// Base delay (QMF bands), rows `0..2` used for the allpass `z^-2`, rows
    /// `0..delay_D[q]` for the pure delay.
    pub delay_qmf: [[Cpx; 64]; 14],
    /// Base delay, hybrid sub-bands (always allpass ⇒ `z^-2` only).
    pub delay_subqmf: [[Cpx; 32]; 2],
    /// Serial-link delay lines, QMF bands: `[link][≤5 samples][band]`.
    pub delay_qmf_ser: [[[Cpx; 64]; 5]; 3],
    /// Serial-link delay lines, hybrid sub-bands.
    pub delay_subqmf_ser: [[[Cpx; 32]; 5]; 3],
    /// Ring write indices `[base, pure_delay, ser0, ser1, ser2]`.
    pub ring: [usize; 5],
    /// `P_PeakDecayNrg` — per-parameter-band transient peak-hold energy.
    pub peak_decay: [f32; 34],
    /// `P_prev` — per-parameter-band smoothed energy.
    pub p_prev: [f32; 34],
    /// `P_SmoothPeakDecayDiffNrg_prev` — per-band smoothed peak-minus-energy.
    pub diff_prev: [f32; 34],
}

impl Default for DecorrState {
    fn default() -> Self {
        Self {
            delay_qmf: [[Cpx { re: 0.0, im: 0.0 }; 64]; 14],
            delay_subqmf: [[Cpx { re: 0.0, im: 0.0 }; 32]; 2],
            delay_qmf_ser: [[[Cpx { re: 0.0, im: 0.0 }; 64]; 5]; 3],
            delay_subqmf_ser: [[[Cpx { re: 0.0, im: 0.0 }; 32]; 5]; 3],
            ring: [0; 5],
            peak_decay: [0.0; 34],
            p_prev: [0.0; 34],
            diff_prev: [0.0; 34],
        }
    }
}

#[inline]
fn cmul(a: Cpx, b: Cpx) -> Cpx {
    Cpx {
        re: a.re * b.re - a.im * b.im,
        im: a.re * b.im + a.im * b.re,
    }
}

#[inline]
fn cscale(s: f32, c: Cpx) -> Cpx {
    Cpx {
        re: s * c.re,
        im: s * c.im,
    }
}

#[inline]
fn csub(a: Cpx, b: Cpx) -> Cpx {
    Cpx {
        re: a.re - b.re,
        im: a.im - b.im,
    }
}

#[inline]
fn cadd(a: Cpx, b: Cpx) -> Cpx {
    Cpx {
        re: a.re + b.re,
        im: a.im + b.im,
    }
}

#[inline]
fn cmag2(c: Cpx) -> f32 {
    c.re * c.re + c.im * c.im
}

/// One slot of the 3-link serial allpass (FFmpeg `ps_decorrelate_c`, faad2
/// allpass branch). Generic over the band-row width `NB` (64 QMF / 32 hybrid)
/// and base-ring height `H` (14 QMF / 2 hybrid) so the QMF and SubQmf paths
/// share one implementation. Returns the allpass output before the transient
/// gain. `ag[m] = FILTER_A[m]·g(k)`.
#[inline]
#[allow(clippy::too_many_arguments)] // one DSP kernel; splitting hurts clarity
fn allpass_slot<const NB: usize, const H: usize>(
    x: Cpx,
    phi: Cpx,
    qf: &[Cpx; 3],
    ag: [f32; 3],
    base: &mut [[Cpx; NB]; H],
    ser: &mut [[[Cpx; NB]; 5]; 3],
    band: usize,
    base_idx: usize,
    ser_idx: &[usize; 3],
) -> Cpx {
    // Base delay z^-2, then the fractional phase rotation Phi_Fract[k].
    let delayed = base[base_idx][band];
    base[base_idx][band] = x;
    let mut in_c = cmul(delayed, phi);

    for m in 0..N_LINKS {
        let idx = ser_idx[m];
        let link_delay = ser[m][idx][band]; // z^-d(m)
        let apd = in_c; // feed-forward numerator input
        // new = Q_Fract·z^-d(m) − a·g·in
        let new_in = csub(cmul(link_delay, qf[m]), cscale(ag[m], in_c));
        // store = in + a·g·new  (read again d(m) slots later)
        ser[m][idx][band] = cadd(apd, cscale(ag[m], new_in));
        in_c = new_in;
    }
    in_c
}

/// Decorrelate (§6): `d[] = allpass/delay + transient reverb of s[]` (the mono
/// hybrid-domain signal). Both `s` (direct) and `d` (decorrelated) feed
/// mixing. Never panics (transient divide guarded → 1.0).
pub(crate) fn decorrelate(
    s: &[[Cpx; NR_BANDS_MAX]; N_SLOTS],
    is34: bool,
    st: &mut DecorrState,
    d: &mut [[Cpx; NR_BANDS_MAX]; N_SLOTS],
) {
    let cfg = usize::from(is34);
    let nr_bands = NR_BANDS[cfg];
    let nr_allpass = NR_ALLPASS_BANDS[cfg];
    let short_delay = SHORT_DELAY_BAND[cfg];
    let decay_cutoff = DECAY_CUTOFF[cfg];
    let nr_par = NR_PAR_BANDS[cfg];

    // Flattened hybrid-sub-band count (Σ split) and the flattened→QMF-band
    // offset: for k ≥ n_hyb the physical QMF band is q = k − offset (§4/§5,
    // "faad2 vs FFmpeg" #2: k≥10(20)/≥32(34) ⇒ QMF band k−7/k−27).
    let splits = hybrid_split(is34);
    let first_pt = splits.len(); // first pass-through QMF band (3 or 5)
    let n_hyb: usize = splits.iter().sum(); // 10 or 32
    let offset = n_hyb - first_pt; // 7 or 27

    // Zero the whole output first (top bands 71..91 unused in 20-cfg).
    for slot in d.iter_mut() {
        for c in slot.iter_mut() {
            *c = Cpx { re: 0.0, im: 0.0 };
        }
    }

    // --- Transient detection (FFmpeg decorrelation(): power → transient_gain)
    // power[i][n] = Σ_{k: k_to_i[k]=i} |s[k][n]|²
    let mut power = [[0.0f32; N_SLOTS]; 34];
    // `s` is [slot][band] row-major, so the band index `k` must drive the outer
    // loop over the (per-slot) inner array — an index loop, not an iterator.
    #[allow(clippy::needless_range_loop)]
    for k in 0..nr_bands {
        let i = k_to_i(k, is34);
        for n in 0..N_SLOTS {
            power[i][n] += cmag2(s[n][k]);
        }
    }
    // Per parameter band, run the peak-decay/smoothing recursion, updating the
    // persistent state and producing the per-slot gain.
    let mut gain = [[1.0f32; N_SLOTS]; 34];
    for i in 0..nr_par {
        let mut peak = st.peak_decay[i];
        let mut psmooth = st.p_prev[i];
        let mut pdiff = st.diff_prev[i];
        for n in 0..N_SLOTS {
            let p = power[i][n];
            // peak-hold decay: P_peak = max(P_peak·alpha_decay, P)
            let decayed = peak * ALPHA_DECAY;
            peak = if decayed > p { decayed } else { p };
            // nrg = P_prev + alpha_smooth·(P − P_prev)
            psmooth += ALPHA_SMOOTH * (p - psmooth);
            // P_diff = P_diff_prev + alpha_smooth·(P_peak − P − P_diff_prev)
            pdiff += ALPHA_SMOOTH * (peak - p - pdiff);
            // [NOPANIC] G_trans = (gamma·P_diff ≤ nrg) ? 1 : nrg/(gamma·P_diff).
            // The else branch requires gamma·P_diff > nrg ≥ 0 ⇒ denom > 0, so
            // P_diff == 0 always takes the `1.0` branch (no divide, no NaN).
            let denom = GAMMA * pdiff;
            gain[i][n] = if denom <= psmooth {
                1.0
            } else {
                psmooth / denom
            };
        }
        st.peak_decay[i] = peak;
        st.p_prev[i] = psmooth;
        st.diff_prev[i] = pdiff;
    }

    // --- Decorrelation + transient reduction ---
    // Snapshot the frame-start ring indices (every band walks from the same
    // start; the stored end value is the shared start + 32 mod ring length).
    let base_start = st.ring[R_BASE];
    let delay_start = st.ring[R_DELAY];
    let ser_start = [st.ring[R_SER], st.ring[R_SER + 1], st.ring[R_SER + 2]];

    for k in 0..nr_bands {
        let i = k_to_i(k, is34);
        let is_hyb = k < n_hyb;

        if k < nr_allpass {
            // 3-link serial allpass. Decay g(k) = clip(1 − 0.05·(k−cutoff),0,1)
            // (= 1 for k ≤ cutoff, incl. all hybrid sub-bands).
            let g = (1.0 - DECAY_SLOPE * (k as f32 - decay_cutoff as f32)).clamp(0.0, 1.0);
            let ag = [
                FILTER_A[0] as f32 * g,
                FILTER_A[1] as f32 * g,
                FILTER_A[2] as f32 * g,
            ];
            for n in 0..N_SLOTS {
                let base_idx = (base_start + n) % BASE_RING;
                let ser_idx = [
                    (ser_start[0] + n) % NUM_SAMPLE_DELAY_SER[0] as usize,
                    (ser_start[1] + n) % NUM_SAMPLE_DELAY_SER[1] as usize,
                    (ser_start[2] + n) % NUM_SAMPLE_DELAY_SER[2] as usize,
                ];
                let out = if is_hyb {
                    // Hybrid sub-band: SubQmf phase tables + delay_subqmf rings.
                    let phi = if is34 {
                        PHI_FRACT_SUBQMF34[k]
                    } else {
                        PHI_FRACT_SUBQMF20[k]
                    };
                    let qf = if is34 {
                        &Q_FRACT_ALLPASS_SUBQMF34[k]
                    } else {
                        &Q_FRACT_ALLPASS_SUBQMF20[k]
                    };
                    allpass_slot(
                        s[n][k],
                        phi,
                        qf,
                        ag,
                        &mut st.delay_subqmf,
                        &mut st.delay_subqmf_ser,
                        k,
                        base_idx,
                        &ser_idx,
                    )
                } else {
                    // QMF band: Qmf phase tables + delay_qmf rings, band q.
                    let q = k - offset;
                    allpass_slot(
                        s[n][k],
                        PHI_FRACT_QMF[q],
                        &Q_FRACT_ALLPASS_QMF[q],
                        ag,
                        &mut st.delay_qmf,
                        &mut st.delay_qmf_ser,
                        q,
                        base_idx,
                        &ser_idx,
                    )
                };
                d[n][k] = cscale(gain[i][n], out);
            }
        } else {
            // Pure delay (QMF bands only): DELAY_D[q] is 14 for q < 35, else 1,
            // which coincides exactly with the flattened SHORT_DELAY_BAND
            // threshold (q = 35 ≡ k = SHORT_DELAY_BAND).
            let q = k - offset;
            let dlen = DELAY_D[q] as usize;
            debug_assert_eq!(dlen, if k < short_delay { 14 } else { 1 });
            for n in 0..N_SLOTS {
                let idx = (delay_start + n) % dlen;
                let delayed = st.delay_qmf[idx][q];
                st.delay_qmf[idx][q] = s[n][k];
                d[n][k] = cscale(gain[i][n], delayed);
            }
        }
    }

    // Store the advanced ring indices (uniform +32 across every band).
    st.ring[R_BASE] = (base_start + N_SLOTS) % BASE_RING;
    st.ring[R_DELAY] = (delay_start + N_SLOTS) % MAX_DELAY_RING;
    for m in 0..N_LINKS {
        st.ring[R_SER + m] = (ser_start[m] + N_SLOTS) % NUM_SAMPLE_DELAY_SER[m] as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replicate the §6 transient recursion over a per-slot power series (the
    /// same math the implementation runs), for verifying `d = signal·G_trans`.
    fn transient_gain(power: &[f32], mut peak: f32, mut psmooth: f32, mut pdiff: f32) -> Vec<f32> {
        let mut g = Vec::with_capacity(power.len());
        for &p in power {
            let decayed = peak * ALPHA_DECAY;
            peak = if decayed > p { decayed } else { p };
            psmooth += ALPHA_SMOOTH * (p - psmooth);
            pdiff += ALPHA_SMOOTH * (peak - p - pdiff);
            let denom = GAMMA * pdiff;
            g.push(if denom <= psmooth {
                1.0
            } else {
                psmooth / denom
            });
        }
        g
    }

    fn zero_frame() -> [[Cpx; NR_BANDS_MAX]; N_SLOTS] {
        [[Cpx { re: 0.0, im: 0.0 }; NR_BANDS_MAX]; N_SLOTS]
    }

    #[test]
    fn pure_delay_band_delays_impulse() {
        // is34: band k=55 is a pure-delay-14 band (NR_ALLPASS_BANDS=50 ≤ 55 <
        // SHORT_DELAY_BAND=62; QMF band q=55−27=28, delay_D[28]=14). A unit
        // impulse at slot 25 of frame 1 re-emerges 14 slots later at global
        // slot 39 = slot 7 of frame 2 (ring-buffered across the frame). The
        // transient ducks the re-emerged sample by G_trans at that slot, which
        // we compute independently from the impulse's power series.
        let is34 = true;
        let k = 55usize;
        assert!(k >= NR_ALLPASS_BANDS[1] && k < SHORT_DELAY_BAND[1]);
        let imp_slot = 25usize;

        let mut st = DecorrState::default();
        let mut f1 = zero_frame();
        f1[imp_slot][k] = Cpx { re: 1.0, im: 0.0 };
        let mut d1 = zero_frame();
        decorrelate(&f1, is34, &mut st, &mut d1);

        // Frame 1: impulse (slot 25) + delay 14 → slot 39 > 31, nothing emerges
        // in-frame. Every output slot is silent.
        for (n, slot) in d1.iter().enumerate() {
            assert!(cmag2(slot[k]) < 1e-12, "frame1 slot {n} should be silent");
        }

        // Frame 2: all-zero input; the delayed impulse surfaces at slot 7.
        let f2 = zero_frame();
        let mut d2 = zero_frame();
        decorrelate(&f2, is34, &mut st, &mut d2);

        // Independent G_trans series over the 2-frame power (impulse at global
        // slot 25, silence elsewhere) — power[i] = |s[k]|² since only band k
        // carries energy in its parameter band.
        let mut power = [0.0f32; 2 * N_SLOTS];
        power[imp_slot] = 1.0;
        let g = transient_gain(&power, 0.0, 0.0, 0.0);
        let out_global = imp_slot + 14; // 39
        let out_slot = out_global - N_SLOTS; // 7
        let expected = g[out_global]; // impulse magnitude 1.0 × G_trans

        assert!(expected > 0.0, "transient gain must stay positive");
        assert!(
            (d2[out_slot][k].re - expected).abs() < 1e-6,
            "delayed impulse: got {}, want {expected}",
            d2[out_slot][k].re
        );
        assert!(d2[out_slot][k].im.abs() < 1e-6);
        // Localizes to exactly slot 7; neighbours are silent.
        for (n, slot) in d2.iter().enumerate() {
            if n != out_slot {
                assert!(
                    cmag2(slot[k]) < 1e-12,
                    "frame2 slot {n} should be silent (impulse only at {out_slot})"
                );
            }
        }
    }

    #[test]
    fn allpass_impulse_matches_reference() {
        // THE verify-first gate. is34: band k=32 is the first QMF allpass band
        // (n_hyb=32 ≤ 32 < NR_ALLPASS_BANDS=50; QMF band q=32−27=5). k=32 ≤
        // DECAY_CUTOFF=32 ⇒ g(k)=1, isolating the pure allpass structure.
        //
        // A unit impulse (slot 0) drives the 3-link serial allpass. The base
        // delay makes the first output land at slot 2. Hand-evaluating the
        // link recursion out[n]=Q·stored[n−d]−a·p[n], stored[n]=p[n]+a·out[n]
        // (a=FILTER_A[m], d(m)={3,4,5}, input phi·δ[n−2]) gives, before the
        // transient gain:
        //   out[2] = −a0·a1·a2·phi
        //   out[5] =  a1·a2·(1−a0²)·(Q0·phi)
        //   out[6] =  a0·a2·(1−a1²)·(Q1·phi)
        //   out[7] =  a0·a1·(1−a2²)·(Q2·phi)
        //   out[0]=out[1]=out[3]=out[4]=0
        let is34 = true;
        let k = 32usize;
        let q = 5usize;
        assert!(k < NR_ALLPASS_BANDS[1]);

        let mut st = DecorrState::default();
        let mut f = zero_frame();
        f[0][k] = Cpx { re: 1.0, im: 0.0 };
        let mut d = zero_frame();
        decorrelate(&f, is34, &mut st, &mut d);

        let phi = PHI_FRACT_QMF[q];
        let qf = Q_FRACT_ALLPASS_QMF[q];
        let a = [FILTER_A[0] as f32, FILTER_A[1] as f32, FILTER_A[2] as f32];
        // Reference difference-equation outputs (g=1).
        let out2 = cscale(-a[0] * a[1] * a[2], phi);
        let out5 = cscale(a[1] * a[2] * (1.0 - a[0] * a[0]), cmul(qf[0], phi));
        let out6 = cscale(a[0] * a[2] * (1.0 - a[1] * a[1]), cmul(qf[1], phi));
        let out7 = cscale(a[0] * a[1] * (1.0 - a[2] * a[2]), cmul(qf[2], phi));

        // Transient gain from the impulse's power series (power = δ[0]).
        let mut power = [0.0f32; N_SLOTS];
        power[0] = 1.0;
        let g = transient_gain(&power, 0.0, 0.0, 0.0);

        let approx = |got: Cpx, want: Cpx, gn: f32| {
            let w = cscale(gn, want);
            assert!(
                (got.re - w.re).abs() < 1e-5 && (got.im - w.im).abs() < 1e-5,
                "got {got:?}, want {w:?}"
            );
        };
        // Silent before the base delay and between the first reflections.
        for &n in &[0usize, 1, 3, 4] {
            assert!(cmag2(d[n][k]) < 1e-10, "slot {n} must be silent");
        }
        approx(d[2][k], out2, g[2]);
        approx(d[5][k], out5, g[5]);
        approx(d[6][k], out6, g[6]);
        approx(d[7][k], out7, g[7]);

        // Allpass ⇒ energy preserved: the un-ducked response out[n] = d[n]/G[n]
        // (G>0 everywhere) has total energy ≈ |phi|²·|input|² = 1 over the tail.
        let mut energy = 0.0f32;
        for n in 0..N_SLOTS {
            if g[n] > 1e-4 {
                let out = cscale(1.0 / g[n], d[n][k]);
                energy += cmag2(out);
            }
        }
        assert!(
            (0.90..=1.10).contains(&energy),
            "allpass tail energy {energy} not ≈ 1.0"
        );
    }

    #[test]
    fn transient_attenuates_and_never_divides_by_zero() {
        // is34: band k=70 is a pure-delay-1 band (k ≥ SHORT_DELAY_BAND=62; QMF
        // band q=70−27=43, delay_D[43]=1 ⇒ d[n]=s[n−1]·G_trans[n]). A big
        // energy spike at slot 0 followed by a small constant makes G_trans<1
        // on the following slots (reverb tail ducked), then recover.
        let is34 = true;
        let k = 70usize;
        assert!(k >= SHORT_DELAY_BAND[1]);
        let big = 10.0f32;
        let small = 0.1f32;

        let mut st = DecorrState::default();
        let mut f = zero_frame();
        f[0][k] = Cpx { re: big, im: 0.0 };
        for slot in f.iter_mut().skip(1) {
            slot[k] = Cpx { re: small, im: 0.0 };
        }
        let mut d = zero_frame();
        decorrelate(&f, is34, &mut st, &mut d);

        // Independent gain series (power[0]=big², power[1..]=small²).
        let mut power = [0.0f32; N_SLOTS];
        power[0] = big * big;
        for p in power.iter_mut().skip(1) {
            *p = small * small;
        }
        let g = transient_gain(&power, 0.0, 0.0, 0.0);

        // For n ≥ 2 the (delayed) input is the small constant, so d[n]=small·G.
        // The spike ducks the following slots; the ducking deepens as the
        // peak-decay memory builds, reaches a trough, then recovers as the peak
        // decays away.
        assert!(g[2] < 0.99, "spike must duck the following slots (G[2]<1)");
        assert!(g[22] < g[2], "ducking deepens after the spike");
        assert!(
            g[N_SLOTS - 1] > g[22],
            "gain recovers as the transient peak decays"
        );
        for n in 2..N_SLOTS {
            let want = small * g[n];
            assert!(
                (d[n][k].re - want).abs() < 1e-6,
                "slot {n}: d={} want {want}",
                d[n][k].re
            );
            assert!(!d[n][k].re.is_nan() && !d[n][k].im.is_nan());
        }

        // Silent band: P_diff==0 ⇒ G_trans==1, no NaN / no divide-by-zero.
        let mut st2 = DecorrState::default();
        let silent = zero_frame();
        let mut d2 = zero_frame();
        decorrelate(&silent, is34, &mut st2, &mut d2);
        for slot in &d2 {
            for c in slot {
                assert!(c.re == 0.0 && c.im == 0.0, "silent → exactly zero, no NaN");
                assert!(!c.re.is_nan() && !c.im.is_nan());
            }
        }
    }
}
