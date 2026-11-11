//! `std`/`alloc` convenience layer: an owned [`Tags`] snapshot collected over
//! the no-alloc streaming [`crate::read`], plus a helper that materializes a
//! [`Picture`](crate::Picture)'s bytes. Entirely behind `feature = "alloc"` —
//! the no-alloc core stays `no_std` regardless.

use alloc::string::String;
use alloc::vec::Vec;

use embedded_io::SeekFrom;
use soul_audio_core::SeekSource;

use crate::util::{parse_gain_db, parse_peak, parse_r128_gain_db, read_exact};
use crate::{DataEncoding, Field, Picture, StreamFacts, TagError, TagSink, read};

/// An owned tag snapshot — one heap-allocated value that outlives the read,
/// unlike the borrowed values a [`TagSink`] streams field-by-field.
///
/// # ReplayGain / R128 loudness
/// `track_gain_db` / `album_gain_db` hold a loudness-normalization gain in dB,
/// and `track_peak` / `album_peak` the linear full-scale peak (`1.0` = 0 dBFS).
/// The gain is stored as-parsed, **without** reconciling reference levels:
/// ReplayGain tags reference ~ -18 LUFS (RG2) / 83 dB SPL (RG1), while R128
/// tags (`R128_*_GAIN`, EBU R128) reference -23 LUFS, so a consumer normalizing
/// to a specific target may need to offset. When a track carries both a
/// ReplayGain and an R128 tag, the ReplayGain value wins. `Tags` is not `Eq`
/// (the gain/peak fields are `f32`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub artist_sort: Option<String>,
    pub album_artist_sort: Option<String>,
    pub track: Option<u32>,
    pub track_total: Option<u32>,
    pub disc: Option<u32>,
    pub disc_total: Option<u32>,
    pub date: Option<String>,
    pub genre: Option<String>,
    pub composer: Option<String>,
    pub pictures: Vec<Picture>,
    /// ReplayGain/R128 track gain, in dB. `None` if untagged.
    pub track_gain_db: Option<f32>,
    /// ReplayGain/R128 album gain, in dB. `None` if untagged.
    pub album_gain_db: Option<f32>,
    /// ReplayGain track peak — linear full-scale (`1.0` = 0 dBFS).
    pub track_peak: Option<f32>,
    /// ReplayGain album peak — linear full-scale (`1.0` = 0 dBFS).
    pub album_peak: Option<f32>,
}

/// [`TagSink`] that collects into an owned [`Tags`].
#[derive(Default)]
struct OwnedTags {
    tags: Tags,
    /// Whether the track/album gain came from a ReplayGain tag (vs R128). Set so
    /// a ReplayGain value always wins over an R128 one regardless of file order:
    /// an R128 gain is only stored while this is `false`.
    track_gain_is_rg: bool,
    album_gain_is_rg: bool,
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
            Field::ArtistSort => {
                self.tags.artist_sort.get_or_insert_with(|| value.into());
            }
            Field::AlbumArtistSort => {
                self.tags
                    .album_artist_sort
                    .get_or_insert_with(|| value.into());
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
            // ReplayGain wins over R128: keep the first ReplayGain gain and let
            // it override any R128 one already stored; only accept an R128 gain
            // while no ReplayGain gain has been seen.
            Field::ReplayGainTrackGain if !self.track_gain_is_rg => {
                if let Some(g) = parse_gain_db(value) {
                    self.tags.track_gain_db = Some(g);
                    self.track_gain_is_rg = true;
                }
            }
            Field::ReplayGainAlbumGain if !self.album_gain_is_rg => {
                if let Some(g) = parse_gain_db(value) {
                    self.tags.album_gain_db = Some(g);
                    self.album_gain_is_rg = true;
                }
            }
            Field::R128TrackGain if !self.track_gain_is_rg && self.tags.track_gain_db.is_none() => {
                self.tags.track_gain_db = parse_r128_gain_db(value);
            }
            Field::R128AlbumGain if !self.album_gain_is_rg && self.tags.album_gain_db.is_none() => {
                self.tags.album_gain_db = parse_r128_gain_db(value);
            }
            Field::ReplayGainTrackPeak if self.tags.track_peak.is_none() => {
                self.tags.track_peak = parse_peak(value);
            }
            Field::ReplayGainAlbumPeak if self.tags.album_peak.is_none() => {
                self.tags.album_peak = parse_peak(value);
            }
            Field::TrackNumber
            | Field::TrackTotal
            | Field::DiscNumber
            | Field::DiscTotal
            | Field::ReplayGainTrackGain
            | Field::ReplayGainAlbumGain
            | Field::ReplayGainTrackPeak
            | Field::ReplayGainAlbumPeak
            | Field::R128TrackGain
            | Field::R128AlbumGain => {}
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

/// Materialize the bytes a [`Picture`] describes.
///
/// - [`DataEncoding::Raw`] (FLAC `PICTURE`, ID3v2 `APIC`, MP4 `covr`): seek to
///   `p.data_offset` and read exactly `p.data_len` bytes.
/// - [`DataEncoding::Base64`] (Ogg `METADATA_BLOCK_PICTURE`): `p.data_offset`
///   is the picture *ordinal*, not a byte offset — re-walk the Ogg stream,
///   base64-decode the Nth picture comment (reassembled across pages), and
///   return the embedded FLAC PICTURE block's image bytes.
pub fn read_picture_bytes<S: SeekSource>(src: &mut S, p: &Picture) -> Result<Vec<u8>, TagError> {
    match p.encoding {
        DataEncoding::Raw => {
            src.seek(SeekFrom::Start(p.data_offset))
                .map_err(|_| TagError::Io)?;
            let mut buf = alloc::vec![0u8; p.data_len as usize];
            read_exact(src, &mut buf)?;
            Ok(buf)
        }
        DataEncoding::Base64 => crate::ogg::read_ogg_picture(src, p.data_offset),
    }
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

    #[test]
    fn read_owned_collects_vorbis_sort_names() {
        let bytes = flac_fixture(&[
            "ARTIST=Eric Clapton",
            "ARTISTSORT=Clapton, Eric",
            "ALBUMARTIST=Eric Clapton",
            "ALBUMARTISTSORT=Clapton, Eric",
            "ALBUM=Slowhand",
        ]);
        let mut src = SliceSource::new(&bytes);

        let (tags, _facts) = read_owned(&mut src).unwrap();

        assert_eq!(tags.artist.as_deref(), Some("Eric Clapton"));
        assert_eq!(tags.artist_sort.as_deref(), Some("Clapton, Eric"));
        assert_eq!(tags.album_artist_sort.as_deref(), Some("Clapton, Eric"));
    }

    #[test]
    fn read_owned_parses_replaygain_vorbis_comments() {
        let bytes = flac_fixture(&[
            "TITLE=Blue Train",
            "REPLAYGAIN_TRACK_GAIN=-7.89 dB",
            "REPLAYGAIN_ALBUM_GAIN=-8.52 dB",
            "REPLAYGAIN_TRACK_PEAK=0.988",
            "REPLAYGAIN_ALBUM_PEAK=0.995",
        ]);
        let mut src = SliceSource::new(&bytes);
        let (tags, _facts) = read_owned(&mut src).unwrap();

        assert_eq!(tags.track_gain_db, Some(-7.89));
        assert_eq!(tags.album_gain_db, Some(-8.52));
        assert_eq!(tags.track_peak, Some(0.988));
        assert_eq!(tags.album_peak, Some(0.995));
    }

    #[test]
    fn read_owned_reads_r128_gain_when_no_replaygain() {
        // Opus/Vorbis R128_*_GAIN is a Q7.8 integer: -2314 / 256 = -9.039 dB.
        let bytes = flac_fixture(&["TITLE=X", "R128_TRACK_GAIN=-2314", "R128_ALBUM_GAIN=512"]);
        let mut src = SliceSource::new(&bytes);
        let (tags, _facts) = read_owned(&mut src).unwrap();

        assert!((tags.track_gain_db.unwrap() - (-9.04)).abs() < 0.01);
        assert_eq!(tags.album_gain_db, Some(2.0)); // 512 / 256
    }

    #[test]
    fn read_owned_prefers_replaygain_over_r128_either_order() {
        // RG wins over R128 regardless of which appears first in the stream.
        let rg_first = flac_fixture(&["REPLAYGAIN_TRACK_GAIN=-6.00 dB", "R128_TRACK_GAIN=-2314"]);
        let (tags, _) = read_owned(&mut SliceSource::new(&rg_first)).unwrap();
        assert_eq!(tags.track_gain_db, Some(-6.0), "RG first");

        let r128_first = flac_fixture(&["R128_TRACK_GAIN=-2314", "REPLAYGAIN_TRACK_GAIN=-6.00 dB"]);
        let (tags, _) = read_owned(&mut SliceSource::new(&r128_first)).unwrap();
        assert_eq!(tags.track_gain_db, Some(-6.0), "R128 first, RG still wins");
    }

    #[test]
    fn read_owned_parses_replaygain_id3_txxx() {
        use crate::testfix::{id3v23, txxx_frame};
        let tag = id3v23(&[
            txxx_frame("replaygain_track_gain", "-7.89 dB"),
            txxx_frame("replaygain_track_peak", "0.988"),
        ]);
        let mut src = SliceSource::new(&tag);
        let (tags, _facts) = read_owned(&mut src).unwrap();

        assert_eq!(tags.track_gain_db, Some(-7.89));
        assert_eq!(tags.track_peak, Some(0.988));
    }

    /// End-to-end acceptance for issue #26: a real `opusenc`-produced Opus file
    /// carrying a `METADATA_BLOCK_PICTURE` cover must surface a [`Picture`]
    /// (so `has_art` is true) and hand back the exact embedded image bytes
    /// through the full public `read_owned` + `read_picture_bytes` path.
    #[test]
    fn opus_fixture_with_embedded_art_reports_has_art_and_returns_the_image() {
        use crate::{MimeType, PictureKind};

        let data = include_bytes!("../testdata/opus-cover.opus");

        let mut src = SliceSource::new(data);
        let (tags, _facts) = read_owned(&mut src).unwrap();

        // `has_art`: the embedded cover surfaces as a Picture descriptor.
        assert!(
            !tags.pictures.is_empty(),
            "embedded Ogg cover must surface as a Picture (has_art)"
        );
        let pic = tags
            .pictures
            .iter()
            .find(|p| matches!(p.usage, PictureKind::FrontCover))
            .unwrap_or(&tags.pictures[0]);
        assert_eq!(
            pic.encoding,
            DataEncoding::Base64,
            "Ogg pictures are lazy base64 descriptors"
        );

        // The bytes round-trip and their leading magic matches the declared MIME.
        let mut src2 = SliceSource::new(data);
        let img = read_picture_bytes(&mut src2, pic).unwrap();
        assert!(!img.is_empty(), "decoded cover image must be non-empty");
        match pic.mime {
            MimeType::Png => {
                assert_eq!(&img[..8], b"\x89PNG\r\n\x1a\n", "PNG magic");
            }
            MimeType::Jpeg => {
                assert_eq!(&img[..3], b"\xFF\xD8\xFF", "JPEG magic");
            }
            MimeType::Other => {}
        }
    }
}
