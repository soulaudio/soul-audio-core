//! Vorbis setup-header parse (spec §4.2.4) — codebooks (Task 4), then floors/
//! residues/mappings/modes (this task).
//!
//! The setup header is packet 3 of the three Vorbis header packets. Its common
//! preamble is a packet-type byte + the `"vorbis"` magic (shared with the
//! identification and comment headers, spec §4.2.1); after that it declares a
//! codebook count and that many packed codebooks, each parsed by
//! [`Codebook::parse`] into the caller's [`Arena`] ([`parse_codebooks`]).
//!
//! What follows the codebooks, in exact spec order, is:
//!
//! 1. **Time-domain transforms** — a vestigial section every conforming
//!    encoder still emits (a count + that many 16-bit fields, each required
//!    to be `0`). It configures nothing a decoder uses, but the bits **must**
//!    be consumed or every later field misaligns; [`parse_setup_tail`] reads
//!    and validates it inline (not its own type, since it produces no
//!    config).
//! 2. **Floors** ([`parse_floors`], spec §7.2 dispatch + §7.2.1 floor-1 body
//!    in [`super::floor`]) — floor 0 is rejected as
//!    [`DecodeError::Unsupported`] before it can consume the rest of the
//!    packet as garbage.
//! 3. **Residues** ([`parse_residues`], spec §8.6.1 in [`super::residue`]).
//! 4. **Mappings** ([`parse_mappings`], spec §4.2.4 below) — submap→floor/
//!    residue assignment and stereo-coupling `(magnitude, angle)` pairs.
//! 5. **Modes** ([`parse_modes`], spec §4.2.4) — `blockflag` + a mapping
//!    reference; window/transform type are read-and-must-be-zero, so they
//!    carry no state worth storing.
//! 6. **The final framing bit**, checked by [`parse_setup_tail`] itself.

use crate::decoder::DecodeError;

use super::arena::Arena;
use super::bitread::{BitReader, ilog};
use super::codebook::Codebook;
use super::floor::Floor1Config;
use super::residue::ResidueConfig;

/// Spec ceiling on codebooks in one setup header: the count field is 8 bits
/// read as `+ 1`, so `1..=256`. Sizing the decoder's codebook array by this is
/// exact, not a heuristic cap — a conforming stream never exceeds it. (Total
/// codebook *memory* is bounded separately, and gracefully, by the arena.)
pub const MAX_CODEBOOKS: usize = 256;

/// Packet-type byte that opens the setup header (spec §4.2.4).
pub const SETUP_HEADER_TYPE: u8 = 5;

/// Packet-type byte that opens the comment header (spec §4.2.3).
pub const COMMENT_HEADER_TYPE: u8 = 3;

/// Validate a Vorbis header packet's common preamble: the packet-type byte
/// (`expected_type`) followed by the `"vorbis"` magic (spec §4.2.1). Both are
/// byte-aligned, but read through [`BitReader`] like every other field.
///
/// # Errors
/// [`DecodeError::Malformed`] on a wrong type byte or wrong magic;
/// [`DecodeError::UnexpectedEof`] if fewer than 7 bytes are available.
pub fn read_common_header(br: &mut BitReader, expected_type: u8) -> Result<(), DecodeError> {
    if br.read_bits(8)? as u8 != expected_type {
        return Err(DecodeError::Malformed);
    }
    for &c in b"vorbis" {
        if br.read_bits(8)? as u8 != c {
            return Err(DecodeError::Malformed);
        }
    }
    Ok(())
}

/// Parse the codebook section of the setup header: the common preamble, the
/// codebook count, and that many packed codebooks (laid into `arena`). Fills
/// `out[..count]` and returns `count`.
///
/// The caller (see [`parse_setup_tail`]) is responsible for continuing the
/// parse over the same `br`/`arena` for the sections that follow (time
/// transforms, floors, residues, mappings, modes, framing bit) — this
/// function stops right after the last codebook.
///
/// # Errors
/// Propagates [`read_common_header`] and [`Codebook::parse`] errors
/// (malformed/truncated header, arena over-cap).
pub fn parse_codebooks(
    br: &mut BitReader,
    arena: &mut Arena,
    out: &mut [Codebook; MAX_CODEBOOKS],
) -> Result<usize, DecodeError> {
    read_common_header(br, SETUP_HEADER_TYPE)?;
    // `+ 1`, so 1..=256 — always fits `out` (MAX_CODEBOOKS).
    let count = br.read_bits(8)? as usize + 1;
    for slot in out.iter_mut().take(count) {
        *slot = Codebook::parse(br, arena)?;
    }
    Ok(count)
}

// --- floors, residues, mappings, modes (spec §7.2/§8.6.1/§4.2.4) ----------

/// Heuristic cap on the floor count: the count field is 6 bits `+ 1` (spec
/// ceiling `1..=64`), but a real encoder never remotely approaches even this
/// lower bound (a real ffmpeg/libvorbis stereo encode uses 2). Picked well
/// below the spec ceiling so a hand-built "over-cap" setup packet can declare
/// a spec-legal count that still exceeds it.
pub const MAX_FLOORS: usize = 32;

/// Heuristic cap on the residue count — see [`MAX_FLOORS`]' reasoning
/// (same `1..=64` spec ceiling, same real-world count of 2).
pub const MAX_RESIDUES: usize = 32;

/// Heuristic cap on the mapping count — see [`MAX_FLOORS`]' reasoning.
pub const MAX_MAPPINGS: usize = 16;

/// Heuristic cap on the mode count — see [`MAX_FLOORS`]' reasoning (a real
/// encode uses 2: one long-block mode, one short-block mode).
pub const MAX_MODES: usize = 16;

/// Heuristic cap on `mapping_coupling_steps`: the field is 8 bits `+ 1`
/// (spec ceiling `1..=256`, or `0` if the "has coupling" flag is clear); a
/// real stereo encode uses at most 1 (the single L/R pair). Picked well below
/// the spec ceiling for the same testability reason as [`MAX_FLOORS`].
pub const MAX_COUPLING_STEPS: usize = 16;

/// Spec ceiling on `mapping_submaps`: the field is 4 bits `+ 1` (or `1` if
/// the "has submaps" flag is clear), so `1..=16` — exact, not a heuristic.
pub const MAX_SUBMAPS: usize = 16;

/// A parsed channel-mapping configuration (spec §4.2.4): small, `Copy`, and
/// free of self-references — the coupling-pair, channel-mux, and per-submap
/// floor/residue tables live in the [`Arena`] at the recorded byte offsets.
#[derive(Clone, Copy, Debug)]
pub struct MappingConfig {
    /// `mapping_submaps` (`1..=16`).
    pub submaps: u8,
    /// `mapping_coupling_steps` (`0..=MAX_COUPLING_STEPS`).
    pub coupling_steps: u8,
    /// Offset of the coupling-pair table: `(u8 magnitude, u8 angle)` pairs,
    /// interleaved, `2 * `[`Self::coupling_steps`]` bytes total.
    coupling_off: u32,
    /// Offset of the per-channel submap-index table (`u8` × `channels`;
    /// all-zero when [`Self::submaps`] `== 1`, since no mux bits are read).
    mux_off: u32,
    /// Offset of the per-submap floor-index table (`u8` × [`Self::submaps`]).
    submap_floor_off: u32,
    /// Offset of the per-submap residue-index table (`u8` × [`Self::submaps`]).
    submap_residue_off: u32,
}

impl MappingConfig {
    /// The zero value, for initializing fixed mapping arrays before parse
    /// fills the live prefix (mirrors [`Codebook::EMPTY`](super::codebook::Codebook::EMPTY)).
    pub const EMPTY: MappingConfig = MappingConfig {
        submaps: 0,
        coupling_steps: 0,
        coupling_off: 0,
        mux_off: 0,
        submap_floor_off: 0,
        submap_residue_off: 0,
    };

    /// Parse one mapping configuration (spec §4.2.4), including its own
    /// leading `mapping_type` field (must be `0` — the only type Vorbis I
    /// defines).
    ///
    /// `channels` is the stream's channel count (from the identification
    /// header) and bounds both the coupling-pair channel indices and the
    /// per-channel mux table; `floor_count`/`residue_count` bound the
    /// per-submap floor/residue indices.
    ///
    /// # Errors
    /// [`DecodeError::Malformed`] for a nonzero `mapping_type`, a nonzero
    /// reserved field, a coupling pair with `magnitude == angle` or either
    /// index `>= channels`, an out-of-range mux/floor/residue index.
    /// [`DecodeError::Unsupported`] if `coupling_steps` exceeds
    /// [`MAX_COUPLING_STEPS`] or the arena cannot fit the tables.
    /// [`DecodeError::UnexpectedEof`] if the packet ends mid-mapping.
    pub fn parse(
        br: &mut BitReader,
        arena: &mut Arena,
        channels: u16,
        floor_count: usize,
        residue_count: usize,
    ) -> Result<MappingConfig, DecodeError> {
        if br.read_bits(16)? != 0 {
            return Err(DecodeError::Malformed); // only mapping type 0 is defined
        }
        let submaps = if br.read_bit()? {
            br.read_bits(4)? as u8 + 1
        } else {
            1
        };
        let coupling_steps = if br.read_bit()? {
            let n = br.read_bits(8)? as usize + 1;
            if n > MAX_COUPLING_STEPS {
                return Err(DecodeError::Unsupported);
            }
            n as u8
        } else {
            0
        };

        let coupling_off = arena.alloc(coupling_steps as usize * 2, 1)? as u32;
        // audio_chan_ilog (spec): the bit width of each coupling channel
        // field, derived from the channel count. `channels >= 1` always
        // (validated by the identification-header parse), so `channels - 1`
        // never underflows; ilog(0) == 0, so a mono stream reads 0-bit
        // (always-0) fields, which then always fails the `mag != ang` check
        // below if any coupling step is declared — correctly rejecting a
        // nonsensical mono-coupled setup rather than silently truncating.
        let chan_ilog = ilog(u32::from(channels) - 1);
        for i in 0..coupling_steps as usize {
            let mag = br.read_bits(chan_ilog)? as u8;
            let ang = br.read_bits(chan_ilog)? as u8;
            if mag == ang || u16::from(mag) >= channels || u16::from(ang) >= channels {
                return Err(DecodeError::Malformed);
            }
            let buf = arena.bytes_mut();
            buf[coupling_off as usize + i * 2] = mag;
            buf[coupling_off as usize + i * 2 + 1] = ang;
        }

        if br.read_bits(2)? != 0 {
            return Err(DecodeError::Malformed); // reserved
        }

        let mux_off = arena.alloc(channels as usize, 1)? as u32;
        if submaps > 1 {
            for ch in 0..channels as usize {
                let v = br.read_bits(4)? as u8;
                if v >= submaps {
                    return Err(DecodeError::Malformed);
                }
                arena.bytes_mut()[mux_off as usize + ch] = v;
            }
        } else {
            for ch in 0..channels as usize {
                arena.bytes_mut()[mux_off as usize + ch] = 0;
            }
        }

        let submap_floor_off = arena.alloc(submaps as usize, 1)? as u32;
        let submap_residue_off = arena.alloc(submaps as usize, 1)? as u32;
        for s in 0..submaps as usize {
            let _reserved = br.read_bits(8)?; // vestigial "time domain transform" index
            let floor_idx = br.read_bits(8)? as usize;
            let residue_idx = br.read_bits(8)? as usize;
            if floor_idx >= floor_count || residue_idx >= residue_count {
                return Err(DecodeError::Malformed);
            }
            arena.bytes_mut()[submap_floor_off as usize + s] = floor_idx as u8;
            arena.bytes_mut()[submap_residue_off as usize + s] = residue_idx as u8;
        }

        Ok(MappingConfig {
            submaps,
            coupling_steps,
            coupling_off,
            mux_off,
            submap_floor_off,
            submap_residue_off,
        })
    }

    /// `(magnitude, angle)` channel indices of coupling step `i`
    /// (`i < self.coupling_steps`).
    pub fn coupling(&self, storage: &[u8], i: usize) -> (u8, u8) {
        let off = self.coupling_off as usize + i * 2;
        (storage[off], storage[off + 1])
    }

    /// The submap index channel `ch` reads from (`ch < channels`).
    pub fn channel_submap(&self, storage: &[u8], ch: usize) -> u8 {
        storage[self.mux_off as usize + ch]
    }

    /// The floor-config index submap `s` uses (`s < self.submaps`).
    pub fn submap_floor(&self, storage: &[u8], s: usize) -> u8 {
        storage[self.submap_floor_off as usize + s]
    }

    /// The residue-config index submap `s` uses (`s < self.submaps`).
    pub fn submap_residue(&self, storage: &[u8], s: usize) -> u8 {
        storage[self.submap_residue_off as usize + s]
    }
}

/// A parsed mode configuration (spec §4.2.4): `blockflag` selects the long
/// vs. short block size, and `mapping` selects which [`MappingConfig`] this
/// mode uses. `windowtype`/`transformtype` are read-and-validated (must be
/// `0`, the only values Vorbis I defines) but carry no further state, so —
/// per the task interface — this struct stores only the two fields that do.
#[derive(Clone, Copy, Debug)]
pub struct ModeConfig {
    /// `mode_blockflag`: `false` selects the short block size, `true` the
    /// long one.
    pub blockflag: bool,
    /// Index into the stream's mapping list (`< mapping_count`).
    pub mapping: u8,
}

impl ModeConfig {
    /// The zero value, for initializing fixed mode arrays before parse fills
    /// the live prefix (mirrors [`Codebook::EMPTY`](super::codebook::Codebook::EMPTY)).
    pub const EMPTY: ModeConfig = ModeConfig {
        blockflag: false,
        mapping: 0,
    };

    /// Parse one mode configuration (spec §4.2.4).
    ///
    /// # Errors
    /// [`DecodeError::Malformed`] for a nonzero `windowtype`/`transformtype`
    /// or a `mapping` index `>= mapping_count`.
    /// [`DecodeError::UnexpectedEof`] if the packet ends mid-mode.
    pub fn parse(br: &mut BitReader, mapping_count: usize) -> Result<ModeConfig, DecodeError> {
        let blockflag = br.read_bit()?;
        let windowtype = br.read_bits(16)?;
        let transformtype = br.read_bits(16)?;
        let mapping = br.read_bits(8)? as usize;
        if windowtype != 0 || transformtype != 0 || mapping >= mapping_count {
            return Err(DecodeError::Malformed);
        }
        Ok(ModeConfig {
            blockflag,
            mapping: mapping as u8,
        })
    }
}

/// Parse the floor-count field and that many floor configurations (spec
/// §7.2): each floor opens with a 16-bit `floor_type` — `0` is rejected
/// outright ([`DecodeError::Unsupported`], floor 0 is not implemented), `1`
/// dispatches to [`Floor1Config::parse`], anything else is
/// [`DecodeError::Malformed`] (a reserved/future type). Fills `out[..count]`
/// and returns `count`.
///
/// # Errors
/// [`DecodeError::Unsupported`] if the declared count exceeds [`MAX_FLOORS`],
/// floor 0 is used, or the arena cannot fit a floor's tables.
/// Propagates [`Floor1Config::parse`]'s other errors.
pub fn parse_floors(
    br: &mut BitReader,
    arena: &mut Arena,
    codebook_count: usize,
    out: &mut [Floor1Config; MAX_FLOORS],
) -> Result<usize, DecodeError> {
    let count = br.read_bits(6)? as usize + 1;
    if count > MAX_FLOORS {
        return Err(DecodeError::Unsupported);
    }
    for slot in out.iter_mut().take(count) {
        let floor_type = br.read_bits(16)?;
        *slot = match floor_type {
            0 => return Err(DecodeError::Unsupported), // floor 0: not implemented
            1 => Floor1Config::parse(br, arena, codebook_count)?,
            _ => return Err(DecodeError::Malformed), // reserved
        };
    }
    Ok(count)
}

/// Parse the residue-count field and that many residue configurations (spec
/// §8.6.1, [`ResidueConfig::parse`]). Fills `out[..count]` and returns
/// `count`.
///
/// # Errors
/// [`DecodeError::Unsupported`] if the declared count exceeds
/// [`MAX_RESIDUES`]. Propagates [`ResidueConfig::parse`]'s errors.
pub fn parse_residues(
    br: &mut BitReader,
    arena: &mut Arena,
    codebook_count: usize,
    out: &mut [ResidueConfig; MAX_RESIDUES],
) -> Result<usize, DecodeError> {
    let count = br.read_bits(6)? as usize + 1;
    if count > MAX_RESIDUES {
        return Err(DecodeError::Unsupported);
    }
    for slot in out.iter_mut().take(count) {
        *slot = ResidueConfig::parse(br, arena, codebook_count)?;
    }
    Ok(count)
}

/// Parse the mapping-count field and that many mapping configurations (spec
/// §4.2.4, [`MappingConfig::parse`]). Fills `out[..count]` and returns
/// `count`.
///
/// # Errors
/// [`DecodeError::Unsupported`] if the declared count exceeds
/// [`MAX_MAPPINGS`]. Propagates [`MappingConfig::parse`]'s errors.
pub fn parse_mappings(
    br: &mut BitReader,
    arena: &mut Arena,
    channels: u16,
    floor_count: usize,
    residue_count: usize,
    out: &mut [MappingConfig; MAX_MAPPINGS],
) -> Result<usize, DecodeError> {
    let count = br.read_bits(6)? as usize + 1;
    if count > MAX_MAPPINGS {
        return Err(DecodeError::Unsupported);
    }
    for slot in out.iter_mut().take(count) {
        *slot = MappingConfig::parse(br, arena, channels, floor_count, residue_count)?;
    }
    Ok(count)
}

/// Parse the mode-count field and that many mode configurations (spec
/// §4.2.4, [`ModeConfig::parse`]). Fills `out[..count]` and returns `count`.
///
/// # Errors
/// [`DecodeError::Unsupported`] if the declared count exceeds [`MAX_MODES`].
/// Propagates [`ModeConfig::parse`]'s errors.
pub fn parse_modes(
    br: &mut BitReader,
    mapping_count: usize,
    out: &mut [ModeConfig; MAX_MODES],
) -> Result<usize, DecodeError> {
    let count = br.read_bits(6)? as usize + 1;
    if count > MAX_MODES {
        return Err(DecodeError::Unsupported);
    }
    for slot in out.iter_mut().take(count) {
        *slot = ModeConfig::parse(br, mapping_count)?;
    }
    Ok(count)
}

/// Parse everything in the setup header that follows the codebooks (see the
/// module docs for the full section order): the vestigial time-domain-
/// transform placeholder section, floors, residues, mappings, modes, and the
/// final framing bit.
///
/// `channels` is the stream's channel count (identification header), needed
/// for mapping coupling/mux validation.
///
/// # Errors
/// [`DecodeError::Malformed`] for a nonzero time-domain-transform placeholder
/// or a missing/clear final framing bit. Propagates every error from
/// [`parse_floors`], [`parse_residues`], [`parse_mappings`], [`parse_modes`].
#[allow(clippy::too_many_arguments)]
pub fn parse_setup_tail(
    br: &mut BitReader,
    arena: &mut Arena,
    codebook_count: usize,
    channels: u16,
    floors_out: &mut [Floor1Config; MAX_FLOORS],
    residues_out: &mut [ResidueConfig; MAX_RESIDUES],
    mappings_out: &mut [MappingConfig; MAX_MAPPINGS],
    modes_out: &mut [ModeConfig; MAX_MODES],
) -> Result<(usize, usize, usize, usize), DecodeError> {
    // Vestigial "time domain transforms" section: every conforming encoder
    // still emits a count + that many 16-bit fields, each required to be 0.
    // Nothing here configures any decoder behavior, but the bits must be
    // consumed or every field after this misaligns.
    let time_count = br.read_bits(6)? as usize + 1;
    for _ in 0..time_count {
        if br.read_bits(16)? != 0 {
            return Err(DecodeError::Malformed);
        }
    }

    let floor_count = parse_floors(br, arena, codebook_count, floors_out)?;
    let residue_count = parse_residues(br, arena, codebook_count, residues_out)?;
    let mapping_count = parse_mappings(
        br,
        arena,
        channels,
        floor_count,
        residue_count,
        mappings_out,
    )?;
    let mode_count = parse_modes(br, mapping_count, modes_out)?;

    if !br.read_bit()? {
        return Err(DecodeError::Malformed); // final framing bit
    }

    Ok((floor_count, residue_count, mapping_count, mode_count))
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

    /// The Step-1 TDD fixture (task brief): 1 floor-1, 1 residue, 1 mapping
    /// with one coupled pair, 2 modes — for a stereo (`channels = 2`) stream,
    /// with `codebook_count = 4` so every codebook cross-reference below is
    /// in range. Assumes codebooks have already been parsed (this bitstream
    /// starts at the time-domain-transform section, i.e. exactly where
    /// [`parse_setup_tail`] itself starts reading).
    fn write_full_tail(w: &mut BitWriter) {
        write_full_tail_with_framing(w, 1);
    }

    /// Same fixture as [`write_full_tail`], but with the final framing bit
    /// set to `framing` — lets [`missing_framing_bit_is_malformed`] reuse the
    /// exact same field layout with just that one bit flipped, instead of a
    /// hand-duplicated (and driftable) copy.
    fn write_full_tail_with_framing(w: &mut BitWriter, framing: u32) {
        // time-domain transforms: 1 entry, placeholder 0.
        w.write(0, 6);
        w.write(0, 16);

        // floor: 1 floor-1 (same shape as floor.rs's own worked fixture).
        w.write(0, 6); // floor_count - 1 = 0 => 1
        w.write(1, 16); // floor_type = 1
        w.write(2, 5); // partitions = 2
        w.write(0, 4); // partition_class_list[0] = 0
        w.write(0, 4); // partition_class_list[1] = 0
        w.write(1, 3); // class_dimensions[0] - 1 = 1 => dim 2
        w.write(1, 2); // class_subclasses[0] = 1 => 2 subclass books
        w.write(0, 8); // class_masterbooks[0] = 0
        w.write(1, 8); // subclass_books[0][0] raw = 1 => book 0
        w.write(2, 8); // subclass_books[0][1] raw = 2 => book 1
        w.write(1, 2); // multiplier - 1 = 1 => multiplier 2
        w.write(4, 4); // rangebits = 4
        w.write(3, 4); // X_list[2]
        w.write(7, 4); // X_list[3]
        w.write(1, 4); // X_list[4]
        w.write(11, 4); // X_list[5]

        // residue: 1 residue, type 2 (same shape as residue.rs's own fixture).
        w.write(0, 6); // residue_count - 1 = 0 => 1
        w.write(2, 16); // residue_type = 2
        w.write(0, 24); // begin
        w.write(128, 24); // end
        w.write(15, 24); // partition_size - 1 = 15 => 16
        w.write(1, 6); // classifications - 1 = 1 => 2
        w.write(0, 8); // classbook = 0
        w.write(1, 3); // classification 0: low = 1
        w.write(0, 1); // classification 0: no high bits => cascade 1
        w.write(1, 3); // classification 1: low = 1
        w.write(1, 1); // classification 1: has high bits
        w.write(1, 5); // classification 1: high = 1 => cascade 9
        w.write(1, 8); // classification0 stage0 book = 1
        w.write(2, 8); // classification1 stage0 book = 2
        w.write(3, 8); // classification1 stage3 book = 3

        // mapping: 1 mapping, submaps = 1, ONE coupled pair (mag=0, ang=1).
        w.write(0, 6); // mapping_count - 1 = 0 => 1
        w.write(0, 16); // mapping_type = 0
        w.write(0, 1); // has_submaps = 0 => submaps = 1
        w.write(1, 1); // has_coupling = 1
        w.write(0, 8); // coupling_steps - 1 = 0 => 1
        // audio_chan_ilog = ilog(channels - 1) = ilog(1) = 1 bit per index.
        w.write(0, 1); // magnitude channel = 0
        w.write(1, 1); // angle channel = 1
        w.write(0, 2); // reserved
        // submaps == 1: no per-channel mux bits.
        w.write(0, 8); // submap[0] reserved
        w.write(0, 8); // submap[0] floor index = 0
        w.write(0, 8); // submap[0] residue index = 0

        // modes: 2 modes — short (blockflag 0) then long (blockflag 1).
        w.write(1, 6); // mode_count - 1 = 1 => 2
        w.write(0, 1); // mode[0] blockflag = 0 (short)
        w.write(0, 16);
        w.write(0, 16);
        w.write(0, 8); // mode[0] mapping = 0
        w.write(1, 1); // mode[1] blockflag = 1 (long)
        w.write(0, 16);
        w.write(0, 16);
        w.write(0, 8); // mode[1] mapping = 0

        w.write(framing, 1); // final framing bit
    }

    fn empty_outs() -> (
        [Floor1Config; MAX_FLOORS],
        [ResidueConfig; MAX_RESIDUES],
        [MappingConfig; MAX_MAPPINGS],
        [ModeConfig; MAX_MODES],
    ) {
        (
            [Floor1Config::EMPTY; MAX_FLOORS],
            [ResidueConfig::EMPTY; MAX_RESIDUES],
            [MappingConfig::EMPTY; MAX_MAPPINGS],
            [ModeConfig::EMPTY; MAX_MODES],
        )
    }

    #[test]
    fn parses_the_step1_fixture_into_expected_config_values() {
        let mut w = BitWriter::new();
        write_full_tail(&mut w);
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 8192];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        let (mut floors, mut residues, mut mappings, mut modes) = empty_outs();

        let (floor_count, residue_count, mapping_count, mode_count) = parse_setup_tail(
            &mut br,
            &mut arena,
            4, // codebook_count
            2, // channels
            &mut floors,
            &mut residues,
            &mut mappings,
            &mut modes,
        )
        .expect("valid full-tail setup");

        assert_eq!(floor_count, 1);
        assert_eq!(residue_count, 1);
        assert_eq!(mapping_count, 1);
        assert_eq!(mode_count, 2);

        let f = floors[0];
        assert_eq!(f.partitions, 2);
        assert_eq!(f.class_count, 1);
        assert_eq!(f.multiplier, 2);
        assert_eq!(f.rangebits, 4);
        assert_eq!(f.n_values, 6);
        assert_eq!(f.x_value(arena.bytes(), 0), 0);
        assert_eq!(f.x_value(arena.bytes(), 1), 16);
        assert_eq!(f.x_value(arena.bytes(), 5), 11);

        let r = residues[0];
        assert_eq!(r.residue_type, 2);
        assert_eq!(r.begin, 0);
        assert_eq!(r.end, 128);
        assert_eq!(r.partition_size, 16);
        assert_eq!(r.classifications, 2);
        assert_eq!(r.classbook, 0);
        assert_eq!(r.cascade(arena.bytes(), 0), 1);
        assert_eq!(r.cascade(arena.bytes(), 1), 9);
        assert_eq!(r.book(arena.bytes(), 1, 3), 3);

        let m = mappings[0];
        assert_eq!(m.submaps, 1);
        assert_eq!(m.coupling_steps, 1);
        assert_eq!(m.coupling(arena.bytes(), 0), (0, 1));
        assert_eq!(m.submap_floor(arena.bytes(), 0), 0);
        assert_eq!(m.submap_residue(arena.bytes(), 0), 0);

        assert!(!modes[0].blockflag);
        assert_eq!(modes[0].mapping, 0);
        assert!(modes[1].blockflag);
        assert_eq!(modes[1].mapping, 0);
    }

    #[test]
    fn floor_type_zero_is_unsupported() {
        let mut w = BitWriter::new();
        w.write(0, 6); // floor_count - 1 = 0 => 1
        w.write(0, 16); // floor_type = 0
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 256];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        let mut floors = [Floor1Config::EMPTY; MAX_FLOORS];
        assert_eq!(
            parse_floors(&mut br, &mut arena, 4, &mut floors).unwrap_err(),
            DecodeError::Unsupported
        );
    }

    #[test]
    fn reserved_floor_type_is_malformed() {
        let mut w = BitWriter::new();
        w.write(0, 6);
        w.write(2, 16); // floor_type = 2 (reserved)
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 256];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        let mut floors = [Floor1Config::EMPTY; MAX_FLOORS];
        assert_eq!(
            parse_floors(&mut br, &mut arena, 4, &mut floors).unwrap_err(),
            DecodeError::Malformed
        );
    }

    #[test]
    fn floor_count_over_cap_is_unsupported() {
        let mut w = BitWriter::new();
        w.write(40, 6); // count - 1 = 40 => 41: spec-legal (<=64), over MAX_FLOORS (32)
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 256];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        let mut floors = [Floor1Config::EMPTY; MAX_FLOORS];
        assert_eq!(
            parse_floors(&mut br, &mut arena, 4, &mut floors).unwrap_err(),
            DecodeError::Unsupported
        );
    }

    #[test]
    fn residue_count_over_cap_is_unsupported() {
        let mut w = BitWriter::new();
        w.write(40, 6);
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 256];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        let mut residues = [ResidueConfig::EMPTY; MAX_RESIDUES];
        assert_eq!(
            parse_residues(&mut br, &mut arena, 4, &mut residues).unwrap_err(),
            DecodeError::Unsupported
        );
    }

    #[test]
    fn mapping_count_over_cap_is_unsupported() {
        let mut w = BitWriter::new();
        w.write(20, 6); // count - 1 = 20 => 21: spec-legal, over MAX_MAPPINGS (16)
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 256];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        let mut mappings = [MappingConfig::EMPTY; MAX_MAPPINGS];
        assert_eq!(
            parse_mappings(&mut br, &mut arena, 2, 1, 1, &mut mappings).unwrap_err(),
            DecodeError::Unsupported
        );
    }

    #[test]
    fn mode_count_over_cap_is_unsupported() {
        let mut w = BitWriter::new();
        w.write(20, 6); // over MAX_MODES (16)
        let bytes = w.into_bytes();
        let mut br = BitReader::new(&bytes);
        let mut modes = [ModeConfig::EMPTY; MAX_MODES];
        assert_eq!(
            parse_modes(&mut br, 1, &mut modes).unwrap_err(),
            DecodeError::Unsupported
        );
    }

    #[test]
    fn coupling_pair_with_equal_channels_is_malformed() {
        let mut w = BitWriter::new();
        w.write(0, 16); // mapping_type = 0
        w.write(0, 1); // submaps = 1
        w.write(1, 1); // has_coupling
        w.write(0, 8); // coupling_steps - 1 = 0 => 1
        w.write(1, 1); // mag = 1
        w.write(1, 1); // ang = 1 (equal to mag)
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 256];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        assert_eq!(
            MappingConfig::parse(&mut br, &mut arena, 2, 1, 1).unwrap_err(),
            DecodeError::Malformed
        );
    }

    #[test]
    fn coupling_pair_channel_out_of_range_is_malformed() {
        // channels = 2, so chan_ilog = ilog(1) = 1 bit; a channel index can
        // only ever encode 0 or 1 with 1 bit, so this instead exercises
        // channels = 3 (chan_ilog = ilog(2) = 2 bits) with an index of 3,
        // which is in-range for the bit width but not for the channel count.
        let mut w = BitWriter::new();
        w.write(0, 16);
        w.write(0, 1);
        w.write(1, 1);
        w.write(0, 8); // coupling_steps = 1
        w.write(3, 2); // mag = 3 (>= channels = 3)
        w.write(0, 2); // ang = 0
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 256];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        assert_eq!(
            MappingConfig::parse(&mut br, &mut arena, 3, 1, 1).unwrap_err(),
            DecodeError::Malformed
        );
    }

    #[test]
    fn mode_nonzero_windowtype_is_malformed() {
        let mut w = BitWriter::new();
        w.write(0, 1); // blockflag
        w.write(1, 16); // windowtype = 1 (must be 0)
        w.write(0, 16);
        w.write(0, 8);
        let bytes = w.into_bytes();
        let mut br = BitReader::new(&bytes);
        assert_eq!(
            ModeConfig::parse(&mut br, 1).unwrap_err(),
            DecodeError::Malformed
        );
    }

    #[test]
    fn mode_mapping_out_of_range_is_malformed() {
        let mut w = BitWriter::new();
        w.write(0, 1);
        w.write(0, 16);
        w.write(0, 16);
        w.write(5, 8); // mapping = 5, but mapping_count = 1
        let bytes = w.into_bytes();
        let mut br = BitReader::new(&bytes);
        assert_eq!(
            ModeConfig::parse(&mut br, 1).unwrap_err(),
            DecodeError::Malformed
        );
    }

    #[test]
    fn nonzero_time_transform_placeholder_is_malformed() {
        let mut w = BitWriter::new();
        w.write(0, 6); // time_count - 1 = 0 => 1
        w.write(1, 16); // placeholder must be 0
        let bytes = w.into_bytes();
        let mut storage = vec![0u8; 256];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        let (mut floors, mut residues, mut mappings, mut modes) = empty_outs();
        assert_eq!(
            parse_setup_tail(
                &mut br,
                &mut arena,
                4,
                2,
                &mut floors,
                &mut residues,
                &mut mappings,
                &mut modes,
            )
            .unwrap_err(),
            DecodeError::Malformed
        );
    }

    #[test]
    fn missing_framing_bit_is_malformed() {
        let mut w = BitWriter::new();
        write_full_tail_with_framing(&mut w, 0); // clear framing bit
        let bytes = w.into_bytes();

        let mut storage = vec![0u8; 8192];
        let mut arena = Arena::new(&mut storage);
        let mut br = BitReader::new(&bytes);
        let (mut floors, mut residues, mut mappings, mut modes) = empty_outs();
        let result = parse_setup_tail(
            &mut br,
            &mut arena,
            4,
            2,
            &mut floors,
            &mut residues,
            &mut mappings,
            &mut modes,
        );
        assert_eq!(result.unwrap_err(), DecodeError::Malformed);
    }
}
