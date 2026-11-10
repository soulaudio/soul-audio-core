//! ADTS (`.aac`) frame reader — parses the 7/9-byte ADTS header (raw AAC
//! elementary-stream framing, as distinct from the MP4 `esds`/`mp4a`
//! container `[super::mp4]` handles) and iterates `raw_data_block` payloads
//! one AAC frame at a time. AAC-LC codec decode (Huffman/IMDCT/etc.) is a
//! later slice; this module only demuxes frame boundaries and exposes the
//! stream's [`AacConfig`].

use embedded_io::SeekFrom;

use super::{SAMPLE_RATES, channels_for, read_exact, skip};
use crate::decoder::DecodeError;
use crate::io::{SeekSource, Source};

/// Frames sampled per region when resolving the CBR length / detecting VBR. A
/// stream of at most this many frames is walked in full (exact count); a longer
/// one is byte-estimated from a start- and end-region mean.
const SAMPLE_FRAMES: u32 = 32;

/// Resync scan window: two maximal ADTS frames. The 13-bit `aac_frame_length`
/// caps a frame at 8191 bytes, so this always spans a syncword plus enough of
/// the following frame to validate its header.
const RESYNC_WINDOW: usize = 2 * 8192;

/// The subset of `AudioSpecificConfig` an ADTS header (or an MP4 `esds`, in a
/// later task) carries — enough to construct an AAC-LC decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AacConfig {
    /// MPEG-4 audio object type (LC = 2 = `profile + 1`, per the ADTS 2-bit
    /// `profile` field). For explicit HE-AAC (ASC AOT 5/29) this is the *core*
    /// object type (LC), with SBR/PS carried separately in the fields below.
    pub object_type: u8,
    /// **Core** PCM frame rate in Hz, from `sampling_frequency_index` — the rate
    /// the AAC-LC core decodes at (SBR output is `2×` this).
    pub sample_rate: u32,
    /// Channel count, from the 3-bit `channel_configuration`.
    pub channels: u8,
    /// Explicit SBR signalling: `true` when the MP4 ASC advertised HE-AAC
    /// (`audioObjectType == 5`/`29`). ADTS discovers SBR *implicitly* (via the
    /// fill element at decode time), so it leaves this `false`. When set, the
    /// decoder reports the 2× output rate from the first frame.
    pub sbr_signalled: bool,
    /// Explicit SBR output (extension) sample rate from the ASC, if signalled
    /// (`extensionSamplingFrequencyIndex`). Usually `2×sample_rate`; `None` for
    /// implicit/ADTS (the decoder then falls back to `2×core`).
    pub ext_sample_rate: Option<u32>,
}

/// One parsed ADTS header: [`AacConfig`] plus the framing needed to locate
/// the `raw_data_block` that follows it.
struct AdtsHeader {
    config: AacConfig,
    /// Header length actually on the wire: 7 bytes, or 9 with the optional
    /// 16-bit CRC (`protection_absent == 0`).
    header_len: usize,
    /// Whole-frame length in bytes, header included (the 13-bit
    /// `aac_frame_length` field).
    frame_len: usize,
}

/// Parse the fixed 7-byte ADTS header (`fixed(28 bits) + variable(28 bits)`).
/// Does not consume the optional CRC — that isn't part of this fixed-size
/// window, so the caller reads/skips it separately once `header_len` is known.
///
/// # Errors
/// [`DecodeError::Malformed`] on a bad syncword, non-zero `layer` (that's an
/// MP3 header, not ADTS), a reserved `sampling_frequency_index` (`>= 13`), or
/// an `aac_frame_length` too short to hold its own header.
/// [`DecodeError::Unsupported`] for a `channel_configuration` this reader
/// can't turn into a plain channel count (see [`channels_for`]).
fn parse_header(h: &[u8; 7]) -> Result<AdtsHeader, DecodeError> {
    // syncword(12) == 0xFFF, then ID(1), layer(2), protection_absent(1).
    if h[0] != 0xFF || (h[1] & 0xF0) != 0xF0 {
        return Err(DecodeError::Malformed);
    }
    let layer = (h[1] >> 1) & 0x3;
    if layer != 0 {
        return Err(DecodeError::Malformed); // layer != 00 is an MP3 header, not ADTS
    }
    let protection_absent = h[1] & 0x1;
    let header_len = if protection_absent == 1 { 7 } else { 9 };

    let profile = (h[2] >> 6) & 0x3;
    let object_type = profile + 1;
    let sfi = usize::from((h[2] >> 2) & 0xF);
    let sample_rate = *SAMPLE_RATES.get(sfi).ok_or(DecodeError::Malformed)?;
    let channel_config = ((h[2] & 0x1) << 2) | ((h[3] >> 6) & 0x3);
    let channels = channels_for(u32::from(channel_config))?;

    let frame_len = ((u16::from(h[3] & 0x03) << 11)
        | (u16::from(h[4]) << 3)
        | (u16::from(h[5] >> 5) & 0x07)) as usize;
    if frame_len < header_len {
        return Err(DecodeError::Malformed);
    }

    Ok(AdtsHeader {
        config: AacConfig {
            object_type,
            sample_rate,
            channels,
            // ADTS carries no explicit SBR signalling; SBR (if any) is
            // discovered implicitly from the fill element at decode time.
            sbr_signalled: false,
            ext_sample_rate: None,
        },
        header_len,
        frame_len,
    })
}

/// Try to parse an ADTS header at the start of `window` (≥ 7 bytes). `None` if
/// the window is too short or the bytes aren't a valid header. Used by the
/// resync scan to test candidate syncword positions without consuming the
/// source.
fn header_at(window: &[u8]) -> Option<AdtsHeader> {
    let h: &[u8; 7] = window.get(..7)?.try_into().ok()?;
    parse_header(h).ok()
}

/// Outcome of reading a 7-byte header at an absolute offset during the full
/// header-walk ([`AdtsReader::count_all_frames`]). Mirrors `tags::adts`.
enum FrameAt {
    /// A valid ADTS header; the `usize` is its whole-frame length.
    Valid(usize),
    /// Clean end of stream (nothing, or a short tail, at this boundary).
    Eof,
    /// Bytes present but not a valid ADTS header (garbage / false sync).
    Bad,
}

/// Read+validate the very first header of the stream. Unlike
/// [`read_next_header`], an empty/too-short source has no frame at all, so any
/// short read here is a genuine [`DecodeError::UnexpectedEof`], never a clean
/// end of stream.
fn read_first_header<S: Source>(src: &mut S) -> Result<(AdtsHeader, u64), DecodeError> {
    let mut h = [0u8; 7];
    read_exact(src, &mut h)?;
    // Byte offset of the first *real* ADTS header: 0, or past a leading ID3v2 tag.
    let mut audio_start = 0u64;
    // A tagger may prepend an ID3v2 tag to a raw `.aac` elementary stream
    // (mirrors `super::flac::skip_id3v2`). Skip it so the first *real* ADTS
    // header is parsed, not the "ID3" magic.
    if &h[0..3] == b"ID3" {
        // `h` holds ID3 bytes 0..=6; read the remaining 3 header bytes (7,8,9)
        // to recover the 28-bit syncsafe tag size and the footer-present flag.
        let mut rest = [0u8; 3];
        read_exact(src, &mut rest)?;
        let size = (u32::from(h[6] & 0x7F) << 21)
            | (u32::from(rest[0] & 0x7F) << 14)
            | (u32::from(rest[1] & 0x7F) << 7)
            | u32::from(rest[2] & 0x7F);
        let footer = if h[5] & 0x10 != 0 { 10 } else { 0 };
        skip(src, u64::from(size) + footer)?;
        // 10-byte ID3v2 header + tag body (+ optional 10-byte footer).
        audio_start = 10 + u64::from(size) + footer;
        read_exact(src, &mut h)?; // the real first ADTS header
    }
    let parsed = parse_header(&h)?;
    if parsed.header_len == 9 {
        skip(src, 2)?; // discard the 16-bit CRC
    }
    Ok((parsed, audio_start))
}

/// Read the next frame's 7-byte header. `Ok(None)` on a clean end of stream
/// (the very first header byte hits EOF); a read that starts but doesn't
/// complete the header is a truncated stream, not an end —
/// [`DecodeError::UnexpectedEof`].
fn read_next_header<S: Source>(src: &mut S) -> Result<Option<AdtsHeader>, DecodeError> {
    let mut h = [0u8; 7];
    let mut n = 0;
    while n < h.len() {
        match src.read(&mut h[n..]) {
            Ok(0) if n == 0 => return Ok(None),
            Ok(0) => return Err(DecodeError::UnexpectedEof),
            Ok(k) => n += k,
            Err(_) => return Err(DecodeError::Io),
        }
    }
    let parsed = parse_header(&h)?;
    if parsed.header_len == 9 {
        skip(src, 2)?; // discard the 16-bit CRC
    }
    Ok(Some(parsed))
}

/// Forward-streaming reader over a raw ADTS (`.aac`) elementary stream:
/// validates the first frame's header (exposing [`AacConfig`] via
/// [`Self::config`]), then serves each frame's `raw_data_block` bytes on
/// demand via [`Self::next_frame`].
pub struct AdtsReader<S> {
    src: S,
    config: AacConfig,
    /// `raw_data_block` length of a header already read but not yet served —
    /// `Some` right after [`Self::new`] (the first frame's header), or after a
    /// [`Self::seek_near_frame`]/[`Self::rewind`] repositions the reader. Once
    /// that payload is delivered, [`Self::next_frame`] goes back to reading a
    /// header itself.
    pending: Option<usize>,
    /// Absolute byte offset of the first ADTS frame (after any leading ID3v2
    /// tag) — the origin for the CBR byte-estimate length and seek.
    audio_start: u64,
    /// Full byte length (header + CRC + payload) of the first frame — the seed
    /// CBR frame size, used before [`Self::resolve_cbr`] runs.
    first_frame_len: usize,
    /// Audio byte span (`end − audio_start −` any trailing ID3v1 tag); set by
    /// [`Self::resolve_cbr`], `0` until then.
    audio_bytes: u64,
    /// Resolved CBR frame count (each ADTS frame = 1024 AAC-LC core
    /// samples/channel), or `None` for a not-yet-resolved, VBR, or
    /// non-seekable stream — in which case seeking stays the linear fallback.
    cbr_frames: Option<u64>,
}

impl<S: Source> AdtsReader<S> {
    /// Validate the stream's first ADTS header and construct a reader
    /// positioned to serve its `raw_data_block` via [`Self::next_frame`].
    ///
    /// # Errors
    /// [`DecodeError::UnexpectedEof`]/[`DecodeError::Io`] on a read failure;
    /// [`DecodeError::Malformed`]/[`DecodeError::Unsupported`] on an invalid
    /// header (see [`parse_header`]).
    pub fn new(mut src: S) -> Result<Self, DecodeError> {
        let (header, audio_start) = read_first_header(&mut src)?;
        let payload_len = header.frame_len - header.header_len;
        Ok(Self {
            src,
            config: header.config,
            pending: Some(payload_len),
            audio_start,
            first_frame_len: header.frame_len,
            audio_bytes: 0,
            cbr_frames: None,
        })
    }

    /// The stream's AAC parameters (object type, sample rate, channels).
    #[must_use]
    pub fn config(&self) -> AacConfig {
        self.config
    }

    /// Resolved CBR frame count (each frame = 1024 AAC-LC core samples/channel),
    /// or `None` until [`Self::resolve_cbr`] runs — or for a VBR/non-seekable
    /// stream, where the decoder keeps its linear decode-discard seek.
    #[must_use]
    pub fn cbr_frames(&self) -> Option<u64> {
        self.cbr_frames
    }

    /// Rewind to the stream start and re-validate the first header, so the next
    /// [`Self::next_frame`] serves frame 0 again. ADTS has no index, so
    /// [`crate::formats::aac::AacDecoder`]'s seek decode-discards from the start
    /// via this.
    ///
    /// # Errors
    /// As [`Self::new`], plus [`DecodeError::Io`] if the seek fails.
    pub fn rewind(&mut self) -> Result<(), DecodeError>
    where
        S: SeekSource,
    {
        self.src
            .seek(SeekFrom::Start(0))
            .map_err(|_| DecodeError::Io)?;
        let (header, _audio_start) = read_first_header(&mut self.src)?;
        self.pending = Some(header.frame_len - header.header_len);
        Ok(())
    }

    /// Read the next frame's `raw_data_block` bytes into `buf`, returning the
    /// filled prefix. `Ok(None)` at a clean end of stream (no more frames).
    ///
    /// # Errors
    /// [`DecodeError::Malformed`] if the `raw_data_block` doesn't fit `buf`,
    /// or the next header is invalid; [`DecodeError::UnexpectedEof`] if the
    /// stream ends mid-frame; [`DecodeError::Io`] on a read failure.
    pub fn next_frame<'a>(&mut self, buf: &'a mut [u8]) -> Result<Option<&'a [u8]>, DecodeError> {
        let payload_len = match self.pending.take() {
            Some(len) => len,
            None => match read_next_header(&mut self.src)? {
                Some(h) => h.frame_len - h.header_len,
                None => return Ok(None),
            },
        };
        let dst = buf.get_mut(..payload_len).ok_or(DecodeError::Malformed)?;
        read_exact(&mut self.src, dst)?;
        Ok(Some(&buf[..payload_len]))
    }
}

impl<S: SeekSource> AdtsReader<S> {
    /// Resolve the stream's CBR length so [`Self::cbr_frames`] reports a frame
    /// count and [`Self::seek_near_frame`] can scale a byte-estimate seek. For a
    /// raw ADTS elementary stream there is no in-band length (no `mdhd`/Xing), so
    /// — mirroring MP3's [`resolve_cbr_length`](crate::demux::Mp3Demuxer) — this
    /// derives one from the audio byte span and a representative (CBR) frame size.
    ///
    /// Strategy:
    /// * Walk up to [`SAMPLE_FRAMES`] frames from the start. If the stream ends
    ///   within that budget, the walked count is the **exact** frame count.
    /// * Otherwise estimate `audio_bytes / mean_frame_len`, taking the mean over
    ///   a start region **and** an end region. When those regional means agree
    ///   (within 25 %) the stream is uniform CBR and the byte estimate is the
    ///   O(1) answer.
    /// * When they **disagree** (apparent VBR drift) the O(1) estimate would be
    ///   unreliable, so instead of giving up (which left the duration unknown and
    ///   the deck/seek-bar unable to compute a target), do a one-pass **full ADTS
    ///   header-walk** ([`Self::count_all_frames`]) — stepping past each frame by
    ///   its `aac_frame_length`, resyncing past any glitch — for the **exact**
    ///   frame count. A seekable ADTS stream therefore never reports `None`.
    ///
    /// The walk is header-only (a few ms even for a 5-minute file) and runs only
    /// on the non-uniform branch, so a uniform-CBR stream keeps the O(1) estimate
    /// and pays nothing extra. Either way [`Self::cbr_frames`] and `audio_bytes`
    /// are set, so [`Self::seek_near_frame`]'s O(1) byte-estimate seek is used in
    /// both cases (unchanged).
    ///
    /// Idempotent, and it saves/restores the source cursor, so it is safe to call
    /// at any point without disturbing an in-progress decode. The `pending`
    /// frame-serving state is never touched.
    pub fn resolve_cbr(&mut self) {
        if self.cbr_frames.is_some() || self.first_frame_len == 0 {
            return;
        }
        let Ok(saved) = self.src.seek(SeekFrom::Current(0)) else {
            return;
        };
        let Ok(end) = self.src.seek(SeekFrom::End(0)) else {
            return;
        };
        // A trailing ID3v1 tag ("TAG", 128 bytes) is metadata, not audio.
        let mut trailer = 0u64;
        if end >= self.audio_start + 128 && self.src.seek(SeekFrom::Start(end - 128)).is_ok() {
            let mut tag = [0u8; 3];
            if read_exact(&mut self.src, &mut tag).is_ok() && &tag == b"TAG" {
                trailer = 128;
            }
        }
        let audio_bytes = end.saturating_sub(self.audio_start).saturating_sub(trailer);
        let resolved = if audio_bytes == 0 {
            None
        } else {
            // Uniform CBR → O(1) byte estimate; non-uniform (was `None`) → exact
            // full header-walk, so the duration is never left unknown.
            self.estimate_frames(audio_bytes)
                .or_else(|| self.count_all_frames())
        };
        if let Some(frames) = resolved {
            if frames > 0 {
                self.audio_bytes = audio_bytes;
                self.cbr_frames = Some(frames);
            }
        }
        let _ = self.src.seek(SeekFrom::Start(saved));
    }

    /// Compute the CBR frame count for `audio_bytes` (see [`Self::resolve_cbr`]).
    /// `None` when the stream looks VBR or can't be sampled.
    fn estimate_frames(&mut self, audio_bytes: u64) -> Option<u64> {
        let (count, bytes, _min, _max, eof) =
            self.walk_from(self.audio_start, SAMPLE_FRAMES).ok()?;
        if count == 0 {
            return None;
        }
        if eof {
            // The whole stream fit in the sample budget: an exact frame count.
            return Some(u64::from(count));
        }
        // Integer byte-per-frame means (no_std has no float rounding; frame sizes
        // are hundreds of bytes, so the ±0.5-byte truncation is negligible).
        let start_mean = bytes / u64::from(count);
        // Sample a run near the end (~90 % through the audio) to catch bitrate
        // drift the opening frames wouldn't show.
        let near_end = self.audio_start + audio_bytes - audio_bytes / 10;
        let end_mean = self
            .sample_region_mean(near_end, SAMPLE_FRAMES)
            .unwrap_or(start_mean);
        let lo = start_mean.min(end_mean);
        let hi = start_mean.max(end_mean);
        // >25 % drift between the regional means ⇒ genuinely VBR.
        if lo == 0 || (hi - lo) * 4 > lo {
            return None; // VBR → unknown length, keep the linear seek
        }
        let mean = (start_mean + end_mean) / 2;
        if mean == 0 {
            return None;
        }
        // Round to the nearest whole frame.
        Some((audio_bytes + mean / 2) / mean)
    }

    /// Position the reader so the next [`Self::next_frame`] serves (approximately)
    /// output frame `target_frame`, via a CBR byte estimate + syncword resync —
    /// O(1) instead of the linear decode-discard from the start.
    ///
    /// Like MP3's CBR seek this is **frame-granular approximate**: the byte
    /// estimate lands within about one frame of the target, and the caller
    /// reports the requested position. Requires a resolved CBR length
    /// ([`Self::cbr_frames`]).
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] with no resolved length; [`DecodeError::Io`]
    /// on a seek failure; [`DecodeError::Malformed`] if no valid frame boundary
    /// is found near the estimate (the caller then falls back to the linear seek).
    pub fn seek_near_frame(&mut self, target_frame: u64) -> Result<(), DecodeError> {
        let (Some(total), bytes) = (self.cbr_frames, self.audio_bytes) else {
            return Err(DecodeError::Unsupported);
        };
        if total == 0 || bytes == 0 {
            return Err(DecodeError::Unsupported);
        }
        let tf = target_frame.min(total - 1);
        let off = (u128::from(tf) * u128::from(bytes) / u128::from(total)) as u64;
        let (boundary, hdr) = self.resync_from(self.audio_start + off)?;
        // Land just past the fixed header (+ CRC) so `next_frame` reads the payload.
        self.src
            .seek(SeekFrom::Start(boundary + hdr.header_len as u64))
            .map_err(|_| DecodeError::Io)?;
        self.pending = Some(hdr.frame_len - hdr.header_len);
        Ok(())
    }

    /// Walk up to `max` consecutive frames from byte `start`, returning
    /// `(count, total_bytes, min_len, max_len, hit_eof)`. Header-only (no decode):
    /// each step reads a 7-byte header and seeks over the frame body. `hit_eof`
    /// is `true` when the stream cleanly ended before `max` frames.
    fn walk_from(
        &mut self,
        start: u64,
        max: u32,
    ) -> Result<(u32, u64, usize, usize, bool), DecodeError> {
        self.src
            .seek(SeekFrom::Start(start))
            .map_err(|_| DecodeError::Io)?;
        let mut count = 0u32;
        let mut bytes = 0u64;
        let mut min = usize::MAX;
        let mut max_len = 0usize;
        while count < max {
            let mut h = [0u8; 7];
            let mut n = 0;
            while n < h.len() {
                match self.src.read(&mut h[n..]) {
                    Ok(0) => break,
                    Ok(k) => n += k,
                    Err(_) => return Err(DecodeError::Io),
                }
            }
            if n < h.len() {
                return Ok((count, bytes, min, max_len, true)); // clean end of stream
            }
            let Ok(hdr) = parse_header(&h) else {
                break; // garbage — stop (not a clean EOF)
            };
            count += 1;
            bytes += hdr.frame_len as u64;
            min = min.min(hdr.frame_len);
            max_len = max_len.max(hdr.frame_len);
            self.src
                .seek(SeekFrom::Current((hdr.frame_len - 7) as i64))
                .map_err(|_| DecodeError::Io)?;
        }
        Ok((count, bytes, min, max_len, false))
    }

    /// Mean frame length (bytes/frame) over a run of frames beginning at a
    /// resynced boundary at/after `anchor`. `None` if no boundary is found or the
    /// run is empty.
    fn sample_region_mean(&mut self, anchor: u64, max: u32) -> Option<u64> {
        let (boundary, _hdr) = self.resync_from(anchor).ok()?;
        let (count, bytes, _min, _max, _eof) = self.walk_from(boundary, max).ok()?;
        (count > 0).then(|| bytes / u64::from(count))
    }

    /// Seek to `off` and classify the 7 bytes there (header-only). A 0-byte or
    /// short read is [`FrameAt::Eof`]; a full-but-unparsable read is
    /// [`FrameAt::Bad`]. `Err` only on a hard IO failure.
    fn frame_at(&mut self, off: u64) -> Result<FrameAt, DecodeError> {
        if self.src.seek(SeekFrom::Start(off)).is_err() {
            return Ok(FrameAt::Bad);
        }
        let mut h = [0u8; 7];
        let mut n = 0;
        while n < h.len() {
            match self.src.read(&mut h[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(_) => return Err(DecodeError::Io),
            }
        }
        if n < h.len() {
            return Ok(FrameAt::Eof); // 0 bytes = clean EOF; 1..7 = truncated tail
        }
        Ok(parse_header(&h).map_or(FrameAt::Bad, |hdr| FrameAt::Valid(hdr.frame_len)))
    }

    /// Walk the **entire** stream header-by-header from [`Self::audio_start`],
    /// returning the exact frame count. The uniform-CBR byte estimate declined
    /// (apparent VBR drift), so this one-pass header-only walk recovers the true
    /// count rather than leave the duration unknown. Steps past each frame by its
    /// `aac_frame_length`, and — like `tags::adts` and [`Self::resync_from`] —
    /// resyncs forward past a garbled frame / inter-frame gap (double-validated
    /// syncword) so a mid-stream glitch can't truncate the count. Ends only at a
    /// genuine end of stream. `pos` strictly increases every step (`parse_header`
    /// rejects `frame_len < header_len`, and `.max(pos + 1)` guarantees progress),
    /// so the walk always terminates. `None` only for a degenerate zero-frame
    /// stream. The caller ([`Self::resolve_cbr`]) restores the source cursor.
    fn count_all_frames(&mut self) -> Option<u64> {
        let mut frames: u64;
        let mut pos: u64;
        let mut frame_len: usize;
        match self.frame_at(self.audio_start).ok()? {
            FrameAt::Valid(len) => {
                frames = 1;
                pos = self.audio_start;
                frame_len = len;
            }
            // A leading glitch before the first real boundary — resync to it.
            FrameAt::Bad => {
                let (boundary, hdr) = self.resync_from(self.audio_start).ok()?;
                frames = 1;
                pos = boundary;
                frame_len = hdr.frame_len;
            }
            FrameAt::Eof => return None,
        }
        loop {
            // Next frame begins `frame_len` bytes on; `.max(pos + 1)` guarantees
            // forward progress even for a degenerate frame length.
            let next = (pos + frame_len as u64).max(pos + 1);
            match self.frame_at(next).ok()? {
                FrameAt::Valid(len) => {
                    frames += 1;
                    pos = next;
                    frame_len = len;
                }
                FrameAt::Eof => break, // genuine end of stream — done
                FrameAt::Bad => match self.resync_from(next) {
                    Ok((boundary, hdr)) => {
                        frames += 1;
                        pos = boundary;
                        frame_len = hdr.frame_len;
                    }
                    Err(_) => break, // no valid frame ahead within the span → end
                },
            }
        }
        Some(frames)
    }

    /// Scan forward from byte `from` for an ADTS frame boundary, validated by a
    /// following frame header (`0xFFF`, `layer == 00`, frame-length-linked) to
    /// reject a false sync inside an AAC payload. Returns the boundary's absolute
    /// offset and its parsed header. Reads a window (moving the cursor); the
    /// caller repositions afterward.
    ///
    /// # Errors
    /// [`DecodeError::Io`] on a seek/read failure; [`DecodeError::Malformed`] if
    /// no validated boundary lies within the scan window.
    fn resync_from(&mut self, from: u64) -> Result<(u64, AdtsHeader), DecodeError> {
        self.src
            .seek(SeekFrom::Start(from))
            .map_err(|_| DecodeError::Io)?;
        let mut buf = [0u8; RESYNC_WINDOW];
        let mut filled = 0usize;
        while filled < buf.len() {
            match self.src.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(k) => filled += k,
                Err(_) => return Err(DecodeError::Io),
            }
        }
        // First double-validated syncword wins; a single-validated candidate whose
        // successor falls outside the window is kept only as a last resort (a
        // boundary within one frame of the stream's end).
        let mut single: Option<(usize, AdtsHeader)> = None;
        let mut i = 0usize;
        while i + 7 <= filled {
            if let Some(hdr) = header_at(&buf[i..filled]) {
                let j = i + hdr.frame_len;
                if j + 7 <= filled {
                    if header_at(&buf[j..filled]).is_some() {
                        return Ok((from + i as u64, hdr)); // two consecutive valid headers
                    }
                    // Successor invalid → false sync inside a payload; keep scanning.
                } else if single.is_none() {
                    single = Some((i, hdr));
                }
            }
            i += 1;
        }
        match single {
            Some((i, hdr)) => Ok((from + i as u64, hdr)),
            None => Err(DecodeError::Malformed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceSource;
    const AAC: &[u8] = include_bytes!("../../testdata/aac.aac");

    /// `Result::unwrap_err` needs the `Ok` type to be `Debug` (for the panic
    /// message), which `AdtsReader<S>` deliberately isn't (matching every
    /// sibling reader in this module — [`super::super::mp4::Mp4Reader`] et
    /// al.). This sidesteps that bound for the error-path tests below.
    fn expect_err<T>(r: Result<T, DecodeError>) -> DecodeError {
        match r {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        }
    }

    #[test]
    fn probes_and_parses_adts_header() {
        assert_eq!(
            crate::probe::probe(&mut SliceSource::new(AAC)).unwrap(),
            crate::model::Format::Adts
        );
        let r = AdtsReader::new(SliceSource::new(AAC)).unwrap();
        let c = r.config();
        assert_eq!(c.object_type, 2); // LC (profile 01 + 1)
        assert_eq!(c.sample_rate, 44_100);
        assert_eq!(c.channels, 2);
    }

    #[test]
    fn iterates_frames() {
        let mut r = AdtsReader::new(SliceSource::new(AAC)).unwrap();
        let mut buf = [0u8; 8192];
        let mut frames = 0;
        while r.next_frame(&mut buf).unwrap().is_some() {
            frames += 1;
        }
        assert!(frames > 10); // ~0.5s / (1024/44100) ≈ 21 frames
    }

    #[test]
    fn bad_syncword_is_malformed() {
        let bytes = [0x00u8; 16];
        assert_eq!(
            expect_err(AdtsReader::new(SliceSource::new(&bytes))),
            DecodeError::Malformed
        );
    }

    #[test]
    fn reserved_sampling_frequency_index_is_malformed() {
        // Real header with SFI forced to 13 (reserved): byte2 bits5-2 = 1101.
        let mut bytes = AAC[..16].to_vec();
        bytes[2] = (bytes[2] & 0xC3) | (0b1101 << 2);
        assert_eq!(
            expect_err(AdtsReader::new(SliceSource::new(&bytes))),
            DecodeError::Malformed
        );
    }

    #[test]
    fn zero_channel_configuration_is_unsupported() {
        // channel_configuration is split across byte2 bit0 and byte3 bits7-6;
        // clear both to force cc == 0 ("from PCE").
        let mut bytes = AAC[..16].to_vec();
        bytes[2] &= !0x01;
        bytes[3] &= 0x3F;
        assert_eq!(
            expect_err(AdtsReader::new(SliceSource::new(&bytes))),
            DecodeError::Unsupported
        );
    }

    #[test]
    fn frame_length_shorter_than_header_is_malformed() {
        // Force the 13-bit aac_frame_length field to 0 (< the 7-byte header).
        let mut bytes = AAC[..16].to_vec();
        bytes[3] &= 0xFC; // clear the top 2 bits of the field
        bytes[4] = 0;
        bytes[5] &= 0x1F; // clear the bottom 3 bits of the field
        assert_eq!(
            expect_err(AdtsReader::new(SliceSource::new(&bytes))),
            DecodeError::Malformed
        );
    }

    #[test]
    fn next_frame_reports_none_at_clean_eos() {
        // A single real frame, exactly frame-length bytes long: EOS lands
        // right on a header boundary.
        let frame_len = ((u16::from(AAC[3] & 0x03) << 11)
            | (u16::from(AAC[4]) << 3)
            | (u16::from(AAC[5] >> 5) & 0x07)) as usize;
        let bytes = &AAC[..frame_len];
        let mut r = AdtsReader::new(SliceSource::new(bytes)).unwrap();
        let mut buf = [0u8; 8192];
        assert!(r.next_frame(&mut buf).unwrap().is_some());
        assert!(r.next_frame(&mut buf).unwrap().is_none());
    }

    #[test]
    fn payload_larger_than_buf_is_malformed() {
        let mut r = AdtsReader::new(SliceSource::new(AAC)).unwrap();
        let mut tiny = [0u8; 1];
        assert_eq!(r.next_frame(&mut tiny).unwrap_err(), DecodeError::Malformed);
    }

    /// Build one 44.1 kHz-stereo (sfi=4, chan_cfg=2), LC (profile=01),
    /// protection-absent ADTS frame: a 7-byte header + `payload` zero bytes, so
    /// `aac_frame_length == 7 + payload`. Mirrors the `tags::adts` test builder.
    fn adts_frame(payload: usize) -> Vec<u8> {
        let frame_len = (7 + payload) as u32;
        let mut v = Vec::with_capacity(frame_len as usize);
        v.push(0xFF); // sync hi
        v.push(0xF1); // sync lo + layer 00 + protection_absent=1
        v.push(0x50); // profile=01(LC) | sfi=0100(4) | priv=0 | chan_cfg hi=0
        v.push(0x80 | ((frame_len >> 11) & 0x03) as u8); // chan_cfg lo=10 | fl[12:11]
        v.push(((frame_len >> 3) & 0xFF) as u8); // fl[10:3]
        v.push((((frame_len & 0x07) << 5) as u8) | 0x1F); // fl[2:0] | buffer-fullness hi
        v.push(0xFC); // buffer-fullness lo | num_raw_blocks=0
        v.extend(core::iter::repeat_n(0u8, payload));
        v
    }

    /// RED→GREEN for the ADTS-duration bug: a stream whose frame sizes drift
    /// enough to trip the >25 % VBR guard (small opening frames, large tail
    /// frames) used to resolve to `cbr_frames() == None` — so the deck/seek-bar
    /// got no duration and seeked to 0. `resolve_cbr` now falls back to the exact
    /// full header-walk, so a seekable ADTS stream reports its true frame count.
    #[test]
    fn resolve_cbr_nonuniform_falls_back_to_exact_header_walk() {
        // 40 small (payload 20 → 27 B) + 40 large (payload 300 → 307 B) frames:
        // > SAMPLE_FRAMES, and the start/end regional means (27 vs 307) differ far
        // beyond 25 %, so the byte estimate declines (was `None`).
        let mut bytes = Vec::new();
        for _ in 0..40 {
            bytes.extend_from_slice(&adts_frame(20));
        }
        for _ in 0..40 {
            bytes.extend_from_slice(&adts_frame(300));
        }
        let mut r = AdtsReader::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(r.cbr_frames(), None, "unresolved before resolve_cbr");
        r.resolve_cbr();
        // RED (old): None (VBR guard gave up). GREEN: exact walked count.
        assert_eq!(
            r.cbr_frames(),
            Some(80),
            "non-uniform ADTS must resolve to the exact frame count, not None"
        );
    }

    /// A mid-stream glitch inside a non-uniform stream must be resynced past, not
    /// truncate the count (the walk mirrors `tags::adts`): two 40-frame runs of
    /// differing size separated by non-sync junk total 80 frames.
    #[test]
    fn resolve_cbr_header_walk_resyncs_past_midstream_gap() {
        let mut bytes = Vec::new();
        for _ in 0..40 {
            bytes.extend_from_slice(&adts_frame(20));
        }
        bytes.extend_from_slice(&[0u8; 40]); // non-sync junk (never 0xFFF)
        for _ in 0..40 {
            bytes.extend_from_slice(&adts_frame(300));
        }
        let mut r = AdtsReader::new(SliceSource::new(&bytes)).unwrap();
        r.resolve_cbr();
        assert_eq!(
            r.cbr_frames(),
            Some(80),
            "resync must count across the gap, not stop at it"
        );
    }

    /// No-regression on the uniform path: a stream that fits the sample budget
    /// (`aac.aac`, 23 frames) keeps resolving to its exact count via the O(1)
    /// estimate — the header-walk fallback is never reached.
    #[test]
    fn resolve_cbr_uniform_stays_exact_via_estimate() {
        let mut r = AdtsReader::new(SliceSource::new(AAC)).unwrap();
        r.resolve_cbr();
        assert_eq!(r.cbr_frames(), Some(23), "uniform CBR aac.aac = 23 frames");
    }
}
