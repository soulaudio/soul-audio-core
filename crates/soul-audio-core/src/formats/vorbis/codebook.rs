//! Vorbis setup-header **codebooks** (spec §3, §3.2.1): the canonical-Huffman
//! entropy books plus their optional vector-quantization (VQ) lookup tables.
//!
//! A codebook maps bitstream codewords to entry indices ([`Codebook::decode_scalar`])
//! and, for VQ books, each entry index to a `dims`-element float vector
//! ([`Codebook::lookup_vector`]). Everything variable-sized — the per-entry
//! codeword lengths, the assigned codewords, and the multiplicand list — lives
//! in the caller's [`Arena`], addressed by byte **offsets** (never references),
//! so [`Codebook`] itself is small, `Copy`, and self-reference-free (see the
//! [`arena`](super::arena) module docs).
//!
//! # The three things that MUST be exactly right (classic Vorbis bug sources)
//!
//! 1. **Canonical Huffman assignment (§3.2.1).** Each *used* entry, in entry
//!    order, receives the lowest-valued unused binary codeword of its declared
//!    length. Implemented by [`assign_codewords`] via the standard
//!    "available prefix-slot" method (one live free codeword per bit-length),
//!    which reproduces the spec's own worked example bit-for-bit (see tests).
//! 2. **Tree completeness.** After assignment the prefix tree must be *exactly
//!    full* — an over-specified tree (no free slot for a codeword) and an
//!    under-specified tree (a free slot left over) are both hard
//!    [`DecodeError::Malformed`] errors, with the one documented exception: a
//!    codebook with a single used entry (the pseudo-nil book) is accepted.
//! 3. **Compact lattice VQ (lookup type 1).** Only the multiplicand list is
//!    stored; each output vector element is reconstructed on the fly in
//!    [`Codebook::lookup_vector`] (the `index_divisor` powers + modulo +
//!    optional `sequence_p` running sum). The vectors are *never* pre-expanded
//!    — that pre-expansion is the dominant RAM cost this codec avoids.

use crate::decoder::DecodeError;

use super::arena::Arena;
use super::bitread::{BitReader, float32_unpack, ilog};

/// The 24-bit codebook sync pattern that opens every packed codebook (spec
/// §3.2.1: bytes `0x42 0x43 0x56`, i.e. the LSb-first 24-bit value `0x564342`).
pub const CODEBOOK_SYNC: u32 = 0x0056_4342;

/// The widest codeword a Vorbis codebook may declare. The length field is
/// `[read 5 bits] + 1`, so a *stored* length maxes at 32; a 32-bit codeword is
/// the deepest a 32-bit prefix-slot accumulator can represent. Anything the
/// bitstream implies beyond this (an over-long `ordered` run) is
/// [`DecodeError::Malformed`].
const MAX_CODEWORD_LEN: u32 = 32;

/// A parsed codebook: small, `Copy`, and free of self-references — the
/// variable-length tables live in the [`Arena`] at the recorded byte offsets.
///
/// Read paths ([`Self::decode_scalar`], [`Self::lookup_vector`]) take the
/// arena's backing bytes as a plain `&[u8]` rather than an `&Arena`: reads are
/// pure indexing, and the real decoder must be able to read from its owned
/// storage (`self.storage.as_ref()`) without constructing a mutable-borrowing
/// [`Arena`] view over `&self` (which would not borrow-check). Offsets are
/// relative to the start of the slice the tables were allocated in.
#[derive(Clone, Copy, Debug)]
pub struct Codebook {
    /// VQ vector dimensionality (`codebook_dimensions`, spec §3.2.1). ≥ 1.
    pub dims: u16,
    /// Number of codebook entries (`codebook_entries`). ≥ 1.
    pub entries: u32,
    /// VQ lookup kind: `0` = scalar/no lookup, `1` = lattice VQ (compact),
    /// `2` = explicit per-scalar VQ.
    pub lookup_type: u8,
    /// Longest assigned codeword, in bits (`0` iff no entry is used) — the read
    /// bound for [`Self::decode_scalar`].
    max_length: u8,
    /// `codebook_sequence_p`: whether VQ reconstruction carries a running sum.
    sequence_p: bool,
    /// Offset of the per-entry codeword-length table (`u8` × `entries`;
    /// `0` = unused entry).
    lengths_off: u32,
    /// Offset of the per-entry assigned-codeword table (`u32` × `entries`,
    /// left-justified: codeword bit 0 at value bit 31).
    codewords_off: u32,
    /// Offset of the multiplicand table (`u16` × [`Self::lookup_values`]);
    /// unused for `lookup_type == 0`.
    multiplicands_off: u32,
    /// Length of the multiplicand table: `lookup1_values(entries, dims)` for
    /// type 1, `entries * dims` for type 2.
    lookup_values: u32,
    /// `codebook_minimum_value` (already `float32_unpack`-ed).
    minimum_value: f32,
    /// `codebook_delta_value` (already `float32_unpack`-ed).
    delta_value: f32,
}

impl Codebook {
    /// The zero value, for initializing fixed codebook arrays before parse
    /// fills the live prefix. Not a decodable book (`max_length == 0`).
    pub const EMPTY: Codebook = Codebook {
        dims: 0,
        entries: 0,
        lookup_type: 0,
        max_length: 0,
        sequence_p: false,
        lengths_off: 0,
        codewords_off: 0,
        multiplicands_off: 0,
        lookup_values: 0,
        minimum_value: 0.0,
        delta_value: 0.0,
    };

    /// Parse one packed codebook (spec §3.2.1) from `br`, laying its tables
    /// into `arena` and returning the small offset-bearing descriptor.
    ///
    /// # Errors
    /// [`DecodeError::Malformed`] for a bad sync pattern, zero
    /// dimensions/entries, an over-long ordered run, an over- or
    /// under-specified Huffman tree, or a reserved (`> 2`) lookup type.
    /// [`DecodeError::Unsupported`] if the arena cannot fit the tables.
    /// [`DecodeError::UnexpectedEof`] if the packet ends mid-codebook.
    pub fn parse(br: &mut BitReader, arena: &mut Arena) -> Result<Codebook, DecodeError> {
        if br.read_bits(24)? != CODEBOOK_SYNC {
            return Err(DecodeError::Malformed);
        }
        let dims = br.read_bits(16)? as u16;
        let entries = br.read_bits(24)?;
        if dims == 0 || entries == 0 {
            return Err(DecodeError::Malformed);
        }
        let entries_usize = entries as usize;

        // --- codeword length list (spec §3.2.1) -------------------------
        // Laid into the arena as one byte per entry (0 == unused).
        let lengths_off = arena.alloc(entries_usize, 1)?;
        if br.read_bit()? {
            // ordered: ascending runs of a rising current_length. Each run's
            // length is read as ilog(entries - current_entry) bits.
            let mut current_entry: u32 = 0;
            let mut current_length: u32 = br.read_bits(5)? + 1;
            while current_entry < entries {
                if current_length > MAX_CODEWORD_LEN {
                    return Err(DecodeError::Malformed);
                }
                let number = br.read_bits(ilog(entries - current_entry))?;
                let end = current_entry
                    .checked_add(number)
                    .ok_or(DecodeError::Malformed)?;
                if end > entries {
                    return Err(DecodeError::Malformed);
                }
                let buf = arena.bytes_mut();
                for i in current_entry..end {
                    buf[lengths_off + i as usize] = current_length as u8;
                }
                current_entry = end;
                current_length += 1;
            }
        } else {
            // unordered: optionally sparse (a per-entry present flag).
            let sparse = br.read_bit()?;
            for i in 0..entries_usize {
                let length = if sparse && !br.read_bit()? {
                    0 // absent => unused entry
                } else {
                    br.read_bits(5)? + 1
                };
                if length > MAX_CODEWORD_LEN {
                    return Err(DecodeError::Malformed);
                }
                arena.bytes_mut()[lengths_off + i] = length as u8;
            }
        }

        // --- canonical Huffman assignment + tree-completeness check ------
        let codewords_off = arena.alloc(
            entries_usize
                .checked_mul(4)
                .ok_or(DecodeError::Unsupported)?,
            4,
        )?;
        let (max_length, _used) = assign_codewords(arena, lengths_off, codewords_off, entries)?;

        // --- VQ lookup table (spec §3.2.1) ------------------------------
        let lookup_type = br.read_bits(4)? as u8;
        let mut minimum_value = 0.0f32;
        let mut delta_value = 0.0f32;
        let mut sequence_p = false;
        let mut lookup_values = 0u32;
        let mut multiplicands_off = 0u32;
        if lookup_type == 1 || lookup_type == 2 {
            minimum_value = float32_unpack(br.read_bits(32)?);
            delta_value = float32_unpack(br.read_bits(32)?);
            // value_bits field is 4 bits + 1 => 1..=16, always <= 32.
            let value_bits = br.read_bits(4)? + 1;
            sequence_p = br.read_bit()?;
            lookup_values = if lookup_type == 1 {
                lookup1_values(entries, dims)
            } else {
                entries
                    .checked_mul(u32::from(dims))
                    .ok_or(DecodeError::Unsupported)?
            };
            let mult_bytes = (lookup_values as usize)
                .checked_mul(2)
                .ok_or(DecodeError::Unsupported)?;
            let off = arena.alloc(mult_bytes, 2)?;
            let buf = arena.bytes_mut();
            for i in 0..lookup_values as usize {
                let v = br.read_bits(value_bits)? as u16; // value_bits <= 16
                buf[off + i * 2..off + i * 2 + 2].copy_from_slice(&v.to_le_bytes());
            }
            multiplicands_off = off as u32;
        } else if lookup_type != 0 {
            // A lookup type > 2 is reserved and renders the stream undecodable.
            return Err(DecodeError::Malformed);
        }

        Ok(Codebook {
            dims,
            entries,
            lookup_type,
            max_length,
            sequence_p,
            lengths_off: lengths_off as u32,
            codewords_off: codewords_off as u32,
            multiplicands_off,
            lookup_values,
            minimum_value,
            delta_value,
        })
    }

    /// Huffman-decode one entry index from `br`. `storage` is the arena bytes
    /// the tables were allocated in (offsets are relative to its start).
    ///
    /// # Errors
    /// [`DecodeError::UnexpectedEof`] if the bitstream ends before a codeword
    /// completes. [`DecodeError::Malformed`] if the bits read reach the
    /// codebook's maximum codeword length without matching any codeword (a
    /// corrupt stream), or the codebook has no usable entries.
    pub fn decode_scalar(&self, br: &mut BitReader, storage: &[u8]) -> Result<u32, DecodeError> {
        let max = u32::from(self.max_length);
        if max == 0 {
            // A book with no used entries can decode nothing.
            return Err(DecodeError::Malformed);
        }
        let lengths_off = self.lengths_off as usize;
        let codewords_off = self.codewords_off as usize;
        let entries = self.entries as usize;
        // `acc` accumulates the codeword left-justified: the first bit read is
        // value bit 31 (the tree root), matching how `assign_codewords` stores
        // each codeword. After `nbits` reads, a length-`nbits` codeword equal
        // to `acc` is the (unique, prefix-free) match.
        let mut acc: u32 = 0;
        let mut nbits: u32 = 0;
        loop {
            if br.read_bit()? {
                acc |= 1u32 << (31 - nbits);
            }
            nbits += 1;
            for i in 0..entries {
                if u32::from(storage[lengths_off + i]) == nbits {
                    let cw = u32::from_le_bytes(
                        storage[codewords_off + i * 4..codewords_off + i * 4 + 4]
                            .try_into()
                            .unwrap(),
                    );
                    if cw == acc {
                        return Ok(i as u32);
                    }
                }
            }
            if nbits >= max {
                // Ran to the deepest codeword with no match: corrupt stream.
                return Err(DecodeError::Malformed);
            }
        }
    }

    /// Reconstruct the VQ vector for `entry` into `out` (`out.len()` should be
    /// `dims`; extra elements are left untouched, missing ones are skipped so
    /// this never panics). Compact for type 1 (multiplicands + on-the-fly
    /// index math), explicit for type 2; type 0 yields no vector (all zeros).
    pub fn lookup_vector(&self, entry: u32, storage: &[u8], out: &mut [f32]) {
        if entry >= self.entries {
            return;
        }
        let dims = usize::from(self.dims).min(out.len());
        let mult_base = self.multiplicands_off as usize;
        let read_mult = |idx: usize| -> f32 {
            let o = mult_base + idx * 2;
            f32::from(u16::from_le_bytes(storage[o..o + 2].try_into().unwrap()))
        };
        match self.lookup_type {
            1 => {
                // Compact lattice VQ: element `i` reads multiplicand
                // (entry / lookup_values^i) mod lookup_values — computed on
                // the fly, never pre-expanded. `index_divisor` is u64 with a
                // saturating step so its powers cannot wrap to 0 (which would
                // divide-by-zero) or panic on overflow.
                let lv = u64::from(self.lookup_values);
                if lv == 0 {
                    return;
                }
                let mut last = 0.0f32;
                let mut index_divisor: u64 = 1;
                for slot in out.iter_mut().take(dims) {
                    let mult_off = ((u64::from(entry) / index_divisor) % lv) as usize;
                    let v = read_mult(mult_off) * self.delta_value + self.minimum_value + last;
                    *slot = v;
                    if self.sequence_p {
                        last = v;
                    }
                    index_divisor = index_divisor.saturating_mul(lv);
                }
            }
            2 => {
                // Explicit VQ: dims consecutive multiplicands per entry.
                let mut last = 0.0f32;
                let mut mult_off = entry as usize * usize::from(self.dims);
                for slot in out.iter_mut().take(dims) {
                    let v = read_mult(mult_off) * self.delta_value + self.minimum_value + last;
                    *slot = v;
                    if self.sequence_p {
                        last = v;
                    }
                    mult_off += 1;
                }
            }
            _ => {
                // Type 0: no vector — the entry index is the whole result.
                for slot in out.iter_mut().take(dims) {
                    *slot = 0.0;
                }
            }
        }
    }
}

/// `lookup1_values(entries, dims)` (spec §3.2.1): the largest integer `m` such
/// that `m^dims <= entries` — the side length of the implicit lattice a type-1
/// codebook's multiplicands index. Computed by integer binary search over `m`
/// with a saturating power, so it needs no transcendentals and cannot overflow.
fn lookup1_values(entries: u32, dims: u16) -> u32 {
    if dims == 0 {
        return 0;
    }
    // pow_le(base): does base^dims fit within `entries` (saturating so it
    // never overflows u64 for the realistic 24-bit entry / 16-bit dim ranges)?
    let pow_le = |base: u32| -> bool {
        let limit = u64::from(entries);
        let mut p: u64 = 1;
        for _ in 0..dims {
            p = p.saturating_mul(u64::from(base));
            if p > limit {
                return false;
            }
        }
        true
    };
    // Largest m with m^dims <= entries. For dims >= 1, m <= entries.
    let mut lo: u32 = 0;
    let mut hi: u32 = entries;
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if pow_le(mid) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

/// Assign canonical Huffman codewords to every used entry and validate tree
/// completeness. Returns `(max_length, used_entries)`. See the module docs.
fn assign_codewords(
    arena: &mut Arena,
    lengths_off: usize,
    codewords_off: usize,
    entries: u32,
) -> Result<(u8, u32), DecodeError> {
    // `available[len]` holds the single lowest free prefix-slot of that bit
    // length as a left-justified codeword (`0` == none). Codewords are stored
    // left-justified (first branch = value bit 31) so "lowest-valued codeword"
    // is "numerically smallest u32", and a slot value is never `0` unless
    // empty (every real slot has a set bit from the branch that created it).
    let mut available = [0u32; 33];
    let mut first = true;
    let mut max_length: u32 = 0;
    let mut used: u32 = 0;
    let buf = arena.bytes_mut();
    for i in 0..entries as usize {
        let len = u32::from(buf[lengths_off + i]);
        let codeword: u32 = if len == 0 {
            0 // unused entry — codeword irrelevant
        } else if len > MAX_CODEWORD_LEN {
            return Err(DecodeError::Malformed);
        } else {
            used += 1;
            if len > max_length {
                max_length = len;
            }
            if first {
                first = false;
                // First used entry takes the all-zeros codeword; every prefix
                // above it frees the sibling '1'-branch slot at that depth.
                for l in 1..=len {
                    available[l as usize] = 1u32 << (32 - l);
                }
                0
            } else {
                // Lowest free slot at length `len`: descend to the deepest
                // available shorter (or equal) length.
                let mut z = len;
                while z > 0 && available[z as usize] == 0 {
                    z -= 1;
                }
                if z == 0 {
                    // No slot: the tree is over-specified.
                    return Err(DecodeError::Malformed);
                }
                let res = available[z as usize];
                available[z as usize] = 0;
                // Splitting a shorter slot down to `len` frees a new sibling at
                // each intermediate depth.
                if z != len {
                    for y in (z + 1)..=len {
                        available[y as usize] = res + (1u32 << (32 - y));
                    }
                }
                res
            }
        };
        let off = codewords_off + i * 4;
        buf[off..off + 4].copy_from_slice(&codeword.to_le_bytes());
    }

    // Tree completeness: a fully specified tree leaves no free slot. The lone
    // exception is a single used entry (the spec's pseudo-nil codebook).
    if used != 1 {
        for slot in &available[1..=32] {
            if *slot != 0 {
                // Under-specified: the tree is not full.
                return Err(DecodeError::Malformed);
            }
        }
    }
    Ok((max_length as u8, used))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- LSb-first bit writer (test-only), the exact inverse of BitReader ---

    /// Packs values LSb-first, byte-for-byte the way [`BitReader`] reads them:
    /// `write(v, n)` emits `v`'s low `n` bits, bit 0 first. Building codebook
    /// packets by hand this way keeps the tests readable and, crucially,
    /// independently derived from the spec's field layout.
    struct BitWriter {
        bytes: Vec<u8>,
        bit: usize,
    }
    impl BitWriter {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                bit: 0,
            }
        }
        fn write(&mut self, value: u32, n: u32) {
            for i in 0..n {
                let byte_idx = self.bit >> 3;
                if byte_idx >= self.bytes.len() {
                    self.bytes.push(0);
                }
                if (value >> i) & 1 != 0 {
                    self.bytes[byte_idx] |= 1 << (self.bit & 7);
                }
                self.bit += 1;
            }
        }
        fn into_bytes(self) -> Vec<u8> {
            self.bytes
        }
    }

    /// `float32_unpack`-encoding of exactly `1.0`: mantissa 1 at the bias
    /// exponent (788) — `2^(788-788) == 1`. Cross-checked by the bitread
    /// module's own `float32_unpack` tests.
    const FLOAT_ONE: u32 = (788u32 << 21) | 1;

    /// Open the codebook header fields common to every fixture: sync + dims +
    /// entries.
    fn write_header(w: &mut BitWriter, dims: u16, entries: u32) {
        w.write(CODEBOOK_SYNC, 24);
        w.write(u32::from(dims), 16);
        w.write(entries, 24);
    }

    /// Parse `bytes` as a codebook into a fresh 8 KiB arena, returning the
    /// descriptor and the arena's owned storage so the caller can drive the
    /// read paths against `storage`.
    fn parse(bytes: &[u8]) -> Result<(Codebook, Vec<u8>), DecodeError> {
        let mut storage = vec![0u8; 8192];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(bytes);
        let cb = Codebook::parse(&mut br, &mut arena)?;
        Ok((cb, storage))
    }

    /// Feed the exact bit sequence `bits` (each 0/1) to `decode_scalar`.
    fn decode(cb: &Codebook, storage: &[u8], bits: &[u8]) -> Result<u32, DecodeError> {
        let mut w = BitWriter::new();
        for &b in bits {
            w.write(u32::from(b), 1);
        }
        let packed = w.into_bytes();
        let mut br = BitReader::new(&packed);
        cb.decode_scalar(&mut br, storage)
    }

    // --- lookup1_values (spec §3.2.1) -----------------------------------

    #[test]
    fn lookup1_values_matches_worked_cases() {
        // Largest m with m^dims <= entries.
        assert_eq!(lookup1_values(4, 2), 2); // 2^2=4 <= 4 < 3^2=9
        assert_eq!(lookup1_values(3, 2), 1); // 1^2=1 <= 3 < 2^2=4
        assert_eq!(lookup1_values(1, 2), 1);
        assert_eq!(lookup1_values(8, 3), 2); // 2^3=8 <= 8 < 3^3=27
        assert_eq!(lookup1_values(26, 3), 2);
        assert_eq!(lookup1_values(27, 3), 3);
        assert_eq!(lookup1_values(100, 1), 100); // dim 1: m == entries
        assert_eq!(lookup1_values(1024, 2), 32); // 32^2 == 1024
    }

    // --- canonical Huffman: the spec's own §3.2.1 worked example ----------

    /// Lengths `[2,4,4,4,4,2,3,3]` — the exact list the spec walks through,
    /// whose canonical codewords are `00 0100 0101 0110 0111 10 110 111`.
    fn spec_example_bytes() -> Vec<u8> {
        let mut w = BitWriter::new();
        write_header(&mut w, 1, 8);
        w.write(0, 1); // ordered = 0
        w.write(0, 1); // sparse  = 0
        for len in [2u32, 4, 4, 4, 4, 2, 3, 3] {
            w.write(len - 1, 5); // stored length is (length - 1)
        }
        w.write(0, 4); // lookup_type = 0
        w.into_bytes()
    }

    #[test]
    fn decodes_the_spec_worked_example_codewords() {
        let (cb, storage) = parse(&spec_example_bytes()).expect("valid codebook");
        assert_eq!(cb.entries, 8);
        // Each expected codeword, MSb-first, must decode to its entry index.
        assert_eq!(decode(&cb, &storage, &[0, 0]).unwrap(), 0); // 00
        assert_eq!(decode(&cb, &storage, &[0, 1, 0, 0]).unwrap(), 1); // 0100
        assert_eq!(decode(&cb, &storage, &[0, 1, 0, 1]).unwrap(), 2); // 0101
        assert_eq!(decode(&cb, &storage, &[0, 1, 1, 0]).unwrap(), 3); // 0110
        assert_eq!(decode(&cb, &storage, &[0, 1, 1, 1]).unwrap(), 4); // 0111
        assert_eq!(decode(&cb, &storage, &[1, 0]).unwrap(), 5); // 10
        assert_eq!(decode(&cb, &storage, &[1, 1, 0]).unwrap(), 6); // 110
        assert_eq!(decode(&cb, &storage, &[1, 1, 1]).unwrap(), 7); // 111
    }

    #[test]
    fn decode_on_an_empty_bitstream_is_unexpected_eof() {
        let (cb, storage) = parse(&spec_example_bytes()).unwrap();
        let mut br = BitReader::new(&[]);
        assert_eq!(
            cb.decode_scalar(&mut br, &storage).unwrap_err(),
            DecodeError::UnexpectedEof
        );
    }

    // --- ordered length decode -------------------------------------------

    #[test]
    fn ordered_lengths_decode_and_round_trip() {
        // entries=4, ascending lengths [1,2,3,3]: runs of (1,1,2) at lengths
        // (1,2,3). Run counts are read as ilog(entries-current_entry) bits:
        // ilog(4)=3, ilog(3)=2, ilog(2)=2.
        let mut w = BitWriter::new();
        write_header(&mut w, 1, 4);
        w.write(1, 1); // ordered = 1
        w.write(0, 5); // first current_length - 1 == 0  => length 1
        w.write(1, 3); // number = 1 at length 1  (ilog(4)=3 bits)
        w.write(1, 2); // number = 1 at length 2  (ilog(3)=2 bits)
        w.write(2, 2); // number = 2 at length 3  (ilog(2)=2 bits)
        w.write(0, 4); // lookup_type = 0
        let (cb, storage) = parse(&w.into_bytes()).expect("valid ordered codebook");
        assert_eq!(cb.entries, 4);
        assert_eq!(decode(&cb, &storage, &[0]).unwrap(), 0); // 0
        assert_eq!(decode(&cb, &storage, &[1, 0]).unwrap(), 1); // 10
        assert_eq!(decode(&cb, &storage, &[1, 1, 0]).unwrap(), 2); // 110
        assert_eq!(decode(&cb, &storage, &[1, 1, 1]).unwrap(), 3); // 111
    }

    // --- sparse length decode --------------------------------------------

    #[test]
    fn sparse_lengths_skip_absent_entries() {
        // entries=4, sparse: entries 0 and 2 used (length 1 each), 1 and 3
        // absent. The two used entries form a full 1-bit tree (0 and 1).
        let mut w = BitWriter::new();
        write_header(&mut w, 1, 4);
        w.write(0, 1); // ordered = 0
        w.write(1, 1); // sparse  = 1
        w.write(1, 1); // entry 0 present
        w.write(0, 5); // length 1
        w.write(0, 1); // entry 1 absent
        w.write(1, 1); // entry 2 present
        w.write(0, 5); // length 1
        w.write(0, 1); // entry 3 absent
        w.write(0, 4); // lookup_type = 0
        let (cb, storage) = parse(&w.into_bytes()).expect("valid sparse codebook");
        assert_eq!(decode(&cb, &storage, &[0]).unwrap(), 0);
        assert_eq!(decode(&cb, &storage, &[1]).unwrap(), 2);
    }

    // --- tree completeness (the cascade-critical checks) ------------------

    #[test]
    fn under_specified_tree_is_malformed() {
        // Two length-2 codewords (00, 01) leave 10 and 11 dangling: the tree
        // is not full.
        let mut w = BitWriter::new();
        write_header(&mut w, 1, 2);
        w.write(0, 1); // ordered = 0
        w.write(0, 1); // sparse  = 0
        w.write(1, 5); // length 2
        w.write(1, 5); // length 2
        w.write(0, 4); // lookup_type = 0
        assert_eq!(parse(&w.into_bytes()).unwrap_err(), DecodeError::Malformed);
    }

    #[test]
    fn over_specified_tree_is_malformed() {
        // Three length-1 codewords cannot coexist: after 0 and 1 the 1-bit
        // space is full, so the third entry has no slot.
        let mut w = BitWriter::new();
        write_header(&mut w, 1, 3);
        w.write(0, 1); // ordered = 0
        w.write(0, 1); // sparse  = 0
        w.write(0, 5); // length 1
        w.write(0, 5); // length 1
        w.write(0, 5); // length 1
        w.write(0, 4); // lookup_type = 0
        assert_eq!(parse(&w.into_bytes()).unwrap_err(), DecodeError::Malformed);
    }

    #[test]
    fn single_used_entry_is_the_accepted_special_case() {
        // One entry of length 1: not a "full" tree, but the spec's documented
        // pseudo-nil exception — must parse, and decode its lone codeword.
        let mut w = BitWriter::new();
        write_header(&mut w, 1, 1);
        w.write(0, 1); // ordered = 0
        w.write(0, 1); // sparse  = 0
        w.write(0, 5); // length 1
        w.write(0, 4); // lookup_type = 0
        let (cb, storage) = parse(&w.into_bytes()).expect("single-entry book is valid");
        assert_eq!(decode(&cb, &storage, &[0]).unwrap(), 0);
        // The lone codeword is "0"; a "1" bit matches nothing and, being a
        // non-full (pseudo-nil) tree, hits max_length with no match => the
        // only case where decode reports Malformed on a syntactically valid
        // book.
        assert_eq!(
            decode(&cb, &storage, &[1]).unwrap_err(),
            DecodeError::Malformed
        );
    }

    // --- VQ lookup type 1 (compact lattice) ------------------------------

    #[test]
    fn lookup_type1_reconstructs_lattice_vectors() {
        // entries=4, dims=2, full length-2 tree, lattice VQ. lookup1_values =
        // 2, so multiplicands index a 2x2 lattice with multiplicands [1, 2],
        // delta 1.0, minimum 0.0, sequence_p off.
        let mut w = BitWriter::new();
        write_header(&mut w, 2, 4);
        w.write(0, 1); // ordered = 0
        w.write(0, 1); // sparse  = 0
        for _ in 0..4 {
            w.write(1, 5); // length 2 each => full tree
        }
        w.write(1, 4); // lookup_type = 1
        w.write(0, 32); // minimum_value = float32_unpack(0) = 0.0
        w.write(FLOAT_ONE, 32); // delta_value = 1.0
        w.write(3, 4); // value_bits - 1 == 3  => 4-bit multiplicands
        w.write(0, 1); // sequence_p = 0
        w.write(1, 4); // multiplicand[0] = 1
        w.write(2, 4); // multiplicand[1] = 2
        let (cb, storage) = parse(&w.into_bytes()).expect("valid lattice VQ codebook");
        assert_eq!(cb.lookup_type, 1);
        assert_eq!(cb.dims, 2);

        let expect = [[1.0f32, 1.0], [2.0, 1.0], [1.0, 2.0], [2.0, 2.0]];
        for (e, want) in expect.iter().enumerate() {
            let mut out = [0.0f32; 2];
            cb.lookup_vector(e as u32, &storage, &mut out);
            assert_eq!(&out, want, "vector for entry {e}");
        }
    }

    #[test]
    fn lookup_type1_sequence_p_accumulates() {
        // Same lattice but sequence_p on: each element adds the previous one.
        // entry 3 -> raw [2,2] -> running sum [2, 4].
        let mut w = BitWriter::new();
        write_header(&mut w, 2, 4);
        w.write(0, 1);
        w.write(0, 1);
        for _ in 0..4 {
            w.write(1, 5);
        }
        w.write(1, 4); // lookup_type = 1
        w.write(0, 32); // minimum 0
        w.write(FLOAT_ONE, 32); // delta 1
        w.write(3, 4); // 4-bit multiplicands
        w.write(1, 1); // sequence_p = 1
        w.write(1, 4);
        w.write(2, 4);
        let (cb, storage) = parse(&w.into_bytes()).expect("valid");
        let mut out = [0.0f32; 2];
        cb.lookup_vector(3, &storage, &mut out);
        assert_eq!(out, [2.0, 4.0]);
    }

    // --- header robustness ----------------------------------------------

    #[test]
    fn bad_sync_pattern_is_malformed() {
        let mut w = BitWriter::new();
        w.write(0x0012_3456, 24); // wrong sync
        w.write(1, 16);
        w.write(2, 24);
        assert_eq!(parse(&w.into_bytes()).unwrap_err(), DecodeError::Malformed);
    }

    #[test]
    fn reserved_lookup_type_is_malformed() {
        let mut w = BitWriter::new();
        write_header(&mut w, 1, 2);
        w.write(0, 1);
        w.write(0, 1);
        w.write(0, 5); // length 1
        w.write(0, 5); // length 1
        w.write(3, 4); // lookup_type = 3 (reserved / undecodable)
        assert_eq!(parse(&w.into_bytes()).unwrap_err(), DecodeError::Malformed);
    }

    #[test]
    fn arena_too_small_is_unsupported() {
        // A tiny arena cannot hold even the length/codeword tables.
        let bytes = spec_example_bytes();
        let mut storage = [0u8; 4];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        assert_eq!(
            Codebook::parse(&mut br, &mut arena).unwrap_err(),
            DecodeError::Unsupported
        );
    }
}
