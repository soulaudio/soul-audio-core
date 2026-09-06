//! Vorbis **residue** setup (spec §8.6.1) — the entropy-coded "everything the
//! floor curve didn't capture" component, decoded in partitions/classes/
//! stages. This module only lands the setup-time **configuration**
//! (`residue_type`/`begin`/`end`/`partition_size`/`classifications`/
//! `classbook`, the per-classification `cascade` bitmap, and the per-stage
//! codebook references); the actual per-packet residue decode is Task 6.
//!
//! # Field layout (spec §8.6.1, cross-checked against lewton's `header.rs`
//! and independently round-tripped against a real ffmpeg/libvorbis setup
//! header — see the Task 5 report)
//!
//! ```text
//! residue_type       = read 16 bits              (validate <= 2)
//! residue_begin      = read 24 bits
//! residue_end        = read 24 bits               (validate begin <= end)
//! residue_partition_size = read 24 bits + 1
//! residue_classifications = read 6 bits + 1       (1..=64; validate <= MAX_RESIDUE_CLASSIFICATIONS)
//! residue_classbook  = read 8 bits                (validate < codebook_count)
//! for i in 0..classifications:
//!   low_bits  = read 3 bits
//!   bitflag   = read 1 bit
//!   high_bits = bitflag ? read 5 bits : 0
//!   cascade[i] = (high_bits << 3) | low_bits      (an 8-bit "which stages are active" bitmap)
//! for i in 0..classifications:
//!   for stage in 0..8:
//!     if cascade[i] has bit `stage` set:
//!       books[i][stage] = read 8 bits             (validate < codebook_count)
//!     // else: no book for that (classification, stage) — never decoded
//! ```

use crate::decoder::DecodeError;

use super::arena::Arena;
use super::bitread::BitReader;
use super::codebook::Codebook;
use super::{MAX_CHANNELS, MAX_N};

/// Heuristic cap on `residue_classifications` (spec ceiling, from the 6-bit
/// `+1` field, is 64 — a real encoder never gets remotely close to even this
/// lower bound; a real ffmpeg/libvorbis stereo encode at a normal quality
/// uses 10). Picked well below the spec ceiling so a hand-built "over-cap"
/// setup packet can still declare a spec-legal value that exceeds it.
pub const MAX_RESIDUE_CLASSIFICATIONS: usize = 32;

/// The 8 possible decode "stages" a residue classification can cascade
/// through (spec: `cascade` is an 8-bit active-stage bitmap) — exact, not a
/// heuristic.
pub const MAX_RESIDUE_STAGES: usize = 8;

/// A parsed residue configuration (spec §8.6.1): small, `Copy`, and free of
/// self-references — the per-classification `cascade` and per-stage codebook
/// tables live in the [`Arena`] at the recorded byte offsets (see
/// [`Codebook`](super::codebook::Codebook)'s docs for why).
#[derive(Clone, Copy, Debug)]
pub struct ResidueConfig {
    /// `residue_type` (`0`, `1`, or `2`).
    pub residue_type: u8,
    /// `residue_begin` — first residue vector element in scope.
    pub begin: u32,
    /// `residue_end` — one past the last residue vector element in scope
    /// (`>= begin`).
    pub end: u32,
    /// `residue_partition_size` (`>= 1`).
    pub partition_size: u32,
    /// `residue_classifications` (`1..=MAX_RESIDUE_CLASSIFICATIONS`).
    pub classifications: u8,
    /// `residue_classbook` — codebook index used to classify each partition
    /// (validated `< codebook_count`).
    pub classbook: u8,
    /// Offset of the per-classification `cascade` bitmap table (`u8` ×
    /// [`Self::classifications`]).
    cascade_off: u32,
    /// Offset of the per-(classification, stage) book-index table (`i16` LE
    /// × [`Self::classifications`] × [`MAX_RESIDUE_STAGES`], stride-8;
    /// `-1` means "no book" — that stage is inactive for the classification).
    books_off: u32,
}

impl ResidueConfig {
    /// The zero value, for initializing fixed residue arrays before parse
    /// fills the live prefix (mirrors [`Codebook::EMPTY`](super::codebook::Codebook::EMPTY)).
    pub const EMPTY: ResidueConfig = ResidueConfig {
        residue_type: 0,
        begin: 0,
        end: 0,
        partition_size: 0,
        classifications: 0,
        classbook: 0,
        cascade_off: 0,
        books_off: 0,
    };

    /// Parse one residue configuration (spec §8.6.1), including its own
    /// leading `residue_type` field.
    ///
    /// # Errors
    /// [`DecodeError::Malformed`] for `residue_type > 2`, `begin > end`, or
    /// an out-of-range `classbook`/stage-book codebook cross-reference.
    /// [`DecodeError::Unsupported`] if `classifications` exceeds
    /// [`MAX_RESIDUE_CLASSIFICATIONS`] or the arena cannot fit the tables.
    /// [`DecodeError::UnexpectedEof`] if the packet ends mid-residue.
    pub fn parse(
        br: &mut BitReader,
        arena: &mut Arena,
        codebook_count: usize,
    ) -> Result<ResidueConfig, DecodeError> {
        let residue_type = br.read_bits(16)? as u8;
        if residue_type > 2 {
            return Err(DecodeError::Malformed);
        }
        let begin = br.read_bits(24)?;
        let end = br.read_bits(24)?;
        if begin > end {
            return Err(DecodeError::Malformed);
        }
        let partition_size = br.read_bits(24)? + 1;
        let classifications = br.read_bits(6)? + 1; // 1..=64
        if classifications as usize > MAX_RESIDUE_CLASSIFICATIONS {
            return Err(DecodeError::Unsupported);
        }
        let classbook = br.read_bits(8)? as u8;
        if classbook as usize >= codebook_count {
            return Err(DecodeError::Malformed);
        }

        let cascade_off = arena.alloc(classifications as usize, 1)? as u32;
        for i in 0..classifications as usize {
            let low = br.read_bits(3)?;
            let flag = br.read_bit()?;
            let high = if flag { br.read_bits(5)? } else { 0 };
            let cascade = ((high << 3) | low) as u8;
            arena.bytes_mut()[cascade_off as usize + i] = cascade;
        }

        let books_len = (classifications as usize)
            .checked_mul(MAX_RESIDUE_STAGES)
            .ok_or(DecodeError::Unsupported)?;
        let books_off =
            arena.alloc(books_len.checked_mul(2).ok_or(DecodeError::Unsupported)?, 2)? as u32;
        // Initialize every stage slot to "-1/no book" up front — only the
        // cascade-active stages below get overwritten with a real index.
        {
            let buf = arena.bytes_mut();
            for i in 0..books_len {
                let off = books_off as usize + i * 2;
                buf[off..off + 2].copy_from_slice(&(-1i16).to_le_bytes());
            }
        }
        for i in 0..classifications as usize {
            let cascade = arena.bytes()[cascade_off as usize + i];
            for stage in 0..MAX_RESIDUE_STAGES {
                if cascade & (1 << stage) != 0 {
                    let book = br.read_bits(8)? as usize;
                    if book >= codebook_count {
                        return Err(DecodeError::Malformed);
                    }
                    let off = books_off as usize + (i * MAX_RESIDUE_STAGES + stage) * 2;
                    arena.bytes_mut()[off..off + 2].copy_from_slice(&(book as i16).to_le_bytes());
                }
            }
        }

        Ok(ResidueConfig {
            residue_type,
            begin,
            end,
            partition_size,
            classifications: classifications as u8,
            classbook,
            cascade_off,
            books_off,
        })
    }

    /// `cascade[i]` — the 8-bit active-stage bitmap for classification `i`
    /// (`i < self.classifications`).
    pub fn cascade(&self, storage: &[u8], i: usize) -> u8 {
        storage[self.cascade_off as usize + i]
    }

    /// `books[i][stage]` (`i < self.classifications`, `stage < 8`); `-1`
    /// means that stage is inactive for classification `i`.
    pub fn book(&self, storage: &[u8], i: usize, stage: usize) -> i16 {
        let off = self.books_off as usize + (i * MAX_RESIDUE_STAGES + stage) * 2;
        i16::from_le_bytes(storage[off..off + 2].try_into().unwrap())
    }
}

/// Widest VQ vector this decoder reconstructs from a residue codebook in one
/// [`Codebook::lookup_vector`] call. Real residue books use small dimensions
/// (typically 1–16); a book beyond this is a graceful [`DecodeError::Unsupported`].
const MAX_CB_DIMS: usize = 256;

/// Upper bound on residue partitions in one decode. Type-2 concatenates all
/// `ch` channels into one pseudo-vector of length `ch·n`, so the worst case is
/// `MAX_CHANNELS · MAX_N` single-element partitions — the classification
/// scratch is sized to it.
const MAX_RESIDUE_PARTS: usize = MAX_CHANNELS * MAX_N;

/// Inverse channel coupling for one `(magnitude, angle)` element pair (spec
/// §4.3.5): reconstruct the two channels' Cartesian spectra from the
/// magnitude/angle representation via the sign-based formula. Returns
/// `(new_magnitude, new_angle)`.
pub(crate) fn couple(mag: f32, ang: f32) -> (f32, f32) {
    if mag > 0.0 {
        if ang > 0.0 {
            (mag, mag - ang)
        } else {
            (mag + ang, mag)
        }
    } else if ang > 0.0 {
        (mag, mag + ang)
    } else {
        (mag - ang, mag)
    }
}

/// Decode one residue (spec §8.6.2–§8.6.5) for the `ch_list` channels of a
/// submap, **accumulating** the decoded VQ vectors into `residue_bufs`
/// (`+=` — the caller has zeroed the `[0..n)` range of every channel first).
///
/// Handles all three residue types: type 0 (strided partition placement,
/// §8.6.3), type 1 (sequential, §8.6.4), and type 2 (§8.6.5 — the channels are
/// interleaved into a single pseudo-vector of length `ch·n`, decoded as a
/// single sequential stream, and de-interleaved back so pseudo-position `p`
/// lands in channel `p mod ch`, element `p div ch`). `do_not_decode[j]` marks
/// a channel whose floor was unused; type 2 still decodes the whole pseudo-
/// vector if **any** channel is used.
///
/// # Errors
/// [`DecodeError::Malformed`] for a zero partition size / classword count.
/// [`DecodeError::Unsupported`] for a residue book wider than [`MAX_CB_DIMS`].
/// Propagates [`Codebook::decode_scalar`] errors (truncated/corrupt packet).
#[allow(clippy::too_many_arguments)]
pub(crate) fn residue_decode(
    cfg: &ResidueConfig,
    codebooks: &[Codebook],
    arena: &[u8],
    br: &mut BitReader,
    n: usize,
    ch_list: &[usize],
    do_not_decode: &[bool],
    residue_bufs: &mut [[f32; MAX_N]; MAX_CHANNELS],
) -> Result<(), DecodeError> {
    let ch = ch_list.len();
    if ch == 0 {
        return Ok(());
    }
    let rtype = cfg.residue_type;
    // Type 2 works over the concatenated ch·n pseudo-vector; 0/1 over n.
    let actual_size = if rtype == 2 { ch * n } else { n };
    let begin = (cfg.begin as usize).min(actual_size);
    let end = (cfg.end as usize).min(actual_size);
    if end <= begin {
        return Ok(());
    }
    let part_size = cfg.partition_size as usize;
    if part_size == 0 {
        return Err(DecodeError::Malformed);
    }
    let parts_to_read = (end - begin) / part_size;
    if parts_to_read == 0 {
        return Ok(());
    }

    let classbook = &codebooks[cfg.classbook as usize];
    let classwords = classbook.dims as usize; // classwords_per_codeword
    if classwords == 0 {
        return Err(DecodeError::Malformed);
    }
    let classifications = cfg.classifications as usize;

    // Reusable VQ output scratch (one memset for the whole residue, not one
    // per partition); `lookup_vector` overwrites `[..dims]` each call.
    let mut vqvec = [0.0f32; MAX_CB_DIMS];
    // Per-partition classification numbers, remembered across the 8 passes.
    // Row `jc` for type 0/1; row 0 alone for the type-2 single stream.
    let mut class = [[0u8; MAX_RESIDUE_PARTS]; MAX_CHANNELS];

    if rtype == 2 {
        // Skip entirely only if every channel is unused.
        if !do_not_decode.iter().any(|&d| !d) {
            return Ok(());
        }
        for pass in 0..MAX_RESIDUE_STAGES {
            let mut pc = 0usize;
            while pc < parts_to_read {
                if pass == 0 {
                    let mut temp = classbook.decode_scalar(br, arena)? as usize;
                    for cw in (0..classwords).rev() {
                        let idx = pc + cw;
                        if idx < MAX_RESIDUE_PARTS {
                            class[0][idx] = (temp % classifications) as u8;
                        }
                        temp /= classifications;
                    }
                }
                let mut cw = 0;
                while cw < classwords && pc < parts_to_read {
                    let vqclass = class[0][pc] as usize;
                    let vqbook = cfg.book(arena, vqclass, pass);
                    if vqbook >= 0 {
                        let book = &codebooks[vqbook as usize];
                        let off = begin + pc * part_size;
                        decode_partition_type2(
                            book,
                            br,
                            arena,
                            residue_bufs,
                            ch_list,
                            ch,
                            off,
                            part_size,
                            n,
                            &mut vqvec,
                        )?;
                    }
                    pc += 1;
                    cw += 1;
                }
            }
        }
    } else {
        for pass in 0..MAX_RESIDUE_STAGES {
            let mut pc = 0usize;
            while pc < parts_to_read {
                if pass == 0 {
                    for (jc, &skip) in do_not_decode.iter().enumerate() {
                        if skip {
                            continue;
                        }
                        let mut temp = classbook.decode_scalar(br, arena)? as usize;
                        for cw in (0..classwords).rev() {
                            let idx = pc + cw;
                            if idx < MAX_RESIDUE_PARTS {
                                class[jc][idx] = (temp % classifications) as u8;
                            }
                            temp /= classifications;
                        }
                    }
                }
                let mut cw = 0;
                while cw < classwords && pc < parts_to_read {
                    for (jc, &gch) in ch_list.iter().enumerate() {
                        if do_not_decode[jc] {
                            continue;
                        }
                        let vqclass = class[jc][pc] as usize;
                        let vqbook = cfg.book(arena, vqclass, pass);
                        if vqbook >= 0 {
                            let book = &codebooks[vqbook as usize];
                            let off = begin + pc * part_size;
                            if rtype == 0 {
                                decode_partition_type0(
                                    book,
                                    br,
                                    arena,
                                    &mut residue_bufs[gch],
                                    off,
                                    part_size,
                                    n,
                                    &mut vqvec,
                                )?;
                            } else {
                                decode_partition_type1(
                                    book,
                                    br,
                                    arena,
                                    &mut residue_bufs[gch],
                                    off,
                                    part_size,
                                    n,
                                    &mut vqvec,
                                )?;
                            }
                        }
                    }
                    pc += 1;
                    cw += 1;
                }
            }
        }
    }
    Ok(())
}

/// Residue format 0 partition decode (spec §8.6.3): `step = part_size / dims`
/// codewords, each scattered with stride `step` (element `k` → `off + s + k·step`).
#[allow(clippy::too_many_arguments)]
fn decode_partition_type0(
    book: &Codebook,
    br: &mut BitReader,
    arena: &[u8],
    out: &mut [f32; MAX_N],
    off: usize,
    part_size: usize,
    n: usize,
    vqvec: &mut [f32],
) -> Result<(), DecodeError> {
    let dims = book.dims as usize;
    if dims == 0 || dims > MAX_CB_DIMS {
        return Err(DecodeError::Unsupported);
    }
    let step = part_size / dims;
    for s in 0..step {
        let entry = book.decode_scalar(br, arena)?;
        book.lookup_vector(entry, arena, &mut vqvec[..dims]);
        for (k, &v) in vqvec.iter().take(dims).enumerate() {
            let idx = off + s + k * step;
            if idx < n {
                out[idx] += v;
            }
        }
    }
    Ok(())
}

/// Residue format 1 partition decode (spec §8.6.4): codewords placed
/// sequentially across the partition (`off`, `off+1`, …).
#[allow(clippy::too_many_arguments)]
fn decode_partition_type1(
    book: &Codebook,
    br: &mut BitReader,
    arena: &[u8],
    out: &mut [f32; MAX_N],
    off: usize,
    part_size: usize,
    n: usize,
    vqvec: &mut [f32],
) -> Result<(), DecodeError> {
    let dims = book.dims as usize;
    if dims == 0 || dims > MAX_CB_DIMS {
        return Err(DecodeError::Unsupported);
    }
    let mut i = 0usize;
    while i < part_size {
        let entry = book.decode_scalar(br, arena)?;
        book.lookup_vector(entry, arena, &mut vqvec[..dims]);
        for &v in vqvec.iter().take(dims) {
            if i >= part_size {
                break;
            }
            let idx = off + i;
            if idx < n {
                out[idx] += v;
            }
            i += 1;
        }
    }
    Ok(())
}

/// Residue format 2 partition decode (spec §8.6.5): sequential like format 1,
/// but over the concatenated pseudo-vector — pseudo-position `p` de-interleaves
/// to channel `ch_list[p mod ch]`, element `p div ch`.
#[allow(clippy::too_many_arguments)]
fn decode_partition_type2(
    book: &Codebook,
    br: &mut BitReader,
    arena: &[u8],
    bufs: &mut [[f32; MAX_N]; MAX_CHANNELS],
    ch_list: &[usize],
    ch: usize,
    off: usize,
    part_size: usize,
    n: usize,
    vqvec: &mut [f32],
) -> Result<(), DecodeError> {
    let dims = book.dims as usize;
    if dims == 0 || dims > MAX_CB_DIMS {
        return Err(DecodeError::Unsupported);
    }
    let end_p = off + part_size;
    let mut p = off;
    while p < end_p {
        let entry = book.decode_scalar(br, arena)?;
        book.lookup_vector(entry, arena, &mut vqvec[..dims]);
        for &v in vqvec.iter().take(dims) {
            if p >= end_p {
                break;
            }
            let gch = ch_list[p % ch];
            let elem = p / ch;
            if gch < MAX_CHANNELS && elem < n {
                bufs[gch][elem] += v;
            }
            p += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
                let idx = self.bit >> 3;
                if idx >= self.bytes.len() {
                    self.bytes.push(0);
                }
                if (value >> i) & 1 != 0 {
                    self.bytes[idx] |= 1 << (self.bit & 7);
                }
                self.bit += 1;
            }
        }
        fn into_bytes(self) -> Vec<u8> {
            self.bytes
        }
    }

    /// A residue with 2 classifications: classification 0 has cascade bit 0
    /// only (stage-0 book); classification 1 has cascade bits 0 and 1 (a
    /// low+high cascade needing the 5-bit high-bits read).
    fn write_residue(w: &mut BitWriter) {
        w.write(2, 16); // residue_type = 2
        w.write(0, 24); // begin
        w.write(128, 24); // end
        w.write(15, 24); // partition_size - 1 = 15 => 16
        w.write(1, 6); // classifications - 1 = 1 => 2
        w.write(0, 8); // classbook = 0
        // classification 0: low=1 (bit0 set), bitflag=0 => cascade = 1
        w.write(1, 3);
        w.write(0, 1);
        // classification 1: low=1, bitflag=1, high=1 (bit3 set) => cascade = (1<<3)|1 = 9
        w.write(1, 3);
        w.write(1, 1);
        w.write(1, 5);
        // stage books: classification0 stage0 => book index 1
        w.write(1, 8);
        // classification1 stage0 => book index 2, stage3 => book index 3
        w.write(2, 8);
        w.write(3, 8);
    }

    fn parse_ok(codebook_count: usize) -> (ResidueConfig, Vec<u8>) {
        let mut w = BitWriter::new();
        write_residue(&mut w);
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 4096];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        let cfg = ResidueConfig::parse(&mut br, &mut arena, codebook_count).expect("valid residue");
        (cfg, storage)
    }

    #[test]
    fn parses_scalar_fields() {
        let (cfg, _storage) = parse_ok(4);
        assert_eq!(cfg.residue_type, 2);
        assert_eq!(cfg.begin, 0);
        assert_eq!(cfg.end, 128);
        assert_eq!(cfg.partition_size, 16);
        assert_eq!(cfg.classifications, 2);
        assert_eq!(cfg.classbook, 0);
    }

    #[test]
    fn parses_cascade_and_stage_books() {
        let (cfg, storage) = parse_ok(4);
        assert_eq!(cfg.cascade(&storage, 0), 1);
        assert_eq!(cfg.cascade(&storage, 1), 9);
        assert_eq!(cfg.book(&storage, 0, 0), 1);
        assert_eq!(cfg.book(&storage, 0, 1), -1);
        assert_eq!(cfg.book(&storage, 1, 0), 2);
        assert_eq!(cfg.book(&storage, 1, 1), -1);
        assert_eq!(cfg.book(&storage, 1, 2), -1);
        assert_eq!(cfg.book(&storage, 1, 3), 3);
    }

    #[test]
    fn residue_type_over_2_is_malformed() {
        let mut w = BitWriter::new();
        w.write(3, 16); // residue_type = 3 (reserved)
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 256];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        assert_eq!(
            ResidueConfig::parse(&mut br, &mut arena, 4).unwrap_err(),
            DecodeError::Malformed
        );
    }

    #[test]
    fn begin_after_end_is_malformed() {
        let mut w = BitWriter::new();
        w.write(0, 16); // residue_type = 0
        w.write(100, 24); // begin
        w.write(50, 24); // end < begin
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 256];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        assert_eq!(
            ResidueConfig::parse(&mut br, &mut arena, 4).unwrap_err(),
            DecodeError::Malformed
        );
    }

    #[test]
    fn classbook_out_of_range_is_malformed() {
        let mut w = BitWriter::new();
        w.write(0, 16);
        w.write(0, 24);
        w.write(0, 24);
        w.write(0, 24); // partition_size - 1
        w.write(0, 6); // classifications - 1 = 0 => 1
        w.write(5, 8); // classbook = 5, but codebook_count = 2
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 256];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        assert_eq!(
            ResidueConfig::parse(&mut br, &mut arena, 2).unwrap_err(),
            DecodeError::Malformed
        );
    }

    #[test]
    fn stage_book_out_of_range_is_malformed() {
        let mut w = BitWriter::new();
        w.write(0, 16);
        w.write(0, 24);
        w.write(0, 24);
        w.write(0, 24);
        w.write(0, 6); // classifications = 1
        w.write(0, 8); // classbook = 0
        w.write(1, 3); // low bits: bit0 set
        w.write(0, 1); // no high bits
        w.write(9, 8); // stage0 book = 9, but codebook_count = 2
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 256];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        assert_eq!(
            ResidueConfig::parse(&mut br, &mut arena, 2).unwrap_err(),
            DecodeError::Malformed
        );
    }

    #[test]
    fn classifications_over_cap_is_unsupported() {
        let mut w = BitWriter::new();
        w.write(0, 16);
        w.write(0, 24);
        w.write(0, 24);
        w.write(0, 24);
        // classifications - 1 = 40 => 41, which is spec-legal (<=64) but over
        // MAX_RESIDUE_CLASSIFICATIONS (32).
        w.write(40, 6);
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 256];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        assert_eq!(
            ResidueConfig::parse(&mut br, &mut arena, 4).unwrap_err(),
            DecodeError::Unsupported
        );
    }
}
