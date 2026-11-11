//! Matroska / WebM (`.mka`/`.mkv`/`.webm`) demuxer — walks the EBML tree to the
//! first audio track and streams its `SimpleBlock`/`Block` frames as codec
//! [`Packet`]s. Codec decoding is delegated to the existing per-codec decoders
//! (Opus/Vorbis/FLAC/AAC), which this demuxer feeds raw frames plus the track's
//! `CodecPrivate` setup bytes — so one container unlocks every codec already
//! implemented.
//!
//! Scope: known-size elements (seekable files, as every muxer writes them);
//! the first audio track; no-lacing and all three lacing modes (Xiph / fixed /
//! EBML). A [`seek_to_cue`](MatroskaDemuxer::seek_to_cue) jumps to the Cluster
//! covering a target time via the Cues index (located through the SeekHead),
//! streaming the CuePoints on demand (no-alloc); a Cues-less file falls back to
//! [`restart`](MatroskaDemuxer::restart) + linear forward decode-discard.

use embedded_io::SeekFrom;

use super::ebml::{read_float, read_id, read_uint, read_vint};
use super::{Demuxer, Packet, read_exact, skip};
use crate::decoder::DecodeError;
use crate::io::{SeekSource, Source};
use crate::model::{SampleFormat, StreamInfo};

// EBML / Matroska element IDs (as [`read_id`] returns them — marker bit kept).
const ID_EBML_HEADER: u32 = 0x1A45_DFA3;
const ID_SEGMENT: u32 = 0x1853_8067;
const ID_INFO: u32 = 0x1549_A966;
const ID_TIMESTAMP_SCALE: u32 = 0x2A_D7_B1;
const ID_DURATION: u32 = 0x4489;
const ID_TRACKS: u32 = 0x1654_AE6B;
const ID_TRACK_ENTRY: u32 = 0xAE;
const ID_TRACK_NUMBER: u32 = 0xD7;
const ID_TRACK_TYPE: u32 = 0x83;
const ID_CODEC_ID: u32 = 0x86;
const ID_CODEC_PRIVATE: u32 = 0x63A2;
const ID_AUDIO: u32 = 0xE1;
const ID_SAMPLING_FREQUENCY: u32 = 0xB5;
const ID_CHANNELS: u32 = 0x9F;
const ID_BIT_DEPTH: u32 = 0x6264;
const ID_CLUSTER: u32 = 0x1F43_B675;
const ID_TIMESTAMP: u32 = 0xE7;
const ID_SIMPLE_BLOCK: u32 = 0xA3;
const ID_BLOCK_GROUP: u32 = 0xA0;
const ID_BLOCK: u32 = 0xA1;
// Seek index: a SeekHead near the Segment start maps element IDs to their
// Segment-data-relative byte positions; the Cues element (usually after the
// clusters) maps presentation timestamps to cluster byte positions.
const ID_SEEK_HEAD: u32 = 0x114D_9B74;
const ID_SEEK: u32 = 0x4DBB;
const ID_SEEK_ID: u32 = 0x53AB;
const ID_SEEK_POSITION: u32 = 0x53AC;
const ID_CUES: u32 = 0x1C53_BB6B;
const ID_CUE_POINT: u32 = 0xBB;
const ID_CUE_TIME: u32 = 0xB3;
const ID_CUE_TRACK_POSITIONS: u32 = 0xB7;
const ID_CUE_TRACK: u32 = 0xF7;
const ID_CUE_CLUSTER_POSITION: u32 = 0xF1;

/// Default `TimestampScale` (1 ms per tick, in nanoseconds) when Info omits it.
const DEFAULT_TIMESTAMP_SCALE: u64 = 1_000_000;

const TRACK_TYPE_AUDIO: u64 = 2;

/// Largest `CodecPrivate` we retain (Vorbis packs its 3 setup headers here —
/// a few KiB; Opus/FLAC/AAC are tens of bytes).
const MAX_CODEC_PRIVATE: usize = 16 * 1024;
/// Largest `CodecID` string we retain (e.g. `A_PCM/INT/LIT`).
const MAX_CODEC_ID: usize = 24;
/// Maximum frames in one laced block (the lace count is a `u8`, so ≤ 256).
const MAX_LACE: usize = 256;

/// Matroska / WebM container parser over a byte [`Source`].
pub struct MatroskaDemuxer<S> {
    src: S,
    info: StreamInfo,
    codec_id: [u8; MAX_CODEC_ID],
    codec_id_len: usize,
    codec_private: [u8; MAX_CODEC_PRIVATE],
    codec_private_len: usize,
    /// The audio track number we stream (blocks for other tracks are skipped).
    track_number: u64,
    /// Absolute byte offset where the Segment's data begins (the first byte after
    /// the Segment element header) — the base for every `SeekPosition` and
    /// `CueClusterPosition`, which are stored Segment-data-relative.
    segment_data_start: u64,
    /// Absolute byte offset where the Segment's data ends (`u64::MAX` if the
    /// Segment had unknown size — then we read to EOF).
    segment_end: u64,
    /// Absolute byte offset of the Cues element (its ID byte), resolved from the
    /// SeekHead during init, or `None` when the file carries no Cues index. The
    /// cue list itself is streamed on demand ([`Self::seek_to_cue`]) — never
    /// stored, keeping the demuxer no-alloc.
    cues_offset: Option<u64>,
    /// `TimestampScale` (nanoseconds per tick) from Info — the factor turning the
    /// tick-valued `CueTime`/Cluster `Timestamp` into a time (and thence samples).
    timestamp_scale: u64,
    /// Absolute byte offset where the current Cluster's data ends, if inside one.
    cluster_end: Option<u64>,
    /// Absolute byte offset of the first Cluster's header — the rewind point for
    /// [`Self::restart`] (seeking is a restart + forward decode-discard).
    first_cluster_offset: u64,
    /// Absolute read position (every byte consumed is accounted here).
    pos: u64,
    /// Pending laced frames in the current block: their byte sizes and how many
    /// are left to emit. Frames are read straight from `src` into the caller
    /// scratch one per `next_packet`, so no whole-block buffering is needed.
    lace_sizes: [u32; MAX_LACE],
    lace_count: usize,
    lace_idx: usize,
}

/// One element header: ID, data size (`None` = unknown), and header byte length.
struct ElemHeader {
    id: u32,
    size: Option<u64>,
    header_len: u64,
}

/// Read an element header (ID vint + size vint) and return it with the total
/// header byte length.
fn read_elem<S: Source>(src: &mut S) -> Result<ElemHeader, DecodeError> {
    let (id, id_len) = read_id(src)?;
    let (size, size_len) = read_vint(src)?;
    Ok(ElemHeader {
        id,
        size,
        header_len: u64::from(id_len) + u64::from(size_len),
    })
}

impl<S: Source> MatroskaDemuxer<S> {
    /// Parse the EBML header and the Segment's Info + Tracks, stopping at the
    /// first Cluster (or the first audio track's data).
    ///
    /// # Errors
    /// [`DecodeError::Malformed`] for a non-EBML / trackless file,
    /// [`DecodeError::Unsupported`] if no audio track is found, plus the usual
    /// I/O errors.
    pub fn new(mut src: S) -> Result<Self, DecodeError> {
        let mut pos: u64 = 0;
        // Top-level EBML header.
        let hdr = read_elem(&mut src)?;
        if hdr.id != ID_EBML_HEADER {
            return Err(DecodeError::Malformed);
        }
        pos += hdr.header_len;
        let hsize = hdr.size.ok_or(DecodeError::Malformed)?;
        skip(&mut src, hsize)?; // DocType etc. — we accept matroska and webm alike
        pos += hsize;

        // Segment.
        let seg = read_elem(&mut src)?;
        if seg.id != ID_SEGMENT {
            return Err(DecodeError::Malformed);
        }
        pos += seg.header_len;
        let segment_data_start = pos;
        let segment_end = seg.size.map_or(u64::MAX, |s| pos.saturating_add(s));

        let mut me = Self {
            src,
            info: StreamInfo {
                sample_rate: 0,
                channels: 0,
                format: SampleFormat::Int { bits: 16 },
            },
            codec_id: [0; MAX_CODEC_ID],
            codec_id_len: 0,
            codec_private: [0; MAX_CODEC_PRIVATE],
            codec_private_len: 0,
            track_number: 0,
            segment_data_start,
            segment_end,
            cues_offset: None,
            timestamp_scale: DEFAULT_TIMESTAMP_SCALE,
            cluster_end: None,
            first_cluster_offset: 0,
            pos,
            lace_sizes: [0; MAX_LACE],
            lace_count: 0,
            lace_idx: 0,
        };

        // Walk Segment children until the first Cluster is reached (its header is
        // consumed and `cluster_end` armed) or the Segment ends.
        loop {
            if me.pos >= me.segment_end {
                return Err(DecodeError::Unsupported); // no cluster / no audio
            }
            let el = read_elem(&mut me.src)?;
            me.pos += el.header_len;
            match el.id {
                ID_INFO => me.parse_info(el.size.ok_or(DecodeError::Malformed)?)?,
                ID_TRACKS => me.parse_tracks(el.size.ok_or(DecodeError::Malformed)?)?,
                ID_SEEK_HEAD => me.parse_seek_head(el.size.ok_or(DecodeError::Malformed)?)?,
                ID_CUES => {
                    // Cues ahead of the clusters (rare) — record its start and
                    // skip; the SeekHead is the usual locator (Cues trails them).
                    me.cues_offset.get_or_insert(me.pos - el.header_len);
                    let s = el.size.ok_or(DecodeError::Malformed)?;
                    skip(&mut me.src, s)?;
                    me.pos += s;
                }
                ID_CLUSTER => {
                    if me.track_number == 0 {
                        return Err(DecodeError::Unsupported); // reached audio data with no track
                    }
                    me.first_cluster_offset = me.pos - el.header_len;
                    me.cluster_end = el.size.map(|s| me.pos.saturating_add(s));
                    return Ok(me);
                }
                _ => {
                    let s = el.size.ok_or(DecodeError::Malformed)?;
                    skip(&mut me.src, s)?;
                    me.pos += s;
                }
            }
        }
    }

    /// `CodecID` string, e.g. `b"A_OPUS"`, `b"A_VORBIS"`, `b"A_FLAC"`, `b"A_AAC"`.
    #[must_use]
    pub fn codec_id(&self) -> &[u8] {
        &self.codec_id[..self.codec_id_len]
    }

    /// `CodecPrivate` setup bytes (OpusHead / Vorbis setup headers / FLAC
    /// STREAMINFO block / AAC AudioSpecificConfig), empty if none.
    #[must_use]
    pub fn codec_private(&self) -> &[u8] {
        &self.codec_private[..self.codec_private_len]
    }

    /// `TimestampScale` (nanoseconds per tick) — turns the tick-valued `CueTime` a
    /// [`Self::seek_to_cue`] returns into a time (and thence a sample count).
    #[must_use]
    pub fn timestamp_scale(&self) -> u64 {
        self.timestamp_scale
    }

    /// Parse the Info element (TimestampScale is read but duration isn't needed
    /// for decode, so only sampling defaults come from Tracks/Audio).
    fn parse_info(&mut self, size: u64) -> Result<(), DecodeError> {
        let end = self.pos.saturating_add(size);
        while self.pos < end {
            let el = read_elem(&mut self.src)?;
            self.pos += el.header_len;
            let s = el.size.ok_or(DecodeError::Malformed)?;
            match el.id {
                ID_TIMESTAMP_SCALE => {
                    let scale = read_uint(&mut self.src, s)?;
                    if scale != 0 {
                        self.timestamp_scale = scale;
                    }
                    self.pos += s;
                }
                ID_DURATION => {
                    let _ = read_float(&mut self.src, s)?;
                    self.pos += s;
                }
                _ => {
                    skip(&mut self.src, s)?;
                    self.pos += s;
                }
            }
        }
        Ok(())
    }

    /// Parse a SeekHead, resolving the Cues element's absolute offset from its
    /// Seek entry (`SeekID == Cues`, `SeekPosition` is Segment-data-relative) so a
    /// later [`Self::seek_to_cue`] can find the index without scanning to EOF.
    fn parse_seek_head(&mut self, size: u64) -> Result<(), DecodeError> {
        let end = self.pos.saturating_add(size);
        while self.pos < end {
            let el = read_elem(&mut self.src)?;
            self.pos += el.header_len;
            let s = el.size.ok_or(DecodeError::Malformed)?;
            if el.id == ID_SEEK {
                self.parse_seek_entry(s)?;
            } else {
                skip(&mut self.src, s)?;
                self.pos += s;
            }
        }
        Ok(())
    }

    /// Parse one Seek entry; if it points at the Cues element, record its
    /// absolute offset (unless one was already found).
    fn parse_seek_entry(&mut self, size: u64) -> Result<(), DecodeError> {
        let end = self.pos.saturating_add(size);
        let mut seek_id = 0u64;
        let mut seek_pos: Option<u64> = None;
        while self.pos < end {
            let el = read_elem(&mut self.src)?;
            self.pos += el.header_len;
            let s = el.size.ok_or(DecodeError::Malformed)?;
            match el.id {
                ID_SEEK_ID => {
                    seek_id = read_uint(&mut self.src, s)?;
                    self.pos += s;
                }
                ID_SEEK_POSITION => {
                    seek_pos = Some(read_uint(&mut self.src, s)?);
                    self.pos += s;
                }
                _ => {
                    skip(&mut self.src, s)?;
                    self.pos += s;
                }
            }
        }
        if seek_id == u64::from(ID_CUES) {
            if let Some(p) = seek_pos {
                self.cues_offset
                    .get_or_insert(self.segment_data_start.saturating_add(p));
            }
        }
        Ok(())
    }

    /// Parse Tracks, keeping the first audio TrackEntry's parameters.
    fn parse_tracks(&mut self, size: u64) -> Result<(), DecodeError> {
        let end = self.pos.saturating_add(size);
        while self.pos < end {
            let el = read_elem(&mut self.src)?;
            self.pos += el.header_len;
            let s = el.size.ok_or(DecodeError::Malformed)?;
            if el.id == ID_TRACK_ENTRY {
                self.parse_track_entry(s)?;
                // Keep walking only if we haven't locked onto an audio track yet.
                if self.track_number != 0 {
                    // Skip the rest of Tracks. Saturate: a nested TrackEntry
                    // whose declared size overran `end` can leave `self.pos`
                    // past it, so a plain subtraction would underflow (a
                    // malformed stream must never panic).
                    let remaining = end.saturating_sub(self.pos);
                    skip(&mut self.src, remaining)?;
                    self.pos = end;
                    return Ok(());
                }
            } else {
                skip(&mut self.src, s)?;
                self.pos += s;
            }
        }
        Ok(())
    }

    /// Parse one TrackEntry. If it is the audio track, record its number, codec,
    /// codec-private, and audio parameters; otherwise leave `track_number` at 0.
    fn parse_track_entry(&mut self, size: u64) -> Result<(), DecodeError> {
        let end = self.pos.saturating_add(size);
        let mut number = 0u64;
        let mut is_audio = false;
        let mut rate = 0u32;
        let mut channels = 0u16;
        let mut bits = 16u16;
        while self.pos < end {
            let el = read_elem(&mut self.src)?;
            self.pos += el.header_len;
            let s = el.size.ok_or(DecodeError::Malformed)?;
            match el.id {
                ID_TRACK_NUMBER => {
                    number = read_uint(&mut self.src, s)?;
                    self.pos += s;
                }
                ID_TRACK_TYPE => {
                    is_audio = read_uint(&mut self.src, s)? == TRACK_TYPE_AUDIO;
                    self.pos += s;
                }
                ID_CODEC_ID => {
                    let take = (s as usize).min(MAX_CODEC_ID);
                    read_exact(&mut self.src, &mut self.codec_id[..take])?;
                    self.codec_id_len = take;
                    skip(&mut self.src, s - take as u64)?;
                    self.pos += s;
                }
                ID_CODEC_PRIVATE => {
                    if s as usize > MAX_CODEC_PRIVATE {
                        return Err(DecodeError::Unsupported);
                    }
                    let take = s as usize;
                    read_exact(&mut self.src, &mut self.codec_private[..take])?;
                    self.codec_private_len = take;
                    self.pos += s;
                }
                ID_AUDIO => {
                    self.parse_audio(s, &mut rate, &mut channels, &mut bits)?;
                }
                _ => {
                    skip(&mut self.src, s)?;
                    self.pos += s;
                }
            }
        }
        if is_audio && number != 0 {
            self.track_number = number;
            self.info = StreamInfo {
                sample_rate: rate,
                channels,
                format: SampleFormat::Int { bits: bits as u8 },
            };
        } else {
            // Not the audio track — discard any codec bytes we tentatively kept.
            self.codec_id_len = 0;
            self.codec_private_len = 0;
        }
        Ok(())
    }

    /// Parse an Audio element into the sampling parameters.
    fn parse_audio(
        &mut self,
        size: u64,
        rate: &mut u32,
        channels: &mut u16,
        bits: &mut u16,
    ) -> Result<(), DecodeError> {
        let end = self.pos.saturating_add(size);
        while self.pos < end {
            let el = read_elem(&mut self.src)?;
            self.pos += el.header_len;
            let s = el.size.ok_or(DecodeError::Malformed)?;
            match el.id {
                ID_SAMPLING_FREQUENCY => {
                    *rate = read_float(&mut self.src, s)? as u32;
                    self.pos += s;
                }
                ID_CHANNELS => {
                    *channels = read_uint(&mut self.src, s)? as u16;
                    self.pos += s;
                }
                ID_BIT_DEPTH => {
                    *bits = read_uint(&mut self.src, s)? as u16;
                    self.pos += s;
                }
                _ => {
                    skip(&mut self.src, s)?;
                    self.pos += s;
                }
            }
        }
        Ok(())
    }

    /// Advance to the next block belonging to our audio track, consuming its
    /// header and arming `lace_sizes`/`lace_count` for [`Self::emit_lace_frame`].
    /// Returns the size of the block's **first** frame, or `Ok(None)` at stream
    /// end. The source is left positioned at the first frame's bytes.
    fn next_block_first_frame(&mut self) -> Result<Option<u32>, DecodeError> {
        loop {
            // Need a live cluster.
            if self.cluster_end.is_none() {
                if self.pos >= self.segment_end {
                    return Ok(None);
                }
                let el = read_elem(&mut self.src)?;
                self.pos += el.header_len;
                if el.id == ID_CLUSTER {
                    self.cluster_end = el.size.map(|s| self.pos.saturating_add(s));
                    continue;
                }
                // Non-cluster top-level element (Cues, Tags, SeekHead, …) — skip.
                let s = el.size.ok_or(DecodeError::Malformed)?;
                skip(&mut self.src, s)?;
                self.pos += s;
                continue;
            }
            let cend = self.cluster_end.unwrap();
            if self.pos >= cend {
                self.cluster_end = None;
                continue;
            }
            let el = read_elem(&mut self.src)?;
            self.pos += el.header_len;
            let s = el.size.ok_or(DecodeError::Malformed)?;
            match el.id {
                ID_TIMESTAMP => {
                    let _ = read_uint(&mut self.src, s)?;
                    self.pos += s;
                }
                ID_SIMPLE_BLOCK => {
                    if let Some(first) = self.parse_block_header(s)? {
                        return Ok(Some(first));
                    }
                }
                ID_BLOCK_GROUP => {
                    // Walk the group for its Block.
                    let gend = self.pos.saturating_add(s);
                    while self.pos < gend {
                        let inner = read_elem(&mut self.src)?;
                        self.pos += inner.header_len;
                        let isz = inner.size.ok_or(DecodeError::Malformed)?;
                        if inner.id == ID_BLOCK {
                            if let Some(first) = self.parse_block_header(isz)? {
                                // Skip any remaining group members after the block
                                // is consumed by the lace emitter; record nothing.
                                return Ok(Some(first));
                            }
                        } else {
                            skip(&mut self.src, isz)?;
                            self.pos += isz;
                        }
                    }
                }
                _ => {
                    skip(&mut self.src, s)?;
                    self.pos += s;
                }
            }
        }
    }

    /// Parse a (Simple)Block header of `block_size` bytes: track vint, int16
    /// timestamp, flags, and the lacing table. If the block is for another
    /// track, it is skipped and `Ok(None)` is returned. Otherwise the source is
    /// left at the first frame and `lace_sizes`/`lace_count` are armed; the first
    /// frame's size is returned.
    fn parse_block_header(&mut self, block_size: u64) -> Result<Option<u32>, DecodeError> {
        let block_start = self.pos;
        let (track, track_len) = read_vint(&mut self.src)?;
        self.pos += u64::from(track_len);
        let track = track.ok_or(DecodeError::Malformed)?;
        // int16 relative timestamp + flags byte.
        let mut tf = [0u8; 3];
        read_exact(&mut self.src, &mut tf)?;
        self.pos += 3;
        let flags = tf[2];
        let header_consumed = self.pos - block_start;
        // A block whose declared size is smaller than its own mandatory header
        // (track vint + 3 bytes) is malformed — reject rather than underflow.
        let payload = block_size
            .checked_sub(header_consumed)
            .ok_or(DecodeError::Malformed)?;

        if track != self.track_number {
            skip(&mut self.src, payload)?;
            self.pos += payload;
            return Ok(None);
        }

        let lacing = (flags >> 1) & 0x03;
        if lacing == 0 {
            // No lacing: the whole payload is one frame.
            self.lace_sizes[0] = payload as u32;
            self.lace_count = 1;
            self.lace_idx = 0;
            return Ok(Some(self.lace_sizes[0]));
        }

        // Laced: a count byte, then (count-1) sizes encode the frame boundaries;
        // the last frame's size is the remainder.
        let mut cnt = [0u8; 1];
        read_exact(&mut self.src, &mut cnt)?;
        self.pos += 1;
        let frames = usize::from(cnt[0]) + 1;
        if frames > MAX_LACE {
            return Err(DecodeError::Unsupported);
        }
        let mut consumed_sizes = 0u64; // bytes of the lace-size table
        let mut sum = 0u64; // bytes accounted by the first (frames-1) frames
        match lacing {
            0b10 => {
                // Fixed lacing: all frames equal; payload split evenly.
                let body = payload.checked_sub(1).ok_or(DecodeError::Malformed)?;
                if body % frames as u64 != 0 {
                    return Err(DecodeError::Malformed);
                }
                let each = (body / frames as u64) as u32;
                for i in 0..frames {
                    self.lace_sizes[i] = each;
                }
            }
            0b01 => {
                // Xiph lacing: each size is a run of 0xFF bytes + a final byte.
                for slot in self.lace_sizes.iter_mut().take(frames - 1) {
                    let mut size = 0u32;
                    loop {
                        let mut b = [0u8; 1];
                        read_exact(&mut self.src, &mut b)?;
                        consumed_sizes += 1;
                        size += u32::from(b[0]);
                        if b[0] != 0xFF {
                            break;
                        }
                    }
                    *slot = size;
                    sum += u64::from(size);
                }
                self.lace_sizes[frames - 1] = payload
                    .checked_sub(1)
                    .and_then(|x| x.checked_sub(consumed_sizes))
                    .and_then(|x| x.checked_sub(sum))
                    .ok_or(DecodeError::Malformed)?
                    as u32;
            }
            _ => {
                // EBML lacing: first size is an unsigned vint; the rest are signed
                // deltas (range-shifted vints).
                let (first, l) = read_vint(&mut self.src)?;
                consumed_sizes += u64::from(l);
                let mut cur = first.ok_or(DecodeError::Malformed)? as i64;
                self.lace_sizes[0] = cur as u32;
                sum = sum.wrapping_add(cur as u64);
                for i in 1..frames - 1 {
                    let (raw, l) = read_vint(&mut self.src)?;
                    consumed_sizes += u64::from(l);
                    let raw = raw.ok_or(DecodeError::Malformed)?;
                    // Signed vint bias: subtract (2^(7*l-1) - 1).
                    let bias = (1i64 << (7 * i64::from(l) - 1)) - 1;
                    cur = cur.wrapping_add(raw as i64 - bias);
                    self.lace_sizes[i] = cur as u32;
                    sum = sum.wrapping_add(cur as u64);
                }
                self.lace_sizes[frames - 1] = payload
                    .checked_sub(1)
                    .and_then(|x| x.checked_sub(consumed_sizes))
                    .and_then(|x| x.checked_sub(sum))
                    .ok_or(DecodeError::Malformed)?
                    as u32;
            }
        }
        self.pos += consumed_sizes;
        self.lace_count = frames;
        self.lace_idx = 0;
        Ok(Some(self.lace_sizes[0]))
    }

    /// Read the current lace frame into `scratch`, advance the lace index, and
    /// return the frame length. Caller guarantees `lace_idx < lace_count`.
    fn emit_lace_frame(&mut self, scratch: &mut [u8]) -> Result<usize, DecodeError> {
        let n = self.lace_sizes[self.lace_idx] as usize;
        if n > scratch.len() {
            return Err(DecodeError::Unsupported); // frame larger than caller scratch
        }
        read_exact(&mut self.src, &mut scratch[..n])?;
        self.pos += n as u64;
        self.lace_idx += 1;
        Ok(n)
    }
}

impl<S: SeekSource> MatroskaDemuxer<S> {
    /// Rewind to the first Cluster (the seek rewind point) and reset the
    /// cluster-walk + lacing state, so the next [`Self::next_packet`] re-reads
    /// from the start of the audio. The parsed track config is retained.
    ///
    /// # Errors
    /// [`DecodeError::Io`] if the underlying seek fails.
    pub(crate) fn restart(&mut self) -> Result<(), DecodeError> {
        self.src
            .seek(SeekFrom::Start(self.first_cluster_offset))
            .map_err(|_| DecodeError::Io)?;
        self.pos = self.first_cluster_offset;
        self.cluster_end = None;
        self.lace_count = 0;
        self.lace_idx = 0;
        Ok(())
    }

    /// Cues-index seek: position the demuxer at the start of the Cluster whose
    /// `CueTime` is the largest `≤ target_ticks`, so the decoder only forward-
    /// decode-discards within that Cluster rather than from the stream start
    /// (O(1) source touches on the index + one cluster, not O(target)).
    ///
    /// Returns `Ok(Some(cluster_ticks))` — the chosen Cue's presentation time, the
    /// base the decoder converts to its landed sample — on a successful jump, or
    /// `Ok(None)` when the file has no Cues index (or no Cue at/under the target),
    /// leaving the position unchanged so the caller can fall back to
    /// [`Self::restart`] + linear decode-discard.
    ///
    /// The CuePoints are streamed and compared on the fly — only the best `(time,
    /// cluster position)` pair is kept — so no cue list is ever allocated. The
    /// scan stops at the first `CueTime > target_ticks` (Cues are time-ordered).
    ///
    /// # Errors
    /// [`DecodeError::Io`] on a source-seek failure; the EBML read errors of a
    /// malformed Cues element.
    pub(crate) fn seek_to_cue(&mut self, target_ticks: u64) -> Result<Option<u64>, DecodeError> {
        let cues_offset = match self.cues_offset {
            Some(o) => o,
            None => return Ok(None),
        };
        self.src
            .seek(SeekFrom::Start(cues_offset))
            .map_err(|_| DecodeError::Io)?;
        self.pos = cues_offset;
        let cues = read_elem(&mut self.src)?;
        self.pos += cues.header_len;
        if cues.id != ID_CUES {
            return Ok(None); // SeekHead lied / corrupt index — caller falls back.
        }
        let cues_end = cues
            .size
            .map_or(self.segment_end, |s| self.pos.saturating_add(s));

        let mut best: Option<(u64, u64)> = None; // (cue_time, cluster position)
        while self.pos < cues_end {
            let cp = read_elem(&mut self.src)?;
            self.pos += cp.header_len;
            let cp_size = cp.size.ok_or(DecodeError::Malformed)?;
            if cp.id != ID_CUE_POINT {
                skip(&mut self.src, cp_size)?;
                self.pos += cp_size;
                continue;
            }
            let cp_end = self.pos.saturating_add(cp_size);
            let mut cue_time: Option<u64> = None;
            let mut cluster_pos: Option<u64> = None;
            while self.pos < cp_end {
                let el = read_elem(&mut self.src)?;
                self.pos += el.header_len;
                let s = el.size.ok_or(DecodeError::Malformed)?;
                match el.id {
                    ID_CUE_TIME => {
                        cue_time = Some(read_uint(&mut self.src, s)?);
                        self.pos += s;
                    }
                    ID_CUE_TRACK_POSITIONS => {
                        let ctp_end = self.pos.saturating_add(s);
                        let mut track: Option<u64> = None;
                        let mut pos: Option<u64> = None;
                        while self.pos < ctp_end {
                            let f = read_elem(&mut self.src)?;
                            self.pos += f.header_len;
                            let fs = f.size.ok_or(DecodeError::Malformed)?;
                            match f.id {
                                ID_CUE_TRACK => {
                                    track = Some(read_uint(&mut self.src, fs)?);
                                    self.pos += fs;
                                }
                                ID_CUE_CLUSTER_POSITION => {
                                    pos = Some(read_uint(&mut self.src, fs)?);
                                    self.pos += fs;
                                }
                                _ => {
                                    skip(&mut self.src, fs)?;
                                    self.pos += fs;
                                }
                            }
                        }
                        // Prefer our audio track's position; a cue without a
                        // CueTrack, or the first seen in a single-track file, is
                        // taken as a fallback.
                        if let Some(p) = pos {
                            let matches = track.is_none_or(|t| t == self.track_number);
                            if cluster_pos.is_none() || matches {
                                cluster_pos = Some(p);
                            }
                        }
                    }
                    _ => {
                        skip(&mut self.src, s)?;
                        self.pos += s;
                    }
                }
            }
            if let (Some(t), Some(p)) = (cue_time, cluster_pos) {
                if t <= target_ticks {
                    if best.is_none_or(|(bt, _)| t >= bt) {
                        best = Some((t, p));
                    }
                } else {
                    break; // Cues are time-ordered — nothing later can qualify.
                }
            }
        }

        let (cue_time, cluster_pos) = match best {
            Some(v) => v,
            None => return Ok(None), // target precedes the first cue — fall back.
        };
        let cluster_abs = self.segment_data_start.saturating_add(cluster_pos);
        self.src
            .seek(SeekFrom::Start(cluster_abs))
            .map_err(|_| DecodeError::Io)?;
        self.pos = cluster_abs;
        // Re-arm the cluster walk exactly as `restart` does: `cluster_end = None`
        // makes `next_block_first_frame` re-read this Cluster's header (+ its own
        // Timestamp), and the lacing cursor is cleared.
        self.cluster_end = None;
        self.lace_count = 0;
        self.lace_idx = 0;
        Ok(Some(cue_time))
    }
}

impl<S: Source> Demuxer for MatroskaDemuxer<S> {
    fn stream_info(&self) -> StreamInfo {
        self.info
    }

    fn next_packet<'a>(
        &mut self,
        scratch: &'a mut [u8],
    ) -> Result<Option<Packet<'a>>, DecodeError> {
        // Finish any pending laced frames from the current block first.
        if self.lace_idx >= self.lace_count {
            match self.next_block_first_frame()? {
                Some(_) => {}
                None => return Ok(None),
            }
        }
        let n = self.emit_lace_frame(scratch)?;
        Ok(Some(Packet {
            ts: 0,
            dur: 0,
            data: &scratch[..n],
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceSource;

    /// Encode an EBML element ID (already marker-kept) as its big-endian bytes.
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

    /// Encode `n` as a 1-byte EBML size vint (marker 0x80 | n), n < 128.
    fn size1(n: usize) -> u8 {
        0x80 | n as u8
    }

    /// Build an element: id + 1-byte size + body.
    fn elem(id: u32, body: &[u8]) -> Vec<u8> {
        let mut v = id_bytes(id);
        v.push(size1(body.len()));
        v.extend_from_slice(body);
        v
    }

    /// A minimal Matroska: EBML header, Segment{ Tracks{ TrackEntry(audio OPUS) },
    /// Cluster{ Timestamp, SimpleBlock(track1, no lacing, 4-byte frame) } }.
    fn minimal_mkv(frame: &[u8]) -> Vec<u8> {
        // Audio sub-element: 48000 Hz, 2ch.
        let audio = {
            let mut a = Vec::new();
            a.extend_from_slice(&elem(ID_SAMPLING_FREQUENCY, &48_000.0f32.to_be_bytes()));
            a.extend_from_slice(&elem(ID_CHANNELS, &[2]));
            a
        };
        let track_entry = {
            let mut t = Vec::new();
            t.extend_from_slice(&elem(ID_TRACK_NUMBER, &[1]));
            t.extend_from_slice(&elem(ID_TRACK_TYPE, &[TRACK_TYPE_AUDIO as u8]));
            t.extend_from_slice(&elem(ID_CODEC_ID, b"A_OPUS"));
            t.extend_from_slice(&elem(ID_CODEC_PRIVATE, &[0xAA, 0xBB]));
            t.extend_from_slice(&elem(ID_AUDIO, &audio));
            t
        };
        let tracks = elem(ID_TRACK_ENTRY, &track_entry);
        let tracks = elem(ID_TRACKS, &tracks);

        // SimpleBlock: track vint (0x81), int16 ts (0x0000), flags (0x00), frame.
        let mut sb = vec![0x81, 0x00, 0x00, 0x00];
        sb.extend_from_slice(frame);
        let simple_block = elem(ID_SIMPLE_BLOCK, &sb);
        let mut cluster_body = Vec::new();
        cluster_body.extend_from_slice(&elem(ID_TIMESTAMP, &[0]));
        cluster_body.extend_from_slice(&simple_block);
        let cluster = elem(ID_CLUSTER, &cluster_body);

        let mut segment_body = Vec::new();
        segment_body.extend_from_slice(&tracks);
        segment_body.extend_from_slice(&cluster);
        let segment = elem(ID_SEGMENT, &segment_body);

        let mut v = Vec::new();
        v.extend_from_slice(&elem(ID_EBML_HEADER, b"\x42\x82\x84webm")); // DocType webm
        v.extend_from_slice(&segment);
        v
    }

    /// Wrap a raw SimpleBlock body (track vint + int16 ts + flags + lacing +
    /// frames) into a minimal Matroska, for the lacing tests.
    fn mkv_with_sb(sb_body: &[u8]) -> Vec<u8> {
        let audio = {
            let mut a = Vec::new();
            a.extend_from_slice(&elem(ID_SAMPLING_FREQUENCY, &48_000.0f32.to_be_bytes()));
            a.extend_from_slice(&elem(ID_CHANNELS, &[2]));
            a
        };
        let mut te = Vec::new();
        te.extend_from_slice(&elem(ID_TRACK_NUMBER, &[1]));
        te.extend_from_slice(&elem(ID_TRACK_TYPE, &[TRACK_TYPE_AUDIO as u8]));
        te.extend_from_slice(&elem(ID_CODEC_ID, b"A_OPUS"));
        te.extend_from_slice(&elem(ID_AUDIO, &audio));
        let tracks = elem(ID_TRACKS, &elem(ID_TRACK_ENTRY, &te));
        let mut cluster_body = elem(ID_TIMESTAMP, &[0]);
        cluster_body.extend_from_slice(&elem(ID_SIMPLE_BLOCK, sb_body));
        let mut segment = tracks;
        segment.extend_from_slice(&elem(ID_CLUSTER, &cluster_body));
        let mut v = elem(ID_EBML_HEADER, b"\x42\x82\x84webm");
        v.extend_from_slice(&elem(ID_SEGMENT, &segment));
        v
    }

    #[test]
    fn undersized_tracks_size_does_not_underflow() {
        // Regression (tests/fuzz.rs: a byte-substitution of an mkv-aac fixture).
        // A Tracks element whose declared size is SMALLER than the TrackEntry it
        // actually carries makes `parse_track_entry` advance `self.pos` past the
        // Tracks `end`. The subsequent "skip the rest of Tracks" then computed
        // `end - self.pos`, which underflowed (a debug-build panic). It must
        // saturate instead — malformed input must never panic.
        let audio = {
            let mut a = Vec::new();
            a.extend_from_slice(&elem(ID_SAMPLING_FREQUENCY, &48_000.0f32.to_be_bytes()));
            a.extend_from_slice(&elem(ID_CHANNELS, &[2]));
            a
        };
        let mut te = Vec::new();
        te.extend_from_slice(&elem(ID_TRACK_NUMBER, &[1]));
        te.extend_from_slice(&elem(ID_TRACK_TYPE, &[TRACK_TYPE_AUDIO as u8]));
        te.extend_from_slice(&elem(ID_CODEC_ID, b"A_OPUS"));
        te.extend_from_slice(&elem(ID_AUDIO, &audio));
        let track_entry = elem(ID_TRACK_ENTRY, &te);
        // Tracks header with a deliberately-too-small declared size (2 bytes),
        // though it is immediately followed by the whole (longer) TrackEntry.
        let mut tracks = id_bytes(ID_TRACKS);
        tracks.push(size1(2));
        tracks.extend_from_slice(&track_entry);

        let mut segment = tracks;
        segment.extend_from_slice(&elem(ID_CLUSTER, &elem(ID_TIMESTAMP, &[0])));
        let mut bytes = elem(ID_EBML_HEADER, b"\x42\x82\x84webm");
        bytes.extend_from_slice(&elem(ID_SEGMENT, &segment));

        // Must return gracefully (Ok or Err), never panic.
        let _ = MatroskaDemuxer::new(SliceSource::new(&bytes));
    }

    /// Drain every frame the demuxer yields, as owned byte vecs.
    fn drain_frames(bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut d = MatroskaDemuxer::new(SliceSource::new(bytes)).unwrap();
        let mut scratch = [0u8; 256];
        let mut out = Vec::new();
        while let Some(p) = d.next_packet(&mut scratch).unwrap() {
            out.push(p.data.to_vec());
        }
        out
    }

    #[test]
    fn parses_track_and_yields_frame() {
        let frame = [0x11, 0x22, 0x33, 0x44];
        let bytes = minimal_mkv(&frame);
        let mut d = MatroskaDemuxer::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(d.codec_id(), b"A_OPUS");
        assert_eq!(d.codec_private(), &[0xAA, 0xBB]);
        assert_eq!(d.stream_info().sample_rate, 48_000);
        assert_eq!(d.stream_info().channels, 2);
        let mut scratch = [0u8; 64];
        let pkt = d.next_packet(&mut scratch).unwrap().unwrap();
        assert_eq!(pkt.data, &frame);
        assert!(d.next_packet(&mut scratch).unwrap().is_none());
    }

    /// Xiph lacing (flags bit pattern `01`): a count byte, then `(count-1)`
    /// Xiph-coded sizes (255-runs + final byte), then the concatenated frames;
    /// the last frame's size is the remainder.
    #[test]
    fn xiph_lacing_splits_frames() {
        let (a, b, c) = ([0x11; 3], [0x22; 5], [0x33; 4]);
        let mut sb = vec![0x81, 0x00, 0x00, 0x02]; // track 1, ts 0, flags = Xiph
        sb.push(2); // frame count − 1
        sb.extend_from_slice(&[3, 5]); // Xiph sizes of frames 0 and 1
        sb.extend_from_slice(&a);
        sb.extend_from_slice(&b);
        sb.extend_from_slice(&c);
        let frames = drain_frames(&mkv_with_sb(&sb));
        assert_eq!(frames, vec![a.to_vec(), b.to_vec(), c.to_vec()]);
    }

    /// Fixed lacing (`10`): a count byte, then equal-size frames (no size table).
    #[test]
    fn fixed_lacing_splits_frames() {
        let data: Vec<u8> = (0..12).collect(); // 3 frames × 4 bytes
        let mut sb = vec![0x81, 0x00, 0x00, 0x04]; // flags = fixed
        sb.push(2); // count − 1
        sb.extend_from_slice(&data);
        let frames = drain_frames(&mkv_with_sb(&sb));
        assert_eq!(
            frames,
            vec![vec![0, 1, 2, 3], vec![4, 5, 6, 7], vec![8, 9, 10, 11]]
        );
    }

    /// EBML lacing (`11`): a count byte, the first size as an unsigned vint, then
    /// signed-vint deltas; the last frame's size is the remainder.
    #[test]
    fn ebml_lacing_splits_frames() {
        let (a, b, c) = ([0x41; 4], [0x42; 6], [0x43; 5]);
        let mut sb = vec![0x81, 0x00, 0x00, 0x06]; // flags = EBML
        sb.push(2); // count − 1
        sb.push(0x84); // first size = 4 (1-byte vint)
        sb.push(0xC1); // delta +2 (signed vint: 2 + bias 63 = 65 = 0x41 | 0x80)
        sb.extend_from_slice(&a);
        sb.extend_from_slice(&b);
        sb.extend_from_slice(&c);
        let frames = drain_frames(&mkv_with_sb(&sb));
        assert_eq!(frames, vec![a.to_vec(), b.to_vec(), c.to_vec()]);
    }

    #[test]
    fn rejects_non_ebml() {
        let bytes = b"RIFF\0\0\0\0WAVE";
        assert!(MatroskaDemuxer::new(SliceSource::new(bytes)).is_err());
    }
}
