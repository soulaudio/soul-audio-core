//! `sbr-reference.md` §10 step 5 — the low/high splice ("`sbr_x_gen`" in
//! FFmpeg `aacsbr_template.c`): build the 64-band synthesis matrix for the
//! current frame by taking the unchanged low band (`X_low[0..kx]`, delayed by
//! `T_HFADJ`) from the analysis QMF and the adjusted high band
//! (`Y[kx..kx+M]`) from [`super::hfadj::hf_adjustment`], zeroing everything
//! above `kx+M`.
//!
//! [`assemble`] itself is the **no-overlap** core (FFmpeg's `sbr_x_gen` with
//! `i_Temp == 0`): the current frame supplies every output slot. The ±1-frame
//! boundary carry (`i_Temp`/`Ypos` ping-pong — when the *previous* frame's
//! grid extended past slot 16, its `Y` slots `32..38` become the *first*
//! `i_Temp` output slots of this frame) is stitched in by the per-channel
//! driver in [`super`] after this call, from the previous frame's retained
//! `Y` (`SbrState`). See the driver's `assemble_overlap` for that step; this
//! function is the piece the task brief pins with an exact vector.
//!
//! **This is the PS seam (§2.4):** Phase 3 will intercept the assembled
//! matrix here (mono → L/R) before synthesis; Phase 2 passes it straight
//! through. The 6-slot history a 13-tap PS FIR needs is retained in
//! per-channel `SbrState`, so Phase 3 can reconstruct the full `X[38][64]`.

use super::freq_tables::FreqTables;
use super::qmf::{Cpx, MAX_SLOTS, N_ANA, N_SYN, T_HFADJ, TIME_SLOTS_RATE};

/// Splice the delayed low band (`x_low[s + T_HFADJ][0..kx]`) with the adjusted
/// high band (`y[s][kx..kx+M]`) into the 64-band synthesis matrix `x_out`
/// (§10 step 5, no-overlap `i_Temp == 0` form). Bands `>= kx+M` are zeroed.
///
/// `x_low` is `[slot][band]` (the analysis QMF's low band, current frame's 32
/// fresh slots placed at physical `[T_HFGEN, T_HFGEN+32)`, so the adjusted
/// region starts at `T_HFADJ`). `y` is `[slot][band]` with the high band
/// already written at absolute band index `kx..kx+M` (see
/// `hfadj::hf_adjustment`). Every index is clamped defensively — a malformed
/// `kx`/`m` can never index out of range.
pub(super) fn assemble(
    x_low: &[[Cpx; N_ANA]; MAX_SLOTS],
    y: &[[Cpx; N_SYN]; TIME_SLOTS_RATE],
    ft: &FreqTables,
    x_out: &mut [[Cpx; N_SYN]; TIME_SLOTS_RATE],
) {
    let kx = (ft.kx as usize).min(N_SYN);
    let kx_m = (ft.kx as usize + ft.m as usize).min(N_SYN);

    for (s, out_slot) in x_out.iter_mut().enumerate() {
        // Clear the whole 64-band slot (bands >= kx+M stay zero, §10 step 5).
        *out_slot = [Cpx::default(); N_SYN];

        // Low band 0..kx: the analysis QMF's low band, delayed by T_HFADJ.
        // x_low bands only span 0..N_ANA (32); the low band is always < kx <= 32.
        let phys = s + T_HFADJ;
        let low_hi = kx.min(N_ANA);
        if let Some(low) = x_low.get(phys) {
            out_slot[..low_hi].copy_from_slice(&low[..low_hi]);
        }

        // High band kx..kx+M: the adjusted high band from Y (absolute band idx).
        out_slot[kx..kx_m].copy_from_slice(&y[s][kx..kx_m]);
    }
}

#[cfg(test)]
// `assemble_splices_low_and_high` is transcribed verbatim from the task brief
// (`let mut ft = ...default(); ft.kx = 4; ft.m = 3;` and the `for s in 0..`
// loop that seeds both `x_low` and `y`); the two clippy lints those idioms
// trip are allowed here rather than deviating from the mandated test text.
#[allow(clippy::field_reassign_with_default, clippy::needless_range_loop)]
mod tests {
    use super::*;

    #[test]
    fn assemble_splices_low_and_high() {
        // Low band (below kx) copied through; high band (kx..kx+M) taken from Y.
        let mut ft = crate::formats::aac::sbr::freq_tables::FreqTables::default();
        ft.kx = 4;
        ft.m = 3; // low bands 0..4 from X_low, high bands 4..7 from Y
        let mut x_low = [[Cpx::default(); N_ANA]; MAX_SLOTS];
        let mut y = [[Cpx::default(); N_SYN]; TIME_SLOTS_RATE];
        for s in 0..TIME_SLOTS_RATE {
            x_low[s + T_HFADJ][2] = Cpx { re: 1.0, im: 0.0 }; // a low band
            y[s][5] = Cpx { re: 2.0, im: 0.0 }; // a high band
        }
        let mut out = [[Cpx::default(); N_SYN]; TIME_SLOTS_RATE];
        assemble(&x_low, &y, &ft, &mut out);
        assert_eq!(out[0][2], Cpx { re: 1.0, im: 0.0 }); // low passed through
        assert_eq!(out[0][5], Cpx { re: 2.0, im: 0.0 }); // high from Y
        assert_eq!(out[0][10], Cpx::default()); // above kx+M zeroed
    }

    #[test]
    fn assemble_zeroes_bands_at_and_above_kx_plus_m() {
        // A band exactly at kx+M (the first zeroed band) and one below kx that
        // has no x_low energy both stay zero; the boundary is [kx, kx+M).
        let mut ft = FreqTables::default();
        ft.kx = 5;
        ft.m = 4; // high 5..9
        let x_low = [[Cpx::default(); N_ANA]; MAX_SLOTS];
        let mut y = [[Cpx::default(); N_SYN]; TIME_SLOTS_RATE];
        for s in 0..TIME_SLOTS_RATE {
            y[s][9] = Cpx { re: 3.0, im: 0.0 }; // band 9 == kx+M, must be dropped
            y[s][8] = Cpx { re: 4.0, im: 0.0 }; // band 8 < kx+M, kept
        }
        let mut out = [[Cpx::default(); N_SYN]; TIME_SLOTS_RATE];
        assemble(&x_low, &y, &ft, &mut out);
        assert_eq!(out[3][8], Cpx { re: 4.0, im: 0.0 });
        assert_eq!(out[3][9], Cpx::default()); // == kx+M -> zeroed
        assert_eq!(out[3][0], Cpx::default()); // low band, no energy
    }
}
