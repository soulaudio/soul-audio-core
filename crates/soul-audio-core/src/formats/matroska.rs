//! Matroska / WebM decoder: the [`MatroskaDemuxer`] container feeding the
//! existing per-codec decoders, selected by `CodecID`.
//!
//! Supports the common `.webm`/`.mka` audio codecs: **Opus** (`A_OPUS`), **FLAC**
//! (`A_FLAC`), and **AAC** (`A_AAC`). Opus drives the shared [`OpusFrameDecoder`]
//! core per `SimpleBlock` (the packet-in/PCM-out seam), applying the `OpusHead`
//! pre-skip and output gain; FLAC reuses [`FlacDecoder`] unchanged over a
//! [`MatroskaFlacReader`] byte adapter (the same trick
//! [`OggReader`](crate::demux::ogg::OggReader) uses for FLAC-in-Ogg); AAC feeds
//! [`AacDecoder`] one access unit per block with the `CodecPrivate`
//! `AudioSpecificConfig`. Vorbis (`A_VORBIS`) decodes too, but — needing a
//! caller-provided arena — it is **consumer-dispatched** (built by the
//! desktop/CLI, not through `AnyDecoder`) over the [`MatroskaVorbisOgg`]
//! Ogg-synthesizing adapter, exactly like bare Ogg-Vorbis. Seeking uses the
//! container's Cues index — the demuxer jumps to the Cluster covering the target
//! and the decoder only decode-discards within it (frame-granular at the index's
//! ms resolution; FLAC self-anchors from its frame header, so it stays sample-
//! exact). A Cues-less file falls back to restart + linear forward-decode.

use embedded_io::{ErrorType, Read};

use crate::decoder::{DecodeError, Decoder, SeekableDecoder};
#[cfg(feature = "aac")]
use crate::demux::mp4::parse_audio_specific_config;
use crate::demux::{Demuxer, MatroskaDemuxer};
#[cfg(feature = "aac")]
use crate::formats::aac::AacDecoder;
use crate::formats::flac::FlacDecoder;
use crate::formats::opus::conformance::OpusFrameDecoder;
use crate::formats::opus::container::OpusHead;
use crate::formats::opus::linear_output_gain;
use crate::formats::opus::toc;
use crate::formats::scratch::ScratchCursor;
use crate::io::{SeekSource, Source};
use crate::model::{SampleFormat, StreamInfo};

/// Opus always decodes at 48 kHz (RFC 6716 §2).
const OPUS_RATE: u32 = 48_000;

/// Matroska tick (`TimestampScale` nanoseconds) → per-channel sample count at
/// `rate`: `samples = ticks · scale · rate / 1e9`. Integer-only (`u128` widens
/// the product so a long stream's ticks can't overflow), truncating — the seek
/// is frame-granular, the decoder then discards to the exact sample.
pub(crate) fn ticks_to_samples(ticks: u64, rate: u32, scale: u64) -> u64 {
    ((u128::from(ticks) * u128::from(scale) * u128::from(rate)) / 1_000_000_000u128) as u64
}

/// Per-channel sample count → Matroska tick (the inverse of [`ticks_to_samples`],
/// for turning a seek target into the `CueTime` to look up). Truncating, so the
/// chosen Cue is always at/under the target time (never past it).
pub(crate) fn samples_to_ticks(samples: u64, rate: u32, scale: u64) -> u64 {
    let denom = u128::from(scale) * u128::from(rate);
    if denom == 0 {
        return 0;
    }
    (u128::from(samples) * 1_000_000_000u128 / denom) as u64
}
/// Largest Opus packet (one `SimpleBlock` frame) we reassemble. Real Opus audio
/// packets are well under this; an over-long frame is a graceful `Unsupported`.
const MAX_OPUS_PACKET: usize = 8192;
/// One internal Opus frame, interleaved: 60 ms SILK (2880 per-channel) × stereo.
const MAX_OPUS_FRAME: usize = 2880 * 2;
/// One Opus packet's decoded interleaved samples: 120 ms × stereo.
const MAX_OPUS_PCM: usize = 5760 * 2;
/// Largest FLAC frame (one `SimpleBlock`) buffered by [`MatroskaFlacReader`].
/// Covers the full range [`crate::formats::flac::FlacDecoder`] accepts on the
/// desktop profile (≤ 4608-block / ≤ 8 ch / ≤ 32-bit VERBATIM worst case); an
/// over-long frame is a graceful `Unsupported` rather than a silent
/// truncation.
///
/// **`embedded-profile`** (Lever A') tightens this to the realistic stereo
/// ceiling (≤ 4608-block / ≤ 2 ch / ≤ 24-bit ≈ 27.6 KiB, rounded up to 28 KiB):
/// [`MatroskaFlacReader`] is this module's one remaining *inline* decode
/// buffer (48 KiB), and it can't be externalized to the shared scratch `P`
/// [`FlacDecoder`]/[`AacDecoder`]/[`MatroskaOpus`] carve their own buffers
/// from — doing so would need the inner `FlacDecoder<MatroskaFlacReader<S,
/// P>, P>` and this reader to each independently *own* a disjoint slice of
/// the SAME `P` while both stay alive for the decoder's whole lifetime, and
/// `P` is taken **by value** everywhere in this crate (`Vec<i32>`/`Box<[i32]>`
/// on desktop, a `&'static mut [i32]` StaticCell slice on embedded) — there's
/// no owned value that splits into two further owned values without either
/// `unsafe` (forbidden here) or restructuring the shared
/// [`crate::demux::Demuxer::next_packet`] trait (used identically by
/// Opus/FLAC/Vorbis-in-Matroska) away from its whole-packet-into-one-buffer
/// contract. So this buffer stays inline, just capped smaller under
/// `embedded-profile` — the same graceful-`Unsupported` tradeoff
/// [`crate::formats::flac::FlacDecoder`]'s own `MAX_CHANNELS` cap already
/// makes for >2-channel FLAC. A 32-bit or >2-channel FLAC-in-Matroska stream
/// (vanishingly rare in practice) declines cleanly instead of fitting; every
/// real-world stereo/≤24-bit stream (including CD-style ≤4096-block content)
/// still decodes unchanged.
#[cfg(not(feature = "embedded-profile"))]
const MAX_FLAC_FRAME: usize = 48 * 1024;
#[cfg(feature = "embedded-profile")]
const MAX_FLAC_FRAME: usize = 28 * 1024;
/// Largest `CodecPrivate` (FLAC's `fLaC` + STREAMINFO is tens of bytes).
const MAX_FLAC_CODEC_PRIVATE: usize = 1024;

/// A Matroska / WebM decoder: the container plus whichever per-codec core the
/// track's `CodecID` selects.
///
/// `P` is the shared scratch store [`MatroskaOpus`]/[`FlacDecoder`]/[`AacDecoder`]
/// each carve their own decode buffer(s) from (see
/// [`AnyDecoder`](crate::dispatch::AnyDecoder)'s doc comment) — every variant
/// uses it now, each independently (only one variant is ever live per
/// `MatroskaDecoder`, so they never contend for the same storage).
#[allow(clippy::large_enum_variant)]
pub enum MatroskaDecoder<S, P> {
    /// `A_OPUS` — the shared Opus core driven per `SimpleBlock`, carving its
    /// decoded-PCM buffer from the shared `P` scratch.
    Opus(MatroskaOpus<S, P>),
    /// `A_FLAC` — [`FlacDecoder`] over a native-FLAC byte adapter, carving its
    /// decode planes from the shared `P` scratch.
    Flac(FlacDecoder<MatroskaFlacReader<S>, P>),
    /// `A_AAC` — [`AacDecoder`] fed one `SimpleBlock` (= raw access unit) at a
    /// time; config from the `CodecPrivate` `AudioSpecificConfig`. Carves its
    /// decoded-PCM carry buffer from the same shared `P` scratch. Patent-gated
    /// behind the `aac` feature (OFF by default).
    #[cfg(feature = "aac")]
    Aac(AacDecoder<S, P>),
}

impl<S: Source, P: AsRef<[i32]> + AsMut<[i32]>> MatroskaDecoder<S, P> {
    /// Parse the Matroska header and build the decoder for its audio track.
    /// `scratch` is the shared working-buffer storage the `A_OPUS`/`A_FLAC`/
    /// `A_AAC` tracks each carve their own decode buffer(s) from (see
    /// [`AnyDecoder`](crate::dispatch::AnyDecoder)'s doc comment).
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] for a non-audio / unsupported `CodecID`
    /// (only Opus, FLAC, and AAC in v1), a `scratch` too small for the chosen
    /// codec's decode buffer(s), plus the demuxer's own errors.
    pub fn new_in(src: S, scratch: P) -> Result<Self, DecodeError> {
        Self::from_demuxer_in(MatroskaDemuxer::new(src)?, scratch)
    }

    /// Build from an already-parsed demuxer — lets a consumer peek
    /// [`MatroskaDemuxer::codec_id`] (e.g. to route `A_VORBIS` to the arena-backed
    /// Vorbis path) and hand the demuxer here for everything else, without
    /// re-reading the container. `scratch` is as [`Self::new_in`].
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] for an unsupported `CodecID` — including
    /// `A_VORBIS`, which is consumer-dispatched via [`MatroskaVorbisOgg`].
    pub fn from_demuxer_in(demux: MatroskaDemuxer<S>, scratch: P) -> Result<Self, DecodeError> {
        match demux.codec_id() {
            b"A_OPUS" => {
                let head = OpusHead::parse(demux.codec_private())?;
                // Surround (mapping family 1/255) in Matroska is a follow-up,
                // mirroring the Ogg multistream split.
                if head.mapping_family != 0 {
                    return Err(DecodeError::Unsupported);
                }
                Ok(MatroskaDecoder::Opus(MatroskaOpus::new_in(
                    demux, &head, scratch,
                )?))
            }
            b"A_FLAC" => {
                let reader = MatroskaFlacReader::new(demux)?;
                Ok(MatroskaDecoder::Flac(FlacDecoder::new_in(reader, scratch)?))
            }
            // AAC is patent-gated behind the `aac` feature; with it off, an
            // `A_AAC` track falls through to `Unsupported` below.
            #[cfg(feature = "aac")]
            b"A_AAC" => {
                let config = parse_audio_specific_config(demux.codec_private())?;
                Ok(MatroskaDecoder::Aac(AacDecoder::new_matroska_in(
                    demux, config, scratch,
                )?))
            }
            _ => Err(DecodeError::Unsupported),
        }
    }

    /// No declared length yet (needs the Info `Duration` / Cues — a follow-up).
    #[must_use]
    pub fn total_frames(&self) -> Option<u64> {
        None
    }
}

impl<S: SeekSource, P: AsRef<[i32]> + AsMut<[i32]>> MatroskaDecoder<S, P> {
    /// Seek to per-channel `sample`: a Cues-index jump to the covering Cluster
    /// plus a short forward decode-discard within it (linear fallback when the
    /// file carries no Cues).
    ///
    /// # Errors
    /// Propagates the container seek/restart + forward-decode errors.
    pub fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        match self {
            MatroskaDecoder::Opus(d) => d.seek_to(sample),
            MatroskaDecoder::Flac(d) => SeekableDecoder::seek_to(d, sample),
            #[cfg(feature = "aac")]
            MatroskaDecoder::Aac(d) => d.seek_to(sample),
        }
    }

    /// Seek to `secs` seconds (convenience over [`Self::seek_to`]).
    ///
    /// # Errors
    /// As [`Self::seek_to`].
    pub fn seek_to_time(&mut self, secs: f64) -> Result<u64, DecodeError> {
        let rate = self.stream_info().sample_rate.max(1);
        self.seek_to(crate::decoder::secs_to_samples(secs, rate))
    }
}

impl<S: Source, P: AsRef<[i32]> + AsMut<[i32]>> Decoder for MatroskaDecoder<S, P> {
    fn stream_info(&self) -> StreamInfo {
        match self {
            MatroskaDecoder::Opus(d) => d.stream_info(),
            MatroskaDecoder::Flac(d) => d.stream_info(),
            #[cfg(feature = "aac")]
            MatroskaDecoder::Aac(d) => d.stream_info(),
        }
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        match self {
            MatroskaDecoder::Opus(d) => d.decode_into(out),
            MatroskaDecoder::Flac(d) => d.decode_into(out),
            #[cfg(feature = "aac")]
            MatroskaDecoder::Aac(d) => d.decode_into(out),
        }
    }

    /// The contained track codec (`A_OPUS` / `A_FLAC` / `A_AAC`) — the
    /// lossless-vs-lossy identity the Matroska container itself can't give.
    fn codec(&self) -> crate::model::SourceCodec {
        match self {
            MatroskaDecoder::Opus(_) => crate::model::SourceCodec::Opus,
            MatroskaDecoder::Flac(_) => crate::model::SourceCodec::Flac,
            #[cfg(feature = "aac")]
            MatroskaDecoder::Aac(_) => crate::model::SourceCodec::Aac,
        }
    }

    /// The channel layout of the **contained codec**. Matroska itself carries no
    /// channel mask — `Audio` has only `Channels`, and `ChannelPositions` is
    /// deprecated and absent in practice — but the codec inside it does fix an
    /// order: FLAC's is spec-ordered like WAVEFORMATEXTENSIBLE, AAC's puts the
    /// centre first. Forwarding the inner decoder's layout is therefore the only
    /// correct answer here, and it is what lets a multichannel FLAC/AAC track in
    /// an `.mka`/`.webm` fold to stereo in the ambiguous channel counts (4/5/7)
    /// the deck will not guess at. Opus-in-Matroska is clamped to stereo
    /// ([`MatroskaOpus::new_in`]), so there is no layout to report.
    fn channel_layout(&self) -> Option<crate::channels::ChannelLayout> {
        match self {
            MatroskaDecoder::Opus(_) => None,
            MatroskaDecoder::Flac(d) => d.channel_layout(),
            #[cfg(feature = "aac")]
            MatroskaDecoder::Aac(d) => d.channel_layout(),
        }
    }
}

/// Opus-in-Matroska: the shared [`OpusFrameDecoder`] core driven one
/// `SimpleBlock` (= one Opus packet) at a time, applying the `OpusHead`
/// stream-global pre-skip and output gain. No Ogg granule end-trim (Matroska
/// uses the `DiscardPadding` element, a follow-up); the few ms of trailing
/// encoder padding are emitted.
///
/// The decoded-PCM-for-the-current-packet buffer is **caller-provided
/// scratch** `P` (`AsRef<[i32]> + AsMut<[i32]>`), carved by a bump cursor in
/// [`Self::new_in`] — the SAME shared scratch
/// [`FlacDecoder`]/[`AacDecoder`]/[`crate::formats::opus::OpusDecoder`] use
/// (decoder-memory-externalization spec §7), exactly mirroring the standalone
/// Ogg-Opus decoder's `pcm_buf` carve. `packet` (the pre-decode `SimpleBlock`
/// byte staging) stays an inline fixed-size array — it's raw encoded bytes,
/// not PCM.
pub struct MatroskaOpus<S, P> {
    demux: MatroskaDemuxer<S>,
    core: OpusFrameDecoder,
    channels: usize,
    output_gain: f32,
    /// The stream's `OpusHead` pre-skip (re-applied from the start after a seek).
    pre_skip: u64,
    /// Per-channel samples still to discard as leading pre-skip (RFC 7845 §4.2).
    skip_remaining: u64,
    packet: [u8; MAX_OPUS_PACKET],
    /// Caller-provided scratch the decoded-PCM-for-the-current-packet buffer
    /// is carved from (see the struct doc comment).
    scratch: P,
    /// i32-slot offset of the `MAX_OPUS_PCM`-long PCM buffer within
    /// [`Self::scratch`] (see [`Self::pcm_ref`]/[`Self::pcm_mut`]).
    pcm_off: usize,
    /// Number of valid samples in the scratch PCM buffer.
    pcm_len: usize,
    /// Read cursor into the scratch PCM buffer.
    pcm_pos: usize,
}

impl<S: Source, P: AsRef<[i32]> + AsMut<[i32]>> MatroskaOpus<S, P> {
    /// Build over an already-positioned demuxer and parsed `OpusHead`, carving
    /// the decoded-PCM buffer from `scratch` (must be at least `MAX_OPUS_PCM`
    /// samples long — [`crate::dispatch::decoder_scratch_len`] is comfortably
    /// larger).
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] if `scratch` is too small to hold the PCM
    /// buffer (never a panic / out-of-bounds index on undersized storage).
    fn new_in(demux: MatroskaDemuxer<S>, head: &OpusHead, scratch: P) -> Result<Self, DecodeError> {
        let channels = usize::from(head.channels).clamp(1, 2);
        let mut cursor = ScratchCursor::new(scratch.as_ref().len());
        let pcm_off = cursor.alloc(MAX_OPUS_PCM)?;
        Ok(Self {
            demux,
            core: OpusFrameDecoder::new(channels),
            channels,
            output_gain: linear_output_gain(head.output_gain_q8),
            pre_skip: u64::from(head.pre_skip),
            skip_remaining: u64::from(head.pre_skip),
            packet: [0; MAX_OPUS_PACKET],
            scratch,
            pcm_off,
            pcm_len: 0,
            pcm_pos: 0,
        })
    }

    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: OPUS_RATE,
            channels: self.channels as u16,
            format: SampleFormat::Int { bits: 16 },
        }
    }

    /// Read-only view of the `MAX_OPUS_PCM`-long scratch PCM buffer.
    fn pcm_ref(&self) -> &[i32] {
        &self.scratch.as_ref()[self.pcm_off..self.pcm_off + MAX_OPUS_PCM]
    }

    /// Decode the next Opus packet into the scratch PCM buffer, applying the
    /// leading pre-skip. Leaves `pcm_len == 0` at a clean end of stream.
    fn fill(&mut self) -> Result<(), DecodeError> {
        self.pcm_len = 0;
        self.pcm_pos = 0;
        let channels = self.channels;
        loop {
            let n = match self.demux.next_packet(&mut self.packet)? {
                Some(p) => p.data.len(),
                None => return Ok(()), // end of stream
            };
            let mut frames: [&[u8]; toc::MAX_FRAMES] = [&[][..]; toc::MAX_FRAMES];
            let (config, count) = match toc::frames(&self.packet[..n], &mut frames) {
                Ok(v) => v,
                Err(_) => continue, // skip a malformed packet rather than abort
            };
            let frame_pch = usize::from(config.frame_size_48k);
            // One decoded frame's worth of interleaved PCM, gain already applied
            // by the core. A plain stack-local temporary (not struct-embedded
            // scratch): it lives only for this one loop iteration, reused across
            // the packet's frames.
            let mut frame_pcm = [0i32; MAX_OPUS_FRAME];
            for &frame in frames.iter().take(count) {
                // Per-channel samples this frame yields (a DTX/empty frame is
                // concealed as silence for its nominal span).
                let produced_pch = if frame.is_empty() {
                    frame_pch
                } else {
                    self.core
                        .decode_frame(frame, config, self.output_gain, &mut frame_pcm)
                        / channels
                };
                // Leading pre-skip: drop whole-or-partial frames below the skip.
                let skip = self.skip_remaining.min(produced_pch as u64) as usize;
                self.skip_remaining -= skip as u64;
                let emit = produced_pch - skip;
                if emit == 0 {
                    continue;
                }
                let end = (self.pcm_len + emit * channels).min(MAX_OPUS_PCM);
                let take = end - self.pcm_len;
                // Direct `self.scratch`/`self.pcm_off` field access (not the
                // `Self::pcm_mut` helper) — `frames`'s slices still borrow
                // `self.packet` here, and a method call would need all of
                // `self`, not just the disjoint `scratch` field.
                let off = self.pcm_off;
                if frame.is_empty() {
                    self.scratch.as_mut()[off + self.pcm_len..off + end].fill(0);
                } else {
                    let src = &frame_pcm[skip * channels..skip * channels + take];
                    self.scratch.as_mut()[off + self.pcm_len..off + end].copy_from_slice(src);
                }
                self.pcm_len = end;
            }
            if self.pcm_len > 0 {
                return Ok(());
            }
            // Whole packet consumed by pre-skip — keep reading.
        }
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        let mut w = 0;
        while w < out.len() {
            if self.pcm_pos >= self.pcm_len {
                self.fill()?;
                if self.pcm_len == 0 {
                    break; // end of stream
                }
            }
            let avail = self.pcm_len - self.pcm_pos;
            let take = avail.min(out.len() - w);
            out[w..w + take].copy_from_slice(&self.pcm_ref()[self.pcm_pos..self.pcm_pos + take]);
            self.pcm_pos += take;
            w += take;
        }
        Ok(w)
    }
}

impl<S: SeekSource, P: AsRef<[i32]> + AsMut<[i32]>> MatroskaOpus<S, P> {
    /// Seek to per-channel `sample`. With a Cues index the demuxer jumps straight
    /// to the Cluster covering the target ([`MatroskaDemuxer::seek_to_cue`]) and
    /// the decoder forward-decode-discards only within it — O(1) on the index +
    /// one cluster, not O(target). Without Cues it falls back to restarting at the
    /// first cluster and decode-discarding from the start (linear).
    ///
    /// Opus carries cross-frame state (CELT overlap, SILK history), so the post-
    /// jump decode warms up over the landed cluster's run-in before the target;
    /// the stream-global `OpusHead` pre-skip is a leading discard of the very
    /// first samples, re-applied only on a from-start landing (`base == 0`). A
    /// mid-stream cue landing is already past it. Returns the frame landed on.
    fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        let scale = self.demux.timestamp_scale();
        // Look up a Cue a short pre-roll *before* the target so the landed Cluster
        // gives the Opus cross-frame state (CELT overlap, SILK history) a run-in to
        // converge before the emitted tail — otherwise a target near a Cluster
        // boundary would emit cold-start frames. The decode-discard from `base` to
        // `sample` spends that run-in.
        const PREROLL: u64 = 3840; // 80 ms @ 48 kHz
        let lookup_ticks = samples_to_ticks(sample.saturating_sub(PREROLL), OPUS_RATE, scale);
        // `base` is the per-channel output sample of the landed Cluster's first
        // emitted sample. A Cue's time is in the *decoded* timeline (pre-skip
        // included), so a mid-stream landing subtracts the stream-global pre-skip
        // to reach the output (post-pre-skip) timeline the decoder counts in; a
        // cluster-0 (cue time 0) or Cues-less landing starts fresh and re-applies
        // the pre-skip as a leading discard instead.
        let (base, from_start) = match self.demux.seek_to_cue(lookup_ticks)? {
            Some(cue_ticks) if cue_ticks != 0 => (
                ticks_to_samples(cue_ticks, OPUS_RATE, scale).saturating_sub(self.pre_skip),
                false,
            ),
            Some(_) => (0, true),
            None => {
                self.demux.restart()?;
                (0, true)
            }
        };
        self.core = OpusFrameDecoder::new(self.channels);
        self.skip_remaining = if from_start { self.pre_skip } else { 0 };
        self.pcm_len = 0;
        self.pcm_pos = 0;
        let mut landed = base;
        while landed < sample {
            if self.pcm_pos >= self.pcm_len {
                self.fill()?;
                if self.pcm_len == 0 {
                    break;
                }
            }
            let avail = (self.pcm_len - self.pcm_pos) / self.channels;
            let take = avail.min((sample - landed) as usize);
            self.pcm_pos += take * self.channels;
            landed += take as u64;
        }
        Ok(landed)
    }
}

/// An [`embedded_io::Read`] adapter presenting an `A_FLAC` Matroska track as a
/// native FLAC byte stream: the `CodecPrivate` (`fLaC` magic + STREAMINFO …)
/// first, then the concatenated `SimpleBlock` frame bytes. [`FlacDecoder`]
/// re-syncs on the frame sync code, so block boundaries need not be preserved —
/// the same contract as [`OggReader`](crate::demux::ogg::OggReader).
pub struct MatroskaFlacReader<S> {
    demux: MatroskaDemuxer<S>,
    codec_private: [u8; MAX_FLAC_CODEC_PRIVATE],
    cp_len: usize,
    cp_pos: usize,
    frame: [u8; MAX_FLAC_FRAME],
    frame_len: usize,
    frame_pos: usize,
    eos: bool,
}

impl<S: Source> MatroskaFlacReader<S> {
    fn new(demux: MatroskaDemuxer<S>) -> Result<Self, DecodeError> {
        let cp = demux.codec_private();
        if cp.is_empty() || cp.len() > MAX_FLAC_CODEC_PRIVATE {
            return Err(DecodeError::Unsupported);
        }
        let mut codec_private = [0u8; MAX_FLAC_CODEC_PRIVATE];
        codec_private[..cp.len()].copy_from_slice(cp);
        let cp_len = cp.len();
        Ok(Self {
            demux,
            codec_private,
            cp_len,
            cp_pos: 0,
            frame: [0; MAX_FLAC_FRAME],
            frame_len: 0,
            frame_pos: 0,
            eos: false,
        })
    }

    /// Refill [`Self::frame`] with the next `SimpleBlock` payload. Returns false
    /// at end of stream.
    fn next_frame(&mut self) -> Result<bool, DecodeError> {
        match self.demux.next_packet(&mut self.frame)? {
            Some(p) => {
                self.frame_len = p.data.len();
                self.frame_pos = 0;
                Ok(true)
            }
            None => {
                self.eos = true;
                Ok(false)
            }
        }
    }
}

impl<S: SeekSource> MatroskaFlacReader<S> {
    /// Rewind to the first cluster for a seek. The `CodecPrivate` (native FLAC
    /// header) is **not** re-emitted — the [`FlacDemuxer`](crate::demux::FlacDemuxer)
    /// already consumed it; the FLAC decoder re-syncs on the frame sync code
    /// (matching [`OggReader::restart_audio`](crate::demux::ogg::OggReader)).
    pub(crate) fn restart(&mut self) -> Result<(), DecodeError> {
        self.demux.restart()?;
        self.cp_pos = self.cp_len; // header already consumed; stream frames only
        self.frame_len = 0;
        self.frame_pos = 0;
        self.eos = false;
        Ok(())
    }

    /// Cues-index seek for FLAC-in-Matroska: jump the demuxer to the Cluster
    /// covering `target_ticks` ([`MatroskaDemuxer::seek_to_cue`]) and reset the
    /// frame-staging state like [`Self::restart`]. Returns `Ok(true)` on a cue
    /// jump, `Ok(false)` when the file has no Cues index (caller should
    /// [`Self::restart`] instead). The FLAC decoder anchors its absolute position
    /// from the landed frame's own coded sample number, so the jump stays
    /// sample-accurate despite the ms-granular index.
    pub(crate) fn seek_to_cue(&mut self, target_ticks: u64) -> Result<bool, DecodeError> {
        let jumped = self.demux.seek_to_cue(target_ticks)?.is_some();
        if jumped {
            self.cp_pos = self.cp_len;
            self.frame_len = 0;
            self.frame_pos = 0;
            self.eos = false;
        }
        Ok(jumped)
    }

    /// `TimestampScale` (ns/tick) of the container — to turn a sample target into
    /// the `CueTime` tick to look up.
    pub(crate) fn timestamp_scale(&self) -> u64 {
        self.demux.timestamp_scale()
    }
}

impl<S: Source> ErrorType for MatroskaFlacReader<S> {
    type Error = embedded_io::ErrorKind;
}

impl<S: Source> Read for MatroskaFlacReader<S> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        let mut w = 0;
        // CodecPrivate (native FLAC header) first.
        while w < buf.len() && self.cp_pos < self.cp_len {
            buf[w] = self.codec_private[self.cp_pos];
            self.cp_pos += 1;
            w += 1;
        }
        // Then the concatenated frame bytes.
        while w < buf.len() && !self.eos {
            if self.frame_pos >= self.frame_len
                && !self
                    .next_frame()
                    .map_err(|_| embedded_io::ErrorKind::Other)?
            {
                break;
            }
            let avail = self.frame_len - self.frame_pos;
            let take = avail.min(buf.len() - w);
            buf[w..w + take].copy_from_slice(&self.frame[self.frame_pos..self.frame_pos + take]);
            self.frame_pos += take;
            w += take;
        }
        Ok(w)
    }
}

// --- Vorbis-in-Matroska (A_VORBIS) -----------------------------------------

/// Largest Vorbis audio packet (one `SimpleBlock`) we reassemble.
const MAX_VORBIS_PACKET: usize = 16 * 1024;
/// Largest `CodecPrivate` we copy (matches the demuxer's own cap).
const MAX_CODEC_PRIVATE: usize = 16 * 1024;
/// Ogg page header (27 bytes) + the maximum 255-entry segment table.
const OGG_PAGE_SCRATCH: usize = 27 + 255;

/// Where the current synthesized Ogg packet body comes from.
#[derive(Clone, Copy)]
enum Body {
    /// A slice of the `CodecPrivate` header blob (`offset`, `len`).
    Header(usize, usize),
    /// The current audio `SimpleBlock` staged in `audio` (`len`).
    Audio(usize),
}

/// An [`embedded_io::Read`] adapter presenting an `A_VORBIS` Matroska track as a
/// native **Ogg-Vorbis** bitstream, so the arena-backed
/// [`VorbisDecoder`](crate::formats::vorbis::VorbisDecoder) decodes it unchanged
/// (the same trick [`MatroskaFlacReader`] / [`OggReader`](crate::demux::ogg::OggReader)
/// use). One synthesized Ogg page per packet: the three `CodecPrivate` setup
/// headers (Xiph-laced) first, then one per `SimpleBlock`. Every page carries the
/// `-1` granule sentinel, so `VorbisDecoder`'s Ogg end-trim is a no-op (Matroska
/// handles trailing padding via block durations — not parsed here). Because the
/// arena is caller-provided, Vorbis-in-Matroska is **consumer-dispatched** (built
/// by the desktop/CLI with an arena), exactly like bare Ogg-Vorbis.
pub struct MatroskaVorbisOgg<S> {
    demux: MatroskaDemuxer<S>,
    /// The `CodecPrivate` blob (the 3 Xiph-laced Vorbis setup headers).
    cp: [u8; MAX_CODEC_PRIVATE],
    /// `(offset, len)` of each setup header within `cp`.
    headers: [(usize, usize); 3],
    header_count: usize,
    /// Which packet we're emitting next (0..header_count = headers, then audio).
    pkt_index: usize,
    seq: u32,
    /// The current page's header+segment-table bytes.
    page: [u8; OGG_PAGE_SCRATCH],
    page_len: usize,
    page_pos: usize,
    /// The current packet body + cursor.
    body: Option<Body>,
    body_pos: usize,
    audio: [u8; MAX_VORBIS_PACKET],
    eos: bool,
}

impl<S: Source> MatroskaVorbisOgg<S> {
    /// Build over a Matroska demuxer positioned at the first audio block, parsing
    /// the Xiph-laced `CodecPrivate` into the three Vorbis setup headers.
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] for a malformed / oversized `CodecPrivate`.
    pub fn new(demux: MatroskaDemuxer<S>) -> Result<Self, DecodeError> {
        let src = demux.codec_private();
        if src.is_empty() || src.len() > MAX_CODEC_PRIVATE {
            return Err(DecodeError::Unsupported);
        }
        let mut cp = [0u8; MAX_CODEC_PRIVATE];
        cp[..src.len()].copy_from_slice(src);
        let (headers, header_count) = parse_xiph_lacing(&cp[..src.len()])?;
        Ok(Self {
            demux,
            cp,
            headers,
            header_count,
            pkt_index: 0,
            seq: 0,
            page: [0; OGG_PAGE_SCRATCH],
            page_len: 0,
            page_pos: 0,
            body: None,
            body_pos: 0,
            audio: [0; MAX_VORBIS_PACKET],
            eos: false,
        })
    }

    /// Stage the next packet (header or audio block) and synthesize its Ogg page
    /// header into `page`. Returns `false` at end of stream.
    fn next_page(&mut self) -> Result<bool, DecodeError> {
        let (body, body_len) = if self.pkt_index < self.header_count {
            let (off, len) = self.headers[self.pkt_index];
            (Body::Header(off, len), len)
        } else {
            match self.demux.next_packet(&mut self.audio)? {
                Some(p) => {
                    let len = p.data.len();
                    (Body::Audio(len), len)
                }
                None => {
                    self.eos = true;
                    return Ok(false);
                }
            }
        };
        let bos = self.pkt_index == 0;
        self.synthesize_page(bos, body_len);
        self.body = Some(body);
        self.body_pos = 0;
        self.pkt_index += 1;
        Ok(true)
    }

    /// Build an Ogg page header + segment table for a single packet of
    /// `body_len` bytes into `page` (one packet per page; the `-1` granule
    /// sentinel disables end-trim).
    fn synthesize_page(&mut self, bos: bool, body_len: usize) {
        let full = body_len / 255;
        let last = (body_len % 255) as u8;
        let nsegs = (full + 1) as u8;
        let mut h = [0u8; 27];
        h[0..4].copy_from_slice(b"OggS");
        h[4] = 0; // stream structure version
        h[5] = if bos { 0x02 } else { 0x00 }; // header type (BOS on the first page)
        h[6..14].copy_from_slice(&[0xFF; 8]); // granule = -1 (no end-trim ceiling)
        h[14..18].copy_from_slice(&1u32.to_le_bytes()); // serial
        h[18..22].copy_from_slice(&self.seq.to_le_bytes()); // page sequence
        // h[22..26] CRC left 0 (OggPackets does not verify it)
        h[26] = nsegs;
        self.page[0..27].copy_from_slice(&h);
        for i in 0..full {
            self.page[27 + i] = 255;
        }
        self.page[27 + full] = last;
        self.page_len = 27 + full + 1;
        self.page_pos = 0;
        self.seq = self.seq.wrapping_add(1);
    }

    fn body_byte(&self, i: usize) -> u8 {
        match self.body {
            Some(Body::Header(off, _)) => self.cp[off + i],
            Some(Body::Audio(_)) => self.audio[i],
            None => 0,
        }
    }

    fn body_len(&self) -> usize {
        match self.body {
            Some(Body::Header(_, len) | Body::Audio(len)) => len,
            None => 0,
        }
    }
}

/// Parse Matroska Vorbis `CodecPrivate` Xiph lacing: `[count-1][lace lengths…]
/// [packets…]`. Returns each header's `(offset, len)` within `cp` and the count.
fn parse_xiph_lacing(cp: &[u8]) -> Result<([(usize, usize); 3], usize), DecodeError> {
    if cp.is_empty() {
        return Err(DecodeError::Unsupported);
    }
    let count = usize::from(cp[0]) + 1;
    if count != 3 {
        return Err(DecodeError::Unsupported); // Vorbis has exactly 3 setup headers
    }
    let mut p = 1usize;
    let mut lens = [0usize; 3];
    // The first (count-1) lengths are Xiph-laced; the last is the remainder.
    for len in lens.iter_mut().take(count - 1) {
        let mut l = 0usize;
        loop {
            let b = *cp.get(p).ok_or(DecodeError::Unsupported)?;
            p += 1;
            l += usize::from(b);
            if b != 255 {
                break;
            }
        }
        *len = l;
    }
    let consumed: usize = p + lens[..count - 1].iter().sum::<usize>();
    if consumed > cp.len() {
        return Err(DecodeError::Unsupported);
    }
    lens[count - 1] = cp.len() - consumed;
    let mut headers = [(0usize, 0usize); 3];
    let mut off = p;
    for i in 0..count {
        headers[i] = (off, lens[i]);
        off += lens[i];
    }
    Ok((headers, count))
}

impl<S: Source> ErrorType for MatroskaVorbisOgg<S> {
    type Error = embedded_io::ErrorKind;
}

impl<S: Source> Read for MatroskaVorbisOgg<S> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        let mut w = 0;
        while w < buf.len() {
            if self.page_pos < self.page_len {
                let take = (self.page_len - self.page_pos).min(buf.len() - w);
                buf[w..w + take].copy_from_slice(&self.page[self.page_pos..self.page_pos + take]);
                self.page_pos += take;
                w += take;
                continue;
            }
            if self.body_pos < self.body_len() {
                let take = (self.body_len() - self.body_pos).min(buf.len() - w);
                for b in buf[w..w + take].iter_mut() {
                    *b = self.body_byte(self.body_pos);
                    self.body_pos += 1;
                }
                w += take;
                continue;
            }
            if self.eos {
                break;
            }
            if !self
                .next_page()
                .map_err(|_| embedded_io::ErrorKind::Other)?
            {
                break;
            }
        }
        Ok(w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceSource;

    /// An EBML element ID as its big-endian bytes (the marker is already in the
    /// constant). Mirrors the demuxer's own test writer in `demux::matroska`.
    fn id_bytes(id: u32) -> Vec<u8> {
        if id <= 0xFF {
            vec![id as u8]
        } else if id <= 0xFFFF {
            id.to_be_bytes()[2..].to_vec()
        } else if id <= 0xFF_FFFF {
            id.to_be_bytes()[1..].to_vec()
        } else {
            id.to_be_bytes().to_vec()
        }
    }

    /// An EBML size as a 1- or 2-byte vint, so bodies past 127 bytes (a FLAC
    /// `CodecPrivate` plus a 4-channel frame) are expressible.
    fn size_vint(n: usize) -> Vec<u8> {
        if n < 0x7F {
            vec![0x80 | n as u8]
        } else {
            assert!(n < 0x3FFF, "test element too large: {n}");
            vec![0x40 | (n >> 8) as u8, (n & 0xFF) as u8]
        }
    }

    fn elem(id: u32, body: &[u8]) -> Vec<u8> {
        let mut v = id_bytes(id);
        v.extend_from_slice(&size_vint(body.len()));
        v.extend_from_slice(body);
        v
    }

    /// MSB-first bit writer, for hand-building the FLAC frame below.
    struct BitWriter {
        bytes: Vec<u8>,
        cur: u8,
        nbits: u8,
    }

    impl BitWriter {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                cur: 0,
                nbits: 0,
            }
        }
        fn bits(&mut self, val: u64, n: u32) {
            for i in (0..n).rev() {
                self.cur = (self.cur << 1) | ((val >> i) & 1) as u8;
                self.nbits += 1;
                if self.nbits == 8 {
                    self.bytes.push(self.cur);
                    self.cur = 0;
                    self.nbits = 0;
                }
            }
        }
        fn signed(&mut self, val: i32, n: u32) {
            let mask = if n >= 32 { u64::MAX } else { (1u64 << n) - 1 };
            self.bits((val as i64 as u64) & mask, n);
        }
        fn align(&mut self) {
            if self.nbits > 0 {
                self.bits(0, u32::from(8 - self.nbits));
            }
        }
        fn into_bytes(mut self) -> Vec<u8> {
            self.align();
            self.bytes
        }
    }

    /// One FLAC frame of `block` frames × `channels` independent VERBATIM
    /// subframes, `sample[ch][i] = ch * 100 + i`. CRCs are written as zero (the
    /// decoder does not validate them), as in `formats::flac`'s own tests.
    fn flac_verbatim_frame(block: usize, channels: u16) -> Vec<u8> {
        let mut bw = BitWriter::new();
        bw.bits(0b11_1111_1111_1110, 14); // sync
        bw.bits(0, 1); // reserved
        bw.bits(0, 1); // fixed block size
        bw.bits(7, 4); // block size code 7 → explicit 16-bit below
        bw.bits(0, 4); // sample rate from STREAMINFO
        bw.bits(u64::from(channels) - 1, 4); // channel assignment = independent
        bw.bits(0, 3); // sample size from STREAMINFO
        bw.bits(0, 1); // reserved
        bw.bits(0, 8); // UTF-8 frame number 0
        bw.bits((block - 1) as u64, 16);
        bw.bits(0, 8); // CRC-8 (unchecked)
        for c in 0..i32::from(channels) {
            bw.bits(0, 1); // subframe leading 0
            bw.bits(1, 6); // VERBATIM
            bw.bits(0, 1); // no wasted bits
            for i in 0..block as i32 {
                bw.signed(c * 100 + i, 16);
            }
        }
        bw.align();
        bw.bits(0, 16); // CRC-16 (unchecked)
        bw.into_bytes()
    }

    /// A `fLaC` stream header (STREAMINFO only) for use as `CodecPrivate` — what
    /// a real FLAC-in-Matroska track carries.
    fn flac_codec_private(rate: u32, channels: u16, bits: u8, total: u64) -> Vec<u8> {
        let mut si = [0u8; 34];
        let v: u64 = (u64::from(rate) << 44)
            | ((u64::from(channels) - 1) << 41)
            | ((u64::from(bits) - 1) << 36)
            | (total & 0xF_FFFF_FFFF);
        si[10..18].copy_from_slice(&v.to_be_bytes());
        let mut cp = Vec::new();
        cp.extend_from_slice(b"fLaC");
        cp.push(0x80); // last metadata block + STREAMINFO
        cp.extend_from_slice(&[0, 0, 34]);
        cp.extend_from_slice(&si);
        cp
    }

    /// A minimal `A_FLAC` Matroska carrying one `SimpleBlock` = one FLAC frame.
    fn flac_mkv(channels: u16, block: usize) -> Vec<u8> {
        const ID_EBML_HEADER: u32 = 0x1A45_DFA3;
        const ID_SEGMENT: u32 = 0x1853_8067;
        const ID_TRACKS: u32 = 0x1654_AE6B;
        const ID_TRACK_ENTRY: u32 = 0xAE;
        const ID_TRACK_NUMBER: u32 = 0xD7;
        const ID_TRACK_TYPE: u32 = 0x83;
        const ID_CODEC_ID: u32 = 0x86;
        const ID_CODEC_PRIVATE: u32 = 0x63A2;
        const ID_AUDIO: u32 = 0xE1;
        const ID_SAMPLING_FREQUENCY: u32 = 0xB5;
        const ID_CHANNELS: u32 = 0x9F;
        const ID_CLUSTER: u32 = 0x1F43_B675;
        const ID_TIMESTAMP: u32 = 0xE7;
        const ID_SIMPLE_BLOCK: u32 = 0xA3;

        let mut audio = Vec::new();
        audio.extend_from_slice(&elem(ID_SAMPLING_FREQUENCY, &96_000.0f32.to_be_bytes()));
        audio.extend_from_slice(&elem(ID_CHANNELS, &[channels as u8]));

        let mut te = Vec::new();
        te.extend_from_slice(&elem(ID_TRACK_NUMBER, &[1]));
        te.extend_from_slice(&elem(ID_TRACK_TYPE, &[2])); // audio
        te.extend_from_slice(&elem(ID_CODEC_ID, b"A_FLAC"));
        te.extend_from_slice(&elem(
            ID_CODEC_PRIVATE,
            &flac_codec_private(96_000, channels, 16, block as u64),
        ));
        te.extend_from_slice(&elem(ID_AUDIO, &audio));
        let tracks = elem(ID_TRACKS, &elem(ID_TRACK_ENTRY, &te));

        // SimpleBlock: track vint, int16 timestamp, flags, then the frame.
        let mut sb = vec![0x81, 0x00, 0x00, 0x00];
        sb.extend_from_slice(&flac_verbatim_frame(block, channels));
        let mut cluster_body = elem(ID_TIMESTAMP, &[0]);
        cluster_body.extend_from_slice(&elem(ID_SIMPLE_BLOCK, &sb));

        let mut segment = tracks;
        segment.extend_from_slice(&elem(ID_CLUSTER, &cluster_body));
        let mut v = elem(ID_EBML_HEADER, b"\x42\x82\x84webm");
        v.extend_from_slice(&elem(ID_SEGMENT, &segment));
        v
    }

    /// Matroska carries no channel mask of its own, so a multichannel track can
    /// only fold if the **contained codec**'s layout is forwarded. A 4-channel
    /// FLAC track is the case that proves it: 4 channels is layout-ambiguous, so
    /// the deck rejects it outright without a layout — and FLAC fixes 4ch as
    /// quad. Desktop profile only (`embedded-profile` caps FLAC at stereo).
    #[cfg(not(feature = "embedded-profile"))]
    #[test]
    fn multichannel_flac_in_matroska_reports_the_contained_codec_layout() {
        let bytes = flac_mkv(4, 5);
        let mut dec = MatroskaDecoder::new_in(
            SliceSource::new(&bytes),
            vec![0i32; crate::dispatch::decoder_scratch_len()],
        )
        .expect("open A_FLAC matroska");
        assert_eq!(dec.stream_info().channels, 4);
        assert_eq!(
            dec.channel_layout(),
            crate::channels::ChannelLayout::standard(4),
            "the inner FLAC decoder's quad layout must reach the deck"
        );
        // And the stream really does decode — the layout is not reported for a
        // track that would fail to play.
        let mut out = [0i32; 64];
        let n = dec.decode_into(&mut out).expect("decode");
        assert_eq!(n, 5 * 4);
        for i in 0..5 {
            for c in 0..4 {
                assert_eq!(out[i * 4 + c], (c as i32) * 100 + i as i32);
            }
        }
    }

    /// A stereo track is a passthrough, not a fold: FLAC reports no layout for
    /// 1/2 channels, and that `None` must travel unchanged (the forwarding does
    /// not invent a layout for stereo).
    #[test]
    fn stereo_flac_in_matroska_reports_no_layout() {
        let bytes = flac_mkv(2, 5);
        let dec = MatroskaDecoder::new_in(
            SliceSource::new(&bytes),
            vec![0i32; crate::dispatch::decoder_scratch_len()],
        )
        .expect("open A_FLAC matroska");
        assert_eq!(dec.channel_layout(), None);
    }
}
