//! From-scratch Opus decoder (design spec:
//! `docs/superpowers/specs/2026-09-06-opus-decoder-design.md`), over an
//! Ogg-Opus container (RFC 7845).
//!
//! [`OpusDecoder::new`] parses the two Opus header packets — `OpusHead`
//! ([`container::OpusHead`], channels/pre-skip/gain/mapping family) and
//! `OpusTags` (validated by magic, otherwise skipped) — so
//! [`Decoder::stream_info`] is real (Opus always decodes at 48 kHz). Per-packet
//! audio decode drives the range coder through CELT and/or SILK synthesis:
//! CELT-only, SILK-only, and Hybrid streams — mono **and** stereo — decode end
//! to end ([`Decoder::decode_into`]).
//! [`Self::fill_packet`] applies the RFC 7845 stream-global pre-skip and Ogg
//! granule end-trim ([`trim_frame`]) and conceals lost/DTX frames as silence
//! (minimal PLC). [`SeekableDecoder::seek_to`] seeks by Ogg-granule bisection
//! with an 80 ms pre-roll re-prime, and [`SeekableDecoder::total_frames`]
//! derives the pre-skip-trimmed length from the stream's final granule.
//!
//! Unlike [`super::vorbis::VorbisDecoder`], Opus needs **no caller-provided
//! arena** — every table RFC 6716 defines is fixed-capacity (no per-file
//! setup header), so `OpusDecoder<S>` is a normal by-value, no-alloc decoder
//! that slots directly into [`crate::dispatch::AnyDecoder`] (see the design
//! spec's "Dispatch" section for why this differs from Vorbis's
//! consumer-dispatched shape).

pub mod celt;
/// Raw-packet decode path for RFC 8251 conformance validation. `#[doc(hidden)]`:
/// not part of the stable API — exposed only so `tests/opus_conformance.rs` can
/// drive it (see the module docs).
#[doc(hidden)]
pub mod conformance;
pub mod container;
pub mod multistream;
mod opus_tables;
pub mod range;
mod silk;
pub mod toc;

use core::cell::{Cell, RefCell};

use celt::decoder::celt_exp2;
use conformance::OpusFrameDecoder;
use container::OpusHead;

use crate::decoder::{DecodeError, Decoder, SeekableDecoder};
use crate::demux::ogg_packets::OggPackets;
use crate::io::{SeekSource, Source};
use crate::model::{SampleFormat, StreamInfo};

/// Opus always decodes at 48 kHz (RFC 6716 §2), regardless of the container's
/// purely informational `input_sample_rate` field.
const OUTPUT_SAMPLE_RATE: u32 = 48_000;

/// Largest Opus packet (bytes) this decoder reassembles. Real-world Opus audio
/// packets are well under a couple KiB; an over-long packet is a graceful
/// [`OggPackets::next_packet`] `Unsupported` rather than a corrupt decode.
const MAX_PACKET_BYTES: usize = 16_384;

/// Largest interleaved sample count ONE internal Opus frame can decode to: a
/// 60 ms SILK-only frame is `2880` per-channel samples at 48 kHz, ×2 for a
/// stereo stream. [`Self::fill_packet`] hands the shared core a scratch buffer
/// this size per frame, then trims/copies the kept sub-range into `pcm_buf`.
const MAX_FRAME_SAMPLES: usize = 2880 * 2;

/// Largest number of decoded interleaved samples one Opus packet can yield: a
/// 120 ms packet at 48 kHz, stereo (`5760 * 2`). One packet is decoded, then
/// drained across [`Decoder::decode_into`] calls.
const MAX_PACKET_SAMPLES: usize = 5760 * 2;

/// Per-channel-sample scale for the `OpusHead` output gain. libopus
/// (`opus_decoder.c`, both the fixed- and float-point builds) forms the gain as
/// `celt_exp2(MULT16_16_P15(QCONST16(6.48814081e-4f, 25), st->decode_gain))`,
/// which in the float build reduces to `celt_exp2(6.48814081e-4 · gain_q8)` —
/// i.e. `2^(gain_q8·log2(10)/5120) = 10^(gain_q8/(20·256))`, the RFC 7845 §5.1
/// Q7.8-dB gain. This decoder is float-domain throughout (its CELT synthesis
/// matches libopus's float build to ~87 dB), so it applies the **float**
/// `celt_exp2` here — matching that build exactly. The fixed-point build's
/// `celt_exp2` differs by only its Q-domain rounding (~1e-4 relative), far below
/// the `opus_compare` weighting RFC 8251 conformance uses, so either is
/// conformant; the float form is the one consistent with the rest of the
/// pipeline. `gain_q8 == 0` short-circuits to an exact `1.0` (below), avoiding
/// the cubic approximation's `celt_exp2(0) = 0.99992` bias on the common path.
/// (`6.488_141e-4` is libopus's `6.48814081e-4f` at f32 precision.)
const OUTPUT_GAIN_SCALE: f32 = 6.488_141e-4;

/// Minimum per-channel run-in (48 kHz samples) decoded and discarded ahead of a
/// seek target so the CELT/SILK cross-frame state (MDCT overlap, energy
/// predictor, LPC/LTP filter memory, resampler delay lines) reconverges before
/// the first emitted sample. 3840 = 80 ms, the pre-roll opusfile uses for
/// sample-accurate Opus seeking (RFC 7845 §4.4). The actual run-in is at least
/// this much and usually more (it rounds back to the start of the landed Ogg
/// page).
const SEEK_PREROLL: u64 = 3840;

/// Scratch for the `OpusHead` identification packet (RFC 7845 §5.1). The
/// fixed prefix this decoder reads is 19 bytes; a mapping-family-1/255 stream
/// appends a channel-mapping table (2 bytes + up to 255 per-stream mapping
/// bytes), so this leaves generous slack for [`OggPackets::next_packet`]'s
/// fixed scratch buffer — an oversized/malformed packet is a graceful
/// [`DecodeError::Unsupported`] there rather than corrupting the parse.
const ID_HEADER_BUF: usize = 64;

/// Scratch for the `OpusTags` comment header's validated prefix — only the
/// 8-byte `"OpusTags"` magic is checked; the vendor string and user comments
/// that follow may be arbitrarily large and are skipped via
/// [`OggPackets::read_packet_prefix`], mirroring the Vorbis comment header.
const TAGS_PREFIX_BUF: usize = 8;

/// A from-scratch Opus decoder over an Ogg-Opus [`Source`] (RFC 7845 framing,
/// RFC 6716 codec). See the module docs for why this is fixed-capacity,
/// no-alloc, and arena-free (unlike Vorbis).
pub struct OpusDecoder<S> {
    /// The packet-preserving Ogg demuxer. `RefCell`-wrapped so
    /// [`SeekableDecoder::total_frames`] (an `&self` method) can drive
    /// [`OggPackets::end_granule`], which needs `&mut` — the same seam
    /// [`super::vorbis::VorbisDecoder`] uses.
    ogg: RefCell<OggPackets<S>>,
    /// Parsed `OpusHead` fields (RFC 7845 §5.1): channels feed
    /// [`Self::stream_info`], pre-skip trims [`SeekableDecoder::total_frames`]
    /// (and, later, the decoded PCM itself); gain/mapping family are read by
    /// later tasks.
    head: OpusHead,
    stream_info: StreamInfo,
    /// Memoized [`OggPackets::end_granule`] result (pre-skip already
    /// subtracted) for [`SeekableDecoder::total_frames`] — `None` means "not
    /// yet computed", `Some(None)` means "computed, but the stream declares
    /// no length". [`OggPackets::end_granule`] is non-destructive, so
    /// computing this doesn't disturb sequential decode.
    total_frames: Cell<Option<Option<u64>>>,
    /// The shared per-frame decode core (CELT + SILK sub-engines + the
    /// `prev_mode`/`prev_redundancy`/`final_range` transition state). The SAME
    /// [`OpusFrameDecoder`] the raw-packet [`conformance::ConformanceDecoder`]
    /// embeds — so the redundancy / mode-transition handling (redundant CELT
    /// frame, `smooth_fade` cross-fades, `OPUS_RESET_STATE` on a mode change) is
    /// applied identically here on every mode switch of an adaptive `.opus`
    /// stream. [`Self::fill_packet`] wraps it with the Ogg-level concerns
    /// (granule pre-skip / end-trim, `OpusHead` output gain, lost-packet PLC).
    core: OpusFrameDecoder,
    /// Running **absolute** per-channel decode position in RFC 7845 granule
    /// units (48 kHz, pre-skip *included*) — the granule the decoder is at.
    /// Every decoded frame advances it by its per-channel sample count. Both
    /// the pre-skip discard and the granule end-trim are expressed against it
    /// (see [`trim_frame`]); a seek re-baselines it to the landed page's floor
    /// granule so the accounting stays absolute afterwards.
    granule_pos: u64,
    /// Absolute granule below which decoded output is discarded: `pre_skip` at
    /// the stream start (RFC 7845 §4.2 priming) and, after a
    /// [`SeekableDecoder::seek_to`], the seek target's granule (so the first
    /// emitted sample lands exactly on it). Frames whose whole span is below
    /// this are decoded — to reconverge cross-frame state — but emit nothing.
    skip_to: u64,
    /// Linear output gain (`OpusHead` §5.1), applied per sample.
    output_gain: f32,
    /// Reassembly scratch for one Ogg packet.
    packet_buf: [u8; MAX_PACKET_BYTES],
    /// Decoded interleaved PCM for the current packet, drained across
    /// [`Decoder::decode_into`] calls.
    pcm_buf: [i32; MAX_PACKET_SAMPLES],
    /// Number of valid samples in [`Self::pcm_buf`].
    pcm_len: usize,
    /// Read cursor into [`Self::pcm_buf`].
    pcm_pos: usize,
}

/// Open an Ogg-Opus stream: construct the Ogg packet reader and consume the two
/// mandatory header packets (RFC 7845 §5) — the identification header
/// (`OpusHead`, parsed) and the comment header (`OpusTags`, validated by magic
/// and skipped). Returns the reader positioned at the first audio packet plus
/// the parsed [`OpusHead`]. Shared by [`OpusDecoder::new`],
/// [`multistream::MultistreamOpusDecoder::new`], and [`crate::dispatch`] (which
/// parses the header once, then routes by mapping family).
///
/// # Errors
/// Ogg framing errors ([`OggPackets::new`]/`next_packet`), [`OpusHead::parse`]
/// validation, or [`DecodeError::Malformed`] if the second packet isn't
/// `OpusTags`.
pub(crate) fn open_opus_headers<S: Source>(
    src: S,
) -> Result<(OggPackets<S>, OpusHead), DecodeError> {
    let mut ogg = OggPackets::new(src)?;

    // Packet 1: identification header.
    let mut id_buf = [0u8; ID_HEADER_BUF];
    let n = ogg.next_packet(&mut id_buf)?;
    let head = OpusHead::parse(&id_buf[..n])?;

    // Packet 2: comment header ("OpusTags") — check the magic, discard the
    // (possibly large) vendor/comment tail.
    let mut tags_prefix = [0u8; TAGS_PREFIX_BUF];
    let tn = ogg.read_packet_prefix(&mut tags_prefix)?;
    if tn < TAGS_PREFIX_BUF || &tags_prefix[..TAGS_PREFIX_BUF] != b"OpusTags" {
        return Err(DecodeError::Malformed);
    }
    Ok((ogg, head))
}

impl<S: Source> OpusDecoder<S> {
    /// Open an Ogg-Opus stream by consuming its two header packets (RFC 7845
    /// §5): the identification header (`OpusHead`) and the comment header
    /// (`OpusTags`, validated by magic only and otherwise skipped). Leaves the
    /// Ogg reader positioned at the first audio packet.
    ///
    /// # Errors
    /// Propagates Ogg framing errors ([`OggPackets::new`]/`next_packet`) and
    /// [`OpusHead::parse`]'s validation errors. [`DecodeError::Malformed`] if
    /// the second packet isn't `OpusTags`.
    pub fn new(src: S) -> Result<Self, DecodeError> {
        let (ogg, head) = open_opus_headers(src)?;
        Self::from_parts(ogg, head)
    }

    /// Build a **single-stream** Opus decoder from an already-opened Ogg reader
    /// (positioned at the first audio packet) and its parsed [`OpusHead`]. Split
    /// out of [`Self::new`] so [`crate::dispatch`] can parse the header once and
    /// route by mapping family — family 0 here, families 1/255 to
    /// [`MultistreamOpusDecoder`](multistream::MultistreamOpusDecoder).
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] for a non-zero mapping family (this type only
    /// decodes mono/stereo family 0; multichannel is the multistream decoder's
    /// job).
    pub(crate) fn from_parts(ogg: OggPackets<S>, head: OpusHead) -> Result<Self, DecodeError> {
        if head.mapping_family != 0 {
            return Err(DecodeError::Unsupported);
        }

        let stream_info = StreamInfo {
            sample_rate: OUTPUT_SAMPLE_RATE,
            channels: u16::from(head.channels),
            format: SampleFormat::Int { bits: 16 },
        };

        let channels = usize::from(head.channels).clamp(1, 2);
        // libopus applies the OpusHead output gain only when it is non-zero
        // (`if (st->decode_gain)` in `opus_decode_frame`); a zero gain — the
        // common case — is an exact 1.0 pass-through, NOT `celt_exp2(0)` (whose
        // cubic approximation is 0.9999252, a ~0.0075% systematic attenuation).
        let output_gain = if head.output_gain_q8 == 0 {
            1.0
        } else {
            celt_exp2(f32::from(head.output_gain_q8) * OUTPUT_GAIN_SCALE)
        };

        Ok(Self {
            ogg: RefCell::new(ogg),
            head,
            stream_info,
            total_frames: Cell::new(None),
            core: OpusFrameDecoder::new(channels),
            granule_pos: 0,
            skip_to: u64::from(head.pre_skip),
            output_gain,
            packet_buf: [0; MAX_PACKET_BYTES],
            pcm_buf: [0; MAX_PACKET_SAMPLES],
            pcm_len: 0,
            pcm_pos: 0,
        })
    }

    /// Decode the next audio packet into [`Self::pcm_buf`], delegating each
    /// internal Opus frame to the shared [`OpusFrameDecoder`] core and wrapping
    /// it with the Ogg-container concerns: the stream-global pre-skip / seek-skip
    /// ([`Self::skip_to`]), the Ogg granule end-trim ([`trim_frame`]), the
    /// `OpusHead` output gain, and lost-packet PLC. Leaves `pcm_len == 0` at a
    /// clean end of stream.
    ///
    /// The core decodes ONE full frame of `frame_pch * channels` interleaved
    /// 48 kHz samples (CELT-only, SILK-only, or Hybrid — mono **and** stereo, a
    /// mono packet in a stereo stream upmixed L = R by the sub-codecs) with the
    /// full `opus_decode_frame` flow, **including** the redundancy /
    /// mode-transition handling on a mode switch (redundant CELT frame,
    /// `smooth_fade` cross-fades, `OPUS_RESET_STATE`) — the same logic the
    /// raw-packet conformance path uses. This wrapper then trims that frame to
    /// its kept `[s, e)` per-channel sub-range and copies it into `pcm_buf`.
    ///
    /// **Pre-skip / seek run-in** (RFC 7845 §4.2, §4.4): a frame whose whole span
    /// sits below [`Self::skip_to`] is still fully decoded through the core — so
    /// the CELT/SILK cross-frame state converges — but emits nothing; the loop
    /// then fetches the next packet. This is exactly the mechanism a seek
    /// re-primes through (it sets `skip_to` to the target granule).
    ///
    /// **Minimal PLC**: a zero-length (DTX) frame is concealed as **silence** for
    /// its nominal duration (the TOC `config.frame_size_48k`) rather than dropped,
    /// so the running [`Self::granule_pos`] — and therefore the pre-skip /
    /// end-trim / seek accounting — never desyncs. A frame whose range decode
    /// fails is likewise concealed as silence **inside** the core (which returns a
    /// zeroed full frame). Full CELT pitch-extrapolation / comfort-noise
    /// concealment (RFC 6716 §4.4) is deliberately **not** implemented; silence is
    /// the documented simplification.
    fn fill_packet(&mut self) -> Result<(), DecodeError> {
        self.pcm_pos = 0;
        self.pcm_len = 0;
        let channels = usize::from(self.stream_info.channels).clamp(1, 2);
        let output_gain = self.output_gain;
        loop {
            let n = self.ogg.borrow_mut().next_packet(&mut self.packet_buf)?;
            if n == 0 {
                return Ok(()); // clean end of stream
            }
            // The granule of the Ogg page this packet completes on — the hard
            // end-trim ceiling for every frame in the packet.
            let page_granule = self.ogg.borrow().last_page_granule();
            let mut frames: [&[u8]; toc::MAX_FRAMES] = [&[][..]; toc::MAX_FRAMES];
            let (config, count) = match toc::frames(&self.packet_buf[..n], &mut frames) {
                Ok(v) => v,
                Err(_) => continue, // skip a malformed packet rather than abort
            };
            // Per-channel sample count of one frame (48 kHz), from the TOC — the
            // granule-accounting/PLC size, independent of a successful decode.
            let frame_pch = usize::from(config.frame_size_48k);
            // One decoded frame's worth of interleaved PCM, gain already applied
            // by the core. Reused across the packet's frames. The disjoint field
            // borrows (`self.core` vs `self.packet_buf` the `frames` slices
            // borrow vs `self.granule_pos`/`self.pcm_buf`) let the core call and
            // the trim/copy stay inline in this loop.
            let mut scratch = [0i32; MAX_FRAME_SAMPLES];
            for &frame in frames.iter().take(count) {
                let (s, e) =
                    trim_frame(&mut self.granule_pos, self.skip_to, page_granule, frame_pch);
                if frame.is_empty() {
                    // DTX / lost frame: conceal as silence for its nominal span
                    // (the core is NOT invoked, so its transition state is left
                    // untouched across the gap — the documented minimal PLC).
                    emit_silence(
                        &mut self.pcm_buf,
                        &mut self.pcm_len,
                        e.saturating_sub(s) * channels,
                    );
                    continue;
                }
                // Decode the full frame through the shared core (transitions +
                // redundancy handled inside), applying the OpusHead output gain,
                // then keep only the `[s, e)` per-channel sub-range.
                let produced = self
                    .core
                    .decode_frame(frame, config, output_gain, &mut scratch);
                let e = e.min(produced / channels);
                if e <= s {
                    continue; // wholly pre-skipped / trimmed (state still built)
                }
                let keep = &scratch[s * channels..e * channels];
                let end = (self.pcm_len + keep.len()).min(self.pcm_buf.len());
                let take = end - self.pcm_len;
                self.pcm_buf[self.pcm_len..end].copy_from_slice(&keep[..take]);
                self.pcm_len = end;
            }
            if self.pcm_len > 0 {
                return Ok(());
            }
            // Whole packet consumed by pre-skip / seek run-in: keep reading.
        }
    }

    /// Reset every per-decode field to the pristine post-[`Self::new`] values
    /// so decoding can resume cleanly at a new Ogg position
    /// ([`SeekableDecoder::seek_to`]). Rebuilds the whole [`OpusFrameDecoder`]
    /// core from scratch (a fresh `new`), dropping every CELT/SILK overlap /
    /// energy-history / filter-memory / resampler-delay-line carry **and** the
    /// mode-transition state (`prev_mode = None`, `prev_redundancy = false`,
    /// `final_range = 0`) so no stale tail — or stale transition context — bleeds
    /// into the first post-seek frame; drops any staged-but-undelivered PCM; and
    /// re-baselines the pre-skip counters. The caller (`seek_to`) then sets
    /// [`Self::granule_pos`] / [`Self::skip_to`] to the landed position — this
    /// leaves them at the stream-start defaults, correct as-is for the
    /// seek-to-0 path.
    fn reset_decode_state(&mut self) {
        let channels = usize::from(self.stream_info.channels).clamp(1, 2);
        self.core = OpusFrameDecoder::new(channels);
        self.pcm_pos = 0;
        self.pcm_len = 0;
        self.granule_pos = 0;
        self.skip_to = u64::from(self.head.pre_skip);
    }

    /// Skip past the two Opus header packets (`OpusHead` + `OpusTags`) without
    /// re-parsing them, leaving the Ogg reader at the first audio packet —
    /// exactly where [`Self::new`] leaves it. Used by
    /// [`SeekableDecoder::seek_to`]'s seek-to-0 path to reproduce that position
    /// after rewinding to the stream start (mirroring the Vorbis decoder's
    /// three-packet skip). [`OggPackets::read_packet_prefix`] tolerates an
    /// oversized `OpusTags` packet that `next_packet` would reject.
    ///
    /// # Errors
    /// Propagates the underlying [`OggPackets`] I/O / framing errors.
    fn skip_header_packets(&mut self) -> Result<(), DecodeError> {
        for _ in 0..2 {
            self.ogg
                .get_mut()
                .read_packet_prefix(&mut self.packet_buf)?;
        }
        Ok(())
    }
}

/// Stage `count` interleaved samples of **silence** into `pcm_buf` at
/// `*pcm_len` (minimal packet-loss / DTX concealment; see
/// [`OpusDecoder::fill_packet`]). Bounded to the buffer's capacity, so an
/// over-long conceal can never overflow it. A free function (not a method) so
/// it borrows only the two PCM fields — leaving `packet_buf`, which the caller's
/// in-flight `frames` slices borrow, untouched.
fn emit_silence(pcm_buf: &mut [i32], pcm_len: &mut usize, count: usize) {
    let end = (*pcm_len + count).min(pcm_buf.len());
    pcm_buf[*pcm_len..end].fill(0);
    *pcm_len = end;
}

/// Apply the stream-global pre-skip / seek-skip and the Ogg granule end-trim to
/// one freshly decoded frame of `per_ch` per-channel samples (RFC 7845 §4.2
/// pre-skip, §4.4 seeking, and the granule↔sample mapping of §4).
///
/// `granule` is the running **absolute** per-channel decode position (48 kHz
/// granule units, pre-skip *included*); it is advanced by `per_ch`. `skip_to`
/// is the absolute granule below which output is discarded (the pre-skip at
/// stream start, or the seek target after a seek). `page_granule` is the
/// granule of the Ogg page this frame's packet completed on — an exact ceiling
/// on total output through this page (spec: every page's granule is the sample
/// count through its last completed packet); `u64::MAX` marks "no packet
/// completed on this page" and carries no ceiling. Returns the per-channel
/// `[start, end)` sub-range of the frame to emit — empty when the frame is
/// wholly below `skip_to` (pre-skip / seek run-in) or wholly past the granule
/// ceiling (final-page end-trim).
pub(crate) fn trim_frame(
    granule: &mut u64,
    skip_to: u64,
    page_granule: u64,
    per_ch: usize,
) -> (usize, usize) {
    let frame_start = *granule;
    let frame_end = frame_start + per_ch as u64;
    *granule = frame_end;
    let kept_end = if page_granule == u64::MAX {
        frame_end
    } else {
        frame_end.min(page_granule)
    };
    let out_start = frame_start.max(skip_to);
    if out_start >= kept_end {
        return (0, 0);
    }
    (
        (out_start - frame_start) as usize,
        (kept_end - frame_start) as usize,
    )
}

impl<S: Source> Decoder for OpusDecoder<S> {
    fn stream_info(&self) -> StreamInfo {
        self.stream_info
    }

    /// Decode Opus into interleaved 16-bit PCM (`i32` slots), draining one Ogg
    /// packet at a time with the stream-global pre-skip / end-trim applied
    /// ([`Self::fill_packet`]). Returns `Ok(0)` at a clean end of stream.
    ///
    /// CELT-only, SILK-only, and Hybrid streams — mono and stereo — decode end
    /// to end.
    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.pcm_pos >= self.pcm_len {
            self.fill_packet()?;
            if self.pcm_len == 0 {
                return Ok(0); // end of stream
            }
        }
        let avail = self.pcm_len - self.pcm_pos;
        let take = avail.min(out.len());
        out[..take].copy_from_slice(&self.pcm_buf[self.pcm_pos..self.pcm_pos + take]);
        self.pcm_pos += take;
        Ok(take)
    }
}

impl<S: SeekSource> SeekableDecoder for OpusDecoder<S> {
    /// Seek to per-channel output `sample` (post-pre-skip; RFC 7845 §4.4).
    ///
    /// Opus output sample `k` sits at absolute granule `k + pre_skip`, so the
    /// requested position maps to granule `sample + pre_skip`. The decoder
    /// bisects Ogg pages by granule ([`OggPackets::seek_to_granule`]) to a page
    /// at least [`SEEK_PREROLL`] (80 ms) of run-in *before* the target, resets
    /// all cross-frame state ([`Self::reset_decode_state`]), then re-baselines
    /// [`Self::granule_pos`] to the landed page's floor granule and sets
    /// [`Self::skip_to`] to the target granule. [`Self::fill_packet`] then
    /// decodes forward from the page start, **discarding** every frame below the
    /// target — which both reconverges the CELT/SILK state (MDCT overlap, energy
    /// predictor, LPC/LTP memory, resampler delay lines) and lands the first
    /// emitted sample *exactly* on the target (the run-in discard is
    /// sample-accurate via `skip_to`, unlike a raw page-granular seek). Returns
    /// the (clamped) output sample decoding will resume at.
    ///
    /// `sample == 0` is exact, not approximate: it rewinds to the stream's start
    /// byte and replays [`Self::skip_header_packets`], reproducing a fresh
    /// decoder (pre-skip re-applied) — mirroring the Vorbis decoder's seek-to-0
    /// path. A `sample` beyond the declared length ([`Self::total_frames`])
    /// clamps to it (never an error), reusing [`OggPackets::seek_to_granule`]'s
    /// own "closest available position" contract.
    ///
    /// # Errors
    /// Propagates [`OggPackets::seek_to_granule`] / `read_packet_prefix` I/O and
    /// framing errors.
    fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        let pre_skip = u64::from(self.head.pre_skip);
        // Clamp the requested OUTPUT sample to the stream's declared length.
        let target = match self.total_frames() {
            Some(total) => sample.min(total),
            None => sample,
        };

        if target == 0 {
            // Exact seek-to-start: rewind past the two Opus header packets and
            // reset, reproducing a fresh decoder (pre-skip re-applied). The
            // header pages share granule 0 with the first audio packet, so a
            // granule bisection for 0 would land on a header page, not the first
            // audio packet — hence the explicit rewind + header skip.
            self.ogg.get_mut().seek_to_granule(0)?;
            self.skip_header_packets()?;
            self.reset_decode_state();
            return Ok(0);
        }

        // Absolute granule of the first output sample we want.
        let granule_target = target + pre_skip;
        // Land on a page giving at least SEEK_PREROLL samples of run-in ahead of
        // the target so the cross-frame state reconverges before emission
        // (RFC 7845 §4.4). `seek_to_granule` lands on the first page whose
        // granule >= this; the preceding page's granule (`landed_floor_granule`)
        // is where the landed page's audio begins.
        let preroll_granule = granule_target.saturating_sub(SEEK_PREROLL);
        self.ogg.get_mut().seek_to_granule(preroll_granule)?;
        let floor = self.ogg.get_mut().landed_floor_granule();
        self.reset_decode_state();
        // Baseline the absolute counter at the landed page's start, and discard
        // everything below the target (both reconverges state and lands the
        // first emitted sample exactly on `granule_target`).
        self.granule_pos = floor;
        self.skip_to = granule_target;
        Ok(target)
    }

    /// The stream's declared per-channel frame count, with the encoder's
    /// pre-skip (RFC 7845 §4.2) already trimmed off: the final page's granule
    /// (48 kHz units, like every Ogg-Opus granule) minus [`OpusHead::pre_skip`].
    /// `None` if the stream declares no length (no page with a real granule
    /// found within the scan window).
    fn total_frames(&self) -> Option<u64> {
        if let Some(cached) = self.total_frames.get() {
            return cached;
        }
        let pre_skip = u64::from(self.head.pre_skip);
        let result = self
            .ogg
            .borrow_mut()
            .end_granule()
            .ok()
            .map(|g| g.saturating_sub(pre_skip));
        self.total_frames.set(Some(result));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceSource;

    /// Build one minimal Ogg page (mirrors `demux::ogg_packets`'s own test
    /// helper) — `packet` must be under 255 bytes (true for every fixture
    /// below).
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

    /// A well-formed `OpusHead` packet (RFC 7845 §5.1), mapping family 0.
    fn opus_head_packet(channels: u8, pre_skip: u16) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"OpusHead");
        v.push(1); // version
        v.push(channels);
        v.extend_from_slice(&pre_skip.to_le_bytes());
        v.extend_from_slice(&48_000u32.to_le_bytes()); // input_sample_rate
        v.extend_from_slice(&0i16.to_le_bytes()); // output_gain
        v.push(0); // mapping_family
        v
    }

    /// A minimal `OpusTags` comment header: magic + empty vendor string + zero
    /// user comments.
    fn opus_tags_packet() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"OpusTags");
        v.extend_from_slice(&0u32.to_le_bytes()); // vendor_length
        v.extend_from_slice(&0u32.to_le_bytes()); // user_comment_list_length
        v
    }

    /// A minimal two-page Ogg-Opus stream: BOS page carrying `OpusHead`, next
    /// page carrying `OpusTags`.
    fn minimal_opus_stream(channels: u8, pre_skip: u16) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(
            0x02,
            0,
            7,
            0,
            &opus_head_packet(channels, pre_skip),
        ));
        bytes.extend(ogg_page(0x00, 0, 7, 1, &opus_tags_packet()));
        bytes
    }

    #[test]
    fn new_parses_headers_and_reports_48k_stream_info() {
        let bytes = minimal_opus_stream(2, 312);
        let dec = OpusDecoder::new(SliceSource::new(&bytes)).expect("open");
        let info = dec.stream_info();
        assert_eq!(info.sample_rate, 48_000);
        assert_eq!(info.channels, 2);
        assert_eq!(info.format, SampleFormat::Int { bits: 16 });
    }

    #[test]
    fn decode_into_is_ok_zero_until_codec_lands() {
        let bytes = minimal_opus_stream(1, 0);
        let mut dec = OpusDecoder::new(SliceSource::new(&bytes)).expect("open");
        let mut out = [7i32; 8];
        assert_eq!(dec.decode_into(&mut out).unwrap(), 0);
    }

    #[test]
    fn seek_to_zero_returns_to_start_on_a_headers_only_stream() {
        // Seeking is implemented (Task 13): on a headers-only stream (no audio
        // pages), seek-to-0 rewinds past the two Opus headers and resets,
        // reporting output sample 0 — no longer `Unsupported`. A subsequent
        // decode is a clean end of stream (nothing to decode).
        let bytes = minimal_opus_stream(2, 0);
        let mut dec = OpusDecoder::new(SliceSource::new(&bytes)).expect("open");
        assert_eq!(dec.seek_to(0).unwrap(), 0);
        let mut out = [7i32; 8];
        assert_eq!(dec.decode_into(&mut out).unwrap(), 0);
    }

    #[test]
    fn total_frames_subtracts_pre_skip_from_the_final_granule() {
        let mut bytes = minimal_opus_stream(1, 100);
        // A trailing audio page whose granule (48 kHz units) is the total
        // sample count through the stream's end.
        bytes.extend(ogg_page(0x04, 4_100, 7, 2, b"audio-packet"));
        let dec = OpusDecoder::new(SliceSource::new(&bytes)).expect("open");
        assert_eq!(dec.total_frames(), Some(4_000));
    }

    #[test]
    fn total_frames_is_none_without_a_discoverable_granule() {
        // Every page's granule is `u64::MAX` ("no packet finished on this
        // page") — `end_granule`'s backward scan finds no real granule to
        // report, so the stream's length is unknown.
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, u64::MAX, 7, 0, &opus_head_packet(1, 0)));
        bytes.extend(ogg_page(0x04, u64::MAX, 7, 1, &opus_tags_packet()));
        let dec = OpusDecoder::new(SliceSource::new(&bytes)).expect("open");
        assert_eq!(dec.total_frames(), None);
    }

    #[test]
    fn stereo_header_mono_hybrid_packet_decodes_without_panicking() {
        // RFC 6716 §3.1: the packet's `config.stereo` s-bit is independent of
        // the container's `OpusHead` channel count — a stereo stream may
        // legally carry a mono hybrid packet. Such a packet decodes through the
        // shared `OpusFrameDecoder` core (mono SILK/CELT upmixed to the stereo
        // output); `fill_packet`'s per-frame scratch is sized for two channels
        // (`MAX_FRAME_SAMPLES`), so the interleaved write is in bounds. This
        // exercises that path end-to-end with an (arbitrary) short payload and
        // asserts only that it does not panic and yields a bounded, in-range
        // result — the garbage frame's exact samples are unspecified.
        let mut bytes = minimal_opus_stream(/* channels */ 2, /* pre_skip */ 0);
        // TOC byte: config 12 (Hybrid, SWB, 10 ms), stereo bit UNSET, code 0.
        let toc = 12u8 << 3; // mode=Hybrid, stereo=0, code=0
        let audio_packet = [toc, 0xAA, 0xBB, 0xCC];
        bytes.extend(ogg_page(0x04, 480, 7, 2, &audio_packet));
        let mut dec = OpusDecoder::new(SliceSource::new(&bytes)).expect("open");
        let mut out = [7i32; 4096];
        // Must not panic; whatever it returns must be interleaved-stereo-sized
        // and inside the i16 range.
        let n = dec.decode_into(&mut out).expect("decode must not error");
        assert!(
            n % 2 == 0,
            "stereo output must be an even (interleaved) count"
        );
        assert!(
            out[..n].iter().all(|&s| (-32768..=32767).contains(&s)),
            "decoded samples must be in i16 range"
        );
    }

    #[test]
    fn rejects_a_second_packet_that_is_not_opustags() {
        let mut bytes = Vec::new();
        bytes.extend(ogg_page(0x02, 0, 7, 0, &opus_head_packet(1, 0)));
        bytes.extend(ogg_page(0x00, 0, 7, 1, b"not-opus-tags-at-all"));
        assert_eq!(
            OpusDecoder::new(SliceSource::new(&bytes)).err(),
            Some(DecodeError::Malformed)
        );
    }
}
