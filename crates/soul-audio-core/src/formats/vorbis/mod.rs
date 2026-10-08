//! From-scratch Vorbis I decoder (design spec:
//! `docs/superpowers/specs/2026-09-05-vorbis-decoder-design.md`).
//!
//! Built up module by module per the implementation plan
//! (`docs/superpowers/plans/2026-09-05-vorbis-decoder.md`); [`bitread`] is
//! the first layer — the LSb-first bit reader and low-level primitives
//! (`ilog`, `float32_unpack`) every later stage parses through; [`arena`] is
//! the bump-allocator seam that lets the decoder stay no-alloc over
//! caller-provided storage (see `arena`'s module docs for why it is a
//! transient view, never a stored field).
//!
//! [`VorbisDecoder`] is the decoder seam itself: `new` parses the full setup
//! header (codebooks + floors + residues + mappings + modes, Task 4–5) plus the
//! identification header (channels/sample-rate/block sizes, so
//! [`Decoder::stream_info`] is real). Per-packet decode up to the **pre-IMDCT
//! spectrum** — floor 1 curve, residue (types 0/1/2), inverse coupling, and the
//! floor×residue dot product — lands in [`VorbisDecoder::decode_audio_packet`]
//! (Task 6); IMDCT + window + overlap-add to real PCM is [`VorbisDecoder::synthesize`]
//! (Task 7), with the final block trimmed to the exact granule-declared total
//! (Task 10). [`SeekableDecoder::seek_to`] bisects Ogg pages by granule and
//! re-primes the overlap state (Task 10). Since this codec needs a
//! caller-provided arena, it is dispatched per-platform rather than folded
//! into `AnyDecoder` — `soul-audio-desktop`'s `OwnedDecoder::Vorbis` (heap
//! arena) and the CLI's `decode` command are the desktop wiring (Task 11);
//! [`MAX_CHANNELS`]/[`MAX_BLOCKSIZE`] are the profile knobs behind the
//! `vorbis-embedded-profile` feature that a future firmware build (issue #32)
//! will tighten for a static/PSRAM arena.

pub mod arena;
pub mod bitread;
pub mod codebook;
pub mod floor;
pub mod imdct;
pub mod residue;
pub mod setup;

use core::cell::{Cell, RefCell};

use arena::Arena;
use bitread::{BitReader, ilog};
use codebook::Codebook;
use floor::Floor1Config;
use residue::ResidueConfig;
use setup::{
    MAX_CODEBOOKS, MAX_FLOORS, MAX_MAPPINGS, MAX_MODES, MAX_RESIDUES, MappingConfig, ModeConfig,
};

use crate::decoder::{DecodeError, Decoder, SeekableDecoder};
use crate::demux::ogg_packets::OggPackets;
use crate::io::{SeekSource, Source};
use crate::model::{SampleFormat, StreamInfo};

/// On-wire size of a conforming Vorbis identification header packet (spec
/// §4.2.2): `packet_type`(1) + `"vorbis"`(6) + `vorbis_version`(4) +
/// `audio_channels`(1) + `audio_sample_rate`(4) + 3×bitrate(4 each) +
/// blocksize byte(1) + framing byte(1) = 30 bytes. Every conforming encoder
/// emits exactly this many bytes; `ID_HEADER_BUF` leaves generous slack over
/// that for [`OggPackets::next_packet`]'s fixed scratch buffer.
const ID_HEADER_BUF: usize = 64;

/// Scratch for the **comment header**'s validated prefix. Only its packet-type
/// byte + `"vorbis"` magic (7 bytes) are checked; the rest of the packet —
/// which may be large (embedded cover art) — is skipped by
/// [`OggPackets::read_packet_prefix`], so this need only span the preamble.
const COMMENT_PREFIX_BUF: usize = 16;

/// Channel cap for the per-packet decode (Task 6+; profile knob, Task 11).
/// The **desktop profile** (default, no feature) raises this to 4 — a real
/// step above the embedded stereo cap, headroom for a multichannel Vorbis
/// file (design spec "Profiles" suggests up to 8; 4 is the value actually
/// exercised: every `[[_; MAX_N]; MAX_CHANNELS]` DSP-scratch field, plus the
/// residue decode's own `MAX_CHANNELS · MAX_N`-sized local, scale with it, and
/// several of this module's own unit tests hold two decoders live on the
/// default (non-big-stack) test thread — 4 was measured as the largest cap
/// those survive without every such test needing its own spawned thread; 8
/// overflowed even a single decoder there). The **`vorbis-embedded-profile`**
/// feature tightens this to stereo for a constrained-SRAM target (firmware
/// wiring is issue #32, out of scope here — it will build with the feature
/// enabled once it supplies a static/PSRAM arena). Either way, a stream
/// declaring more channels than the active cap is a graceful
/// [`DecodeError::Unsupported`] at decode time (`decode_audio_packet`'s early
/// channel-count check), never a panic.
#[cfg(not(feature = "vorbis-embedded-profile"))]
pub(crate) const MAX_CHANNELS: usize = 4;
#[cfg(feature = "vorbis-embedded-profile")]
pub(crate) const MAX_CHANNELS: usize = 2;

/// Width of the per-codebook FAST_HUFFMAN acceleration table
/// (`2^FAST_HUFFMAN_BITS` slots) built at setup to resolve any codeword this
/// long or shorter in one table lookup, short-circuiting the `decode_scalar`
/// linear scan (the residue hot loop, #34). The desktop profile uses 10
/// (stb_vorbis's value; 4 KiB/book); the `vorbis-embedded-profile` feature turns
/// it **off** (`0` → no table built, zero extra arena RAM). Longer codewords and
/// near-EOF reads fall back to the exact scan, so this never affects decoded
/// output — only speed.
#[cfg(not(feature = "vorbis-embedded-profile"))]
pub(crate) const FAST_HUFFMAN_BITS: u32 = 10;
#[cfg(feature = "vorbis-embedded-profile")]
pub(crate) const FAST_HUFFMAN_BITS: u32 = 0;

/// Largest (long) block size this profile supports (profile knob, Task 11).
/// The desktop profile (default) keeps the Vorbis I spec ceiling (`2^13` =
/// 8192); the `vorbis-embedded-profile` feature caps it at 4096 (design spec
/// "Profiles": "~halves the IMDCT/PCM working set"). [`IdHeader::parse`]
/// rejects a stream whose declared long block exceeds this as
/// [`DecodeError::Unsupported`] — load-bearing under the embedded profile,
/// where a real 8192-block stream would otherwise overrun the smaller
/// [`MAX_N`]-sized spectrum/residue/overlap scratch. [`MAX_N`] (its half) is
/// the length of every per-channel spectral scratch vector.
#[cfg(not(feature = "vorbis-embedded-profile"))]
pub(crate) const MAX_BLOCKSIZE: usize = 8192;
#[cfg(feature = "vorbis-embedded-profile")]
pub(crate) const MAX_BLOCKSIZE: usize = 4096;

/// Half [`MAX_BLOCKSIZE`]: the spectral-line count of a maximum-size block and
/// the fixed length of each per-channel spectrum/residue scratch vector.
pub(crate) const MAX_N: usize = MAX_BLOCKSIZE / 2;

/// Fixed scratch holding one audio packet's raw bytes, read from the Ogg
/// demuxer before bit-level decode. An audio packet larger than this is a
/// graceful [`DecodeError::Unsupported`] (via [`OggPackets::next_packet`]),
/// never a silent truncation.
pub(crate) const MAX_PACKET: usize = 16 * 1024;

/// Full-scale factor turning the IMDCT/overlap-add float PCM (in `[-1, 1)`, the
/// libvorbis reconstruction range) into 16-bit output. The unnormalized IMDCT
/// (libvorbis `mdct_backward` scale — see [`imdct`]) plus the Vorbis window's
/// Princen–Bradley reconstruction land the overlap-added signal at the encoder's
/// original amplitude, so no extra decoder gain is applied — only the
/// `[-1, 1) → i16` scale, matching ffmpeg's own `-f s16le` conversion (the golden
/// vs ffmpeg is the scale gate; a constant offset would show here). Named so a
/// future recalibration is a one-line change.
const OUTPUT_SCALE: f32 = 32768.0;

/// Round an IMDCT float PCM sample to the `i16` range, stored in an `i32`.
/// Manual round-half-away-from-zero (`s ± 0.5` then truncate) — `f32::round` is
/// std-only — with saturation to `[-32768, 32767]`; `NaN` maps to `0` (the
/// saturating `as i32` cast), so a stray non-finite never yields a garbage
/// sample. Mirrors the AAC filterbank's `round_i16`, with the `[-1, 1) → i16`
/// [`OUTPUT_SCALE`] folded in.
fn to_i16(s: f32) -> i32 {
    let r = s * OUTPUT_SCALE;
    let r = if r >= 0.0 { r + 0.5 } else { r - 0.5 };
    if r >= 32767.0 {
        32767
    } else if r <= -32768.0 {
        -32768
    } else {
        r as i32
    }
}

/// The identification-header fields the decoder retains: channels + sample rate
/// for [`StreamInfo`], and the two block sizes the per-packet mode selects
/// between (Task 6 DSP). The bitrate hint fields are validated for
/// well-formedness and discarded.
struct IdHeader {
    channels: u8,
    sample_rate: u32,
    /// Short block size (`2^blocksize_0`), in samples — the `false`-`blockflag`
    /// mode's transform length.
    blocksize_0: usize,
    /// Long block size (`2^blocksize_1`), in samples — the `true`-`blockflag`
    /// mode's transform length (`>= blocksize_0`).
    blocksize_1: usize,
}

impl IdHeader {
    /// Parse + validate an identification header packet (spec §4.2.2):
    /// packet type 1, the `"vorbis"` magic, format version 0, and the
    /// trailing framing bit. All of these fields happen to be byte-aligned
    /// (each width is a whole byte, except the last: `blocksize_0`(4 bits) +
    /// `blocksize_1`(4 bits) share one byte, and `framing_flag` is bit 0 of
    /// the final byte) — but they are still read through [`BitReader`], the
    /// crate's one proven LSb-first primitive, rather than hand-rolled as raw
    /// byte indexing.
    ///
    /// # Errors
    /// [`DecodeError::Malformed`] for a wrong packet type, wrong magic,
    /// zero channels/sample-rate, or a missing framing bit (all point to a
    /// corrupt/non-Vorbis packet, not a valid-but-unsupported variant).
    /// [`DecodeError::Unsupported`] for a nonzero `vorbis_version` (a future
    /// format revision this decoder doesn't implement).
    /// [`DecodeError::UnexpectedEof`] if the packet is shorter than the
    /// fixed header.
    fn parse(packet: &[u8]) -> Result<Self, DecodeError> {
        let mut r = BitReader::new(packet);
        if r.read_bits(8)? != 1 {
            return Err(DecodeError::Malformed);
        }
        for &expected in b"vorbis" {
            if r.read_bits(8)? != u32::from(expected) {
                return Err(DecodeError::Malformed);
            }
        }
        if r.read_bits(32)? != 0 {
            return Err(DecodeError::Unsupported); // a future vorbis_version
        }
        let channels = r.read_bits(8)? as u8;
        let sample_rate = r.read_bits(32)?;
        let _bitrate_maximum = r.read_bits(32)?;
        let _bitrate_nominal = r.read_bits(32)?;
        let _bitrate_minimum = r.read_bits(32)?;
        let blocksize_0_exp = r.read_bits(4)?;
        let blocksize_1_exp = r.read_bits(4)?;
        let framing = r.read_bit()?;
        if channels == 0 || sample_rate == 0 || !framing {
            return Err(DecodeError::Malformed);
        }
        // Block sizes are `2^exp` with `exp` in `6..=13` (64..=8192) and
        // `blocksize_0 <= blocksize_1` (spec §4.2.2 / §1.3.2); anything else
        // renders the stream undecodable.
        if !(6..=13).contains(&blocksize_0_exp)
            || !(6..=13).contains(&blocksize_1_exp)
            || blocksize_0_exp > blocksize_1_exp
        {
            return Err(DecodeError::Malformed);
        }
        // Profile cap (Task 11): a conforming stream can legally declare up to
        // the spec's 8192-sample long block, but the active profile's
        // [`MAX_BLOCKSIZE`] may be smaller (the embedded profile caps at
        // 4096). `blocksize_0 <= blocksize_1` is already enforced above, so
        // checking only the long block covers both. This must reject
        // *before* the sizes are stored — every later spectrum/residue/
        // overlap scratch is a fixed `[_; MAX_N]`/`[_; MAX_BLOCKSIZE]` array,
        // and indexing it with a bigger `n` would panic, not error.
        if (1usize << blocksize_1_exp) > MAX_BLOCKSIZE {
            return Err(DecodeError::Unsupported);
        }
        Ok(Self {
            channels,
            sample_rate,
            blocksize_0: 1usize << blocksize_0_exp,
            blocksize_1: 1usize << blocksize_1_exp,
        })
    }
}

/// A from-scratch Vorbis I decoder over a byte [`Source`], bump-allocating
/// setup-header structures into caller-owned storage `A` instead of the
/// heap — see the [`arena`] module docs for why `A` is stored by value here
/// (never behind a borrowed `Arena<'a>` field, which would self-reference).
///
/// Per-packet decode reaches the pre-IMDCT spectrum (Task 6); [`Decoder::decode_into`]
/// drives IMDCT/window/overlap-add to real PCM (Task 7), trimmed to the exact
/// granule-declared total on the final block (Task 10); [`SeekableDecoder::seek_to`]
/// bisects Ogg pages by granule and re-primes the overlap state (Task 10).
/// `A: AsMut<[u8]> + AsRef<[u8]>` names no concrete storage, so this type — and
/// the whole crate — stays `no_std`/no-alloc: the desktop caller supplies a heap
/// `Box<[u8]>`, firmware a `&'static mut [u8]` in SRAM/PSRAM.
pub struct VorbisDecoder<S, A: AsMut<[u8]> + AsRef<[u8]>> {
    /// The packet-preserving Ogg demuxer. `RefCell`-wrapped so
    /// [`SeekableDecoder::total_frames`] (an `&self` method, per the trait)
    /// can drive [`OggPackets::end_granule`], which needs `&mut`. `decode_into`
    /// and `seek_to` don't touch `ogg` at all yet (both are still stubs); once
    /// Task 7/10 give them real bodies, they already hold `&mut self` and
    /// should reach `ogg` via [`RefCell::get_mut`] — a zero-cost,
    /// compile-time borrow, not `borrow_mut()`'s runtime-checked one.
    ogg: RefCell<OggPackets<S>>,
    /// Caller-owned setup-header storage. Its front holds the raw setup
    /// packet (kept only because the bit reader over it borrows the same
    /// buffer during `new`; nothing re-reads it afterwards); the region
    /// after [`Self::setup_out_base`] holds every table the codebook/floor/
    /// residue/mapping offsets address. Read by [`Decoder::decode_into`]
    /// once audio decode (Task 6–7) lands.
    #[allow(dead_code)]
    storage: A,
    /// Start of the arena region within [`Self::storage`] — the byte length
    /// of the raw setup packet parked at the front. Every offset ([`Codebook`],
    /// [`Floor1Config`], [`ResidueConfig`], [`MappingConfig`]) is relative to
    /// this, so a read is `storage[setup_out_base + off]`.
    #[allow(dead_code)]
    setup_out_base: usize,
    /// Bump cursor within the arena region after the full setup parse — how
    /// many bytes the tables consumed in total.
    #[allow(dead_code)]
    top: usize,
    /// The parsed setup codebooks; only `codebooks[..codebook_count]` are live.
    /// Consumed by the floor/residue/audio decode.
    #[allow(dead_code)]
    codebooks: [Codebook; MAX_CODEBOOKS],
    /// Number of live entries in [`Self::codebooks`] (`1..=MAX_CODEBOOKS`).
    #[allow(dead_code)]
    codebook_count: usize,
    /// The parsed floor-1 configurations; only `floors[..floor_count]` are
    /// live. Consumed by the per-packet floor decode (Task 6).
    #[allow(dead_code)]
    floors: [Floor1Config; MAX_FLOORS],
    /// Number of live entries in [`Self::floors`] (`1..=MAX_FLOORS`).
    #[allow(dead_code)]
    floor_count: usize,
    /// The parsed residue configurations; only `residues[..residue_count]`
    /// are live. Consumed by the per-packet residue decode (Task 6).
    #[allow(dead_code)]
    residues: [ResidueConfig; MAX_RESIDUES],
    /// Number of live entries in [`Self::residues`] (`1..=MAX_RESIDUES`).
    #[allow(dead_code)]
    residue_count: usize,
    /// The parsed channel-mapping configurations; only
    /// `mappings[..mapping_count]` are live. Consumed by the per-packet
    /// mode/mapping dispatch (Task 6).
    #[allow(dead_code)]
    mappings: [MappingConfig; MAX_MAPPINGS],
    /// Number of live entries in [`Self::mappings`] (`1..=MAX_MAPPINGS`).
    #[allow(dead_code)]
    mapping_count: usize,
    /// The parsed mode configurations; only `modes[..mode_count]` are live.
    /// Consumed by the per-packet mode-selector bits (Task 6).
    #[allow(dead_code)]
    modes: [ModeConfig; MAX_MODES],
    /// Number of live entries in [`Self::modes`] (`1..=MAX_MODES`).
    #[allow(dead_code)]
    mode_count: usize,
    /// Short/long transform lengths (`2^blocksize_0` / `2^blocksize_1`), from
    /// the identification header — the per-packet mode's `blockflag` picks one.
    blocksize_0: usize,
    blocksize_1: usize,
    /// Per-channel **pre-IMDCT spectrum** scratch (Task 6): the floor curve is
    /// decoded into `spectrum[ch][..n]`, then multiplied in place by the
    /// decoded (and inverse-coupled) residue to become the final spectrum the
    /// IMDCT will consume in Task 7. Fixed-size (max blocksize/2 × channel cap),
    /// owned here rather than in the arena (the arena holds setup data only).
    spectrum: [[f32; MAX_N]; MAX_CHANNELS],
    /// Per-channel residue scratch (Task 6): the residue vectors, decoded then
    /// inverse-coupled in place, before the dot product folds them into
    /// [`Self::spectrum`].
    residue_buf: [[f32; MAX_N]; MAX_CHANNELS],
    /// Per-channel "unused this packet" flags (Task 6): `true` where the floor
    /// decoded as unused (no energy), so that channel's whole spectrum is zero.
    unused: [bool; MAX_CHANNELS],
    /// Per-channel IMDCT overlap-add accumulator (Task 7). Holds the running
    /// windowed sum for the output positions from the previous block's window
    /// **center** onward — the not-yet-finalized tail carried across packets.
    /// `overlap[ch][0..overlap_fill]` is live. The lapping is center-based (spec
    /// §1.3.2 / §4.3.8): each block is added at an offset derived from the
    /// previous and current block sizes, then the finalized prefix is emitted.
    overlap: [[f32; MAX_BLOCKSIZE]; MAX_CHANNELS],
    /// Live length of every [`Self::overlap`] channel buffer (shared — all
    /// channels of a packet use the same block size, so the lap geometry is
    /// identical across channels).
    overlap_fill: usize,
    /// Previous block's size in samples (`0` before the first audio packet, the
    /// priming sentinel). With the current block size it sets both the emitted
    /// sample count `(prev+cur)/4` and the overlap offset.
    prev_bs: usize,
    /// The window shape of the packet decoded into [`Self::spectrum`]: `blockflag`
    /// (long/short) plus the two per-long-block flags. Set by
    /// [`Self::decode_audio_packet`], consumed by [`Self::synthesize`] to build
    /// the correct (possibly long↔short transition) window.
    cur_blockflag: bool,
    cur_prev_window: bool,
    cur_next_window: bool,
    /// Interleaved `i16`-range PCM staged by one packet's synthesis, drained by
    /// [`Decoder::decode_into`] across as many calls as the caller's buffer
    /// needs (`pcm[pcm_pos..pcm_len]` is the undelivered remainder). One packet
    /// emits at most `MAX_N` per-channel samples.
    pcm: [i32; MAX_N * MAX_CHANNELS],
    pcm_pos: usize,
    pcm_len: usize,
    /// Set once [`OggPackets::next_packet`] reports a clean end of stream, so
    /// [`Decoder::decode_into`] stops pulling packets.
    eof: bool,
    /// Scratch for one audio packet's raw bytes (reused every [`Decoder::decode_into`]).
    packet_buf: [u8; MAX_PACKET],
    stream_info: StreamInfo,
    /// Running **absolute** (file-wide, not seek-relative) per-channel sample
    /// count already staged into [`Self::pcm`] across the whole decode (Task
    /// 10 granule end-trim). Every Ogg page's granule is a hard ABSOLUTE
    /// ceiling on "total samples through this page" (spec §4.3.8); comparing
    /// it against this counter each time [`Self::synthesize`] emits a block
    /// lets [`Decoder::decode_into`] trim the **final** block's emission so
    /// the decode stops at exactly the last page's granule instead of
    /// overshooting by whatever the last block's `(prev+cur)/4` formula would
    /// otherwise emit. [`Self::reset_decode_state`] zeroes this — correct for
    /// a from-start decode (where "absolute" and "since the last reset" are
    /// the same thing) and for [`SeekableDecoder::seek_to`]'s seek-to-0 path,
    /// but a non-zero-target seek re-baselines it to the landed page's floor
    /// granule right after, so it keeps meaning "absolute samples emitted so
    /// far" post-seek too — see `seek_to`'s doc comment for why an
    /// un-rebaselined `0` here silently disabled this end-trim after a seek.
    samples_emitted: u64,
    /// Memoized [`OggPackets::end_granule`] result for
    /// [`SeekableDecoder::total_frames`] — `None` means "not yet computed",
    /// `Some(None)` means "computed, but the stream declares no length".
    ///
    /// Computing it calls `end_granule`, which is now **non-destructive**: it
    /// snapshots and restores `ogg`'s reader position + page state, so querying
    /// the duration before or between [`Decoder::decode_into`] calls no longer
    /// corrupts the sequential decode (the Task-3 carry-forward, fixed in Task 7;
    /// see the `end_granule` doc and the `vorbis_total_frames_then_decode`
    /// integration test).
    total_frames: Cell<Option<Option<u64>>>,
}

impl<S: Source, A: AsMut<[u8]> + AsRef<[u8]>> VorbisDecoder<S, A> {
    /// Open a Vorbis stream by consuming its three header packets (spec §4.2):
    /// the identification header (channels/rate → [`Decoder::stream_info`]),
    /// the comment header (validated + skipped), and the **full** setup
    /// header — codebooks, floors, residues, mappings, and modes — parsed
    /// into `storage`.
    ///
    /// The raw setup packet is read into the front of `storage` and every
    /// setup table laid down after it, so the whole setup lives in the one
    /// caller-provided region; a setup packet or table set too large for
    /// `storage` returns [`DecodeError::Unsupported`] rather than panicking.
    ///
    /// # Errors
    /// Propagates Ogg framing errors ([`OggPackets`]), header-validation errors
    /// ([`IdHeader::parse`], the comment/setup common-header checks), and
    /// every setup-parse error ([`Codebook::parse`], [`setup::parse_setup_tail`])
    /// — malformed/unsupported as appropriate.
    pub fn new(src: S, mut storage: A) -> Result<Self, DecodeError> {
        let mut ogg = OggPackets::new(src)?;

        // Packet 1: identification header (channels + sample rate).
        let mut id_buf = [0u8; ID_HEADER_BUF];
        let n = ogg.next_packet(&mut id_buf)?;
        let id = IdHeader::parse(&id_buf[..n])?;

        // Packet 2: comment header — validate its type byte + magic, discard
        // the (possibly large) remainder.
        let mut comment_prefix = [0u8; COMMENT_PREFIX_BUF];
        let cn = ogg.read_packet_prefix(&mut comment_prefix)?;
        setup::read_common_header(
            &mut BitReader::new(&comment_prefix[..cn]),
            setup::COMMENT_HEADER_TYPE,
        )?;

        // Packet 3: setup header — read the raw packet into the front of
        // `storage`, then parse the whole setup (codebooks, then floors/
        // residues/mappings/modes/framing bit) into the region after it. The
        // two regions are disjoint sub-slices (`split_at_mut`), so the bit
        // reader over the raw packet and the arena writing the tables never
        // alias.
        let plen = ogg.next_packet(storage.as_mut())?;
        let mut codebooks = [Codebook::EMPTY; MAX_CODEBOOKS];
        let mut floors = [Floor1Config::EMPTY; MAX_FLOORS];
        let mut residues = [ResidueConfig::EMPTY; MAX_RESIDUES];
        let mut mappings = [MappingConfig::EMPTY; MAX_MAPPINGS];
        let mut modes = [ModeConfig::EMPTY; MAX_MODES];
        let (codebook_count, floor_count, residue_count, mapping_count, mode_count, top) = {
            let (raw, out) = storage.as_mut().split_at_mut(plen);
            let mut br = BitReader::new(raw);
            let mut arena = Arena::new(out);
            let codebook_count = setup::parse_codebooks(&mut br, &mut arena, &mut codebooks)?;
            let (floor_count, residue_count, mapping_count, mode_count) = setup::parse_setup_tail(
                &mut br,
                &mut arena,
                codebook_count,
                u16::from(id.channels),
                &mut floors,
                &mut residues,
                &mut mappings,
                &mut modes,
            )?;
            // FAST_HUFFMAN post-pass (#34): required setup (codebooks, floors,
            // residues, multiplicands) is now fully allocated, so build the
            // large best-effort acceleration tables from whatever arena space
            // remains — never starving a required allocation. `arena.top()` is
            // read AFTER so the recorded consumption includes them.
            for cb in &mut codebooks[..codebook_count] {
                cb.build_fast_table(&mut arena);
            }
            (
                codebook_count,
                floor_count,
                residue_count,
                mapping_count,
                mode_count,
                arena.top(),
            )
        };

        Ok(Self {
            ogg: RefCell::new(ogg),
            storage,
            setup_out_base: plen,
            top,
            codebooks,
            codebook_count,
            floors,
            floor_count,
            residues,
            residue_count,
            mappings,
            mapping_count,
            modes,
            mode_count,
            blocksize_0: id.blocksize_0,
            blocksize_1: id.blocksize_1,
            spectrum: [[0.0; MAX_N]; MAX_CHANNELS],
            residue_buf: [[0.0; MAX_N]; MAX_CHANNELS],
            unused: [false; MAX_CHANNELS],
            overlap: [[0.0; MAX_BLOCKSIZE]; MAX_CHANNELS],
            overlap_fill: 0,
            prev_bs: 0,
            cur_blockflag: false,
            cur_prev_window: true,
            cur_next_window: true,
            pcm: [0i32; MAX_N * MAX_CHANNELS],
            pcm_pos: 0,
            pcm_len: 0,
            eof: false,
            packet_buf: [0u8; MAX_PACKET],
            samples_emitted: 0,
            stream_info: StreamInfo {
                sample_rate: id.sample_rate,
                channels: u16::from(id.channels),
                format: SampleFormat::Int { bits: 16 },
            },
            total_frames: Cell::new(None),
        })
    }

    /// Decode one audio packet (spec §4.3) from `self.packet_buf[..plen]` up to
    /// the **pre-IMDCT spectrum**: §4.3.1 mode/window bits → §4.3.2 floor 1
    /// curve per channel → §4.3.3 coupling `no_residue` propagation → §4.3.4
    /// residue per submap → §4.3.5 inverse coupling → §4.3.6 floor×residue dot
    /// product. The per-channel result lands in [`Self::spectrum`]`[ch][..n]`,
    /// the floor-unused flags in [`Self::unused`]. Returns `n` — the current
    /// block size / 2, i.e. the spectral-line count.
    ///
    /// IMDCT + window + overlap-add → PCM is Task 7; this stops at the spectrum.
    ///
    /// # Errors
    /// [`DecodeError::Malformed`] for a non-audio packet type, an out-of-range
    /// mode number, or a corrupt floor/residue; [`DecodeError::Unsupported`] if
    /// the stream has more channels than [`MAX_CHANNELS`] or a residue book is
    /// too wide; [`DecodeError::UnexpectedEof`] on a truncated packet.
    fn decode_audio_packet(&mut self, plen: usize) -> Result<usize, DecodeError> {
        let channels = usize::from(self.stream_info.channels);
        if channels > MAX_CHANNELS {
            return Err(DecodeError::Unsupported);
        }
        let base = self.setup_out_base;

        // §4.3.1: packet type (0 = audio), mode selector, block size, and — for
        // a long block — the two window-shape flags (consumed for bit
        // alignment; they drive the window in Task 8, not the spectrum here).
        let mut br = BitReader::new(&self.packet_buf[..plen]);
        if br.read_bit()? {
            return Err(DecodeError::Malformed); // not an audio packet
        }
        let mode_number = br.read_bits(ilog((self.mode_count - 1) as u32))? as usize;
        if mode_number >= self.mode_count {
            return Err(DecodeError::Malformed);
        }
        let mode = self.modes[mode_number];
        let blockflag = mode.blockflag;
        let n = if blockflag {
            self.blocksize_1
        } else {
            self.blocksize_0
        } / 2;
        // §4.3.1: a long block carries two window-shape flags (a cleared flag
        // means the neighbour is a short block, compressing that slope — Task 8's
        // transitions). A short block has neither; its flags are irrelevant to
        // the symmetric short window, so default them to "long".
        let (prev_window, next_window) = if blockflag {
            (br.read_bit()?, br.read_bit()?)
        } else {
            (true, true)
        };
        self.cur_blockflag = blockflag;
        self.cur_prev_window = prev_window;
        self.cur_next_window = next_window;
        let mapping = self.mappings[mode.mapping as usize];
        // Borrow the setup arena once; it is a distinct field from every DSP
        // scratch mutated below, so the shared borrow coexists with them.
        let arena = &self.storage.as_ref()[base..];

        // §4.3.2: decode each channel's floor curve into the spectrum scratch.
        // `no_residue[ch]` mirrors the reported unused flag before coupling.
        // (`ch` is a shared channel index across several parallel structures,
        // not a single-slice walk, so the range loop is the clearest form.)
        let mut no_residue = [false; MAX_CHANNELS];
        #[allow(clippy::needless_range_loop)]
        for ch in 0..channels {
            let submap = mapping.channel_submap(arena, ch) as usize;
            let floor_idx = mapping.submap_floor(arena, submap) as usize;
            let fcfg = self.floors[floor_idx];
            let used = floor::floor1_decode(
                &fcfg,
                &self.codebooks,
                arena,
                &mut br,
                n,
                &mut self.spectrum[ch],
            )?;
            self.unused[ch] = !used;
            no_residue[ch] = !used;
        }

        // §4.3.3: if either side of a coupling pair is used, both are decoded.
        let coupling_steps = mapping.coupling_steps as usize;
        for step in 0..coupling_steps {
            let (mag, ang) = mapping.coupling(arena, step);
            let (mag, ang) = (mag as usize, ang as usize);
            if !no_residue[mag] || !no_residue[ang] {
                no_residue[mag] = false;
                no_residue[ang] = false;
            }
        }

        // §4.3.4: residue accumulates into a zeroed scratch, decoded per submap
        // over exactly the channels that map to it.
        for ch in 0..channels {
            for j in 0..n {
                self.residue_buf[ch][j] = 0.0;
            }
        }
        for submap in 0..mapping.submaps as usize {
            let mut ch_list = [0usize; MAX_CHANNELS];
            let mut dnd = [false; MAX_CHANNELS];
            let mut count = 0usize;
            #[allow(clippy::needless_range_loop)]
            for ch in 0..channels {
                if mapping.channel_submap(arena, ch) as usize == submap {
                    ch_list[count] = ch;
                    dnd[count] = no_residue[ch];
                    count += 1;
                }
            }
            if count == 0 {
                continue;
            }
            let res_idx = mapping.submap_residue(arena, submap) as usize;
            let rcfg = self.residues[res_idx];
            residue::residue_decode(
                &rcfg,
                &self.codebooks,
                arena,
                &mut br,
                n,
                &ch_list[..count],
                &dnd[..count],
                &mut self.residue_buf,
            )?;
        }

        // §4.3.5: inverse coupling, applied in reverse coupling-step order.
        for step in (0..coupling_steps).rev() {
            let (mag, ang) = mapping.coupling(arena, step);
            let (mag, ang) = (mag as usize, ang as usize);
            for j in 0..n {
                let (nm, na) = residue::couple(self.residue_buf[mag][j], self.residue_buf[ang][j]);
                self.residue_buf[mag][j] = nm;
                self.residue_buf[ang][j] = na;
            }
        }

        // §4.3.6: dot product — floor curve × residue → the pre-IMDCT spectrum.
        // An unused channel keeps its zero floor, so its spectrum stays zero.
        for ch in 0..channels {
            for j in 0..n {
                self.spectrum[ch][j] *= self.residue_buf[ch][j];
            }
        }

        Ok(n)
    }

    /// IMDCT → Vorbis window → overlap-add → interleaved `i32` PCM (spec §4.3.7
    /// IMDCT, §4.3.8 window + lapping) for the packet just decoded into
    /// [`Self::spectrum`]`[ch][..n]` (`n = blocksize/2`). Stages the finalized
    /// samples into [`Self::pcm`] and returns the per-channel emitted count.
    ///
    /// # Lapping (center-based, spec §1.3.2)
    ///
    /// Each block, once IMDCT'd and windowed, spans `bs = 2·n` samples centered
    /// on its window center; consecutive centers are `(prev_bs + bs)/4` apart,
    /// which is exactly the emitted-sample count. The block is added into the
    /// per-channel accumulator [`Self::overlap`] at offset `(prev_bs − bs)/4`
    /// relative to the previous center (negative only when the current block is
    /// larger, where its leading window samples are zero anyway), then the
    /// finalized prefix `[0, emit)` is drained and the accumulator shifted. The
    /// **first** audio packet emits nothing — its right half primes the overlap
    /// (spec: "Data is not returned from the first frame"). This reduces to plain
    /// 50 % overlap-add when every block is the same (long) size.
    fn synthesize(&mut self, n: usize) -> usize {
        let channels = usize::from(self.stream_info.channels).min(MAX_CHANNELS);
        let bs = 2 * n;
        let bp = self.prev_bs;

        // The synthesis window for this block's shape (shared across channels).
        let mut window = [0.0f32; MAX_BLOCKSIZE];
        imdct::build_window(
            self.cur_blockflag,
            self.cur_prev_window,
            self.cur_next_window,
            bs,
            self.blocksize_0,
            &mut window,
        );

        // Center-based lap geometry (see the doc comment). `bp`/`bs` are always
        // multiples of 4 (block sizes are powers of two ≥ 64), so these divide
        // exactly. `start` can be negative; the window zeroes those samples.
        let emit = if bp == 0 { 0 } else { (bp + bs) / 4 };
        let start = (bp as isize - bs as isize) / 4;
        let need = (start + bs as isize).max(0) as usize; // = bp/4 + 3·bs/4

        let mut td = [0.0f32; MAX_BLOCKSIZE];
        for ch in 0..channels {
            imdct::imdct(&self.spectrum[ch], n, &mut td);

            if bp == 0 {
                // Priming: keep only the windowed right half as the overlap tail.
                let acc = &mut self.overlap[ch];
                for i in 0..bs / 2 {
                    acc[i] = td[bs / 2 + i] * window[bs / 2 + i];
                }
                continue;
            }

            let curfill = self.overlap_fill.max(need);
            let acc = &mut self.overlap[ch];
            // Zero the region newly exposed beyond the previous fill.
            for v in acc.iter_mut().take(curfill).skip(self.overlap_fill) {
                *v = 0.0;
            }
            // Overlap-add this block's windowed samples at `start`.
            for i in 0..bs {
                let p = start + i as isize;
                if p >= 0 {
                    acc[p as usize] += td[i] * window[i];
                }
            }
            // Drain the finalized prefix into interleaved PCM. (`t` indexes both
            // the interleaved output stride and the mono accumulator, so the
            // range loop is clearer than a zip.)
            #[allow(clippy::needless_range_loop)]
            for t in 0..emit {
                self.pcm[t * channels + ch] = to_i16(acc[t]);
            }
            // Shift the accumulator left by `emit`; zero the vacated tail.
            acc.copy_within(emit..curfill, 0);
            for v in acc.iter_mut().take(curfill).skip(curfill - emit) {
                *v = 0.0;
            }
        }

        // Advance the shared lap state.
        self.overlap_fill = if bp == 0 {
            bs / 2
        } else {
            self.overlap_fill.max(need) - emit
        };
        self.prev_bs = bs;
        emit
    }

    /// Reset every per-packet overlap-add / PCM-staging field to the pristine
    /// values [`Self::new`] establishes (Task 10 [`SeekableDecoder::seek_to`]).
    /// Does **not** touch the parsed setup tables (codebooks/floors/residues/
    /// mappings/modes) or [`Self::stream_info`] — those don't change across a
    /// seek. The old overlap tail and any staged-but-undelivered PCM belong to
    /// the position we're jumping away from, so both must be dropped before
    /// decoding resumes at the new Ogg position; otherwise a stale
    /// [`Self::overlap`] tail would bleed into the first post-seek block, or a
    /// leftover [`Self::pcm`] window would be handed out ahead of the seek.
    /// Zeroes [`Self::samples_emitted`] too — correct as-is for the seek-to-0
    /// path, but `seek_to`'s general (non-zero-target) path re-baselines it
    /// to the landed page's floor granule right after calling this, so the
    /// end-trim counter keeps meaning "absolute samples emitted" post-seek
    /// (see [`Self::samples_emitted`]'s doc comment).
    fn reset_decode_state(&mut self) {
        for ch in &mut self.overlap {
            ch.fill(0.0);
        }
        self.overlap_fill = 0;
        self.prev_bs = 0;
        self.pcm_pos = 0;
        self.pcm_len = 0;
        self.eof = false;
        self.samples_emitted = 0;
    }

    /// Skip forward past the three Vorbis header packets (id/comment/setup)
    /// without re-parsing them, leaving the Ogg reader positioned at the first
    /// audio packet — exactly where [`Self::new`] leaves it. Used by
    /// [`SeekableDecoder::seek_to`]'s seek-to-0 path (Task 10) to reproduce that
    /// exact position after rewinding to the stream's start byte: the setup
    /// tables already live in `self` from the original `new()` parse, so this
    /// only needs to consume the same number of Ogg packets, not re-decode
    /// their contents. [`OggPackets::read_packet_prefix`] (rather than
    /// `next_packet`) is used throughout so an oversized comment/setup packet
    /// can't fail this the way `next_packet`'s strict size check would.
    ///
    /// # Errors
    /// Propagates the underlying [`OggPackets`] I/O/framing errors.
    fn skip_header_packets(&mut self) -> Result<(), DecodeError> {
        for _ in 0..3 {
            self.ogg
                .get_mut()
                .read_packet_prefix(&mut self.packet_buf)?;
        }
        Ok(())
    }
}

impl<S: Source, A: AsMut<[u8]> + AsRef<[u8]>> Decoder for VorbisDecoder<S, A> {
    fn stream_info(&self) -> StreamInfo {
        self.stream_info
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        // Drain any PCM staged by a previous packet, refilling by decoding and
        // synthesizing the next packet(s) as the caller's buffer demands. A
        // priming first packet (or any packet that emits nothing) stages zero
        // samples, so the loop simply fetches the next one.
        let mut written = 0;
        while written < out.len() {
            if self.pcm_pos >= self.pcm_len {
                if self.eof {
                    break;
                }
                let plen = self.ogg.get_mut().next_packet(&mut self.packet_buf)?;
                if plen == 0 {
                    self.eof = true;
                    break;
                }
                let n = self.decode_audio_packet(plen)?;
                let mut emit = self.synthesize(n);
                // Granule end-trim (Task 10): the page we just finished reading
                // this packet from caps "total samples through here" — an exact
                // ceiling for every page (spec §4.3.8), not just the last one.
                // For every page but the last, real encoders never let the
                // running total exceed it, so this is a no-op; on the true last
                // page, the final block's `(prev+cur)/4` formula can overshoot
                // the exact tail, and this clamps `emit` so the whole decode
                // stops at precisely the granule-declared total rather than
                // running past it. `u64::MAX` marks "no packet finished on this
                // page" (spec) and carries no usable ceiling, so it's skipped.
                let ceiling = self.ogg.get_mut().last_page_granule();
                if ceiling != u64::MAX {
                    let remaining = ceiling.saturating_sub(self.samples_emitted);
                    if (emit as u64) > remaining {
                        emit = remaining as usize;
                    }
                }
                self.samples_emitted += emit as u64;
                self.pcm_pos = 0;
                self.pcm_len = emit * usize::from(self.stream_info.channels);
                continue;
            }
            let navail = self.pcm_len - self.pcm_pos;
            let ncopy = navail.min(out.len() - written);
            out[written..written + ncopy]
                .copy_from_slice(&self.pcm[self.pcm_pos..self.pcm_pos + ncopy]);
            self.pcm_pos += ncopy;
            written += ncopy;
        }
        Ok(written)
    }
}

impl<S: SeekSource, A: AsMut<[u8]> + AsRef<[u8]>> SeekableDecoder for VorbisDecoder<S, A> {
    /// Seek to per-channel `sample` (spec §4.3.8 granule↔sample mapping; Ogg
    /// embedding, spec appendix): bisect Ogg pages by granule position
    /// ([`OggPackets::seek_to_granule`]) to land near the target, then
    /// **decode-and-discard one packet** to rebuild the overlap-add state
    /// before emitting audio (design spec, "Seeking (in v1)" — emitting
    /// immediately after a raw seek would synthesize a wrong first block, since
    /// its left half has no valid overlap context to lap against). With the
    /// overlap state freshly reset ([`Self::reset_decode_state`] sets
    /// `prev_bs = 0`), that discarded packet is decoded exactly like the
    /// stream's true first audio packet: [`Self::synthesize`]'s existing
    /// `bp == 0` priming case emits nothing for it and keeps only its windowed
    /// right half as the new overlap tail — so the very next
    /// [`Decoder::decode_into`] call resumes emitting correctly-overlapped real
    /// audio, with no seek-specific synthesis path needed.
    ///
    /// `sample == 0` is handled **exactly**, not approximately: every header
    /// page shares granule `0` with the stream's first (priming) audio packet
    /// (spec: "the first audio packet produces no output"), so bisecting for
    /// granule `0` would land on the identification-header page, not the first
    /// audio packet. Instead this rewinds to the stream's start byte and
    /// [`Self::skip_header_packets`] replays the exact three-packet skip
    /// [`Self::new`] performed to reach the same position — so seeking to `0`
    /// decodes **identically** to a fresh decoder, not merely "close to the
    /// start".
    ///
    /// A `sample` beyond the stream's declared length ([`Self::total_frames`])
    /// clamps to it, mirroring [`OggPackets::seek_to_granule`]'s own "closest
    /// available position" contract (never an error).
    ///
    /// Ogg-granule bisection is inherently **page-granular** (design spec:
    /// "land NEAR the target sample") — a page's granule is the sample count
    /// through its *last* completed packet, and packets within a page carry no
    /// individually-addressable position, so the returned value is whatever
    /// [`OggPackets::seek_to_granule`] reports for the landed page, and real
    /// audio resumes somewhere at or before it (the granularity of one Ogg
    /// page), not a sample-exact position.
    ///
    /// # Errors
    /// Propagates [`OggPackets::seek_to_granule`]/`next_packet` I/O and framing
    /// errors, and [`Self::decode_audio_packet`]'s decode errors for the
    /// re-priming packet.
    fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        let target = match self.total_frames() {
            Some(total) => sample.min(total),
            None => sample,
        };

        if target == 0 {
            self.ogg.get_mut().seek_to_granule(0)?;
            self.skip_header_packets()?;
            self.reset_decode_state();
            return Ok(0);
        }

        let landed = self.ogg.get_mut().seek_to_granule(target)?;
        // The landed page's FLOOR — the granule of the page immediately
        // before it, i.e. the exact absolute per-channel sample count
        // already accounted for as of the START of this page's audio
        // (`OggPackets::landed_floor_granule`'s doc comment). Fetched before
        // `reset_decode_state` zeroes `samples_emitted`, so it can be
        // combined with the re-priming packet's own size below.
        let floor = self.ogg.get_mut().landed_floor_granule();
        self.reset_decode_state();

        // Re-prime: decode-and-discard the landed page's first packet so the
        // overlap-add state is rebuilt before real emission resumes (design
        // spec doc comment above `seek_to`).
        let plen = self.ogg.get_mut().next_packet(&mut self.packet_buf)?;
        if plen == 0 {
            self.eof = true;
            // Nothing to re-prime: no packet was decoded, so there is no
            // "discarded packet" contribution to account for — the baseline
            // is exactly `floor` (see the general case below).
            self.samples_emitted = floor;
            return Ok(landed);
        }
        let n = self.decode_audio_packet(plen)?;
        let _ = self.synthesize(n); // discarded — this call always emits 0 (bp == 0)

        // `reset_decode_state` zeroed `samples_emitted`; re-baseline it so it
        // keeps meaning "absolute per-channel samples emitted so far" after a
        // seek, exactly as it does for a from-start decode (where it also
        // counts from 0 = absolute). Without this, `decode_into`'s end-trim
        // ceiling check (`ceiling - samples_emitted`, `ceiling` being the
        // file-wide absolute `last_page_granule()`) would compare an absolute
        // ceiling against a counter that stayed relative to the seek —
        // understating "samples emitted so far" and letting decode run past
        // the true final block (the Task-7 overshoot bug, reproduced by
        // seeking instead of decoding from the start).
        //
        // The baseline is `floor + n`, not plain `floor`. `floor` is where
        // decoding of the landed page's audio BEGINS — but the packet just
        // discarded above still represents `n` real per-channel samples that
        // a true from-start decode would have counted from that point (its
        // OWN contribution to the running total, lost forever because
        // reconstructing it needs the true previous block size, which lives
        // on the page the seek skipped past — the very reason it must be
        // discarded rather than emitted). Using plain `floor` leaves the
        // counter permanently `n` samples behind the real absolute position,
        // which — since the ceiling never otherwise changes once decoding
        // reaches the stream's own final page — lets the true final block's
        // trim fire `n` samples too late, silently reproducing the same
        // overshoot this fix targets, just short of the full landed-page span
        // instead of the whole thing.
        //
        // `n` here is the discarded packet's own half-block-size, i.e. this
        // assumes its window is the same size as the (unknown) block before
        // it — exact whenever true, which includes every all-long-block or
        // all-short-block stream (every real-file fixture in this crate,
        // e.g. the mono fixture below) and is only approximate exactly AT a
        // block-size transition landing on this specific page seam, in which
        // case it is still strictly closer than the `floor`-only baseline.
        self.samples_emitted = floor + n as u64;

        Ok(landed)
    }

    fn total_frames(&self) -> Option<u64> {
        if let Some(cached) = self.total_frames.get() {
            return cached;
        }
        let result = self.ogg.borrow_mut().end_granule().ok();
        self.total_frames.set(Some(result));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceSource;

    /// A minimal single-packet Ogg page (mirrors
    /// `demux::ogg_packets`'s own test helper) — `packet` must be under 255
    /// bytes (true for every fixture below).
    fn ogg_page(header_type: u8, granule: u64, serial: u32, seq: u32, packet: &[u8]) -> Vec<u8> {
        assert!(packet.len() < 255);
        let mut v = Vec::new();
        v.extend_from_slice(b"OggS");
        v.push(0);
        v.push(header_type);
        v.extend_from_slice(&granule.to_le_bytes());
        v.extend_from_slice(&serial.to_le_bytes());
        v.extend_from_slice(&seq.to_le_bytes());
        v.extend_from_slice(&[0, 0, 0, 0]);
        v.push(1);
        v.push(packet.len() as u8);
        v.extend_from_slice(packet);
        v
    }

    /// A well-formed 30-byte identification header packet. Every
    /// multi-byte field here is byte-aligned, so plain `to_le_bytes()`
    /// matches what `BitReader` (LSb-first) reads back — see
    /// [`IdHeader::parse`]'s doc comment.
    fn id_header_packet(channels: u8, sample_rate: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.push(1); // packet_type
        v.extend_from_slice(b"vorbis");
        v.extend_from_slice(&0u32.to_le_bytes()); // vorbis_version
        v.push(channels);
        v.extend_from_slice(&sample_rate.to_le_bytes());
        v.extend_from_slice(&0i32.to_le_bytes()); // bitrate_maximum
        v.extend_from_slice(&0i32.to_le_bytes()); // bitrate_nominal
        v.extend_from_slice(&0i32.to_le_bytes()); // bitrate_minimum
        v.push(0xB8); // blocksize_0=8 (256), blocksize_1=11 (2048)
        v.push(0x01); // framing_flag=1, 7 padding bits
        assert_eq!(v.len(), 30, "fixture must match the real on-wire size");
        v
    }

    /// Minimal LSb-first bit writer, to hand-build the packed setup-header
    /// codebook. Mirrors [`BitReader`]'s ordering (bit 0 first).
    struct TestBitWriter {
        bytes: Vec<u8>,
        bit: usize,
    }
    impl TestBitWriter {
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

    /// A valid, minimal comment header packet (spec §4.2.3): type 3 + magic,
    /// an empty vendor string, zero user comments, framing bit set. The
    /// decoder only validates the type + magic and skips the rest.
    fn comment_header_packet() -> Vec<u8> {
        let mut v = Vec::new();
        v.push(3);
        v.extend_from_slice(b"vorbis");
        v.extend_from_slice(&0u32.to_le_bytes()); // vendor_length
        v.extend_from_slice(&0u32.to_le_bytes()); // user_comment_list_length
        v.push(0x01); // framing
        v
    }

    /// A valid, minimal but **complete** setup header packet (spec §4.2.4):
    /// type 5, magic, one codebook, one floor-1, one residue, one mapping
    /// (`submaps=1`, no coupling, sized for `channels=2`), one mode, and the
    /// final framing bit. `new` now parses the whole setup header, so every
    /// section must be present and internally consistent (indices
    /// cross-referencing the single floor/residue/mapping, all `0`) for the
    /// happy-path tests below to open successfully.
    fn setup_header_packet() -> Vec<u8> {
        let mut w = TestBitWriter::new();
        // --- codebook (dims=1, entries=2, full 1-bit tree, scalar) --------
        w.write(0x0056_4342, 24); // codebook sync
        w.write(1, 16); // dimensions
        w.write(2, 24); // entries
        w.write(0, 1); // ordered = 0
        w.write(0, 1); // sparse = 0
        w.write(0, 5); // entry 0: length 1 (stored as 0)
        w.write(0, 5); // entry 1: length 1
        w.write(0, 4); // lookup_type = 0

        // --- time-domain transforms (vestigial): 1 entry, must be 0 ------
        w.write(0, 6); // time_count - 1 = 0 => 1
        w.write(0, 16); // placeholder = 0

        // --- floors: 1 floor-1, 0 partitions (no classes, no X reads) ----
        w.write(0, 6); // floor_count - 1 = 0 => 1
        w.write(1, 16); // floor_type = 1
        w.write(0, 5); // floor1_partitions = 0
        w.write(0, 2); // multiplier - 1 = 0 => 1
        w.write(0, 4); // rangebits = 0

        // --- residues: 1 residue, type 0, 1 classification, cascade = 0 --
        w.write(0, 6); // residue_count - 1 = 0 => 1
        w.write(0, 16); // residue_type = 0
        w.write(0, 24); // begin
        w.write(0, 24); // end
        w.write(0, 24); // partition_size - 1 = 0 => 1
        w.write(0, 6); // classifications - 1 = 0 => 1
        w.write(0, 8); // classbook = 0 (< codebook_count = 1)
        w.write(0, 3); // cascade[0] low bits = 0
        w.write(0, 1); // cascade[0] high-bits flag = 0 => cascade = 0 (no stage books)

        // --- mappings: 1 mapping, submaps=1, no coupling ------------------
        w.write(0, 6); // mapping_count - 1 = 0 => 1
        w.write(0, 16); // mapping_type = 0
        w.write(0, 1); // has_submaps = 0 => submaps = 1
        w.write(0, 1); // has_coupling = 0 => coupling_steps = 0
        w.write(0, 2); // reserved = 0
        // submaps == 1, so no per-channel mux bits are read.
        w.write(0, 8); // submap[0] reserved
        w.write(0, 8); // submap[0] floor index = 0
        w.write(0, 8); // submap[0] residue index = 0

        // --- modes: 1 mode, mapping 0 -------------------------------------
        w.write(0, 6); // mode_count - 1 = 0 => 1
        w.write(0, 1); // blockflag = 0
        w.write(0, 16); // windowtype = 0
        w.write(0, 16); // transformtype = 0
        w.write(0, 8); // mapping = 0

        w.write(1, 1); // final framing bit
        let body = w.into_bytes();

        let mut v = Vec::new();
        v.push(5);
        v.extend_from_slice(b"vorbis");
        v.push(0); // codebook_count - 1 = 0 => 1 codebook
        v.extend_from_slice(&body);
        v
    }

    /// The three Vorbis header pages (id on the BOS page, then comment, then
    /// setup) at `granule`, ready to prepend to any audio pages.
    fn header_pages(id: &[u8], granule: u64, serial: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(ogg_page(0x02, granule, serial, 0, id));
        b.extend(ogg_page(0x00, granule, serial, 1, &comment_header_packet()));
        b.extend(ogg_page(0x00, granule, serial, 2, &setup_header_packet()));
        b
    }

    fn open_dec(packet: &[u8]) -> Result<VorbisDecoder<SliceSource<'_>, [u8; 256]>, DecodeError> {
        // A complete three-header stream: `new` now parses all three headers,
        // so a corrupt id fails at packet 1 while the (valid) comment/setup
        // pages that follow keep every positive case openable.
        let bytes = header_pages(packet, 0, 1);
        // Leaked so the returned decoder's `S` can outlive this helper — fine
        // in tests (no repeated-call growth: each fixture is tiny).
        let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        VorbisDecoder::new(SliceSource::new(bytes), [0u8; 256])
    }

    #[test]
    fn new_parses_id_header_into_stream_info() {
        let packet = id_header_packet(2, 44_100);
        let dec = open_dec(&packet).expect("valid id header");
        let info = dec.stream_info();
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.channels, 2);
        assert_eq!(info.format, SampleFormat::Int { bits: 16 });
    }

    #[test]
    fn new_rejects_wrong_packet_type() {
        let mut packet = id_header_packet(2, 44_100);
        packet[0] = 3; // setup header's type, not id header's
        assert_eq!(open_dec(&packet).err(), Some(DecodeError::Malformed));
    }

    #[test]
    fn new_rejects_bad_vorbis_magic() {
        let mut packet = id_header_packet(2, 44_100);
        packet[1..7].copy_from_slice(b"garbge");
        assert_eq!(open_dec(&packet).err(), Some(DecodeError::Malformed));
    }

    #[test]
    fn new_rejects_nonzero_version() {
        let mut packet = id_header_packet(2, 44_100);
        packet[7..11].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(open_dec(&packet).err(), Some(DecodeError::Unsupported));
    }

    #[test]
    fn new_rejects_zero_channels() {
        let packet = id_header_packet(0, 44_100);
        assert_eq!(open_dec(&packet).err(), Some(DecodeError::Malformed));
    }

    #[test]
    fn new_rejects_zero_sample_rate() {
        let packet = id_header_packet(2, 0);
        assert_eq!(open_dec(&packet).err(), Some(DecodeError::Malformed));
    }

    #[test]
    fn new_rejects_missing_framing_bit() {
        let mut packet = id_header_packet(2, 44_100);
        packet[29] = 0x00; // framing_flag bit cleared
        assert_eq!(open_dec(&packet).err(), Some(DecodeError::Malformed));
    }

    #[test]
    fn new_rejects_blocksize_over_the_profile_cap() {
        // Only load-bearing under `--features vorbis-embedded-profile`
        // (`MAX_BLOCKSIZE` = 4096): a conforming stream declaring the spec's
        // max long block (exponent 13 = 8192, for both blocksize_0 and
        // blocksize_1 — blocksize_0 <= blocksize_1 must still hold) must be
        // gracefully `Unsupported`, not a panic from decoding an n=4096
        // spectrum into a 2048-element (`MAX_N`) scratch array. Under the
        // default desktop profile (`MAX_BLOCKSIZE` = 8192) this is a no-op
        // assertion — 8192 is exactly the cap, so it's accepted — which is
        // why this still needs the embedded-profile test run to actually
        // exercise the rejection path.
        let mut packet = id_header_packet(2, 44_100);
        packet[28] = (13 << 4) | 13; // blocksize_0_exp = blocksize_1_exp = 13
        let want = if cfg!(feature = "vorbis-embedded-profile") {
            Some(DecodeError::Unsupported)
        } else {
            None
        };
        assert_eq!(open_dec(&packet).err(), want);
    }

    #[test]
    fn decode_into_returns_ok_zero_at_eof() {
        // A headers-only stream (no audio packets): `decode_into` must report
        // clean end-of-stream (`Ok(0)`), not stub-out silently — real content
        // is exercised by the golden decode tests elsewhere in this crate.
        let packet = id_header_packet(2, 44_100);
        let mut dec = open_dec(&packet).unwrap();
        let mut out = [0i32; 16];
        assert_eq!(dec.decode_into(&mut out).unwrap(), 0);
    }

    #[test]
    fn seek_to_zero_on_a_headers_only_stream_is_a_no_op() {
        // No audio packets at all: `total_frames` is `Some(0)` (the header
        // pages' own granule), so every seek clamps to 0 and the special-cased
        // seek-to-0 path (rewind + re-skip the three headers) must still
        // succeed without error even though there's nothing to re-prime.
        let packet = id_header_packet(2, 44_100);
        let mut dec = open_dec(&packet).unwrap();
        assert_eq!(dec.seek_to(0), Ok(0));
        assert_eq!(dec.seek_to(1_000_000), Ok(0)); // clamps to total_frames == 0
    }

    /// One audio packet per Ogg page (mirrors real per-page granule
    /// accounting at the finest possible granularity), built against the
    /// minimal setup fixture: channel 0's floor is always "unused" (its
    /// spectrum is exactly zero); channel 1's floor is "used" with only its
    /// two `Y` endpoints (`floor1_partitions == 0`); the residue is empty
    /// (`end == 0`), so nothing else is read — same bit layout as
    /// `unused_floor_channel_yields_zero_spectrum`. The setup's lone mode is
    /// short-block-only (`blockflag == 0`, `blocksize_0 == 256` ⇒ `n == 128`),
    /// so `mode_count == 1` also means no mode-selector bits are read.
    fn audio_packet(y0: u32, y1: u32) -> Vec<u8> {
        let mut w = TestBitWriter::new();
        w.write(0, 1); // packet type = audio
        w.write(0, 1); // channel 0 floor nonzero = 0 (unused)
        w.write(1, 1); // channel 1 floor nonzero = 1 (used)
        w.write(y0, 8);
        w.write(y1, 8);
        w.into_bytes()
    }

    /// Header pages + three one-packet audio pages with real, ascending
    /// per-channel granules: packet 1 primes (spec: "data is not returned
    /// from the first frame", so its page's granule stays 0); packets 2/3 are
    /// short blocks (`n = blocksize_0/2 = 128`, `bs = 2n = 256`) each emitting
    /// `(bs+bs)/4 = 128` samples, landing the pages at granule 128 then 256
    /// (the final, EOS page).
    fn seek_test_stream() -> &'static [u8] {
        let id = id_header_packet(2, 44_100);
        let mut bytes = header_pages(&id, 0, 9);
        bytes.extend(ogg_page(0x00, 0, 9, 3, &audio_packet(10, 20)));
        bytes.extend(ogg_page(0x00, 128, 9, 4, &audio_packet(30, 40)));
        bytes.extend(ogg_page(0x04, 256, 9, 5, &audio_packet(50, 60)));
        Box::leak(bytes.into_boxed_slice())
    }

    fn decode_all_i32(dec: &mut VorbisDecoder<SliceSource<'_>, [u8; 256]>) -> Vec<i32> {
        let mut out = Vec::new();
        let mut buf = [0i32; 256];
        loop {
            let n = dec.decode_into(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    #[test]
    fn seek_lands_on_the_bracketing_page_and_reproduces_the_tail_of_a_full_decode() {
        // Two `VorbisDecoder`s (`full`/`dec`) live at once here — fine at
        // `MAX_CHANNELS == 2`, but the desktop profile's raised cap (Task 11)
        // needs the crate's established big-stack pattern for two concurrent
        // decoders, same as the real-file tests elsewhere in this module.
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                let bytes = seek_test_stream();

                // Full from-start decode: packet 1 primes (0 samples), packets 2/3
                // each emit 128 — 256 total, exactly the last page's granule (so this
                // also exercises the Task 10 exact-length trim, a no-op here since
                // there's nothing to trim).
                let mut full = VorbisDecoder::new(SliceSource::new(bytes), [0u8; 256]).unwrap();
                assert_eq!(full.total_frames(), Some(256));
                let full_pcm = decode_all_i32(&mut full);
                assert_eq!(full_pcm.len(), 256 * 2); // 256 frames x 2 channels

                // Seek to sample 128 — the exact bracket between packets 2 and 3 —
                // lands on the page ending at granule 128 (packet 2's page), discards
                // packet 2 to re-prime the overlap, and the very next decode_into
                // must reproduce EXACTLY packet 3's contribution: the tail of the
                // full decode above. (One packet per page here, so "discard the
                // landed page's first packet" and "discard the whole landed page"
                // coincide — this is the fixture where that equivalence holds.)
                let mut dec = VorbisDecoder::new(SliceSource::new(bytes), [0u8; 256]).unwrap();
                let landed = dec.seek_to(128).unwrap();
                assert_eq!(landed, 128);
                let seek_pcm = decode_all_i32(&mut dec);
                assert_eq!(
                    seek_pcm,
                    &full_pcm[128 * 2..],
                    "seek(128) must reproduce exactly the tail of a full decode"
                );
            })
            .expect("spawn decode thread")
            .join()
            .expect("decode thread panicked");
    }

    #[test]
    fn seek_to_zero_matches_a_fresh_decode_exactly() {
        // Two concurrent decoders (`fresh`/`dec`) — see the big-stack note on
        // `seek_lands_on_the_bracketing_page_and_reproduces_the_tail_of_a_full_decode`.
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                let bytes = seek_test_stream();

                let mut fresh = VorbisDecoder::new(SliceSource::new(bytes), [0u8; 256]).unwrap();
                let fresh_pcm = decode_all_i32(&mut fresh);

                // Partially decode, then seek back to 0: must reproduce the fresh
                // decode bit for bit, not merely approximately.
                let mut dec = VorbisDecoder::new(SliceSource::new(bytes), [0u8; 256]).unwrap();
                let mut probe = [0i32; 4];
                let _ = dec.decode_into(&mut probe).unwrap();
                assert_eq!(dec.seek_to(0).unwrap(), 0);
                let seek_pcm = decode_all_i32(&mut dec);
                assert_eq!(seek_pcm, fresh_pcm);
            })
            .expect("spawn decode thread")
            .join()
            .expect("decode thread panicked");
    }

    #[test]
    fn seek_beyond_end_clamps_to_total_frames_without_panicking() {
        let bytes = seek_test_stream();
        let mut dec = VorbisDecoder::new(SliceSource::new(bytes), [0u8; 256]).unwrap();
        assert_eq!(dec.total_frames(), Some(256));
        assert_eq!(dec.seek_to(1_000_000).unwrap(), 256);
        // Whatever trails after landing at/near the end must still decode
        // without error and stay within the exact declared total.
        let tail = decode_all_i32(&mut dec);
        assert!(tail.len() <= 256 * 2);
    }

    #[test]
    fn new_parses_setup_header_codebooks() {
        // Opening the full three-header stream must parse the setup header's
        // lone codebook (dims=1, entries=2, scalar) into the decoder.
        let packet = id_header_packet(2, 44_100);
        let dec = open_dec(&packet).expect("valid stream");
        assert_eq!(dec.codebook_count, 1);
        assert_eq!(dec.codebooks[0].entries, 2);
        assert_eq!(dec.codebooks[0].dims, 1);
        assert_eq!(dec.codebooks[0].lookup_type, 0);
    }

    #[test]
    fn new_rejects_malformed_setup_codebook() {
        // Corrupt the codebook's sync pattern inside an otherwise valid setup
        // packet: the setup parse must surface Malformed from `new`.
        let id = id_header_packet(2, 44_100);
        let mut setup = setup_header_packet();
        // Byte layout: [type 5][v o r b i s][count 0][sync b0 b1 b2 ...]. The
        // first sync byte is index 8; clobber it.
        setup[8] ^= 0xFF;
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, 0, 1, 0, &id));
        bytes.extend(ogg_page(0x00, 0, 1, 1, &comment_header_packet()));
        bytes.extend(ogg_page(0x00, 0, 1, 2, &setup));
        let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        assert_eq!(
            VorbisDecoder::new(SliceSource::new(bytes), [0u8; 256]).err(),
            Some(DecodeError::Malformed)
        );
    }

    #[test]
    fn new_rejects_setup_too_large_for_the_arena() {
        // A setup packet that does not fit the caller's storage is a graceful
        // Unsupported (the arena over-cap path), never a panic.
        let id = id_header_packet(2, 44_100);
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, 0, 1, 0, &id));
        bytes.extend(ogg_page(0x00, 0, 1, 1, &comment_header_packet()));
        bytes.extend(ogg_page(0x00, 0, 1, 2, &setup_header_packet()));
        let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        // 8-byte storage cannot even hold the ~18-byte raw setup packet.
        assert_eq!(
            VorbisDecoder::new(SliceSource::new(bytes), [0u8; 8]).err(),
            Some(DecodeError::Unsupported)
        );
    }

    #[test]
    fn new_rejects_missing_comment_header() {
        // Only the id header present: reading the comment header hits a clean
        // end of stream, and its (empty) common-header check fails.
        let id = id_header_packet(2, 44_100);
        let bytes = ogg_page(0x02, 0, 1, 0, &id);
        let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        assert!(VorbisDecoder::new(SliceSource::new(bytes), [0u8; 256]).is_err());
    }

    #[test]
    fn total_frames_reads_the_ogg_end_granule() {
        let id_packet = id_header_packet(1, 44_100);
        let mut bytes = header_pages(&id_packet, 0, 7);
        // A final audio page whose granule is the stream's real total
        // per-channel sample count.
        bytes.extend(ogg_page(0x04, 12_345, 7, 3, b"audio-packet"));
        let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());

        let dec = VorbisDecoder::new(SliceSource::new(bytes), [0u8; 256]).unwrap();
        assert_eq!(dec.total_frames(), Some(12_345));
        // Memoized: calling again must return the same value (not re-scan
        // and potentially disagree).
        assert_eq!(dec.total_frames(), Some(12_345));
    }

    #[test]
    fn total_frames_reports_the_header_pages_granule_when_there_is_no_audio() {
        // Header-only stream, all pages at granule 0: `end_granule`'s backward
        // scan finds that real granule (from the last header page) rather than
        // treating the stream as length-unknown.
        let id_packet = id_header_packet(1, 44_100);
        let bytes = header_pages(&id_packet, 0, 7);
        let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());

        let dec = VorbisDecoder::new(SliceSource::new(bytes), [0u8; 256]).unwrap();
        assert_eq!(dec.total_frames(), Some(0));
    }

    #[test]
    fn total_frames_is_none_when_no_page_reports_a_real_granule() {
        // `u64::MAX` means "no packet finished on this page" (spec); with every
        // page carrying it, `end_granule`'s backward scan finds nothing real
        // and `total_frames` must report `None`, not panic or fabricate a
        // value. (Header parsing is unaffected — granule is not read there.)
        let id_packet = id_header_packet(1, 44_100);
        let bytes = header_pages(&id_packet, u64::MAX, 7);
        let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());

        let dec = VorbisDecoder::new(SliceSource::new(bytes), [0u8; 256]).unwrap();
        assert_eq!(dec.total_frames(), None);
    }

    /// Task 5's fixture: a real ffmpeg/libvorbis-encoded stereo file
    /// (`testdata/tone-vorbis.ogg`, workspace root — see `testdata/generate.sh`
    /// for the shared tone signal and the Task 5 report for the exact
    /// `ffmpeg -c:a libvorbis -q:a 5` provenance). `new` must now parse its
    /// full setup header — codebooks, floors, residues, mappings, modes —
    /// without error, proving the whole Task 4–5 parse against a real
    /// encoder's output rather than only hand-built fixtures.
    #[test]
    fn new_parses_a_real_vorbis_files_three_headers() {
        let bytes: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../testdata/tone-vorbis.ogg"
        ));
        let storage = vec![0u8; 128 * 1024];
        let dec = VorbisDecoder::new(SliceSource::new(bytes), storage)
            .expect("a real ffmpeg/libvorbis setup header must parse cleanly");

        let info = dec.stream_info();
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.channels, 2);

        // Real-world setup shape (independently confirmed via a standalone
        // bit-level dump against this exact file — see the Task 5 report):
        // 42 codebooks, 2 floors, 2 residues, 2 mappings, 2 modes.
        assert_eq!(dec.codebook_count, 42);
        assert_eq!(dec.floor_count, 2);
        assert_eq!(dec.residue_count, 2);
        assert_eq!(dec.mapping_count, 2);
        assert_eq!(dec.mode_count, 2);
    }

    /// Task 6: decode real audio packets from `tone-vorbis.ogg` up to the
    /// pre-IMDCT spectrum and assert it is finite, in a sane range, and
    /// non-trivial — and that any naturally-unused channel is exactly zero. The
    /// decoder is large by value (~90 KiB of fixed DSP scratch), so this runs
    /// on a generous stack (the crate's established big-stack pattern).
    #[test]
    fn decodes_a_real_long_block_packet_to_finite_nontrivial_spectrum() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                let bytes: &[u8] = include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../testdata/tone-vorbis.ogg"
                ));
                let storage = vec![0u8; 128 * 1024];
                let mut dec = VorbisDecoder::new(SliceSource::new(bytes), storage)
                    .expect("real vorbis headers parse");
                let channels = usize::from(dec.stream_info().channels);
                let long_n = dec.blocksize_1 / 2;

                let mut found_long = false;
                let mut saw_nontrivial = false;
                for _ in 0..16 {
                    let plen = dec
                        .ogg
                        .get_mut()
                        .next_packet(&mut dec.packet_buf)
                        .expect("read audio packet");
                    if plen == 0 {
                        break;
                    }
                    let n = dec.decode_audio_packet(plen).expect("decode audio packet");
                    assert!(n > 0 && n <= MAX_N);

                    for ch in 0..channels {
                        for &s in &dec.spectrum[ch][..n] {
                            assert!(s.is_finite(), "spectrum must be finite");
                            assert!(s.abs() < 1.0e12, "spectrum must stay in a sane range");
                        }
                        if dec.unused[ch] {
                            assert!(
                                dec.spectrum[ch][..n].iter().all(|&s| s == 0.0),
                                "an unused (all-zero-floor) channel must be zero"
                            );
                        }
                        if dec.spectrum[ch][..n].iter().any(|&s| s.abs() > 1.0e-6) {
                            saw_nontrivial = true;
                        }
                    }

                    if n == long_n {
                        found_long = true;
                        let used_nonzero = (0..channels).any(|ch| {
                            !dec.unused[ch]
                                && dec.spectrum[ch][..n].iter().any(|&s| s.abs() > 1.0e-6)
                        });
                        assert!(used_nonzero, "a long-block spectrum must be non-trivial");
                        break;
                    }
                }
                assert!(found_long, "expected a long block among the first packets");
                assert!(saw_nontrivial, "expected non-trivial spectral energy");
            })
            .expect("spawn decode thread")
            .join()
            .expect("decode thread panicked");
    }

    /// Task 6: a channel whose floor decodes as "unused" (leading `nonzero` bit
    /// clear) is flagged and yields a zero spectrum. Driven with a hand-built
    /// short-block packet against the minimal setup fixture (whose residue is
    /// empty), so this isolates the unused-floor path end to end through the
    /// per-packet driver; non-trivial used-channel spectra are covered by the
    /// real-file test above.
    #[test]
    fn unused_floor_channel_yields_zero_spectrum() {
        let id = id_header_packet(2, 44_100);
        let mut dec = open_dec(&id).expect("valid stream");
        assert_eq!(dec.blocksize_0, 256);
        let n = dec.blocksize_0 / 2; // mode 0 has blockflag 0 (short block)

        // packet type 0; no mode bits (mode_count == 1 => ilog(0) == 0); no
        // window flags (short block); channel 0 floor nonzero=0 (unused);
        // channel 1 floor nonzero=1 with Y[0]/Y[1] (8 bits each, multiplier 1);
        // the residue is empty (end == 0), so nothing else is read.
        let mut w = TestBitWriter::new();
        w.write(0, 1); // packet type = audio
        w.write(0, 1); // channel 0 floor nonzero = 0 (unused)
        w.write(1, 1); // channel 1 floor nonzero = 1 (used)
        w.write(123, 8); // channel 1 Y[0]
        w.write(45, 8); // channel 1 Y[1]
        let packet = w.into_bytes();

        dec.packet_buf[..packet.len()].copy_from_slice(&packet);
        let got_n = dec
            .decode_audio_packet(packet.len())
            .expect("decode packet");
        assert_eq!(got_n, n);

        assert!(dec.unused[0], "channel 0 floor was unused");
        assert!(!dec.unused[1], "channel 1 floor was used");
        assert!(
            dec.spectrum[0][..n].iter().all(|&s| s == 0.0),
            "an all-zero-floor channel yields a zero spectrum"
        );
        assert!(dec.spectrum[1][..n].iter().all(|&s| s.is_finite()));
    }

    /// Task 8: the block-switching fixture (`tone-vorbis-blocks.ogg`, a 440 Hz
    /// tone plus sharp 3 kHz clicks every 0.25 s to force libvorbis transients)
    /// must actually contain **both** block sizes — otherwise the block-switching
    /// golden in `tests/decode.rs` would not exercise long↔short transitions at
    /// all. Tallying every packet's decoded `n` against `blocksize_0/1` is a
    /// direct structural check (independent of the SNR gate), and a permanent
    /// regression guard against ever swapping in an all-long-block fixture by
    /// mistake. Measured: 81 long + 46 short blocks (bs0=256, bs1=2048) — plenty
    /// of both directions plus sustained short runs.
    #[test]
    fn block_switching_fixture_contains_both_block_sizes() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                let bytes: &[u8] = include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/testdata/tone-vorbis-blocks.ogg"
                ));
                let storage = vec![0u8; 128 * 1024];
                let mut dec = VorbisDecoder::new(SliceSource::new(bytes), storage)
                    .expect("real vorbis headers parse");
                let mut long_count = 0usize;
                let mut short_count = 0usize;
                loop {
                    let plen = dec
                        .ogg
                        .get_mut()
                        .next_packet(&mut dec.packet_buf)
                        .expect("read audio packet");
                    if plen == 0 {
                        break;
                    }
                    let n = dec.decode_audio_packet(plen).expect("decode audio packet");
                    if n == dec.blocksize_1 / 2 {
                        long_count += 1;
                    } else if n == dec.blocksize_0 / 2 {
                        short_count += 1;
                    } else {
                        panic!("unexpected block size n={n}");
                    }
                }
                assert!(long_count > 0, "fixture has no long blocks");
                assert!(
                    short_count > 0,
                    "fixture has no short blocks — doesn't test block switching"
                );
            })
            .expect("spawn decode thread")
            .join()
            .expect("decode thread panicked");
    }

    /// Task 9: the coupled-stereo fixture (`tone-vorbis-stereo.ogg`) must
    /// actually declare **both** channel coupling and a residue type-2 config
    /// in its setup header — otherwise the stereo golden in `tests/decode.rs`
    /// wouldn't exercise the coupling/type-2 code path the Task-6 review
    /// flagged as the first suspect for a channel-swap/stereo-image bug. A
    /// real ffmpeg/libvorbis stereo encode shares its one mapping across every
    /// mode, so a setup-level check (rather than tracing which mapping each
    /// packet selects) is sufficient and much simpler; `VorbisDecoder` is
    /// large by value (no-alloc DSP scratch), so — mirroring every other
    /// real-file test in this module — construction runs on a generous stack.
    #[test]
    fn stereo_fixture_setup_declares_coupling_and_residue_type2() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                let bytes: &[u8] = include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/testdata/tone-vorbis-stereo.ogg"
                ));
                let storage = vec![0u8; 128 * 1024];
                let dec = VorbisDecoder::new(SliceSource::new(bytes), storage)
                    .expect("real stereo vorbis headers parse");
                assert_eq!(dec.stream_info.channels, 2);
                let has_coupling = dec.mappings[..dec.mapping_count]
                    .iter()
                    .any(|m| m.coupling_steps > 0);
                let has_type2 = dec.residues[..dec.residue_count]
                    .iter()
                    .any(|r| r.residue_type == 2);
                assert!(
                    has_coupling,
                    "stereo fixture setup declares no channel coupling"
                );
                assert!(has_type2, "stereo fixture setup declares no residue type 2");
            })
            .expect("spawn decode thread")
            .join()
            .expect("decode thread panicked");
    }
}
