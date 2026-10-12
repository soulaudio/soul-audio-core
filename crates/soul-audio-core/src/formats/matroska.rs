//! Matroska / WebM decoder: the [`MatroskaDemuxer`] container feeding the
//! existing per-codec decoders, selected by `CodecID`.
//!
//! v1 supports the dominant `.webm`/`.mka` audio: **Opus** (`A_OPUS`) and
//! **FLAC** (`A_FLAC`). Opus drives the shared [`OpusFrameDecoder`] core per
//! `SimpleBlock` (the GREEN packet-in/PCM-out seam), applying the `OpusHead`
//! pre-skip and output gain; FLAC reuses [`FlacDecoder`] unchanged over a
//! [`MatroskaFlacReader`] byte adapter — the same trick
//! [`OggReader`](crate::demux::ogg::OggReader) uses for FLAC-in-Ogg. AAC and
//! Vorbis in Matroska are follow-ups (see the codec-seam notes); Cues-based
//! seeking is likewise deferred — this forward decoder reports no length and
//! declines `seek_to`.

use embedded_io::{ErrorType, Read};

use crate::decoder::{DecodeError, Decoder};
use crate::demux::{Demuxer, MatroskaDemuxer};
use crate::formats::flac::FlacDecoder;
use crate::formats::opus::conformance::OpusFrameDecoder;
use crate::formats::opus::container::OpusHead;
use crate::formats::opus::linear_output_gain;
use crate::formats::opus::toc;
use crate::io::Source;
use crate::model::{SampleFormat, StreamInfo};

/// Opus always decodes at 48 kHz (RFC 6716 §2).
const OPUS_RATE: u32 = 48_000;
/// Largest Opus packet (one `SimpleBlock` frame) we reassemble. Real Opus audio
/// packets are well under this; an over-long frame is a graceful `Unsupported`.
const MAX_OPUS_PACKET: usize = 8192;
/// One internal Opus frame, interleaved: 60 ms SILK (2880 per-channel) × stereo.
const MAX_OPUS_FRAME: usize = 2880 * 2;
/// One Opus packet's decoded interleaved samples: 120 ms × stereo.
const MAX_OPUS_PCM: usize = 5760 * 2;
/// Largest FLAC frame (one `SimpleBlock`) buffered by [`MatroskaFlacReader`].
/// Covers CD-style content (≤ 4096-block / ≤ 2 ch / ≤ 24-bit ≈ 25 KiB); an
/// over-long frame is a graceful `Unsupported` rather than a silent truncation.
const MAX_FLAC_FRAME: usize = 48 * 1024;
/// Largest `CodecPrivate` (FLAC's `fLaC` + STREAMINFO is tens of bytes).
const MAX_FLAC_CODEC_PRIVATE: usize = 1024;

/// A Matroska / WebM decoder: the container plus whichever per-codec core the
/// track's `CodecID` selects.
///
/// The `Flac` variant is much larger than `Opus` (it embeds `FlacDecoder`'s
/// inline no-alloc block buffers plus the frame-staging adapter). Boxing is
/// impossible under `#![no_std]`, so — like [`AnyDecoder`](crate::dispatch::AnyDecoder)
/// itself — the size gap is accepted.
#[allow(clippy::large_enum_variant)]
pub enum MatroskaDecoder<S> {
    /// `A_OPUS` — the shared Opus core driven per `SimpleBlock`.
    Opus(MatroskaOpus<S>),
    /// `A_FLAC` — [`FlacDecoder`] over a native-FLAC byte adapter.
    Flac(FlacDecoder<MatroskaFlacReader<S>>),
}

impl<S: Source> MatroskaDecoder<S> {
    /// Parse the Matroska header and build the decoder for its audio track.
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] for a non-audio / unsupported `CodecID`
    /// (only Opus and FLAC in v1), plus the demuxer's own errors.
    pub fn new(src: S) -> Result<Self, DecodeError> {
        let demux = MatroskaDemuxer::new(src)?;
        match demux.codec_id() {
            b"A_OPUS" => {
                let head = OpusHead::parse(demux.codec_private())?;
                // Surround (mapping family 1/255) in Matroska is a follow-up,
                // mirroring the Ogg multistream split.
                if head.mapping_family != 0 {
                    return Err(DecodeError::Unsupported);
                }
                Ok(MatroskaDecoder::Opus(MatroskaOpus::new(demux, &head)))
            }
            b"A_FLAC" => {
                let reader = MatroskaFlacReader::new(demux)?;
                Ok(MatroskaDecoder::Flac(FlacDecoder::new(reader)?))
            }
            _ => Err(DecodeError::Unsupported),
        }
    }

    /// Seek is not yet implemented for Matroska (Cues parsing is a follow-up).
    ///
    /// # Errors
    /// Always [`DecodeError::Unsupported`].
    pub fn seek_to(&mut self, _sample: u64) -> Result<u64, DecodeError> {
        Err(DecodeError::Unsupported)
    }

    /// Seconds variant of [`Self::seek_to`]; likewise unsupported.
    ///
    /// # Errors
    /// Always [`DecodeError::Unsupported`].
    pub fn seek_to_time(&mut self, _secs: f64) -> Result<u64, DecodeError> {
        Err(DecodeError::Unsupported)
    }

    /// No declared length yet (needs the Info `Duration` / Cues — a follow-up).
    #[must_use]
    pub fn total_frames(&self) -> Option<u64> {
        None
    }
}

impl<S: Source> Decoder for MatroskaDecoder<S> {
    fn stream_info(&self) -> StreamInfo {
        match self {
            MatroskaDecoder::Opus(d) => d.stream_info(),
            MatroskaDecoder::Flac(d) => d.stream_info(),
        }
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        match self {
            MatroskaDecoder::Opus(d) => d.decode_into(out),
            MatroskaDecoder::Flac(d) => d.decode_into(out),
        }
    }
}

/// Opus-in-Matroska: the shared [`OpusFrameDecoder`] core driven one
/// `SimpleBlock` (= one Opus packet) at a time, applying the `OpusHead`
/// stream-global pre-skip and output gain. No Ogg granule end-trim (Matroska
/// uses the `DiscardPadding` element, a follow-up); the few ms of trailing
/// encoder padding are emitted.
pub struct MatroskaOpus<S> {
    demux: MatroskaDemuxer<S>,
    core: OpusFrameDecoder,
    channels: usize,
    output_gain: f32,
    /// Per-channel samples still to discard as leading pre-skip (RFC 7845 §4.2).
    skip_remaining: u64,
    packet: [u8; MAX_OPUS_PACKET],
    pcm: [i32; MAX_OPUS_PCM],
    pcm_len: usize,
    pcm_pos: usize,
}

impl<S: Source> MatroskaOpus<S> {
    fn new(demux: MatroskaDemuxer<S>, head: &OpusHead) -> Self {
        let channels = usize::from(head.channels).clamp(1, 2);
        Self {
            demux,
            core: OpusFrameDecoder::new(channels),
            channels,
            output_gain: linear_output_gain(head.output_gain_q8),
            skip_remaining: u64::from(head.pre_skip),
            packet: [0; MAX_OPUS_PACKET],
            pcm: [0; MAX_OPUS_PCM],
            pcm_len: 0,
            pcm_pos: 0,
        }
    }

    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: OPUS_RATE,
            channels: self.channels as u16,
            format: SampleFormat::Int { bits: 16 },
        }
    }

    /// Decode the next Opus packet into [`Self::pcm`], applying the leading
    /// pre-skip. Leaves `pcm_len == 0` at a clean end of stream.
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
            let mut scratch = [0i32; MAX_OPUS_FRAME];
            for &frame in frames.iter().take(count) {
                // Per-channel samples this frame yields (a DTX/empty frame is
                // concealed as silence for its nominal span).
                let produced_pch = if frame.is_empty() {
                    frame_pch
                } else {
                    self.core
                        .decode_frame(frame, config, self.output_gain, &mut scratch)
                        / channels
                };
                // Leading pre-skip: drop whole-or-partial frames below the skip.
                let skip = self.skip_remaining.min(produced_pch as u64) as usize;
                self.skip_remaining -= skip as u64;
                let emit = produced_pch - skip;
                if emit == 0 {
                    continue;
                }
                let end = (self.pcm_len + emit * channels).min(self.pcm.len());
                let take = end - self.pcm_len;
                if frame.is_empty() {
                    self.pcm[self.pcm_len..end].fill(0);
                } else {
                    let src = &scratch[skip * channels..skip * channels + take];
                    self.pcm[self.pcm_len..end].copy_from_slice(src);
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
            out[w..w + take].copy_from_slice(&self.pcm[self.pcm_pos..self.pcm_pos + take]);
            self.pcm_pos += take;
            w += take;
        }
        Ok(w)
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
