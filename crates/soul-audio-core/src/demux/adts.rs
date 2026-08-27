//! ADTS (`.aac`) frame reader — parses the 7/9-byte ADTS header (raw AAC
//! elementary-stream framing, as distinct from the MP4 `esds`/`mp4a`
//! container `[super::mp4]` handles) and iterates `raw_data_block` payloads
//! one AAC frame at a time. AAC-LC codec decode (Huffman/IMDCT/etc.) is a
//! later slice; this module only demuxes frame boundaries and exposes the
//! stream's [`AacConfig`].

use super::{SAMPLE_RATES, channels_for, read_exact, skip};
use crate::decoder::DecodeError;
use crate::io::Source;

/// The subset of `AudioSpecificConfig` an ADTS header (or an MP4 `esds`, in a
/// later task) carries — enough to construct an AAC-LC decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AacConfig {
    /// MPEG-4 audio object type (LC = 2 = `profile + 1`, per the ADTS 2-bit
    /// `profile` field).
    pub object_type: u8,
    /// PCM frame rate in Hz, from `sampling_frequency_index`.
    pub sample_rate: u32,
    /// Channel count, from the 3-bit `channel_configuration`.
    pub channels: u8,
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
        },
        header_len,
        frame_len,
    })
}

/// Read+validate the very first header of the stream. Unlike
/// [`read_next_header`], an empty/too-short source has no frame at all, so any
/// short read here is a genuine [`DecodeError::UnexpectedEof`], never a clean
/// end of stream.
fn read_first_header<S: Source>(src: &mut S) -> Result<AdtsHeader, DecodeError> {
    let mut h = [0u8; 7];
    read_exact(src, &mut h)?;
    let parsed = parse_header(&h)?;
    if parsed.header_len == 9 {
        skip(src, 2)?; // discard the 16-bit CRC
    }
    Ok(parsed)
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
    /// `Some` right after [`Self::new`] (the first frame's header). Once that
    /// payload is delivered, [`Self::next_frame`] goes back to reading a
    /// header itself, so this is only ever populated by construction.
    pending: Option<usize>,
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
        let header = read_first_header(&mut src)?;
        let payload_len = header.frame_len - header.header_len;
        Ok(Self {
            src,
            config: header.config,
            pending: Some(payload_len),
        })
    }

    /// The stream's AAC parameters (object type, sample rate, channels).
    #[must_use]
    pub fn config(&self) -> AacConfig {
        self.config
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
}
