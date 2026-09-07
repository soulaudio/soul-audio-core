//! RFC 7845 Ogg-Opus container headers: the `OpusHead` identification packet
//! (§5.1) this module parses, and `OpusTags` (§5.2, parsed-and-skipped by
//! [`super::OpusDecoder::new`] — its vendor/comment strings carry no field the
//! decoder needs).

use crate::decoder::DecodeError;

/// On-wire byte length of the fixed `OpusHead` prefix this decoder reads:
/// `"OpusHead"`(8) + version(1) + channels(1) + pre_skip(2) + input rate(4) +
/// output_gain(2) + mapping_family(1) = 19. A conforming packet may be longer
/// (mapping family != 0 appends a channel-mapping table), but nothing past
/// byte 19 is needed here.
const HEAD_LEN: usize = 19;

/// Parsed fields of the RFC 7845 §5.1 `OpusHead` identification packet — the
/// first Ogg packet of an Opus logical stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpusHead {
    /// Number of audio channels (`> 0`).
    pub channels: u8,
    /// Number of decoded samples (at 48 kHz) to discard from the start of the
    /// stream — the encoder's priming samples.
    pub pre_skip: u16,
    /// Output gain in Q7.8 dB, applied by the decoder (`gain_dB = value/256`).
    pub output_gain_q8: i16,
    /// Channel mapping family (`0` = mono/stereo, no extra table; `1`/`255` =
    /// a channel-mapping table follows, not parsed by this skeleton).
    pub mapping_family: u8,
}

impl OpusHead {
    /// The fixed 8-byte identification magic.
    pub const MAGIC: &'static [u8; 8] = b"OpusHead";

    /// Parse an `OpusHead` packet (RFC 7845 §5.1): magic, version, channel
    /// count, pre-skip, output gain, and mapping family. The (optional,
    /// family-dependent) channel-mapping table past byte 19 is not parsed —
    /// this skeleton only needs the fixed prefix.
    ///
    /// # Errors
    /// [`DecodeError::UnexpectedEof`] if `id_packet` is shorter than the fixed
    /// 19-byte prefix. [`DecodeError::Malformed`] if the magic doesn't match
    /// or `channels` is zero. [`DecodeError::Unsupported`] if the major
    /// version (the high nibble of the version byte) is nonzero — RFC 7845
    /// reserves that for a future incompatible revision; a nonzero *minor*
    /// version (low nibble) is accepted leniently, per the RFC's forward
    /// compatibility guidance.
    pub fn parse(id_packet: &[u8]) -> Result<Self, DecodeError> {
        if id_packet.len() < HEAD_LEN {
            return Err(DecodeError::UnexpectedEof);
        }
        if &id_packet[0..8] != Self::MAGIC {
            return Err(DecodeError::Malformed);
        }
        let version = id_packet[8];
        if version & 0xF0 != 0 {
            return Err(DecodeError::Unsupported);
        }
        let channels = id_packet[9];
        if channels == 0 {
            return Err(DecodeError::Malformed);
        }
        let pre_skip = u16::from_le_bytes([id_packet[10], id_packet[11]]);
        // input_sample_rate @12 (4 bytes, LE u32) is informational only (the
        // encoder's original input rate) — Opus always decodes at 48 kHz
        // regardless, so it is intentionally not read.
        let output_gain_q8 = i16::from_le_bytes([id_packet[16], id_packet[17]]);
        let mapping_family = id_packet[18];
        Ok(Self {
            channels,
            pre_skip,
            output_gain_q8,
            mapping_family,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal, spec-sized `OpusHead` packet (RFC 7845 §5.1) with
    /// mapping family 0 (no channel-mapping table).
    fn opus_head_packet(channels: u8, pre_skip: u16, gain_q8: i16) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"OpusHead");
        v.push(1); // version
        v.push(channels);
        v.extend_from_slice(&pre_skip.to_le_bytes());
        v.extend_from_slice(&48_000u32.to_le_bytes()); // input_sample_rate (informational)
        v.extend_from_slice(&gain_q8.to_le_bytes());
        v.push(0); // mapping_family
        v
    }

    #[test]
    fn parses_channels_pre_skip_and_gain() {
        let packet = opus_head_packet(2, 312, -256);
        let head = OpusHead::parse(&packet).unwrap();
        assert_eq!(head.channels, 2);
        assert_eq!(head.pre_skip, 312);
        assert_eq!(head.output_gain_q8, -256);
        assert_eq!(head.mapping_family, 0);
    }

    #[test]
    fn rejects_wrong_magic() {
        let mut packet = opus_head_packet(1, 0, 0);
        packet[0] = b'X';
        assert_eq!(
            OpusHead::parse(&packet).unwrap_err(),
            DecodeError::Malformed
        );
    }

    #[test]
    fn rejects_zero_channels() {
        let packet = opus_head_packet(0, 0, 0);
        assert_eq!(
            OpusHead::parse(&packet).unwrap_err(),
            DecodeError::Malformed
        );
    }

    #[test]
    fn rejects_future_major_version() {
        let mut packet = opus_head_packet(1, 0, 0);
        packet[8] = 0x10; // major version 1 (high nibble) — a future revision
        assert_eq!(
            OpusHead::parse(&packet).unwrap_err(),
            DecodeError::Unsupported
        );
    }

    #[test]
    fn accepts_lenient_minor_version_bump() {
        let mut packet = opus_head_packet(1, 0, 0);
        packet[8] = 0x0F; // major 0, minor 15 — RFC says accept leniently
        assert!(OpusHead::parse(&packet).is_ok());
    }

    #[test]
    fn truncated_packet_is_unexpected_eof() {
        let packet = opus_head_packet(2, 312, 0);
        assert_eq!(
            OpusHead::parse(&packet[..10]).unwrap_err(),
            DecodeError::UnexpectedEof
        );
    }

    /// The real fixture's identification packet (`testdata/tone.opus`,
    /// extracted from its BOS page): stereo, pre-skip 312, 48 kHz input,
    /// zero gain, mapping family 0.
    #[test]
    fn parses_the_real_tone_opus_header() {
        let mut packet = Vec::new();
        packet.extend_from_slice(b"OpusHead");
        packet.push(1);
        packet.push(2);
        packet.extend_from_slice(&312u16.to_le_bytes());
        packet.extend_from_slice(&48_000u32.to_le_bytes());
        packet.extend_from_slice(&0i16.to_le_bytes());
        packet.push(0);
        let head = OpusHead::parse(&packet).unwrap();
        assert_eq!(head.channels, 2);
        assert_eq!(head.pre_skip, 312);
    }
}
