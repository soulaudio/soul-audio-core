//! Opus range decoder (`ec_dec`), RFC 6716 §4.1.
//!
//! This is the entropy-coder core every downstream CELT/SILK stage feeds
//! through, so it must be **bit-exact** with libopus. The algorithm here is
//! transcribed line-for-line from libopus `celt/entdec.c` + `celt/entcode.c`
//! (the *libopus* top-renormalization formulation — `rng = 128`,
//! `val = 127 - (b0 >> 1)` at init — not the RFC's alternate pseudocode, which
//! is arithmetically equivalent but shaped differently).
//!
//! # The classic trap: two readers that meet in the middle
//!
//! An Opus packet is consumed from **both ends at once**. The arithmetic
//! (range-coded) stream advances **forward** from the front of the buffer via
//! [`RangeDecoder::decode`] / [`RangeDecoder::dec_update`] and their
//! renormalization ([`RangeDecoder::normalize`], which pulls one byte at a
//! time through [`RangeDecoder::read_byte`]). Independently, raw bits
//! ([`RangeDecoder::dec_bits`], used for e.g. CELT pulse signs) are read
//! **backward** from the *end* of the same buffer via
//! [`RangeDecoder::read_byte_from_end`]. The two cursors only ever approach
//! each other; a well-formed packet guarantees they never cross. Getting this
//! wrong makes every later CELT/SILK bug masquerade as a range-coder bug, so
//! the tests below exercise the front/back coexistence explicitly on a
//! deliberately tight buffer.
//!
//! # Modular (`wrapping_*`) arithmetic
//!
//! libopus is written in C over `uint32_t`, which is defined to wrap. A few
//! sites here (`normalize`'s `val` update, the `ext * (…)` products in
//! `dec_update` / `decode_icdf`) genuinely overflow 32 bits before a mask or
//! subtraction brings them back in range — libopus relies on the wraparound.
//! Those sites use `wrapping_shl` / `wrapping_add` / `wrapping_mul` so a Rust
//! debug build stays bit-identical instead of panicking. Every other
//! subtraction here (`val -= s`, `rng - s`, `t - s`) is provably
//! non-negative for a well-formed stream (see the per-function notes), so it
//! is left as a plain operator on purpose — a panic there would signal a real
//! logic bug, not a modular-arithmetic artifact.

// --- Constants (libopus `celt/entcode.h`) --------------------------------

/// Bits per input symbol (one byte).
const EC_SYM_BITS: u32 = 8;
/// Total bits in the coder state word.
const EC_CODE_BITS: u32 = 32;
/// Maximum symbol value (`(1 << EC_SYM_BITS) - 1`).
const EC_SYM_MAX: u32 = 0x0000_00FF;
/// The high-order bit of the code word (`1 << (EC_CODE_BITS - 1)`).
const EC_CODE_TOP: u32 = 0x8000_0000;
/// The lowest point at which the range must be renormalized
/// (`EC_CODE_TOP >> EC_SYM_BITS`).
const EC_CODE_BOT: u32 = 0x0080_0000;
/// Bits available beyond a whole number of symbols (`(EC_CODE_BITS - 2) %
/// EC_SYM_BITS + 1`).
const EC_CODE_EXTRA: u32 = 7;
/// Threshold (in `EC_ILOG` terms) at which `dec_uint` splits off raw bits.
const EC_UINT_BITS: u32 = 8;
/// Width of the raw-bit accumulation window, in bits.
const EC_WINDOW_SIZE: u32 = 32;
/// Fractional-bit resolution for [`RangeDecoder::tell_frac`] (`ec_tell_frac`).
const BITRES: u32 = 3;

/// `EC_ILOG(x)`: the number of significant bits in `x` (i.e. `floor(log2(x)) +
/// 1`), with `EC_ILOG(0) == 0`. libopus defines this via count-leading-zeros
/// and leaves the `x == 0` case undefined (it is never called with 0);
/// `EC_CODE_BITS - x.leading_zeros()` happens to yield the desired `0` there,
/// so the same expression is safe for every input.
const fn ec_ilog(x: u32) -> u32 {
    EC_CODE_BITS - x.leading_zeros()
}

/// The range decoder state (libopus `ec_dec` / `ec_ctx`).
pub struct RangeDecoder<'a> {
    /// The packet being decoded.
    buf: &'a [u8],
    /// `buf.len()` — cached so the read helpers don't re-borrow the slice.
    storage: usize,
    /// Forward (front) read cursor for the arithmetic stream.
    offs: usize,
    /// Number of bytes already consumed from the **end** by the raw-bit reader.
    end_offs: usize,
    /// Accumulated raw bits pulled from the end but not yet returned.
    end_window: u32,
    /// Number of valid bits currently in `end_window`.
    nend_bits: u32,
    /// Total bits consumed so far (front + back), used by `tell`/`tell_frac`.
    nbits_total: u32,
    /// The difference between the most recent symbol boundary and `val`.
    val: u32,
    /// The size of the current range.
    rng: u32,
    /// The most recent raw byte read from the front (`-1`-style init is
    /// unnecessary here; it is always set before use).
    rem: i32,
    /// `rng / ft` cached by [`RangeDecoder::decode`] for the paired
    /// [`RangeDecoder::dec_update`].
    ext: u32,
    /// Set once a `dec_uint` value overflows its declared range; sticky.
    error: bool,
}

impl<'a> RangeDecoder<'a> {
    /// Initialize the decoder over `buf` (libopus `ec_dec_init`): seed
    /// `rng = 128` and `val = 127 - (b0 >> 1)` from the first front byte, then
    /// renormalize so the range is valid.
    pub fn new(buf: &'a [u8]) -> Self {
        let mut dec = Self {
            buf,
            storage: buf.len(),
            offs: 0,
            end_offs: 0,
            end_window: 0,
            nend_bits: 0,
            // 33 - 3*8 = 9; the offset from which `tell` subtracts partial bits.
            nbits_total: EC_CODE_BITS + 1
                - ((EC_CODE_BITS - EC_CODE_EXTRA) / EC_SYM_BITS) * EC_SYM_BITS,
            val: 0,
            rng: 1u32 << EC_CODE_EXTRA,
            rem: 0,
            ext: 0,
            error: false,
        };
        dec.rem = dec.read_byte();
        // val = rng - 1 - (rem >> (EC_SYM_BITS - EC_CODE_EXTRA)) = 127 - (rem >> 1).
        dec.val = (dec.rng as i32 - 1 - (dec.rem >> (EC_SYM_BITS - EC_CODE_EXTRA))) as u32;
        dec.normalize();
        dec
    }

    /// Read one byte forward from the front (libopus `ec_read_byte`); `0` past
    /// the end of the packet.
    fn read_byte(&mut self) -> i32 {
        if self.offs < self.storage {
            let b = self.buf[self.offs];
            self.offs += 1;
            i32::from(b)
        } else {
            0
        }
    }

    /// Read one byte backward from the end (libopus `ec_read_byte_from_end`);
    /// `0` once the raw-bit reader has consumed the whole packet.
    fn read_byte_from_end(&mut self) -> i32 {
        if self.end_offs < self.storage {
            self.end_offs += 1;
            i32::from(self.buf[self.storage - self.end_offs])
        } else {
            0
        }
    }

    /// Renormalize the range, pulling front bytes until `rng > EC_CODE_BOT`
    /// (libopus `ec_dec_normalize`).
    ///
    /// The `val` update multiplies by 256 (`<< 8`) *before* masking to
    /// `EC_CODE_TOP - 1`, which overflows 32 bits; `wrapping_shl` /
    /// `wrapping_add` reproduce C's modular result exactly.
    fn normalize(&mut self) {
        while self.rng <= EC_CODE_BOT {
            self.nbits_total += EC_SYM_BITS;
            self.rng <<= EC_SYM_BITS;
            let sym_prev = self.rem;
            self.rem = self.read_byte();
            // sym = ((sym_prev << 8) | rem) >> (EC_SYM_BITS - EC_CODE_EXTRA) = (…) >> 1.
            let sym =
                (((sym_prev << EC_SYM_BITS) | self.rem) >> (EC_SYM_BITS - EC_CODE_EXTRA)) as u32;
            self.val = self
                .val
                .wrapping_shl(EC_SYM_BITS)
                .wrapping_add(EC_SYM_MAX & !sym)
                & (EC_CODE_TOP - 1);
        }
    }

    /// Decode the cumulative frequency of the next symbol given the total
    /// frequency `ft` (libopus `ec_decode`). The caller inspects the returned
    /// value against its frequency table, then calls [`Self::dec_update`] with
    /// the chosen symbol's `[fl, fh)` bounds. `ext = rng / ft` is stashed for
    /// that update.
    pub fn decode(&mut self, ft: u32) -> u32 {
        self.ext = self.rng / ft;
        let s = self.val / self.ext;
        ft - core::cmp::min(s + 1, ft)
    }

    /// Fold the chosen symbol's `[fl, fh)` bounds (out of total `ft`) back into
    /// the coder state and renormalize (libopus `ec_dec_update`).
    ///
    /// The two `ext * (…)` products are bounded by `rng < 2^32` but are written
    /// with `wrapping_mul` to match libopus's modular semantics. `val -= s` is
    /// provably non-negative: `decode` guarantees `val / ext >= ft - fh`, so
    /// `val >= ext * (ft - fh) = s`.
    pub fn dec_update(&mut self, fl: u32, fh: u32, ft: u32) {
        let s = self.ext.wrapping_mul(ft - fh);
        self.val -= s;
        self.rng = if fl > 0 {
            self.ext.wrapping_mul(fh - fl)
        } else {
            self.rng - s
        };
        self.normalize();
    }

    /// Decode a single bit with probability `1/(1 << logp)` of being one
    /// (libopus `ec_dec_bit_logp`). `val -= s` only runs on the `ret == 0`
    /// branch, where `val >= s` holds by construction.
    pub fn decode_bit_logp(&mut self, logp: u32) -> u32 {
        let r = self.rng;
        let d = self.val;
        let s = r >> logp;
        let ret = u32::from(d < s);
        if ret == 0 {
            self.val = d - s;
        }
        self.rng = if ret == 1 { s } else { r - s };
        self.normalize();
        ret
    }

    /// Decode a symbol against an inverse-CDF table `icdf` scaled to `1 << ftb`
    /// (libopus `ec_dec_icdf`). `icdf` is non-increasing and terminates at `0`,
    /// which bounds the loop. The `r * icdf[…]` product uses `wrapping_mul`
    /// (modular, like libopus); the loop exits with `d >= s`, so `val = d - s`
    /// and `rng = t - s` are both non-negative.
    pub fn decode_icdf(&mut self, icdf: &[u8], ftb: u32) -> u32 {
        let d = self.val;
        let r = self.rng >> ftb;
        let mut s = self.rng;
        let mut t;
        let mut ret: i32 = -1;
        loop {
            t = s;
            ret += 1;
            s = r.wrapping_mul(u32::from(icdf[ret as usize]));
            if d >= s {
                break;
            }
        }
        self.val = d - s;
        self.rng = t - s;
        self.normalize();
        ret as u32
    }

    /// Read `bits` raw bits from the **end** of the packet (libopus
    /// `ec_dec_bits`). This is the backward reader that shares the buffer with
    /// the forward arithmetic stream; `bits == 0` is a no-op returning `0`.
    /// `bits` is always `<= 25` in Opus (the widest split in
    /// [`Self::dec_uint`]), well within the 32-bit window.
    pub fn dec_bits(&mut self, bits: u32) -> u32 {
        let mut window = self.end_window;
        let mut available = self.nend_bits;
        if available < bits {
            loop {
                window |= (self.read_byte_from_end() as u32) << available;
                available += EC_SYM_BITS;
                if available > EC_WINDOW_SIZE - EC_SYM_BITS {
                    break;
                }
            }
        }
        let mask = if bits >= 32 {
            u32::MAX
        } else {
            (1u32 << bits) - 1
        };
        let ret = window & mask;
        window >>= bits;
        available -= bits;
        self.end_window = window;
        self.nend_bits = available;
        self.nbits_total += bits;
        ret
    }

    /// Decode a uniformly-distributed integer in `[0, ft)` (libopus
    /// `ec_dec_uint`); requires `ft > 1`. Large values are split into an
    /// arithmetic high part and raw low bits. A value exceeding `ft - 1` (only
    /// possible from a corrupt stream) sets [`Self::error`] and clamps.
    pub fn dec_uint(&mut self, ft: u32) -> u32 {
        debug_assert!(ft > 1, "dec_uint requires ft > 1");
        let ft1 = ft - 1;
        let ftb = ec_ilog(ft1);
        if ftb > EC_UINT_BITS {
            let ftb2 = ftb - EC_UINT_BITS;
            let ftv = (ft1 >> ftb2) + 1;
            let s = self.decode(ftv);
            self.dec_update(s, s + 1, ftv);
            let t = (s << ftb2) | self.dec_bits(ftb2);
            if t <= ft1 {
                t
            } else {
                self.error = true;
                ft1
            }
        } else {
            let s = self.decode(ft);
            self.dec_update(s, s + 1, ft);
            s
        }
    }

    /// Bits consumed so far, rounded up to the whole bit (libopus `ec_tell`).
    pub fn tell(&self) -> u32 {
        self.nbits_total - ec_ilog(self.rng)
    }

    /// Bits consumed so far in eighth-of-a-bit units (libopus `ec_tell_frac`).
    pub fn tell_frac(&self) -> u32 {
        // Correction table for the fractional bits of log2(rng) (libopus).
        const CORRECTION: [u32; 8] = [35733, 38967, 42495, 46340, 50535, 55109, 60097, 65535];
        let nbits = self.nbits_total << BITRES;
        let l = ec_ilog(self.rng);
        // `rng > EC_CODE_BOT` after every renormalization, so `l >= 24` and the
        // shift below is always non-negative.
        let r = self.rng >> (l - 16);
        let b = (r >> 12) - 8;
        let b = b + u32::from(r > CORRECTION[b as usize]);
        let l = (l << BITRES) + b;
        nbits - l
    }

    /// Whether a `dec_uint` value has overflowed its declared range on this
    /// stream (sticky). Downstream code treats a set flag as a corrupt packet.
    pub fn error(&self) -> bool {
        self.error
    }

    /// The packet length in bytes the decoder was initialized over (libopus
    /// `ec_dec.storage`). CELT coarse-energy decode derives its per-frame bit
    /// budget from this (`budget = storage * 8`, RFC 6716 §4.3.2).
    pub fn storage(&self) -> usize {
        self.storage
    }

    /// The current range value (`ec_ctx.rng`). CELT reads this after a frame's
    /// entropy decode and stashes it as the next frame's PVQ-fold / anti-collapse
    /// LCG seed (libopus `st->rng = dec->rng`, `celt/celt_decoder.c`).
    pub fn rng(&self) -> u32 {
        self.rng
    }

    /// Shrink the effective packet length by `by` bytes (libopus
    /// `dec.storage -= redundancy_bytes` in `opus_decode_frame`). The redundant
    /// CELT frame's bytes live at the **end** of the Opus frame; after SILK has
    /// been decoded and the redundancy header read, the main CELT decode must
    /// treat those trailing bytes as absent so its raw-bit (back) reader and its
    /// `storage * 8` bit budget both exclude them. Saturating, so a malformed
    /// `redundancy_bytes` can never underflow the length.
    pub fn shrink_storage(&mut self, by: usize) {
        self.storage = self.storage.saturating_sub(by);
    }

    /// Fast-forward the bit accounting so [`Self::tell`] reports `total_bits`,
    /// consuming no actual stream bytes. This is libopus's CELT silence
    /// handling in `celt_decode_with_ec` — `dec->nbits_total += len*8 -
    /// ec_tell(dec)` — which pretends the rest of the frame has been read.
    /// With the budget thus exhausted, every band in coarse-energy decode
    /// falls through to the minimum-energy (`qi = -1`) branch, which is how a
    /// silent CELT frame is represented. The `i64` intermediate mirrors C's
    /// signed `tell - ec_tell(dec)` (which can be negative on a tiny frame)
    /// wrapping into the `u32` accumulator.
    pub fn skip_to_end(&mut self, total_bits: u32) {
        let delta = i64::from(total_bits) - i64::from(self.tell());
        self.nbits_total = (i64::from(self.nbits_total) + delta) as u32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ================================================================
    // Independently-transcribed range ENCODER (test-only).
    //
    // Transcribed from a DIFFERENT libopus file (`celt/entenc.c`) than the
    // decoder under test (`celt/entdec.c`), and it writes bits in the OPPOSITE
    // direction (arithmetic bytes grow forward from the front; raw bits grow
    // backward from the end). A round-trip that survives thousands of random
    // programs therefore can't be passing on a bug the two share — a
    // compensating error would have to appear independently in both files and
    // cancel across the front/back boundary, which a single shared typo can't
    // do. This is what lets us prove bit-exactness without an external oracle.
    // ================================================================

    /// `EC_CODE_BITS - EC_SYM_BITS - 1`; the shift `ec_enc_carry_out` uses to
    /// peel the high symbol off `val`. Encoder-only, hence defined here.
    const EC_CODE_SHIFT: u32 = 23;

    struct RangeEncoder {
        buf: Vec<u8>,
        storage: usize,
        offs: usize,
        end_offs: usize,
        end_window: u32,
        nend_bits: u32,
        nbits_total: u32,
        rng: u32,
        val: u32,
        /// Count of deferred `0xFF` carry symbols.
        ext: u32,
        /// Buffered output byte awaiting carry propagation, or `-1` if none.
        rem: i32,
        error: bool,
    }

    impl RangeEncoder {
        fn new(size: usize) -> Self {
            Self {
                buf: vec![0u8; size],
                storage: size,
                offs: 0,
                end_offs: 0,
                end_window: 0,
                nend_bits: 0,
                nbits_total: EC_CODE_BITS + 1,
                rng: EC_CODE_TOP,
                val: 0,
                ext: 0,
                rem: -1,
                error: false,
            }
        }

        fn write_byte(&mut self, value: u32) {
            if self.offs + self.end_offs >= self.storage {
                self.error = true;
                return;
            }
            self.buf[self.offs] = (value & 0xFF) as u8;
            self.offs += 1;
        }

        fn write_byte_at_end(&mut self, value: u32) {
            if self.offs + self.end_offs >= self.storage {
                self.error = true;
                return;
            }
            self.end_offs += 1;
            let idx = self.storage - self.end_offs;
            self.buf[idx] = (value & 0xFF) as u8;
        }

        fn carry_out(&mut self, c: i32) {
            if c as u32 != EC_SYM_MAX {
                let carry = c >> EC_SYM_BITS;
                if self.rem >= 0 {
                    self.write_byte((self.rem + carry) as u32);
                }
                if self.ext > 0 {
                    let sym = EC_SYM_MAX.wrapping_add(carry as u32) & EC_SYM_MAX;
                    loop {
                        self.write_byte(sym);
                        self.ext -= 1;
                        if self.ext == 0 {
                            break;
                        }
                    }
                }
                self.rem = (c as u32 & EC_SYM_MAX) as i32;
            } else {
                self.ext += 1;
            }
        }

        fn normalize(&mut self) {
            while self.rng <= EC_CODE_BOT {
                self.carry_out((self.val >> EC_CODE_SHIFT) as i32);
                self.val = self.val.wrapping_shl(EC_SYM_BITS) & (EC_CODE_TOP - 1);
                self.rng <<= EC_SYM_BITS;
                self.nbits_total += EC_SYM_BITS;
            }
        }

        fn encode(&mut self, fl: u32, fh: u32, ft: u32) {
            let r = self.rng / ft;
            if fl > 0 {
                self.val = self.val.wrapping_add(self.rng - r.wrapping_mul(ft - fl));
                self.rng = r.wrapping_mul(fh - fl);
            } else {
                self.rng -= r.wrapping_mul(ft - fh);
            }
            self.normalize();
        }

        fn enc_bit_logp(&mut self, bit: u32, logp: u32) {
            let r = self.rng;
            let l = self.val;
            let s = r >> logp;
            let r2 = r - s;
            if bit != 0 {
                self.val = l.wrapping_add(r2);
            }
            self.rng = if bit != 0 { s } else { r2 };
            self.normalize();
        }

        fn enc_icdf(&mut self, s: usize, icdf: &[u8], ftb: u32) {
            let r = self.rng >> ftb;
            if s > 0 {
                self.val = self
                    .val
                    .wrapping_add(self.rng - r.wrapping_mul(u32::from(icdf[s - 1])));
                self.rng = r.wrapping_mul(u32::from(icdf[s - 1]) - u32::from(icdf[s]));
            } else {
                self.rng -= r.wrapping_mul(u32::from(icdf[s]));
            }
            self.normalize();
        }

        fn enc_bits(&mut self, val: u32, bits: u32) {
            if bits == 0 {
                return;
            }
            let mut window = self.end_window;
            let mut used = self.nend_bits;
            if used + bits > EC_WINDOW_SIZE {
                loop {
                    self.write_byte_at_end(window & EC_SYM_MAX);
                    window >>= EC_SYM_BITS;
                    used -= EC_SYM_BITS;
                    if used < EC_SYM_BITS {
                        break;
                    }
                }
            }
            window |= val << used;
            used += bits;
            self.end_window = window;
            self.nend_bits = used;
            self.nbits_total += bits;
        }

        fn enc_uint(&mut self, val: u32, ft: u32) {
            assert!(ft > 1);
            let ft1 = ft - 1;
            let ftb = ec_ilog(ft1);
            if ftb > EC_UINT_BITS {
                let ftb2 = ftb - EC_UINT_BITS;
                let ftv = (ft1 >> ftb2) + 1;
                let fl = val >> ftb2;
                self.encode(fl, fl + 1, ftv);
                self.enc_bits(val & ((1u32 << ftb2) - 1), ftb2);
            } else {
                self.encode(val, val + 1, ft);
            }
        }

        fn tell(&self) -> u32 {
            self.nbits_total - ec_ilog(self.rng)
        }

        fn tell_frac(&self) -> u32 {
            const CORRECTION: [u32; 8] = [35733, 38967, 42495, 46340, 50535, 55109, 60097, 65535];
            let nbits = self.nbits_total << BITRES;
            let l = ec_ilog(self.rng);
            let r = self.rng >> (l - 16);
            let b = (r >> 12) - 8;
            let b = b + u32::from(r > CORRECTION[b as usize]);
            let l = (l << BITRES) + b;
            nbits - l
        }

        /// libopus `ec_enc_done`: flush the minimum number of bits that pin the
        /// symbols decoded so far, then merge any buffered raw bits.
        fn done(&mut self) {
            let mut l = (EC_CODE_BITS - ec_ilog(self.rng)) as i32;
            let mut msk = (EC_CODE_TOP - 1) >> l;
            let mut end = self.val.wrapping_add(msk) & !msk;
            if (end | msk) >= self.val.wrapping_add(self.rng) {
                l += 1;
                msk >>= 1;
                end = self.val.wrapping_add(msk) & !msk;
            }
            while l > 0 {
                self.carry_out((end >> EC_CODE_SHIFT) as i32);
                end = end.wrapping_shl(EC_SYM_BITS) & (EC_CODE_TOP - 1);
                l -= EC_SYM_BITS as i32;
            }
            if self.rem >= 0 || self.ext > 0 {
                self.carry_out(0);
            }
            let mut window = self.end_window;
            let mut used = self.nend_bits as i32;
            while used >= EC_SYM_BITS as i32 {
                self.write_byte_at_end(window & EC_SYM_MAX);
                window >>= EC_SYM_BITS;
                used -= EC_SYM_BITS as i32;
            }
            if !self.error {
                self.buf[self.offs..self.storage - self.end_offs].fill(0);
                if used > 0 {
                    if self.end_offs >= self.storage {
                        self.error = true;
                    } else {
                        l = -l;
                        if self.offs + self.end_offs >= self.storage && l < used {
                            window &= (1u32 << l) - 1;
                            self.error = true;
                        }
                        let idx = self.storage - self.end_offs - 1;
                        self.buf[idx] |= (window & 0xFF) as u8;
                    }
                }
            }
        }
    }

    // --- Seeded xorshift PRNG (no external crates) -----------------------

    struct XorShift(u64);

    impl XorShift {
        fn new(seed: u64) -> Self {
            // xorshift needs a nonzero state.
            Self(seed | 1)
        }
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        /// Uniform in `[0, n)`; `n > 0`.
        fn below(&mut self, n: u32) -> u32 {
            (self.next_u64() % u64::from(n)) as u32
        }
        /// Uniform in `[lo, hi)`; `lo < hi`.
        fn range(&mut self, lo: u32, hi: u32) -> u32 {
            lo + self.below(hi - lo)
        }
    }

    // --- Program model ---------------------------------------------------

    enum Op {
        /// `encode(fl, fh, ft)` on the way in; `decode(ft)` must land a
        /// frequency inside `[fl, fh)` on the way out.
        Arith {
            fl: u32,
            fh: u32,
            ft: u32,
        },
        Icdf {
            s: usize,
            icdf: Vec<u8>,
            ftb: u32,
        },
        BitLogp {
            bit: u32,
            logp: u32,
        },
        Bits {
            val: u32,
            bits: u32,
        },
        Uint {
            val: u32,
            ft: u32,
        },
    }

    /// Build a valid inverse-CDF table of `n` symbols scaled to `1 << ftb`
    /// (`n <= 1 << ftb`), non-increasing and terminating at `0`.
    fn build_icdf(rng: &mut XorShift, n: usize, ftb: u32) -> Vec<u8> {
        let tot = 1u32 << ftb;
        let mut freq = vec![1u32; n];
        let mut remaining = tot - n as u32;
        while remaining > 0 {
            let i = rng.below(n as u32) as usize;
            freq[i] += 1;
            remaining -= 1;
        }
        let mut cdf = vec![0u32; n + 1];
        for i in 0..n {
            cdf[i + 1] = cdf[i] + freq[i];
        }
        (0..n).map(|i| (tot - cdf[i + 1]) as u8).collect()
    }

    fn gen_op(rng: &mut XorShift) -> Op {
        match rng.below(5) {
            0 => {
                // Arbitrary-width arithmetic bucket, including large `ft`.
                let ft = rng.range(2, 1 << 20);
                let fh = rng.range(1, ft + 1);
                let fl = rng.below(fh);
                Op::Arith { fl, fh, ft }
            }
            1 => {
                let ftb = rng.range(1, 9); // 1..=8
                let tot = 1u32 << ftb;
                let maxn = tot.min(8);
                let n = rng.range(2, maxn + 1) as usize; // 2..=maxn
                let icdf = build_icdf(rng, n, ftb);
                let s = rng.below(n as u32) as usize;
                Op::Icdf { s, icdf, ftb }
            }
            2 => Op::BitLogp {
                bit: rng.below(2),
                logp: rng.range(1, 9),
            },
            3 => {
                let bits = rng.range(1, 25); // 1..=24
                let val = rng.below(1u32 << bits);
                Op::Bits { val, bits }
            }
            _ => {
                let ft = rng.range(2, 1 << 24);
                let val = rng.below(ft);
                Op::Uint { val, ft }
            }
        }
    }

    fn gen_program(rng: &mut XorShift, len: usize) -> Vec<Op> {
        (0..len).map(|_| gen_op(rng)).collect()
    }

    /// Encode `program` into a buffer of exactly `storage` bytes. Returns
    /// `None` if the encoder busts (buffer too small).
    fn encode_program(program: &[Op], storage: usize) -> Option<(Vec<u8>, u32, u32)> {
        let mut enc = RangeEncoder::new(storage);
        for op in program {
            match op {
                Op::Arith { fl, fh, ft } => enc.encode(*fl, *fh, *ft),
                Op::Icdf { s, icdf, ftb } => enc.enc_icdf(*s, icdf, *ftb),
                Op::BitLogp { bit, logp } => enc.enc_bit_logp(*bit, *logp),
                Op::Bits { val, bits } => enc.enc_bits(*val, *bits),
                Op::Uint { val, ft } => enc.enc_uint(*val, *ft),
            }
            if enc.error {
                return None;
            }
        }
        // `tell`/`tell_frac` are captured *before* `done` so they can be
        // cross-checked against the decoder's, which necessarily stops there.
        let tell = enc.tell();
        let tell_frac = enc.tell_frac();
        enc.done();
        if enc.error {
            return None;
        }
        Some((enc.buf, tell, tell_frac))
    }

    /// Encode with a generous buffer (a wide zero gap between the front
    /// arithmetic bytes and the back raw bits — exercises the gap path).
    fn encode_auto(program: &[Op]) -> (Vec<u8>, u32, u32) {
        let mut storage = 64usize;
        loop {
            if let Some(r) = encode_program(program, storage) {
                return r;
            }
            storage *= 2;
            assert!(storage < (1 << 22), "program never fit");
        }
    }

    /// Encode with the *minimal* non-busting buffer (front and back cursors
    /// packed as tightly as `ec_enc_done` permits — the real meet-in-the-middle
    /// case).
    fn encode_min(program: &[Op]) -> (Vec<u8>, u32, u32) {
        let mut hi = 8usize;
        while encode_program(program, hi).is_none() {
            hi *= 2;
            assert!(hi < (1 << 22), "program never fit");
        }
        let (mut a, mut b) = (1usize, hi);
        while a < b {
            let m = (a + b) / 2;
            if encode_program(program, m).is_some() {
                b = m;
            } else {
                a = m + 1;
            }
        }
        encode_program(program, a).expect("minimal storage encodes")
    }

    /// Decode `program` from `buf` and assert every recovered symbol matches
    /// what was encoded, then that `tell`/`tell_frac` agree with the encoder.
    fn decode_and_check(program: &[Op], buf: &[u8], enc_tell: u32, enc_tell_frac: u32) {
        let mut dec = RangeDecoder::new(buf);
        for (i, op) in program.iter().enumerate() {
            match op {
                Op::Arith { fl, fh, ft } => {
                    let fs = dec.decode(*ft);
                    assert!(
                        (*fl..*fh).contains(&fs),
                        "op {i}: decode({ft}) = {fs} not in [{fl}, {fh})"
                    );
                    dec.dec_update(*fl, *fh, *ft);
                }
                Op::Icdf { s, icdf, ftb } => {
                    let got = dec.decode_icdf(icdf, *ftb) as usize;
                    assert_eq!(got, *s, "op {i}: icdf symbol mismatch");
                }
                Op::BitLogp { bit, logp } => {
                    assert_eq!(
                        dec.decode_bit_logp(*logp),
                        *bit,
                        "op {i}: bit_logp mismatch"
                    );
                }
                Op::Bits { val, bits } => {
                    assert_eq!(dec.dec_bits(*bits), *val, "op {i}: raw bits mismatch");
                }
                Op::Uint { val, ft } => {
                    assert_eq!(dec.dec_uint(*ft), *val, "op {i}: uint mismatch");
                }
            }
        }
        assert!(
            !dec.error(),
            "decoder flagged an error on a well-formed stream"
        );
        assert_eq!(dec.tell(), enc_tell, "tell() disagreed with the encoder");
        assert_eq!(
            dec.tell_frac(),
            enc_tell_frac,
            "tell_frac() disagreed with the encoder"
        );
    }

    // ================================================================
    // Round-trip tests
    // ================================================================

    #[test]
    fn round_trip_many_seeds_generous_buffer() {
        for seed in 0..300u64 {
            let mut rng =
                XorShift::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x1234_5678_9ABC_DEF0);
            let len = 1 + (seed as usize % 80);
            let program = gen_program(&mut rng, len);
            let (buf, tell, tell_frac) = encode_auto(&program);
            decode_and_check(&program, &buf, tell, tell_frac);
        }
    }

    #[test]
    fn round_trip_many_seeds_minimal_buffer_meets_in_the_middle() {
        for seed in 0..150u64 {
            let mut rng = XorShift::new(seed.wrapping_mul(0xD1B5_4A32_D192_ED03) ^ 0x00C0_FFEE);
            let len = 4 + (seed as usize % 50);
            let program = gen_program(&mut rng, len);
            let (buf, tell, tell_frac) = encode_min(&program);
            decode_and_check(&program, &buf, tell, tell_frac);
        }
    }

    #[test]
    fn front_arith_and_back_raw_bits_coexist_on_a_tight_buffer() {
        // Brief Step 1: deliberately interleave arithmetic-coded symbols with
        // backward raw bits, packed into the tightest possible buffer, so the
        // forward and backward cursors approach byte-adjacency. If the two
        // readers corrupted each other this is where it would show.
        let mut rng = XorShift::new(0xBADC_0DE5);
        let mut program = Vec::new();
        for i in 0..64u32 {
            // Arithmetic symbol.
            let ft = 251;
            let fl = (i.wrapping_mul(37)) % ft;
            program.push(Op::Arith { fl, fh: fl + 1, ft });
            // Raw bits of assorted widths.
            let bits = 1 + (i % 24);
            let val = rng.below(1u32 << bits);
            program.push(Op::Bits { val, bits });
        }
        let (buf, tell, tell_frac) = encode_min(&program);
        decode_and_check(&program, &buf, tell, tell_frac);
    }

    #[test]
    fn round_trip_each_op_type_in_isolation() {
        // Guards against one op type masking another in the mixed programs.
        let cases: Vec<Vec<Op>> = vec![
            vec![Op::Arith {
                fl: 3,
                fh: 9,
                ft: 32,
            }],
            vec![Op::Icdf {
                s: 2,
                icdf: vec![200, 130, 60, 0],
                ftb: 8,
            }],
            vec![Op::BitLogp { bit: 1, logp: 4 }],
            vec![Op::BitLogp { bit: 0, logp: 1 }],
            vec![Op::Bits {
                val: 0x0AB_CDE,
                bits: 24,
            }],
            vec![Op::Uint {
                val: 1_000_000,
                ft: 1_048_576,
            }],
            vec![Op::Uint { val: 5, ft: 7 }],
        ];
        for program in &cases {
            let (buf, tell, tell_frac) = encode_auto(program);
            decode_and_check(program, &buf, tell, tell_frac);
        }
    }

    // ================================================================
    // Hand-computed vectors (fully independent of the test encoder).
    //
    // These pin the front reader (init + renormalization) against values
    // derived by hand from the libopus algorithm, so an encoder/decoder pair
    // that shared a compensating bug would still have to match *these* fixed
    // numbers to pass.
    // ================================================================

    #[test]
    fn init_and_normalize_on_all_zero_input_matches_hand_computation() {
        // buf = [0,0,0,0]: rem=0 -> val = 127; then normalize runs 3 times
        // (rng: 128 -> 2^15 -> 2^23 -> 2^31), feeding sym=0 each time so
        //   val: 127 -> 0x7FFF -> 0x7FFFFF -> 0x7FFFFFFF.
        let dec = RangeDecoder::new(&[0, 0, 0, 0]);
        assert_eq!(dec.rng, 0x8000_0000);
        assert_eq!(dec.val, 0x7FFF_FFFF);
        assert_eq!(dec.offs, 4);
        assert_eq!(dec.nbits_total, 33);
        assert_eq!(dec.tell(), 1); // 33 - ilog(0x80000000)=33-32=1.
    }

    #[test]
    fn empty_buffer_reads_as_zeros_without_panicking() {
        // No bytes at all: every read yields 0, identical to the all-zero case.
        let dec = RangeDecoder::new(&[]);
        assert_eq!(dec.rng, 0x8000_0000);
        assert_eq!(dec.val, 0x7FFF_FFFF);
    }

    #[test]
    fn hand_computed_decode_step_on_all_zero_input() {
        // From the state above (val=0x7FFFFFFF, rng=0x80000000):
        //   decode(2): ext=0x40000000, s=val/ext=1, return 2-min(2,2)=0.
        //   dec_update(0,1,2): s=ext*(2-1)=0x40000000,
        //     val=0x7FFFFFFF-0x40000000=0x3FFFFFFF, rng=0x80000000-s=0x40000000.
        let mut dec = RangeDecoder::new(&[0, 0, 0, 0]);
        assert_eq!(dec.decode(2), 0);
        dec.dec_update(0, 1, 2);
        assert_eq!(dec.val, 0x3FFF_FFFF);
        assert_eq!(dec.rng, 0x4000_0000);
        assert_eq!(dec.tell(), 2); // 33 - ilog(0x40000000)=33-31=2.
    }

    #[test]
    fn ec_ilog_edge_values() {
        assert_eq!(ec_ilog(0), 0);
        assert_eq!(ec_ilog(1), 1);
        assert_eq!(ec_ilog(2), 2);
        assert_eq!(ec_ilog(0x7FFF_FFFF), 31);
        assert_eq!(ec_ilog(0x8000_0000), 32);
        assert_eq!(ec_ilog(u32::MAX), 32);
    }

    #[test]
    fn dec_bits_zero_is_a_noop() {
        let mut dec = RangeDecoder::new(&[0xAB, 0xCD, 0xEF, 0x12]);
        let before = dec.tell();
        assert_eq!(dec.dec_bits(0), 0);
        assert_eq!(dec.tell(), before);
    }
}
