//! Vorbis **floor 1** setup (spec §7.2.1) — the only floor type this decoder
//! implements (floor 0 is dead in practice on modern encodes and is rejected
//! by the caller as [`DecodeError::Unsupported`] before reaching this module).
//!
//! A floor-1 curve is defined by a small tree of "partition classes" (each
//! declaring a dimension count, a subclass count, and the codebooks used to
//! refine each subclass) and a list of X (frequency-axis) positions those
//! classes place points at. Every list here is small and spec-bounded — the
//! X-value list itself is *hard-capped at 65 elements by the spec itself*
//! (§7.2.1: "renders the stream undecodable" past that), not a cap of ours —
//! so, like [`Codebook`](super::codebook::Codebook), the descriptor is a
//! small `Copy` struct with the per-partition/per-class/per-value tables laid
//! into the caller's [`Arena`] and addressed by byte offset.
//!
//! # Field layout (spec §7.2.1, cross-checked against lewton's `header.rs`
//! and independently round-tripped bit-for-bit against a real
//! ffmpeg/libvorbis setup header — see the Task 5 report)
//!
//! ```text
//! floor1_partitions            = read 5 bits                (0..=31)
//! partition_class_list[i]      = read 4 bits, i in 0..partitions
//! maximum_class                = max(partition_class_list) (or -1 if partitions == 0)
//! for c in 0..=maximum_class:
//!   class_dimensions[c]        = read 3 bits + 1             (1..=8)
//!   class_subclasses[c]        = read 2 bits                 (0..=3)
//!   if class_subclasses[c] != 0:
//!     class_masterbooks[c]     = read 8 bits                 (validate < codebook_count)
//!   for j in 0..2^class_subclasses[c]:
//!     subclass_books[c][j]     = read 8 bits, then -1        (-1..=254, validate < codebook_count)
//! floor1_multiplier             = read 2 bits + 1             (1..=4)
//! rangebits                     = read 4 bits                 (0..=15)
//! floor1_values                 = 2 + sum(class_dimensions[partition_class_list[i]] for i in partitions)
//!                                  (validate <= 65)
//! X_list[0] = 0; X_list[1] = 2^rangebits
//! X_list[2..]                   = read `rangebits` bits each, in partition/dimension order
//! ```

use crate::decoder::DecodeError;

use super::arena::Arena;
use super::bitread::{BitReader, ilog};
use super::codebook::Codebook;

/// Spec ceiling on `floor1_partitions`: the field is a raw (no `+1`) 5-bit
/// unsigned read, so `0..=31` — exact, not a heuristic (mirrors
/// [`super::setup::MAX_CODEBOOKS`]'s reasoning).
pub const MAX_FLOOR1_PARTITIONS: usize = 31;

/// Spec ceiling on the number of distinct partition *classes*: a class id is
/// a 4-bit field (`0..=15`), so at most 16 distinct classes can ever be
/// referenced — exact, not a heuristic.
pub const MAX_FLOOR1_CLASSES: usize = 16;

/// Spec ceiling on subclass books per class: `class_subclasses` is a 2-bit
/// field (`0..=3`), so a class has at most `2^3 == 8` subclass-book slots —
/// exact, not a heuristic.
pub const MAX_FLOOR1_SUBCLASS_BOOKS: usize = 8;

/// **Spec-mandated** (not merely a local heuristic) ceiling on the total
/// element count of `floor1_X_list`: spec §7.2.1 states outright that a setup
/// implying more than 65 total elements (including the two implicit `0` /
/// `2^rangebits` entries) "renders the stream undecodable" — i.e. this is a
/// hard conformance limit, not a resource cap, so exceeding it is
/// [`DecodeError::Malformed`], not [`DecodeError::Unsupported`] (matches
/// lewton's `HeaderBadFormat` on the same check).
pub const MAX_FLOOR1_VALUES: usize = 65;

/// A parsed floor-1 configuration (spec §7.2.1): small, `Copy`, and free of
/// self-references — the per-partition/per-class/per-value tables live in the
/// [`Arena`] at the recorded byte offsets. See the module docs for the exact
/// field layout and [`Codebook`](super::codebook::Codebook)'s docs for why
/// this offset-addressed shape is used throughout the setup parse.
#[derive(Clone, Copy, Debug)]
pub struct Floor1Config {
    /// `floor1_partitions` (`0..=31`).
    pub partitions: u8,
    /// `maximum_class + 1` — the number of distinct partition classes
    /// actually declared (`0` iff `partitions == 0`).
    pub class_count: u8,
    /// `floor1_multiplier` (`1..=4`).
    pub multiplier: u8,
    /// `rangebits` (`0..=15`) — both the bit width of every `X_list` read
    /// past the first two entries, and the log2 of the curve's X range.
    pub rangebits: u8,
    /// `floor1_values` — total `X_list` length (`2..=65`).
    pub n_values: u16,
    /// Offset of the per-partition class-id table (`u8` × [`Self::partitions`]).
    partition_class_off: u32,
    /// Offset of the per-class dimension table (`u8` × [`Self::class_count`]).
    class_dims_off: u32,
    /// Offset of the per-class subclass-count table (`u8` × [`Self::class_count`]).
    class_subclasses_off: u32,
    /// Offset of the per-class masterbook-index table (`u8` × [`Self::class_count`];
    /// meaningful only where the class's subclass count is nonzero).
    class_masterbook_off: u32,
    /// Offset of the subclass-book table: `i16` (LE) × [`Self::class_count`] ×
    /// [`MAX_FLOOR1_SUBCLASS_BOOKS`], stride-8 per class (only the first
    /// `2^class_subclasses[c]` slots of each class's stride are meaningful;
    /// `-1` means "no book"/unused).
    subclass_books_off: u32,
    /// Offset of the `X_list` table (`u32` LE × [`Self::n_values`]).
    x_list_off: u32,
}

impl Floor1Config {
    /// The zero value, for initializing fixed floor arrays before parse
    /// fills the live prefix (mirrors [`Codebook::EMPTY`](super::codebook::Codebook::EMPTY)).
    pub const EMPTY: Floor1Config = Floor1Config {
        partitions: 0,
        class_count: 0,
        multiplier: 0,
        rangebits: 0,
        n_values: 0,
        partition_class_off: 0,
        class_dims_off: 0,
        class_subclasses_off: 0,
        class_masterbook_off: 0,
        subclass_books_off: 0,
        x_list_off: 0,
    };

    /// Parse one floor-1 configuration body (spec §7.2.1) — the caller has
    /// already consumed the discriminating `floor_type == 1` field.
    ///
    /// # Errors
    /// [`DecodeError::Malformed`] for an out-of-range codebook cross-reference
    /// (masterbook/subclass-book index), an over-65 `X_list`, or a duplicate
    /// `X_list` value (spec: floor curve fitting requires unique X positions;
    /// mirrors lewton's `header.rs`, which rejects the same). Bit widths
    /// derived from the file (`rangebits`) are always `<= 15`, so every
    /// dynamic-width read here is provably in [`BitReader::read_bits`]'s
    /// `<= 32` contract — no separate range check is needed.
    /// [`DecodeError::Unsupported`] if the arena cannot fit the tables.
    /// [`DecodeError::UnexpectedEof`] if the packet ends mid-floor.
    pub fn parse(
        br: &mut BitReader,
        arena: &mut Arena,
        codebook_count: usize,
    ) -> Result<Floor1Config, DecodeError> {
        let partitions = br.read_bits(5)? as u8;
        let partition_class_off = arena.alloc(partitions as usize, 1)? as u32;
        let mut maximum_class: i32 = -1;
        {
            let buf = arena.bytes_mut();
            for i in 0..partitions as usize {
                let class_id = br.read_bits(4)? as u8;
                if i32::from(class_id) > maximum_class {
                    maximum_class = i32::from(class_id);
                }
                buf[partition_class_off as usize + i] = class_id;
            }
        }
        let class_count = (maximum_class + 1) as usize; // 0..=15 => class_count 0..=16

        let class_dims_off = arena.alloc(class_count, 1)? as u32;
        let class_subclasses_off = arena.alloc(class_count, 1)? as u32;
        let class_masterbook_off = arena.alloc(class_count, 1)? as u32;
        let subclass_books_len = class_count
            .checked_mul(MAX_FLOOR1_SUBCLASS_BOOKS)
            .ok_or(DecodeError::Unsupported)?;
        let subclass_books_off = arena.alloc(
            subclass_books_len
                .checked_mul(2)
                .ok_or(DecodeError::Unsupported)?,
            2,
        )? as u32;
        // Slots beyond a class's real 2^subclasses count are never read at
        // decode time, but initialize the whole stride to "-1/no book" so a
        // stray read (or a future differential dump) never observes
        // uninitialized arena bytes as a spurious book index.
        {
            let buf = arena.bytes_mut();
            for i in 0..subclass_books_len {
                let off = subclass_books_off as usize + i * 2;
                buf[off..off + 2].copy_from_slice(&(-1i16).to_le_bytes());
            }
        }

        for c in 0..class_count {
            let dim = br.read_bits(3)? as u8 + 1;
            let subclass = br.read_bits(2)? as u8;
            arena.bytes_mut()[class_dims_off as usize + c] = dim;
            arena.bytes_mut()[class_subclasses_off as usize + c] = subclass;
            let masterbook = if subclass != 0 {
                let mb = br.read_bits(8)? as u8;
                if mb as usize >= codebook_count {
                    return Err(DecodeError::Malformed);
                }
                mb
            } else {
                0
            };
            arena.bytes_mut()[class_masterbook_off as usize + c] = masterbook;

            let books_cnt = 1u32 << subclass; // subclass <= 3, so <= 8
            for j in 0..books_cnt as usize {
                let raw = br.read_bits(8)? as i32 - 1; // -1..=254
                if raw >= codebook_count as i32 {
                    return Err(DecodeError::Malformed);
                }
                let off = subclass_books_off as usize + (c * MAX_FLOOR1_SUBCLASS_BOOKS + j) * 2;
                arena.bytes_mut()[off..off + 2].copy_from_slice(&(raw as i16).to_le_bytes());
            }
        }

        let multiplier = br.read_bits(2)? as u8 + 1;
        let rangebits = br.read_bits(4)? as u8; // 0..=15, always a valid read_bits width

        // floor1_values: 2 fixed entries + the per-partition class dimensions,
        // computed up front (spec order) so the X_list arena alloc is exact.
        let mut n_values: u32 = 2;
        {
            let buf = arena.bytes();
            for i in 0..partitions as usize {
                let class_id = buf[partition_class_off as usize + i] as usize;
                n_values += u32::from(buf[class_dims_off as usize + class_id]);
            }
        }
        if n_values as usize > MAX_FLOOR1_VALUES {
            return Err(DecodeError::Malformed);
        }

        let x_list_off = arena.alloc(n_values as usize * 4, 4)? as u32;
        {
            let buf = arena.bytes_mut();
            buf[x_list_off as usize..x_list_off as usize + 4].copy_from_slice(&0u32.to_le_bytes());
            let x1 = 1u32 << rangebits;
            buf[x_list_off as usize + 4..x_list_off as usize + 8]
                .copy_from_slice(&x1.to_le_bytes());
        }
        let mut next = 2usize;
        for i in 0..partitions as usize {
            let class_id = arena.bytes()[partition_class_off as usize + i] as usize;
            let dim = arena.bytes()[class_dims_off as usize + class_id] as usize;
            for _ in 0..dim {
                let v = br.read_bits(u32::from(rangebits))?;
                let off = x_list_off as usize + next * 4;
                arena.bytes_mut()[off..off + 4].copy_from_slice(&v.to_le_bytes());
                next += 1;
            }
        }
        debug_assert_eq!(next, n_values as usize);

        // Duplicate-X check (spec: floor curve fitting requires unique X
        // positions; mirrors lewton's `header.rs`). n_values <= 65, so a
        // fixed 65-slot scratch array plus an in-place unstable sort (no
        // alloc — `sort_unstable` is a `core` API) is cheap and correct.
        let mut sorted = [0u32; MAX_FLOOR1_VALUES];
        {
            let buf = arena.bytes();
            for (i, slot) in sorted.iter_mut().enumerate().take(n_values as usize) {
                let off = x_list_off as usize + i * 4;
                *slot = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
            }
        }
        let used = &mut sorted[..n_values as usize];
        used.sort_unstable();
        for w in used.windows(2) {
            if w[0] == w[1] {
                return Err(DecodeError::Malformed);
            }
        }

        Ok(Floor1Config {
            partitions,
            class_count: class_count as u8,
            multiplier,
            rangebits,
            n_values: n_values as u16,
            partition_class_off,
            class_dims_off,
            class_subclasses_off,
            class_masterbook_off,
            subclass_books_off,
            x_list_off,
        })
    }

    /// `partition_class_list[i]` — the class id of partition `i`
    /// (`i < self.partitions`).
    pub fn partition_class(&self, storage: &[u8], i: usize) -> u8 {
        storage[self.partition_class_off as usize + i]
    }

    /// `class_dimensions[c]` (`c < self.class_count`).
    pub fn class_dimension(&self, storage: &[u8], c: usize) -> u8 {
        storage[self.class_dims_off as usize + c]
    }

    /// `class_subclasses[c]` (`c < self.class_count`).
    pub fn class_subclasses(&self, storage: &[u8], c: usize) -> u8 {
        storage[self.class_subclasses_off as usize + c]
    }

    /// `class_masterbooks[c]` (`c < self.class_count`); meaningful only when
    /// [`Self::class_subclasses`] is nonzero.
    pub fn class_masterbook(&self, storage: &[u8], c: usize) -> u8 {
        storage[self.class_masterbook_off as usize + c]
    }

    /// `subclass_books[c][j]` (`c < self.class_count`,
    /// `j < 2^class_subclasses[c]`); `-1` means "no book" (unused).
    pub fn subclass_book(&self, storage: &[u8], c: usize, j: usize) -> i16 {
        let off = self.subclass_books_off as usize + (c * MAX_FLOOR1_SUBCLASS_BOOKS + j) * 2;
        i16::from_le_bytes(storage[off..off + 2].try_into().unwrap())
    }

    /// `X_list[i]` (`i < self.n_values`).
    pub fn x_value(&self, storage: &[u8], i: usize) -> u32 {
        let off = self.x_list_off as usize + i * 4;
        u32::from_le_bytes(storage[off..off + 4].try_into().unwrap())
    }
}

/// `floor1_inverse_dB_static_table` (spec §7.2.4): the 256-entry dB→linear
/// amplitude map applied to the integer floor curve to produce the linear
/// multipliers the dot product (spec §4.3.6) uses.
///
/// Generated host-side (this crate forbids `libm`/runtime transcendentals) as
/// the pure geometric progression `10^(7·(i−255)/256)`. That reproduces the
/// libvorbis/spec published constants **exactly** at the loud end (`i ≥ 254`)
/// and to within ~6 `f32` ULP (~7e-7 relative) only at the tiny near-silent
/// values around `1e-7` — far below any golden/audible tolerance. See the
/// Task 6 report for the derivation and the anchor cross-check.
pub(crate) const FLOOR1_INVERSE_DB: [f32; 256] = [
    0.00000010649856,
    0.00000011341944,
    0.00000012079008,
    0.0000001286397,
    0.00000013699943,
    0.00000014590242,
    0.00000015538399,
    0.00000016548171,
    0.00000017623564,
    0.00000018768843,
    0.00000019988548,
    0.00000021287516,
    0.00000022670899,
    0.00000024144182,
    0.0000002571321,
    0.00000027384198,
    0.00000029163775,
    0.00000031059002,
    0.0000003307739,
    0.00000035226947,
    0.00000037516193,
    0.00000039954205,
    0.00000042550656,
    0.00000045315835,
    0.00000048260716,
    0.00000051396967,
    0.00000054737035,
    0.00000058294154,
    0.0000006208244,
    0.00000066116905,
    0.0000007041355,
    0.0000007498942,
    0.00000079862656,
    0.0000008505258,
    0.0000009057978,
    0.0000009646616,
    0.0000010273508,
    0.0000010941138,
    0.0000011652155,
    0.0000012409378,
    0.0000013215808,
    0.0000014074647,
    0.0000014989297,
    0.0000015963385,
    0.0000017000776,
    0.0000018105583,
    0.0000019282186,
    0.0000020535251,
    0.0000021869746,
    0.0000023290966,
    0.0000024804544,
    0.0000026416483,
    0.0000028133175,
    0.0000029961427,
    0.000003190849,
    0.0000033982083,
    0.0000036190431,
    0.000003854229,
    0.0000041046983,
    0.000004371445,
    0.000004655526,
    0.000004958068,
    0.0000052802716,
    0.0000056234135,
    0.0000059888544,
    0.000006378044,
    0.0000067925253,
    0.0000072339417,
    0.000007704044,
    0.000008204696,
    0.000008737884,
    0.000009305721,
    0.000009910458,
    0.000010554496,
    0.000011240387,
    0.00001197085,
    0.000012748784,
    0.000013577272,
    0.000014459599,
    0.000015399266,
    0.000016399996,
    0.00001746576,
    0.000018600784,
    0.000019809568,
    0.000021096905,
    0.0000224679,
    0.000023927993,
    0.000025482968,
    0.000027138994,
    0.00002890264,
    0.000030780895,
    0.000032781212,
    0.000034911518,
    0.000037180267,
    0.00003959645,
    0.00004216965,
    0.000044910073,
    0.00004782858,
    0.000050936753,
    0.00005424691,
    0.00005777218,
    0.00006152654,
    0.00006552488,
    0.00006978306,
    0.000074317955,
    0.000079147554,
    0.00008429101,
    0.00008976871,
    0.00009560239,
    0.000101815174,
    0.0001084317,
    0.0001154782,
    0.00012298262,
    0.00013097473,
    0.0001394862,
    0.0001485508,
    0.00015820446,
    0.0001684855,
    0.00017943462,
    0.0001910953,
    0.00020351374,
    0.00021673922,
    0.00023082415,
    0.0002458244,
    0.00026179946,
    0.00027881266,
    0.00029693148,
    0.00031622776,
    0.00033677803,
    0.00035866376,
    0.00038197177,
    0.00040679442,
    0.00043323022,
    0.00046138396,
    0.0004913673,
    0.0005232991,
    0.00055730605,
    0.0005935229,
    0.0006320934,
    0.0006731704,
    0.0007169168,
    0.0007635061,
    0.000813123,
    0.00086596434,
    0.00092223956,
    0.0009821719,
    0.001045999,
    0.0011139739,
    0.0011863662,
    0.0012634629,
    0.0013455698,
    0.0014330125,
    0.0015261378,
    0.0016253148,
    0.0017309369,
    0.001843423,
    0.001963219,
    0.0020908,
    0.002226672,
    0.0023713738,
    0.0025254788,
    0.0026895988,
    0.002864384,
    0.003050528,
    0.0032487684,
    0.0034598918,
    0.003684735,
    0.0039241896,
    0.0041792057,
    0.004450794,
    0.004740032,
    0.005048066,
    0.0053761173,
    0.005725488,
    0.006097562,
    0.006493816,
    0.006915821,
    0.00736525,
    0.007843886,
    0.008353625,
    0.0088964915,
    0.009474635,
    0.01009035,
    0.010746079,
    0.011444419,
    0.012188142,
    0.012980196,
    0.0138237225,
    0.014722066,
    0.015678788,
    0.016697684,
    0.017782794,
    0.01893842,
    0.020169146,
    0.02147985,
    0.022875732,
    0.024362326,
    0.025945527,
    0.027631614,
    0.029427271,
    0.031339623,
    0.033376247,
    0.035545222,
    0.037855152,
    0.040315192,
    0.042935103,
    0.045725267,
    0.048696753,
    0.051861342,
    0.055231582,
    0.058820844,
    0.06264336,
    0.06671427,
    0.07104974,
    0.07566696,
    0.08058422,
    0.08582103,
    0.09139817,
    0.09733774,
    0.103663296,
    0.11039992,
    0.11757433,
    0.12521496,
    0.13335215,
    0.14201811,
    0.15124725,
    0.16107616,
    0.17154379,
    0.18269168,
    0.194564,
    0.20720787,
    0.22067341,
    0.235014,
    0.25028655,
    0.26655158,
    0.2838736,
    0.3023213,
    0.32196784,
    0.34289113,
    0.3651741,
    0.3889052,
    0.41417846,
    0.4410941,
    0.46975887,
    0.50028646,
    0.5327979,
    0.5674221,
    0.6042964,
    0.64356697,
    0.6853896,
    0.72993004,
    0.777365,
    0.8278826,
    0.88168305,
    0.9389798,
    1.0,
];

/// Decode one channel's floor-1 curve for a single audio packet — the packet
/// decode (spec §7.2.2: `nonzero` flag → `Y` amplitudes via the class/subclass
/// books) followed by the curve synthesis (spec §7.2.4: neighbor prediction,
/// amplitude reconstruction, integer Bresenham line rendering, dB→linear).
///
/// Writes the **linear** floor multipliers into `out[..n]`. Returns `Ok(true)`
/// when the channel carries floor energy this packet, or `Ok(false)` for an
/// "unused" channel (the leading `nonzero` bit was clear) — in which case
/// `out[..n]` is zeroed and the caller treats the whole channel spectrum as
/// zero (§4.3.2: `no_residue` is set for that channel).
///
/// # Errors
/// Propagates [`Codebook::decode_scalar`] errors (truncated/corrupt packet).
pub(crate) fn floor1_decode(
    cfg: &Floor1Config,
    codebooks: &[Codebook],
    arena: &[u8],
    br: &mut BitReader,
    n: usize,
    out: &mut [f32],
) -> Result<bool, DecodeError> {
    // §7.2.2 step 1: the leading "this channel is used" flag.
    if !br.read_bit()? {
        for v in out.iter_mut().take(n) {
            *v = 0.0;
        }
        return Ok(false);
    }

    // §7.2.2: `range` selects the amplitude quantization; the first two Y
    // values are read raw, the rest via the per-partition class/subclass books.
    const RANGES: [i32; 4] = [256, 128, 86, 64];
    let mult = i32::from(cfg.multiplier); // 1..=4
    let range = RANGES[(cfg.multiplier - 1) as usize];
    let ybits = ilog((range - 1) as u32);
    let nv = cfg.n_values as usize;

    let mut yv = [0i32; MAX_FLOOR1_VALUES];
    yv[0] = br.read_bits(ybits)? as i32;
    yv[1] = br.read_bits(ybits)? as i32;

    let mut offset = 2usize;
    for i in 0..cfg.partitions as usize {
        let class = cfg.partition_class(arena, i) as usize;
        let cdim = cfg.class_dimension(arena, class) as usize;
        let cbits = u32::from(cfg.class_subclasses(arena, class));
        let csub = (1u32 << cbits) - 1;
        let mut cval = 0u32;
        if cbits > 0 {
            let master = cfg.class_masterbook(arena, class) as usize;
            cval = codebooks[master].decode_scalar(br, arena)?;
        }
        for j in 0..cdim {
            let book = cfg.subclass_book(arena, class, (cval & csub) as usize);
            cval >>= cbits;
            let y = if book >= 0 {
                codebooks[book as usize].decode_scalar(br, arena)? as i32
            } else {
                0
            };
            if offset + j < MAX_FLOOR1_VALUES {
                yv[offset + j] = y;
            }
        }
        offset += cdim;
    }

    // §7.2.4: amplitude reconstruction. `finaly` accumulates the dB-domain Y
    // values; `step2` marks which posts actually anchor a line segment.
    let mut finaly = [0i32; MAX_FLOOR1_VALUES];
    let mut step2 = [false; MAX_FLOOR1_VALUES];
    finaly[0] = yv[0];
    finaly[1] = yv[1];
    step2[0] = true;
    step2[1] = true;
    for i in 2..nv {
        let xi = cfg.x_value(arena, i) as i32;
        let low = low_neighbor(cfg, arena, i);
        let high = high_neighbor(cfg, arena, i);
        let predicted = render_point(
            cfg.x_value(arena, low) as i32,
            finaly[low],
            cfg.x_value(arena, high) as i32,
            finaly[high],
            xi,
        );
        let val = yv[i];
        let highroom = range - predicted;
        let lowroom = predicted;
        let room = if highroom < lowroom {
            highroom * 2
        } else {
            lowroom * 2
        };
        if val != 0 {
            step2[low] = true;
            step2[high] = true;
            step2[i] = true;
            finaly[i] = if val >= room {
                if highroom > lowroom {
                    val - lowroom + predicted
                } else {
                    predicted - val + highroom - 1
                }
            } else if val & 1 == 1 {
                predicted - ((val + 1) / 2)
            } else {
                predicted + (val / 2)
            };
        } else {
            step2[i] = false;
            finaly[i] = predicted;
        }
    }

    // §7.2.4: render the curve. Walk the posts in ascending X order, drawing an
    // integer line between successive `step2`-set posts, then extend flat to
    // `n`; each written point is mapped dB→linear through the table.
    let mut order = [0u8; MAX_FLOOR1_VALUES];
    for (i, slot) in order.iter_mut().enumerate().take(nv) {
        *slot = i as u8;
    }
    // Insertion sort the post indices by their X value (nv <= 65, so this is
    // cheap and needs no scratch beyond `order`).
    for a in 1..nv {
        let mut b = a;
        while b > 0
            && cfg.x_value(arena, order[b - 1] as usize) > cfg.x_value(arena, order[b] as usize)
        {
            order.swap(b, b - 1);
            b -= 1;
        }
    }

    let mut lx = 0i32;
    let mut ly = finaly[order[0] as usize] * mult;
    for &o in order.iter().take(nv).skip(1) {
        let cur = o as usize;
        if step2[cur] {
            let hy = finaly[cur] * mult;
            let hx = cfg.x_value(arena, cur) as i32;
            render_line(lx, ly, hx, hy, out, n);
            lx = hx;
            ly = hy;
        }
    }
    if (lx as usize) < n {
        render_line(lx, ly, n as i32, ly, out, n);
    }
    Ok(true)
}

/// `low_neighbor` (spec §7.2.4): index of the post `j < i` whose X value is the
/// **greatest** value still strictly less than `X[i]`.
fn low_neighbor(cfg: &Floor1Config, arena: &[u8], i: usize) -> usize {
    let xi = i64::from(cfg.x_value(arena, i));
    let mut best = 0usize;
    let mut best_x = -1i64;
    for j in 0..i {
        let xj = i64::from(cfg.x_value(arena, j));
        if xj < xi && xj > best_x {
            best_x = xj;
            best = j;
        }
    }
    best
}

/// `high_neighbor` (spec §7.2.4): index of the post `j < i` whose X value is the
/// **least** value still strictly greater than `X[i]`.
fn high_neighbor(cfg: &Floor1Config, arena: &[u8], i: usize) -> usize {
    let xi = i64::from(cfg.x_value(arena, i));
    let mut best = 0usize;
    let mut best_x = i64::MAX;
    for j in 0..i {
        let xj = i64::from(cfg.x_value(arena, j));
        if xj > xi && xj < best_x {
            best_x = xj;
            best = j;
        }
    }
    best
}

/// `render_point` (spec §7.2.4): the integer y value of the line through
/// `(x0,y0)`–`(x1,y1)` at abscissa `x` (`x0 < x < x1`).
fn render_point(x0: i32, y0: i32, x1: i32, y1: i32, x: i32) -> i32 {
    let dy = y1 - y0;
    let adx = x1 - x0;
    let ady = dy.abs();
    let err = ady * (x - x0);
    let off = if adx != 0 { err / adx } else { 0 };
    if dy < 0 { y0 - off } else { y0 + off }
}

/// `render_line` (spec §7.2.4): integer Bresenham-style line from `(x0,y0)` to
/// `(x1,y1)`, writing the **linear** (dB→linear) floor value into `out[x]` for
/// `x` in `[x0, min(x1, n))`. Writes are clipped to `[0, n)` so an X post at or
/// beyond the curve length can never index out of bounds.
fn render_line(x0: i32, y0: i32, x1: i32, y1: i32, out: &mut [f32], n: usize) {
    let adx = x1 - x0;
    if adx <= 0 {
        return;
    }
    let dy = y1 - y0;
    let base = dy / adx;
    let sy = if dy < 0 { base - 1 } else { base + 1 };
    let ady = dy.abs() - base.abs() * adx;
    let x1c = (x1).min(n as i32);
    let mut err = 0i32;
    let mut y = y0;
    let mut x = x0;
    if x >= 0 && (x as usize) < n {
        out[x as usize] = FLOOR1_INVERSE_DB[y.clamp(0, 255) as usize];
    }
    x += 1;
    while x < x1c {
        err += ady;
        if err >= adx {
            err -= adx;
            y += sy;
        } else {
            y += base;
        }
        if x >= 0 && (x as usize) < n {
            out[x as usize] = FLOOR1_INVERSE_DB[y.clamp(0, 255) as usize];
        }
        x += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal LSb-first bit writer (test-only), the exact inverse of
    /// [`BitReader`] — mirrors [`super::codebook::Codebook`]'s test helper.
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

    /// A small but non-trivial floor-1 config: 2 partitions, both class 0;
    /// class 0 has dim 2, subclasses 1 (2 subclass books, no masterbook read
    /// since... wait subclasses=1 != 0 so masterbook IS read); multiplier 2,
    /// rangebits 4. `floor1_values = 2 + 2*2 = 6`.
    fn write_floor1(w: &mut BitWriter) {
        w.write(2, 5); // partitions = 2
        w.write(0, 4); // partition_class_list[0] = 0
        w.write(0, 4); // partition_class_list[1] = 0
        // class 0 (maximum_class = 0, so class_count = 1):
        w.write(1, 3); // class_dimensions[0] - 1 = 1 => dim 2
        w.write(1, 2); // class_subclasses[0] = 1 => 2 subclass books
        w.write(0, 8); // class_masterbooks[0] = 0 (must be < codebook_count)
        w.write(1, 8); // subclass_books[0][0] raw = 1 => book 0
        w.write(2, 8); // subclass_books[0][1] raw = 2 => book 1
        w.write(1, 2); // multiplier - 1 = 1 => multiplier 2
        w.write(4, 4); // rangebits = 4
        // X_list: 2 partitions * dim 2 = 4 more values (total 6), each 4 bits.
        w.write(3, 4);
        w.write(7, 4);
        w.write(1, 4);
        w.write(11, 4);
    }

    fn parse_ok(codebook_count: usize) -> (Floor1Config, Vec<u8>) {
        let mut w = BitWriter::new();
        write_floor1(&mut w);
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 4096];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        let cfg = Floor1Config::parse(&mut br, &mut arena, codebook_count).expect("valid floor1");
        (cfg, storage)
    }

    #[test]
    fn parses_partitions_classes_and_metadata() {
        let (cfg, _storage) = parse_ok(4);
        assert_eq!(cfg.partitions, 2);
        assert_eq!(cfg.class_count, 1);
        assert_eq!(cfg.multiplier, 2);
        assert_eq!(cfg.rangebits, 4);
        assert_eq!(cfg.n_values, 6);
    }

    #[test]
    fn parses_class_and_subclass_book_tables() {
        let (cfg, storage) = parse_ok(4);
        assert_eq!(cfg.partition_class(&storage, 0), 0);
        assert_eq!(cfg.partition_class(&storage, 1), 0);
        assert_eq!(cfg.class_dimension(&storage, 0), 2);
        assert_eq!(cfg.class_subclasses(&storage, 0), 1);
        assert_eq!(cfg.class_masterbook(&storage, 0), 0);
        assert_eq!(cfg.subclass_book(&storage, 0, 0), 0); // raw 1 - 1 = 0
        assert_eq!(cfg.subclass_book(&storage, 0, 1), 1); // raw 2 - 1 = 1
    }

    #[test]
    fn parses_x_list_in_partition_dimension_order() {
        let (cfg, storage) = parse_ok(4);
        assert_eq!(cfg.x_value(&storage, 0), 0); // implicit
        assert_eq!(cfg.x_value(&storage, 1), 1 << 4); // implicit 2^rangebits
        assert_eq!(cfg.x_value(&storage, 2), 3);
        assert_eq!(cfg.x_value(&storage, 3), 7);
        assert_eq!(cfg.x_value(&storage, 4), 1);
        assert_eq!(cfg.x_value(&storage, 5), 11);
    }

    #[test]
    fn masterbook_out_of_range_is_malformed() {
        let mut w = BitWriter::new();
        w.write(1, 5); // partitions = 1
        w.write(0, 4); // partition_class_list[0] = 0
        w.write(0, 3); // class_dimensions[0] - 1 = 0 => dim 1
        w.write(1, 2); // class_subclasses[0] = 1
        w.write(5, 8); // masterbook = 5, but codebook_count will be 2 => out of range
        w.write(0, 8); // subclass_books[0][0]
        w.write(0, 8); // subclass_books[0][1]
        w.write(0, 2); // multiplier - 1
        w.write(1, 4); // rangebits
        w.write(0, 4); // X_list[2]
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 4096];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        assert_eq!(
            Floor1Config::parse(&mut br, &mut arena, 2).unwrap_err(),
            DecodeError::Malformed
        );
    }

    #[test]
    fn subclass_book_out_of_range_is_malformed() {
        let mut w = BitWriter::new();
        w.write(1, 5); // partitions = 1
        w.write(0, 4); // partition_class_list[0] = 0
        w.write(0, 3); // dim 1
        w.write(0, 2); // subclasses = 0 => no masterbook, 1 subclass book
        w.write(10, 8); // subclass_books[0][0] raw = 10 -> 9, but codebook_count=2 => out of range
        w.write(0, 2); // multiplier - 1
        w.write(1, 4); // rangebits
        w.write(0, 4); // X_list[2]
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 4096];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        assert_eq!(
            Floor1Config::parse(&mut br, &mut arena, 2).unwrap_err(),
            DecodeError::Malformed
        );
    }

    #[test]
    fn no_book_sentinel_minus_one_is_accepted() {
        // subclass_books raw byte 0 => -1 ("no book"), always valid regardless
        // of codebook_count.
        let mut w = BitWriter::new();
        w.write(1, 5);
        w.write(0, 4);
        w.write(0, 3);
        w.write(0, 2); // subclasses = 0 => 1 book slot
        w.write(0, 8); // raw 0 -> -1
        w.write(0, 2); // multiplier - 1 = 0 => 1
        w.write(1, 4); // rangebits = 1 => X_list[1] = 2
        w.write(1, 4); // X_list[2]'s 1-bit read = 1 (must differ from X_list[0] = 0)
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 4096];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        let cfg = Floor1Config::parse(&mut br, &mut arena, 1).expect("valid: -1 is always ok");
        assert_eq!(cfg.subclass_book(&storage, 0, 0), -1);
    }

    #[test]
    fn over_65_values_is_malformed() {
        // 31 partitions (the max), each of class 0 with dim 8 (the max):
        // floor1_values = 2 + 31*8 = 250 > 65.
        let mut w = BitWriter::new();
        w.write(31, 5); // partitions = 31
        for _ in 0..31 {
            w.write(0, 4); // all class 0
        }
        w.write(7, 3); // class_dimensions[0] - 1 = 7 => dim 8
        w.write(0, 2); // subclasses = 0 => 1 book slot
        w.write(0, 8); // subclass_books[0][0] raw 0 -> -1
        w.write(0, 2); // multiplier
        w.write(1, 4); // rangebits
        // No X_list reads needed: the >65 check fires before reading them.
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 4096];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        assert_eq!(
            Floor1Config::parse(&mut br, &mut arena, 1).unwrap_err(),
            DecodeError::Malformed
        );
    }

    #[test]
    fn duplicate_x_values_are_malformed() {
        // 2 partitions, class 0 dim 1 each => floor1_values = 4: [0, 2^rb, a, b].
        // Make the two read X values collide with each other.
        let mut w = BitWriter::new();
        w.write(2, 5); // partitions = 2
        w.write(0, 4);
        w.write(0, 4);
        w.write(0, 3); // dim 1
        w.write(0, 2); // subclasses 0
        w.write(0, 8); // subclass book raw 0 -> -1
        w.write(0, 2); // multiplier
        w.write(4, 4); // rangebits = 4
        w.write(5, 4); // X_list[2] = 5
        w.write(5, 4); // X_list[3] = 5 (duplicate)
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 4096];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        assert_eq!(
            Floor1Config::parse(&mut br, &mut arena, 1).unwrap_err(),
            DecodeError::Malformed
        );
    }

    #[test]
    fn zero_partitions_yields_zero_classes_and_two_values() {
        // partitions = 0 => no class loop iterations, no X_list reads beyond
        // the two implicit entries.
        let mut w = BitWriter::new();
        w.write(0, 5); // partitions = 0
        w.write(0, 2); // multiplier - 1
        w.write(3, 4); // rangebits
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 4096];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        let cfg = Floor1Config::parse(&mut br, &mut arena, 1).expect("valid: no partitions");
        assert_eq!(cfg.class_count, 0);
        assert_eq!(cfg.n_values, 2);
        assert_eq!(cfg.x_value(&storage, 0), 0);
        assert_eq!(cfg.x_value(&storage, 1), 1 << 3);
    }

    // --- floor 1 packet decode + curve synthesis (Task 6) ----------------

    /// The dB→linear table is monotonic increasing and spans (near-0, 1.0], so
    /// a larger integer floor Y always yields a larger linear amplitude.
    #[test]
    fn inverse_db_table_is_monotonic_and_bounded() {
        // `.first()`/`.last()` (rather than `[0]`/`[255]`) keep these runtime
        // assertions, not const-folded ones.
        let first = *FLOOR1_INVERSE_DB.first().unwrap();
        let last = *FLOOR1_INVERSE_DB.last().unwrap();
        assert_eq!(last, 1.0);
        assert!(first > 0.0 && first < 1.0e-6);
        for w in FLOOR1_INVERSE_DB.windows(2) {
            assert!(w[1] > w[0], "table must be strictly increasing");
        }
    }

    /// A partitions=0 floor with the leading `nonzero` bit clear decodes as
    /// "unused": `floor1_decode` returns `false` and zeroes the whole curve
    /// (no codebook reads, so an empty codebook slice suffices).
    #[test]
    fn floor1_decode_unused_channel_is_false_and_zero() {
        let mut w = BitWriter::new();
        w.write(0, 5); // partitions = 0
        w.write(0, 2); // multiplier - 1 = 0 => 1
        w.write(4, 4); // rangebits = 4
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 256];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        let cfg = Floor1Config::parse(&mut br, &mut arena, 1).unwrap();

        let mut pw = BitWriter::new();
        pw.write(0, 1); // nonzero = 0 => unused
        let pbytes = pw.into_bytes();
        let mut pbr = BitReader::new(&pbytes);
        let mut out = [7.0f32; 32]; // pre-filled non-zero to prove it gets cleared
        let used = super::floor1_decode(&cfg, &[], arena.bytes(), &mut pbr, 32, &mut out).unwrap();
        assert!(!used);
        assert!(out.iter().all(|&v| v == 0.0));
    }

    /// A partitions=0 floor with `nonzero` set renders a real, finite, positive,
    /// non-trivial curve; a high `Y[0]` and lower `Y[1]` produce a louder start
    /// than tail (both dB values map through the increasing table).
    #[test]
    fn floor1_decode_used_channel_renders_nonzero_curve() {
        let mut w = BitWriter::new();
        w.write(0, 5); // partitions = 0
        w.write(0, 2); // multiplier - 1 = 0 => 1 (range 256 => 8-bit Y)
        w.write(4, 4); // rangebits = 4 => X_list[1] = 16
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 256];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        let cfg = Floor1Config::parse(&mut br, &mut arena, 1).unwrap();

        let mut pw = BitWriter::new();
        pw.write(1, 1); // nonzero = 1
        pw.write(200, 8); // Y[0] (loud)
        pw.write(40, 8); // Y[1] (quiet)
        let pbytes = pw.into_bytes();
        let mut pbr = BitReader::new(&pbytes);
        let n = 64;
        let mut out = [0.0f32; 64];
        let used = super::floor1_decode(&cfg, &[], arena.bytes(), &mut pbr, n, &mut out).unwrap();
        assert!(used);
        assert!(out.iter().all(|&v| v.is_finite() && v >= 0.0));
        assert!(out.iter().any(|&v| v > 0.0), "curve must be non-trivial");
        // X_list[1] = 16, so [16, n) is the flat tail at Y[1]; out[0] (at Y[0])
        // must be louder.
        assert!(out[0] > out[n - 1]);
        assert_eq!(out[n - 1], FLOOR1_INVERSE_DB[40]);
    }
}
