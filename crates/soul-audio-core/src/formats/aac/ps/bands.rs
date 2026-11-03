//! 10 ↔ 20 ↔ 34 stereo-band mapping + FFmpeg-authoritative config constants
//! (`ps-reference.md` §4/§5).
//!
//! Parameters are transmitted/expanded to the WORKING resolution (20 or 34
//! parameter bands, §2/§4 -- see [`super::params`]) but the hybrid
//! filterbank, decorrelator and mixer all operate on the wider **flattened
//! processing-band** space (`NR_BANDS = {71, 91}`, §5): hybrid sub-bands of
//! the lowest few QMF bands, followed by the pass-through QMF bands. This
//! module carries the flattened-band -> working-parameter-band lookup
//! ([`k_to_i`], backed by the V3 `FF_K_TO_I_20`/`FF_K_TO_I_34` tables) plus
//! the handful of FFmpeg-authoritative thresholds (`NR_ALLPASS_BANDS`,
//! `DECAY_CUTOFF`, `SHORT_DELAY_BAND`) that the decorrelator (§6) keys off,
//! and the hybrid split layout ([`hybrid_split`], §5) that both the hybrid
//! filterbank and the decorrelator/mixer need to agree on.

use super::tables::{FF_K_TO_I_20, FF_K_TO_I_34};

/// Flattened processing-band count, indexed by `is34` (§5): 71 for the
/// 20-parameter-band config, 91 for the 34-parameter-band config.
pub(crate) const NR_BANDS: [usize; 2] = [71, 91];
/// Working parameter-band resolution, indexed by `is34` (§2/§4).
pub(crate) const NR_PAR_BANDS: [usize; 2] = [20, 34];
/// IPD/OPD parameter-band resolution, indexed by `is34` (§2/§4) -- IPD/OPD
/// only ever live on the lowest `NR_IPDOPD_BANDS[is34]` parameter bands.
pub(crate) const NR_IPDOPD_BANDS: [usize; 2] = [11, 17];
/// Flattened-band threshold below which the decorrelator applies the 3-link
/// allpass reverberator; at/above it, a pure delay is used instead (§6).
pub(crate) const NR_ALLPASS_BANDS: [usize; 2] = [30, 50];
/// Flattened-band threshold at/above which the decorrelator's decay slope
/// `g(k)` starts falling off from `1.0` (§6, `DECAY_SLOPE = 0.05`).
pub(crate) const DECAY_CUTOFF: [usize; 2] = [10, 32];
/// Flattened-band threshold below which the pure-delay decorrelator path
/// uses a 14-sample delay (else 1 sample), §6. Flattened index equivalent
/// to QMF band 35.
pub(crate) const SHORT_DELAY_BAND: [usize; 2] = [42, 62];

/// Flattened band `k` -> working-resolution parameter band `i` (§4):
/// `k_to_i(k, is34) = FF_K_TO_I_20[k]` / `FF_K_TO_I_34[k]`.
///
/// [NOPANIC]: caller guarantees `k < NR_BANDS[is34]`; result is always
/// `< NR_PAR_BANDS[is34]` (V3 table invariant, exercised by
/// `k_to_i_anchors_and_bounds` below). An out-of-range `k` returns `0`
/// rather than panicking/indexing OOB, so a malformed caller degrades
/// instead of crashing.
#[inline]
pub(crate) fn k_to_i(k: usize, is34: bool) -> usize {
    let table: &[u8] = if is34 { &FF_K_TO_I_34 } else { &FF_K_TO_I_20 };
    table.get(k).copied().unwrap_or(0) as usize
}

/// Number of hybrid sub-bands each of the lowest QMF bands splits into
/// (§5), for the hybrid unit to size its flattened<->QMF fold: 20-cfg
/// splits QMF bands 0..3 into `{6,2,2}` sub-bands (10 total, then QMF bands
/// 3..64 pass through unsplit); 34-cfg splits QMF bands 0..5 into
/// `{12,8,4,4,4}` (32 total, then QMF bands 5..64 pass through). Exposed
/// here so hybrid (§5) and decorrelate/mix (§6/§7) agree on the layout.
#[inline]
pub(crate) fn hybrid_split(is34: bool) -> &'static [usize] {
    if is34 { &[12, 8, 4, 4, 4] } else { &[6, 2, 2] }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn k_to_i_anchors_and_bounds() {
        // V3 anchors: ff_k_to_i_20 starts 1,0,0,1,2,3,... and every value < 20.
        assert_eq!(k_to_i(0, false), 1);
        assert_eq!(k_to_i(1, false), 0);
        assert_eq!(k_to_i(2, false), 0);
        for k in 0..NR_BANDS[0] {
            assert!(k_to_i(k, false) < NR_PAR_BANDS[0]);
        }
        for k in 0..NR_BANDS[1] {
            assert!(k_to_i(k, true) < NR_PAR_BANDS[1]);
        }
        // ff_k_to_i_34 starts 0,1,2,3,4,5,6,6,7,2,1,0,... (the low-band fold-back).
        assert_eq!(k_to_i(0, true), 0);
        assert_eq!(k_to_i(9, true), 2);
        assert_eq!(k_to_i(11, true), 0);
    }

    #[test]
    fn config_constants_match_ffmpeg() {
        assert_eq!(NR_ALLPASS_BANDS, [30, 50]);
        assert_eq!(DECAY_CUTOFF, [10, 32]);
        assert_eq!(SHORT_DELAY_BAND, [42, 62]);
        assert_eq!(hybrid_split(false), &[6, 2, 2]);
        assert_eq!(hybrid_split(true), &[12, 8, 4, 4, 4]);
    }
}
