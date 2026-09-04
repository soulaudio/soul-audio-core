//! ID3v2 (2.2/2.3/2.4) tag reader + MP3 container walk.
//!
//! `read_id3v2` parses a tag whose 10-byte header starts at the source's
//! *current* position. The source is always the whole file, so picture data
//! offsets are read straight from the live (absolute) position — correct for a
//! bare MP3 (tag at offset 0) and for a tag embedded in another container (a
//! WAV `id3 ` chunk, a DSF header) once the caller has seeked the source to the
//! tag. `read_mp3` is the MP3 container walk: it reads a leading ID3v2 tag (if
//! any), then hands off to [`crate::mp3::read_facts`] for the Xing/Info/VBRI
//! (or CBR-estimate) `StreamFacts` scan.
//!
//! Known, deliberate deferrals (none exercised by required tests):
//! - The global unsynchronisation scheme (header flag bit 7) is not
//!   de-stuffed; a tag that sets it is rejected with `TagError::Unsupported`
//!   rather than risking silently-wrong decoded text/picture bytes.
//! - Per-frame compression/encryption (frame flag bits in v2.3/v2.4) are not
//!   interpreted; such a frame's body would be parsed as raw text/binary,
//!   which is wrong but *safe* (bounded reads, no panics). These flags are
//!   essentially unused in the wild.

use crate::locate;
use crate::util::{read_exact, utf8_prefix};
use crate::{DataEncoding, Field, MimeType, Picture, PictureKind, StreamFacts, TagError, TagSink};
use embedded_io::SeekFrom;
use soul_audio_core::SeekSource;

/// Decode a 4-byte synchsafe integer (7 bits per byte, MSB first).
fn synchsafe_u32(b: &[u8; 4]) -> u32 {
    (u32::from(b[0]) << 21) | (u32::from(b[1]) << 14) | (u32::from(b[2]) << 7) | u32::from(b[3])
}

/// Append `ch`'s UTF-8 encoding to `out[*out_len..]` if it fits without
/// overflowing `out`; leaves `out_len` unchanged and returns `false` if not
/// (the caller stops appending — this is how every decoder here truncates at
/// a valid UTF-8 boundary instead of splitting a codepoint).
fn push_char(out: &mut [u8], out_len: &mut usize, ch: char) -> bool {
    let mut buf = [0u8; 4];
    let s = ch.encode_utf8(&mut buf);
    let bytes = s.as_bytes();
    if *out_len + bytes.len() > out.len() {
        return false;
    }
    out[*out_len..*out_len + bytes.len()].copy_from_slice(bytes);
    *out_len += bytes.len();
    true
}

/// Decode a Latin-1 (ISO-8859-1) string of `raw_len` bytes into `out` as
/// UTF-8, stopping at a NUL terminator or when `out` is full.
fn decode_latin1<'a, S: embedded_io::Read>(
    src: &mut S,
    raw_len: usize,
    out: &'a mut [u8],
) -> Result<&'a str, TagError> {
    let mut out_len = 0usize;
    let mut stopped = false;
    for _ in 0..raw_len {
        let mut b = [0u8; 1];
        read_exact(src, &mut b)?;
        if stopped {
            continue;
        }
        if b[0] == 0 {
            stopped = true;
            continue;
        }
        // Latin-1 byte values map 1:1 onto the first 256 Unicode codepoints.
        if !push_char(out, &mut out_len, b[0] as char) {
            stopped = true;
        }
    }
    Ok(core::str::from_utf8(&out[..out_len]).unwrap_or(""))
}

/// Copy a UTF-8 string of `raw_len` bytes directly into `out` (truncating at
/// `out.len()` and then at the last valid UTF-8 boundary), stopping at a NUL
/// terminator if present in the copied prefix.
fn decode_utf8<'a, S: embedded_io::Read>(
    src: &mut S,
    raw_len: usize,
    out: &'a mut [u8],
) -> Result<&'a str, TagError> {
    let cap = raw_len.min(out.len());
    read_exact(src, &mut out[..cap])?;
    let end = out[..cap].iter().position(|&b| b == 0).unwrap_or(cap);
    Ok(utf8_prefix(&out[..end]))
}

/// Read one UTF-16 code unit: either a previously-buffered `pending` unit
/// (used to re-play a BOM-shaped pair of bytes when no real BOM was found),
/// or the next 2 bytes from `src`. `Ok(None)` means fewer than 2 bytes remain
/// in the declared budget.
fn read_utf16_unit<S: embedded_io::Read>(
    src: &mut S,
    remaining: &mut usize,
    pending: &mut Option<u16>,
    big_endian: bool,
) -> Result<Option<u16>, TagError> {
    if let Some(u) = pending.take() {
        return Ok(Some(u));
    }
    if *remaining < 2 {
        return Ok(None);
    }
    let mut cu = [0u8; 2];
    read_exact(src, &mut cu)?;
    *remaining -= 2;
    Ok(Some(if big_endian {
        u16::from_be_bytes(cu)
    } else {
        u16::from_le_bytes(cu)
    }))
}

/// Decode a UTF-16 string of `raw_len` bytes into `out` as UTF-8. When
/// `forced_big_endian` is `None` (encoding byte 1), the first 2 bytes are
/// read as a BOM (`FF FE` little-endian, `FE FF` big-endian); if they match
/// neither, they're treated as the first (little-endian) code unit instead
/// of a BOM, per common real-world behavior. `Some(bool)` (encoding byte 2)
/// fixes the endianness with no BOM. Stops at a NUL code unit, an
/// unfollowed/mismatched surrogate half, or when `out` is full.
fn decode_utf16<'a, S: embedded_io::Read>(
    src: &mut S,
    raw_len: usize,
    forced_big_endian: Option<bool>,
    out: &'a mut [u8],
) -> Result<&'a str, TagError> {
    let mut out_len = 0usize;
    let mut remaining = raw_len;
    let mut big_endian = forced_big_endian.unwrap_or(false);
    let mut pending: Option<u16> = None;

    if forced_big_endian.is_none() && remaining >= 2 {
        let mut bom = [0u8; 2];
        read_exact(src, &mut bom)?;
        remaining -= 2;
        match bom {
            [0xFF, 0xFE] => {}
            [0xFE, 0xFF] => big_endian = true,
            _ => pending = Some(u16::from_le_bytes(bom)),
        }
    }

    while let Some(unit) = read_utf16_unit(src, &mut remaining, &mut pending, big_endian)? {
        if unit == 0 {
            break;
        }

        let cp: u32 = if (0xD800..=0xDBFF).contains(&unit) {
            let unit2 = match read_utf16_unit(src, &mut remaining, &mut pending, big_endian)? {
                Some(u) => u,
                None => break,
            };
            if !(0xDC00..=0xDFFF).contains(&unit2) {
                break; // unpaired/malformed surrogate: stop rather than emit garbage
            }
            0x10000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(unit2) - 0xDC00)
        } else {
            u32::from(unit)
        };

        match char::from_u32(cp) {
            Some(ch) => {
                if !push_char(out, &mut out_len, ch) {
                    break;
                }
            }
            None => break,
        }
    }

    Ok(core::str::from_utf8(&out[..out_len]).unwrap_or(""))
}

/// Dispatch on the ID3v2 text-encoding byte (0=Latin-1, 1=UTF-16 w/ BOM,
/// 2=UTF-16BE, 3=UTF-8) and decode `raw_len` raw bytes into `out` as UTF-8.
fn decode_text<'a, S: embedded_io::Read>(
    src: &mut S,
    encoding: u8,
    raw_len: usize,
    out: &'a mut [u8],
) -> Result<&'a str, TagError> {
    match encoding {
        0 => decode_latin1(src, raw_len, out),
        1 => decode_utf16(src, raw_len, None, out),
        2 => decode_utf16(src, raw_len, Some(true), out),
        3 => decode_utf8(src, raw_len, out),
        _ => Err(TagError::Malformed),
    }
}

/// Skip a NUL-terminated string in the given text encoding without
/// retaining its content (used for the APIC/PIC description, which the
/// [`Picture`] model doesn't carry). Stops at the terminator — a single
/// `0x00` byte for Latin-1/UTF-8, a `0x00 0x00` code unit for either UTF-16
/// variant (NUL is byte-order-symmetric, so no BOM/endianness handling is
/// needed just to find it) — or after `max_len` bytes if none is found.
/// Returns the number of raw bytes consumed.
fn skip_encoded_string<S: embedded_io::Read>(
    src: &mut S,
    encoding: u8,
    max_len: usize,
) -> Result<usize, TagError> {
    match encoding {
        1 | 2 => {
            let mut consumed = 0usize;
            while consumed + 2 <= max_len {
                let mut cu = [0u8; 2];
                read_exact(src, &mut cu)?;
                consumed += 2;
                if cu == [0, 0] {
                    break;
                }
            }
            Ok(consumed)
        }
        _ => {
            let mut consumed = 0usize;
            while consumed < max_len {
                let mut b = [0u8; 1];
                read_exact(src, &mut b)?;
                consumed += 1;
                if b[0] == 0 {
                    break;
                }
            }
            Ok(consumed)
        }
    }
}

/// Map a text-frame id (v2.2 3-char or v2.3/v2.4 4-char) to a normalized
/// [`Field`]. `TRCK`/`TPOS` (and their v2.2 equivalents) are handled
/// separately since they split into two sink calls.
fn map_frame_id(id: &str) -> Option<Field> {
    match id {
        "TIT2" | "TT2" => Some(Field::Title),
        "TPE1" | "TP1" => Some(Field::Artist),
        "TALB" | "TAL" => Some(Field::Album),
        "TPE2" | "TP2" => Some(Field::AlbumArtist),
        "TDRC" | "TYER" | "TYE" => Some(Field::Date),
        "TCON" | "TCO" => Some(Field::Genre),
        "TCOM" | "TCM" => Some(Field::Composer),
        _ => None,
    }
}

fn mime_kind(bytes: &[u8]) -> MimeType {
    match bytes {
        b"image/jpeg" => MimeType::Jpeg,
        b"image/png" => MimeType::Png,
        _ => MimeType::Other,
    }
}

/// v2.2's `PIC` frame uses a fixed 3-char image-format code instead of a
/// MIME string.
fn mime_kind_v22(code: &[u8; 3]) -> MimeType {
    match code {
        b"PNG" => MimeType::Png,
        b"JPG" => MimeType::Jpeg,
        _ => MimeType::Other,
    }
}

fn picture_kind(pic_type: u8) -> PictureKind {
    match pic_type {
        3 => PictureKind::FrontCover,
        4 => PictureKind::BackCover,
        8 => PictureKind::Artist,
        _ => PictureKind::Other,
    }
}

/// A plain text frame: 1 encoding byte + string body → one `sink.text` call.
fn handle_text<S: embedded_io::Read>(
    src: &mut S,
    raw_len: usize,
    raw_key: &str,
    field: Field,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    if raw_len == 0 {
        return Ok(());
    }
    let mut enc = [0u8; 1];
    read_exact(src, &mut enc)?;
    let s = decode_text(src, enc[0], raw_len - 1, scratch)?;
    sink.text(Some(field), raw_key, s);
    Ok(())
}

/// `TRCK`/`TPOS` (and v2.2 `TRK`/`TPA`): "n" or "n/total" → one or two
/// `sink.text` calls (number, then total if present).
fn handle_num_total<S: embedded_io::Read>(
    src: &mut S,
    raw_len: usize,
    raw_key: &str,
    number_field: Field,
    total_field: Field,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    if raw_len == 0 {
        return Ok(());
    }
    let mut enc = [0u8; 1];
    read_exact(src, &mut enc)?;
    let s = decode_text(src, enc[0], raw_len - 1, scratch)?;
    if let Some(idx) = s.find('/') {
        let (num, rest) = s.split_at(idx);
        let total = &rest[1..];
        sink.text(Some(number_field), raw_key, num);
        if !total.is_empty() {
            sink.text(Some(total_field), raw_key, total);
        }
    } else {
        sink.text(Some(number_field), raw_key, s);
    }
    Ok(())
}

/// `APIC` (v2.3/v2.4) or `PIC` (v2.2): encoding byte, mime (NUL-terminated
/// ASCII, or a fixed 3-char code for `PIC`), picture-type byte, description
/// (NUL-terminated per encoding, skipped), then image bytes. Records
/// `data_offset` as the absolute source offset of the image bytes without
/// reading them.
fn handle_apic<S: SeekSource>(
    src: &mut S,
    ver_major: u8,
    raw_len: usize,
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    if raw_len == 0 {
        return Ok(());
    }
    let mut budget = raw_len;

    let mut enc = [0u8; 1];
    read_exact(src, &mut enc)?;
    budget -= 1;

    let mime = if ver_major == 2 {
        if budget < 3 {
            return Err(TagError::Malformed);
        }
        let mut code = [0u8; 3];
        read_exact(src, &mut code)?;
        budget -= 3;
        mime_kind_v22(&code)
    } else {
        let mut mime_buf = [0u8; 32];
        let mut mime_len = 0usize;
        loop {
            if budget == 0 {
                return Err(TagError::Malformed); // ran out before the NUL / picture-type byte
            }
            let mut b = [0u8; 1];
            read_exact(src, &mut b)?;
            budget -= 1;
            if b[0] == 0 {
                break;
            }
            if mime_len < mime_buf.len() {
                mime_buf[mime_len] = b[0];
                mime_len += 1;
            }
        }
        mime_kind(&mime_buf[..mime_len])
    };

    if budget == 0 {
        return Err(TagError::Malformed);
    }
    let mut ptype = [0u8; 1];
    read_exact(src, &mut ptype)?;
    budget -= 1;
    let usage = picture_kind(ptype[0]);

    let desc_consumed = skip_encoded_string(src, enc[0], budget)?;
    budget -= desc_consumed;

    // `src` is the whole-file source (the WAV/DSF readers seek it to the tag
    // before calling us, `read_owned` wraps the real file), so the current
    // position IS the absolute file offset of the image bytes. Do NOT add any
    // base — that double-counted for embedded tags and put the offset past EOF.
    let data_offset = src.seek(SeekFrom::Current(0)).map_err(|_| TagError::Io)?;
    let data_len = budget as u32;

    sink.picture(&Picture {
        usage,
        mime,
        encoding: DataEncoding::Raw,
        data_offset,
        data_len,
        width: None,
        height: None,
    });
    Ok(())
}

fn handle_frame<S: SeekSource>(
    src: &mut S,
    ver_major: u8,
    id: &str,
    raw_len: usize,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    match id {
        "TRCK" | "TRK" => handle_num_total(
            src,
            raw_len,
            id,
            Field::TrackNumber,
            Field::TrackTotal,
            scratch,
            sink,
        ),
        "TPOS" | "TPA" => handle_num_total(
            src,
            raw_len,
            id,
            Field::DiscNumber,
            Field::DiscTotal,
            scratch,
            sink,
        ),
        "APIC" | "PIC" => handle_apic(src, ver_major, raw_len, sink),
        _ => match map_frame_id(id) {
            Some(field) => handle_text(src, raw_len, id, field, scratch, sink),
            None => Ok(()), // unknown frame: skip (caller re-syncs past its declared size)
        },
    }
}

/// Skip the ID3v2.3/v2.4 extended header (present when header flag bit 6 is
/// set). Its first 4 bytes are a size field whose encoding *and* meaning
/// differ by version:
/// - v2.3: a plain big-endian integer counting the bytes that follow the
///   size field (the size field itself is excluded), so after reading it we
///   skip exactly that many more bytes.
/// - v2.4: a synchsafe (7 bits/byte) integer counting the *entire* extended
///   header, *including* the 4 size bytes already consumed — so we skip
///   `ext_size - 4` more bytes (saturating: a malformed size smaller than 4
///   skips zero rather than seeking backwards or overflowing).
fn skip_extended_header<S: SeekSource>(src: &mut S, ver_major: u8) -> Result<(), TagError> {
    let mut size_buf = [0u8; 4];
    read_exact(src, &mut size_buf)?;
    let skip_len = if ver_major == 4 {
        synchsafe_u32(&size_buf).saturating_sub(4)
    } else {
        u32::from_be_bytes(size_buf)
    };
    src.seek(SeekFrom::Current(i64::from(skip_len)))
        .map_err(|_| TagError::Io)?;
    Ok(())
}

/// Parse an ID3v2 tag whose 10-byte header starts at `src`'s current
/// position. `src` must be the whole-file source positioned at the tag (a bare
/// MP3 is at offset 0; the WAV/DSF readers seek it to the embedded tag first),
/// so `Picture.data_offset` is read directly as the absolute file position.
/// Leaves `src` positioned just past the tag on success.
pub fn read_id3v2<S: SeekSource>(
    src: &mut S,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    let mut header = [0u8; 10];
    read_exact(src, &mut header)?;
    if &header[0..3] != b"ID3" {
        return Err(TagError::Malformed);
    }
    let ver_major = header[3];
    if !(2..=4).contains(&ver_major) {
        return Err(TagError::Unsupported);
    }
    let flags = header[5];
    if flags & 0x80 != 0 {
        // Global unsynchronisation: see module doc's documented-deferral note.
        return Err(TagError::Unsupported);
    }

    let mut size_buf = [0u8; 4];
    size_buf.copy_from_slice(&header[6..10]);
    let tag_size = synchsafe_u32(&size_buf);

    let tag_body_start = src.seek(SeekFrom::Current(0)).map_err(|_| TagError::Io)?;
    let tag_end = tag_body_start
        .checked_add(u64::from(tag_size))
        .ok_or(TagError::Malformed)?;

    if flags & 0x40 != 0 {
        if ver_major == 2 {
            // v2.2 bit 6 means "compression" (deprecated, scheme
            // unspecified) rather than an extended header.
            return Err(TagError::Unsupported);
        }
        skip_extended_header(src, ver_major)?;
    }

    let id_len: usize = if ver_major == 2 { 3 } else { 4 };
    let frame_header_len: u64 = if ver_major == 2 { 6 } else { 10 };

    loop {
        let pos = src.seek(SeekFrom::Current(0)).map_err(|_| TagError::Io)?;
        if pos + frame_header_len > tag_end {
            break;
        }

        let mut id_buf = [0u8; 4];
        read_exact(src, &mut id_buf[..id_len])?;
        if id_buf[0] == 0 {
            break; // zero-id padding: end of real frames
        }

        let frame_size: u32 = if ver_major == 2 {
            let mut sz = [0u8; 3];
            read_exact(src, &mut sz)?;
            (u32::from(sz[0]) << 16) | (u32::from(sz[1]) << 8) | u32::from(sz[2])
        } else {
            let mut sz = [0u8; 4];
            read_exact(src, &mut sz)?;
            if ver_major == 4 {
                synchsafe_u32(&sz)
            } else {
                u32::from_be_bytes(sz)
            }
        };

        if ver_major != 2 {
            let mut frame_flags = [0u8; 2];
            read_exact(src, &mut frame_flags)?;
        }

        let body_start = src.seek(SeekFrom::Current(0)).map_err(|_| TagError::Io)?;
        let body_end = body_start
            .checked_add(u64::from(frame_size))
            .ok_or(TagError::Malformed)?;
        if body_end > tag_end {
            return Err(TagError::Malformed);
        }

        let id_str = core::str::from_utf8(&id_buf[..id_len]).unwrap_or("");
        handle_frame(src, ver_major, id_str, frame_size as usize, scratch, sink)?;

        // Re-sync to the frame's declared end regardless of how many bytes
        // the handler above actually consumed — keeps a slightly-off parser
        // (or a deliberately unhandled frame) from desyncing the walk.
        src.seek(SeekFrom::Start(body_end))
            .map_err(|_| TagError::Io)?;
    }

    src.seek(SeekFrom::Start(tag_end))
        .map_err(|_| TagError::Io)?;
    Ok(())
}

/// Read the MP3 container: an optional leading ID3v2 tag, then the first
/// MPEG audio frame's header (+ Xing/Info/VBRI tag, if present) for
/// `StreamFacts` — a cheap header scan, not a decode. See
/// [`crate::mp3::read_facts`] for the frame/duration parsing itself.
pub fn read_mp3<S: SeekSource>(
    src: &mut S,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<StreamFacts, TagError> {
    let mut magic = [0u8; 3];
    let n = locate::read_full(src, &mut magic)?;
    src.seek(SeekFrom::Start(0)).map_err(|_| TagError::Io)?;
    if n == 3 && &magic == b"ID3" {
        read_id3v2(src, scratch, sink)?;
    }
    Ok(crate::mp3::read_facts(src))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testfix::*;
    use crate::{Field, MimeType, PictureKind};
    use soul_audio_core::SliceSource;
    extern crate alloc;
    use alloc::{string::String, vec::Vec};

    fn text_frame_v24(id: &[u8; 4], s: &str) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(id);
        let body_len = 1 + s.len() as u32;
        b.extend_from_slice(&synchsafe(body_len)); // v2.4 synchsafe size
        b.extend_from_slice(&[0, 0]); // flags
        b.push(0x03); // UTF-8
        b.extend_from_slice(s.as_bytes());
        b
    }
    fn text_frame_v22(id: &[u8; 3], s: &str) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(id);
        let body_len = 1 + s.len() as u32;
        b.extend_from_slice(&body_len.to_be_bytes()[1..]); // 3-byte plain size, no flags
        b.push(0x03); // UTF-8
        b.extend_from_slice(s.as_bytes());
        b
    }
    fn text_frame_enc(id: &[u8; 4], enc: u8, raw: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(id);
        let body_len = 1 + raw.len() as u32;
        b.extend_from_slice(&body_len.to_be_bytes());
        b.extend_from_slice(&[0, 0]);
        b.push(enc);
        b.extend_from_slice(raw);
        b
    }
    fn pic_v22(fmt: &[u8; 3], ptype: u8, data: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.push(0x00); // enc latin-1
        body.extend_from_slice(fmt); // 3-char image format, no nul
        body.push(ptype);
        body.push(0); // empty description + nul
        body.extend_from_slice(data);
        let mut b = Vec::new();
        b.extend_from_slice(b"PIC");
        b.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        b.extend_from_slice(&body);
        b
    }
    fn id3v23_padded(frames: &[Vec<u8>], padding: usize) -> Vec<u8> {
        let mut body = Vec::new();
        for f in frames {
            body.extend_from_slice(f);
        }
        let mut v = Vec::new();
        v.extend_from_slice(b"ID3");
        v.extend_from_slice(&[3, 0, 0]);
        v.extend_from_slice(&synchsafe((body.len() + padding) as u32));
        v.extend_from_slice(&body);
        v.extend(core::iter::repeat_n(0u8, padding));
        v
    }
    fn id3v23_flags(frames: &[Vec<u8>], header_flags: u8) -> Vec<u8> {
        let mut body = Vec::new();
        for f in frames {
            body.extend_from_slice(f);
        }
        let mut v = Vec::new();
        v.extend_from_slice(b"ID3");
        v.extend_from_slice(&[3, 0, header_flags]);
        v.extend_from_slice(&synchsafe(body.len() as u32));
        v.extend_from_slice(&body);
        v
    }
    fn id3v24(frames: &[Vec<u8>]) -> Vec<u8> {
        let mut body = Vec::new();
        for f in frames {
            body.extend_from_slice(f);
        }
        let mut v = Vec::new();
        v.extend_from_slice(b"ID3");
        v.extend_from_slice(&[4, 0, 0]); // v2.4, no flags
        v.extend_from_slice(&synchsafe(body.len() as u32));
        v.extend_from_slice(&body);
        v
    }
    fn id3v22(frames: &[Vec<u8>]) -> Vec<u8> {
        let mut body = Vec::new();
        for f in frames {
            body.extend_from_slice(f);
        }
        let mut v = Vec::new();
        v.extend_from_slice(b"ID3");
        v.extend_from_slice(&[2, 0, 0]); // v2.2, no flags
        v.extend_from_slice(&synchsafe(body.len() as u32));
        v.extend_from_slice(&body);
        v
    }

    #[test]
    fn id3v23_text_and_apic() {
        let tag = id3v23(&[
            text_frame(b"TIT2", "Moment's Notice"),
            text_frame(b"TPE1", "John Coltrane"),
            text_frame(b"TALB", "Blue Train"),
            text_frame(b"TRCK", "2/5"),
            apic("image/png", 3, b"\x89PNG\r\n\x1a\n"),
        ]);
        let mut src = SliceSource::new(&tag);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        read_id3v2(&mut src, &mut scratch, &mut c).unwrap();
        assert!(
            c.fields
                .contains(&(Some(Field::Title), "TIT2".into(), "Moment's Notice".into()))
        );
        assert!(
            c.fields
                .contains(&(Some(Field::Artist), "TPE1".into(), "John Coltrane".into()))
        );
        assert!(
            c.fields
                .iter()
                .any(|(f, _, v)| *f == Some(Field::TrackNumber) && v == "2")
        );
        assert!(
            c.fields
                .iter()
                .any(|(f, _, v)| *f == Some(Field::TrackTotal) && v == "5")
        );
        assert_eq!(c.pics.len(), 1);
        assert_eq!(c.pics[0].usage, PictureKind::FrontCover);
        assert_eq!(c.pics[0].mime, MimeType::Png);
        assert_eq!(
            &tag[c.pics[0].data_offset as usize..][..8],
            b"\x89PNG\r\n\x1a\n"
        );
    }

    #[test]
    fn id3v24_synchsafe_frame_size() {
        // A 200-byte body forces the synchsafe and plain-big-endian
        // interpretations of the 4-byte size field to diverge (200 < 256 but
        // > 127, so the synchsafe encoding's 3rd byte carries a bit that a
        // plain-big-endian reader would place differently). If frame-size
        // decoding used plain big-endian here instead of synchsafe, the
        // computed size would overrun into TPE1 (or past the tag), and
        // either TPE1 would come out wrong or parsing would error.
        let long_title: String = "x".repeat(200);
        let tag = id3v24(&[
            text_frame_v24(b"TIT2", &long_title),
            text_frame_v24(b"TPE1", "Artist After"),
        ]);
        let mut src = SliceSource::new(&tag);
        let mut scratch = [0u8; 512];
        let mut c = Collect::default();
        read_id3v2(&mut src, &mut scratch, &mut c).unwrap();
        assert!(
            c.fields
                .iter()
                .any(|(f, k, v)| *f == Some(Field::Title) && k == "TIT2" && v == &long_title)
        );
        assert!(
            c.fields
                .contains(&(Some(Field::Artist), "TPE1".into(), "Artist After".into()))
        );
    }

    #[test]
    fn id3v23_utf16_le_bom_text_frame() {
        // "Café ♫" — includes a non-ASCII BMP char (é) and a beyond-BMP
        // surrogate pair (🎵, U+1F3B5) to pin both the plain-BMP and
        // surrogate-pair decode paths, little-endian with a BOM.
        let s = "Café 🎵";
        let mut raw = Vec::new();
        raw.extend_from_slice(&[0xFF, 0xFE]); // BOM: little-endian
        for u in s.encode_utf16() {
            raw.extend_from_slice(&u.to_le_bytes());
        }
        let tag = id3v23(&[text_frame_enc(b"TIT2", 0x01, &raw)]);
        let mut src = SliceSource::new(&tag);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        read_id3v2(&mut src, &mut scratch, &mut c).unwrap();
        assert!(
            c.fields
                .contains(&(Some(Field::Title), "TIT2".into(), s.into()))
        );
    }

    #[test]
    fn id3v23_utf16be_text_frame() {
        let s = "Blue Train";
        let mut raw = Vec::new();
        for u in s.encode_utf16() {
            raw.extend_from_slice(&u.to_be_bytes());
        }
        let tag = id3v23(&[text_frame_enc(b"TALB", 0x02, &raw)]);
        let mut src = SliceSource::new(&tag);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        read_id3v2(&mut src, &mut scratch, &mut c).unwrap();
        assert!(
            c.fields
                .contains(&(Some(Field::Album), "TALB".into(), s.into()))
        );
    }

    #[test]
    fn id3v23_latin1_high_byte_text_frame() {
        // 0xE9 (Latin-1 'é') must grow to a 2-byte UTF-8 sequence.
        let raw = [b'C', b'a', b'f', 0xE9];
        let tag = id3v23(&[text_frame_enc(b"TIT2", 0x00, &raw)]);
        let mut src = SliceSource::new(&tag);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        read_id3v2(&mut src, &mut scratch, &mut c).unwrap();
        assert!(
            c.fields
                .contains(&(Some(Field::Title), "TIT2".into(), "Café".into()))
        );
    }

    #[test]
    fn id3v22_frame_ids_and_pic() {
        let tag = id3v22(&[
            text_frame_v22(b"TT2", "Moment's Notice"),
            text_frame_v22(b"TRK", "3"), // no total
            pic_v22(b"PNG", 3, b"\x89PNG\r\n\x1a\n"),
        ]);
        let mut src = SliceSource::new(&tag);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        read_id3v2(&mut src, &mut scratch, &mut c).unwrap();
        assert!(
            c.fields
                .contains(&(Some(Field::Title), "TT2".into(), "Moment's Notice".into()))
        );
        assert!(
            c.fields
                .contains(&(Some(Field::TrackNumber), "TRK".into(), "3".into()))
        );
        assert!(
            !c.fields
                .iter()
                .any(|(f, _, _)| *f == Some(Field::TrackTotal))
        );
        assert_eq!(c.pics.len(), 1);
        assert_eq!(c.pics[0].mime, MimeType::Png);
        assert_eq!(c.pics[0].usage, PictureKind::FrontCover);
        assert_eq!(
            &tag[c.pics[0].data_offset as usize..][..8],
            b"\x89PNG\r\n\x1a\n"
        );
    }

    #[test]
    fn stops_at_zero_id_padding() {
        let tag = id3v23_padded(&[text_frame(b"TIT2", "Padded")], 20);
        let mut src = SliceSource::new(&tag);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        read_id3v2(&mut src, &mut scratch, &mut c).unwrap();
        assert!(
            c.fields
                .contains(&(Some(Field::Title), "TIT2".into(), "Padded".into()))
        );
    }

    #[test]
    fn global_unsync_is_unsupported() {
        let tag = id3v23_flags(&[text_frame(b"TIT2", "x")], 0x80);
        let mut src = SliceSource::new(&tag);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        assert_eq!(
            read_id3v2(&mut src, &mut scratch, &mut c),
            Err(TagError::Unsupported)
        );
    }

    #[test]
    fn id3v24_extended_header_size_is_synchsafe() {
        // v2.4 extended-header size is synchsafe and counts the WHOLE
        // extended header, including the 4 size bytes themselves: a minimal
        // extended header (size field + 1 "number of flag bytes" byte + 1
        // flags byte, no CRC/restrictions) is exactly 6 bytes total, so the
        // size field's value here is 6 and only 2 more bytes should be
        // skipped after the size field. The old (pre-fix) logic skipped 6
        // more bytes after the size field too (misreading v2.4's "whole
        // header" size as "bytes after the size field", same as v2.3),
        // landing 4 bytes into the following TIT2 frame and misparsing it.
        let mut ext_header = Vec::new();
        ext_header.extend_from_slice(&synchsafe(6)); // extended header size: 6
        ext_header.push(0x01); // number of flag bytes
        ext_header.push(0x00); // extended flags: none set

        let mut body = Vec::new();
        body.extend_from_slice(&ext_header);
        body.extend_from_slice(&text_frame_v24(b"TIT2", "Moment's Notice"));

        let mut tag = Vec::new();
        tag.extend_from_slice(b"ID3");
        tag.extend_from_slice(&[4, 0, 0x40]); // v2.4, extended-header flag set
        tag.extend_from_slice(&synchsafe(body.len() as u32));
        tag.extend_from_slice(&body);

        let mut src = SliceSource::new(&tag);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        read_id3v2(&mut src, &mut scratch, &mut c).unwrap();
        assert!(
            c.fields
                .contains(&(Some(Field::Title), "TIT2".into(), "Moment's Notice".into()))
        );
    }

    #[test]
    fn picture_data_offset_is_absolute_for_an_embedded_tag() {
        // Regression: an ID3v2 tag embedded at a NON-ZERO file offset (as in a
        // WAV `id3 ` chunk or a DSF metadata pointer). The WAV/DSF readers seek
        // the whole-file source to the tag before calling, so `data_offset` must
        // be the ABSOLUTE position of the image bytes. The old code added a
        // `base_offset` on top of an already-absolute seek and pointed past EOF.
        use embedded_io::Seek as _;
        let tag = id3v23(&[apic("image/jpeg", 4, b"\xFF\xD8\xFF\xEE")]);
        let prefix = 1000usize;
        let mut buf = alloc::vec![0u8; prefix];
        buf.extend_from_slice(&tag);
        let mut src = SliceSource::new(&buf);
        src.seek(SeekFrom::Start(prefix as u64)).unwrap();
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        read_id3v2(&mut src, &mut scratch, &mut c).unwrap();
        assert_eq!(c.pics.len(), 1);
        assert_eq!(c.pics[0].usage, PictureKind::BackCover);
        assert_eq!(c.pics[0].mime, MimeType::Jpeg);
        let off = c.pics[0].data_offset as usize;
        assert!(
            off < buf.len(),
            "data_offset {off} must be within the file (len {})",
            buf.len()
        );
        assert_eq!(
            &buf[off..][..4],
            b"\xFF\xD8\xFF\xEE",
            "data_offset must point at the image bytes"
        );
    }
}
