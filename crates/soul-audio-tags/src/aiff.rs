//! AIFF / AIFF-C (`FORM`…`AIFF`/`AIFC`) container: big-endian IFF chunks.
//! `COMM` yields [`StreamFacts`] (channels, sample-frame count, and the
//! 80-bit extended-float sample rate); `NAME`/`AUTH`/`(c) ` carry text fields,
//! and an embedded `ID3 ` chunk (if present) is delegated to
//! [`crate::id3v2::read_id3v2`]. Mirrors [`crate::riff`], but AIFF is
//! big-endian and its text lives in bespoke chunks rather than a `LIST`.

use crate::id3v2;
use crate::locate;
use crate::util::read_exact;
use crate::{Field, StreamFacts, TagError, TagSink};
use embedded_io::SeekFrom;
use soul_audio_core::SeekSource;

/// Walk an AIFF/AIFF-C container's top-level chunks.
pub fn read_aiff<S: SeekSource>(
    src: &mut S,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<StreamFacts, TagError> {
    let mut form = [0u8; 12];
    read_exact(src, &mut form)?;
    if &form[0..4] != b"FORM" || (&form[8..12] != b"AIFF" && &form[8..12] != b"AIFC") {
        return Err(TagError::Malformed);
    }

    let mut facts = StreamFacts::default();

    loop {
        let mut hdr = [0u8; 8];
        let n = locate::read_full(src, &mut hdr)?;
        if n == 0 {
            break; // clean EOF: no more chunks
        }
        if n < 8 {
            return Err(TagError::Malformed); // truncated chunk header
        }

        let id = [hdr[0], hdr[1], hdr[2], hdr[3]];
        let size = u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]); // IFF is big-endian
        let body_start = src.seek(SeekFrom::Current(0)).map_err(|_| TagError::Io)?;
        let body_end = body_start
            .checked_add(u64::from(size))
            .ok_or(TagError::Malformed)?;

        if &id == b"COMM" {
            // numChannels(2) numSampleFrames(4) sampleSize(2) sampleRate(10,
            // 80-bit IEEE-754 extended) — all big-endian.
            if size >= 18 {
                let mut comm = [0u8; 18];
                read_exact(src, &mut comm)?;
                facts.channels = Some(u16::from_be_bytes([comm[0], comm[1]]));
                facts.total_frames = Some(u64::from(u32::from_be_bytes([
                    comm[2], comm[3], comm[4], comm[5],
                ])));
                let ext: [u8; 10] = comm[8..18].try_into().unwrap();
                if let Some(rate) = extended_to_u32(&ext) {
                    facts.sample_rate = Some(rate);
                }
            }
        } else if &id == b"NAME" || &id == b"AUTH" || &id == b"(c) " || &id == b"ANNO" {
            read_text_chunk(src, &id, size, scratch, sink)?;
        } else if &id == b"ID3 " || &id == b"id3 " {
            // Cursor is at the chunk body start = the ID3v2 tag start, same as
            // the WAV path; `read_id3v2` reads picture offsets from there.
            id3v2::read_id3v2(src, scratch, sink)?;
        }

        // Re-sync to the chunk's declared end (IFF pads odd sizes to even),
        // regardless of how many bytes the handler above consumed.
        let padded_end = if size % 2 == 1 {
            body_end + 1
        } else {
            body_end
        };
        src.seek(SeekFrom::Start(padded_end))
            .map_err(|_| TagError::Io)?;
    }

    Ok(facts)
}

/// Read an AIFF text chunk (`NAME`/`AUTH`/`(c) `/`ANNO`) and emit it. The value
/// is the chunk body (not NUL-terminated, but some writers append one, so trim
/// a trailing NUL run). Only the leading `scratch.len()` bytes are materialized;
/// the caller re-syncs past any remainder.
fn read_text_chunk<S: SeekSource>(
    src: &mut S,
    id: &[u8; 4],
    size: u32,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    let want = (size as usize).min(scratch.len());
    read_exact(src, &mut scratch[..want])?;
    let end = scratch[..want].iter().position(|&b| b == 0).unwrap_or(want);
    if let Ok(s) = core::str::from_utf8(&scratch[..end]) {
        let s = s.trim();
        if !s.is_empty() {
            let field = match id {
                b"NAME" => Some(Field::Title),
                b"AUTH" => Some(Field::Artist),
                _ => None, // (c) copyright / ANNO annotation have no Field
            };
            let raw = core::str::from_utf8(id).unwrap_or("");
            sink.text(field, raw, s);
        }
    }
    Ok(())
}

/// Convert an AIFF `COMM` 80-bit IEEE-754 extended float (sign · 15-bit biased
/// exponent · 64-bit mantissa with an explicit integer bit) to a `u32`. Audio
/// sample rates are integers represented exactly, so truncation is exact.
/// Returns `None` for a value that doesn't fit a `u32` (bias-removed exponent
/// outside `0..=63`) — leaving the sample rate unknown rather than wrong.
fn extended_to_u32(ext: &[u8; 10]) -> Option<u32> {
    let exponent = i32::from((u16::from(ext[0] & 0x7F) << 8) | u16::from(ext[1]));
    let mantissa = u64::from_be_bytes([
        ext[2], ext[3], ext[4], ext[5], ext[6], ext[7], ext[8], ext[9],
    ]);
    if exponent == 0 && mantissa == 0 {
        return Some(0);
    }
    let unbiased = exponent - 16383;
    let shift = 63 - unbiased; // mantissa's integer bit sits at bit 63
    if !(0..=63).contains(&shift) {
        return None;
    }
    Some((mantissa >> shift) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testfix::Collect;
    use soul_audio_core::SliceSource;
    extern crate alloc;
    use alloc::vec::Vec;

    /// 80-bit extended encoding of an integer sample rate (sign 0, normalized).
    fn extended(rate: u32) -> [u8; 10] {
        let mut out = [0u8; 10];
        if rate == 0 {
            return out;
        }
        // Normalize so the integer bit (bit 63) is set: find the top set bit.
        let top = 31 - rate.leading_zeros(); // position of MSB in `rate`
        let mantissa = u64::from(rate) << (63 - top);
        let exponent = (top as i32 + 16383) as u16;
        out[0] = (exponent >> 8) as u8;
        out[1] = exponent as u8;
        out[2..10].copy_from_slice(&mantissa.to_be_bytes());
        out
    }

    fn aiff(chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (id, data) in chunks {
            body.extend_from_slice(*id);
            body.extend_from_slice(&(data.len() as u32).to_be_bytes());
            body.extend_from_slice(data);
            if data.len() % 2 == 1 {
                body.push(0); // IFF even-padding
            }
        }
        let mut v = Vec::new();
        v.extend_from_slice(b"FORM");
        v.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
        v.extend_from_slice(b"AIFF");
        v.extend_from_slice(&body);
        v
    }

    fn comm(channels: u16, frames: u32, rate: u32) -> Vec<u8> {
        let mut c = Vec::new();
        c.extend_from_slice(&channels.to_be_bytes());
        c.extend_from_slice(&frames.to_be_bytes());
        c.extend_from_slice(&16u16.to_be_bytes()); // sample size
        c.extend_from_slice(&extended(rate));
        c
    }

    #[test]
    fn extended_round_trips_common_rates() {
        for &r in &[8_000u32, 44_100, 48_000, 96_000, 192_000] {
            assert_eq!(extended_to_u32(&extended(r)), Some(r));
        }
    }

    #[test]
    fn comm_gives_facts_and_name_gives_title() {
        let bytes = aiff(&[
            (b"COMM", comm(2, 132_300, 44_100)),
            (b"NAME", b"Aria\0".to_vec()),
            (b"AUTH", b"Kimiko".to_vec()),
            (b"(c) ", b"CC0".to_vec()),
            (b"SSND", alloc::vec![0u8; 8]),
        ]);
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = read_aiff(&mut src, &mut scratch, &mut c).unwrap();

        assert_eq!(facts.channels, Some(2));
        assert_eq!(facts.sample_rate, Some(44_100));
        assert_eq!(facts.total_frames, Some(132_300));
        assert!(
            c.fields
                .contains(&(Some(Field::Title), "NAME".into(), "Aria".into()))
        );
        assert!(
            c.fields
                .contains(&(Some(Field::Artist), "AUTH".into(), "Kimiko".into()))
        );
    }

    #[test]
    fn rejects_non_aiff_form() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"FORM");
        bytes.extend_from_slice(&4u32.to_be_bytes());
        bytes.extend_from_slice(b"AVI "); // not AIFF/AIFC
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 64];
        let mut c = Collect::default();
        assert_eq!(
            read_aiff(&mut src, &mut scratch, &mut c).unwrap_err(),
            TagError::Malformed
        );
    }
}
