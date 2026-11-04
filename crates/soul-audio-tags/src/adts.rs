//! Bare ADTS (`.aac`) facts, plus tags from an optional leading ID3v2. A raw
//! AAC elementary stream has no metadata container of its own, but iTunes/FAAC
//! and similar taggers prepend an ID3v2 tag (the same TIT2/TPE1/TALB frames as
//! MP3) — so this reads that tag if present, then frame-scans the 7-byte ADTS
//! headers (advancing by the 13-bit `aac_frame_length`, 1024 PCM samples per
//! frame) for a duration. Mirrors the header fields
//! `soul_audio_core::demux::adts` parses.

use crate::id3v2;
use crate::util::read_exact;
use crate::{StreamFacts, TagError, TagSink};
use embedded_io::SeekFrom;
use soul_audio_core::SeekSource;

/// `sampling_frequency_index` → Hz (reserved indices `>= 13` are rejected).
const AAC_SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// Parse a 7-byte ADTS header → (core sample rate, channels, whole-frame length
/// in bytes). `Err` on a bad syncword/layer or a reserved sample-rate index.
fn parse_header(h: &[u8; 7]) -> Result<(u32, u16, usize), TagError> {
    // syncword(12)==0xFFF and layer(2)==00 (0x06 mask); layer!=00 is an MP3 frame.
    if h[0] != 0xFF || (h[1] & 0xF6) != 0xF0 {
        return Err(TagError::Malformed);
    }
    let sfi = usize::from((h[2] >> 2) & 0xF);
    let rate = *AAC_SAMPLE_RATES.get(sfi).ok_or(TagError::Malformed)?;
    let chan_cfg = ((h[2] & 0x1) << 2) | ((h[3] >> 6) & 0x3);
    let channels = match chan_cfg {
        0 => 2, // "from PCE" — assume stereo for a facts estimate
        7 => 8, // 7.1
        c => u16::from(c),
    };
    let frame_len = (usize::from(h[3] & 0x03) << 11)
        | (usize::from(h[4]) << 3)
        | (usize::from(h[5] >> 5) & 0x07);
    Ok((rate, channels, frame_len))
}

/// If `src` starts with an ID3v2 tag, read it (title/artist/album) and leave the
/// cursor at the first byte past it; otherwise rewind to the start. Lets a
/// tagged `.aac` surface its tags while the frame-scan below still runs over the
/// ADTS frames beneath the tag.
fn read_leading_id3<S: SeekSource>(
    src: &mut S,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    let mut magic = [0u8; 3];
    read_exact(src, &mut magic)?;
    if &magic == b"ID3" {
        let mut rest = [0u8; 7]; // version(2) flags(1) size(4, synchsafe)
        read_exact(src, &mut rest)?;
        let size = (u32::from(rest[3] & 0x7F) << 21)
            | (u32::from(rest[4] & 0x7F) << 14)
            | (u32::from(rest[5] & 0x7F) << 7)
            | u32::from(rest[6] & 0x7F);
        src.seek(SeekFrom::Start(0)).map_err(|_| TagError::Io)?;
        id3v2::read_id3v2(src, scratch, sink)?;
        src.seek(SeekFrom::Start(10 + u64::from(size)))
            .map_err(|_| TagError::Io)?;
    } else {
        src.seek(SeekFrom::Start(0)).map_err(|_| TagError::Io)?;
    }
    Ok(())
}

/// Read an optional leading ID3v2 tag, then frame-scan the ADTS stream for
/// duration facts.
pub fn read_adts<S: SeekSource>(
    src: &mut S,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<StreamFacts, TagError> {
    read_leading_id3(src, scratch, sink)?;
    let mut h = [0u8; 7];
    read_exact(src, &mut h)?; // at least one frame, else genuinely malformed
    let (rate, channels, first_len) = parse_header(&h)?;

    let mut frames: u64 = 0;
    let mut frame_len = first_len;
    loop {
        frames += 1;
        if frame_len < 7 {
            break; // guards the skip; a <7 frame_len is malformed/last
        }
        // Advance from just-past-this-header to the next frame start.
        if src.seek(SeekFrom::Current((frame_len - 7) as i64)).is_err() {
            break;
        }
        // Read the next header; a clean EOF (0 bytes) ends the scan.
        match read_up_to_7(src, &mut h) {
            Ok(7) => match parse_header(&h) {
                Ok((_, _, len)) => frame_len = len,
                Err(_) => break, // resync garbage at the tail — stop, keep count
            },
            _ => break,
        }
    }

    Ok(StreamFacts {
        sample_rate: Some(rate),
        channels: Some(channels),
        total_frames: Some(frames * 1024),
    })
}

/// Read up to 7 bytes; returns how many were read (0 at clean EOF).
fn read_up_to_7<S: SeekSource>(src: &mut S, buf: &mut [u8; 7]) -> Result<usize, TagError> {
    let mut off = 0;
    while off < buf.len() {
        match embedded_io::Read::read(src, &mut buf[off..]).map_err(|_| TagError::Io)? {
            0 => break,
            k => off += k,
        }
    }
    Ok(off)
}

#[cfg(test)]
mod tests {
    use soul_audio_core::SliceSource;
    extern crate alloc;
    use alloc::vec::Vec;

    /// Build `n` ADTS frames, each a 7-byte header + `payload` bytes, at 44.1 kHz
    /// stereo (sfi=4, channel_config=2). frame_len = 7 + payload.
    fn adts(n: usize, payload: usize) -> Vec<u8> {
        let frame_len = (7 + payload) as u32;
        let mut v = Vec::new();
        for _ in 0..n {
            v.push(0xFF); // sync hi
            v.push(0xF1); // sync lo + layer 00 + protection_absent=1
            v.push(0x50); // profile=01(LC) | sfi=0100(4) | priv=0 | chan_cfg hi=0
            v.push(0x80 | ((frame_len >> 11) & 0x03) as u8); // chan_cfg lo=10 | fl[12:11]
            v.push(((frame_len >> 3) & 0xFF) as u8); // fl[10:3]
            v.push((((frame_len & 0x07) << 5) as u8) | 0x1F); // fl[2:0] | buffer-fullness hi
            v.push(0xFC); // buffer-fullness lo | num_raw_blocks=0
            v.extend(core::iter::repeat_n(0u8, payload));
        }
        v
    }

    #[test]
    fn frame_scan_reports_duration_facts() {
        // 86 frames × 1024 / 44100 ≈ 1.997 s.
        let bytes = adts(86, 100);
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 16];
        let mut sink = crate::testfix::Collect::default();
        let facts = super::read_adts(&mut src, &mut scratch, &mut sink).unwrap();
        assert_eq!(facts.sample_rate, Some(44_100));
        assert_eq!(facts.channels, Some(2));
        assert_eq!(facts.total_frames, Some(86 * 1024));
        assert!(sink.fields.is_empty()); // raw ADTS carries no tags
    }

    #[test]
    fn empty_or_nonadts_is_malformed() {
        let mut src = SliceSource::new(b"\0\0\0\0\0\0\0");
        let mut scratch = [0u8; 16];
        let mut sink = crate::testfix::Collect::default();
        assert_eq!(
            super::read_adts(&mut src, &mut scratch, &mut sink),
            Err(crate::TagError::Malformed)
        );
    }
}
