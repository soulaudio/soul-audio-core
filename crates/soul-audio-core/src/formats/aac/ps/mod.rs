//! AAC Parametric Stereo (PS, HE-AACv2 Phase 3) — `.superpowers/sdd/ps-reference.md`.
//! PS upmixes the mono AAC-LC+SBR core to stereo inside the SBR 64-band
//! complex QMF domain (§8 seam): SBR hands PS one mono QMF matrix, PS hands
//! back two (`X_left`, `X_right`).
//!
//! [`PsDecoder`] owns the whole PS decode: [`parse_frame`](PsDecoder::parse_frame)
//! is the parse-time seam (driven from the SBR extension loop behind
//! `EXTENSION_ID_PS`), and [`apply`](PsDecoder::apply) is the QMF-domain seam
//! (§8): consume this frame's assembled MONO matrix and produce two matrices
//! for SBR to synthesize twice. `apply` runs the §8 order over no-alloc stack
//! scratch — hybrid analysis ([`hybrid`], Task 6) →
//! decorrelate ([`decorrelate`], Task 7) → stereo mix ([`mix`], Task 8) →
//! hybrid synthesis — carrying the persistent DSP state ([`hybrid::HybridHistory`],
//! [`decorrelate::DecorrState`], [`mix::MixState`]) and the 6-slot group-delay
//! history frame-to-frame.

pub(crate) mod tables;

pub(super) mod bands;
pub(super) mod data;
pub(super) mod decorrelate;
pub(super) mod frame;
pub(super) mod hybrid;
pub(super) mod mix;
pub(crate) mod params;

use crate::decoder::DecodeError;
use crate::formats::aac::bits::BitReader;
use crate::formats::aac::sbr::qmf::Cpx;

use decorrelate::DecorrState;
use hybrid::HybridHistory;
use mix::MixState;

/// QMF bands (the SBR/PS seam's per-slot band count, §8).
pub(crate) const N_QMF: usize = 64;
/// `numTimeSlotsRate` — QMF time slots carrying data per frame (§0/§8).
pub(crate) const N_SLOTS: usize = 32;
/// Hybrid filterbank group delay in QMF slots (§5) — the per-band history
/// carried across frames so the 13-tap hybrid FIR stays causal.
pub(crate) const HISTORY: usize = 6;
/// `N_SLOTS + HISTORY` — the QMF matrix's time dimension, `[38][64]` in
/// faad2 (§8 seam: `qmf_t X[38][64]`).
pub(crate) const N_SLOTS_HIST: usize = 38;
/// `NR_BANDS` upper bound — the flattened hybrid+QMF processing-band count
/// for the 34-parameter-band configuration (§4/§5, `NR_BANDS={71,91}`).
pub(crate) const NR_BANDS_MAX: usize = 91;
/// `MAX_PS_ENVELOPES` / `PS_MAX_NUM_ENV` — envelopes per frame (§2, V6).
pub(crate) const MAX_PS_ENVELOPES: usize = 5;

/// Persistent PS decode state, carried frame-to-frame across calls to
/// [`PsDecoder::parse_frame`]/[`PsDecoder::apply`]: the last accepted header,
/// the current frame's layout/params, the delta-time param bases, the hybrid/
/// decorrelation/mix DSP state, and the 6-slot QMF group-delay history.
/// `Default` = "no header seen yet": a `ps_data()` call with no header
/// present is a no-op (`ps_data_available` stays false; mono passes through
/// unmodified, §1), and [`is_active`](PsDecoder::is_active) stays false so the
/// SBR driver keeps the mono path.
///
/// The large per-frame hybrid-domain scratch (`x38`/`s`/`d`/`xl`/`xr`, ~112 KB)
/// is **not** stored here: it carries no frame-to-frame state, so `apply` holds
/// it as (no-alloc) stack locals for the duration of the call. Keeping it out
/// of the struct is deliberate — `PsDecoder` is embedded by value all the way
/// up through `SbrState` -> `AacDecoder` -> `AnyDecoder`, and a 112 KB struct
/// bloat overflows the default-stack unit tests that construct an `AnyDecoder`
/// by value. See the Task-9 report's stack-headroom section.
pub(crate) struct PsDecoder {
    /// Set once a PS header has been read at least once (§1). Gates every
    /// later use of `enable_iid`/`enable_icc`/`enable_ext`/`iid_mode`/
    /// `icc_mode` — a bitstream with `enable_ps_header==0` before the first
    /// header is entirely inert.
    pub(crate) ps_data_available: bool,
    // last accepted header (reused when enable_ps_header==0):
    pub(crate) enable_iid: bool,
    pub(crate) enable_icc: bool,
    pub(crate) enable_ext: bool,
    /// Persisted `ps_extension()`'s own IPD/OPD gate (§1/§3; faad2
    /// `ps_syntax.c`'s `ps->enable_ipdopd`, FFmpeg `aacps_common.c`'s
    /// `PSCommonContext.enable_ipdopd`): assigned ONLY inside the
    /// `ps_ext_id == 0` sub-extension record when `enable_ext == 1` and that
    /// record is actually present this frame — otherwise it carries over
    /// verbatim from the last frame that did carry one, exactly like
    /// `enable_iid`/`enable_icc`/`enable_ext` above. A frame with no fresh
    /// id==0 record neither re-decodes nor resets `params.ipd_*`/`opd_*`
    /// while this stays true (`ps::data::ps_data` mirrors this into
    /// `params.enable_ipdopd` at the end of every call for Task 8's
    /// interface).
    pub(crate) enable_ipdopd: bool,
    /// `0..=5` (§2) — bounds-checked at parse time, never written
    /// out-of-range (`[NOPANIC]`).
    pub(crate) iid_mode: u8,
    /// `0..=5` (§2) — bounds-checked at parse time, never written
    /// out-of-range (`[NOPANIC]`).
    pub(crate) icc_mode: u8,
    // current-frame layout + params (filled by Tasks 3/4):
    pub(crate) frame: frame::PsFrameLayout,
    pub(crate) params: params::PsParams,
    // persistent delta-time bases (Task 4):
    pub(crate) iid_index_prev: [i8; 34],
    pub(crate) icc_index_prev: [i8; 34],
    pub(crate) ipd_index_prev: [i8; 17],
    pub(crate) opd_index_prev: [i8; 17],
    /// Previous frame's FINAL (post-coverage-fixup) `num_env` — FFmpeg
    /// `PSCommonContext.num_env_old` (`aacps_common.c` L170/L226). The source
    /// param row for a `num_env == 0` reuse frame's synthetic envelope is the
    /// previous frame's last envelope, `num_env_old - 1`; `0` at cold start
    /// (Default) makes that source `-1`, matching FFmpeg's `source >= 0` "no
    /// prior envelope" guard (the cold-start-reuse edge). Updated at the end of
    /// every parsed frame body (`data::ps_data`) to this frame's post-fixup
    /// `num_env`, so the NEXT frame's fixup reads the correct prior count.
    pub(crate) num_env_old: usize,

    // --- Task 9: the §8 seam (parse -> apply) DSP state + scratch. ---
    /// Previous frame's `is34` (the config the DSP rings were last primed
    /// for). `apply` resets the layout-specific decorrelator/hybrid state
    /// whenever this flips (conservative, matching FFmpeg zeroing on an
    /// `is34bands_old` change); `None` forces the cold-start reset path on
    /// the first frame. `MixState` carries its own equivalent tracker.
    apply_is34_prev: Option<bool>,
    /// The retained last `HISTORY` (=6) assembled QMF slots of the previous
    /// frame (R1): `apply` prepends them to this frame's mono matrix to form
    /// the `[38][64]` working matrix the 13-tap hybrid FIR reads as `X[n+6]`
    /// (§5). Zeroed = correct cold start.
    x_hist: [[Cpx; N_QMF]; HISTORY],
    /// Persistent hybrid-FIR memory (12 past samples × 5 split bands, §5).
    hyb: HybridHistory,
    /// Persistent decorrelator rings + transient state (~19.6 KB, §6) — kept
    /// behind `&mut`, never moved by value on the per-frame path.
    dec: DecorrState,
    /// Persistent stereo-mix interpolation anchors + IPD/OPD phase rings (§7).
    mix: MixState,
}

impl Default for PsDecoder {
    fn default() -> Self {
        Self {
            ps_data_available: false,
            enable_iid: false,
            enable_icc: false,
            enable_ext: false,
            enable_ipdopd: false,
            iid_mode: 0,
            icc_mode: 0,
            frame: frame::PsFrameLayout::default(),
            params: params::PsParams::default(),
            iid_index_prev: [0; 34],
            icc_index_prev: [0; 34],
            ipd_index_prev: [0; 17],
            opd_index_prev: [0; 17],
            num_env_old: 0,
            apply_is34_prev: None,
            x_hist: [[Cpx { re: 0.0, im: 0.0 }; N_QMF]; HISTORY],
            hyb: HybridHistory::default(),
            dec: DecorrState::default(),
            mix: MixState::default(),
        }
    }
}

impl PsDecoder {
    /// Parse-time seam (driven from the SBR extension loop behind
    /// `EXTENSION_ID_PS`, R2/R3). Delegates to [`data::ps_data`]; updates the
    /// header/frame/param state (including the delta-time bases fed to the
    /// next frame's env-0 decode) and returns the exact bit count consumed so
    /// the enclosing loop can advance. On malformed input returns `Err`, and
    /// the caller disables PS for the frame (mono passthrough). Never reads
    /// past `end_bit`, never panics.
    pub(crate) fn parse_frame(
        &mut self,
        bits: &mut BitReader,
        end_bit: usize,
    ) -> Result<u32, DecodeError> {
        data::ps_data(self, bits, end_bit)
    }

    /// True once a PS header has been seen (`ps_data_available`, §1). The SBR
    /// driver only routes the mono core through PS when this holds.
    pub(crate) fn is_active(&self) -> bool {
        self.ps_data_available
    }

    /// QMF-domain seam (R1/R4, §8): consume this frame's assembled MONO matrix
    /// `x_mono` (`[[Cpx; 64]; 32]`) and fill two matrices `out_left`/`out_right`
    /// for the SBR driver to synthesize twice. Uses this frame's parsed
    /// parameters (or the last header's when `enable_ps_header == 0`, already
    /// resolved by [`parse_frame`](Self::parse_frame)).
    ///
    /// Pipeline (§8): prepend the retained 6-slot history to form the
    /// `[38][64]` working matrix → hybrid analysis → decorrelate → stereo mix →
    /// hybrid synthesis (×2), then retain this frame's trailing 6 slots as next
    /// frame's history. The working buffers are no-alloc stack locals; only the
    /// frame-to-frame DSP state lives on `self`. Every step is no-panic; `Err`
    /// is reserved for a caller mono-passthrough fallback (the DSP itself cannot
    /// fail on valid state).
    ///
    /// The layout-specific decorrelator/hybrid rings are reset whenever `is34`
    /// flips between frames (their anchors are config-specific).
    pub(crate) fn apply(
        &mut self,
        x_mono: &[[Cpx; N_QMF]; N_SLOTS],
        out_left: &mut [[Cpx; N_QMF]; N_SLOTS],
        out_right: &mut [[Cpx; N_QMF]; N_SLOTS],
    ) -> Result<(), DecodeError> {
        let is34 = self.frame.is34;

        // is34-transition reset: the decorrelator rings and hybrid FIR memory
        // are laid out per-config, so drop them on a 20<->34 switch (and on the
        // cold start, `None`) rather than reuse them at the wrong resolution.
        // `MixState` resets its own anchors/phase rings internally.
        if self.apply_is34_prev != Some(is34) {
            self.dec = DecorrState::default();
            self.hyb = HybridHistory::default();
            self.apply_is34_prev = Some(is34);
        }

        // Per-frame hybrid-domain scratch — stack locals (no alloc), reused
        // only within this call. Held here rather than as struct fields to keep
        // `PsDecoder` small on the by-value `AnyDecoder` stack (see the type doc).
        let mut x38 = [[Cpx::default(); N_QMF]; N_SLOTS_HIST];
        let mut s = [[Cpx::default(); NR_BANDS_MAX]; N_SLOTS];
        let mut d = [[Cpx::default(); NR_BANDS_MAX]; N_SLOTS];

        // R1: build the [38][64] working matrix = retained 6-slot history ++
        // this frame's 32 mono slots.
        x38[..HISTORY].copy_from_slice(&self.x_hist);
        x38[HISTORY..N_SLOTS_HIST].copy_from_slice(x_mono);

        // §8 pipeline.
        hybrid::hybrid_analysis(&x38, is34, &mut self.hyb, &mut s);
        decorrelate::decorrelate(&s, is34, &mut self.dec, &mut d);

        // Coverage defense: the parser (`parse_frame` -> `data::ps_data` ->
        // `data::coverage_fixup`) now guarantees every frame fully covers all
        // 32 QMF slots by appending a synthetic held-param envelope (FFmpeg
        // `aacps_common.c` "Fix up envelopes"), so the mixer overwrites every
        // covered (slot, band) — including the tail of a `num_env == 0`
        // reuse-previous frame or a variable-class frame whose last border
        // stopped short of slot 32. This `xl = xr = s` prefill is retained only
        // as harmless defense for any band beyond `nr_bands` that the mixer
        // never touches (it renders centre-mono there rather than silence);
        // full-coverage frames are unaffected. [DONE: coverage fixup handles it]
        let mut xl = s;
        let mut xr = s;
        mix::stereo_mix(
            &s,
            &d,
            &self.params,
            &self.frame,
            self.icc_mode,
            &mut self.mix,
            &mut xl,
            &mut xr,
        );

        // Undo the hybrid split back to plain 64-band QMF for each channel.
        hybrid::hybrid_synthesis(&xl, is34, out_left);
        hybrid::hybrid_synthesis(&xr, is34, out_right);

        // Retain this frame's trailing 6 assembled slots as next frame's
        // group-delay history (R1).
        for h in 0..HISTORY {
            self.x_hist[h] = x_mono[N_SLOTS - HISTORY + h];
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::aac::sbr::qmf::Cpx;

    /// A hand-activated `PsDecoder` (nonzero IID on the bands, ICC neutral) fed
    /// a nonzero mono matrix must fill `out_left != out_right` — a genuine
    /// stereo image — and must not panic. Step-1 TDD gate for the `apply` seam.
    #[test]
    fn apply_produces_distinct_stereo_from_active_ps() {
        // ICC neutral (rho=1 -> alpha=0), IID a strong non-unity pan on every
        // param band, so wherever the input lands L = c2*s, R = c1*s with
        // c1 != c2 (d drops out at alpha=beta=0) -> L != R.
        let mut p = params::PsParams::default();
        for b in 0..20 {
            p.iid_c[0][b] = tables::SF_IID_NORMAL[10]; // q = +3
            p.icc_cos[0][b] = tables::COS_ALPHAS[0];
            p.icc_sin[0][b] = tables::SIN_ALPHAS[0];
        }
        // 20-config, one fixed envelope spanning all 32 slots (full coverage).
        let mut ps = PsDecoder {
            ps_data_available: true,
            enable_iid: true,
            enable_icc: true,
            icc_mode: 0, // Type-A mixing (icc_mode < 3).
            frame: frame::PsFrameLayout {
                num_env: 1,
                border_position: [0, 32, 0, 0, 0, 0],
                nr_iid_par: 20,
                nr_icc_par: 20,
                nr_ipdopd_par: 11,
                is34: false,
                iid_quant: false,
            },
            params: p,
            ..Default::default()
        };

        // Nonzero mono input in a low QMF band (mapped into the param bands).
        let mut x = [[Cpx::default(); N_QMF]; N_SLOTS];
        for slot in x.iter_mut() {
            slot[2] = Cpx { re: 0.5, im: -0.2 };
            slot[5] = Cpx { re: 0.3, im: 0.4 };
        }
        let mut l = [[Cpx::default(); N_QMF]; N_SLOTS];
        let mut r = [[Cpx::default(); N_QMF]; N_SLOTS];
        assert!(ps.apply(&x, &mut l, &mut r).is_ok());
        let differ = l
            .iter()
            .zip(r.iter())
            .any(|(a, b)| a.iter().zip(b.iter()).any(|(p, q)| p != q));
        assert!(differ, "PS must produce a genuine stereo image (L != R)");
    }

    /// A `Default` `PsDecoder` is inactive: `is_active()` is false, so the SBR
    /// driver keeps the mono path.
    #[test]
    fn inactive_ps_is_untouched() {
        assert!(!PsDecoder::default().is_active());
    }
}
