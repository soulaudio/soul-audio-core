//! PS hybrid filterbank (`ps-reference.md` §5) — analysis splits the lowest
//! QMF bands into finer hybrid sub-bands via a modulated complex FIR;
//! synthesis sums them back to plain 64-band QMF. Verified against FFmpeg
//! `libavcodec/aacps.c` (`ps_hybrid_analysis`, `hybrid6_cx`, `hybrid2_re`,
//! `hybrid_analysis`/`hybrid_synthesis`, `ff_k_to_i_20`) and faad2
//! `libfaad/ps_dec.c` (`channel_filter{2,4,8,12}`, `hybrid_analysis`).
//!
//! ## Strictly-causal analysis FIR (faad2 scheme)
//! Each split QMF band is convolved with a **13-tap complex FIR** whose sub-band
//! `q` coefficients are the modulated prototype
//! `filter[q][t] = proto[t]·exp(-i·2π(q+½)(t-6)/bands)` (the [`super::tables`]
//! `HYB_FILT_*`, Task 1). The FIR runs over a per-band **44-sample causal
//! working buffer** `work = [12 carried history ++ 32 current slots]`, the
//! current slots being `x38[6..38][b]` (`work[12+j] = x38[6+j][b]`). Output slot
//! `n` (0..32) is `s[n][flat] = Σ_{t=0..12} filter[q][t]·work[n+t]`, centred at
//! `work[n+6]`. This mirrors faad2 `hybrid_analysis` exactly
//! (`memcpy(work, buffer[band], 12)`; `work[12+n] = X[n+6][band]`;
//! `channel_filterN` reads `work[i..i+12]` centre `work[i+6]`). Every tap index
//! `n+t ≤ 43 < 44`, so all 32 output slots are exact — no look-ahead, no
//! zero-guard.
//!
//! ## Per-band history and the 6-slot group delay
//! `work[0..12)` is [`HybridHistory`]`.buf[b]`: the trailing 12 current slots
//! `x38[26..38][b]` (= `work[32..44)`) carried from the previous frame — faad2
//! `memcpy(buffer[band], work+32, 12)`. `x38` is `[6 history ++ 32 current]` (38
//! slots), but the FIR's past reach comes entirely from this per-band history,
//! NOT from `x38[0..6)`. The FIR centre `work[n+6]` equals `x38[n][b]` for
//! `n ≥ 6`, so a split band's output slot `n` sits on input `x38[n][b]`: current
//! QMF slot `n0` (at `x38[6+n0]`) lands at output slot `6+n0` — a **6-slot group
//! delay**. Pass-through QMF bands copy `s[n][flat] = x38[n][b]` to sit at the
//! same delay (the `passthrough_band_delayed_by_six` test: impulse at
//! `x38[6+n0]` → output slot `6+n0`). Output whose delay overflows the 32-slot
//! frame is carried into the next frame through `work[0..12)`.
use crate::formats::aac::ps::{HISTORY, N_QMF, N_SLOTS, N_SLOTS_HIST, NR_BANDS_MAX};
use crate::formats::aac::sbr::qmf::Cpx;

use super::bands::hybrid_split;
use super::tables::{HYB_FILT_4, HYB_FILT_6, HYB_FILT_8, HYB_FILT_12};

/// Persistent per-band 13-tap FIR memory (12 past samples × up to 5 split
/// bands), carried frame-to-frame. faad2 `hyb->buffer`.
#[derive(Clone, Copy, Default)]
pub(crate) struct HybridHistory {
    pub buf: [[Cpx; 12]; 5],
}

/// First pass-through QMF band per config (`is34`): the split region is QMF
/// bands `0..FIRST_PASSTHROUGH` (3 for 20-cfg, 5 for 34-cfg), everything above
/// copies through. §5 split map.
const FIRST_PASSTHROUGH: [usize; 2] = [3, 5];

/// Reorder from the pre-merged [`HYB_FILT_6`] sub-band order
/// (`[f8₀, f8₁, f8₂₊₅, f8₃₊₄, f8₆, f8₇]`, Task 1's `hyb_filt_6`) to FFmpeg
/// `hybrid6_cx`'s flattened output order (`[f8₆, f8₇, f8₀, f8₁, f8₂₊₅, f8₃₊₄]`):
/// flattened sub-band `p` uses `HYB_FILT_6[PERM6[p]]`. This is what
/// `ff_k_to_i_20` (Task 1 `FF_K_TO_I_20`) indexes, so the 20-cfg band-0 split
/// must emit sub-bands in exactly this order for Task 7/8's band mapping.
const PERM6: [usize; 6] = [4, 5, 0, 1, 2, 3];

/// 20-cfg 2-way split (QMF bands 1,2): faad2 `channel_filter2`/FFmpeg
/// `hybrid2_re` split a band into `in±op` via the real `g1_Q2` prototype
/// (non-centre even taps are zero). Materialised here as the two real 13-tap
/// filters — `HYB2_SUM` = `re_in + re_op`, `HYB2_DIFF` = `re_in − re_op` — so
/// the same 13-tap MAC handles them. `g1_Q2` half =
/// `[0, 0.01899487526049, 0, -0.07293139167538, 0, 0.30596630545168, 0.5]`.
const HYB2_SUM: [Cpx; 13] = real13([
    0.0,
    0.018_994_875,
    0.0,
    -0.072_931_39,
    0.0,
    0.305_966_3,
    0.5,
    0.305_966_3,
    0.0,
    -0.072_931_39,
    0.0,
    0.018_994_875,
    0.0,
]);
const HYB2_DIFF: [Cpx; 13] = real13([
    0.0,
    -0.018_994_875,
    0.0,
    0.072_931_39,
    0.0,
    -0.305_966_3,
    0.5,
    -0.305_966_3,
    0.0,
    0.072_931_39,
    0.0,
    -0.018_994_875,
    0.0,
]);

/// Build a 13-tap real filter as `[Cpx; 13]` (imag part 0) — `const fn` so the
/// 2-way tables above stay compile-time.
const fn real13(re: [f32; 13]) -> [Cpx; 13] {
    let mut out = [Cpx { re: 0.0, im: 0.0 }; 13];
    let mut t = 0;
    while t < 13 {
        out[t] = Cpx { re: re[t], im: 0.0 };
        t += 1;
    }
    out
}

/// One 13-tap complex FIR output over the causal working buffer:
/// `Σ_{t=0..12} filter[t]·work[n+t]`, centre `work[n+6]`. Output slot `n < 32`
/// so `n+12 ≤ 43 < 44` — the whole window is always in-bounds (strictly causal,
/// no zero-guard). `work` = `[12 history ++ 32 current]`, see module header.
#[inline]
fn fir13(filter: &[Cpx; 13], work: &[Cpx; 44], n: usize) -> Cpx {
    let mut acc = Cpx { re: 0.0, im: 0.0 };
    for (f, x) in filter.iter().zip(work[n..n + 13].iter()) {
        acc.re += f.re * x.re - f.im * x.im;
        acc.im += f.re * x.im + f.im * x.re;
    }
    acc
}

pub(crate) fn hybrid_analysis(
    x38: &[[Cpx; N_QMF]; N_SLOTS_HIST],
    is34: bool,
    hist: &mut HybridHistory,
    s: &mut [[Cpx; NR_BANDS_MAX]; N_SLOTS],
) {
    // Zero the whole flattened matrix first: for the 20-cfg the top bands
    // (71..91) are unused and must stay zero (analysis_zero_in_zero_out).
    for slot in s.iter_mut() {
        for c in slot.iter_mut() {
            *c = Cpx::default();
        }
    }

    let splits = hybrid_split(is34);

    // --- split bands: FIR-split each low QMF band into `bands` sub-bands ---
    let mut flat_off = 0usize;
    for (b, &bands) in splits.iter().enumerate() {
        // Build the 44-sample causal working buffer for this band: the 12-sample
        // per-band history carried from the previous frame, followed by the 32
        // current QMF slots `x38[6..38][b]` (faad2 `memcpy(work, buffer[band],
        // 12)`; `work[12+n] = X[n+6][band]`).
        let mut work = [Cpx::default(); 44];
        work[..12].copy_from_slice(&hist.buf[b]);
        for j in 0..N_SLOTS {
            work[12 + j] = x38[HISTORY + j][b];
        }

        for (n, s_row) in s.iter_mut().enumerate() {
            match bands {
                12 => {
                    for q in 0..12 {
                        s_row[flat_off + q] = fir13(&HYB_FILT_12[q], &work, n);
                    }
                }
                8 => {
                    for q in 0..8 {
                        s_row[flat_off + q] = fir13(&HYB_FILT_8[q], &work, n);
                    }
                }
                6 => {
                    // 20-cfg band 0: pre-merged HYB_FILT_6 emitted in FFmpeg's
                    // hybrid6_cx flattened order via PERM6.
                    for p in 0..6 {
                        s_row[flat_off + p] = fir13(&HYB_FILT_6[PERM6[p]], &work, n);
                    }
                }
                4 => {
                    for q in 0..4 {
                        s_row[flat_off + q] = fir13(&HYB_FILT_4[q], &work, n);
                    }
                }
                2 => {
                    // 20-cfg bands 1,2: FFmpeg `hybrid2_re` emits the sub-bands
                    // as (diff,sum) for band 1 (reverse=1) and (sum,diff) for
                    // band 2 (reverse=0).
                    let (f0, f1): (&[Cpx; 13], &[Cpx; 13]) = if b == 1 {
                        (&HYB2_DIFF, &HYB2_SUM)
                    } else {
                        (&HYB2_SUM, &HYB2_DIFF)
                    };
                    s_row[flat_off] = fir13(f0, &work, n);
                    s_row[flat_off + 1] = fir13(f1, &work, n);
                }
                _ => {}
            }
        }

        // Carry this frame's trailing 12 current slots as next frame's history
        // (faad2 `memcpy(buffer[band], work+32, 12)` — `work[32..44)` ==
        // `x38[26..38][b]`, i.e. exactly the slots that become `work[0..12)`
        // next frame).
        hist.buf[b].copy_from_slice(&work[N_SLOTS..N_SLOTS + 12]);

        flat_off += bands;
    }

    // --- pass-through QMF bands: 6-slot-delayed copy `s[n] = x38[n][b]`, which
    // aligns with the split bands' FIR group delay (centre `work[n+6]` =
    // `x38[n][b]`). flat_off is now Σ hybrid_split.
    let first_pt = FIRST_PASSTHROUGH[usize::from(is34)];
    // `b` is a QMF band index mapping to flattened band `flat`; enumerate does
    // not fit (one band index drives x38's inner dim and the flat offset).
    #[allow(clippy::needless_range_loop)]
    for b in first_pt..N_QMF {
        let flat = flat_off + (b - first_pt);
        for n in 0..N_SLOTS {
            s[n][flat] = x38[n][b];
        }
    }
}

pub(crate) fn hybrid_synthesis(
    x_hyb: &[[Cpx; NR_BANDS_MAX]; N_SLOTS],
    is34: bool,
    out: &mut [[Cpx; N_QMF]; N_SLOTS],
) {
    let splits = hybrid_split(is34);

    // --- split bands: sum each sub-band group back to its one QMF band ---
    let mut flat_off = 0usize;
    for (b, &bands) in splits.iter().enumerate() {
        for n in 0..N_SLOTS {
            let mut acc = Cpx::default();
            for q in 0..bands {
                let c = x_hyb[n][flat_off + q];
                acc.re += c.re;
                acc.im += c.im;
            }
            out[n][b] = acc;
        }
        flat_off += bands;
    }

    // --- pass-through: copy each flattened QMF band straight through ---
    let first_pt = FIRST_PASSTHROUGH[usize::from(is34)];
    #[allow(clippy::needless_range_loop)]
    for b in first_pt..N_QMF {
        let flat = flat_off + (b - first_pt);
        for n in 0..N_SLOTS {
            out[n][b] = x_hyb[n][flat];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::f32::consts::PI;

    fn mag2(c: &Cpx) -> f32 {
        c.re * c.re + c.im * c.im
    }

    #[test]
    fn analysis_zero_in_zero_out() {
        let x = [[Cpx::default(); N_QMF]; N_SLOTS_HIST];
        let mut hist = HybridHistory::default();
        let mut s = [[Cpx::default(); NR_BANDS_MAX]; N_SLOTS];
        hybrid_analysis(&x, true, &mut hist, &mut s);
        for slot in &s {
            for c in slot {
                assert_eq!(*c, Cpx::default());
            }
        }
    }

    #[test]
    fn passthrough_band_delayed_by_six() {
        // A unit impulse in a HIGH (pass-through) QMF band at input slot 6+n0
        // must appear in the corresponding flattened pass-through band at output
        // slot 6+n0: pass-through copies s[n] = x38[n][b], so current QMF slot n0
        // (living at x38[6+n0]) lands at output slot 6+n0 — the same 6-slot group
        // delay the split-band FIR carries. Pick a band well above the split
        // region so no FIR runs on it.
        let mut x = [[Cpx::default(); N_QMF]; N_SLOTS_HIST];
        let qmf_band = 40usize;
        let n0 = 3usize;
        x[6 + n0][qmf_band] = Cpx { re: 1.0, im: 0.0 };
        let mut hist = HybridHistory::default();
        let mut s = [[Cpx::default(); NR_BANDS_MAX]; N_SLOTS];
        hybrid_analysis(&x, true, &mut hist, &mut s);
        // flattened index of qmf_band = 32 (Σ hybrid) + (qmf_band - 5).
        let flat = 32 + (qmf_band - 5);
        assert!((s[6 + n0][flat].re - 1.0).abs() < 1e-5);
        // Localizes to exactly slot 6+n0 — the neighbouring slot is silent.
        assert!(s[6 + n0 + 1][flat].re.abs() < 1e-5);
    }

    #[test]
    fn split_band_tone_localizes_to_expected_subband() {
        // Drive the lowest (12-way, 34-cfg) split QMF band with a phasor whose
        // intra-band frequency centres on hybrid sub-band q0 — i.e. it matches
        // the modulated-prototype filter[q0][t] = proto[t]·exp(-i·2π(q0+½)(t-6)/12).
        // At q==q0 the modulation cancels the t-dependence, leaving Σ proto[t]
        // (the prototype's DC, its maximum), so sub-band q0 dominates the split
        // output. This is the verify-first reference-vector gate for the FIR.
        let bands = 12usize; // 34-cfg QMF band 0 split
        let q0 = 3usize;
        let mut x = [[Cpx::default(); N_QMF]; N_SLOTS_HIST];
        for (m, slot) in x.iter_mut().enumerate() {
            let theta = 2.0 * PI * (q0 as f32 + 0.5) * (m as f32) / bands as f32;
            slot[0] = Cpx {
                re: theta.cos(),
                im: theta.sin(),
            };
        }
        let mut hist = HybridHistory::default();
        let mut s = [[Cpx::default(); NR_BANDS_MAX]; N_SLOTS];
        hybrid_analysis(&x, true, &mut hist, &mut s);

        // Check a mid-frame output slot whose 44-sample causal window
        // work[n..n+12] lies entirely in the current region (n ≥ 12 ⇒ all 13
        // taps read pure phasor, cold-start history irrelevant), so the FIR sees
        // the steady-state tone and is exact.
        let n = 13usize;
        let row = &s[n];
        let (best_q, _) =
            row[..bands]
                .iter()
                .enumerate()
                .fold((0usize, -1.0f32), |(bq, be), (q, c)| {
                    let e = mag2(c);
                    if e > be { (q, e) } else { (bq, be) }
                });
        assert_eq!(
            best_q, q0,
            "expected sub-band {q0} to dominate, got {best_q}"
        );
        // Dominant sub-band should clearly beat every neighbour.
        let peak = mag2(&row[q0]);
        for (q, c) in row[..bands].iter().enumerate() {
            if q != q0 {
                assert!(peak > 4.0 * mag2(c), "sub-band {q0} not dominant over {q}");
            }
        }
    }

    #[test]
    fn cfg20_passthrough_layout_and_delay() {
        // Guards the 20-cfg (is34=false) split layout the brief's four tests do
        // not reach: Σ hybrid_split = 6+2+2 = 10, pass-through QMF band b → flat
        // 10 + (b-3), and unused flattened bands 71..91 stay zero.
        let mut x = [[Cpx::default(); N_QMF]; N_SLOTS_HIST];
        let qmf_band = 40usize;
        let n0 = 4usize;
        x[6 + n0][qmf_band] = Cpx { re: 0.0, im: 1.0 };
        let mut hist = HybridHistory::default();
        let mut s = [[Cpx::default(); NR_BANDS_MAX]; N_SLOTS];
        hybrid_analysis(&x, false, &mut hist, &mut s);
        let flat = 10 + (qmf_band - 3);
        assert!((s[6 + n0][flat].im - 1.0).abs() < 1e-5);
        // Round-trips back to the same QMF band under synthesis (same 6-delay).
        let mut out = [[Cpx::default(); N_QMF]; N_SLOTS];
        hybrid_synthesis(&s, false, &mut out);
        assert!((out[6 + n0][qmf_band].im - 1.0).abs() < 1e-5);
        // Flattened bands past NR_BANDS[20]=71 are untouched.
        for slot in &s {
            for c in &slot[71..NR_BANDS_MAX] {
                assert_eq!(*c, Cpx::default());
            }
        }
    }

    #[test]
    fn analysis_then_synthesis_roundtrips_passthrough() {
        // A pass-through QMF band round-trips analysis→synthesis to the same QMF
        // band at the 6-slot group delay: pass-through copies s[n] = x38[n][b]
        // and synthesis copies it straight back, so out[n][b] == x38[n][b].
        let qmf_band = 40usize;
        let mut x = [[Cpx::default(); N_QMF]; N_SLOTS_HIST];
        for (m, slot) in x.iter_mut().enumerate() {
            let theta = 2.0 * PI * 0.1 * m as f32;
            slot[qmf_band] = Cpx {
                re: theta.cos(),
                im: 0.5 * theta.sin(),
            };
        }
        let mut hist = HybridHistory::default();
        let mut s = [[Cpx::default(); NR_BANDS_MAX]; N_SLOTS];
        hybrid_analysis(&x, true, &mut hist, &mut s);
        let mut out = [[Cpx::default(); N_QMF]; N_SLOTS];
        hybrid_synthesis(&s, true, &mut out);
        for n in 0..N_SLOTS {
            let want = x[n][qmf_band];
            assert!((out[n][qmf_band].re - want.re).abs() < 1e-5);
            assert!((out[n][qmf_band].im - want.im).abs() < 1e-5);
        }
    }

    #[test]
    fn split_band_slot31_not_zero_guarded() {
        // Regression for the causal-FIR fix. A constant drive across a split QMF
        // band makes every fully-windowed output slot (n ≥ 12) compute the SAME
        // causal FIR value C·Σ_t filter[q][t]. The pre-fix NON-causal FIR reached
        // look-ahead taps x38[38..44) for slots 26..31 and zero-guarded them,
        // corrupting slot 31 so it disagreed with slot 13. Assert slot 31 equals
        // slot 13 exactly (proof the guard is gone) and carries real energy.
        let bands = 12usize; // 34-cfg QMF band 0 → flat 0..12
        let c = Cpx { re: 0.7, im: -0.3 };
        let mut x = [[Cpx::default(); N_QMF]; N_SLOTS_HIST];
        for slot in x.iter_mut() {
            slot[0] = c;
        }
        let mut hist = HybridHistory::default();
        let mut s = [[Cpx::default(); NR_BANDS_MAX]; N_SLOTS];
        hybrid_analysis(&x, true, &mut hist, &mut s);
        let (row31, row13) = (&s[31], &s[13]);
        let mut energy31 = 0.0f32;
        for (q, (a, b)) in row31[..bands].iter().zip(&row13[..bands]).enumerate() {
            assert!(
                (a.re - b.re).abs() < 1e-6,
                "slot 31 sub-band {q} re corrupted"
            );
            assert!(
                (a.im - b.im).abs() < 1e-6,
                "slot 31 sub-band {q} im corrupted"
            );
            energy31 += mag2(a);
        }
        assert!(
            energy31 > 1e-3,
            "slot 31 must carry real FIR energy, got {energy31}"
        );
    }
}
