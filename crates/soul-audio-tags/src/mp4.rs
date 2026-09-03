//! MP4/M4A `ilst` tag reader (M4A/AAC/ALAC): walks `moov`→`udta`→`meta`→`ilst`
//! for text/track/disc/cover atoms, and `moov`→`mvhd` for stream facts.
//!
//! Atom layout: `u32` BE size + 4-byte type + body; `size` includes the
//! 8-byte header. `size == 1` means a 64-bit size follows immediately (the
//! header is then 16 bytes, per ISO/IEC 14496-12 §4.2); `size == 0` means the
//! atom extends to the end of its enclosing container. This mirrors
//! `soul_audio_core::demux::mp4`'s `box_header` — the repo's already-tested
//! MP4 demuxer — cross-checked against it for these exact size semantics
//! (ground truth, since a previous task's plan had wrong offsets here). This
//! reader is written in this crate's absolute-offset/reseek style (as in
//! `id3v2`/`riff`/`vorbis`: compute an atom's absolute `[body_start,
//! body_end)`, process it, then unconditionally reseek to `body_end` for the
//! next sibling) rather than that module's relative-remaining-budget style.
//!
//! **`meta` is a `FullBox`**: its body starts with a 4-byte version/flags
//! prefix *before* its children (`hdlr`, `ilst`, ...) — easy to miss (ISO/IEC
//! 14496-12 §8.11.1). `soul_audio_core::demux::mp4` never parses `meta` (it's
//! an audio-only demuxer), so there was nothing to cross-reference for this
//! specific prefix; it's confirmed against the ISO/IEC 14496-12 box
//! definition instead.
//!
//! Each `ilst` entry is an atom whose 4-byte type *is* the key (e.g. `©nam`)
//! containing a `data` atom: `u32` type-indicator, `u32` locale, then the
//! payload. `©`-prefixed keys use the raw byte `0xA9` (not the 2-byte UTF-8
//! encoding of `©`, `0xC2 0xA9`) — matched as raw 4-byte arrays, never as a
//! `&str`.
//!
//! Deliberate scope decision: the top-level walk stops as soon as `moov` has
//! been found and processed — everything this reader needs (`mvhd`, and the
//! `udta/meta/ilst` tag tree) lives inside `moov`. `mdat` (which can be huge,
//! or use `size == 0` to extend to EOF) and anything after it is never
//! visited, so neither its size nor any trailing padding/garbage after it can
//! turn a successful tag read into a spurious `Malformed`.

use crate::util::{read_exact, utf8_prefix};
use crate::{DataEncoding, Field, MimeType, Picture, PictureKind, StreamFacts, TagError, TagSink};
use embedded_io::SeekFrom;
use soul_audio_core::SeekSource;

const MOOV: [u8; 4] = *b"moov";
const UDTA: [u8; 4] = *b"udta";
const META: [u8; 4] = *b"meta";
const ILST: [u8; 4] = *b"ilst";
const MVHD: [u8; 4] = *b"mvhd";
const DATA: [u8; 4] = *b"data";

const NAM: [u8; 4] = [0xA9, b'n', b'a', b'm']; // ©nam
const ART: [u8; 4] = [0xA9, b'A', b'R', b'T']; // ©ART
const ALB: [u8; 4] = [0xA9, b'a', b'l', b'b']; // ©alb
const AART: [u8; 4] = *b"aART";
const TRKN: [u8; 4] = *b"trkn";
const DISK: [u8; 4] = *b"disk";
const DAY: [u8; 4] = [0xA9, b'd', b'a', b'y']; // ©day
const GEN: [u8; 4] = [0xA9, b'g', b'e', b'n']; // ©gen
const COVR: [u8; 4] = *b"covr";
const WRT: [u8; 4] = [0xA9, b'w', b'r', b't']; // ©wrt

/// Read one atom header at the (caller-positioned) current stream position
/// `pos`, bounded by the enclosing container's absolute end
/// `container_end`. Returns the absolute `[body_start, body_end)` of the
/// atom's content and its fourcc, or `Ok(None)` if `pos` has already reached
/// `container_end` (clean end of container — the normal loop-termination
/// case, not an error).
///
/// Every declared size is checked against `container_end` *before* trusting
/// it (both "is there room for the header itself" and "does the computed end
/// stay within the container"), so a truncated or oversized declared size is
/// always `TagError::Malformed`, never a read past the container boundary or
/// a panic on overflow.
fn read_atom_header<S: embedded_io::Read>(
    src: &mut S,
    pos: u64,
    container_end: u64,
) -> Result<Option<(u64, u64, [u8; 4])>, TagError> {
    if pos >= container_end {
        return Ok(None);
    }
    if container_end - pos < 8 {
        return Err(TagError::Malformed); // not even room for a plain header
    }
    let mut hdr = [0u8; 8];
    read_exact(src, &mut hdr)?;
    let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    let fourcc = [hdr[4], hdr[5], hdr[6], hdr[7]];

    let (body_start, body_end) = if size32 == 1 {
        if container_end - pos < 16 {
            return Err(TagError::Malformed);
        }
        let mut big = [0u8; 8];
        read_exact(src, &mut big)?;
        let size64 = u64::from_be_bytes(big);
        let end = pos.checked_add(size64).ok_or(TagError::Malformed)?;
        (pos + 16, end)
    } else if size32 == 0 {
        (pos + 8, container_end)
    } else {
        let end = pos
            .checked_add(u64::from(size32))
            .ok_or(TagError::Malformed)?;
        (pos + 8, end)
    };

    if body_start > body_end || body_end > container_end {
        return Err(TagError::Malformed);
    }
    Ok(Some((body_start, body_end, fourcc)))
}

/// Format `n` as ASCII decimal digits into the front of `out`, returning the
/// written prefix as `&str`. `no_std`/no-`alloc` stand-in for `n.to_string()`.
fn format_u16(n: u16, out: &mut [u8]) -> &str {
    let mut digits = [0u8; 5]; // u16::MAX == "65535", 5 digits
    let mut i = digits.len();
    let mut v = n;
    loop {
        i -= 1;
        digits[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    let written = &digits[i..];
    let len = written.len().min(out.len());
    out[..len].copy_from_slice(&written[..len]);
    core::str::from_utf8(&out[..len]).unwrap_or("")
}

fn field_for_key(key: [u8; 4]) -> Option<Field> {
    match key {
        NAM => Some(Field::Title),
        ART => Some(Field::Artist),
        ALB => Some(Field::Album),
        AART => Some(Field::AlbumArtist),
        DAY => Some(Field::Date),
        GEN => Some(Field::Genre),
        WRT => Some(Field::Composer),
        _ => None,
    }
}

/// The atom-type bytes aren't valid UTF-8 on their own (`©` is the single
/// raw byte `0xA9`, not its 2-byte UTF-8 encoding) — this maps a recognized
/// key back to its human-readable form (`"©nam"`, UTF-8-encoded) for
/// [`TagSink::text`]'s `raw_key`.
fn raw_key_str(key: [u8; 4]) -> &'static str {
    match key {
        NAM => "©nam",
        ART => "©ART",
        ALB => "©alb",
        AART => "aART",
        DAY => "©day",
        GEN => "©gen",
        WRT => "©wrt",
        TRKN => "trkn",
        DISK => "disk",
        COVR => "covr",
        _ => "",
    }
}

/// A UTF-8 text `data` atom (type-indicator `1`): decode `payload_len` bytes
/// into `scratch` and hand it to `sink` under `key`'s mapped [`Field`].
/// Unrecognized keys are silently skipped (nothing sensible to map).
fn handle_text_key<S: embedded_io::Read>(
    src: &mut S,
    key: [u8; 4],
    payload_len: u64,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    let Some(field) = field_for_key(key) else {
        return Ok(());
    };
    let cap = payload_len.min(scratch.len() as u64) as usize;
    read_exact(src, &mut scratch[..cap])?;
    let text = utf8_prefix(&scratch[..cap]);
    sink.text(Some(field), raw_key_str(key), text);
    Ok(())
}

/// `trkn`/`disk`: a binary payload `00 00 <number be16> <total be16>`. Emits
/// the number, then the total too if it's non-zero (an absent total is
/// conventionally encoded as `0`, mirroring how ID3v2 `TRCK`/`TPOS`'s
/// `"n"` — no `/total` — skips the total call).
fn handle_number_pair<S: embedded_io::Read>(
    src: &mut S,
    key: [u8; 4],
    payload_len: u64,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    if payload_len < 6 {
        return Ok(()); // too short to hold reserved+number+total: nothing to report
    }
    let mut buf = [0u8; 6];
    read_exact(src, &mut buf)?;
    let number = u16::from_be_bytes([buf[2], buf[3]]);
    let total = u16::from_be_bytes([buf[4], buf[5]]);

    let (number_field, total_field) = if key == TRKN {
        (Field::TrackNumber, Field::TrackTotal)
    } else {
        (Field::DiscNumber, Field::DiscTotal)
    };
    let raw_key = raw_key_str(key);

    let s = format_u16(number, scratch);
    sink.text(Some(number_field), raw_key, s);
    if total > 0 {
        let s = format_u16(total, scratch);
        sink.text(Some(total_field), raw_key, s);
    }
    Ok(())
}

/// `covr`: type-indicator `13` (JPEG) or `14` (PNG) selects the MIME type;
/// any other value is reported as [`MimeType::Other`] rather than dropped, so
/// a cover in an unrecognized encoding is still discoverable. `payload_start`
/// must already be the *live* source position of the image bytes (queried
/// via `Seek`, not computed by hand — see [`handle_data_atom`]).
fn handle_cover(
    type_indicator: u32,
    payload_start: u64,
    payload_len: u64,
    sink: &mut impl TagSink,
) {
    let mime = match type_indicator {
        13 => MimeType::Jpeg,
        14 => MimeType::Png,
        _ => MimeType::Other,
    };
    let data_len = u32::try_from(payload_len).unwrap_or(u32::MAX);
    sink.picture(&Picture {
        usage: PictureKind::FrontCover,
        mime,
        encoding: DataEncoding::Raw,
        data_offset: payload_start,
        data_len,
        width: None,
        height: None,
    });
}

/// A `data` atom's content is `[body_start, body_end)`: `u32` type-indicator,
/// `u32` locale, then the payload. Reads the two fixed fields, queries the
/// *live* stream position for the payload's absolute offset (the established
/// Task 3/4 pattern — `Picture.data_offset` must come from a real `Seek`
/// query, not hand-accumulated byte counts), then dispatches by `key`.
fn handle_data_atom<S: SeekSource>(
    src: &mut S,
    key: [u8; 4],
    body_start: u64,
    body_end: u64,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    if body_end - body_start < 8 {
        return Err(TagError::Malformed);
    }
    src.seek(SeekFrom::Start(body_start))
        .map_err(|_| TagError::Io)?;
    let mut type_buf = [0u8; 4];
    read_exact(src, &mut type_buf)?;
    let type_indicator = u32::from_be_bytes(type_buf);
    let mut locale = [0u8; 4];
    read_exact(src, &mut locale)?; // not modeled by Picture/Field; skipped

    let payload_start = src.seek(SeekFrom::Current(0)).map_err(|_| TagError::Io)?;
    let payload_len = body_end - payload_start;

    if key == TRKN || key == DISK {
        handle_number_pair(src, key, payload_len, scratch, sink)?;
    } else if key == COVR {
        handle_cover(type_indicator, payload_start, payload_len, sink);
    } else if type_indicator == 1 {
        handle_text_key(src, key, payload_len, scratch, sink)?;
    }
    // Anything else (unrecognized key, or a non-UTF-8 type-indicator on a
    // text-mapped key) has nothing sensible to hand to the sink: skip. The
    // caller re-syncs to this ilst entry's declared end regardless.
    Ok(())
}

/// Walk one `ilst` entry's children looking for its `data` atom (an entry
/// may also carry `mean`/`name` atoms for freeform `----` tags, which aren't
/// modeled here and are simply skipped). Stops at the first `data` atom
/// found — every key this reader maps carries exactly one.
fn handle_ilst_entry<S: SeekSource>(
    src: &mut S,
    key: [u8; 4],
    start: u64,
    end: u64,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    let mut pos = start;
    loop {
        src.seek(SeekFrom::Start(pos)).map_err(|_| TagError::Io)?;
        let Some((body_start, body_end, fourcc)) = read_atom_header(src, pos, end)? else {
            break;
        };
        if fourcc == DATA {
            handle_data_atom(src, key, body_start, body_end, scratch, sink)?;
            break;
        }
        pos = body_end;
    }
    Ok(())
}

/// Walk `ilst`'s children: each is a key atom (`©nam`, `trkn`, `covr`, ...)
/// whose fourcc *is* the tag key.
fn walk_ilst<S: SeekSource>(
    src: &mut S,
    start: u64,
    end: u64,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    let mut pos = start;
    loop {
        src.seek(SeekFrom::Start(pos)).map_err(|_| TagError::Io)?;
        let Some((body_start, body_end, fourcc)) = read_atom_header(src, pos, end)? else {
            break;
        };
        handle_ilst_entry(src, fourcc, body_start, body_end, scratch, sink)?;
        pos = body_end;
    }
    Ok(())
}

/// `meta` is a `FullBox`: a 4-byte version/flags prefix precedes its
/// children. Skip that prefix, then walk for `ilst`.
fn walk_meta<S: SeekSource>(
    src: &mut S,
    start: u64,
    end: u64,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    if end - start < 4 {
        return Err(TagError::Malformed);
    }
    let mut pos = start + 4;
    loop {
        src.seek(SeekFrom::Start(pos)).map_err(|_| TagError::Io)?;
        let Some((body_start, body_end, fourcc)) = read_atom_header(src, pos, end)? else {
            break;
        };
        if fourcc == ILST {
            walk_ilst(src, body_start, body_end, scratch, sink)?;
        }
        pos = body_end;
    }
    Ok(())
}

fn walk_udta<S: SeekSource>(
    src: &mut S,
    start: u64,
    end: u64,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    let mut pos = start;
    loop {
        src.seek(SeekFrom::Start(pos)).map_err(|_| TagError::Io)?;
        let Some((body_start, body_end, fourcc)) = read_atom_header(src, pos, end)? else {
            break;
        };
        if fourcc == META {
            walk_meta(src, body_start, body_end, scratch, sink)?;
        }
        pos = body_end;
    }
    Ok(())
}

/// `mvhd`: `version(1)+flags(3)`, then (v0) `u32` timescale @12 + `u32`
/// duration @16, or (v1) `u32` timescale @20 + `u64` duration @24 — offsets
/// from the atom's content start (`start`), per ISO/IEC 14496-12 §8.2.2.
/// `total_frames = duration`, `sample_rate = timescale` (the movie
/// timescale — adequate for a library listing, not a per-track precise
/// rate).
fn parse_mvhd<S: SeekSource>(
    src: &mut S,
    start: u64,
    end: u64,
    facts: &mut StreamFacts,
) -> Result<(), TagError> {
    if end - start < 1 {
        return Err(TagError::Malformed);
    }
    src.seek(SeekFrom::Start(start)).map_err(|_| TagError::Io)?;
    let mut version = [0u8; 1];
    read_exact(src, &mut version)?;

    if version[0] == 1 {
        if end - start < 32 {
            return Err(TagError::Malformed);
        }
        src.seek(SeekFrom::Start(start + 20))
            .map_err(|_| TagError::Io)?;
        let mut buf = [0u8; 12]; // timescale(4) + duration(8)
        read_exact(src, &mut buf)?;
        let timescale = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let duration = u64::from_be_bytes([
            buf[4], buf[5], buf[6], buf[7], buf[8], buf[9], buf[10], buf[11],
        ]);
        facts.sample_rate = Some(timescale);
        facts.total_frames = Some(duration);
    } else {
        if end - start < 20 {
            return Err(TagError::Malformed);
        }
        src.seek(SeekFrom::Start(start + 12))
            .map_err(|_| TagError::Io)?;
        let mut buf = [0u8; 8]; // timescale(4) + duration(4)
        read_exact(src, &mut buf)?;
        let timescale = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let duration = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        facts.sample_rate = Some(timescale);
        facts.total_frames = Some(u64::from(duration));
    }
    Ok(())
}

fn walk_moov<S: SeekSource>(
    src: &mut S,
    start: u64,
    end: u64,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
    facts: &mut StreamFacts,
) -> Result<(), TagError> {
    let mut pos = start;
    loop {
        src.seek(SeekFrom::Start(pos)).map_err(|_| TagError::Io)?;
        let Some((body_start, body_end, fourcc)) = read_atom_header(src, pos, end)? else {
            break;
        };
        if fourcc == MVHD {
            parse_mvhd(src, body_start, body_end, facts)?;
        } else if fourcc == UDTA {
            walk_udta(src, body_start, body_end, scratch, sink)?;
        }
        pos = body_end;
    }
    Ok(())
}

/// Read tags + facts from an MP4/M4A container. Walks top-level atoms until
/// `moov` is found and fully processed (see the module doc for why the walk
/// deliberately stops there — `mdat` and anything after it is never
/// visited); if no `moov` is ever found, returns `StreamFacts::default()`
/// with nothing delivered to `sink`, mirroring how the other container
/// readers handle "expected structure absent" rather than erroring.
pub fn read_mp4<S: SeekSource>(
    src: &mut S,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<StreamFacts, TagError> {
    let total_len = src.seek(SeekFrom::End(0)).map_err(|_| TagError::Io)?;

    let mut facts = StreamFacts::default();
    let mut pos = 0u64;
    loop {
        src.seek(SeekFrom::Start(pos)).map_err(|_| TagError::Io)?;
        let Some((body_start, body_end, fourcc)) = read_atom_header(src, pos, total_len)? else {
            break;
        };
        if fourcc == MOOV {
            walk_moov(src, body_start, body_end, scratch, sink, &mut facts)?;
            break;
        }
        pos = body_end;
    }
    Ok(facts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testfix::Collect;
    use crate::{Field, MimeType, PictureKind};
    use soul_audio_core::SliceSource;
    extern crate alloc;
    use alloc::vec::Vec;

    /// A generic MP4 atom: `u32` BE size (header + body) + 4-byte type + body.
    fn atom(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
        v.extend_from_slice(fourcc);
        v.extend_from_slice(body);
        v
    }

    /// A minimal version-0 `mvhd`: version+flags(4) + creation+modification
    /// times(8) + timescale(4)@12 + duration(4)@16. Nothing past duration is
    /// needed — the walker re-syncs to the atom's declared end regardless.
    fn mvhd_v0(timescale: u32, duration: u32) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&[0, 0, 0, 0]); // version 0, flags 0
        body.extend_from_slice(&0u32.to_be_bytes()); // creation_time
        body.extend_from_slice(&0u32.to_be_bytes()); // modification_time
        body.extend_from_slice(&timescale.to_be_bytes());
        body.extend_from_slice(&duration.to_be_bytes());
        atom(b"mvhd", &body)
    }

    /// A version-1 `mvhd`: version+flags(4) + creation+modification
    /// times(16) + timescale(4)@20 + duration(8)@24.
    fn mvhd_v1(timescale: u32, duration: u64) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&[1, 0, 0, 0]); // version 1, flags 0
        body.extend_from_slice(&0u64.to_be_bytes()); // creation_time
        body.extend_from_slice(&0u64.to_be_bytes()); // modification_time
        body.extend_from_slice(&timescale.to_be_bytes());
        body.extend_from_slice(&duration.to_be_bytes());
        atom(b"mvhd", &body)
    }

    /// An `ilst` key atom: `key` wrapping a single `data` atom (type
    /// indicator + locale(0) + payload).
    fn ilst_entry(key: &[u8; 4], type_indicator: u32, payload: &[u8]) -> Vec<u8> {
        let mut data_body = Vec::new();
        data_body.extend_from_slice(&type_indicator.to_be_bytes());
        data_body.extend_from_slice(&0u32.to_be_bytes()); // locale
        data_body.extend_from_slice(payload);
        let data_atom = atom(b"data", &data_body);
        atom(key, &data_atom)
    }

    /// A text `ilst` entry (type-indicator 1 = UTF-8).
    fn text_entry(key: &[u8; 4], s: &str) -> Vec<u8> {
        ilst_entry(key, 1, s.as_bytes())
    }

    /// A `trkn`/`disk` binary entry: `00 00 <number be16> <total be16>`.
    fn number_pair_entry(key: &[u8; 4], number: u16, total: u16) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&[0, 0]);
        payload.extend_from_slice(&number.to_be_bytes());
        payload.extend_from_slice(&total.to_be_bytes());
        ilst_entry(key, 0, &payload)
    }

    /// A `covr` entry (type-indicator 13 = JPEG).
    fn covr_entry(data: &[u8]) -> Vec<u8> {
        ilst_entry(b"covr", 13, data)
    }

    fn ilst(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut body = Vec::new();
        for e in entries {
            body.extend_from_slice(e);
        }
        atom(b"ilst", &body)
    }

    /// `meta`'s `FullBox` prefix (version+flags, 4 bytes) then its children.
    fn meta(ilst_atom: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&[0, 0, 0, 0]);
        body.extend_from_slice(ilst_atom);
        atom(b"meta", &body)
    }

    fn udta(meta_atom: &[u8]) -> Vec<u8> {
        atom(b"udta", meta_atom)
    }

    /// A minimal faststart-shaped M4A: `ftyp` + `moov`(`mvhd` +
    /// `udta/meta/ilst` with `©nam`, `©ART`, `trkn`, `covr`).
    fn minimal_m4a() -> Vec<u8> {
        let entries = ilst(&[
            text_entry(&NAM, "Test Title"),
            text_entry(&ART, "Test Artist"),
            number_pair_entry(&TRKN, 3, 12),
            covr_entry(b"\xFF\xD8\xFF\xEEjpegdata"),
        ]);
        let moov_body = {
            let mut v = Vec::new();
            v.extend_from_slice(&mvhd_v0(44_100, 132_300));
            v.extend_from_slice(&udta(&meta(&entries)));
            v
        };
        let mut v = Vec::new();
        v.extend_from_slice(&atom(b"ftyp", b"M4A \0\0\0\0M4A mp42isom"));
        v.extend_from_slice(&atom(b"moov", &moov_body));
        v
    }

    #[test]
    fn mp4_fields_facts_and_cover() {
        let bytes = minimal_m4a();
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = read_mp4(&mut src, &mut scratch, &mut c).unwrap();

        assert_eq!(facts.sample_rate, Some(44_100));
        assert_eq!(facts.total_frames, Some(132_300));

        assert!(
            c.fields
                .iter()
                .any(|(f, k, v)| *f == Some(Field::Title) && k == "©nam" && v == "Test Title")
        );
        assert!(
            c.fields
                .iter()
                .any(|(f, k, v)| *f == Some(Field::Artist) && k == "©ART" && v == "Test Artist")
        );
        assert!(
            c.fields
                .iter()
                .any(|(f, _, v)| *f == Some(Field::TrackNumber) && v == "3")
        );
        assert!(
            c.fields
                .iter()
                .any(|(f, _, v)| *f == Some(Field::TrackTotal) && v == "12")
        );

        assert_eq!(c.pics.len(), 1);
        assert_eq!(c.pics[0].usage, PictureKind::FrontCover);
        assert_eq!(c.pics[0].mime, MimeType::Jpeg);
        assert_eq!(c.pics[0].encoding, DataEncoding::Raw);
        assert_eq!(c.pics[0].data_len, 12);
        assert_eq!(
            &bytes[c.pics[0].data_offset as usize..][..12],
            b"\xFF\xD8\xFF\xEEjpegdata"
        );
    }

    #[test]
    fn mp4_mvhd_version1_wide_duration() {
        let entries = ilst(&[text_entry(&NAM, "V1")]);
        let moov_body = {
            let mut v = Vec::new();
            v.extend_from_slice(&mvhd_v1(48_000, 10_000_000_000));
            v.extend_from_slice(&udta(&meta(&entries)));
            v
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&atom(b"ftyp", b"M4A "));
        bytes.extend_from_slice(&atom(b"moov", &moov_body));

        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = read_mp4(&mut src, &mut scratch, &mut c).unwrap();
        assert_eq!(facts.sample_rate, Some(48_000));
        assert_eq!(facts.total_frames, Some(10_000_000_000));
    }

    #[test]
    fn mp4_no_track_total_when_zero() {
        let entries = ilst(&[number_pair_entry(&TRKN, 5, 0)]);
        let moov_body = {
            let mut v = Vec::new();
            v.extend_from_slice(&mvhd_v0(44_100, 100));
            v.extend_from_slice(&udta(&meta(&entries)));
            v
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&atom(b"ftyp", b"M4A "));
        bytes.extend_from_slice(&atom(b"moov", &moov_body));

        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        read_mp4(&mut src, &mut scratch, &mut c).unwrap();
        assert!(
            c.fields
                .iter()
                .any(|(f, _, v)| *f == Some(Field::TrackNumber) && v == "5")
        );
        assert!(
            !c.fields
                .iter()
                .any(|(f, _, _)| *f == Some(Field::TrackTotal))
        );
    }

    #[test]
    fn mp4_without_moov_returns_default_facts() {
        let bytes = atom(b"ftyp", b"M4A \0\0\0\0M4A mp42isom");
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = read_mp4(&mut src, &mut scratch, &mut c).unwrap();
        assert_eq!(facts, StreamFacts::default());
        assert!(c.fields.is_empty());
        assert!(c.pics.is_empty());
    }

    #[test]
    fn mp4_trailing_garbage_after_moov_is_ignored() {
        // A short (< 8 byte) trailing stray tail after moov must not turn a
        // successful tag read into Malformed — the walk stops at moov.
        let mut bytes = minimal_m4a();
        bytes.extend_from_slice(&[0, 0, 1]); // 3 stray bytes, not a valid atom header
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = read_mp4(&mut src, &mut scratch, &mut c).unwrap();
        assert_eq!(facts.sample_rate, Some(44_100));
        assert!(
            c.fields
                .iter()
                .any(|(f, _, v)| *f == Some(Field::Title) && v == "Test Title")
        );
    }
}
