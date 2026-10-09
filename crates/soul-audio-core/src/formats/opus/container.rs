//! RFC 7845 Ogg-Opus container headers: the `OpusHead` identification packet
//! (§5.1) this module parses, and `OpusTags` (§5.2, parsed-and-skipped by
//! [`super::OpusDecoder::new`] — its vendor/comment strings carry no field the
//! decoder needs).

use crate::decoder::DecodeError;

/// On-wire byte length of the fixed `OpusHead` prefix:
/// `"OpusHead"`(8) + version(1) + channels(1) + pre_skip(2) + input rate(4) +
/// output_gain(2) + mapping_family(1) = 19. Mapping family != 0 appends a
/// channel-mapping table (`stream_count` + `coupled_count` + one byte per
/// output channel), parsed by [`OpusHead::parse`] for the multistream decoder.
const HEAD_LEN: usize = 19;

/// Maximum output channels this crate decodes (7.1). Also bounds the
/// mapping-table arrays. Opus mapping family 1 covers 1..=8 channels.
pub const MAX_CHANNELS: usize = 8;

/// Parsed fields of the RFC 7845 §5.1 `OpusHead` identification packet — the
/// first Ogg packet of an Opus logical stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpusHead {
    /// Number of audio (output) channels (`> 0`).
    pub channels: u8,
    /// Number of decoded samples (at 48 kHz) to discard from the start of the
    /// stream — the encoder's priming samples.
    pub pre_skip: u16,
    /// Output gain in Q7.8 dB, applied by the decoder (`gain_dB = value/256`).
    pub output_gain_q8: i16,
    /// Channel mapping family (`0` = mono/stereo, no on-wire table; `1`/`255` =
    /// a channel-mapping table follows — see the `stream_*`/`channel_mapping`
    /// fields, synthesized for family 0).
    pub mapping_family: u8,
    /// Number of Opus streams in each packet (RFC 7845 §5.1.1, `>= 1`). The
    /// first `coupled_count` are stereo, the rest mono. Family 0 → `1`.
    pub stream_count: u8,
    /// Number of the streams that are stereo (`<= stream_count`). The total
    /// decoded channel count is `stream_count + coupled_count`. Family 0 →
    /// `channels - 1` (0 for mono, 1 for stereo).
    pub coupled_count: u8,
    /// `channel_mapping[c]` = index of the decoded channel feeding output
    /// channel `c` (`255` = a silent channel). Family 0 → identity (`[0]` /
    /// `[0, 1]`). Only `channels` entries are meaningful.
    pub channel_mapping: [u8; MAX_CHANNELS],
}

impl OpusHead {
    /// The fixed 8-byte identification magic.
    pub const MAGIC: &'static [u8; 8] = b"OpusHead";

    /// Parse an `OpusHead` packet (RFC 7845 §5.1): magic, version, channel
    /// count, pre-skip, output gain, mapping family, and — for family `1`/`255`
    /// — the channel-mapping table (`stream_count`, `coupled_count`, and one
    /// decoded-channel index per output channel). Family `0` synthesizes the
    /// equivalent single-stream mapping (identity, mono = 1 stream / stereo = 1
    /// coupled stream).
    ///
    /// # Errors
    /// [`DecodeError::UnexpectedEof`] if `id_packet` is shorter than the fixed
    /// 19-byte prefix (or the declared mapping table). [`DecodeError::Malformed`]
    /// if the magic doesn't match, `channels` is zero, or a family-1/255 table
    /// is inconsistent (`stream_count == 0`, `coupled_count > stream_count`, or
    /// a mapping index past the decoded-channel count and not the `255` silent
    /// marker). [`DecodeError::Unsupported`] if the major version (high nibble)
    /// is nonzero (RFC 7845 reserves it), family 0 has more than 2 channels, or
    /// a family-1/255 layout exceeds this decoder's [`MAX_CHANNELS`] / stream
    /// caps; a nonzero *minor* version is accepted leniently.
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

        let mut channel_mapping = [0u8; MAX_CHANNELS];
        let (stream_count, coupled_count) = if mapping_family == 0 {
            // Family 0: no on-wire table. Mono = 1 stream / stereo = 1 coupled
            // stream, identity mapping. RFC 7845 limits family 0 to 1-2 ch.
            if channels > 2 {
                return Err(DecodeError::Unsupported);
            }
            for (c, m) in channel_mapping
                .iter_mut()
                .take(channels as usize)
                .enumerate()
            {
                *m = c as u8;
            }
            (1u8, channels - 1)
        } else {
            // Family 1 (Vorbis order) / 255 (identity): a `stream_count` +
            // `coupled_count` + `channels`-byte mapping table follows byte 19.
            let ch = channels as usize;
            if ch > MAX_CHANNELS {
                return Err(DecodeError::Unsupported);
            }
            let table_end = HEAD_LEN + 2 + ch;
            if id_packet.len() < table_end {
                return Err(DecodeError::UnexpectedEof);
            }
            let stream_count = id_packet[19];
            let coupled_count = id_packet[20];
            if stream_count == 0 || coupled_count > stream_count {
                return Err(DecodeError::Malformed);
            }
            // Decoded-channel count = mono-equivalent streams + 2 per coupled.
            let decoded = u16::from(stream_count) + u16::from(coupled_count);
            for (c, m) in channel_mapping.iter_mut().take(ch).enumerate() {
                let idx = id_packet[HEAD_LEN + 2 + c];
                // Each index selects a decoded channel, or 255 = silent.
                if idx != 255 && u16::from(idx) >= decoded {
                    return Err(DecodeError::Malformed);
                }
                *m = idx;
            }
            (stream_count, coupled_count)
        };

        Ok(Self {
            channels,
            pre_skip,
            output_gain_q8,
            mapping_family,
            stream_count,
            coupled_count,
            channel_mapping,
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
