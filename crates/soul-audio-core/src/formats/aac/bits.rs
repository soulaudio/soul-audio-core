//! MSB-first bit reader over a byte slice, for the AAC-LC bitstream (Huffman
//! codewords, side info, and — in later tasks — the rest of the element
//! syntax). A separate copy from [`crate::formats::alac::BitReader`] (do not
//! share or modify that one): AAC needs single reads up to 32 bits (the
//! scalefactor Huffman book has codewords up to 19 bits, and codebook 11's
//! escape mechanism reads up to a 15-bit offset), where ALAC's reader caps
//! `read` at 16 bits and routes anything wider through a separate
//! `read_long`.
//!
//! Reads past the end of `data` are zero-padded (mirroring ALAC's reader) so
//! a truncated/malformed trailing frame degrades to a bounded decode error
//! rather than a panic or out-of-bounds access.

pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    /// Construct a reader positioned at absolute bit offset `bit_pos`. Used to
    /// re-open an SBR fill-element payload at the exact bit where the syntax
    /// parser found it, once the LC core PCM the SBR drive needs is ready
    /// (`formats::aac::decode_rdb`). Reads past the end of `data` are still
    /// zero-padded, so an out-of-range `bit_pos` degrades to zero reads rather
    /// than panicking.
    pub(crate) fn new_at(data: &'a [u8], bit_pos: usize) -> Self {
        Self { data, bit_pos }
    }

    #[inline]
    fn byte(&self, i: usize) -> u64 {
        self.data.get(i).copied().map_or(0, u64::from)
    }

    /// Read `n` bits (0 <= n <= 32), MSB-first, right-aligned. Reads past the
    /// end of `data` return zero bits.
    pub(crate) fn read(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        let byte_idx = self.bit_pos >> 3;
        let bit = (self.bit_pos & 7) as u32;
        // A 40-bit (5-byte) window is always enough: the farthest bit we
        // might need is at offset `bit + n - 1 <= 7 + 32 - 1 = 38`.
        let window: u64 = (self.byte(byte_idx) << 32)
            | (self.byte(byte_idx + 1) << 24)
            | (self.byte(byte_idx + 2) << 16)
            | (self.byte(byte_idx + 3) << 8)
            | self.byte(byte_idx + 4);
        let masked = (window << bit) & 0xFF_FFFF_FFFF; // keep the low 40 bits
        let v = masked >> (40 - n);
        self.bit_pos += n as usize;
        v as u32
    }

    /// Read a single bit.
    pub(crate) fn read_bit(&mut self) -> u32 {
        let byte_idx = self.bit_pos >> 3;
        let shift = 7 - (self.bit_pos & 7) as u32;
        let v = (self.byte(byte_idx) >> shift) & 1;
        self.bit_pos += 1;
        v as u32
    }

    /// Advance to the next byte boundary (a no-op if already aligned).
    pub(crate) fn byte_align(&mut self) {
        let rem = self.bit_pos & 7;
        if rem != 0 {
            self.bit_pos += 8 - rem;
        }
    }

    pub(crate) fn bit_pos(&self) -> usize {
        self.bit_pos
    }

    /// Bits remaining in `data` before the (zero-padded) end; saturates to 0
    /// once `bit_pos` has passed the true end of the buffer. Kept for API
    /// symmetry with the other readers; currently exercised only by tests.
    #[allow(dead_code)]
    pub(crate) fn bits_left(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.bit_pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_msb_first() {
        // 0b1010_0110, 0b1100_0000
        let mut r = BitReader::new(&[0xA6, 0xC0]);
        assert_eq!(r.read(3), 0b101); // top 3 bits
        assert_eq!(r.read(1), 0);
        assert_eq!(r.read(4), 0b0110);
        assert_eq!(r.read_bit(), 1);
        assert_eq!(r.read(3), 0b100);
    }

    #[test]
    fn reads_past_19_bits() {
        // Scalefactor codewords go up to 19 bits; verify a >16-bit read in
        // one call (ALAC's reader needs a separate `read_long` for this).
        let mut r = BitReader::new(&[0xFF, 0xFF, 0xFF]);
        assert_eq!(r.read(19), 0x7_FFFF);
    }

    #[test]
    fn reads_32_bits() {
        let mut r = BitReader::new(&[0x12, 0x34, 0x56, 0x78, 0x9A]);
        assert_eq!(r.read(32), 0x1234_5678);
        assert_eq!(r.read(8), 0x9A);
    }

    #[test]
    fn byte_align_and_bits_left() {
        let mut r = BitReader::new(&[0xFF, 0x00, 0x12]);
        assert_eq!(r.bits_left(), 24);
        assert_eq!(r.read(4), 0xF);
        assert_eq!(r.bits_left(), 20);
        r.byte_align();
        assert_eq!(r.bit_pos(), 8);
        assert_eq!(r.read(8), 0x00);
        assert_eq!(r.read(8), 0x12);
        assert_eq!(r.bits_left(), 0);
    }

    #[test]
    fn reads_past_end_are_zero_padded() {
        let mut r = BitReader::new(&[0xFF]);
        assert_eq!(r.read(4), 0xF);
        // 4 real bits left, then zero-padding past the buffer's true end.
        assert_eq!(r.read(16), 0xF000);
    }
}
