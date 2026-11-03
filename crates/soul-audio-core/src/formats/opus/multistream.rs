//! Multichannel (surround) Ogg-Opus: RFC 7845 §5.1.1 mapping families 1 and
//! 255. A multistream packet concatenates N independent Opus streams (the first
//! `coupled_count` stereo, the rest mono); each stream is decoded by its own
//! single-stream core ([`OpusFrameDecoder`], the SAME engine the mono/stereo
//! [`OpusDecoder`](super::OpusDecoder) and the conformance path use), and the
//! decoded channels are permuted to the output channels through the header's
//! channel-mapping table.
//!
//! This is a faithful analogue of libopus `opus_multistream_decode`: split the
//! packet into its N sub-packets — all but the last use RFC 6716 Appendix B
//! self-delimiting framing ([`toc::frames_self_delimited`]) so their byte
//! boundaries are explicit — decode each to the packet's common per-channel
//! sample count, then gather `output[c] = decoded[mapping[c]]` (`255` = silent).
//!
//! **Scope:** decode-only. Mapping family 1 standard layouts through 7.1 (≤
//! [`MAX_STREAMS`] streams); larger/non-standard layouts are a clean
//! `Unsupported`. Seeking and a declared length are **not** provided for
//! surround streams (the dispatch wrapper reports `None`/`Unsupported`) — the
//! per-stream pre-roll a multichannel seek needs is left to a later revision.
//! The RFC-conformance-verified single-stream path and its embedded footprint
//! are untouched: this larger decoder is a separate type, only built for a
//! non-zero mapping family.

use crate::decoder::{DecodeError, Decoder};
use crate::demux::ogg_packets::OggPackets;
use crate::io::Source;
use crate::model::{SampleFormat, StreamInfo};

use super::celt::decoder::celt_exp2;
use super::conformance::OpusFrameDecoder;
use super::container::{MAX_CHANNELS, OpusHead};
use super::{MAX_PACKET_BYTES, OUTPUT_GAIN_SCALE, OUTPUT_SAMPLE_RATE, open_opus_headers, toc};

/// Maximum Opus streams per packet this decoder handles. Covers every standard
/// mapping-family-1 layout through 7.1 (8 channels = 5 streams: 3 coupled + 2
/// mono); a layout needing more is [`DecodeError::Unsupported`].
const MAX_STREAMS: usize = 5;

/// Maximum decoded channel count (`stream_count + coupled_count`). For a
/// standard family-1 layout this equals the output channel count (≤ 8).
const MAX_DECODED: usize = 8;

/// Maximum per-channel samples one Opus packet can yield (120 ms @ 48 kHz).
const MAX_PACKET_PCH: usize = 5760;

/// A multichannel Ogg-Opus decoder (RFC 7845 mapping family 1/255). Holds one
/// independent single-stream core per Opus stream; no allocation (every buffer
/// fixed-size and owned by value).
pub struct MultistreamOpusDecoder<S> {
    ogg: OggPackets<S>,
    head: OpusHead,
    stream_info: StreamInfo,
    /// One core per stream; `cores[i]` is stereo for `i < coupled_count`, else
    /// mono (its channel count is fixed at construction). Streams past
    /// `stream_count` are unused.
    cores: [OpusFrameDecoder; MAX_STREAMS],
    stream_count: usize,
    coupled_count: usize,
    channels: usize,
    /// Linear `OpusHead` output gain, applied per stream (as libopus does).
    output_gain: f32,
    /// Absolute per-channel decode position (48 kHz granule units, pre-skip
    /// included) — drives pre-skip discard and the Ogg end-trim via
    /// [`super::trim_frame`].
    granule_pos: u64,
    /// Absolute granule below which output is discarded (stream-start pre-skip).
    skip_to: u64,
    packet_buf: [u8; MAX_PACKET_BYTES],
    /// Decoded + channel-mapped interleaved PCM for the current packet.
    pcm_buf: [i32; MAX_PACKET_PCH * MAX_CHANNELS],
    pcm_len: usize,
    pcm_pos: usize,
}

impl<S: Source> MultistreamOpusDecoder<S> {
    /// Open a surround Ogg-Opus stream (consumes the two header packets).
    ///
    /// # Errors
    /// As [`open_opus_headers`], plus [`DecodeError::Unsupported`] if the
    /// mapping family is 0 (use [`OpusDecoder`](super::OpusDecoder)) or the
    /// layout exceeds the stream/channel caps.
    pub fn new(src: S) -> Result<Self, DecodeError> {
        let (ogg, head) = open_opus_headers(src)?;
        Self::from_parts(ogg, head)
    }

    /// Build from an already-opened Ogg reader + parsed [`OpusHead`] (the
    /// dispatch seam — see [`super::open_opus_headers`]).
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] for a family-0 header (single-stream; wrong
    /// type) or a layout beyond [`MAX_STREAMS`] / [`MAX_DECODED`] /
    /// [`MAX_CHANNELS`].
    pub(crate) fn from_parts(ogg: OggPackets<S>, head: OpusHead) -> Result<Self, DecodeError> {
        if head.mapping_family == 0 {
            return Err(DecodeError::Unsupported); // single-stream → OpusDecoder
        }
        let stream_count = usize::from(head.stream_count);
        let coupled_count = usize::from(head.coupled_count);
        let channels = usize::from(head.channels);
        // `OpusHead::parse` already checked `coupled_count <= stream_count` and
        // the mapping indices; cap the sizes this fixed-capacity decoder holds.
        if stream_count == 0
            || stream_count > MAX_STREAMS
            || stream_count + coupled_count > MAX_DECODED
            || channels > MAX_CHANNELS
        {
            return Err(DecodeError::Unsupported);
        }

        let output_gain = if head.output_gain_q8 == 0 {
            1.0
        } else {
            celt_exp2(f32::from(head.output_gain_q8) * OUTPUT_GAIN_SCALE)
        };

        let cores = core::array::from_fn(|i| {
            // Coupled streams decode stereo, the rest mono; unused streams
            // (i >= stream_count) get a harmless mono core.
            let ch = if i < coupled_count { 2 } else { 1 };
            OpusFrameDecoder::new(ch)
        });

        let stream_info = StreamInfo {
            sample_rate: OUTPUT_SAMPLE_RATE,
            channels: head.channels.into(),
            format: SampleFormat::Int { bits: 16 },
        };

        Ok(Self {
            ogg,
            head,
            stream_info,
            cores,
            stream_count,
            coupled_count,
            channels,
            output_gain,
            granule_pos: 0,
            skip_to: u64::from(head.pre_skip),
            packet_buf: [0; MAX_PACKET_BYTES],
            pcm_buf: [0; MAX_PACKET_PCH * MAX_CHANNELS],
            pcm_len: 0,
            pcm_pos: 0,
        })
    }

    /// The decoded-channel index pair a stream contributes: coupled stream `s`
    /// → `(2s, Some(2s+1))`; mono stream `s` → `(2*coupled + (s-coupled), None)`.
    fn stream_decoded_base(&self, s: usize) -> (usize, Option<usize>) {
        if s < self.coupled_count {
            (2 * s, Some(2 * s + 1))
        } else {
            (2 * self.coupled_count + (s - self.coupled_count), None)
        }
    }

    /// Decode the next Ogg packet: split it into its N streams, decode each into
    /// a planar decoded-channel buffer, apply pre-skip / end-trim once over the
    /// packet, then gather the channel-mapped interleaved output into
    /// `pcm_buf`. Leaves `pcm_len == 0` at a clean end of stream.
    fn fill_packet(&mut self) -> Result<(), DecodeError> {
        self.pcm_pos = 0;
        self.pcm_len = 0;
        let n_streams = self.stream_count;
        let channels = self.channels;

        loop {
            let n = self.ogg.next_packet(&mut self.packet_buf)?;
            if n == 0 {
                return Ok(()); // clean end of stream
            }
            let page_granule = self.ogg.last_page_granule();

            // Planar decoded channels for this packet (one row per decoded
            // channel). A big scratch — fine on the decode (big-stack) thread.
            let mut decoded = [[0i32; MAX_PACKET_PCH]; MAX_DECODED];
            let mut spc = usize::MAX; // common per-channel sample count
            let mut byte_off = 0usize; // cursor into the packet across streams
            let mut malformed = false;

            for s in 0..n_streams {
                let stream_ch = if s < self.coupled_count { 2 } else { 1 };
                let sub = &self.packet_buf[byte_off..n];
                let mut frames: [&[u8]; toc::MAX_FRAMES] = [&[][..]; toc::MAX_FRAMES];
                // Every stream but the last is self-delimited; the last uses
                // ordinary framing (its length is the packet remainder).
                let parsed = if s + 1 < n_streams {
                    toc::frames_self_delimited(sub, &mut frames)
                } else {
                    toc::frames(sub, &mut frames).map(|(cfg, cnt)| (cfg, cnt, sub.len()))
                };
                let (config, count, used) = match parsed {
                    Ok(v) => v,
                    Err(_) => {
                        malformed = true;
                        break;
                    }
                };
                byte_off += used;

                // Decode all of this stream's frames into an interleaved temp,
                // then de-interleave into the stream's decoded channel row(s).
                let frame_pch = usize::from(config.frame_size_48k);
                let mut tmp = [0i32; MAX_PACKET_PCH * 2];
                let mut written = 0usize;
                for &frame in frames.iter().take(count) {
                    let span = (frame_pch * stream_ch).min(tmp.len() - written);
                    if frame.is_empty() {
                        // DTX / lost frame: conceal as silence for its span (the
                        // core is not invoked, matching the single-stream PLC).
                        for v in &mut tmp[written..written + span] {
                            *v = 0;
                        }
                        written += span;
                        continue;
                    }
                    written += self.cores[s].decode_frame(
                        frame,
                        config,
                        self.output_gain,
                        &mut tmp[written..],
                    );
                }
                let this_spc = written / stream_ch;
                spc = spc.min(this_spc);

                let (d0, d1) = self.stream_decoded_base(s);
                for f in 0..this_spc {
                    decoded[d0][f] = tmp[f * stream_ch];
                    if let Some(d1) = d1 {
                        decoded[d1][f] = tmp[f * stream_ch + 1];
                    }
                }
            }

            if malformed || spc == usize::MAX || spc == 0 {
                continue; // skip a malformed/empty packet rather than abort
            }

            // Pre-skip + Ogg end-trim over the whole packet span (one call: the
            // kept range is contiguous — pre-skip trims the front, end-trim the
            // back — so this matches a per-frame trim).
            let (keep_s, keep_e) =
                super::trim_frame(&mut self.granule_pos, self.skip_to, page_granule, spc);
            if keep_e <= keep_s {
                continue; // wholly pre-skipped / trimmed (stream state still built)
            }

            // Gather channel-mapped interleaved output. `f` indexes the sample
            // axis of each decoded row and `c` selects the mapped row, so the
            // range loops stay clearer than an iterator rewrite.
            let mut out_len = 0usize;
            #[allow(clippy::needless_range_loop)]
            for f in keep_s..keep_e {
                for c in 0..channels {
                    let d = self.head.channel_mapping[c];
                    self.pcm_buf[out_len] = if d == 255 {
                        0 // explicitly silent channel
                    } else {
                        decoded[usize::from(d)][f]
                    };
                    out_len += 1;
                }
            }
            self.pcm_len = out_len;
            if self.pcm_len > 0 {
                return Ok(());
            }
        }
    }
}

impl<S: Source> Decoder for MultistreamOpusDecoder<S> {
    fn stream_info(&self) -> StreamInfo {
        self.stream_info
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        let mut written = 0usize;
        while written < out.len() {
            if self.pcm_pos >= self.pcm_len {
                self.fill_packet()?;
                if self.pcm_len == 0 {
                    break; // end of stream
                }
            }
            let n = (out.len() - written).min(self.pcm_len - self.pcm_pos);
            out[written..written + n]
                .copy_from_slice(&self.pcm_buf[self.pcm_pos..self.pcm_pos + n]);
            self.pcm_pos += n;
            written += n;
        }
        Ok(written)
    }
}
