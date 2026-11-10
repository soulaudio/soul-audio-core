//! MP3 (MPEG-1/2/2.5 Audio Layer III) demuxer — skips ID3v2, locks onto the
//! frame sync, parses stream parameters, and packetizes one MPEG frame per
//! packet. MP3 *decoding* (Huffman, IMDCT, the bit reservoir) is the codec's
//! job, deferred.

use embedded_io::SeekFrom;

use super::{Demuxer, Packet, read_exact, skip};
use crate::decoder::DecodeError;
use crate::io::{SeekSource, Source};
use crate::model::{SampleFormat, StreamInfo};

/// Bitrate (kbps) by index, per MPEG version + layer (index 0 = free, 15 = bad).
const BR_V1_L1: [u32; 16] = [
    0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448, 0,
];
const BR_V1_L2: [u32; 16] = [
    0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0,
];
const BR_V1_L3: [u32; 16] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
];
const BR_V2_L1: [u32; 16] = [
    0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 0,
];
const BR_V2_L23: [u32; 16] = [
    0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
];

/// Bitrate (kbps) for `(mpeg1, layer ∈ {1,2,3})` at table index `idx`.
fn bitrate_kbps(mpeg1: bool, layer: u8, idx: usize) -> u32 {
    let table = match (mpeg1, layer) {
        (true, 1) => &BR_V1_L1,
        (true, 2) => &BR_V1_L2,
        (true, _) => &BR_V1_L3,
        (false, 1) => &BR_V2_L1,
        (false, _) => &BR_V2_L23,
    };
    table[idx]
}
/// Sample rate by version bits (00=2.5, 01=reserved, 10=2, 11=1) and index.
const SRATE: [[u32; 4]; 4] = [
    [11025, 12000, 8000, 0],  // MPEG-2.5
    [0, 0, 0, 0],             // reserved
    [22050, 24000, 16000, 0], // MPEG-2
    [44100, 48000, 32000, 0], // MPEG-1
];

/// Largest MPEG frame buffered for the first-frame (Xing/VBRI) scan: MPEG-1
/// Layer III at 320 kbps / 32 kHz is 1441 bytes.
const FRAME_MAX: usize = 1600;

/// Bytes read in one window when resyncing to a frame boundary after a
/// header-less CBR byte-estimate seek ([`Mp3Demuxer::resync_from`]). Holds
/// several MPEG frames (max ≈1728 B for MPEG-1 Layer II) so the scan can
/// **double-validate** a candidate sync against the following frame's header,
/// yet stays a small constant so the seek is O(1) on the stream size.
const RESYNC_WINDOW: usize = 4096;

/// A parsed Xing/Info/VBRI VBR header (present in the first frame of most
/// encoder output). Gives duration and a seek index.
#[derive(Clone, Copy)]
struct VbrTag {
    /// Total MPEG frames in the stream (excludes the tag frame itself).
    total_frames: u64,
    /// Total audio bytes covered by the TOC (0 if absent).
    total_bytes: u64,
    /// `true` for a LAME `Info` tag (constant bit rate) → O(1) exact seek.
    cbr: bool,
    /// Xing 100-entry table-of-contents (byte ≈ `TOC[i]/256 × total_bytes` at
    /// time fraction `i%`), if present.
    toc: Option<[u8; 100]>,
    /// Encoder delay (per-channel priming samples) from the LAME/Lavc info tag's
    /// delays field; `0` when no recognized info tag is present. The gapless
    /// leading skip the decoder reports is this plus the MPEG decoder delay.
    enc_delay: u32,
    /// Encoder padding (per-channel trailing samples added to fill the last
    /// frame) from the same delays field; `0` when none. Used to compute the
    /// gapless playable length.
    enc_padding: u32,
}

/// MP3 container parser over a byte [`Source`].
pub struct Mp3Demuxer<S> {
    src: S,
    info: StreamInfo,
    spf: u32, // samples per frame
    pos_samples: u64,
    /// Absolute byte offset of the first frame's sync (after any ID3 tag).
    audio_start: u64,
    /// Constant CBR frame length in bytes (the first frame's length).
    frame_len: usize,
    /// First frame's bitrate in bits/second — the basis for the header-less CBR
    /// length ([`Self::resolve_cbr_length`]), which is bitrate-derived rather
    /// than frame-length-based so per-frame padding doesn't bias it.
    bitrate_bps: u32,
    /// VBR/duration header from the first frame, if any.
    vbr: Option<VbrTag>,
    /// Resolved total length in per-channel samples. Seeded in [`Self::new`] from
    /// the Xing/Info/VBRI header (`None` for a header-less CBR MP1/MP2/MP3), and
    /// filled in on demand by [`Self::resolve_cbr_length`] with a CBR byte-size
    /// estimate when no header declared it.
    total: Option<u64>,
    /// The buffered first frame (header + body), served before any seek.
    first_frame: [u8; FRAME_MAX],
    first_len: usize,
    first_pending: bool,
    /// A header already read (by the forward-scan seek), to serve next.
    pending: Option<[u8; 4]>,
}

impl<S: Source> Mp3Demuxer<S> {
    /// Skip ID3v2, find the first valid Layer III frame, and read its parameters.
    ///
    /// # Errors
    /// [`DecodeError::Malformed`] if no valid frame header is found.
    pub fn new(mut src: S) -> Result<Self, DecodeError> {
        let mut head = [0u8; 4];
        read_exact(&mut src, &mut head)?;
        let mut pos: u64 = 4;

        if &head[0..3] == b"ID3" {
            let mut rest = [0u8; 6];
            read_exact(&mut src, &mut rest)?;
            let flags = rest[1];
            let size = (u32::from(rest[2]) << 21)
                | (u32::from(rest[3]) << 14)
                | (u32::from(rest[4]) << 7)
                | u32::from(rest[5]);
            let footer = if flags & 0x10 != 0 { 10 } else { 0 };
            skip(&mut src, u64::from(size) + footer)?;
            read_exact(&mut src, &mut head)?;
            pos += 6 + u64::from(size) + footer + 4;
        }

        // Resync to the first valid frame header. `pos` indexes the byte just
        // past the 4-byte `head` window, so the header starts at `pos - 4`.
        let f = loop {
            if let Some(f) = parse_header(&head) {
                break f;
            }
            head[0] = head[1];
            head[1] = head[2];
            head[2] = head[3];
            let mut b = [0u8; 1];
            read_exact(&mut src, &mut b)?; // EOF here → Malformed (no frame)
            head[3] = b[0];
            pos += 1;
        };

        // Buffer the whole first frame (it carries any Xing/Info/VBRI tag), so
        // it can be both parsed and served as the first packet.
        let mut first_frame = [0u8; FRAME_MAX];
        let frame_len = f.len.min(FRAME_MAX);
        first_frame[0..4].copy_from_slice(&head);
        let mut n = 4;
        while n < frame_len {
            match src.read(&mut first_frame[n..frame_len]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(_) => return Err(DecodeError::Io),
            }
        }
        let vbr = parse_vbr_tag(&first_frame[..n], &f);

        Ok(Self {
            src,
            info: StreamInfo {
                sample_rate: f.sample_rate,
                channels: f.channels,
                format: SampleFormat::Int { bits: 16 },
            },
            spf: f.spf,
            pos_samples: 0,
            audio_start: pos - 4,
            frame_len: f.len,
            bitrate_bps: f.bitrate_bps,
            vbr,
            total: vbr.map(|v| v.total_frames * u64::from(f.spf)),
            first_frame,
            first_len: n,
            first_pending: true,
            pending: None,
        })
    }

    /// Samples per frame (1152 for MPEG-1 Layer III, 576 for MPEG-2/2.5).
    #[must_use]
    pub fn samples_per_frame(&self) -> u32 {
        self.spf
    }

    /// Total samples per channel. Comes from a Xing/Info/VBRI header when present;
    /// otherwise `None` until [`Self::resolve_cbr_length`] computes a CBR estimate
    /// (a header-less CBR MP1/MP2/MP3 has no in-band length).
    #[must_use]
    pub fn total_samples(&self) -> Option<u64> {
        self.total
    }

    /// Encoder delay (per-channel priming samples) from the LAME/Lavc info tag;
    /// `0` when none is present. This is the *encoder's* delay only — the
    /// decoder adds its own MPEG synthesis delay for the full gapless skip.
    #[must_use]
    pub fn encoder_delay(&self) -> u32 {
        self.vbr.map_or(0, |v| v.enc_delay)
    }

    /// Encoder padding (per-channel trailing samples) from the LAME/Lavc info
    /// tag; `0` when none. The gapless trailing trim is this minus the MPEG
    /// decoder delay.
    #[must_use]
    pub fn encoder_padding(&self) -> u32 {
        self.vbr.map_or(0, |v| v.enc_padding)
    }

    /// Read the next frame header (resyncing past garbage). `Ok(None)` at EOF.
    fn next_header(&mut self) -> Result<Option<[u8; 4]>, DecodeError> {
        let mut h = [0u8; 4];
        if !fill_or_eof(&mut self.src, &mut h)? {
            return Ok(None);
        }
        loop {
            if parse_header(&h).is_some() {
                return Ok(Some(h));
            }
            h[0] = h[1];
            h[1] = h[2];
            h[2] = h[3];
            let mut b = [0u8; 1];
            if !fill_or_eof(&mut self.src, &mut b)? {
                return Ok(None);
            }
            h[3] = b[0];
        }
    }
}

impl<S: SeekSource> Mp3Demuxer<S> {
    /// Position the stream at frame `target` (0-based). With a Xing/VBRI header
    /// this jumps in O(1) via the TOC (VBR) or the byte-per-frame ratio (CBR) —
    /// **approximate** (MP3 seeking is inherently so: frames vary by a padding
    /// byte and the decoder warms up after a seek), landing on the nearest frame
    /// boundary near `target`. Without a header it **forward-scans** (exact but
    /// O(frames)). Clamps to the frame count. Returns the requested frame's first
    /// sample.
    ///
    /// # Errors
    /// [`DecodeError::Io`] on a seek failure; propagates read errors.
    pub(crate) fn seek_to_frame(&mut self, target: u64) -> Result<u64, DecodeError> {
        self.first_pending = false;
        self.pending = None;

        if let Some(v) = self.vbr {
            let target = target.min(v.total_frames);
            if let Some(byte_off) = self.estimate_byte(target, &v) {
                // Fast path: jump near the target; `next_header` resyncs to the
                // next real frame boundary on the following `next_packet`.
                self.src
                    .seek(SeekFrom::Start(self.audio_start + byte_off))
                    .map_err(|_| DecodeError::Io)?;
                self.pos_samples = target * u64::from(self.spf);
                return Ok(self.pos_samples);
            }
        }

        // Fast path for a **header-less CBR** stream (MP1/MP2, or an MP3 with no
        // Xing/Info/VBRI tag) once [`Self::resolve_cbr_length`] has filled
        // `total`: jump by a bitrate-derived byte estimate and resync to a real
        // frame boundary — O(1), vs the linear decode-discard below. Mirrors the
        // raw-ADTS fast seek. Only taken without a VBR tag (a tagged VBR stream
        // keeps the exact scan so no bad landing is produced); without `total`
        // (true tagless VBR) it stays linear.
        if self.vbr.is_none() {
            if let Some(total) = self.total {
                let total_frames = total / u64::from(self.spf);
                if total_frames > 0 {
                    if let Ok(pos) = self.seek_cbr_estimate(target, total_frames) {
                        return Ok(pos);
                    }
                    // Resync found no validated boundary near the estimate — fall
                    // through to the exact forward-scan rather than risk garbage.
                }
            }
        }

        // Exact fallback: forward-scan frame headers from the start.
        self.src
            .seek(SeekFrom::Start(self.audio_start))
            .map_err(|_| DecodeError::Io)?;
        let mut idx = 0u64;
        loop {
            let head = match self.next_header()? {
                Some(h) => h,
                None => {
                    self.pos_samples = idx * u64::from(self.spf); // clamp to end
                    return Ok(self.pos_samples);
                }
            };
            if idx == target {
                self.pending = Some(head); // src is past the 4 header bytes
                self.pos_samples = idx * u64::from(self.spf);
                return Ok(self.pos_samples);
            }
            let f = parse_header(&head).ok_or(DecodeError::Malformed)?;
            skip(&mut self.src, (f.len - 4) as u64)?;
            idx += 1;
        }
    }

    /// Estimate the byte offset (from `audio_start`) of frame `target` using the
    /// VBR header. `None` if the header lacks the fields needed.
    fn estimate_byte(&self, target: u64, v: &VbrTag) -> Option<u64> {
        if v.total_frames == 0 {
            return None;
        }
        if v.cbr {
            // Constant frame length (± a padding byte, corrected by resync).
            return Some(target * self.frame_len as u64);
        }
        let toc = v.toc?;
        if v.total_bytes == 0 {
            return None;
        }
        // Xing TOC: byte ≈ TOC[i]/256 × total_bytes at time fraction i%.
        let frac = (target as f64 / v.total_frames as f64) * 100.0;
        let i = (frac as usize).min(99);
        let lo = f64::from(toc[i]);
        let hi = if i < 99 { f64::from(toc[i + 1]) } else { 256.0 };
        let interp = lo + (hi - lo) * (frac - i as f64);
        Some((interp / 256.0 * v.total_bytes as f64) as u64)
    }

    /// Byte-estimate seek for a header-less CBR stream: compute frame `target`'s
    /// approximate byte offset from the **bitrate** (padding-immune, like
    /// [`Self::resolve_cbr_length`]), seek there, then [`Self::resync_from`]
    /// snaps to the next real, double-validated frame boundary and positions the
    /// source at it so the next `next_packet` serves a clean frame. Frame-granular
    /// approximate (the landing is within ~one frame; the decoder's pre-roll warms
    /// the filterbank from there). `total_frames` is the resolved frame count.
    ///
    /// # Errors
    /// [`DecodeError::Io`] on a seek failure; [`DecodeError::Malformed`] if no
    /// validated boundary is found near the estimate (caller falls back to scan).
    fn seek_cbr_estimate(&mut self, target: u64, total_frames: u64) -> Result<u64, DecodeError> {
        let tf = target.min(total_frames.saturating_sub(1));
        // byte ≈ frame × bitrate × spf ÷ (8 × sample_rate). u128 keeps the product
        // (frames·bits·spf can reach ~1e15 on a long file) from overflowing;
        // integer math only (`no_std`).
        let off = (u128::from(tf) * u128::from(self.bitrate_bps) * u128::from(self.spf)
            / (8 * u128::from(self.info.sample_rate))) as u64;
        let boundary = self.resync_from(self.audio_start + off)?;
        self.src
            .seek(SeekFrom::Start(boundary))
            .map_err(|_| DecodeError::Io)?;
        self.pending = None; // next_packet reads the header fresh at `boundary`
        self.pos_samples = tf * u64::from(self.spf);
        Ok(self.pos_samples)
    }

    /// Scan forward from byte `from` for an MPEG frame boundary whose header
    /// matches the stream's (constant, CBR) parameters and is **confirmed by a
    /// second matching header exactly `frame_len` bytes later** — rejecting a
    /// false `0xFFE`/`0xFFF` sync inside the audio data. Returns the boundary's
    /// absolute offset. Reads a bounded [`RESYNC_WINDOW`] (moving the cursor); the
    /// caller repositions afterwards.
    ///
    /// # Errors
    /// [`DecodeError::Io`] on a seek/read failure; [`DecodeError::Malformed`] if
    /// no validated boundary lies within the window.
    fn resync_from(&mut self, from: u64) -> Result<u64, DecodeError> {
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
        let mut single: Option<usize> = None;
        let mut i = 0usize;
        while i + 4 <= filled {
            if let Some(fc) = header_at(&buf[i..filled]) {
                if self.matches_stream(&fc) {
                    let j = i + fc.len;
                    if j + 4 <= filled {
                        if header_at(&buf[j..filled]).is_some_and(|fs| self.matches_stream(&fs)) {
                            return Ok(from + i as u64); // two consecutive stream frames
                        }
                        // Successor mismatched → false sync in a payload; keep scanning.
                    } else if single.is_none() {
                        single = Some(i);
                    }
                }
            }
            i += 1;
        }
        match single {
            Some(i) => Ok(from + i as u64),
            None => Err(DecodeError::Malformed),
        }
    }

    /// Does `f` carry this stream's constant CBR parameters? Used to lock a resync
    /// candidate onto the real frame grid (sample rate, bitrate, and samples per
    /// frame are invariant across a CBR MPEG stream).
    fn matches_stream(&self, f: &Frame) -> bool {
        f.sample_rate == self.info.sample_rate
            && f.bitrate_bps == self.bitrate_bps
            && f.spf == self.spf
            && f.channels == self.info.channels
    }

    /// Resolve the stream length for a header-less CBR stream (MP1/MP2, or an
    /// MP3 with no Xing/Info/VBRI tag), seeding [`Self::total_samples`] from the
    /// **bitrate**: `round(audio_bytes · 8 · sample_rate ÷ bitrate)` per-channel
    /// samples.
    ///
    /// Bitrate-based, not frame-count-based, because CBR pads only some frames
    /// (+1 slot) — so the first frame's length isn't representative and dividing
    /// the byte span by it reads long (MP1 +1.4 s / MP2 +0.44 s). `audio_bytes ·
    /// 8 ÷ bitrate` is the exact CBR duration regardless of padding distribution.
    ///
    /// No-op if a header already declared the length (that exact count wins) or
    /// if the bitrate is degenerate. Saves and restores the source position, so
    /// it is safe to call at any point without disturbing an in-progress decode.
    pub(crate) fn resolve_cbr_length(&mut self) {
        if self.total.is_some() || self.bitrate_bps == 0 {
            return;
        }
        // Save the cursor so a mid-decode call doesn't lose its place.
        let Ok(saved) = self.src.seek(SeekFrom::Current(0)) else {
            return;
        };
        let Ok(end) = self.src.seek(SeekFrom::End(0)) else {
            return;
        };
        // A trailing ID3v1 tag (128 bytes beginning with "TAG") is metadata, not
        // audio — exclude it from the frame count.
        let mut trailer = 0u64;
        if end >= self.audio_start + 128 && self.src.seek(SeekFrom::Start(end - 128)).is_ok() {
            let mut tag = [0u8; 3];
            if read_exact(&mut self.src, &mut tag).is_ok() && &tag == b"TAG" {
                trailer = 128;
            }
        }
        let audio_bytes = end.saturating_sub(self.audio_start).saturating_sub(trailer);
        // Bitrate-based CBR length: audio_bytes·8 ÷ bitrate = seconds (exact for
        // CBR), ×sample_rate = per-channel samples, round-half-up. u128 keeps the
        // intermediate product (bytes·8·rate ≈ 1e15 for a long file) from
        // overflowing. `no_std`: integer math only.
        let num = u128::from(audio_bytes) * 8 * u128::from(self.info.sample_rate);
        let den = u128::from(self.bitrate_bps);
        self.total = Some(((num + den / 2) / den) as u64);
        // Restore the cursor the caller was reading from.
        let _ = self.src.seek(SeekFrom::Start(saved));
    }
}

impl<S: Source> Demuxer for Mp3Demuxer<S> {
    fn stream_info(&self) -> StreamInfo {
        self.info
    }

    fn next_packet<'a>(
        &mut self,
        scratch: &'a mut [u8],
    ) -> Result<Option<Packet<'a>>, DecodeError> {
        // Serve the buffered first frame (which carried the VBR tag) before any
        // seek has repositioned the source.
        if self.first_pending {
            self.first_pending = false;
            let len = self.first_len;
            if len > scratch.len() {
                return Err(DecodeError::Unsupported);
            }
            scratch[..len].copy_from_slice(&self.first_frame[..len]);
            let ts = self.pos_samples;
            self.pos_samples += u64::from(self.spf);
            return Ok(Some(Packet {
                ts,
                dur: self.spf,
                data: &scratch[..len],
            }));
        }
        let header = match self.pending.take() {
            Some(h) => h,
            None => match self.next_header()? {
                Some(h) => h,
                None => return Ok(None),
            },
        };
        let frame = parse_header(&header).ok_or(DecodeError::Malformed)?;
        if frame.len < 4 || frame.len > scratch.len() {
            return Err(DecodeError::Unsupported); // scratch too small for the frame
        }
        scratch[0..4].copy_from_slice(&header);
        // Read the frame body; a short read at EOF truncates the final frame.
        let mut n = 4;
        while n < frame.len {
            match self.src.read(&mut scratch[n..frame.len]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(_) => return Err(DecodeError::Io),
            }
        }
        let ts = self.pos_samples;
        self.pos_samples += u64::from(self.spf);
        Ok(Some(Packet {
            ts,
            dur: self.spf,
            data: &scratch[..n],
        }))
    }
}

struct Frame {
    sample_rate: u32,
    channels: u16,
    spf: u32,
    len: usize,
    /// Bitrate in bits/second — the basis for the header-less CBR length, which
    /// is bitrate-derived (padding-immune) rather than first-frame-length-based.
    bitrate_bps: u32,
}

/// Read a big-endian `u32` at `off`, or `None` if out of bounds.
fn be32(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

/// Parse a Xing/`Info`/VBRI header from the first frame `frame` (a Layer III
/// frame described by `f`). Returns duration + a seek index, or `None`.
fn parse_vbr_tag(frame: &[u8], f: &Frame) -> Option<VbrTag> {
    if frame.len() < 4 {
        return None;
    }
    let mpeg1 = (frame[1] >> 3) & 0x3 == 3;
    let crc = if frame[1] & 1 == 0 { 2 } else { 0 }; // protection bit 0 ⇒ CRC present
    let mono = f.channels == 1;
    let side_info = match (mpeg1, mono) {
        (true, true) => 17,
        (true, false) => 32,
        (false, true) => 9,
        (false, false) => 17,
    };

    // Xing (VBR) / Info (CBR) at 4 + CRC + side-info.
    let xoff = 4 + crc + side_info;
    if let Some(magic) = frame.get(xoff..xoff + 4) {
        if magic == b"Xing" || magic == b"Info" {
            let cbr = magic == b"Info";
            let flags = be32(frame, xoff + 4)?;
            let mut p = xoff + 8;
            let mut total_frames = 0u64;
            let mut total_bytes = 0u64;
            let mut toc = None;
            if flags & 1 != 0 {
                total_frames = u64::from(be32(frame, p)?);
                p += 4;
            }
            if flags & 2 != 0 {
                total_bytes = u64::from(be32(frame, p)?);
                p += 4;
            }
            if flags & 4 != 0 {
                if let Some(t) = frame.get(p..p + 100) {
                    let mut arr = [0u8; 100];
                    arr.copy_from_slice(t);
                    toc = Some(arr);
                }
                p += 100;
            }
            if flags & 8 != 0 {
                p += 4; // quality field
            }
            // The LAME/Lavc info-tag extension begins here (`p`). Its 9-byte
            // vendor string ("LAME…"/"Lavc…") precedes a fixed layout whose
            // "encoder delays" field sits at offset 21: delay (12 bits) +
            // padding (12 bits). Guard on a recognized vendor so a bare Xing
            // tag (no extension) isn't misread as garbage delay.
            let (enc_delay, enc_padding) = parse_lame_delays(frame, p);
            if total_frames > 0 {
                return Some(VbrTag {
                    total_frames,
                    total_bytes,
                    cbr,
                    toc,
                    enc_delay,
                    enc_padding,
                });
            }
        }
    }

    // VBRI (Fraunhofer): fixed 32 bytes after the 4-byte header (byte 36).
    if frame.get(36..40) == Some(b"VBRI".as_slice()) {
        let total_bytes = u64::from(be32(frame, 36 + 10)?);
        let total_frames = u64::from(be32(frame, 36 + 14)?);
        if total_frames > 0 {
            // VBRI's TOC has a variable entry size; use the header for duration
            // and clamping (seeks fall back to the exact forward-scan).
            return Some(VbrTag {
                total_frames,
                total_bytes,
                cbr: false,
                toc: None,
                enc_delay: 0, // VBRI has no LAME encoder-delay field
                enc_padding: 0,
            });
        }
    }
    None
}

/// Read the encoder `(delay, padding)` (per-channel samples) from a LAME/Lavc
/// info tag whose 9-byte vendor string begins at `off` in `frame`. The "encoder
/// delays" field is 3 bytes at offset 21 — `delay` is the high 12 bits, padding
/// the low 12. Returns `(0, 0)` unless a recognized vendor (`LAME`/`Lavc`) is
/// present with the full field in range (so a Xing tag without the extension,
/// or an unknown encoder, yields no spurious values).
fn parse_lame_delays(frame: &[u8], off: usize) -> (u32, u32) {
    let tag = match frame.get(off..off + 24) {
        Some(t) => t,
        None => return (0, 0),
    };
    if &tag[0..4] != b"LAME" && &tag[0..4] != b"Lavc" {
        return (0, 0);
    }
    // bytes 21..24: delay(12) | padding(12).
    let delay = (u32::from(tag[21]) << 4) | (u32::from(tag[22]) >> 4);
    let padding = ((u32::from(tag[22]) & 0xF) << 8) | u32::from(tag[23]);
    (delay, padding)
}

/// Parse the 4-byte MPEG frame header at the start of `b` (which must be ≥ 4
/// bytes). `None` if the slice is too short or the header is invalid. Convenience
/// over [`parse_header`] for byte-window scans (resync).
fn header_at(b: &[u8]) -> Option<Frame> {
    let h: [u8; 4] = b.get(0..4)?.try_into().ok()?;
    parse_header(&h)
}

/// Parse+validate a 4-byte MPEG Layer III frame header. `None` if not a valid L3
/// frame (reserved fields, free/bad bitrate, non-Layer-III, etc.).
fn parse_header(h: &[u8; 4]) -> Option<Frame> {
    if h[0] != 0xFF || (h[1] & 0xE0) != 0xE0 {
        return None;
    }
    let version = (h[1] >> 3) & 0x3; // 0=2.5, 1=reserved, 2=v2, 3=v1
    let layer_code = (h[1] >> 1) & 0x3; // 01=III, 10=II, 11=I, 00=reserved
    let br_idx = ((h[2] >> 4) & 0xF) as usize;
    let sr_idx = ((h[2] >> 2) & 0x3) as usize;
    let padding = u32::from((h[2] >> 1) & 0x1);
    let chan_mode = (h[3] >> 6) & 0x3;

    // `br_idx == 0` is free-format (constant but table-less frame length) — it is
    // intentionally declined (rare; measuring the length would rework the
    // streaming read model). `br_idx == 15` is the reserved "bad" index.
    if version == 1 || layer_code == 0 || br_idx == 0 || br_idx == 15 || sr_idx == 3 {
        return None;
    }
    let layer = match layer_code {
        1 => 3, // Layer III
        2 => 2, // Layer II
        _ => 1, // Layer I
    };
    let mpeg1 = version == 3;
    let sample_rate = SRATE[version as usize][sr_idx];
    if sample_rate == 0 {
        return None;
    }
    let bitrate = bitrate_kbps(mpeg1, layer, br_idx);
    if bitrate == 0 {
        return None;
    }
    let bits = bitrate * 1000;
    // Samples per frame and byte length differ by layer/version.
    let (spf, len) = match layer {
        1 => (384, ((12 * bits / sample_rate + padding) * 4) as usize),
        2 => (1152, (144 * bits / sample_rate + padding) as usize),
        _ => {
            let spf = if mpeg1 { 1152 } else { 576 };
            let coef = spf / 8; // 144 (MPEG-1) or 72 (MPEG-2/2.5)
            (spf, (coef * bits / sample_rate + padding) as usize)
        }
    };
    let channels = if chan_mode == 3 { 1 } else { 2 };
    Some(Frame {
        sample_rate,
        channels,
        spf,
        len,
        bitrate_bps: bits,
    })
}

/// Read exactly `buf.len()` bytes; `Ok(true)` if filled, `Ok(false)` if EOF
/// reached before filling.
fn fill_or_eof<S: Source>(src: &mut S, buf: &mut [u8]) -> Result<bool, DecodeError> {
    let mut n = 0;
    while n < buf.len() {
        match src.read(&mut buf[n..]) {
            Ok(0) => return Ok(false),
            Ok(k) => n += k,
            Err(_) => return Err(DecodeError::Io),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceSource;

    #[test]
    fn parse_lame_delays_reads_recognized_tag_and_rejects_others() {
        // A LAME/Lavc info tag: 9-byte vendor, then the delays field at offset
        // 21 (delay 12 bits | padding 12 bits). 0x24,0x03,0xC0 → delay 0x240 =
        // 576, padding 0x3C0 = 960.
        let mut buf = [0u8; 40];
        buf[0..4].copy_from_slice(b"LAME");
        buf[21] = 0x24;
        buf[22] = 0x03;
        buf[23] = 0xC0;
        assert_eq!(parse_lame_delays(&buf, 0), (576, 960));
        buf[0..4].copy_from_slice(b"Lavc");
        assert_eq!(parse_lame_delays(&buf, 0), (576, 960));

        // Unknown vendor → no gapless info → (0, 0) (no spurious values from a
        // bare Xing tag or non-LAME encoder).
        buf[0..4].copy_from_slice(b"XXXX");
        assert_eq!(parse_lame_delays(&buf, 0), (0, 0));
        // Out of range (frame too short for the 24-byte tag) → (0, 0).
        assert_eq!(parse_lame_delays(&buf, 20), (0, 0));
    }

    /// Build a 4-byte MPEG-1 Layer III header (stereo unless `mono`).
    fn header_v1(br_idx: u8, sr_idx: u8, padding: bool, mono: bool) -> [u8; 4] {
        let b1 = 0xE0 | (0b11 << 3) | (0b01 << 1); // sync tail, version 1, layer III, prot=1
        let b2 = (br_idx << 4) | (sr_idx << 2) | (u8::from(padding) << 1);
        let b3 = if mono { 0b11 << 6 } else { 0 };
        [0xFF, b1, b2, b3]
    }

    fn header_v2(br_idx: u8, sr_idx: u8) -> [u8; 4] {
        let b1 = 0xE0 | (0b10 << 3) | (0b01 << 1); // version 2, layer III
        let b2 = (br_idx << 4) | (sr_idx << 2);
        [0xFF, b1, b2, 0]
    }

    #[test]
    fn parses_mpeg1_l3_stream_info() {
        // 128 kbps (idx 9), 44100 (idx 0), stereo.
        let h = header_v1(9, 0, false, false);
        let mut bytes = h.to_vec();
        bytes.resize(2048, 0); // frame body padding
        let d = Mp3Demuxer::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(d.stream_info().sample_rate, 44_100);
        assert_eq!(d.stream_info().channels, 2);
        assert_eq!(d.stream_info().format, SampleFormat::Int { bits: 16 });
    }

    #[test]
    fn frame_len_uses_version_specific_coefficient() {
        // MPEG-1 128k/44100: 144*128000/44100 = 417 bytes.
        let h1 = header_v1(9, 0, false, false);
        let mut b1 = h1.to_vec();
        b1.resize(1024, 0);
        let mut d1 = Mp3Demuxer::new(SliceSource::new(&b1)).unwrap();
        let mut scratch = [0u8; 2048];
        let p1 = d1.next_packet(&mut scratch).unwrap().unwrap();
        assert_eq!(p1.data.len(), 417);
        assert_eq!(p1.dur, 1152);

        // MPEG-2 64k/22050: 72*64000/22050 = 208 bytes, spf 576.
        let h2 = header_v2(8, 0); // 64 kbps (idx 8 in V2 table), 22050 (idx 0)
        let mut b2 = h2.to_vec();
        b2.resize(1024, 0);
        let mut d2 = Mp3Demuxer::new(SliceSource::new(&b2)).unwrap();
        assert_eq!(d2.stream_info().sample_rate, 22_050);
        let p2 = d2.next_packet(&mut scratch).unwrap().unwrap();
        assert_eq!(p2.data.len(), 208);
        assert_eq!(p2.dur, 576);
    }

    #[test]
    fn mono_header_reports_one_channel() {
        let h = header_v1(9, 0, false, true);
        let mut bytes = h.to_vec();
        bytes.resize(1024, 0);
        let d = Mp3Demuxer::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(d.stream_info().channels, 1);
    }

    #[test]
    fn skips_id3v2_and_resyncs_past_garbage() {
        let mut bytes = Vec::new();
        // ID3v2 header + 4-byte payload (syncsafe size = 4).
        bytes.extend_from_slice(b"ID3");
        bytes.extend_from_slice(&[0x04, 0x00, 0x00]);
        bytes.extend_from_slice(&[0, 0, 0, 4]);
        bytes.extend_from_slice(&[0xAA; 4]);
        // Some non-sync garbage before the first frame.
        bytes.extend_from_slice(&[0x00, 0x13, 0x37]);
        bytes.extend_from_slice(&header_v1(9, 0, false, false));
        bytes.resize(bytes.len() + 512, 0);
        let d = Mp3Demuxer::new(SliceSource::new(&bytes)).unwrap();
        assert_eq!(d.stream_info().sample_rate, 44_100);
    }

    #[test]
    fn ts_advances_by_samples_per_frame() {
        let h = header_v1(9, 0, false, false);
        let mut bytes = h.to_vec();
        // two frames back-to-back: 417 bytes each
        bytes.resize(417, 0);
        bytes.extend_from_slice(&h);
        bytes.resize(417 * 2, 0);
        let mut d = Mp3Demuxer::new(SliceSource::new(&bytes)).unwrap();
        let mut scratch = [0u8; 2048];
        let p0 = d.next_packet(&mut scratch).unwrap().unwrap();
        assert_eq!(p0.ts, 0);
        let p1 = d.next_packet(&mut scratch).unwrap().unwrap();
        assert_eq!(p1.ts, 1152);
    }
}
