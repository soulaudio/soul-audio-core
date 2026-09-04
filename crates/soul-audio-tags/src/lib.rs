//! no_std, no-alloc audio tag + cover-art reader. See the design spec.
#![cfg_attr(not(test), no_std)] // matches soul-audio-core; std is available under `cargo test`
#![forbid(unsafe_code)]

use soul_audio_core::SeekSource;

#[cfg(feature = "alloc")]
extern crate alloc;

/// No-alloc base64 decode, exposed for the desktop layer to decode a
/// [`Picture`] whose `encoding` is [`DataEncoding::Base64`] (an Ogg
/// `METADATA_BLOCK_PICTURE` comment value) into raw picture bytes.
pub use base64::decode_into;
#[cfg(feature = "alloc")]
pub use owned::{Tags, read_owned, read_picture_bytes};

mod base64;
mod dsf;
mod id3v2;
mod locate;
mod mp3;
mod mp4;
mod ogg;
#[cfg(feature = "alloc")]
mod owned;
mod riff;
#[cfg(test)]
pub(crate) mod testfix;
mod util;
mod vorbis;

/// Normalized tag field, modeled on Symphonia's StandardTag (player subset).
#[non_exhaustive]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Title,
    Artist,
    Album,
    AlbumArtist,
    TrackNumber,
    TrackTotal,
    DiscNumber,
    DiscTotal,
    Date,
    Genre,
    Composer,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MimeType {
    Jpeg,
    Png,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PictureKind {
    FrontCover,
    BackCover,
    Artist,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DataEncoding {
    Raw,
    Base64,
}

/// A cover-art *descriptor* — never the bytes. `Copy`, no borrows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Picture {
    pub usage: PictureKind,
    pub mime: MimeType,
    pub encoding: DataEncoding,
    pub data_offset: u64,
    pub data_len: u32,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Cheap stream facts read from the container header during the same walk.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StreamFacts {
    pub total_frames: Option<u64>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
}

/// Streaming visitor. `value`/`raw_key` borrow internal scratch — copy what you keep.
pub trait TagSink {
    fn text(&mut self, field: Option<Field>, raw_key: &str, value: &str);
    fn picture(&mut self, pic: &Picture);
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TagError {
    Io,
    Malformed,
    Unsupported,
}

impl From<embedded_io::ErrorKind> for TagError {
    fn from(_: embedded_io::ErrorKind) -> Self {
        TagError::Io
    }
}

/// Read tags + facts from `src`. `scratch` holds one field value at a time.
pub fn read<S: SeekSource>(
    src: &mut S,
    scratch: &mut [u8],
    sink: &mut impl TagSink,
) -> Result<StreamFacts, TagError> {
    match locate::sniff(src)? {
        locate::Container::Flac => vorbis::read_flac(src, scratch, sink),
        locate::Container::Id3 => id3v2::read_mp3(src, scratch, sink),
        locate::Container::Wav => riff::read_wav(src, scratch, sink),
        locate::Container::Dsf => dsf::read_dsf(src, scratch, sink),
        locate::Container::Mp4 => mp4::read_mp4(src, scratch, sink),
        locate::Container::Ogg => ogg::read_ogg(src, scratch, sink),
        _ => Err(TagError::Unsupported),
    }
}
