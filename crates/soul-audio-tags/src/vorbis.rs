//! VorbisComment reader + FLAC container walk.
//!
//! `read_comment_vector` parses a Vorbis comment block *body* (vendor string +
//! `KEY=VALUE` pairs) and is reused verbatim for Ogg (Task 7) since that
//! format embeds the same body inside its own packet framing. `read_flac`
//! walks the FLAC METADATA_BLOCK sequence, extracting `StreamFacts` from
//! STREAMINFO and delegating to `read_comment_vector` / picture parsing for
//! VORBIS_COMMENT / PICTURE blocks.

use crate::util::{discard, read_exact, utf8_prefix};
use crate::{DataEncoding, Field, MimeType, Picture, PictureKind, StreamFacts, TagError, TagSink};
use embedded_io::SeekFrom;
use soul_audio_core::SeekSource;

/// Map a Vorbis comment key (case-insensitive) to a normalized [`Field`].
pub fn map_vorbis_key(key: &str) -> Option<Field> {
    if key.eq_ignore_ascii_case("TITLE") {
        Some(Field::Title)
    } else if key.eq_ignore_ascii_case("ARTIST") {
        Some(Field::Artist)
    } else if key.eq_ignore_ascii_case("ALBUM") {
        Some(Field::Album)
    } else if key.eq_ignore_ascii_case("ALBUMARTIST") {
        Some(Field::AlbumArtist)
    } else if key.eq_ignore_ascii_case("ARTISTSORT") {
        Some(Field::ArtistSort)
    } else if key.eq_ignore_ascii_case("ALBUMARTISTSORT") {
        Some(Field::AlbumArtistSort)
    } else if key.eq_ignore_ascii_case("TRACKNUMBER") {
        Some(Field::TrackNumber)
    } else if key.eq_ignore_ascii_case("TRACKTOTAL") || key.eq_ignore_ascii_case("TOTALTRACKS") {
        Some(Field::TrackTotal)
    } else if key.eq_ignore_ascii_case("DISCNUMBER") {
        Some(Field::DiscNumber)
    } else if key.eq_ignore_ascii_case("DATE") || key.eq_ignore_ascii_case("YEAR") {
        Some(Field::Date)
    } else if key.eq_ignore_ascii_case("GENRE") {
        Some(Field::Genre)
    } else if key.eq_ignore_ascii_case("COMPOSER") {
        Some(Field::Composer)
    } else {
        // ReplayGain / R128 keys (REPLAYGAIN_*, R128_*) share one mapping across
        // all comment-based containers.
        crate::util::map_replaygain_key(key)
    }
}

/// Debit `n` bytes from a per-block byte budget; a budget that goes negative
/// means the declared lengths inside the block overran the block's own
/// declared body length, i.e. the file is malformed.
fn charge(budget: &mut i64, n: u32) -> Result<(), TagError> {
    *budget -= i64::from(n);
    if *budget < 0 {
        Err(TagError::Malformed)
    } else {
        Ok(())
    }
}

/// Parse a Vorbis comment block *body* of `len` bytes: `u32 vendor_len`,
/// vendor bytes, `u32 count`, then `count` × (`u32 comment_len`, `KEY=VALUE`
/// UTF-8 bytes). Each comment is split on the first `=`; `scratch` holds one
/// comment at a time (`KEY=VALUE` truncated at a UTF-8 boundary if it doesn't
/// fit). Reused as-is for Ogg Vorbis/FLAC-in-Ogg comment headers.
pub fn read_comment_vector<S: embedded_io::Read>(
    src: &mut S,
    len: u32,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<(), TagError> {
    let mut budget = i64::from(len);
    let mut u32buf = [0u8; 4];

    charge(&mut budget, 4)?;
    read_exact(src, &mut u32buf)?;
    let vendor_len = u32::from_le_bytes(u32buf);
    charge(&mut budget, vendor_len)?;
    discard(src, vendor_len as usize)?;

    charge(&mut budget, 4)?;
    read_exact(src, &mut u32buf)?;
    let count = u32::from_le_bytes(u32buf);

    // 0-based counter of `METADATA_BLOCK_PICTURE` comments seen, stashed in the
    // emitted descriptor's `data_offset` so the (alloc) consumer can re-walk
    // the stream and pick out the Nth one to materialize its bytes.
    let mut pic_ordinal: u32 = 0;

    for _ in 0..count {
        charge(&mut budget, 4)?;
        read_exact(src, &mut u32buf)?;
        let comment_len = u32::from_le_bytes(u32buf);
        charge(&mut budget, comment_len)?;

        let comment_len = comment_len as usize;
        let cap = comment_len.min(scratch.len());
        read_exact(src, &mut scratch[..cap])?;
        if comment_len > cap {
            discard(src, comment_len - cap)?;
        }

        let text = utf8_prefix(&scratch[..cap]);
        if let Some(eq) = text.find('=') {
            let key = &text[..eq];
            let value = &text[eq + 1..];
            if key.eq_ignore_ascii_case("METADATA_BLOCK_PICTURE") {
                // A base64 FLAC PICTURE block: emit a descriptor from the
                // header sitting at the *start* of the (possibly truncated)
                // value prefix, and — like FLAC's native PICTURE handling —
                // do NOT also surface it as a text field. The image bytes are
                // never buffered here; the consumer decodes them on demand.
                if let Some(pic) = decode_picture_prefix(value.as_bytes(), pic_ordinal) {
                    sink.picture(&pic);
                }
                // Count every picture comment (even one whose prefix was too
                // short to emit) so later ordinals stay aligned with the
                // consumer's by-key walk.
                pic_ordinal = pic_ordinal.saturating_add(1);
            } else {
                sink.text(map_vorbis_key(key), key, value);
            }
        }
        // No `=` found (malformed comment, or it fell in the truncated tail):
        // silently skip it rather than guessing at a key/value split.
    }

    Ok(())
}

/// Read the 34-byte STREAMINFO body's sample rate / channels / total sample
/// count into `facts`. Cursor must be at the start of the STREAMINFO body;
/// the caller re-syncs to the declared block length afterward regardless of
/// how much this reads.
fn read_streaminfo<S: SeekSource>(src: &mut S, facts: &mut StreamFacts) -> Result<(), TagError> {
    // min/max block size (2+2) + min/max frame size (3+3) = 10 bytes, skipped.
    src.seek(SeekFrom::Current(10)).map_err(|_| TagError::Io)?;
    let mut buf = [0u8; 8];
    read_exact(src, &mut buf)?;

    let sample_rate =
        (u32::from(buf[0]) << 12) | (u32::from(buf[1]) << 4) | (u32::from(buf[2]) >> 4);
    let channels = ((buf[2] >> 1) & 0x7) + 1;
    let total_samples = (u64::from(buf[3] & 0xF) << 32)
        | u64::from(u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]));

    facts.sample_rate = Some(sample_rate);
    facts.channels = Some(u16::from(channels));
    facts.total_frames = Some(total_samples);
    Ok(())
}

fn mime_kind(bytes: &[u8]) -> MimeType {
    match bytes {
        b"image/jpeg" => MimeType::Jpeg,
        b"image/png" => MimeType::Png,
        _ => MimeType::Other,
    }
}

fn picture_kind(pic_type: u32) -> PictureKind {
    match pic_type {
        3 => PictureKind::FrontCover,
        4 => PictureKind::BackCover,
        8 => PictureKind::Artist,
        _ => PictureKind::Other,
    }
}

/// Read a big-endian `u32` at `pos` in `buf`, or `None` if the four bytes
/// aren't fully present (never panics / over-reads on a short prefix).
fn be_u32(buf: &[u8], pos: usize) -> Option<u32> {
    let end = pos.checked_add(4)?;
    let s = buf.get(pos..end)?;
    Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

/// Parse the *header* of a FLAC PICTURE block from a decoded base64 **prefix**
/// (see [`decode_picture_prefix`]). The prefix may be truncated anywhere, so
/// every field is read defensively and any field the prefix doesn't reach is
/// degraded (`width`/`height` = `None`, `data_len` = 0) rather than failing.
/// `ordinal` is stashed in `data_offset` so the consumer
/// ([`crate::ogg`]-side) can locate the Nth `METADATA_BLOCK_PICTURE` comment;
/// `encoding` is always [`DataEncoding::Base64`]. Returns `None` only when the
/// prefix can't even yield the leading `type` word.
fn picture_from_prefix(buf: &[u8], ordinal: u32) -> Option<Picture> {
    let pic_type = be_u32(buf, 0)?;
    let usage = picture_kind(pic_type);

    // Best-effort defaults if the prefix stops before a later field.
    let mut mime = MimeType::Other;
    let mut width = None;
    let mut height = None;
    let mut data_len = 0u32;

    // type(4) mime_len(4) mime desc_len(4) desc width(4) height(4) depth(4)
    // colors(4) data_len(4) — walk it, bailing to the degraded defaults the
    // moment a field runs past the decoded prefix.
    'reach: {
        let Some(mime_len) = be_u32(buf, 4) else {
            break 'reach;
        };
        let mime_start = 8usize;
        let Some(mime_end) = mime_start.checked_add(mime_len as usize) else {
            break 'reach;
        };
        let Some(mime_bytes) = buf.get(mime_start..mime_end) else {
            break 'reach;
        };
        mime = mime_kind(mime_bytes);

        let Some(desc_len) = be_u32(buf, mime_end) else {
            break 'reach;
        };
        let desc_end = mime_end.checked_add(4);
        let Some(after_desc) = desc_end.and_then(|p| p.checked_add(desc_len as usize)) else {
            break 'reach;
        };
        // width, height (depth/colors follow but aren't modeled on Picture).
        width = be_u32(buf, after_desc);
        height = be_u32(buf, after_desc.checked_add(4)?);
        // data_len sits after width(4) height(4) depth(4) colors(4).
        if let Some(dl_pos) = after_desc.checked_add(16) {
            data_len = be_u32(buf, dl_pos).unwrap_or(0);
        }
    }

    Some(Picture {
        usage,
        mime,
        encoding: DataEncoding::Base64,
        data_offset: u64::from(ordinal),
        data_len,
        width,
        height,
    })
}

/// Decode the leading whole base64 groups of a `METADATA_BLOCK_PICTURE` value
/// into a fixed **stack** buffer (no alloc — this runs on the no-alloc scan
/// path), then parse the FLAC PICTURE header out of the decoded prefix. Only
/// as many leading groups as fit the stack buffer are decoded, which is far
/// more than enough to reach the header fields (they sit at the very start of
/// the block, before the image bytes). Returns `None` on invalid base64 or a
/// prefix too short to yield even the `type` word.
fn decode_picture_prefix(value: &[u8], ordinal: u32) -> Option<Picture> {
    let mut hdr = [0u8; 512];
    // Cap the input to the whole base64 groups whose output fits `hdr`.
    let max_in = (hdr.len() / 3) * 4;
    let usable = value.len() - (value.len() % 4);
    let take = usable.min(max_in);
    let n = crate::base64::decode_into(value.get(..take)?, &mut hdr).ok()?;
    picture_from_prefix(&hdr[..n], ordinal)
}

/// Read a PICTURE block body: `u32 type, u32 mime_len, mime, u32 desc_len,
/// desc, u32 width, u32 height, u32 depth, u32 colors, u32 data_len, data`
/// (all big-endian). Records `data_offset` as the absolute source offset of
/// `data` without reading the image bytes themselves; the caller re-syncs
/// past `data` to the declared block length afterward.
fn read_picture<S: SeekSource>(src: &mut S, sink: &mut impl TagSink) -> Result<(), TagError> {
    let mut u32buf = [0u8; 4];

    read_exact(src, &mut u32buf)?;
    let pic_type = u32::from_be_bytes(u32buf);

    read_exact(src, &mut u32buf)?;
    let mime_len = u32::from_be_bytes(u32buf) as usize;
    let mut mime_buf = [0u8; 32];
    let mime_cap = mime_len.min(mime_buf.len());
    read_exact(src, &mut mime_buf[..mime_cap])?;
    if mime_len > mime_cap {
        src.seek(SeekFrom::Current((mime_len - mime_cap) as i64))
            .map_err(|_| TagError::Io)?;
    }
    let mime = mime_kind(&mime_buf[..mime_cap]);

    read_exact(src, &mut u32buf)?;
    let desc_len = u32::from_be_bytes(u32buf);
    src.seek(SeekFrom::Current(i64::from(desc_len)))
        .map_err(|_| TagError::Io)?;

    read_exact(src, &mut u32buf)?;
    let width = u32::from_be_bytes(u32buf);
    read_exact(src, &mut u32buf)?;
    let height = u32::from_be_bytes(u32buf);
    read_exact(src, &mut u32buf)?; // depth: not modeled on Picture
    read_exact(src, &mut u32buf)?; // colors (palette count): not modeled

    read_exact(src, &mut u32buf)?;
    let data_len = u32::from_be_bytes(u32buf);

    let data_offset = src.seek(SeekFrom::Current(0)).map_err(|_| TagError::Io)?;

    sink.picture(&Picture {
        usage: picture_kind(pic_type),
        mime,
        encoding: DataEncoding::Raw,
        data_offset,
        data_len,
        width: Some(width),
        height: Some(height),
    });
    Ok(())
}

/// Walk a FLAC container: the `fLaC` marker, then METADATA_BLOCKs until the
/// one whose header marks it last. Returns `StreamFacts` from STREAMINFO;
/// delivers text fields and picture descriptors to `sink` along the way.
pub fn read_flac<S: SeekSource>(
    src: &mut S,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<StreamFacts, TagError> {
    let mut magic = [0u8; 4];
    read_exact(src, &mut magic)?;
    if &magic != b"fLaC" {
        return Err(TagError::Malformed);
    }

    let mut facts = StreamFacts::default();

    loop {
        let mut header = [0u8; 4];
        read_exact(src, &mut header)?;
        let last = header[0] & 0x80 != 0;
        let block_type = header[0] & 0x7F;
        let body_len =
            (u32::from(header[1]) << 16) | (u32::from(header[2]) << 8) | u32::from(header[3]);

        let body_start = src.seek(SeekFrom::Current(0)).map_err(|_| TagError::Io)?;

        match block_type {
            0 => read_streaminfo(src, &mut facts)?,
            4 => read_comment_vector(src, body_len, scratch, sink)?,
            6 => read_picture(src, sink)?,
            _ => {}
        }

        // Re-sync to the block's declared end regardless of how many bytes
        // the handler above actually consumed — keeps a slightly-off parser
        // (or a deliberately unhandled block type) from desyncing the walk.
        let body_end = body_start
            .checked_add(u64::from(body_len))
            .ok_or(TagError::Malformed)?;
        src.seek(SeekFrom::Start(body_end))
            .map_err(|_| TagError::Io)?;

        if last {
            break;
        }
    }

    Ok(facts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DataEncoding, Field, MimeType, Picture, PictureKind};
    use soul_audio_core::SliceSource;
    extern crate alloc;
    use alloc::{borrow::ToOwned, string::String, vec::Vec};

    // Minimal FLAC: fLaC + STREAMINFO(min) + VORBIS_COMMENT + PICTURE(1x1 jpeg stub).
    fn flac(comments: &[&str], with_pic: bool) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"fLaC");
        // STREAMINFO (type 0), 34-byte body, not last.
        v.push(0x00);
        v.extend_from_slice(&[0, 0, 34]);
        let mut si = [0u8; 34];
        // The 64 bits at si[10..18] pack: sample_rate(20) | channels-1(3) | bps-1(5) | total_samples(36).
        let sr: u32 = 44_100;
        let ch: u32 = 2;
        let bps: u32 = 16;
        let total: u64 = 88_200;
        si[10] = (sr >> 12) as u8; // sample_rate bits 19..12
        si[11] = (sr >> 4) as u8; // sample_rate bits 11..4
        si[12] = (((sr & 0xF) as u8) << 4)                // sample_rate bits 3..0
               | ((((ch - 1) & 0x7) as u8) << 1)          // channels-1
               | (((bps - 1) >> 4) as u8); // bps-1 high bit
        si[13] = ((((bps - 1) & 0xF) as u8) << 4)         // bps-1 low 4 bits
               | (((total >> 32) & 0xF) as u8); // total_samples bits 35..32
        si[14] = (total >> 24) as u8; // total_samples bits 31..0
        si[15] = (total >> 16) as u8;
        si[16] = (total >> 8) as u8;
        si[17] = total as u8;
        v.extend_from_slice(&si);
        // VORBIS_COMMENT (type 4)
        let mut vc = Vec::new();
        vc.extend_from_slice(&(6u32).to_le_bytes());
        vc.extend_from_slice(b"vendor");
        vc.extend_from_slice(&(comments.len() as u32).to_le_bytes());
        for c in comments {
            vc.extend_from_slice(&(c.len() as u32).to_le_bytes());
            vc.extend_from_slice(c.as_bytes());
        }
        let last_vc = if with_pic { 0x04 } else { 0x84 };
        v.push(last_vc);
        v.extend_from_slice(&(vc.len() as u32).to_be_bytes()[1..]);
        v.extend_from_slice(&vc);
        if with_pic {
            let data = b"\xFF\xD8\xFF\xEE"; // jpeg stub
            let mut pic = Vec::new();
            pic.extend_from_slice(&(3u32).to_be_bytes()); // type 3 = front cover
            let mime = b"image/jpeg";
            pic.extend_from_slice(&(mime.len() as u32).to_be_bytes());
            pic.extend_from_slice(mime);
            pic.extend_from_slice(&(0u32).to_be_bytes()); // desc len
            pic.extend_from_slice(&(1u32).to_be_bytes()); // width
            pic.extend_from_slice(&(1u32).to_be_bytes()); // height
            pic.extend_from_slice(&(24u32).to_be_bytes()); // depth
            pic.extend_from_slice(&(0u32).to_be_bytes()); // colors
            pic.extend_from_slice(&(data.len() as u32).to_be_bytes());
            pic.extend_from_slice(data);
            v.push(0x86);
            v.extend_from_slice(&(pic.len() as u32).to_be_bytes()[1..]);
            v.extend_from_slice(&pic);
        }
        v
    }

    #[derive(Default)]
    struct Collect {
        fields: Vec<(Option<Field>, String, String)>,
        pics: Vec<Picture>,
    }
    impl crate::TagSink for Collect {
        fn text(&mut self, f: Option<Field>, k: &str, val: &str) {
            self.fields.push((f, k.to_owned(), val.to_owned()));
        }
        fn picture(&mut self, p: &Picture) {
            self.pics.push(*p);
        }
    }

    #[test]
    fn flac_fields_and_facts() {
        let bytes = flac(
            &[
                "TITLE=Blue Train",
                "ARTIST=John Coltrane",
                "ALBUM=Blue Train",
            ],
            false,
        );
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        let facts = read_flac(&mut src, &mut scratch, &mut c).unwrap();
        assert_eq!(facts.sample_rate, Some(44_100));
        assert_eq!(facts.channels, Some(2));
        assert_eq!(facts.total_frames, Some(88_200));
        assert!(
            c.fields
                .contains(&(Some(Field::Title), "TITLE".into(), "Blue Train".into()))
        );
        assert!(
            c.fields
                .contains(&(Some(Field::Artist), "ARTIST".into(), "John Coltrane".into()))
        );
    }

    #[test]
    fn flac_front_cover_descriptor() {
        let bytes = flac(&["TITLE=x"], true);
        let mut src = SliceSource::new(&bytes);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        read_flac(&mut src, &mut scratch, &mut c).unwrap();
        assert_eq!(c.pics.len(), 1);
        let p = c.pics[0];
        assert_eq!(p.usage, PictureKind::FrontCover);
        assert_eq!(p.mime, MimeType::Jpeg);
        assert_eq!(p.encoding, DataEncoding::Raw);
        assert_eq!(p.data_len, 4);
        assert_eq!(p.width, Some(1));
        // data_offset points at the 4 jpeg bytes:
        assert_eq!(&bytes[p.data_offset as usize..][..4], b"\xFF\xD8\xFF\xEE");
    }

    /// Build a bare Vorbis comment *body* (vendor + count + comments) — the
    /// input `read_comment_vector` parses, without any container framing.
    fn comment_body(comments: &[&str]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&6u32.to_le_bytes());
        v.extend_from_slice(b"vendor");
        v.extend_from_slice(&(comments.len() as u32).to_le_bytes());
        for c in comments {
            v.extend_from_slice(&(c.len() as u32).to_le_bytes());
            v.extend_from_slice(c.as_bytes());
        }
        v
    }

    #[test]
    fn metadata_block_picture_emits_base64_descriptor_not_text() {
        // A full, untruncated value: the prefix reaches every header field.
        let png = b"\x89PNG\r\n\x1a\ntiny-image-bytes";
        let comment = crate::testfix::metadata_block_picture(3, b"image/png", 1, 1, png);
        let body = comment_body(&["TITLE=Song", &comment]);

        let mut src = SliceSource::new(&body);
        let mut scratch = [0u8; 512];
        let mut c = Collect::default();
        read_comment_vector(&mut src, body.len() as u32, &mut scratch, &mut c).unwrap();

        assert_eq!(c.pics.len(), 1);
        let p = c.pics[0];
        assert_eq!(p.usage, PictureKind::FrontCover);
        assert_eq!(p.mime, MimeType::Png);
        assert_eq!(p.encoding, DataEncoding::Base64);
        assert_eq!(p.data_offset, 0); // ordinal 0
        assert_eq!(p.data_len, png.len() as u32);
        assert_eq!(p.width, Some(1));
        assert_eq!(p.height, Some(1));

        // The picture comment must NOT also surface as a text field...
        assert!(
            !c.fields
                .iter()
                .any(|(_, k, _)| k.eq_ignore_ascii_case("METADATA_BLOCK_PICTURE"))
        );
        // ...but ordinary fields around it still do.
        assert!(c.fields.iter().any(|(_, k, v)| k == "TITLE" && v == "Song"));
    }

    #[test]
    fn metadata_block_picture_second_picture_gets_ordinal_one() {
        let a = crate::testfix::metadata_block_picture(3, b"image/png", 1, 1, b"aaaa");
        let b = crate::testfix::metadata_block_picture(4, b"image/jpeg", 2, 2, b"bbbb");
        let body = comment_body(&[&a, "TITLE=x", &b]);

        let mut src = SliceSource::new(&body);
        let mut scratch = [0u8; 512];
        let mut c = Collect::default();
        read_comment_vector(&mut src, body.len() as u32, &mut scratch, &mut c).unwrap();

        assert_eq!(c.pics.len(), 2);
        assert_eq!(c.pics[0].data_offset, 0);
        assert_eq!(c.pics[0].usage, PictureKind::FrontCover);
        assert_eq!(c.pics[1].data_offset, 1);
        assert_eq!(c.pics[1].usage, PictureKind::BackCover);
        assert_eq!(c.pics[1].mime, MimeType::Jpeg);
    }

    #[test]
    fn metadata_block_picture_truncated_prefix_degrades_gracefully() {
        // A scratch too small to reach width/data_len: the descriptor still
        // carries the leading fields (usage) and degrades the rest.
        let png = [0u8; 64];
        let comment = crate::testfix::metadata_block_picture(3, b"image/png", 7, 7, &png);
        let body = comment_body(&[&comment]);

        let mut src = SliceSource::new(&body);
        let mut scratch = [0u8; 40]; // < the ~30-byte header prefix + key
        let mut c = Collect::default();
        read_comment_vector(&mut src, body.len() as u32, &mut scratch, &mut c).unwrap();

        assert_eq!(c.pics.len(), 1);
        let p = c.pics[0];
        assert_eq!(p.usage, PictureKind::FrontCover);
        assert_eq!(p.encoding, DataEncoding::Base64);
        assert_eq!(p.width, None);
        assert_eq!(p.height, None);
        assert_eq!(p.data_len, 0);
    }

    #[test]
    fn metadata_block_picture_malformed_base64_emits_no_picture() {
        // Invalid base64 characters: decode fails, no picture, no panic.
        let body = comment_body(&["METADATA_BLOCK_PICTURE=****not valid base64****"]);
        let mut src = SliceSource::new(&body);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        read_comment_vector(&mut src, body.len() as u32, &mut scratch, &mut c).unwrap();
        assert!(c.pics.is_empty());
        assert!(c.fields.is_empty());
    }

    #[test]
    fn metadata_block_picture_too_short_to_reach_type_emits_no_picture() {
        // "AA==" decodes to a single byte — not enough for the 4-byte type.
        let body = comment_body(&["METADATA_BLOCK_PICTURE=AA=="]);
        let mut src = SliceSource::new(&body);
        let mut scratch = [0u8; 256];
        let mut c = Collect::default();
        read_comment_vector(&mut src, body.len() as u32, &mut scratch, &mut c).unwrap();
        assert!(c.pics.is_empty());
    }
}
