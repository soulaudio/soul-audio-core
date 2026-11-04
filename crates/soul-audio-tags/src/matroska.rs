//! Matroska / WebM (`.mka`/`.webm`): an EBML walk for `\Segment\Info`
//! (TimecodeScale + Duration + Title) and `\Segment\Tags` (SimpleTag
//! TagName/TagString → artist/album/…). Duration is reported MP4-style —
//! `total_frames` = the raw `Duration`, `sample_rate` = `1e9 / TimecodeScale` —
//! so `total_frames / sample_rate` is seconds. The core demuxer
//! [`soul_audio_core::demux::matroska`] handles decoding; this reads metadata.

use crate::util::read_exact;
use crate::{Field, StreamFacts, TagError, TagSink};
use embedded_io::SeekFrom;
use soul_audio_core::SeekSource;

const SEGMENT: u64 = 0x1853_8067;
const INFO: u64 = 0x1549_A966;
const TIMECODE_SCALE: u64 = 0x2AD7B1;
const DURATION: u64 = 0x4489;
const TITLE: u64 = 0x7BA9;
const TAGS: u64 = 0x1254_C367;
const TAG: u64 = 0x7373;
const SIMPLE_TAG: u64 = 0x67C8;
const TAG_NAME: u64 = 0x45A3;
const TAG_STRING: u64 = 0x4487;

/// Read an EBML element header (ID + data size) at the cursor. Returns the ID,
/// the absolute `[body_start, body_end)` of its data, and whether the size was
/// "unknown" (all-ones vint, i.e. extends to its parent's end). `Ok(None)` at a
/// clean EOF.
fn read_header<S: SeekSource>(
    src: &mut S,
    parent_end: u64,
) -> Result<Option<(u64, u64, u64, bool)>, TagError> {
    let pos = src.seek(SeekFrom::Current(0)).map_err(|_| TagError::Io)?;
    if pos >= parent_end {
        return Ok(None);
    }
    let mut first = [0u8; 1];
    if embedded_io::Read::read(src, &mut first).map_err(|_| TagError::Io)? == 0 {
        return Ok(None);
    }
    // Element ID: length from the leading-zero count; the marker bits are KEPT.
    let id_len = (first[0].leading_zeros() + 1) as usize;
    if id_len > 4 {
        return Err(TagError::Malformed);
    }
    let mut id = u64::from(first[0]);
    for _ in 1..id_len {
        let mut b = [0u8; 1];
        read_exact(src, &mut b)?;
        id = (id << 8) | u64::from(b[0]);
    }
    // Data size: leading-zero length, marker bit STRIPPED; all-ones = unknown.
    let mut sfirst = [0u8; 1];
    read_exact(src, &mut sfirst)?;
    let size_len = (sfirst[0].leading_zeros() + 1) as usize;
    if size_len > 8 {
        return Err(TagError::Malformed);
    }
    // Mask off the length-marker bit. Compute in a wider type: `0xFFu8 >> 8`
    // would be a shift-overflow panic when size_len == 8.
    let mask = (0xFFu16 >> size_len) as u8;
    let mut size = u64::from(sfirst[0] & mask);
    let mut all_ones = (sfirst[0] & mask) == mask;
    for _ in 1..size_len {
        let mut b = [0u8; 1];
        read_exact(src, &mut b)?;
        size = (size << 8) | u64::from(b[0]);
        all_ones &= b[0] == 0xFF;
    }

    let body_start = src.seek(SeekFrom::Current(0)).map_err(|_| TagError::Io)?;
    let body_end = if all_ones {
        parent_end // unknown size: runs to the parent's end
    } else {
        body_start.saturating_add(size).min(parent_end)
    };
    Ok(Some((id, body_start, body_end, all_ones)))
}

/// Read up to 8 big-endian bytes as a `u64` (for TimecodeScale).
fn read_uint<S: SeekSource>(src: &mut S, start: u64, end: u64) -> Result<u64, TagError> {
    let len = (end - start).min(8) as usize;
    let mut buf = [0u8; 8];
    read_exact(src, &mut buf[..len])?;
    let mut v = 0u64;
    for &b in &buf[..len] {
        v = (v << 8) | u64::from(b);
    }
    Ok(v)
}

/// Read a 4- or 8-byte big-endian IEEE float (for Duration).
fn read_float<S: SeekSource>(src: &mut S, start: u64, end: u64) -> Result<f64, TagError> {
    let len = (end - start) as usize;
    if len == 4 {
        let mut b = [0u8; 4];
        read_exact(src, &mut b)?;
        Ok(f64::from(f32::from_be_bytes(b)))
    } else if len == 8 {
        let mut b = [0u8; 8];
        read_exact(src, &mut b)?;
        Ok(f64::from_be_bytes(b))
    } else {
        Err(TagError::Malformed)
    }
}

/// Walk a Matroska/WebM container's `\Segment` for `Info` + `Tags`.
pub fn read_matroska<S: SeekSource>(
    src: &mut S,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<StreamFacts, TagError> {
    let total_len = src.seek(SeekFrom::End(0)).map_err(|_| TagError::Io)?;
    src.seek(SeekFrom::Start(0)).map_err(|_| TagError::Io)?;

    // The first element is the EBML header; skip it, then find Segment.
    let mut facts = StreamFacts::default();
    let mut timecode_scale: u64 = 1_000_000; // Matroska default (ns)
    let mut duration: Option<f64> = None;

    while let Some((id, _body_start, body_end, _unknown)) = read_header(src, total_len)? {
        if id == SEGMENT {
            walk_segment(
                src,
                body_end,
                scratch,
                sink,
                &mut timecode_scale,
                &mut duration,
            )?;
            break; // one Segment is all a tag reader needs
        }
        src.seek(SeekFrom::Start(body_end))
            .map_err(|_| TagError::Io)?;
    }

    if timecode_scale > 0 {
        facts.sample_rate = Some((1_000_000_000u64 / timecode_scale) as u32);
    }
    facts.total_frames = duration.map(|d| d as u64);
    Ok(facts)
}

fn walk_segment<S: SeekSource>(
    src: &mut S,
    end: u64,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
    timecode_scale: &mut u64,
    duration: &mut Option<f64>,
) -> Result<(), TagError> {
    while let Some((id, _body_start, body_end, unknown)) = read_header(src, end)? {
        match id {
            INFO => walk_info(src, body_end, scratch, sink, timecode_scale, duration)?,
            TAGS => walk_tags(src, body_end, scratch, sink)?,
            _ => {}
        }
        if unknown {
            break; // can't skip an unknown-size element (e.g. a streamed Cluster)
        }
        src.seek(SeekFrom::Start(body_end))
            .map_err(|_| TagError::Io)?;
    }
    Ok(())
}

fn walk_info<S: SeekSource>(
    src: &mut S,
    end: u64,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
    timecode_scale: &mut u64,
    duration: &mut Option<f64>,
) -> Result<(), TagError> {
    while let Some((id, body_start, body_end, _)) = read_header(src, end)? {
        match id {
            TIMECODE_SCALE => {
                let v = read_uint(src, body_start, body_end)?;
                if v > 0 {
                    *timecode_scale = v;
                }
            }
            DURATION => *duration = read_float(src, body_start, body_end).ok(),
            TITLE => emit_string(
                src,
                Some(Field::Title),
                "TITLE",
                body_start,
                body_end,
                scratch,
                sink,
            )?,
            _ => {}
        }
        src.seek(SeekFrom::Start(body_end))
            .map_err(|_| TagError::Io)?;
    }
    Ok(())
}

fn walk_tags<S: SeekSource>(
    src: &mut S,
    end: u64,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    while let Some((id, _body_start, body_end, _)) = read_header(src, end)? {
        if id == TAG {
            walk_tag(src, body_end, scratch, sink)?;
        }
        src.seek(SeekFrom::Start(body_end))
            .map_err(|_| TagError::Io)?;
    }
    Ok(())
}

fn walk_tag<S: SeekSource>(
    src: &mut S,
    end: u64,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    while let Some((id, _body_start, body_end, _)) = read_header(src, end)? {
        if id == SIMPLE_TAG {
            walk_simple_tag(src, body_end, scratch, sink)?;
        }
        src.seek(SeekFrom::Start(body_end))
            .map_err(|_| TagError::Io)?;
    }
    Ok(())
}

/// One `SimpleTag`: a `TagName` + `TagString`. Read the name into a small
/// buffer, map it, and if recognized emit the string value.
fn walk_simple_tag<S: SeekSource>(
    src: &mut S,
    end: u64,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    let mut name = [0u8; 32];
    let mut name_len = 0usize;
    let mut name_present = false;

    while let Some((id, body_start, body_end, _)) = read_header(src, end)? {
        if id == TAG_NAME {
            let len = (body_end - body_start).min(name.len() as u64) as usize;
            read_exact(src, &mut name[..len])?;
            name_len = len;
            name_present = true;
        } else if id == TAG_STRING && name_present {
            let field = map_name(&name[..name_len]);
            let raw = core::str::from_utf8(&name[..name_len]).unwrap_or("");
            emit_string(src, field, raw, body_start, body_end, scratch, sink)?;
        }
        src.seek(SeekFrom::Start(body_end))
            .map_err(|_| TagError::Io)?;
    }
    Ok(())
}

/// Read a UTF-8 string element body into `scratch` (bounded) and emit it.
fn emit_string<S: SeekSource>(
    src: &mut S,
    field: Option<Field>,
    raw: &str,
    start: u64,
    end: u64,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    let want = (end - start).min(scratch.len() as u64) as usize;
    read_exact(src, &mut scratch[..want])?;
    if let Ok(s) = core::str::from_utf8(&scratch[..want]) {
        let s = s.trim_end_matches('\0').trim();
        if !s.is_empty() {
            // Only emit if it maps to a field, OR it's the Info Title (raw given).
            if field.is_some() || !raw.is_empty() {
                sink.text(field, raw, s);
            }
        }
    }
    Ok(())
}

/// Map a Matroska `TagName` (case-insensitive) to a normalized [`Field`].
fn map_name(name: &[u8]) -> Option<Field> {
    if name.eq_ignore_ascii_case(b"TITLE") {
        Some(Field::Title)
    } else if name.eq_ignore_ascii_case(b"ARTIST") {
        Some(Field::Artist)
    } else if name.eq_ignore_ascii_case(b"ALBUM") {
        Some(Field::Album)
    } else if name.eq_ignore_ascii_case(b"DATE") || name.eq_ignore_ascii_case(b"DATE_RELEASED") {
        Some(Field::Date)
    } else if name.eq_ignore_ascii_case(b"GENRE") {
        Some(Field::Genre)
    } else if name.eq_ignore_ascii_case(b"PART_NUMBER") {
        Some(Field::TrackNumber)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testfix::Collect;
    use soul_audio_core::SliceSource;
    extern crate alloc;
    use alloc::vec::Vec;

    /// Encode an EBML element ID (already the full stored form) + a 1-byte or
    /// 8-byte size + body. IDs here are given as their canonical hex, emitted
    /// big-endian at their natural length.
    fn elem(id: u64, body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        // ID: emit the minimal big-endian bytes of the canonical value.
        let id_bytes = match id {
            x if x <= 0xFF => alloc::vec![x as u8],
            x if x <= 0xFFFF => x.to_be_bytes()[6..].to_vec(),
            x if x <= 0xFF_FFFF => x.to_be_bytes()[5..].to_vec(),
            x => x.to_be_bytes()[4..].to_vec(),
        };
        v.extend_from_slice(&id_bytes);
        // Size: 8-byte form (0x01 marker + 7 bytes) for simplicity.
        v.push(0x01);
        v.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
        v.extend_from_slice(body);
        v
    }

    fn simple_tag(name: &str, value: &str) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&elem(TAG_NAME, name.as_bytes()));
        body.extend_from_slice(&elem(TAG_STRING, value.as_bytes()));
        elem(SIMPLE_TAG, &body)
    }

    #[test]
    fn mkv_info_duration_and_tags() {
        // \Info: TimecodeScale=1_000_000, Duration=132300.0, Title.
        let mut info = Vec::new();
        info.extend_from_slice(&elem(TIMECODE_SCALE, &1_000_000u64.to_be_bytes()[5..])); // 3-byte uint
        info.extend_from_slice(&elem(DURATION, &132_300.0f64.to_be_bytes()));
        info.extend_from_slice(&elem(TITLE, b"Aria"));

        // \Tags: Tag { SimpleTag ARTIST, SimpleTag ALBUM }.
        let mut tag = Vec::new();
        tag.extend_from_slice(&simple_tag("ARTIST", "Kimiko Ishizaka"));
        tag.extend_from_slice(&simple_tag("ALBUM", "OGV"));
        let tags = elem(TAGS, &elem(TAG, &tag));

        let mut seg = Vec::new();
        seg.extend_from_slice(&elem(INFO, &info));
        seg.extend_from_slice(&tags);
        let segment = elem(SEGMENT, &seg);

        // EBML header (id 0x1A45DFA3) then Segment.
        let mut file = Vec::new();
        file.extend_from_slice(&elem(0x1A45_DFA3, b"\x42\x82\x84webm")); // minimal, ignored
        file.extend_from_slice(&segment);

        let mut src = SliceSource::new(&file);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = read_matroska(&mut src, &mut scratch, &mut c).unwrap();

        assert_eq!(facts.sample_rate, Some(1000)); // 1e9 / 1e6
        assert_eq!(facts.total_frames, Some(132_300)); // 132300 / 1000 = 132.3 s
        assert!(
            c.fields
                .contains(&(Some(Field::Title), "TITLE".into(), "Aria".into()))
        );
        assert!(c.fields.contains(&(
            Some(Field::Artist),
            "ARTIST".into(),
            "Kimiko Ishizaka".into()
        )));
        assert!(
            c.fields
                .contains(&(Some(Field::Album), "ALBUM".into(), "OGV".into()))
        );
    }
}
