//! Shared music-catalog model + **ports**.
//!
//! This crate is the no_std, no-alloc heart of the data layer: the domain ids,
//! the Tier-1 record shapes, and the traits (`Catalog`, `ArtStore`) that the
//! desktop (in-RAM) and the DAP (`.soulidx` reader) both implement. It owns no
//! I/O and no persistence — those are adapters (hexagonal ports & adapters).
//!
//! The `Catalog` port is deliberately **pull/query** and buffer-based: it never
//! returns an owned collection, so it survives the DAP's no-alloc gate and maps
//! onto paged SD reads. See the design spec:
//! `docs/superpowers/specs/2026-11-08-shared-catalog-index-architecture.md`.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

// (The `alloc`/`std` features are reserved for a future desktop in-RAM model +
// grouping logic; the ports + record shapes here are no-alloc.)

/// Stable record id = position in the index record table. Compact integers, not
/// UUID strings (see the spec's "lessons avoided").
pub type TrackId = u32;
pub type AlbumId = u32;
pub type ArtistId = u32;
pub type GenreId = u32;
pub type FolderId = u32;
pub type PlaylistId = u32;

/// Sentinel [`FolderRec::parent`] meaning "no parent" — a top-level folder whose
/// parent is the card root itself. (Chosen as `FolderId::MAX` so it can never
/// collide with a real dense folder id.)
pub const NO_FOLDER: FolderId = FolderId::MAX;

/// A reference into the index's UTF-8 string heap: `(offset, len)`. The reader
/// copies the bytes into a caller-provided buffer on demand — no allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct StrRef {
    pub off: u32,
    pub len: u16,
}

/// Tier-1 track record (the browse set). `Copy`; string fields are heap refs
/// resolved via [`Catalog::resolve_str`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct TrackRec {
    pub id: TrackId,
    pub title: StrRef,
    pub artist: ArtistId,
    pub album: AlbumId,
    pub album_artist: ArtistId,
    /// Genre entity referenced by this track; resolves to a display name via
    /// [`Catalog::genre_name`] and keys the [`BrowseAxis::Genre`] order.
    pub genre: GenreId,
    pub folder: FolderId,
    pub track_no: u16,
    pub disc_no: u16,
    pub duration_ms: u32,
    pub sample_rate: u32,
    pub channels: u8,
    pub art_id: u32,
    pub flags: u16,
    /// Byte offset of this track's Tier-2/Tier-3 TLV in `extended.blob`, or 0.
    pub extended_off: u32,
    /// Leaf file name (e.g. `01 - Aria.flac`), a heap ref. The physical file is
    /// `card_root / folder(self.folder).path / file_name` — this is what makes a
    /// track locatable and playable.
    pub file_name: StrRef,
    /// MusicBrainz **recording** MBID, a heap ref (empty `StrRef` when the file
    /// carried none). The stable identity key an enrichment pass joins on. (New in
    /// format v4.)
    pub mbid: StrRef,
}

/// Tier-1 folder record. `path` is the folder's location **relative to the card
/// root** (e.g. `Music/Bach/Goldberg`, `/`-separated), which doubles as the
/// playback locator and the browse-by-folder grouping. `parent` is the enclosing
/// folder's id, or [`NO_FOLDER`] for a top-level folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FolderRec {
    pub id: FolderId,
    pub path: StrRef,
    pub parent: FolderId,
}

impl Default for FolderRec {
    fn default() -> Self {
        Self {
            id: 0,
            path: StrRef::default(),
            parent: NO_FOLDER,
        }
    }
}

/// A packed calendar date for album metadata: `(year << 9) | (month << 5) | day`.
/// `day` occupies bits 0..4, `month` bits 5..8, `year` bits 9..31. A bare year
/// keeps `month`/`day` = 0 ("precision unknown"); `0` overall is a fully-unknown
/// date. Pure integer bit-ops only — `no_std`, no-alloc, no float (this type is
/// read by the DAP's on-device index reader).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PackedDate(pub u32);

impl PackedDate {
    /// A fully-unknown date.
    pub const UNKNOWN: PackedDate = PackedDate(0);

    /// Pack components. `month` outside `1..=12` or `day` outside `1..=31`
    /// collapses that component to `0` (unknown); the year is stored as given.
    pub const fn new(year: u16, month: u8, day: u8) -> PackedDate {
        let y = year as u32;
        let m = if month >= 1 && month <= 12 {
            month as u32
        } else {
            0
        };
        let d = if day >= 1 && day <= 31 { day as u32 } else { 0 };
        PackedDate((y << 9) | (m << 5) | d)
    }

    /// A year-only date (month/day unknown).
    pub const fn from_year(year: u16) -> PackedDate {
        PackedDate::new(year, 0, 0)
    }

    pub const fn year(self) -> u16 {
        (self.0 >> 9) as u16
    }
    pub const fn month(self) -> u8 {
        ((self.0 >> 5) & 0xF) as u8
    }
    pub const fn day(self) -> u8 {
        (self.0 & 0x1F) as u8
    }

    /// Whether any component is known (i.e. not the fully-unknown `0`).
    pub const fn is_known(self) -> bool {
        self.0 != 0
    }

    /// Ascending chronological sort key with unknown (`0`) forced LAST.
    pub const fn sort_key(self) -> u32 {
        if self.0 == 0 { u32::MAX } else { self.0 }
    }
}

/// Tier-1 album record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct AlbumRec {
    pub id: AlbumId,
    pub title: StrRef,
    pub album_artist: ArtistId,
    pub year: u16,
    pub folder: FolderId,
    pub art_id: u32,
}

/// Browse axes the desktop pre-sorts into order tables. The DAP (and desktop
/// browse) walk the frozen order — no on-device sort, no alloc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowseAxis {
    /// Flat, by track title.
    Title,
    /// Artist → album → disc/track.
    ArtistAlbumTrack,
    /// By album (then disc/track within).
    Album,
    /// By folder path.
    Folder,
    /// By genre (then the artist/album/disc/track chain within).
    Genre,
}

/// The read port both platforms implement. Seek-based, buffer-based, fallible.
///
/// `resolve_str` / `art` write UTF-8 / image bytes into the caller's buffer and
/// return the number of bytes written (or [`CatalogError::Truncated`] if the
/// buffer is too small) — never an owned `String`/`Vec`.
pub trait Catalog {
    fn track_count(&self) -> u32;
    fn album_count(&self) -> u32;
    fn artist_count(&self) -> u32;
    fn genre_count(&self) -> u32;

    fn track(&self, id: TrackId) -> Result<TrackRec, CatalogError>;
    fn album(&self, id: AlbumId) -> Result<AlbumRec, CatalogError>;

    /// Resolve an artist's display name into `buf`; returns bytes written.
    /// Artists are name entities referenced by [`TrackRec::artist`] /
    /// [`AlbumRec::album_artist`].
    fn artist_name(&self, id: ArtistId, buf: &mut [u8]) -> Result<usize, CatalogError>;

    /// Resolve an artist's **sort-name** into `buf` ("Clapton, Eric", so "Eric
    /// Clapton" files under C); returns bytes written. This is the name the
    /// `ArtistAlbumTrack` browse order keys on. Defaults to
    /// [`artist_name`](Self::artist_name): a source without stored sort-names (an
    /// untagged library, or an older index) orders by the display name, exactly
    /// as before. Same buffer contract as `artist_name` ([`CatalogError::Truncated`]
    /// when too small).
    fn artist_sort_name(&self, id: ArtistId, buf: &mut [u8]) -> Result<usize, CatalogError> {
        self.artist_name(id, buf)
    }

    /// Resolve a genre's display name into `buf`; returns bytes written. Genres
    /// are name entities referenced by [`TrackRec::genre`] and keyed on by the
    /// [`BrowseAxis::Genre`] browse order. Same buffer contract as
    /// [`artist_name`](Self::artist_name) ([`CatalogError::Truncated`] when too
    /// small).
    fn genre_name(&self, id: GenreId, buf: &mut [u8]) -> Result<usize, CatalogError>;

    /// Resolve a heap string into `buf`; returns bytes written.
    fn resolve_str(&self, s: StrRef, buf: &mut [u8]) -> Result<usize, CatalogError>;

    /// Length of a pre-sorted browse axis.
    fn order_len(&self, axis: BrowseAxis) -> u32;
    /// The `i`-th track id along a pre-sorted browse axis.
    fn order(&self, axis: BrowseAxis, i: u32) -> Result<TrackId, CatalogError>;

    /// Tracks of an album, in disc/track order: the `i`-th track id.
    fn album_track(&self, album: AlbumId, i: u32) -> Result<TrackId, CatalogError>;
    fn album_track_count(&self, album: AlbumId) -> u32;

    // --- folders: browse-by-folder grouping + the playback locator ---
    fn folder_count(&self) -> u32;
    /// The folder record for `id` (its relative path ref + parent).
    fn folder(&self, id: FolderId) -> Result<FolderRec, CatalogError>;
    /// Resolve a folder's root-relative path into `buf`; returns bytes written.
    /// Convenience over `resolve_str(folder(id)?.path, buf)`.
    fn folder_path(&self, id: FolderId, buf: &mut [u8]) -> Result<usize, CatalogError>;

    // --- playlists live in the index (decision §8) ---
    fn playlist_count(&self) -> u32;
    fn playlist_name(&self, p: PlaylistId, buf: &mut [u8]) -> Result<usize, CatalogError>;
    fn playlist_len(&self, p: PlaylistId) -> u32;
    fn playlist_track(&self, p: PlaylistId, i: u32) -> Result<TrackId, CatalogError>;
}

/// Cover-art bytes for an `art_id`, written into a caller buffer. On the DAP the
/// bytes are a baked 1-bit grayscale thumbnail (decision §8); on the desktop an
/// adapter may hand back the original file's bytes.
pub trait ArtStore {
    fn art(&self, art_id: u32, buf: &mut [u8]) -> Result<usize, CatalogError>;
}

/// Errors any `Catalog`/`ArtStore` read can surface. Intentionally small and
/// `Copy` so it costs nothing on the embedded side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogError {
    /// Id / index out of range.
    OutOfRange,
    /// The caller buffer was too small for the value.
    Truncated,
    /// The backing store returned malformed/corrupt data.
    Corrupt,
    /// Underlying I/O failure (SD read, etc.).
    Io,
}

#[cfg(test)]
mod packed_date_tests {
    use super::PackedDate;

    #[test]
    fn packs_and_unpacks_components() {
        let d = PackedDate::new(1973, 8, 7);
        assert_eq!(d.year(), 1973);
        assert_eq!(d.month(), 8);
        assert_eq!(d.day(), 7);
        assert!(d.is_known());
    }

    #[test]
    fn year_only_has_zero_month_and_day() {
        let d = PackedDate::from_year(2015);
        assert_eq!(d.year(), 2015);
        assert_eq!(d.month(), 0);
        assert_eq!(d.day(), 0);
        assert!(d.is_known());
    }

    #[test]
    fn out_of_range_month_or_day_is_dropped_to_unknown_component() {
        let d = PackedDate::new(2001, 13, 40);
        assert_eq!(d.year(), 2001);
        assert_eq!(d.month(), 0);
        assert_eq!(d.day(), 0);
    }

    #[test]
    fn zero_is_fully_unknown_and_sorts_last() {
        assert_eq!(PackedDate::UNKNOWN, PackedDate::default());
        assert!(!PackedDate::UNKNOWN.is_known());
        assert_eq!(PackedDate::UNKNOWN.sort_key(), u32::MAX);
    }

    #[test]
    fn sort_key_is_chronological() {
        // Earlier date → smaller sort key; same year, later month → larger.
        assert!(PackedDate::from_year(1973).sort_key() < PackedDate::from_year(2015).sort_key());
        assert!(PackedDate::new(1973, 1, 1).sort_key() < PackedDate::new(1973, 12, 1).sort_key());
        // Any known date sorts before unknown.
        assert!(PackedDate::from_year(2200).sort_key() < PackedDate::UNKNOWN.sort_key());
    }
}

#[cfg(all(test, feature = "serde"))]
mod packed_date_serde_tests {
    use super::PackedDate;

    /// `PackedDate` is a bare `u32` newtype — it must serialize transparently as
    /// the plain integer (not a wrapped object) and round-trip exactly, since
    /// `soul-audio-import`'s `RawTrack`/`ScanCache` embed it in the persisted
    /// warm-scan cache.
    #[test]
    fn serializes_as_a_bare_u32_and_round_trips() {
        let d = PackedDate::new(1973, 8, 7);
        let json = serde_json::to_string(&d).expect("serialize");
        assert_eq!(json, d.0.to_string());

        let back: PackedDate = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, d);
    }
}
