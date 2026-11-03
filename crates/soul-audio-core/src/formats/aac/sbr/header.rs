//! `sbr_header()` parse (`sbr-reference.md` §2 / ISO Table 4.56) and the
//! header-change / reset diff later tasks (frequency-table recompute, patch
//! map rebuild, QMF/smoothing re-init) key off of.

use super::bitstream::clamped_read;
use crate::decoder::DecodeError;
use crate::formats::aac::bits::BitReader;

/// Parsed `sbr_header()` fields (§2 / ISO Table 4.56), post-default
/// resolution: `bs_reserved` is discarded, and any field gated by
/// `bs_header_extra_1`/`_2` carries its ISO-mandated default when the gate
/// is 0.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct SbrHeader {
    pub amp_res: u8,        // 1 bit
    pub start_freq: u8,     // 4 bits, 0..15
    pub stop_freq: u8,      // 4 bits, 0..15
    pub xover_band: u8,     // 3 bits, 0..7
    pub freq_scale: u8,     // default 2
    pub alter_scale: u8,    // default 1
    pub noise_bands: u8,    // default 2
    pub limiter_bands: u8,  // default 2
    pub limiter_gains: u8,  // default 2
    pub interpol_freq: u8,  // default 1
    pub smoothing_mode: u8, // default 1
}

/// Header-change classification against the previously accepted header (§2
/// "Header-change / reset semantics").
#[derive(Clone, Copy, Default)]
pub(super) struct HeaderReset {
    pub reset: bool,
    pub limiter_only: bool,
}

/// Read `n` bits, clamped to `end_bit`; a read that would cross it becomes
/// `Err(DecodeError::Malformed)` (caught upstream → upsample-only fallback).
fn read_bits(bits: &mut BitReader, end_bit: usize, n: u32) -> Result<u32, DecodeError> {
    clamped_read(bits, end_bit, n).ok_or(DecodeError::Malformed)
}

/// Parse `sbr_header()` (§2). `prev` is the last accepted header (for the
/// reset diff). Reads exactly the fixed field layout, in order; `bs_reserved`
/// (2 bits) is skipped. Every read is clamped to `end_bit` via [`read_bits`].
pub(super) fn sbr_header(
    bits: &mut BitReader,
    end_bit: usize,
    prev: Option<&SbrHeader>,
) -> Result<(SbrHeader, HeaderReset), DecodeError> {
    let amp_res = read_bits(bits, end_bit, 1)? as u8;
    let start_freq = read_bits(bits, end_bit, 4)? as u8;
    let stop_freq = read_bits(bits, end_bit, 4)? as u8;
    let xover_band = read_bits(bits, end_bit, 3)? as u8;
    let _bs_reserved = read_bits(bits, end_bit, 2)?; // discarded

    let extra_1 = read_bits(bits, end_bit, 1)? != 0;
    let extra_2 = read_bits(bits, end_bit, 1)? != 0;

    let (freq_scale, alter_scale, noise_bands) = if extra_1 {
        let freq_scale = read_bits(bits, end_bit, 2)? as u8;
        let alter_scale = read_bits(bits, end_bit, 1)? as u8;
        let noise_bands = read_bits(bits, end_bit, 2)? as u8;
        (freq_scale, alter_scale, noise_bands)
    } else {
        (2, 1, 2)
    };

    let (limiter_bands, limiter_gains, interpol_freq, smoothing_mode) = if extra_2 {
        let limiter_bands = read_bits(bits, end_bit, 2)? as u8;
        let limiter_gains = read_bits(bits, end_bit, 2)? as u8;
        let interpol_freq = read_bits(bits, end_bit, 1)? as u8;
        let smoothing_mode = read_bits(bits, end_bit, 1)? as u8;
        (limiter_bands, limiter_gains, interpol_freq, smoothing_mode)
    } else {
        (2, 2, 1, 1)
    };

    let header = SbrHeader {
        amp_res,
        start_freq,
        stop_freq,
        xover_band,
        freq_scale,
        alter_scale,
        noise_bands,
        limiter_bands,
        limiter_gains,
        interpol_freq,
        smoothing_mode,
    };

    let reset = compute_reset(&header, prev);

    Ok((header, reset))
}

/// §2 "Header-change / reset semantics": `reset` if `prev` is absent or any
/// of start/stop/freq_scale/alter_scale/xover/noise_bands changed;
/// `limiter_only` if, absent a reset, only `limiter_bands` differs.
fn compute_reset(header: &SbrHeader, prev: Option<&SbrHeader>) -> HeaderReset {
    let Some(prev) = prev else {
        return HeaderReset {
            reset: true,
            limiter_only: false,
        };
    };

    let reset = header.start_freq != prev.start_freq
        || header.stop_freq != prev.stop_freq
        || header.freq_scale != prev.freq_scale
        || header.alter_scale != prev.alter_scale
        || header.xover_band != prev.xover_band
        || header.noise_bands != prev.noise_bands;

    let limiter_only = !reset && header.limiter_bands != prev.limiter_bands;

    HeaderReset {
        reset,
        limiter_only,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formats::aac::bits::BitReader;
    #[test]
    fn parses_minimal_header_with_defaults() {
        // amp_res=1, start=5(0101), stop=6(0110), xover=3(011), reserved=00,
        // extra_1=0, extra_2=0 → defaults applied. Bit stream MSB-first:
        // 1 0101 0110 011 00 0 0  = 1010 1011 0011 0000 (pad) = 0xAB,0x30
        let buf = [0b1010_1011, 0b0011_0000];
        let mut b = BitReader::new(&buf);
        let (h, rst) = sbr_header(&mut b, 16, None).unwrap();
        assert_eq!(h.amp_res, 1);
        assert_eq!(h.start_freq, 5);
        assert_eq!(h.stop_freq, 6);
        assert_eq!(h.xover_band, 3);
        assert_eq!(h.freq_scale, 2); // default
        assert_eq!(h.alter_scale, 1); // default
        assert_eq!(h.noise_bands, 2); // default
        assert_eq!(h.limiter_bands, 2);
        assert_eq!(h.interpol_freq, 1);
        assert_eq!(h.smoothing_mode, 1);
        assert!(rst.reset); // prev None → reset
    }
    #[test]
    fn limiter_only_change_flagged() {
        let buf = [0b1010_1011, 0b0011_0000];
        let mut b = BitReader::new(&buf);
        let (h0, _) = sbr_header(&mut b, 16, None).unwrap();
        let mut h1 = h0;
        h1.limiter_bands = 3;
        // Re-parse identical bytes but diff against h1: only limiter differs.
        let mut b2 = BitReader::new(&buf);
        let (_h, rst) = sbr_header(&mut b2, 16, Some(&h1)).unwrap();
        assert!(!rst.reset);
        assert!(rst.limiter_only);
    }
}
