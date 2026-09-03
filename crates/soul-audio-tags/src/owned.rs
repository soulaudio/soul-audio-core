//! `std`/`alloc` convenience layer: an owned [`Tags`] snapshot collected over
//! the no-alloc streaming [`crate::read`], plus a helper that materializes a
//! [`Picture`](crate::Picture)'s bytes. Entirely behind `feature = "alloc"` —
//! the no-alloc core stays `no_std` regardless.

use alloc::string::String;
use alloc::vec::Vec;

use embedded_io::SeekFrom;
use soul_audio_core::SeekSource;

use crate::util::read_exact;
use crate::{DataEncoding, Field, Picture, StreamFacts, TagError, TagSink, read};

/// An owned tag snapshot — one heap-allocated value that outlives the read,
/// unlike the borrowed values a [`TagSink`] streams field-by-field.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track: Option<u32>,
    pub track_total: Option<u32>,
    pub disc: Option<u32>,
    pub disc_total: Option<u32>,
    pub date: Option<String>,
    pub genre: Option<String>,
    pub composer: Option<String>,
    pub pictures: Vec<Picture>,
}

/// [`TagSink`] that collects into an owned [`Tags`].
#[derive(Default)]
struct OwnedTags {
    tags: Tags,
}

impl TagSink for OwnedTags {
    /// Vorbis comments (FLAC/Ogg) explicitly allow repeated keys — real files
    /// in the wild carry e.g. two `ARTIST=` entries for a collaboration
    /// ("ARTIST=Jiro Inagaki" then "ARTIST=Soul Media" for "Jiro Inagaki &
    /// Soul Media"). A single-valued snapshot has to pick one; this keeps the
    /// *first* occurrence and ignores later duplicates for the same field,
    /// matching `lofty`'s `Tag::get` (`items().find(...)`, first match wins)
    /// and general tagging convention (the first value is the canonical
    /// single one; later repeats are considered supplementary).
    fn text(&mut self, field: Option<Field>, _raw_key: &str, value: &str) {
        let Some(field) = field else { return };
        match field {
            Field::Title => {
                self.tags.title.get_or_insert_with(|| value.into());
            }
            Field::Artist => {
                self.tags.artist.get_or_insert_with(|| value.into());
            }
            Field::Album => {
                self.tags.album.get_or_insert_with(|| value.into());
            }
            Field::AlbumArtist => {
                self.tags.album_artist.get_or_insert_with(|| value.into());
            }
            Field::Date => {
                self.tags.date.get_or_insert_with(|| value.into());
            }
            Field::Genre => {
                self.tags.genre.get_or_insert_with(|| value.into());
            }
            Field::Composer => {
                self.tags.composer.get_or_insert_with(|| value.into());
            }
            Field::TrackNumber if self.tags.track.is_none() => {
                self.tags.track = value.parse().ok();
            }
            Field::TrackTotal if self.tags.track_total.is_none() => {
                self.tags.track_total = value.parse().ok();
            }
            Field::DiscNumber if self.tags.disc.is_none() => {
                self.tags.disc = value.parse().ok();
            }
            Field::DiscTotal if self.tags.disc_total.is_none() => {
                self.tags.disc_total = value.parse().ok();
            }
            Field::TrackNumber | Field::TrackTotal | Field::DiscNumber | Field::DiscTotal => {}
        }
    }

    fn picture(&mut self, pic: &Picture) {
        self.tags.pictures.push(*pic);
    }
}

/// Read tags + facts from `src`, collecting into an owned [`Tags`] (a 256-byte
/// scratch buffer backs the underlying streaming [`read`]).
pub fn read_owned<S: SeekSource>(src: &mut S) -> Result<(Tags, StreamFacts), TagError> {
    let mut scratch = [0u8; 256];
    let mut sink = OwnedTags::default();
    let facts = read(src, &mut scratch, &mut sink)?;
    Ok((sink.tags, facts))
}

/// Materialize the bytes a [`Picture`] describes: seeks to `p.data_offset`
/// and reads exactly `p.data_len` bytes.
///
/// Only [`DataEncoding::Raw`] is implemented — every reader in this crate
/// (FLAC `PICTURE`, ID3v2 `APIC`, MP4 `covr`) currently emits raw picture
/// data. Ogg's `METADATA_BLOCK_PICTURE` comment value is base64-encoded
/// ([`DataEncoding::Base64`]); decoding it is a paired follow-up (a producer
/// in `ogg.rs` that emits a `Base64` [`Picture`], plus a
/// [`crate::decode_into`]-based consumer here) — no reader emits one yet, so
/// that path is intentionally left unimplemented (YAGNI).
pub fn read_picture_bytes<S: SeekSource>(src: &mut S, p: &Picture) -> Result<Vec<u8>, TagError> {
    if p.encoding != DataEncoding::Raw {
        // TODO: Ogg embedded-picture (METADATA_BLOCK_PICTURE) support: a
        // producer in ogg.rs emitting DataEncoding::Base64, paired with a
        // base64::decode_into-based consumer here.
        return Err(TagError::Unsupported);
    }
    src.seek(SeekFrom::Start(p.data_offset))
        .map_err(|_| TagError::Io)?;
    let mut buf = alloc::vec![0u8; p.data_len as usize];
    read_exact(src, &mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use soul_audio_core::SliceSource;

    // Minimal FLAC: fLaC + STREAMINFO(min) + VORBIS_COMMENT(`comments`) +
    // PICTURE(1x1 jpeg stub, front cover). Mirrors the Task 3 fixture in
    // vorbis.rs's own tests (not reusable across modules — that helper is
    // private).
    fn flac_fixture(comments: &[&str]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"fLaC");
        // STREAMINFO (type 0), 34-byte body, not last.
        v.push(0x00);
        v.extend_from_slice(&[0, 0, 34]);
        let mut si = [0u8; 34];
        let sr: u32 = 44_100;
        let ch: u32 = 2;
        let bps: u32 = 16;
        let total: u64 = 88_200;
        si[10] = (sr >> 12) as u8;
        si[11] = (sr >> 4) as u8;
        si[12] =
            (((sr & 0xF) as u8) << 4) | ((((ch - 1) & 0x7) as u8) << 1) | (((bps - 1) >> 4) as u8);
        si[13] = ((((bps - 1) & 0xF) as u8) << 4) | (((total >> 32) & 0xF) as u8);
        si[14] = (total >> 24) as u8;
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
        v.push(0x04); // not last (a PICTURE block follows)
        v.extend_from_slice(&(vc.len() as u32).to_be_bytes()[1..]);
        v.extend_from_slice(&vc);
        // PICTURE (type 6), front cover, 1x1 jpeg stub, last block.
        let data = b"\xFF\xD8\xFF\xEE";
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
        v.push(0x86); // last, type 6
        v.extend_from_slice(&(pic.len() as u32).to_be_bytes()[1..]);
        v.extend_from_slice(&pic);
        v
    }

    #[test]
    fn read_owned_collects_flac_tags_and_front_cover_bytes() {
        let bytes = flac_fixture(&[
            "TITLE=Blue Train",
            "ARTIST=John Coltrane",
            "ALBUM=Blue Train",
        ]);
        let mut src = SliceSource::new(&bytes);

        let (tags, facts) = read_owned(&mut src).unwrap();

        assert_eq!(tags.title.as_deref(), Some("Blue Train"));
        assert_eq!(tags.artist.as_deref(), Some("John Coltrane"));
        assert_eq!(tags.album.as_deref(), Some("Blue Train"));
        assert_eq!(facts.sample_rate, Some(44_100));
        assert_eq!(facts.channels, Some(2));
        assert_eq!(tags.pictures.len(), 1);

        let mut src2 = SliceSource::new(&bytes);
        let pic_bytes = read_picture_bytes(&mut src2, &tags.pictures[0]).unwrap();
        assert_eq!(pic_bytes, b"\xFF\xD8\xFF\xEE");
    }

    #[test]
    fn read_owned_keeps_first_value_for_a_repeated_vorbis_comment_key() {
        // Regression test for a real-world corpus finding (Task 9's
        // differential test vs `lofty`): Vorbis comments explicitly allow
        // repeated keys, and real FLAC files in the wild use it for
        // multi-artist collaborations — e.g. "ARTIST=Jiro Inagaki" then
        // "ARTIST=Soul Media" for "Jiro Inagaki & Soul Media". `lofty`'s
        // `Tag::get` (first match wins) reports the first, so `read_owned`
        // must too; it previously overwrote with the *last* occurrence.
        let bytes = flac_fixture(&[
            "ARTIST=Jiro Inagaki",
            "TITLE=Painted Paradise",
            "ALBUM=Funky Stuff",
            "ARTIST=Soul Media",
        ]);
        let mut src = SliceSource::new(&bytes);

        let (tags, _facts) = read_owned(&mut src).unwrap();

        assert_eq!(tags.artist.as_deref(), Some("Jiro Inagaki"));
    }
}
