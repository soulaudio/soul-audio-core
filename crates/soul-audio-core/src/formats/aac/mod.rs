//! AAC-LC decoder. The submodules provide the bitstream primitives (`bits`),
//! the Huffman codebook decode (`huffman`), the static tables (`tables`), the
//! `raw_data_block`/ICS syntax parser (`syntax`), the inverse quant + stereo +
//! TNS DSP (`dsp`), and the IMDCT filterbank (`filterbank`). This module's
//! [`AacDecoder`] is the top-level orchestration that wires them into the
//! crate's [`Decoder`] seam.
//!
//! Two container entry paths converge on one core (see the design spec §2): an
//! MP4/`.m4a` `mp4a` sample entry (via [`Mp4Reader`]) and a raw ADTS `.aac`
//! elementary stream (via [`AdtsReader`]). Both hand the same self-delimiting
//! `raw_data_block` bytes to [`decode_rdb`]; the only difference is framing —
//! ADTS headers give exact frame lengths, while MP4 access units self-delimit
//! (each `raw_data_block` ends with `ID_END` + byte-align), so — like ALAC —
//! no `stsz` table is needed.

pub(crate) mod bits;
pub(super) mod dsp;
pub(crate) mod filterbank;
mod huffman;
// `ps` (Parametric Stereo) is only ever driven from inside `sbr::SbrState`
// (HE-AACv2) — it has no purpose without SBR, so it's gated off alongside it
// under `embedded-profile` (decoder-memory-externalization spec, Option A).
#[cfg(not(feature = "embedded-profile"))]
pub(crate) mod ps;
/// SBR (HE-AAC) decode. Desktop only: under `embedded-profile` the whole
/// module (and its substantial const tables / per-channel QMF state) is
/// dropped from the build — the LC core decodes and the extension payload is
/// skip-consumed as a no-op (see `syntax::decode_fil`) instead of
/// reconstructing the high band. This is the bulk of AAC's embedded-profile
/// size win: `sbr::SbrState` (133,712 B) + `sbr::SbrOutput` (16,400 B) is most
/// of the pre-externalization 200,536 B `AacDecoder`.
#[cfg(not(feature = "embedded-profile"))]
pub(crate) mod sbr;
mod syntax;
mod tables;

use crate::decoder::{DecodeError, Decoder, SeekableDecoder};
use crate::demux::adts::{AacConfig, AdtsReader};
use crate::demux::mp4::{Mp4Codec, Mp4Reader};
use crate::demux::{Demuxer, MatroskaDemuxer};
use crate::formats::scratch::ScratchCursor;
use crate::io::{SeekSource, Source};
use crate::model::{SampleFormat, StreamInfo};

use filterbank::Filterbank;

/// One AAC-LC frame is 1024 samples/channel; an SBR (or SBR+PS) frame is
/// **2048** samples/channel (2× rate, `sbr-reference.md` §0). The carry buffer
/// holds exactly one decoded stereo frame — `2048 · 2 ch` interleaved i32, the
/// largest a single `decode_rdb` can produce (SBR/PS 2×-rate stereo; the LC
/// path is half that) — which `decode_into` drains across calls. Sized to one
/// frame (not two): `decode_rdb` overwrites `pcm` per frame and never
/// accumulates, so a second frame's headroom was pure slack. Keeping
/// `AacDecoder` compact matters because it is the largest [`AnyDecoder`]
/// variant, embedded by value on the (default-stack) construction path.
/// Maximum channels this decoder reconstructs. The desktop profile (default)
/// covers up to 7.1 (`channel_configuration` 7): mono/stereo (1/2), 5.1 (6),
/// and 7.1 (8) — every standard AAC layout. The large per-channel
/// [`syntax::DecodedFrame`] scales with this but stays a transient on the
/// (big-stack) decode path, never embedded in the persistent
/// `AacDecoder`/[`AnyDecoder`](crate::AnyDecoder) value.
///
/// The **`embedded-profile`** feature (decoder-memory externalization spec,
/// Lever A) tightens this to stereo: the DAP consumer this profile targets
/// is stereo-only and downmixes surround to stereo at import, so the
/// `MAX_CHANNELS`-scaled filterbank/SBR/`DecodedFrame` buffers never need
/// more than 2 channels on that target. [`AacDecoder::build`]'s
/// `channels > MAX_CHANNELS` check, and `decode_element`'s own per-block
/// `next_ch >= MAX_CHANNELS` bound (`syntax.rs`), already turn a wider stream
/// into a graceful [`DecodeError::Unsupported`] under either cap, never a
/// panic.
#[cfg(not(feature = "embedded-profile"))]
pub(crate) const MAX_CHANNELS: usize = 8;
#[cfg(feature = "embedded-profile")]
pub(crate) const MAX_CHANNELS: usize = 2;

/// Interleaved PCM scratch for one decoded frame, carved from the caller-
/// provided `scratch: P` (decoder-memory-externalization spec §7 — the same
/// `ScratchCursor` pattern [`crate::formats::flac::FlacDecoder`]/
/// [`crate::formats::alac::AlacDecoder`] use). `decode_rdb` overwrites it per
/// frame and never accumulates, so this sizes the scratch requirement, not
/// `AacDecoder` itself.
///
/// Desktop: the larger of the LC multichannel write (1024 samples ×
/// `MAX_CHANNELS`) and the ≤2-channel SBR write (2048 × 2 at the doubled
/// HE-AAC rate). Explicitly `max`'d rather than just `1024 * MAX_CHANNELS`:
/// SBR/PS stay mono/stereo-only regardless of the active channel cap
/// ([`sbr::SbrOutput::pcm`] is a fixed `[[f32; 2048]; 2]`, and `decode_rdb`'s
/// SBR branch only runs when `n <= 2`), so that write's size (4096) does
/// **not** shrink with `MAX_CHANNELS`. Without the `max`, `PCM_CAP` would drop
/// to `1024 * 2 = 2048` — smaller than the 4096-sample SBR write — and a
/// stereo HE-AAC stream would write out of bounds instead of decoding.
///
/// `embedded-profile`: SBR is gated off entirely (Option A), so there is no
/// 2×-rate write to size for — just the LC write, `1024 * MAX_CHANNELS` (=
/// 2048, `MAX_CHANNELS` = 2), half the desktop-formula's 4096-sample floor.
#[cfg(not(feature = "embedded-profile"))]
const PCM_CAP: usize = if 1024 * MAX_CHANNELS > 2048 * 2 {
    1024 * MAX_CHANNELS
} else {
    2048 * 2
};
#[cfg(feature = "embedded-profile")]
const PCM_CAP: usize = 1024 * MAX_CHANNELS;

/// Input byte buffer: one whole `raw_data_block` must fit. ADTS's 13-bit
/// `aac_frame_length` caps a frame at 8191 bytes (payload a little less), and a
/// real AAC-LC stereo access unit is far smaller; 8192 covers both the ADTS
/// `next_frame` target and the MP4 sliding-window that must always hold a
/// complete access unit before decoding it.
const AU_CAP: usize = 8192;

/// Full-scale factor turning the filterbank's ISO `2/N`-scaled float time
/// samples into 16-bit PCM. The IMDCT (`filterbank.rs`) reconstructs to the
/// encoder's coefficient units — the same inverse-quant convention ffmpeg uses
/// — which already land in the ±32768 range, so no extra gain is applied
/// (empirically the ours-vs-ffmpeg amplitude ratio is 1.000; see the Task 8
/// report). Kept as a named constant so a future scale recalibration is a
/// one-line change, exactly where the design spec says the scale lives.
const OUTPUT_GAIN: f32 = 1.0;

/// Round an `f32` time sample to the `i16` range, stored in an `i32`. Manual
/// round-half-away-from-zero (`s ± 0.5` then truncate) — no `f32::round`, which
/// is std-only — with saturation to `[-32768, 32767]`. `NaN` maps to `0` (the
/// saturating `as i32` cast), so this never produces a garbage sample.
fn round_i16(s: f32) -> i32 {
    let r = if s >= 0.0 { s + 0.5 } else { s - 0.5 };
    if r >= 32767.0 {
        32767
    } else if r <= -32768.0 {
        -32768
    } else {
        r as i32
    }
}

/// [`decode_rdb_lc`]'s return shape: the parsed frame, the per-channel LC
/// time-domain output, the channel count, and the consumed byte length.
/// Factored into a named alias (clippy `type_complexity`).
type LcCoreDecode = (
    syntax::DecodedFrame,
    [[f32; 1024]; MAX_CHANNELS],
    usize,
    usize,
);

/// Shared LC-core decode stage (desktop AND `embedded-profile`): syntax parse
/// → per-channel `dequantize` → per-CPE `apply_stereo` (M/S + intensity, needs
/// both of a pair's dequantized spectra; a 5.1/7.1 block has several CPEs) →
/// per-channel `apply_tns` → per-channel `imdct_and_overlap`. Returns the
/// parsed [`syntax::DecodedFrame`] (its `sbr` field, present on desktop only —
/// see the field's doc comment — is read by the desktop [`decode_rdb`] after
/// this returns), the per-channel LC time-domain output, the channel count,
/// and the exact access-unit byte length consumed (from the reader's
/// byte-aligned end position — what the MP4 sliding window advances by; ADTS
/// ignores it, its header already framed the block).
///
/// The caller's [`Filterbank`] is stateful (per-channel overlap carried across
/// frames), so a channel keeps the same index every frame. Never panics:
/// `decode_raw_data_block` bounds-checks every field, and every DSP stage
/// independently re-clamps its indices (see `dsp`/`filterbank`).
fn decode_rdb_lc(
    rdb: &[u8],
    config: AacConfig,
    sfi: u8,
    fb: &mut Filterbank,
) -> Result<LcCoreDecode, DecodeError> {
    let mut r = bits::BitReader::new(rdb);
    let mut frame = syntax::decode_raw_data_block(&mut r, config)?;
    // `decode_raw_data_block` byte-aligns before returning, so this is the exact
    // access-unit length in bytes.
    let consumed = r.bit_pos().div_ceil(8);
    let n = frame.n_channels as usize;
    let mut outs = [[0f32; 1024]; MAX_CHANNELS];
    if n == 0 {
        // A block carrying only DSE/FIL/PCE (no audio element) — no samples,
        // but real bytes were consumed, so the MP4 window still advances. The
        // loops below are no-ops for `n == 0` anyway; returning early just
        // skips the (pointless) dequant/stereo/tns/imdct calls.
        return Ok((frame, outs, n, consumed));
    }

    for ch in 0..n {
        dsp::dequantize(&mut frame.channels[ch]);
    }

    // M/S + intensity stereo, applied per CPE (each carries its own side info —
    // a 5.1/7.1 block has several CPEs). Both are no-ops when the bitstream
    // didn't signal them, and `apply_stereo` re-validates the shared window
    // layout, so this is safe for any pair. `ms_used` is a `Copy` bool matrix,
    // so copying it out releases the `frame.stereo` borrow before the disjoint
    // `frame.channels` split.
    for si in 0..frame.n_stereo as usize {
        let ch0 = frame.stereo[si].ch0 as usize;
        let ms_mask_present = frame.stereo[si].ms_mask_present;
        let ms_used = frame.stereo[si].ms_used;
        let (a, b) = frame.channels.split_at_mut(ch0 + 1);
        dsp::apply_stereo(&mut a[ch0], &mut b[0], ms_mask_present, &ms_used);
    }

    for ch in 0..n {
        dsp::apply_tns(&mut frame.channels[ch], sfi);
    }

    // LC core PCM for every channel (also the SBR analysis QMF's input on
    // desktop).
    for (ch, out) in outs.iter_mut().enumerate().take(n) {
        fb.imdct_and_overlap(ch, &frame.channels[ch], out);
    }

    Ok((frame, outs, n, consumed))
}

/// Decode one `raw_data_block` all the way to interleaved 16-bit-range PCM in
/// `pcm`, returning `(interleaved_samples_written, bytes_consumed, sbr_applied)`.
///
/// Desktop: runs [`decode_rdb_lc`], then — if a fill element in this block
/// carried `EXT_SBR_DATA` — drives the §10 SBR reconstruction over the core
/// PCM, emitting **2048** interleaved samples/channel (2× rate) instead of
/// 1024. Mono/stereo only — the SBR drive and its QMF scratch are sized for
/// ≤2 channels, so a (rare) multichannel HE-AAC stream decodes its LC core
/// without SBR rather than misindex. The LC-only path is byte-identical when
/// no SBR fill element was present.
#[cfg(not(feature = "embedded-profile"))]
#[allow(clippy::too_many_arguments)]
fn decode_rdb(
    rdb: &[u8],
    config: AacConfig,
    sfi: u8,
    fb: &mut Filterbank,
    sbr: &mut sbr::SbrState,
    sbr_out: &mut sbr::SbrOutput,
    output_rate: u32,
    pcm: &mut [i32],
) -> Result<(usize, usize, bool), DecodeError> {
    let (frame, outs, n, consumed) = decode_rdb_lc(rdb, config, sfi, fb)?;
    if n == 0 {
        return Ok((0, consumed, false));
    }

    // SBR (HE-AAC): a fill element in this block carried EXT_SBR_DATA.
    if let Some(loc) = frame.sbr.filter(|_| n <= 2) {
        // SBR consumes the ≤2 core channels; hand it a fixed 2-element view of
        // the wider multichannel `outs` (always in range — `outs` is length
        // `MAX_CHANNELS` ≥ 2, and this arm only runs when `n <= 2`).
        let core: &[[f32; 1024]; 2] = outs[..2]
            .try_into()
            .expect("outs has >= 2 channels for the SBR path");
        let applied = sbr::decode_fill_sbr(
            rdb,
            loc.start_bit,
            loc.end_bit,
            loc.crc,
            loc.id_cpe,
            sbr,
            core,
            n,
            output_rate,
            sbr_out,
        )?;
        if applied {
            // R6: Parametric Stereo may have upmixed a mono core to stereo, so
            // interleave by the channel count SBR actually produced
            // (`sbr_out.channels`), not the core element count `n`. The caller
            // reads `sbr_out.channels` after this returns to promote
            // `AacDecoder.channels` 1->2 (mirroring the `sbr_confirmed` rate
            // promotion). This 2x-rate stereo write of 2048·2 samples is the
            // largest a frame produces and exactly fills `PCM_CAP` (= 2048·2).
            let out_ch = sbr_out.channels.clamp(1, 2);
            let frames = 2048usize;
            for i in 0..frames {
                for ch in 0..out_ch {
                    pcm[i * out_ch + ch] = round_i16(sbr_out.pcm[ch][i] * OUTPUT_GAIN);
                }
            }
            return Ok((frames * out_ch, consumed, true));
        }
    }

    let frames = 1024usize;
    for i in 0..frames {
        for ch in 0..n {
            pcm[i * n + ch] = round_i16(outs[ch][i] * OUTPUT_GAIN);
        }
    }
    Ok((frames * n, consumed, false))
}

/// `embedded-profile`: AAC-LC core only — SBR is gated off entirely (Option
/// A), so this is [`decode_rdb_lc`] plus the interleave/round, nothing else.
/// The returned `bool` is always `false` (no SBR path exists here); kept so
/// the three `decode_next_rdb_*` callers share one return shape with the
/// desktop build.
#[cfg(feature = "embedded-profile")]
fn decode_rdb(
    rdb: &[u8],
    config: AacConfig,
    sfi: u8,
    fb: &mut Filterbank,
    pcm: &mut [i32],
) -> Result<(usize, usize, bool), DecodeError> {
    let (_frame, outs, n, consumed) = decode_rdb_lc(rdb, config, sfi, fb)?;
    if n == 0 {
        return Ok((0, consumed, false));
    }
    let frames = 1024usize;
    for i in 0..frames {
        for ch in 0..n {
            pcm[i * n + ch] = round_i16(outs[ch][i] * OUTPUT_GAIN);
        }
    }
    Ok((frames * n, consumed, false))
}

/// The frame-source seam: yields the next access unit's `raw_data_block` bytes
/// plus (at construction) the stream config, over either container.
///
/// The `Matroska` variant embeds a `MatroskaDemuxer` (which carries an inline
/// `CodecPrivate` buffer), so it is larger than the MP4/ADTS readers; boxing is
/// impossible under `#![no_std]`, so the size gap is accepted.
#[allow(clippy::large_enum_variant)]
enum FrameSrc<S> {
    /// MP4/`.m4a` — access units concatenated in `mdat`, framed by decode.
    Mp4(Mp4Reader<S>),
    /// Raw ADTS `.aac` — each frame's `raw_data_block` framed by its header.
    Adts(AdtsReader<S>),
    /// Matroska / WebM — each `SimpleBlock` is one `raw_data_block` (no ADTS
    /// header); the config comes from the track's `CodecPrivate` (ASC).
    Matroska(MatroskaDemuxer<S>),
}

/// A streaming AAC-LC decoder over an MP4/`.m4a` or raw ADTS [`Source`],
/// producing interleaved 16-bit-range `i32` PCM. Mono through 7.1 (up to
/// [`MAX_CHANNELS`]); on desktop, HE-AAC (SBR/PS) is mono/stereo only. No
/// allocation: every buffer is fixed-size and owned by value except the
/// decoded-PCM carry buffer (see below); the [`Filterbank`] overlap state and
/// the decoded-PCM carry buffer persist across `decode_into` calls.
///
/// The decoded-PCM carry buffer is **caller-provided scratch** `P`
/// (`AsRef<[i32]> + AsMut<[i32]>`), carved via [`crate::formats::scratch::ScratchCursor`]
/// — the SAME shared scratch [`crate::formats::flac::FlacDecoder`]/
/// [`crate::formats::alac::AlacDecoder`]/[`crate::formats::wavpack::WavpackDecoder`]
/// use (decoder-memory-externalization spec §7). The [`Filterbank`]'s own
/// f32 overlap state (8 KB) stays inline — small enough that externalizing it
/// would need f32/bytemuck handling in `P` for no real size win.
///
/// **`embedded-profile`** (decoder-memory externalization spec, Option A):
/// SBR is gated off entirely — the decoder reconstructs the AAC-LC core only
/// and skip-consumes any `EXT_SBR_DATA` extension payload as a no-op (never a
/// panic, reject, or bitstream desync); `stream_info()` reports the **core**
/// rate (no SBR 2× doubling). An HE-AAC source should be transcoded to AAC-LC
/// or FLAC at desktop import for full quality on that profile (see
/// `docs/superpowers/2026-11-03-metadata-expansion-and-dap-index-exploration.md`'s
/// "HE-AAC fold-down" note, mirroring the existing surround→stereo downmix).
/// Desktop (no feature) keeps SBR/PS fully intact and byte-identical.
pub struct AacDecoder<S, P> {
    src: FrameSrc<S>,
    config: AacConfig,
    sfi: u8,
    channels: u16,
    fb: Filterbank,
    /// Shared input buffer: ADTS's `next_frame` target, or the MP4 sliding
    /// access-unit window (`mp4_filled` valid bytes, `mp4_used` already decoded).
    inbuf: [u8; AU_CAP],
    mp4_filled: usize,
    mp4_used: usize,
    total_samples: Option<u64>,
    /// SBR (HE-AAC) decode state — always present, starts inactive (pure
    /// upsample-only, `kx=32 M=0`). Costs nothing on an LC-only stream: the
    /// drive only runs when a fill element carries `EXT_SBR_DATA`.
    ///
    /// Desktop only — gated off under `embedded-profile` (see the struct doc
    /// comment); this is the bulk of AAC's embedded-profile size win.
    #[cfg(not(feature = "embedded-profile"))]
    sbr: sbr::SbrState,
    /// 2×-rate PCM scratch the SBR drive fills (reused each frame). Desktop only.
    #[cfg(not(feature = "embedded-profile"))]
    sbr_out: sbr::SbrOutput,
    /// SBR operating/output rate (= 2×core, or the explicit `ext_sample_rate`).
    /// Desktop only — under `embedded-profile` [`Self::reported_rate`]/
    /// [`Self::output_samples_per_au`] always use the core rate, so there is
    /// nothing for this field to hold.
    #[cfg(not(feature = "embedded-profile"))]
    output_rate: u32,
    /// `true` once SBR output has been produced (explicit ASC signal, or the
    /// first `EXT_SBR_DATA` frame) — flips `stream_info().sample_rate` to 2×.
    /// Desktop only; see [`Self::he_aac_active`].
    #[cfg(not(feature = "embedded-profile"))]
    sbr_confirmed: bool,
    /// Caller-provided scratch the decoded-PCM carry buffer is carved from
    /// (see the struct doc comment).
    scratch: P,
    /// i32-slot offset of the `PCM_CAP`-long decoded-PCM buffer within
    /// [`Self::scratch`].
    pcm_off: usize,
    /// Decoded interleaved PCM awaiting hand-off, `[pcm_pos, pcm_len)` within
    /// the `scratch[pcm_off..pcm_off + PCM_CAP]` window.
    pcm_len: usize,
    pcm_pos: usize,
    eof: bool,
}

impl<S: Source, P: AsRef<[i32]> + AsMut<[i32]>> AacDecoder<S, P> {
    /// Build over a parsed MP4 reader positioned at `mdat`. Its sample entry
    /// must be `mp4a` (AAC); an `alac` reader is [`DecodeError::Unsupported`]
    /// (route it to `AlacDecoder` instead). `scratch` is carved for the
    /// decoded-PCM carry buffer (see the struct doc comment); it must be at
    /// least [`crate::dispatch::decoder_scratch_len`] samples long.
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] for a non-AAC sample entry, a non-LC object
    /// type, a channel count outside 1..=2, or a `scratch` too small to hold
    /// the decoded-PCM buffer; [`DecodeError::Malformed`] for an unresolvable
    /// sample rate.
    pub fn new_mp4_in(reader: Mp4Reader<S>, scratch: P) -> Result<Self, DecodeError> {
        let config = match reader.codec() {
            Mp4Codec::Aac(c) => c,
            Mp4Codec::Alac(_) => return Err(DecodeError::Unsupported),
        };
        // A fragmented stream carries no `mdhd` total; report length unknown
        // (like ADTS) and let decode run until the `mdat` fragments drain.
        let total = if reader.is_fragmented() {
            None
        } else {
            Some(reader.total_samples())
        };
        Self::build(FrameSrc::Mp4(reader), config, total, scratch)
    }

    /// Build over an ADTS reader (its first header already validated). ADTS is
    /// streaming, so there's no declared length ([`Self::total_frames`] is
    /// `None`). `scratch` is as [`Self::new_mp4_in`].
    ///
    /// # Errors
    /// As [`Self::new_mp4_in`] (config validation).
    pub fn new_adts_in(reader: AdtsReader<S>, scratch: P) -> Result<Self, DecodeError> {
        let config = reader.config();
        Self::build(FrameSrc::Adts(reader), config, None, scratch)
    }

    /// Build over a Matroska/WebM demuxer positioned at the first audio block.
    /// `config` is the track's `AudioSpecificConfig` (from `CodecPrivate`, parsed
    /// by [`crate::demux::mp4::parse_audio_specific_config`]). Each `SimpleBlock`
    /// is one raw access unit; no declared length ([`Self::total_frames`] =
    /// `None`), mirroring ADTS. `scratch` is as [`Self::new_mp4_in`].
    ///
    /// # Errors
    /// As [`Self::new_mp4_in`] (config validation).
    pub fn new_matroska_in(
        demux: MatroskaDemuxer<S>,
        config: AacConfig,
        scratch: P,
    ) -> Result<Self, DecodeError> {
        Self::build(FrameSrc::Matroska(demux), config, None, scratch)
    }

    fn build(
        src: FrameSrc<S>,
        config: AacConfig,
        total_samples: Option<u64>,
        scratch: P,
    ) -> Result<Self, DecodeError> {
        if config.object_type != 2 {
            return Err(DecodeError::Unsupported); // AAC-LC only
        }
        if config.channels == 0 || config.channels as usize > MAX_CHANNELS {
            return Err(DecodeError::Unsupported); // mono … 7.1 (channel_configuration 1-7)
        }
        // Validate the sample rate up front (also the `sfi` the DSP stages need)
        // so a bad ASC/header fails at construction, not mid-decode.
        let sfi = tables::resolve_sfi(config.sample_rate)?;
        // SBR output rate: the explicit ASC extension rate if signalled, else
        // 2×core (the dual-rate default). Reported by `stream_info` only once
        // SBR is confirmed (up front for explicit ASC, after the first
        // EXT_SBR_DATA frame for implicit ADTS). Desktop only — embedded-profile
        // never promotes off the core rate.
        #[cfg(not(feature = "embedded-profile"))]
        let output_rate = config
            .ext_sample_rate
            .unwrap_or_else(|| config.sample_rate.saturating_mul(2));
        let mut cursor = ScratchCursor::new(scratch.as_ref().len());
        let pcm_off = cursor.alloc(PCM_CAP)?;
        Ok(Self {
            src,
            config,
            sfi,
            channels: u16::from(config.channels),
            fb: Filterbank::new(),
            inbuf: [0; AU_CAP],
            mp4_filled: 0,
            mp4_used: 0,
            total_samples,
            #[cfg(not(feature = "embedded-profile"))]
            sbr: sbr::SbrState::default(),
            #[cfg(not(feature = "embedded-profile"))]
            sbr_out: sbr::SbrOutput::default(),
            #[cfg(not(feature = "embedded-profile"))]
            output_rate,
            #[cfg(not(feature = "embedded-profile"))]
            sbr_confirmed: config.sbr_signalled,
            scratch,
            pcm_off,
            pcm_len: 0,
            pcm_pos: 0,
            eof: false,
        })
    }

    /// Read-only view of the decoded-PCM carry buffer (`PCM_CAP` i32 slots
    /// carved from [`Self::scratch`]).
    fn pcm_ref(&self) -> &[i32] {
        &self.scratch.as_ref()[self.pcm_off..self.pcm_off + PCM_CAP]
    }

    /// Total per-channel output frames from the container's `mdhd` (MP4), or —
    /// for a raw ADTS stream once [`Self::resolve_length`] has run — from the CBR
    /// byte estimate. `None` for an unresolved, VBR, or fragmented stream.
    ///
    /// The ADTS value scales with the current SBR state ([`Self::output_samples_per_au`]):
    /// a resolved frame count times 1024 (AAC-LC) or 2048 (HE-AAC, once SBR is
    /// confirmed). Either way it stays consistent with the reported output rate,
    /// so a duration computed as `total_frames / sample_rate` is correct in both
    /// states.
    ///
    /// The MP4/Matroska value is the container's declared `mdhd` duration — for
    /// an explicit-ASC HE-AAC file (`config.sbr_signalled`) that's already in
    /// the 2× SBR output domain (desktop's actual decode domain, so no
    /// adjustment needed there). `embedded-profile` never reconstructs SBR
    /// (decodes the core at 1×), so it halves that declared total back to the
    /// core domain — see the `embedded-profile` sibling below.
    #[must_use]
    #[cfg(not(feature = "embedded-profile"))]
    pub fn total_frames(&self) -> Option<u64> {
        match &self.src {
            FrameSrc::Adts(r) => r.cbr_frames().map(|f| f * self.output_samples_per_au()),
            FrameSrc::Mp4(_) | FrameSrc::Matroska(_) => self.total_samples,
        }
    }

    /// `embedded-profile` sibling of the above: see its doc comment for the
    /// MP4/Matroska halving rationale. ADTS is unaffected — `output_samples_per_au`
    /// is already the core-only 1024 there.
    #[must_use]
    #[cfg(feature = "embedded-profile")]
    pub fn total_frames(&self) -> Option<u64> {
        match &self.src {
            FrameSrc::Adts(r) => r.cbr_frames().map(|f| f * self.output_samples_per_au()),
            FrameSrc::Mp4(_) | FrameSrc::Matroska(_) => self
                .total_samples
                .map(|t| if self.config.sbr_signalled { t / 2 } else { t }),
        }
    }

    /// Output samples per access unit: 1024 core samples, doubled once dual-rate
    /// SBR is confirmed (output rate = 2× core). Desktop only; `embedded-profile`
    /// never leaves the core rate — see the `embedded-profile` sibling below.
    #[cfg(not(feature = "embedded-profile"))]
    fn output_samples_per_au(&self) -> u64 {
        if self.sbr_confirmed && self.config.sample_rate != 0 {
            1024 * u64::from((self.output_rate / self.config.sample_rate).max(1))
        } else {
            1024
        }
    }

    /// `embedded-profile`: always the AAC-LC core rate's 1024 samples/AU — SBR
    /// (which would double this) is gated off entirely.
    #[cfg(feature = "embedded-profile")]
    fn output_samples_per_au(&self) -> u64 {
        1024
    }

    /// The sample rate [`Decoder::stream_info`]/the time-based seeks report:
    /// the 2× SBR output rate once confirmed on desktop, the core rate
    /// otherwise (and always on `embedded-profile`, which never confirms SBR).
    #[cfg(not(feature = "embedded-profile"))]
    fn reported_rate(&self) -> u32 {
        if self.sbr_confirmed {
            self.output_rate
        } else {
            self.config.sample_rate
        }
    }
    #[cfg(feature = "embedded-profile")]
    fn reported_rate(&self) -> u32 {
        self.config.sample_rate
    }

    /// Whether HE-AAC (SBR) output is active — `embedded-profile` never
    /// activates it (SBR is gated off entirely), so this is a hardwired
    /// `false` there; on desktop it mirrors `sbr_confirmed`.
    #[cfg(not(feature = "embedded-profile"))]
    fn he_aac_active(&self) -> bool {
        self.sbr_confirmed
    }
    #[cfg(feature = "embedded-profile")]
    fn he_aac_active(&self) -> bool {
        false
    }

    /// Gapless encoder delay (leading priming, per-channel samples) from the
    /// MP4 `elst` edit list; `0` for raw ADTS (no container edit list) or an MP4
    /// without one. The decoder emits the full decode — the [`Deck`] trims this
    /// many samples off the front for gapless playback.
    ///
    /// Like [`Self::total_frames`], an explicit-ASC HE-AAC file's `elst` is
    /// declared in the 2× SBR output domain; `embedded-profile` (core-only, no
    /// SBR reconstruction) halves it back to the core domain it actually
    /// decodes at.
    ///
    /// [`Deck`]: crate::deck::Deck
    fn mp4_encoder_delay(&self) -> u64 {
        match &self.src {
            FrameSrc::Mp4(r) => {
                let d = r.encoder_delay();
                #[cfg(feature = "embedded-profile")]
                {
                    if self.config.sbr_signalled { d / 2 } else { d }
                }
                #[cfg(not(feature = "embedded-profile"))]
                {
                    d
                }
            }
            FrameSrc::Adts(_) | FrameSrc::Matroska(_) => 0,
        }
    }

    /// Gapless presented length (per-channel frames to emit after the delay)
    /// from the MP4 `elst`; `None` for ADTS (no container edit list) or an MP4
    /// without one. See [`Self::mp4_encoder_delay`]'s note on the
    /// `embedded-profile` domain halving.
    fn mp4_playable_frames(&self) -> Option<u64> {
        match &self.src {
            FrameSrc::Mp4(r) => {
                let p = r.playable_frames();
                #[cfg(feature = "embedded-profile")]
                {
                    p.map(|v| if self.config.sbr_signalled { v / 2 } else { v })
                }
                #[cfg(not(feature = "embedded-profile"))]
                {
                    p
                }
            }
            FrameSrc::Adts(_) | FrameSrc::Matroska(_) => None,
        }
    }

    /// Decode the next access unit into `self.pcm`, looping past any audio-less
    /// blocks. `Ok(false)` at end of stream.
    fn fill_next_frame(&mut self) -> Result<bool, DecodeError> {
        loop {
            let samples = match self.decode_next_rdb()? {
                Some(s) => s,
                None => return Ok(false),
            };
            if samples > 0 {
                self.pcm_len = samples;
                self.pcm_pos = 0;
                return Ok(true);
            }
            // Zero-sample block (DSE/FIL/PCE only) — keep decoding.
        }
    }

    /// Decode one `raw_data_block`, dispatching on the container. `Ok(None)` at
    /// end of stream; `Ok(Some(samples))` otherwise (`samples` may be 0).
    fn decode_next_rdb(&mut self) -> Result<Option<usize>, DecodeError> {
        match &self.src {
            FrameSrc::Mp4(_) => self.decode_next_rdb_mp4(),
            FrameSrc::Adts(_) => self.decode_next_rdb_adts(),
            FrameSrc::Matroska(_) => self.decode_next_rdb_matroska(),
        }
    }

    /// MP4 path: slide out the previously-decoded bytes, refill the window from
    /// `mdat`, decode the leading access unit, and record its byte length.
    fn decode_next_rdb_mp4(&mut self) -> Result<Option<usize>, DecodeError> {
        // Drop the previous frame's bytes to the front.
        if self.mp4_used > 0 {
            if self.mp4_used < self.mp4_filled {
                self.inbuf.copy_within(self.mp4_used..self.mp4_filled, 0);
                self.mp4_filled -= self.mp4_used;
            } else {
                self.mp4_filled = 0;
            }
            self.mp4_used = 0;
        }
        // Refill from the mdat stream (up to capacity). `read_audio` bounds the
        // read to the current `mdat` and, for a fragmented file, transparently
        // crosses into the next fragment's `mdat`; it returns 0 at true EOF.
        while self.mp4_filled < AU_CAP {
            let start = self.mp4_filled;
            let n = {
                let reader = match &mut self.src {
                    FrameSrc::Mp4(r) => r,
                    FrameSrc::Adts(_) | FrameSrc::Matroska(_) => {
                        unreachable!("dispatched by decode_next_rdb")
                    }
                };
                reader.read_audio(&mut self.inbuf[start..])?
            };
            if n == 0 {
                break;
            }
            self.mp4_filled += n;
        }
        if self.mp4_filled == 0 {
            return Ok(None); // mdat drained
        }
        let pcm_off = self.pcm_off;
        let (samples, consumed, sbr_applied) = {
            #[cfg(not(feature = "embedded-profile"))]
            {
                decode_rdb(
                    &self.inbuf[..self.mp4_filled],
                    self.config,
                    self.sfi,
                    &mut self.fb,
                    &mut self.sbr,
                    &mut self.sbr_out,
                    self.output_rate,
                    &mut self.scratch.as_mut()[pcm_off..pcm_off + PCM_CAP],
                )?
            }
            #[cfg(feature = "embedded-profile")]
            {
                decode_rdb(
                    &self.inbuf[..self.mp4_filled],
                    self.config,
                    self.sfi,
                    &mut self.fb,
                    &mut self.scratch.as_mut()[pcm_off..pcm_off + PCM_CAP],
                )?
            }
        };
        self.mp4_used = consumed;
        if sbr_applied {
            #[cfg(not(feature = "embedded-profile"))]
            {
                self.sbr_confirmed = true;
                self.promote_channels_if_ps();
            }
        }
        Ok(Some(samples))
    }

    /// ADTS path: the reader frames each `raw_data_block` from its header.
    fn decode_next_rdb_adts(&mut self) -> Result<Option<usize>, DecodeError> {
        let len = {
            let reader = match &mut self.src {
                FrameSrc::Adts(r) => r,
                FrameSrc::Mp4(_) | FrameSrc::Matroska(_) => {
                    unreachable!("dispatched by decode_next_rdb")
                }
            };
            match reader.next_frame(&mut self.inbuf)? {
                Some(rdb) => rdb.len(),
                None => return Ok(None),
            }
        };
        let pcm_off = self.pcm_off;
        let (samples, _consumed, sbr_applied) = {
            #[cfg(not(feature = "embedded-profile"))]
            {
                decode_rdb(
                    &self.inbuf[..len],
                    self.config,
                    self.sfi,
                    &mut self.fb,
                    &mut self.sbr,
                    &mut self.sbr_out,
                    self.output_rate,
                    &mut self.scratch.as_mut()[pcm_off..pcm_off + PCM_CAP],
                )?
            }
            #[cfg(feature = "embedded-profile")]
            {
                decode_rdb(
                    &self.inbuf[..len],
                    self.config,
                    self.sfi,
                    &mut self.fb,
                    &mut self.scratch.as_mut()[pcm_off..pcm_off + PCM_CAP],
                )?
            }
        };
        if sbr_applied {
            #[cfg(not(feature = "embedded-profile"))]
            {
                self.sbr_confirmed = true;
                self.promote_channels_if_ps();
            }
        }
        Ok(Some(samples))
    }

    /// Matroska path: each `SimpleBlock` is one `raw_data_block` (no ADTS
    /// header), pulled straight into `inbuf`.
    fn decode_next_rdb_matroska(&mut self) -> Result<Option<usize>, DecodeError> {
        let len = {
            let demux = match &mut self.src {
                FrameSrc::Matroska(d) => d,
                _ => unreachable!("dispatched by decode_next_rdb"),
            };
            match demux.next_packet(&mut self.inbuf)? {
                Some(pkt) => pkt.data.len(),
                None => return Ok(None),
            }
        };
        let pcm_off = self.pcm_off;
        let (samples, _consumed, sbr_applied) = {
            #[cfg(not(feature = "embedded-profile"))]
            {
                decode_rdb(
                    &self.inbuf[..len],
                    self.config,
                    self.sfi,
                    &mut self.fb,
                    &mut self.sbr,
                    &mut self.sbr_out,
                    self.output_rate,
                    &mut self.scratch.as_mut()[pcm_off..pcm_off + PCM_CAP],
                )?
            }
            #[cfg(feature = "embedded-profile")]
            {
                decode_rdb(
                    &self.inbuf[..len],
                    self.config,
                    self.sfi,
                    &mut self.fb,
                    &mut self.scratch.as_mut()[pcm_off..pcm_off + PCM_CAP],
                )?
            }
        };
        if sbr_applied {
            #[cfg(not(feature = "embedded-profile"))]
            {
                self.sbr_confirmed = true;
                self.promote_channels_if_ps();
            }
        }
        Ok(Some(samples))
    }

    /// Promote the reported channel count 1->2 the first time Parametric Stereo
    /// upmixes a mono core to stereo (R6), mirroring the `sbr_confirmed`
    /// sample-rate promotion. `SbrOutput.channels` is set to 2 by the SBR drive
    /// whenever PS ran; from then on `stream_info().channels` reports 2 and
    /// `decode_rdb` interleaves every frame as stereo.
    ///
    /// Desktop only (SBR/PS are gated off under `embedded-profile`, and every
    /// call site is itself desktop-only).
    #[cfg(not(feature = "embedded-profile"))]
    fn promote_channels_if_ps(&mut self) {
        if self.sbr_out.channels == 2 && self.channels < 2 {
            self.channels = 2;
        }
    }
}

impl<S: SeekSource, P: AsRef<[i32]> + AsMut<[i32]>> AacDecoder<S, P> {
    /// Resolve a CBR byte-estimate length for a raw ADTS stream, so
    /// [`Self::total_frames`] reports a duration and [`Self::seek_to`] can use the
    /// O(1) byte-estimate path instead of a linear decode-discard from the start.
    /// No-op for MP4/Matroska (they declare their length) and for a VBR ADTS
    /// stream (whose length stays unknown). Mirrors MP3's `resolve_length`;
    /// called once after `open` (see [`crate::dispatch::AnyDecoder::resolve_lengths`]).
    pub fn resolve_length(&mut self) {
        if let FrameSrc::Adts(r) = &mut self.src {
            r.resolve_cbr();
        }
    }

    /// Seek to per-channel output `sample` (clamped). AAC frames are **not**
    /// independent — the filterbank carries a 1024-sample MDCT overlap, and
    /// HE-AAC SBR/PS carry QMF delay lines + delta-time-coded envelopes — so a
    /// seek resets all decode state and **pre-rolls** (decode-and-discards) a
    /// run of frames before the target to reconverge before emitting. MP4 lands
    /// via the sample table (Task 1); ADTS (no index) decode-discards from the
    /// start. The landing is trimmed to the exact output sample.
    ///
    /// # Errors
    /// Propagates the sample-table / IO / decode errors of the pre-roll.
    pub fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        match &self.src {
            FrameSrc::Mp4(_) => self.seek_to_mp4(sample),
            FrameSrc::Adts(_) => self.seek_to_adts(sample),
            FrameSrc::Matroska(_) => self.seek_to_matroska(sample),
        }
    }

    /// Seek to `secs` seconds (convenience over [`Self::seek_to`]). Uses the
    /// reported output rate (2× core once SBR is confirmed).
    ///
    /// # Errors
    /// As [`Self::seek_to`].
    pub fn seek_to_time(&mut self, secs: f64) -> Result<u64, DecodeError> {
        self.seek_to(crate::decoder::secs_to_samples(secs, self.reported_rate()))
    }

    /// Reset all cross-frame decode state to a clean post-seek baseline. Keeps
    /// the monotonic `sbr_confirmed`/`channels`/`output_rate` promotions (the
    /// pre-roll re-establishes the SBR/PS state they describe).
    fn reset_decode_state(&mut self) {
        self.fb = Filterbank::new();
        #[cfg(not(feature = "embedded-profile"))]
        {
            self.sbr = sbr::SbrState::default();
            self.sbr_out = sbr::SbrOutput::default();
        }
        self.pcm_len = 0;
        self.pcm_pos = 0;
        self.eof = false;
        self.mp4_filled = 0;
        self.mp4_used = 0;
    }

    fn mp4_reader(&mut self) -> &mut Mp4Reader<S> {
        match &mut self.src {
            FrameSrc::Mp4(r) => r,
            FrameSrc::Adts(_) | FrameSrc::Matroska(_) => unreachable!("mp4 seek path"),
        }
    }

    fn seek_to_mp4(&mut self, sample: u64) -> Result<u64, DecodeError> {
        // Pre-roll frames to reconverge filterbank overlap (1 needed) and SBR
        // QMF delay lines + delta-time envelopes (several); a generous fixed
        // run covers both. For tiny targets this clamps to frame 0 (= decode
        // from start, which is exact).
        const PREROLL: u32 = 12;
        let out_per_au = self.output_samples_per_au();
        let num_frames = self.mp4_reader().coded_frame_count()?;
        if num_frames == 0 {
            return Ok(0);
        }
        let max_out = u64::from(num_frames) * out_per_au;
        let target = sample.min(max_out);
        let au_index = u32::try_from(target / out_per_au)
            .unwrap_or(num_frames)
            .min(num_frames - 1);
        // Pure AAC-LC: a short pre-roll restores the MDCT overlap → land near the
        // target (efficient, bit-exact). HE-AAC: SBR is initialized by a header
        // typically sent only in frame 0, and envelopes are delta-time coded, so
        // a mid-stream landing can't reconstruct the high band — decode from the
        // start instead (correct + bit-exact; O(target), acceptable on the rarer
        // HE-AAC seek). (Implicit-SBR MP4, where `sbr_confirmed` is still false
        // here, uses the pre-roll path — no such fixture ships; documented.)
        // `embedded-profile` never activates HE-AAC (`he_aac_active` is a
        // hardwired `false`), so this always takes the pre-roll path there.
        let land = if self.he_aac_active() {
            0
        } else {
            au_index.saturating_sub(PREROLL)
        };
        let within_out = target - u64::from(au_index) * out_per_au;

        let offset = self.mp4_reader().frame_file_offset(land)?;
        let end_offset = self.mp4_reader().frame_file_offset(num_frames)?;
        self.mp4_reader()
            .seek_audio(offset, end_offset.saturating_sub(offset))?;
        self.reset_decode_state();

        // Decode-and-discard whole frames [land, au_index).
        let mut cur = land;
        while cur < au_index {
            if !self.fill_next_frame()? {
                return Ok(target); // EOF during pre-roll — clamp
            }
            self.pcm_pos = self.pcm_len;
            cur += 1;
        }
        // Land in au_index's frame and trim to the exact output sample.
        if within_out > 0 && self.fill_next_frame()? {
            let drop = (within_out * u64::from(self.channels.max(1))).min(self.pcm_len as u64);
            self.pcm_pos = drop as usize;
        }
        Ok(target)
    }

    fn seek_to_adts(&mut self, sample: u64) -> Result<u64, DecodeError> {
        // Fast path: a resolved CBR length ([`Self::resolve_length`]) lets us jump
        // by byte estimate + syncword resync (O(1)) instead of decode-discarding
        // from the start (O(target), the ~10 s block this fixes). Frame-granular
        // approximate, like MP3's CBR seek — the landing is within ~1 frame.
        //
        // HE-AAC (SBR confirmed) is excluded: the frame-0 SBR header and the
        // delta-time envelopes can't be reconstructed from a mid-stream landing,
        // so it keeps the decode-from-start path (same rule as the MP4 seek).
        let adts_total = match &self.src {
            FrameSrc::Adts(r) => r.cbr_frames(),
            FrameSrc::Mp4(_) | FrameSrc::Matroska(_) => None,
        };
        if !self.he_aac_active() {
            if let Some(total) = adts_total {
                let per_au = self.output_samples_per_au();
                let target_frame = (sample / per_au).min(total.saturating_sub(1));
                // Bias the byte estimate to land a short run-in *before* the
                // target so a decode-discard warms the filterbank before the
                // target AU is emitted. AAC frames are not independent — the
                // filterbank carries a 1024-sample MDCT overlap — so landing cold
                // on the target would emit a warm-up transient (TDAC uncancelled).
                // Matches the Matroska path's 4-AU run-in; a small constant, so
                // the seek stays O(1) (not linear).
                const PREROLL_AAC: u64 = 4;
                let preroll_frame = target_frame.saturating_sub(PREROLL_AAC);
                let landed = match &mut self.src {
                    FrameSrc::Adts(r) => r.seek_near_frame(preroll_frame).is_ok(),
                    _ => false,
                };
                if landed {
                    self.reset_decode_state();
                    // Warm the filterbank over [preroll_frame, target_frame),
                    // discarding the primed AUs.
                    for _ in preroll_frame..target_frame {
                        if !self.fill_next_frame()? {
                            return Ok(target_frame * per_au); // EOF during pre-roll
                        }
                        self.pcm_pos = self.pcm_len;
                    }
                    // Frame-granular: report the requested frame's first sample.
                    return Ok(target_frame * per_au);
                }
                // Resync failed near the estimate — fall through to the linear seek.
            }
        }

        // Fallback: rewind, reset, and decode-discard output samples until the
        // target (exact; VBR/unresolved length or an HE-AAC stream).
        match &mut self.src {
            FrameSrc::Adts(r) => r.rewind()?,
            FrameSrc::Mp4(_) | FrameSrc::Matroska(_) => unreachable!("adts seek path"),
        }
        self.reset_decode_state();
        self.discard_to(sample)
    }

    fn seek_to_matroska(&mut self, sample: u64) -> Result<u64, DecodeError> {
        // Cues index (O(1)): jump to the Cluster covering the target, then decode-
        // discard only within it; a Cue's time is in the decoded timeline, so its
        // sample base needs no pre-skip adjustment (AAC has none). Falls back to a
        // linear restart when the file carries no Cues. The landed Cluster's first
        // access unit warms up the filterbank over the run-in before the target.
        let rate = self.reported_rate();
        // Look up a Cue a short pre-roll *before* the target so the landed Cluster
        // gives the filterbank (and any SBR/PS state) a run-in to warm up before
        // the emitted tail — a target near a Cluster boundary would otherwise emit
        // the cold-start access unit (missing the prior frame's MDCT overlap). The
        // decode-discard from `base` to `sample` spends that run-in.
        let preroll = 4 * self.output_samples_per_au();
        let base = match &mut self.src {
            FrameSrc::Matroska(d) => {
                let scale = d.timestamp_scale();
                let ticks = crate::formats::matroska::samples_to_ticks(
                    sample.saturating_sub(preroll),
                    rate,
                    scale,
                );
                match d.seek_to_cue(ticks)? {
                    Some(cue_ticks) => {
                        crate::formats::matroska::ticks_to_samples(cue_ticks, rate, scale)
                    }
                    None => {
                        d.restart()?;
                        0
                    }
                }
            }
            FrameSrc::Mp4(_) | FrameSrc::Adts(_) => unreachable!("matroska seek path"),
        };
        self.reset_decode_state();
        self.discard_from(base, sample)
    }

    /// Decode-discard per-channel output samples up to `sample`, positioning the
    /// emit cursor exactly. Returns the frame landed on (clamped at EOF). Shared
    /// by the ADTS and Matroska seeks (both container formats carry no index).
    fn discard_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        self.discard_from(0, sample)
    }

    /// As [`Self::discard_to`], but counting from `base` — the per-channel output
    /// sample of the first frame the source now yields (0 for a from-start
    /// rewind; the landed Cluster's base for a Cues jump).
    fn discard_from(&mut self, base: u64, sample: u64) -> Result<u64, DecodeError> {
        let mut passed = base; // per-channel output samples discarded so far
        while passed < sample {
            if !self.fill_next_frame()? {
                return Ok(passed); // EOF → clamp to the true length
            }
            let ch = u64::from(self.channels.max(1));
            let frame_pc = self.pcm_len as u64 / ch;
            if passed + frame_pc <= sample {
                self.pcm_pos = self.pcm_len; // discard the whole frame
                passed += frame_pc;
            } else {
                self.pcm_pos = ((sample - passed) * ch) as usize;
                passed = sample;
            }
        }
        Ok(sample)
    }
}

impl<S: SeekSource, P: AsRef<[i32]> + AsMut<[i32]>> SeekableDecoder for AacDecoder<S, P> {
    fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        AacDecoder::seek_to(self, sample)
    }
    fn total_frames(&self) -> Option<u64> {
        AacDecoder::total_frames(self)
    }
}

impl<S: Source, P: AsRef<[i32]> + AsMut<[i32]>> Decoder for AacDecoder<S, P> {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            // SBR doubles the output rate (dual-rate HE-AAC). Report 2× once
            // SBR is confirmed — up front for an explicit ASC signal, or after
            // the first EXT_SBR_DATA frame for implicit ADTS; the core rate
            // otherwise (pure AAC-LC, byte-identical to before). Always the
            // core rate under `embedded-profile` (SBR never activates there).
            sample_rate: self.reported_rate(),
            channels: self.channels,
            format: SampleFormat::Int { bits: 16 },
        }
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        let mut written = 0;
        while written < out.len() {
            if self.pcm_pos >= self.pcm_len {
                if self.eof {
                    break;
                }
                if !self.fill_next_frame()? {
                    self.eof = true;
                    break;
                }
            }
            let n = (out.len() - written).min(self.pcm_len - self.pcm_pos);
            out[written..written + n]
                .copy_from_slice(&self.pcm_ref()[self.pcm_pos..self.pcm_pos + n]);
            self.pcm_pos += n;
            written += n;
        }
        Ok(written)
    }

    fn encoder_delay(&self) -> u64 {
        self.mp4_encoder_delay()
    }

    fn playable_frames(&self) -> Option<u64> {
        self.mp4_playable_frames()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::adts::AdtsReader;
    use crate::io::SliceSource;

    /// Scratch storage for `AacDecoder::new_*_in` in this suite, sized via the
    /// shared [`crate::dispatch::decoder_scratch_len`] helper — the same
    /// storage `open()`'s callers provide.
    fn scratch() -> Vec<i32> {
        vec![0i32; crate::dispatch::decoder_scratch_len()]
    }

    /// Decode a whole stream through `AacDecoder` on a generous stack (the SBR
    /// state + per-frame DSP scratch are large — see MEMORY.md / the CLI's own
    /// big-stack loader), returning the interleaved PCM and final stream rate.
    fn decode_all_adts(bytes: &'static [u8]) -> (Vec<i32>, u32, u16) {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                let reader = AdtsReader::new(SliceSource::new(bytes)).expect("adts header");
                let mut dec = AacDecoder::new_adts_in(reader, scratch()).expect("aac decoder");
                let mut out = Vec::new();
                let mut buf = [0i32; 4096];
                loop {
                    let n = dec.decode_into(&mut buf).expect("decode");
                    if n == 0 {
                        break;
                    }
                    out.extend_from_slice(&buf[..n]);
                }
                let info = dec.stream_info();
                (out, info.sample_rate, info.channels)
            })
            .expect("spawn")
            .join()
            .expect("join")
    }

    /// Decode a whole stream through the public `open` dispatch on a big stack.
    fn decode_all_open(bytes: &'static [u8]) -> (Vec<i32>, u32, u16) {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                let mut dec = crate::dispatch::open(
                    SliceSource::new(bytes),
                    vec![0i32; crate::dispatch::decoder_scratch_len()],
                )
                .expect("probe + construct");
                let rate_up_front = dec.stream_info().sample_rate;
                let mut out = Vec::new();
                let mut buf = [0i32; 4096];
                loop {
                    let n = dec.decode_into(&mut buf).expect("decode");
                    if n == 0 {
                        break;
                    }
                    out.extend_from_slice(&buf[..n]);
                }
                let info = dec.stream_info();
                assert_eq!(
                    rate_up_front, info.sample_rate,
                    "explicit-SBR rate must be reported up front, not only after decode"
                );
                (out, info.sample_rate, info.channels)
            })
            .expect("spawn")
            .join()
            .expect("join")
    }

    /// Explicit HE-AAC in MP4 (`sbr.m4a`): the ASC advertises SBR (AOT 5), so
    /// the decoder reports the 2× output rate (44100) *up front* — before any
    /// frame is decoded — and produces 2048 samples/channel of non-silent audio.
    ///
    /// Desktop only: `embedded-profile` gates SBR off entirely (decoder-memory-
    /// externalization spec, Option A), so this file decodes its AAC-LC core
    /// instead — see `he_aac_decodes_lc_core_only_under_embedded_profile`.
    #[cfg(not(feature = "embedded-profile"))]
    #[test]
    fn sbr_m4a_explicit_decodes_2x_non_silent() {
        let m4a = include_bytes!("../../../testdata/sbr.m4a");
        let (pcm, rate, channels) = decode_all_open(m4a);
        assert_eq!(channels, 2);
        assert_eq!(rate, 44_100, "explicit SBR core 22050 → output 44100");
        assert!(!pcm.is_empty());
        assert_eq!(
            pcm.len() % (2048 * 2),
            0,
            "output length {} is not a whole number of 2048/ch stereo frames",
            pcm.len()
        );
        assert!(
            pcm.iter().any(|&s| s != 0),
            "explicit SBR output all-silent"
        );
    }

    /// Smoke test (task brief): the HE-AAC ADTS fixture decodes end-to-end with
    /// no panic, SBR activates (output rate doubles to 2×core = 44100), every
    /// frame yields 2048 samples/channel, and the reconstructed audio is
    /// non-silent and finite. Structural only — Task 12 calibrates SNR.
    ///
    /// Desktop only: see `sbr_m4a_explicit_decodes_2x_non_silent`'s note on the
    /// `embedded-profile` counterpart.
    #[cfg(not(feature = "embedded-profile"))]
    #[test]
    fn sbr_adts_smoke_decodes_2x_non_silent() {
        let aac = include_bytes!("../../../testdata/sbr.aac");
        let (pcm, rate, channels) = decode_all_adts(aac);

        // SBR discovered implicitly from the fill element → 2× output rate.
        assert_eq!(channels, 2, "sbr.aac is stereo (CPE)");
        assert_eq!(rate, 44_100, "SBR must double the 22050 core to 44100");

        // Each SBR frame is 2048 samples/channel = 4096 interleaved (stereo);
        // the whole decode is a whole number of such frames.
        assert!(!pcm.is_empty(), "no audio decoded");
        assert_eq!(
            pcm.len() % (2048 * 2),
            0,
            "output length {} is not a whole number of 2048/ch stereo frames",
            pcm.len()
        );
        let frames = pcm.len() / (2048 * 2);
        assert!(frames >= 10, "decoded only {frames} SBR frames, want >= 10");

        // Non-silent + finite (i32 samples in the i16 output range, no NaN —
        // `round_i16` maps NaN to 0 and saturates, so any out-of-range value
        // would be a real bug).
        let mut any_nonzero = false;
        let mut peak = 0i32;
        for &s in &pcm {
            assert!(
                (-32768..=32767).contains(&s),
                "sample {s} outside i16 range (NaN/overflow leaked through)"
            );
            if s != 0 {
                any_nonzero = true;
            }
            peak = peak.max(s.abs());
        }
        assert!(any_nonzero, "SBR decode produced all-silent output");
        assert!(
            peak > 8,
            "SBR decode output implausibly quiet (peak {peak})"
        );
    }

    /// Build one 44.1 kHz-stereo LC, protection-absent ADTS frame (7-byte header
    /// + `payload` zero bytes), so `aac_frame_length == 7 + payload`.
    fn adts_frame(payload: usize) -> Vec<u8> {
        let frame_len = (7 + payload) as u32;
        let mut v = Vec::with_capacity(frame_len as usize);
        v.push(0xFF);
        v.push(0xF1);
        v.push(0x50);
        v.push(0x80 | ((frame_len >> 11) & 0x03) as u8);
        v.push(((frame_len >> 3) & 0xFF) as u8);
        v.push((((frame_len & 0x07) << 5) as u8) | 0x1F);
        v.push(0xFC);
        v.extend(core::iter::repeat_n(0u8, payload));
        v
    }

    /// The public total-frames path for the ADTS-duration fix: a non-uniform ADTS
    /// stream (frame sizes that trip the >25 % VBR guard) must report an **exact**
    /// `total_frames()` after `resolve_length()`, not `None`. Header-only — the
    /// zero-payload frames never decode — so no big stack is needed. RED (old):
    /// `None` (the deck/seek-bar then had no duration → seeked to 0). GREEN:
    /// `Some(frames × 1024)` from the full header-walk.
    #[test]
    fn adts_nonuniform_total_frames_exact_not_none() {
        let mut bytes = Vec::new();
        for _ in 0..40 {
            bytes.extend_from_slice(&adts_frame(20)); // small opening frames
        }
        for _ in 0..40 {
            bytes.extend_from_slice(&adts_frame(300)); // large tail frames
        }
        let reader = AdtsReader::new(SliceSource::new(&bytes)).expect("adts header");
        let mut dec = AacDecoder::new_adts_in(reader, scratch()).expect("aac decoder");
        assert_eq!(
            dec.total_frames(),
            None,
            "ADTS length is unknown until resolve"
        );
        dec.resolve_length();
        assert_eq!(
            dec.total_frames(),
            Some(80 * 1024),
            "non-uniform ADTS must report an exact duration, not None"
        );
    }

    /// Backward-compatibility: a pure AAC-LC ADTS stream (no `EXT_SBR_DATA`
    /// fill element) decodes exactly as before — SBR never activates, the rate
    /// stays the core rate (NOT doubled), and every frame stays 1024
    /// samples/channel. This is the "SBR is purely additive; the LC-only path
    /// stays byte-identical" invariant: `decode_fil` now inspects the extension
    /// type, but consumes the same `count·8` bits as before for a non-SBR
    /// extension and leaves `frame.sbr == None`, so the shared LC `decode_rdb`
    /// path is unchanged. (The semantic byte-for-byte match vs ffmpeg is the
    /// `tests/decode.rs` segmental-SNR gate; this pins the structural shape.)
    #[test]
    fn lc_only_adts_stays_unchanged_no_spurious_sbr() {
        let aac = include_bytes!("../../../testdata/aac.aac");
        let (pcm, rate, channels) = decode_all_adts(aac);
        assert_eq!(channels, 2);
        // If SBR had spuriously activated, the rate would double to 88200 and
        // the length would be 2048/ch/frame.
        assert_eq!(rate, 44_100, "LC stream must NOT double its rate (no SBR)");
        assert_eq!(
            pcm.len(),
            47_104,
            "LC decode length changed — 23 frames × 1024/ch × 2ch expected"
        );
        assert!(pcm.iter().any(|&s| s != 0), "LC decode was all-silent");
    }

    /// `embedded-profile` only: the inverse of `sbr_m4a_explicit_decodes_2x_non_silent`
    /// / `sbr_adts_smoke_decodes_2x_non_silent` above. SBR is gated off entirely
    /// under this feature (decoder-memory-externalization spec, Option A), so
    /// an HE-AAC source — explicit ASC signalling (`sbr.m4a`) and implicit
    /// fill-element signalling (`sbr.aac`) alike — must decode its **AAC-LC
    /// core** end to end with no panic, no rejection, and no bitstream desync
    /// (`EXT_SBR_DATA` is skip-consumed as a no-op, per `syntax::decode_fil`).
    /// `stream_info().sample_rate` reports the **core** rate (22050) — never
    /// the 2× SBR rate (44100) a desktop decode of the same files produces.
    #[cfg(feature = "embedded-profile")]
    #[test]
    fn he_aac_decodes_lc_core_only_under_embedded_profile() {
        let m4a = include_bytes!("../../../testdata/sbr.m4a");
        let (pcm_m4a, rate_m4a, channels_m4a) = decode_all_open(m4a);
        assert_eq!(channels_m4a, 2);
        assert_eq!(
            rate_m4a, 22_050,
            "embedded-profile must report the AAC-LC core rate, never the 2x SBR rate"
        );
        assert!(!pcm_m4a.is_empty(), "no audio decoded from sbr.m4a");
        assert_eq!(
            pcm_m4a.len() % 2,
            0,
            "stereo interleave must stay even-length"
        );
        assert!(
            pcm_m4a.iter().any(|&s| s != 0),
            "LC-core decode of sbr.m4a (MP4/explicit ASC) was all-silent"
        );

        let aac = include_bytes!("../../../testdata/sbr.aac");
        let (pcm_aac, rate_aac, channels_aac) = decode_all_adts(aac);
        assert_eq!(channels_aac, 2, "sbr.aac is stereo (CPE)");
        assert_eq!(
            rate_aac, 22_050,
            "embedded-profile must report the AAC-LC core rate, never the 2x SBR rate"
        );
        assert!(!pcm_aac.is_empty(), "no audio decoded from sbr.aac");
        assert!(
            pcm_aac.iter().any(|&s| s != 0),
            "LC-core decode of sbr.aac (ADTS/implicit fill element) was all-silent"
        );
        assert!(
            pcm_aac.iter().all(|&s| (-32768..=32767).contains(&s)),
            "LC-core decode produced a sample outside i16 range"
        );
    }
}

#[cfg(test)]
mod seek_tests {
    use super::AacDecoder;
    use crate::decoder::Decoder;
    use crate::demux::adts::AdtsReader;
    use crate::demux::mp4::Mp4Reader;
    use crate::io::{SliceSource, Source};

    /// AAC decoders (esp. HE-AAC's ~250 KiB SBR state) overflow the default
    /// test-thread stack; run the whole decode on a generous one (as the
    /// integration suite and the CLI loader do).
    fn on_big_stack<R: Send>(f: impl FnOnce() -> R + Send) -> R {
        std::thread::scope(|s| {
            std::thread::Builder::new()
                .stack_size(16 * 1024 * 1024)
                .spawn_scoped(s, f)
                .unwrap()
                .join()
                .unwrap()
        })
    }

    /// Scratch storage for `AacDecoder::new_*_in` in this suite (see the
    /// sibling helper in `mod tests`).
    fn scratch() -> Vec<i32> {
        vec![0i32; crate::dispatch::decoder_scratch_len()]
    }

    fn drain<S: Source, P: AsRef<[i32]> + AsMut<[i32]>>(d: &mut AacDecoder<S, P>) -> Vec<i32> {
        let mut out = Vec::new();
        let mut buf = [0i32; 4096];
        loop {
            let n = d.decode_into(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    fn full_mp4(bytes: &'static [u8]) -> Vec<i32> {
        on_big_stack(|| {
            let mut d =
                AacDecoder::new_mp4_in(Mp4Reader::new(SliceSource::new(bytes)).unwrap(), scratch())
                    .unwrap();
            drain(&mut d)
        })
    }

    fn seek_tail_mp4(bytes: &'static [u8], target: u64) -> (u64, Vec<i32>) {
        on_big_stack(move || {
            let mut d =
                AacDecoder::new_mp4_in(Mp4Reader::new(SliceSource::new(bytes)).unwrap(), scratch())
                    .unwrap();
            let landed = d.seek_to(target).unwrap();
            (landed, drain(&mut d))
        })
    }

    fn full_adts(bytes: &'static [u8]) -> Vec<i32> {
        on_big_stack(|| {
            let mut d = AacDecoder::new_adts_in(
                AdtsReader::new(SliceSource::new(bytes)).unwrap(),
                scratch(),
            )
            .unwrap();
            drain(&mut d)
        })
    }

    fn seek_tail_adts(bytes: &'static [u8], target: u64) -> (u64, Vec<i32>) {
        on_big_stack(move || {
            let mut d = AacDecoder::new_adts_in(
                AdtsReader::new(SliceSource::new(bytes)).unwrap(),
                scratch(),
            )
            .unwrap();
            let landed = d.seek_to(target).unwrap();
            (landed, drain(&mut d))
        })
    }

    /// AAC-LC: a 12-frame pre-roll fully restores the 1-frame MDCT overlap, so
    /// the post-seek tail is **bit-identical** to decoding from the start and
    /// dropping the head — at frame boundaries and interiors alike.
    #[test]
    fn aac_mp4_lc_seek_bit_exact() {
        const CH: usize = 2;
        let full = full_mp4(include_bytes!("../../../testdata/aac.m4a"));
        let pc = (full.len() / CH) as u64;
        for &t in &[0u64, 1, 1024, 1500, pc / 3, pc / 2, (3 * pc) / 4, pc - 1] {
            let (landed, tail) = seek_tail_mp4(include_bytes!("../../../testdata/aac.m4a"), t);
            assert_eq!(landed, t, "LC seek landing");
            let from = t as usize * CH;
            assert_eq!(tail.len(), full.len() - from, "LC tail length at {t}");
            assert_eq!(tail, &full[from..], "LC tail bit-exact at {t}");
        }
    }

    #[test]
    fn aac_mp4_lc_seek_to_zero_matches_fresh() {
        let full = full_mp4(include_bytes!("../../../testdata/aac.m4a"));
        let (_, tail) = seek_tail_mp4(include_bytes!("../../../testdata/aac.m4a"), 0);
        assert_eq!(tail, full, "LC seek-to-0 == fresh decode");
    }

    #[test]
    fn aac_mp4_lc_seek_beyond_end_clamps() {
        const CH: usize = 2;
        let full = full_mp4(include_bytes!("../../../testdata/aac.m4a"));
        let pc = (full.len() / CH) as u64;
        let (landed, _) = seek_tail_mp4(include_bytes!("../../../testdata/aac.m4a"), 10_000_000);
        assert!(
            landed <= pc + 1024,
            "clamps near the declared length: {landed}"
        );
    }

    /// HE-AAC (SBR, dual-rate): seeking decodes from the start so the frame-0
    /// SBR header + delta-time envelopes are always seen, making the tail
    /// bit-identical to the from-start decode (the output position is
    /// sample-accurate at the dual rate).
    ///
    /// Desktop only: `embedded-profile` gates SBR off entirely, so this file's
    /// seek exercises the plain AAC-LC pre-roll path instead — see
    /// `aac_mp4_he_aac_seek_decodes_lc_core_under_embedded_profile`.
    #[cfg(not(feature = "embedded-profile"))]
    #[test]
    fn aac_mp4_sbr_seek_bit_exact() {
        const CH: usize = 2;
        let full = full_mp4(include_bytes!("../../../testdata/sbr.m4a"));
        let pc = (full.len() / CH) as u64;
        for &t in &[pc / 4, pc / 2, (3 * pc) / 4] {
            let (landed, tail) = seek_tail_mp4(include_bytes!("../../../testdata/sbr.m4a"), t);
            assert_eq!(landed, t, "SBR seek landing (sample-accurate output pos)");
            let from = t as usize * CH;
            assert_eq!(tail.len(), full.len() - from, "SBR tail length at {t}");
            assert_eq!(tail, &full[from..], "HE-AAC SBR tail bit-exact at {t}");
        }
    }

    /// ADTS LC decode-discards from the start (no index) → bit-exact tail.
    #[test]
    fn aac_adts_lc_seek_bit_exact() {
        const CH: usize = 2;
        let full = full_adts(include_bytes!("../../../testdata/aac.aac"));
        let pc = (full.len() / CH) as u64;
        for &t in &[0u64, 1024, pc / 2, pc - 1] {
            let (landed, tail) = seek_tail_adts(include_bytes!("../../../testdata/aac.aac"), t);
            assert_eq!(landed, t, "ADTS seek landing");
            let from = t as usize * CH;
            assert_eq!(tail, &full[from..], "ADTS LC tail bit-exact at {t}");
        }
    }

    /// ADTS HE-AAC decode-discards from the start too → also bit-exact (no
    /// reconvergence gap, unlike the sample-table pre-roll path).
    ///
    /// Desktop only: see `aac_mp4_sbr_seek_bit_exact`'s note on the
    /// `embedded-profile` counterpart.
    #[cfg(not(feature = "embedded-profile"))]
    #[test]
    fn aac_adts_sbr_seek_bit_exact() {
        const CH: usize = 2;
        let full = full_adts(include_bytes!("../../../testdata/sbr.aac"));
        let pc = (full.len() / CH) as u64;
        for &t in &[pc / 4, pc / 2] {
            let (landed, tail) = seek_tail_adts(include_bytes!("../../../testdata/sbr.aac"), t);
            assert_eq!(landed, t, "ADTS SBR seek landing");
            let from = t as usize * CH;
            assert_eq!(tail, &full[from..], "ADTS SBR tail bit-exact at {t}");
        }
    }

    /// `embedded-profile` only: seeking an HE-AAC source still works — SBR is
    /// gated off, so the seek exercises the plain AAC-LC pre-roll/decode-discard
    /// paths over `sbr.m4a`/`sbr.aac`'s LC core, landing exactly and never
    /// panicking or desyncing the bitstream (decoder-memory-externalization
    /// spec, Option A).
    #[cfg(feature = "embedded-profile")]
    #[test]
    fn aac_he_aac_seek_decodes_lc_core_under_embedded_profile() {
        const CH: usize = 2;
        let full = full_mp4(include_bytes!("../../../testdata/sbr.m4a"));
        assert!(!full.is_empty(), "no audio decoded from sbr.m4a");
        let pc = (full.len() / CH) as u64;
        let (landed, tail) = seek_tail_mp4(include_bytes!("../../../testdata/sbr.m4a"), pc / 2);
        assert_eq!(
            landed,
            pc / 2,
            "LC-core seek landing on an HE-AAC MP4 source"
        );
        assert_eq!(
            tail,
            &full[(pc / 2) as usize * CH..],
            "LC-core MP4 seek tail"
        );

        let full_a = full_adts(include_bytes!("../../../testdata/sbr.aac"));
        assert!(!full_a.is_empty(), "no audio decoded from sbr.aac");
        let pc_a = (full_a.len() / CH) as u64;
        let (landed_a, tail_a) =
            seek_tail_adts(include_bytes!("../../../testdata/sbr.aac"), pc_a / 2);
        assert_eq!(
            landed_a,
            pc_a / 2,
            "LC-core seek landing on an HE-AAC ADTS source"
        );
        assert_eq!(
            tail_a,
            &full_a[(pc_a / 2) as usize * CH..],
            "LC-core ADTS seek tail"
        );
    }
}
