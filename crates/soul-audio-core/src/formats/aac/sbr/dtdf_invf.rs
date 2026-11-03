//! `sbr_dtdf()` + `invf_mode()` — ISO Table 4.62/4.63 (`sbr-reference.md`
//! §5). Two small per-channel reads that follow the grid: the delta-coding
//! direction flags (`bs_df_env`/`bs_df_noise`, one bit each — whether the
//! matching envelope/noise-floor row is coded delta-frequency or
//! delta-time, consumed by the Huffman table selection in §6) and the
//! per-noise-band inverse-filter (chirp) mode (`bs_invf_mode`, two bits
//! each, consumed by HF adjustment in §8).
//!
//! **CPE coupling:** when `bs_coupling == 1`, channel 0 reads `invf_mode`
//! and channel 1 *copies* it verbatim (`bs_invf_mode[1][n] =
//! bs_invf_mode[0][n]`) rather than reading its own — along with the grid,
//! `L_E`/`L_Q`, `t_env`/`t_noise`, `bs_pointer`, and `bs_freq_res`, all also
//! copied 0->1 in that case (only the envelope/noise scalefactors stay
//! per-channel — §6/§10). That copy is orchestration across both channels'
//! parsed state, which belongs to the Task 11 frame-level driver, not this
//! file: [`read_invf`] always performs the read and returns ch0's array.
//!
//! **No-panic:** both loops are bounded by the caller-supplied counts
//! (`l_e<=5`, `l_q<=2`, `n_q<=5` — the grid/frequency-table parsers already
//! enforce those caps), and every bit read is clamped against `end_bit` via
//! [`clamped_read`]; a read that would cross it returns
//! `Err(DecodeError::Malformed)` instead of panicking or reading garbage.

use super::bitstream::clamped_read;
use crate::decoder::DecodeError;
use crate::formats::aac::bits::BitReader;

/// Read `n` bits, clamped to `end_bit`. Mirrors `grid::read_bits` — each
/// SBR sub-parser keeps its own thin wrapper around the shared
/// [`clamped_read`] clamp.
fn read_bits(bits: &mut BitReader, end_bit: usize, n: u32) -> Result<u32, DecodeError> {
    clamped_read(bits, end_bit, n).ok_or(DecodeError::Malformed)
}

/// `sbr_dtdf()` (§5): delta-coding direction flags, one bit each —
/// `df_env[i]`/`df_noise[i]`: 0 = delta-freq, 1 = delta-time. Read
/// per-channel (the CPE-coupled path still reads both channels' `dtdf`
/// independently; only `invf_mode`/grid/borders are copied 0->1). Only
/// indices `0..l_e` / `0..l_q` are written; the rest of each fixed array
/// stays zeroed and must not be read by the caller.
pub(super) fn read_dtdf(
    bits: &mut BitReader,
    end_bit: usize,
    l_e: u8,
    l_q: u8,
) -> Result<([u8; 5], [u8; 2]), DecodeError> {
    let mut df_env = [0u8; 5];
    for slot in df_env.iter_mut().take(l_e as usize) {
        *slot = read_bits(bits, end_bit, 1)? as u8;
    }

    let mut df_noise = [0u8; 2];
    for slot in df_noise.iter_mut().take(l_q as usize) {
        *slot = read_bits(bits, end_bit, 1)? as u8;
    }

    Ok((df_env, df_noise))
}

/// `invf_mode()` (§5): per-noise-band inverse-filter (chirp) mode, two
/// bits each — 0=OFF, 1=LOW, 2=MID, 3=HIGH. Reads ch0's array; the
/// CPE-coupled ch1 copy is the caller's job (see module docs). Only
/// indices `0..n_q` are written; the rest stays zeroed.
pub(super) fn read_invf(
    bits: &mut BitReader,
    end_bit: usize,
    n_q: u8,
) -> Result<[u8; 5], DecodeError> {
    let mut invf_mode = [0u8; 5];
    for slot in invf_mode.iter_mut().take(n_q as usize) {
        *slot = read_bits(bits, end_bit, 2)? as u8;
    }
    Ok(invf_mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::aac::bits::BitReader;

    #[test]
    fn reads_dtdf_flags() {
        // L_E=2, L_Q=1 -> df_env[0]=1, df_env[1]=0, df_noise[0]=1.
        // bits: 1 0 1 = 0b10100000 = 0xA0.
        let buf = [0xA0];
        let (df_env, df_noise) = read_dtdf(&mut BitReader::new(&buf), 8, 2, 1).unwrap();
        assert_eq!(&df_env[..2], &[1, 0]);
        assert_eq!(df_noise[0], 1);
    }

    #[test]
    fn reads_invf_modes() {
        // N_Q=2 -> invf[0]=read(2)=10=2(MID), invf[1]=read(2)=01=1(LOW).
        // bits: 10 01 = 0b10010000 = 0x90.
        let buf = [0x90];
        let invf = read_invf(&mut BitReader::new(&buf), 8, 2).unwrap();
        assert_eq!(&invf[..2], &[2, 1]);
    }

    #[test]
    fn read_past_end_bit_is_malformed_not_panic() {
        // end_bit=1 leaves room for exactly one 1-bit read; a second
        // df_env bit (l_e=2) and any 2-bit invf_mode read must both fail
        // cleanly rather than panic/read out of bounds.
        let buf = [0xFFu8];
        let dtdf_err = read_dtdf(&mut BitReader::new(&buf), 1, 2, 0);
        assert!(dtdf_err.is_err());

        let invf_err = read_invf(&mut BitReader::new(&buf), 1, 1);
        assert!(invf_err.is_err());
    }
}
