//! Play from the SD-baked `.soulidx` — the DAP's half of "desktop bakes, device
//! reads".
//!
//! The desktop scans a library and bakes `/.soulidx/index.sidx` onto the card
//! (see `soul-audio-import`); the device only ever **reads** it. This module is
//! that read: open the index over the card's byte source, walk the catalog, and
//! locate a track's physical coordinates (`folder / file`) so the decoder can
//! open it. It is `no_std` + **no-alloc** — every string is copied into a fixed
//! caller stack buffer, exactly as it must run on the H743.
//!
//! Status, mirroring `sd_source`: the logic here is complete and **links into the
//! thumbv7em binary** (it is generic over any [`SeekSource`], so the host tests
//! and the device share it 1:1). The one thing that still waits for a board is
//! *constructing* the card source — the SPI / `embedded-sdmmc` `VolumeManager`
//! bring-up in `sd_source`. Until that lands, `main` streams a test tone and this
//! path is compiled and type-checked but not yet driven from hardware.

#![allow(dead_code)] // driven once the board SD stack (sd_source) is wired

use soul_audio_catalog::Catalog;
use soul_audio_core::SeekSource;
use soul_audio_index::IndexReader;

/// Longest on-card relative folder path the DAP will reconstruct (`/`-separated,
/// relative to the card root). A handful of nested album folders fit easily.
pub const MAX_FOLDER: usize = 256;
/// Longest leaf file name the DAP will open (e.g. `01 - Aria.flac`).
pub const MAX_FILE: usize = 128;

/// A located track's physical coordinates on the card, copied into this
/// struct's own fixed buffers — **no heap**. The DAP opens the audio file at
/// `card_root / folder() / file()` and decodes it with the stream facts below.
pub struct Located {
    folder: [u8; MAX_FOLDER],
    folder_len: usize,
    file: [u8; MAX_FILE],
    file_len: usize,
    /// PCM frame rate from the baked record (0 if the desktop had no stream facts).
    pub sample_rate: u32,
    /// Interleaved channel count from the baked record.
    pub channels: u8,
}

impl Located {
    /// The folder path relative to the card root, `/`-separated (e.g.
    /// `Classical/Bach`); empty for a track sitting at the card root.
    pub fn folder(&self) -> &str {
        core::str::from_utf8(&self.folder[..self.folder_len]).unwrap_or("")
    }

    /// The leaf file name including extension (e.g. `01 - Aria.flac`).
    pub fn file(&self) -> &str {
        core::str::from_utf8(&self.file[..self.file_len]).unwrap_or("")
    }
}

/// Read the baked index over the card and locate track `id` — its folder path,
/// file name, and stream facts — without allocating. Returns `None` if `id` is
/// out of range or a record / string read fails (a corrupt or truncated card).
///
/// `S` is the card's [`crate::sd_source::SdSource`] on hardware; in host tests it
/// is any seekable byte source, which is precisely why the format is shared.
pub fn locate<S: SeekSource>(reader: &IndexReader<S>, id: u32) -> Option<Located> {
    if id >= reader.track_count() {
        return None;
    }
    let rec = reader.track(id).ok()?;
    let folder = reader.folder(rec.folder).ok()?;

    let mut out = Located {
        folder: [0; MAX_FOLDER],
        folder_len: 0,
        file: [0; MAX_FILE],
        file_len: 0,
        sample_rate: rec.sample_rate,
        channels: rec.channels,
    };
    out.folder_len = reader.resolve_str(folder.path, &mut out.folder).ok()?;
    out.file_len = reader.resolve_str(rec.file_name, &mut out.file).ok()?;
    Some(out)
}

/// Number of tracks on the card's baked index — the DAP's library size, read with
/// a single seek. A convenience wrapper so `main` need not import [`Catalog`].
pub fn track_count<S: SeekSource>(reader: &IndexReader<S>) -> u32 {
    reader.track_count()
}
