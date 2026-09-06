//! LSb-first bit reader + the two low-level Vorbis primitives that sit on top
//! of it (`ilog`, `float32_unpack`). This is the parsing foundation for the
//! rest of the decoder: every setup-header and audio-packet field is read
//! through [`BitReader`], so a subtle ordering bug here would make every
//! later stage fail in confusing, hard-to-localize ways.
//!
//! # Bit convention (spec §2)
//!
//! Vorbis (unlike MP3/AAC, which are MSb-first) packs bits **LSb-first**: the
//! first bit read from the stream becomes bit 0 of the returned value, the
//! second bit becomes bit 1, and so on — i.e. multi-bit values are
//! little-endian *at the bit level*, and that ordering carries across byte
//! boundaries exactly the same way a little-endian multi-byte integer's byte
//! order carries across the bytes that hold it. Concretely, within one byte
//! bit 0 (`0x01`) is read before bit 7 (`0x80`); once a byte is exhausted,
//! reading continues at bit 0 of the next byte, which becomes the *next
//! more-significant* bit of the value being assembled.
//!
//! [`BitReader::read_bit`] reads exactly one bit and is the only place that
//! actually touches the backing byte slice; every wider read
//! ([`BitReader::read_bits`], [`BitReader::read_bits_u64`]) is built directly
//! on top of it, so LSb-first-ness only has to be proven correct in one
//! place (see the `tests` module below, which packs the spec's own §2.1
//! worked example by hand and reads it back).

use crate::decoder::DecodeError;

/// LSb-first bit reader over a borrowed byte slice (spec §2). Reads nothing
/// past the end of `bytes`; running out of bits is a [`DecodeError::UnexpectedEof`],
/// never a panic or a silent zero-pad (unlike the MSb-first AAC/ALAC readers
/// elsewhere in this crate, which zero-pad past their end — Vorbis packets
/// are self-delimiting containers, so running past the end always signals a
/// genuinely malformed/truncated packet, not a normal "read a bit further
/// than strictly needed" pattern).
pub struct BitReader<'a> {
    bytes: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    /// Start reading from bit 0 of `bytes`.
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit_pos: 0 }
    }

    /// Total bits available in the backing slice.
    fn total_bits(&self) -> usize {
        self.bytes.len().saturating_mul(8)
    }

    /// Read the bit at the current position without any bounds check —
    /// callers must have already verified `bit_pos < total_bits()`. Never
    /// panics either way: an out-of-range byte index reads as `0`, so a
    /// caller bug here degrades to a wrong-but-safe value rather than UB.
    fn bit_unchecked(&mut self) -> bool {
        let byte_idx = self.bit_pos >> 3;
        let shift = (self.bit_pos & 7) as u32;
        let byte = self.bytes.get(byte_idx).copied().unwrap_or(0);
        self.bit_pos += 1;
        (byte >> shift) & 1 != 0
    }

    /// Read a single bit (the next-least-significant bit of whatever
    /// multi-bit value it belongs to).
    ///
    /// # Errors
    /// [`DecodeError::UnexpectedEof`] if the reader is already at the end of
    /// `bytes`.
    pub fn read_bit(&mut self) -> Result<bool, DecodeError> {
        if self.bit_pos >= self.total_bits() {
            return Err(DecodeError::UnexpectedEof);
        }
        Ok(self.bit_unchecked())
    }

    /// Read `n` bits (`n <= 64`) LSb-first into a `u64`: the first bit read
    /// becomes bit 0 of the result, the last bit read becomes bit `n - 1`.
    ///
    /// Checks that all `n` bits are available *before* consuming any of
    /// them, so a failed read never partially advances the reader.
    ///
    /// # Errors
    /// [`DecodeError::UnexpectedEof`] if fewer than `n` bits remain.
    pub fn read_bits_u64(&mut self, n: u32) -> Result<u64, DecodeError> {
        debug_assert!(n <= 64, "read_bits_u64: n must be <= 64, got {n}");
        let n = n as usize;
        if n > self.total_bits().saturating_sub(self.bit_pos) {
            return Err(DecodeError::UnexpectedEof);
        }
        let mut v: u64 = 0;
        for i in 0..n as u32 {
            if self.bit_unchecked() {
                v |= 1u64 << i;
            }
        }
        Ok(v)
    }

    /// Read `n` bits (`n <= 32`) LSb-first into a `u32`. See [`Self::read_bits_u64`].
    ///
    /// # Errors
    /// [`DecodeError::UnexpectedEof`] if fewer than `n` bits remain.
    pub fn read_bits(&mut self, n: u32) -> Result<u32, DecodeError> {
        debug_assert!(n <= 32, "read_bits: n must be <= 32, got {n}");
        Ok(self.read_bits_u64(n)? as u32)
    }

    /// Total bits consumed so far.
    pub fn bits_read(&self) -> usize {
        self.bit_pos
    }
}

/// `ilog(n)`: bits needed to represent `0..=n-1` (spec §9.2.1) — equivalently,
/// the position of the highest set bit in `n`, plus one. `ilog(0) == 0`.
///
/// Spec pseudocode (§9.2.1, `ilog(x)`, adapted from the two's-complement
/// signed-`x` original to this crate's unsigned `u32` signature — the
/// spec's "negative returns 0" case does not apply):
/// ```text
/// 1) [return_value] = 0;
/// 2) if ( [x] is greater than zero ) {
///      3) increment [return_value];
///      4) logical shift [x] one bit to the right, padding the MSb with zero
///      5) repeat at step 2)
///    }
/// 6) done
/// ```
pub fn ilog(n: u32) -> u32 {
    let mut v = n;
    let mut ret = 0u32;
    while v > 0 {
        ret += 1;
        v >>= 1;
    }
    ret
}

/// `2^n` for integer `n`, via IEEE-754 exponent bit-pack — no `libm`/`powf`
/// (forbidden in this `no_std` crate). Same technique as
/// [`crate::formats::aac::sbr::envelope`]'s (private) `exp2i`.
///
/// [`float32_unpack`]'s exponent field is 10 bits wide (`0..=1023`) and its
/// bias is 788, so `n` can in principle span `-788..=235` — well outside
/// `f32`'s normalized-exponent range. No real Vorbis codebook ever encodes a
/// magnitude anywhere near that extreme, so clamping `n` to `[-126, 127]`
/// keeps the bit-pack itself well-defined (always a normalized, non-NaN,
/// non-infinite `f32`) and saturates gracefully towards `0`/`±infinity`
/// at the (unrealized-in-practice) extremes once multiplied by the mantissa,
/// rather than wrapping to a nonsense value.
fn pow2(n: i32) -> f32 {
    let clamped = n.clamp(-126, 127);
    f32::from_bits(((clamped + 127) as u32) << 23)
}

/// `float32_unpack(x)` (spec §9.2.2): unpack one of Vorbis's codebook
/// floats. This is **not** an IEEE-754 bit-cast — codebook floats are their
/// own mantissa/sign/exponent packing used only in the setup header
/// (codebook `minimum_value`/`delta_value`, floor curve amplitudes, etc.).
///
/// Spec pseudocode (§9.2.2, verified against `libvorbis`'s
/// `sharedbook.c: _float32_unpack`, which computes the same value via
/// `ldexp`):
/// ```text
/// 1) [mantissa]  = [x] bitwise AND 0x1fffff
/// 2) [sign]      = [x] bitwise AND 0x80000000
/// 3) [exponent]  = ([x] bitwise AND 0x7fe00000) shifted RIGHT 21 bits
/// 4) if ( [sign] is nonzero ) then negate [mantissa]
/// 5) return [mantissa] * ( 2 ^ ( [exponent] - 788 ) )
/// ```
pub fn float32_unpack(x: u32) -> f32 {
    let mantissa = (x & 0x001f_ffff) as i32;
    let sign = x & 0x8000_0000;
    let exponent = ((x & 0x7fe0_0000) >> 21) as i32;
    let mantissa = if sign != 0 { -mantissa } else { mantissa };
    mantissa as f32 * pow2(exponent - 788)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- ilog (spec §9.2.1) ---------------------------------------------

    #[test]
    fn ilog_matches_spec_examples() {
        // ilog(0)=0; ilog(1)=1; ilog(2)=2; ilog(3)=2; ilog(4)=3; ilog(7)=3;
        // ilog(8)=4; ilog(255)=8 — all straight from the spec's own worked
        // examples (§9.2.1).
        assert_eq!(ilog(0), 0);
        assert_eq!(ilog(1), 1);
        assert_eq!(ilog(2), 2);
        assert_eq!(ilog(3), 2);
        assert_eq!(ilog(4), 3);
        assert_eq!(ilog(7), 3);
        assert_eq!(ilog(8), 4);
        assert_eq!(ilog(255), 8);
    }

    // --- float32_unpack (spec §9.2.2) -----------------------------------

    #[test]
    fn float32_unpack_zero_is_zero() {
        assert_eq!(float32_unpack(0), 0.0);
    }

    #[test]
    fn float32_unpack_unit_mantissa_at_bias_is_one() {
        // exponent field == 788 (the bias) makes the power-of-two term
        // 2^(788-788) == 2^0 == 1, so mantissa==1 unpacks to exactly 1.0.
        let x = (788u32 << 21) | 1;
        assert_eq!(float32_unpack(x), 1.0);
    }

    #[test]
    fn float32_unpack_sign_bit_negates() {
        let x = 0x8000_0000 | (788u32 << 21) | 1;
        assert_eq!(float32_unpack(x), -1.0);
    }

    #[test]
    fn float32_unpack_scales_by_power_of_two_exponent() {
        // mantissa=3, exponent field=789 (bias+1) -> 3 * 2^1 == 6.0.
        let x = (789u32 << 21) | 3;
        assert_eq!(float32_unpack(x), 6.0);
    }

    #[test]
    fn float32_unpack_handles_negative_effective_exponent() {
        // Max 21-bit mantissa (2^21 - 1) with exponent field 788-21=767, so
        // the effective power is 2^-21: value == (2^21-1) * 2^-21, just
        // under 1.0. Exercises the "shift right" (fractional) direction,
        // not just left-shift/integer-multiple cases.
        let x = (767u32 << 21) | 0x001f_ffff;
        let want = 2_097_151.0_f32 * 2f32.powi(-21);
        assert_eq!(float32_unpack(x), want);
    }

    // --- BitReader: LSb-first (spec §2) ----------------------------------

    #[test]
    fn read_bit_is_lsb_first_within_a_byte() {
        // 0xB2 == 0b1011_0010. LSb-first means bit 0 (value 1, i.e. 0x01)
        // comes off the wire first, bit 7 (0x80) last.
        let mut r = BitReader::new(&[0xB2]);
        let bits: Vec<bool> = (0..8).map(|_| r.read_bit().unwrap()).collect();
        assert_eq!(
            bits,
            vec![false, true, false, false, true, true, false, true]
        );
    }

    #[test]
    fn read_bits_reproduces_spec_2_1_worked_example() {
        // Spec §2.1's own bit-packing worked example: packing a 4-bit value
        // 12, then a 3-bit value 7 (0b111), then a 7-bit value 17, then a
        // 13-bit value 6969, LSb-first back-to-back, produces the byte
        // sequence [0xFC, 0x48, 0xCE, 0x06]. Hand-verified independently by
        // packing each value's bits LSb-first starting at the running
        // cursor position and cross-checked against the spec's own
        // byte-by-byte diagram (matches exactly at every one of the 4
        // bytes). This is the single most important test in this module:
        // it exercises multi-bit reads that start mid-byte and straddle
        // byte boundaries, which is exactly where an LSb/MSb mix-up shows
        // up.
        let mut r = BitReader::new(&[0xFC, 0x48, 0xCE, 0x06]);
        assert_eq!(r.read_bits(4).unwrap(), 12);
        assert_eq!(r.read_bits(3).unwrap(), 0b111); // 7 -- the 3-bit pattern for -1
        assert_eq!(r.read_bits(7).unwrap(), 17);
        assert_eq!(r.read_bits(13).unwrap(), 6969);
        assert_eq!(r.bits_read(), 4 + 3 + 7 + 13);
    }

    #[test]
    fn read_bits_u64_is_little_endian_at_byte_and_bit_level() {
        // Byte 0 supplies the *least*-significant bits of the u64 and byte
        // 7 the *most*-significant, mirroring how read_bits_u64 assembles
        // wide values: byte 0 = 0x01 -> bit 0 of the u64 is set; byte 7 =
        // 0x80 -> bit 63 of the u64 is set.
        let bytes = [0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80];
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_bits_u64(64).unwrap(), 0x8000_0000_0000_0001);
        assert_eq!(r.bits_read(), 64);
    }

    #[test]
    fn read_bits_u64_all_ones() {
        let bytes = [0xFFu8; 8];
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_bits_u64(64).unwrap(), u64::MAX);
    }

    #[test]
    fn read_bits_zero_is_a_noop() {
        let mut r = BitReader::new(&[0xFF]);
        assert_eq!(r.read_bits(0).unwrap(), 0);
        assert_eq!(r.bits_read(), 0);
    }

    #[test]
    fn read_bits_errors_at_unexpected_eof_without_partial_consumption() {
        // Only 8 bits available; asking for 9 must fail cleanly and must
        // not advance bit_pos (all-or-nothing semantics).
        let mut r = BitReader::new(&[0xFF]);
        assert_eq!(r.read_bits(9), Err(DecodeError::UnexpectedEof));
        assert_eq!(r.bits_read(), 0);
    }

    #[test]
    fn read_bit_past_end_errors_after_exact_exhaustion() {
        let mut r = BitReader::new(&[0xAB]);
        assert_eq!(r.read_bits(8).unwrap(), 0xAB);
        assert_eq!(r.read_bit(), Err(DecodeError::UnexpectedEof));
    }
}
