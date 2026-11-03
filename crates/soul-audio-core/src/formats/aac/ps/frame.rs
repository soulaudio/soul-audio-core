//! Per-frame PS layout: envelope count, QMF-slot borders, and the
//! mode-derived parameter-band/`is34`/`iid_quant` context (`ps-reference.md`
//! §2). [`data::ps_data`](super::data::ps_data) reads `frame_class` and
//! `num_env_idx` off the bitstream and hands both, plus the header's
//! `iid_mode`/`icc_mode`, to [`read_frame`] here, which turns them into a
//! [`PsFrameLayout`]: `num_env` (`NUM_ENV_TAB`), `border_position` (evenly
//! spaced for the fixed class, or read explicitly off the bitstream for the
//! variable class), and the per-type band counts (`NR_IIDICC_PAR_TAB`/
//! `NR_IIDOPD_PAR_TAB`) plus `is34`/`iid_quant`.

use super::MAX_PS_ENVELOPES;
use super::data::clamped_read;
use super::tables::{NR_IIDICC_PAR_TAB, NR_IIDOPD_PAR_TAB, NUM_ENV_TAB};
use crate::decoder::DecodeError;
use crate::formats::aac::bits::BitReader;

/// One frame's PS envelope/band layout (`ps-reference.md` §2).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PsFrameLayout {
    /// `num_env = num_env_tab[frame_class][num_env_idx]` (§2), clamped
    /// `<= MAX_PS_ENVELOPES`. Range `0..=5`; `0` only occurs for the fixed
    /// class and signals "no envelope this frame, reuse the previous
    /// frame's parameters" (Task 4's territory) `[NOPANIC]`.
    pub(crate) num_env: u8,
    /// QMF-slot envelope borders (§2): `border_position[0] == 0` always.
    /// For `num_env >= 1`, `border_position[num_env]` is the frame's last
    /// envelope edge (forced to `32` for the fixed class; the last
    /// bitstream-read value, `<= 32`, for the variable class — *not*
    /// re-forced to `32`). Strictly increasing over `0..=num_env`,
    /// validated at parse time `[NOPANIC monotone]`.
    pub(crate) border_position: [u8; MAX_PS_ENVELOPES + 1],
    /// `nr_iidicc_par_tab[iid_mode]` (§2): `10`/`20`/`34` IID parameter
    /// bands.
    pub(crate) nr_iid_par: u8,
    /// `nr_iidicc_par_tab[icc_mode]` (§2): `10`/`20`/`34` ICC parameter
    /// bands.
    pub(crate) nr_icc_par: u8,
    /// `nr_iidopd_par_tab[iid_mode]` (§2): `5`/`11`/`17` IPD/OPD parameter
    /// bands — IPD/OPD carry no independent band-mode field and instead
    /// track the IID mode.
    pub(crate) nr_ipdopd_par: u8,
    /// `(enable_iid && nr_iid_par == 34) || (enable_icc && nr_icc_par == 34)`
    /// (§2; FFmpeg `aacps_common.c`'s `is34bands`) — selects the 34-band
    /// hybrid config used everywhere downstream (§4/§5). Each term is gated
    /// on THIS frame's `enable_iid`/`enable_icc`, not just the mode value:
    /// a persisted `iid_mode`/`icc_mode` left over from a frame that once
    /// enabled 34-band IID/ICC must not keep counting once that parameter
    /// type is off. When neither is enabled this frame, `is34` is not
    /// recomputed at all — it holds the previous frame's value (FFmpeg's
    /// `is34bands` is only reassigned inside
    /// `if (ps->enable_iid || ps->enable_icc)`; see [`read_frame`]'s
    /// `is34_old` parameter).
    pub(crate) is34: bool,
    /// `iid_mode >= 3` (§2): selects the fine (½-dB steps, ±15) IID scale
    /// over the coarse (±7) one.
    pub(crate) iid_quant: bool,
}

/// Build the frame layout (§2). `frame_class`/`num_env_idx` were read by
/// `ps_data`; borders are read from `bits` only for the variable class
/// (`frame_class == 1`). `enable_iid`/`enable_icc` are THIS frame's
/// (persisted-or-fresh) header flags, and `is34_old` is the previous frame's
/// `PsFrameLayout::is34` — together they drive the `is34` gate/persistence
/// described on [`PsFrameLayout::is34`].
///
/// `[NOPANIC]`: `frame_class` (`0..=1`) and `num_env_idx` (`0..=3`) are
/// range-checked before indexing `NUM_ENV_TAB`; `iid_mode`/`icc_mode`
/// (`0..=5`) before indexing the mode tables; every bitstream read is
/// bounded by `end_bit`; and the resulting `num_env`/`border_position` are
/// validated (`num_env <= MAX_PS_ENVELOPES`; borders strictly increasing
/// and `<= 32`) before being returned — a malformed frame errors out rather
/// than panicking, indexing out of bounds, or leaving `border_position` fit
/// to divide by a non-positive envelope length in §7's interpolation.
#[allow(clippy::too_many_arguments)]
pub(crate) fn read_frame(
    bits: &mut BitReader,
    end_bit: usize,
    frame_class: u8,
    num_env_idx: u8,
    iid_mode: u8,
    icc_mode: u8,
    enable_iid: bool,
    enable_icc: bool,
    is34_old: bool,
) -> Result<PsFrameLayout, DecodeError> {
    if frame_class > 1 || num_env_idx > 3 {
        return Err(DecodeError::Malformed);
    }
    if iid_mode as usize >= NR_IIDICC_PAR_TAB.len() || icc_mode as usize >= NR_IIDICC_PAR_TAB.len()
    {
        return Err(DecodeError::Malformed);
    }

    let num_env =
        NUM_ENV_TAB[frame_class as usize][num_env_idx as usize].min(MAX_PS_ENVELOPES as u8);

    let mut border_position = [0u8; MAX_PS_ENVELOPES + 1];
    if frame_class == 0 {
        // Fixed class (§2): evenly spaced borders. `border_position[0]`
        // is already `0` from the zero-init above. `num_env == 0` (the
        // "reuse previous" sentinel) leaves the loop and the final
        // assignment both skipped — no envelope, no border beyond the
        // leading 0.
        for e in 1..num_env {
            border_position[e as usize] = ((e as u32 * 32) / num_env as u32) as u8;
        }
        if num_env > 0 {
            border_position[num_env as usize] = 32;
        }
    } else {
        // Variable class (§2): explicit borders read off the bitstream,
        // `read(5) + 1` each (range `1..=32`), required strictly
        // increasing over the whole `0..=num_env` run.
        for e in 1..=num_env {
            let raw = clamped_read(bits, end_bit, 5).ok_or(DecodeError::Malformed)?;
            let border = (raw + 1) as u8;
            if border > 32 || border <= border_position[(e - 1) as usize] {
                return Err(DecodeError::Malformed);
            }
            border_position[e as usize] = border;
        }
    }

    let nr_iid_par = NR_IIDICC_PAR_TAB[iid_mode as usize];
    let nr_icc_par = NR_IIDICC_PAR_TAB[icc_mode as usize];
    let nr_ipdopd_par = NR_IIDOPD_PAR_TAB[iid_mode as usize];

    // `is34bands` (§2; FFmpeg `aacps_common.c` ~L262-264): each OR-term
    // gated on THIS frame's enable flag, and the whole expression only
    // recomputed `if (enable_iid || enable_icc)` -- otherwise `is34` holds
    // `is34_old` (the previous frame's value) untouched.
    let is34 = if enable_iid || enable_icc {
        (enable_iid && nr_iid_par == 34) || (enable_icc && nr_icc_par == 34)
    } else {
        is34_old
    };

    Ok(PsFrameLayout {
        num_env,
        border_position,
        nr_iid_par,
        nr_icc_par,
        nr_ipdopd_par,
        is34,
        iid_quant: iid_mode >= 3,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::aac::bits::BitReader;

    #[test]
    fn fixed_class_even_borders() {
        // frame_class=0, num_env_idx=3 -> num_env=4 (NUM_ENV_TAB[0][3]).
        // iid_mode=1 (20-band, coarse), icc_mode=1 (20-band).
        let buf = [0u8; 4];
        let mut b = BitReader::new(&buf);
        let l = read_frame(&mut b, 32, 0, 3, 1, 1, true, true, false).unwrap();
        assert_eq!(l.num_env, 4);
        assert_eq!(&l.border_position[..5], &[0, 8, 16, 24, 32]);
        assert_eq!(l.nr_iid_par, 20);
        assert_eq!(l.nr_icc_par, 20);
        assert_eq!(l.nr_ipdopd_par, 11);
        assert!(!l.is34 && !l.iid_quant);
    }

    #[test]
    fn is34_and_fine_from_modes() {
        // iid_mode=5 -> 34-band fine; icc_mode=2 -> 34-band.
        let buf = [0u8; 4];
        let mut b = BitReader::new(&buf);
        let l = read_frame(&mut b, 32, 0, 1, 5, 2, true, true, false).unwrap(); // num_env_idx=1 -> 1
        assert_eq!(l.num_env, 1);
        assert_eq!(l.nr_iid_par, 34);
        assert!(l.is34 && l.iid_quant);
        assert_eq!(l.nr_ipdopd_par, 17);
    }

    #[test]
    fn variable_class_explicit_borders() {
        // frame_class=1, num_env_idx=1 -> num_env=2 (NUM_ENV_TAB[1][1]).
        // borders read as read(5)+1: raw=9 -> 10, raw=31 -> 32 (increasing,
        // last one hits the 32 ceiling exactly -- must be accepted).
        // bits: 01001 11111 = 0100_1111 1100_0000.
        let buf = [0b0100_1111, 0b1100_0000, 0, 0];
        let mut b = BitReader::new(&buf);
        let l = read_frame(&mut b, 32, 1, 1, 0, 0, true, true, false).unwrap();
        assert_eq!(l.num_env, 2);
        assert_eq!(&l.border_position[..3], &[0, 10, 32]);
    }

    #[test]
    fn variable_borders_must_increase() {
        // frame_class=1, num_env_idx=1 -> num_env=2 (NUM_ENV_TAB[1][1]).
        // borders read as read(5)+1: give 3 then 2 (decreasing) -> Err.
        // bits: 00010 00001 = border[1]=3, border[2]=2.
        let buf = [0b0001_0000, 0b0100_0000, 0, 0];
        let mut b = BitReader::new(&buf);
        assert!(read_frame(&mut b, 32, 1, 1, 1, 1, true, true, false).is_err());
    }

    #[test]
    fn out_of_range_mode_is_malformed() {
        // iid_mode=6 is out of NR_IIDICC_PAR_TAB's 0..=5 range: must error
        // rather than index the mode tables out of bounds [NOPANIC].
        let buf = [0u8; 4];
        let mut b = BitReader::new(&buf);
        assert!(read_frame(&mut b, 32, 0, 0, 6, 0, true, true, false).is_err());
    }

    #[test]
    fn out_of_range_frame_class_or_idx_is_malformed() {
        let buf = [0u8; 4];
        let mut b1 = BitReader::new(&buf);
        assert!(read_frame(&mut b1, 32, 2, 0, 0, 0, true, true, false).is_err());
        let mut b2 = BitReader::new(&buf);
        assert!(read_frame(&mut b2, 32, 0, 4, 0, 0, true, true, false).is_err());
    }

    #[test]
    fn is34_gates_on_current_frame_enable_flags() {
        // iid_mode=5 is a 34-band value, but enable_iid=false this frame:
        // the IID term must NOT count. enable_icc=true with icc_mode=0
        // (10-band, not 34) forces the `is34` expression to actually
        // recompute (enable_iid || enable_icc == true) rather than just
        // holding `is34_old` -- this is what discriminates the fix from the
        // pre-fix `nr_iid_par == 34 || nr_icc_par == 34` formula, which was
        // unconditional on `enable_iid`/`enable_icc` entirely and would have
        // returned `true` here (nr_iid_par == 34 from the persisted mode)
        // regardless of enable_iid being off this frame.
        let buf = [0u8; 4];
        let mut b = BitReader::new(&buf);
        let l = read_frame(&mut b, 32, 0, 1, 5, 0, false, true, false).unwrap();
        assert_eq!(l.nr_iid_par, 34); // the mode value itself is still 34-band
        assert_eq!(l.nr_icc_par, 10);
        assert!(
            !l.is34,
            "is34 must gate the IID term on enable_iid, not just nr_iid_par"
        );
    }

    #[test]
    fn is34_holds_previous_value_when_neither_type_is_enabled() {
        // enable_iid=false and enable_icc=false this frame (both off) with
        // NEITHER mode 34-band (iid_mode=1 -> 20, icc_mode=1 -> 20): per
        // FFmpeg's `is34bands`, the whole expression is skipped when
        // `!(enable_iid || enable_icc)`, so the result must be `is34_old`
        // (simulating a persisted true from an earlier 34-band frame), NOT
        // recomputed to false from this frame's (irrelevant, since neither
        // param type is active) non-34 modes.
        let buf = [0u8; 4];
        let mut b = BitReader::new(&buf);
        let l = read_frame(&mut b, 32, 0, 1, 1, 1, false, false, true).unwrap();
        assert_eq!(l.nr_iid_par, 20);
        assert_eq!(l.nr_icc_par, 20);
        assert!(
            l.is34,
            "is34 must persist is34_old when enable_iid and enable_icc are both false"
        );
    }
}
