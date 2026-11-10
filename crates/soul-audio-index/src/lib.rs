//! The `.soulidx` on-SD catalog index.
//!
//! Two faces, one format:
//! - **reader** (default, no_std, no-alloc): seeks fixed-width records, walks
//!   desktop-baked pre-sorted order tables, resolves strings into caller
//!   buffers. Used by the DAP and by desktop browse.
//! - **baker** (`build` feature, std): writes the sections. Desktop-only.
//!
//! Reading needs `&mut` on the byte source, but [`Catalog`] is `&self`, so the
//! reader wraps the source in a [`core::cell::RefCell`] (no_std, no-alloc).
//!
//! # Byte layout (`index.sidx`, all little-endian)
//!
//! Sections are laid out back-to-back; the header records each one's absolute
//! byte offset so the reader seeks directly. String refs ([`StrRef`]) are
//! `(offset, len)` with the offset **relative to the string heap**.
//!
//! ```text
//! HEADER (64 B, fixed):
//!    0  magic "SIDX"            [u8;4]
//!    4  format_version          u16
//!    6  reserved                u16
//!    8  track_count             u32
//!   12  album_count             u32
//!   16  artist_count            u32
//!   20  folder_count            u32
//!   24  playlist_count          u32
//!   28  records_off             u32   (abs)
//!   32  albums_off              u32   (abs)
//!   36  artists_off             u32   (abs)
//!   40  folders_off             u32   (abs)
//!   44  order_tables_off        u32   (abs)
//!   48  album_tracks_off        u32   (abs)
//!   52  playlists_off           u32   (abs)
//!   56  string_heap_off         u32   (abs)
//!   60  string_heap_len         u32
//!
//! TRACK RECORD (55 B, fixed; `record[i]` at records_off + i*55):
//!    0 id u32 · 4 title.off u32 · 8 title.len u16 · 10 artist u32 ·
//!   14 album u32 · 18 album_artist u32 · 22 folder u32 · 26 track_no u16 ·
//!   28 disc_no u16 · 30 duration_ms u32 · 34 sample_rate u32 · 38 channels u8 ·
//!   39 art_id u32 · 43 flags u16 · 45 extended_off u32 ·
//!   49 file_name.off u32 · 53 file_name.len u16
//!
//! ALBUM RECORD (24 B, fixed; indexed by dense album id):
//!    0 id u32 · 4 title.off u32 · 8 title.len u16 · 10 album_artist u32 ·
//!   14 year u16 · 16 folder u32 · 20 art_id u32
//!
//! ARTIST NAME TABLE: artist_count × StrRef (6 B: off u32, len u16), indexed by
//!   artist id; each resolves to the artist's display name in the string heap.
//!
//! FOLDER RECORD (10 B, fixed; indexed by dense folder id):
//!    0 path.off u32 · 4 path.len u16 · 6 parent u32 (FolderId::MAX = no parent).
//!   `path` is the folder's path relative to the card root; a track's physical
//!   file is `root / folder.path / track.file_name`.
//!
//! ORDER TABLES: AXIS_COUNT (4) arrays of `track_count` × u32 record-ids,
//!   pre-sorted by the baker, in axis order [Title, ArtistAlbumTrack, Album,
//!   Folder]. axis `ax`, slot `i` → order_tables_off + (ax*track_count + i)*4.
//!
//! ALBUM→TRACKS: a directory of album_count × (count u32, run_abs_off u32),
//!   then the concatenated u32 track-id runs.
//!
//! PLAYLISTS: a directory of playlist_count × (name.off u32, name.len u16,
//!   count u32, run_abs_off u32), then the concatenated u32 track-id runs.
//!
//! STRING HEAP: a UTF-8 blob; `StrRef` indexes into it by (offset, len).
//! ```
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

#[cfg(feature = "alloc")]
extern crate alloc;

/// Collation sort keys for browse ordering — byte-comparable keys the desktop
/// baker sorts by and the desktop `MultiCatalog` merge compares; the device only
/// ever reads the frozen order (no collation on `no_std`).
#[cfg(feature = "alloc")]
pub mod collation;

use core::cell::RefCell;

use embedded_io::SeekFrom;
use soul_audio_catalog::{
    AlbumId, AlbumRec, ArtistId, BrowseAxis, Catalog, CatalogError, FolderId, FolderRec,
    PlaylistId, StrRef, TrackId, TrackRec,
};
use soul_audio_core::SeekSource;

/// Magic at the head of `index.sidx` — `"SIDX"`.
pub const MAGIC: [u8; 4] = *b"SIDX";
/// Format version in the header.
pub const FORMAT_VERSION: u16 = 1;

const HEADER_SIZE: usize = 64;
const RECORD_SIZE: usize = 55;
const ALBUM_RECORD_SIZE: usize = 24;
const ARTIST_REF_SIZE: usize = 6; // StrRef (off u32, len u16) per artist
const FOLDER_RECORD_SIZE: usize = 10; // path StrRef (6) + parent u32 (4)
const ALBUM_DIR_ENTRY: usize = 8;
const PLAYLIST_DIR_ENTRY: usize = 14;
/// Number of pre-sorted browse axes (keep in sync with [`axis_index`]). Used by
/// the baker to size the order-table section.
#[cfg(feature = "build")]
const AXIS_COUNT: usize = 4;

fn axis_index(a: BrowseAxis) -> u32 {
    match a {
        BrowseAxis::Title => 0,
        BrowseAxis::ArtistAlbumTrack => 1,
        BrowseAxis::Album => 2,
        BrowseAxis::Folder => 3,
    }
}

#[inline]
fn le_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
#[inline]
fn le_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Cached header — parsed once in [`IndexReader::new`]; all fields are `Copy`.
#[derive(Clone, Copy, Default)]
struct Header {
    track_count: u32,
    album_count: u32,
    artist_count: u32,
    folder_count: u32,
    playlist_count: u32,
    records_off: u32,
    albums_off: u32,
    artists_off: u32,
    folders_off: u32,
    order_tables_off: u32,
    album_tracks_off: u32,
    playlists_off: u32,
    string_heap_off: u32,
    string_heap_len: u32,
}

impl Header {
    fn read<S: SeekSource>(src: &mut S) -> Result<Self, ()> {
        let mut h = [0u8; HEADER_SIZE];
        src.seek(SeekFrom::Start(0)).map_err(|_| ())?;
        src.read_exact(&mut h).map_err(|_| ())?;
        if h[0..4] != MAGIC || le_u16(&h, 4) != FORMAT_VERSION {
            return Err(());
        }
        Ok(Header {
            track_count: le_u32(&h, 8),
            album_count: le_u32(&h, 12),
            artist_count: le_u32(&h, 16),
            folder_count: le_u32(&h, 20),
            playlist_count: le_u32(&h, 24),
            records_off: le_u32(&h, 28),
            albums_off: le_u32(&h, 32),
            artists_off: le_u32(&h, 36),
            folders_off: le_u32(&h, 40),
            order_tables_off: le_u32(&h, 44),
            album_tracks_off: le_u32(&h, 48),
            playlists_off: le_u32(&h, 52),
            string_heap_off: le_u32(&h, 56),
            string_heap_len: le_u32(&h, 60),
        })
    }
}

/// Reads a baked `.soulidx` index over any seekable byte source (a `SliceSource`
/// on the host, an SD-card source on the DAP). No heap allocation.
pub struct IndexReader<S> {
    src: RefCell<S>,
    header: Header,
}

impl<S: SeekSource> IndexReader<S> {
    /// Open a reader over `src`, parsing the header. A malformed/short source
    /// yields an empty catalog (all counts 0) rather than a panic.
    pub fn new(mut src: S) -> Self {
        let header = Header::read(&mut src).unwrap_or_default();
        Self {
            src: RefCell::new(src),
            header,
        }
    }

    /// Consume the reader, returning the source.
    pub fn into_inner(self) -> S {
        self.src.into_inner()
    }

    /// Seek to `off` and fill `buf` exactly. Maps any I/O error to `Io`.
    fn read_at(&self, off: u32, buf: &mut [u8]) -> Result<(), CatalogError> {
        let mut src = self.src.borrow_mut();
        src.seek(SeekFrom::Start(u64::from(off)))
            .map_err(|_| CatalogError::Io)?;
        src.read_exact(buf).map_err(|_| CatalogError::Io)?;
        Ok(())
    }

    fn read_u32_at(&self, off: u32) -> Result<u32, CatalogError> {
        let mut b = [0u8; 4];
        self.read_at(off, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    /// `(name, track_count, run_abs_off)` for playlist `p`.
    fn playlist_entry(&self, p: PlaylistId) -> Result<(StrRef, u32, u32), CatalogError> {
        if p >= self.header.playlist_count {
            return Err(CatalogError::OutOfRange);
        }
        let off = self.header.playlists_off + p * PLAYLIST_DIR_ENTRY as u32;
        let mut b = [0u8; PLAYLIST_DIR_ENTRY];
        self.read_at(off, &mut b)?;
        let name = StrRef {
            off: le_u32(&b, 0),
            len: le_u16(&b, 4),
        };
        Ok((name, le_u32(&b, 6), le_u32(&b, 10)))
    }
}

impl<S: SeekSource> Catalog for IndexReader<S> {
    fn track_count(&self) -> u32 {
        self.header.track_count
    }
    fn album_count(&self) -> u32 {
        self.header.album_count
    }
    fn artist_count(&self) -> u32 {
        self.header.artist_count
    }

    fn artist_name(&self, id: ArtistId, buf: &mut [u8]) -> Result<usize, CatalogError> {
        if id >= self.header.artist_count {
            return Err(CatalogError::OutOfRange);
        }
        let mut b = [0u8; ARTIST_REF_SIZE];
        self.read_at(
            self.header.artists_off + id * ARTIST_REF_SIZE as u32,
            &mut b,
        )?;
        let name = StrRef {
            off: le_u32(&b, 0),
            len: le_u16(&b, 4),
        };
        self.resolve_str(name, buf)
    }

    fn track(&self, id: TrackId) -> Result<TrackRec, CatalogError> {
        if id >= self.header.track_count {
            return Err(CatalogError::OutOfRange);
        }
        let off = self.header.records_off + id * RECORD_SIZE as u32;
        let mut r = [0u8; RECORD_SIZE];
        self.read_at(off, &mut r)?;
        Ok(TrackRec {
            id: le_u32(&r, 0),
            title: StrRef {
                off: le_u32(&r, 4),
                len: le_u16(&r, 8),
            },
            artist: le_u32(&r, 10),
            album: le_u32(&r, 14),
            album_artist: le_u32(&r, 18),
            folder: le_u32(&r, 22),
            track_no: le_u16(&r, 26),
            disc_no: le_u16(&r, 28),
            duration_ms: le_u32(&r, 30),
            sample_rate: le_u32(&r, 34),
            channels: r[38],
            art_id: le_u32(&r, 39),
            flags: le_u16(&r, 43),
            extended_off: le_u32(&r, 45),
            file_name: StrRef {
                off: le_u32(&r, 49),
                len: le_u16(&r, 53),
            },
        })
    }

    fn album(&self, id: AlbumId) -> Result<AlbumRec, CatalogError> {
        if id >= self.header.album_count {
            return Err(CatalogError::OutOfRange);
        }
        let off = self.header.albums_off + id * ALBUM_RECORD_SIZE as u32;
        let mut r = [0u8; ALBUM_RECORD_SIZE];
        self.read_at(off, &mut r)?;
        Ok(AlbumRec {
            id: le_u32(&r, 0),
            title: StrRef {
                off: le_u32(&r, 4),
                len: le_u16(&r, 8),
            },
            album_artist: le_u32(&r, 10),
            year: le_u16(&r, 14),
            folder: le_u32(&r, 16),
            art_id: le_u32(&r, 20),
        })
    }

    fn resolve_str(&self, s: StrRef, buf: &mut [u8]) -> Result<usize, CatalogError> {
        let len = s.len as usize;
        if len > buf.len() {
            return Err(CatalogError::Truncated);
        }
        if u64::from(s.off) + len as u64 > u64::from(self.header.string_heap_len) {
            return Err(CatalogError::Corrupt);
        }
        self.read_at(self.header.string_heap_off + s.off, &mut buf[..len])?;
        Ok(len)
    }

    fn order_len(&self, _axis: BrowseAxis) -> u32 {
        // Every axis is a permutation of all tracks.
        self.header.track_count
    }

    fn order(&self, axis: BrowseAxis, i: u32) -> Result<TrackId, CatalogError> {
        if i >= self.header.track_count {
            return Err(CatalogError::OutOfRange);
        }
        let slot = axis_index(axis) * self.header.track_count + i;
        self.read_u32_at(self.header.order_tables_off + slot * 4)
    }

    fn album_track(&self, album: AlbumId, i: u32) -> Result<TrackId, CatalogError> {
        if album >= self.header.album_count {
            return Err(CatalogError::OutOfRange);
        }
        let mut dir = [0u8; ALBUM_DIR_ENTRY];
        self.read_at(
            self.header.album_tracks_off + album * ALBUM_DIR_ENTRY as u32,
            &mut dir,
        )?;
        let count = le_u32(&dir, 0);
        let run_off = le_u32(&dir, 4);
        if i >= count {
            return Err(CatalogError::OutOfRange);
        }
        self.read_u32_at(run_off + i * 4)
    }

    fn album_track_count(&self, album: AlbumId) -> u32 {
        if album >= self.header.album_count {
            return 0;
        }
        let mut dir = [0u8; ALBUM_DIR_ENTRY];
        if self
            .read_at(
                self.header.album_tracks_off + album * ALBUM_DIR_ENTRY as u32,
                &mut dir,
            )
            .is_err()
        {
            return 0;
        }
        le_u32(&dir, 0)
    }

    fn folder_count(&self) -> u32 {
        self.header.folder_count
    }

    fn folder(&self, id: FolderId) -> Result<FolderRec, CatalogError> {
        if id >= self.header.folder_count {
            return Err(CatalogError::OutOfRange);
        }
        let mut b = [0u8; FOLDER_RECORD_SIZE];
        self.read_at(
            self.header.folders_off + id * FOLDER_RECORD_SIZE as u32,
            &mut b,
        )?;
        Ok(FolderRec {
            id,
            path: StrRef {
                off: le_u32(&b, 0),
                len: le_u16(&b, 4),
            },
            parent: le_u32(&b, 6),
        })
    }

    fn folder_path(&self, id: FolderId, buf: &mut [u8]) -> Result<usize, CatalogError> {
        let rec = self.folder(id)?;
        self.resolve_str(rec.path, buf)
    }

    fn playlist_count(&self) -> u32 {
        self.header.playlist_count
    }

    fn playlist_name(&self, p: PlaylistId, buf: &mut [u8]) -> Result<usize, CatalogError> {
        let (name, _, _) = self.playlist_entry(p)?;
        self.resolve_str(name, buf)
    }

    fn playlist_len(&self, p: PlaylistId) -> u32 {
        self.playlist_entry(p).map(|(_, n, _)| n).unwrap_or(0)
    }

    fn playlist_track(&self, p: PlaylistId, i: u32) -> Result<TrackId, CatalogError> {
        let (_, count, run_off) = self.playlist_entry(p)?;
        if i >= count {
            return Err(CatalogError::OutOfRange);
        }
        self.read_u32_at(run_off + i * 4)
    }
}

/// The baker (desktop-only). Takes an owned input catalog and emits the
/// `index.sidx` bytes laid out per the module-level byte layout. Album records
/// are derived from the tracks' dense `album` ids.
#[cfg(feature = "build")]
pub mod build {
    use super::{
        ALBUM_DIR_ENTRY, ALBUM_RECORD_SIZE, ARTIST_REF_SIZE, AXIS_COUNT, FOLDER_RECORD_SIZE,
        FORMAT_VERSION, HEADER_SIZE, MAGIC, PLAYLIST_DIR_ENTRY, RECORD_SIZE,
    };
    use alloc::{string::String, vec, vec::Vec};
    use core::cmp::Ordering;

    /// One track to bake into the index. `artist`/`album`/`folder` are dense ids
    /// into [`CatalogInput::artists`] / `albums` / `folders`. `file_name` is the
    /// leaf (e.g. `01 - Aria.flac`); with `folders[folder].path` it is the file's
    /// physical location relative to the card root.
    pub struct TrackInput {
        pub title: String,
        pub artist: u32,
        pub album: u32,
        pub album_artist: u32,
        pub folder: u32,
        pub track_no: u16,
        pub disc_no: u16,
        pub duration_ms: u32,
        pub sample_rate: u32,
        pub channels: u8,
        pub file_name: String,
    }

    /// One folder to bake into the folder table. `path` is relative to the card
    /// root (`/`-separated, e.g. `Music/Bach/Goldberg`); `parent` is the dense id
    /// of the enclosing folder, or `FolderId::MAX` ([`soul_audio_catalog::NO_FOLDER`])
    /// for a top-level folder.
    pub struct FolderInput {
        pub path: String,
        pub parent: u32,
    }

    /// Per-album metadata (title etc.) — the desktop computes these with
    /// folder-isolation; the baker writes them verbatim as album records.
    pub struct AlbumMeta {
        pub title: String,
        pub album_artist: u32,
        pub year: u16,
        pub folder: u32,
    }

    /// One playlist (lives inside the index — decision §8).
    pub struct PlaylistInput {
        pub name: String,
        pub tracks: Vec<u32>,
    }

    /// The desktop-built catalog handed to the baker. `artists[i]` is the display
    /// name for artist id `i`; `albums[i]` is album id `i`.
    #[derive(Default)]
    pub struct CatalogInput {
        pub tracks: Vec<TrackInput>,
        pub artists: Vec<String>,
        pub albums: Vec<AlbumMeta>,
        pub folders: Vec<FolderInput>,
        pub playlists: Vec<PlaylistInput>,
        /// Per-album cover source bytes, aligned to `albums` (index i =
        /// albums[i]); `None` = no cover. Empty vec = no covers at all (the
        /// baker treats a missing index as `None`).
        pub covers: Vec<Option<Vec<u8>>>,
    }

    fn intern(heap: &mut Vec<u8>, s: &str) -> (u32, u16) {
        let off = heap.len() as u32;
        heap.extend_from_slice(s.as_bytes());
        (off, s.len() as u16)
    }

    fn sorted_ids(n: u32, cmp: impl Fn(usize, usize) -> Ordering) -> Vec<u32> {
        let mut ids: Vec<u32> = (0..n).collect();
        ids.sort_by(|&a, &b| cmp(a as usize, b as usize));
        ids
    }

    /// Default cover art size baked into `art.blob` (see [`bake_art`]).
    pub const ART_W: u16 = 200;
    pub const ART_H: u16 = 200;

    /// Encodes each album's cover (via [`soul_audio_artbake::build::encode_cover`])
    /// into an `art.blob`, deduping identical bitmaps with
    /// [`soul_audio_artbake::build::ArtBlobWriter`]. Returns the blob bytes and
    /// the per-album `art_id` (aligned to `input.albums`; `0` = no art). An
    /// album with no cover (`None`/missing index) or an unsupported/corrupt
    /// cover (`encode_cover` error) gets `art_id = 0` — best-effort, never
    /// panics.
    #[must_use]
    pub fn bake_art(input: &CatalogInput, width: u16, height: u16) -> (Vec<u8>, Vec<u32>) {
        let mut writer = soul_audio_artbake::build::ArtBlobWriter::new(width, height);
        let mut album_art_id = vec![0u32; input.albums.len()];
        for (i, id) in album_art_id.iter_mut().enumerate() {
            if let Some(Some(bytes)) = input.covers.get(i) {
                if let Ok(bitmap) = soul_audio_artbake::build::encode_cover(bytes, width, height) {
                    *id = writer.add(bitmap);
                }
            }
        }
        (writer.finish(), album_art_id)
    }

    /// Serialize `input` into `index.sidx` bytes, with every record's `art_id`
    /// set to `0` (no cover art baked). Delegates to [`write_index_with_art`].
    #[must_use]
    pub fn write_index(input: &CatalogInput) -> Vec<u8> {
        write_index_with_art(input, &vec![0u32; input.albums.len()])
    }

    /// Serialize `input` into `index.sidx` bytes, threading `album_art_id`
    /// (aligned to `input.albums`, as returned by [`bake_art`]) into each
    /// track's and album's `art_id` field — a track's `art_id` is its album's.
    #[must_use]
    pub fn write_index_with_art(input: &CatalogInput, album_art_id: &[u32]) -> Vec<u8> {
        let tracks = &input.tracks;
        let track_count = tracks.len() as u32;
        let album_count = input.albums.len() as u32;
        let artist_count = input.artists.len() as u32;
        let folder_count = input.folders.len() as u32;
        let playlist_count = input.playlists.len() as u32;

        // String heap + interned refs (built first; placed at the file's end).
        let mut heap: Vec<u8> = Vec::new();
        let track_titles: Vec<(u32, u16)> =
            tracks.iter().map(|t| intern(&mut heap, &t.title)).collect();
        let track_file_names: Vec<(u32, u16)> = tracks
            .iter()
            .map(|t| intern(&mut heap, &t.file_name))
            .collect();
        let album_titles: Vec<(u32, u16)> = input
            .albums
            .iter()
            .map(|a| intern(&mut heap, &a.title))
            .collect();
        let artist_names: Vec<(u32, u16)> =
            input.artists.iter().map(|a| intern(&mut heap, a)).collect();
        let folder_paths: Vec<(u32, u16)> = input
            .folders
            .iter()
            .map(|f| intern(&mut heap, &f.path))
            .collect();
        let playlist_names: Vec<(u32, u16)> = input
            .playlists
            .iter()
            .map(|p| intern(&mut heap, &p.name))
            .collect();

        // Per-album track runs (sorted disc/track). A track with an out-of-range
        // album id is dropped from album runs (it still exists as a track).
        let n_albums = album_count as usize;
        let mut album_tracks: Vec<Vec<u32>> = vec![Vec::new(); n_albums];
        for (i, t) in tracks.iter().enumerate() {
            if (t.album as usize) < n_albums {
                album_tracks[t.album as usize].push(i as u32);
            }
        }
        for run in &mut album_tracks {
            run.sort_by(|&x, &y| {
                let (tx, ty) = (&tracks[x as usize], &tracks[y as usize]);
                (tx.disc_no, tx.track_no).cmp(&(ty.disc_no, ty.track_no))
            });
        }

        // Pre-sorted order tables, keyed by COLLATION order (the browse order S+
        // players use: leading-article-folded, case-folded, natural numbers).
        // Keys are computed once per entity here; the `no_std` device reads this
        // frozen order and never runs collation, and `MultiCatalog` recomputes
        // the same key to merge sources. Ids break ties for a deterministic order
        // (and keep folder-isolated same-title albums distinct).
        let title_key: Vec<Vec<u8>> = tracks
            .iter()
            .map(|t| crate::collation::key(&t.title))
            .collect();
        let album_key: Vec<Vec<u8>> = input
            .albums
            .iter()
            .map(|a| crate::collation::key(&a.title))
            .collect();
        let artist_key: Vec<Vec<u8>> = input
            .artists
            .iter()
            .map(|a| crate::collation::key(a))
            .collect();
        let folder_key: Vec<Vec<u8>> = input
            .folders
            .iter()
            .map(|f| crate::collation::key(&f.path))
            .collect();
        // An out-of-range id sorts as an empty key (never panics).
        fn key_at(keys: &[Vec<u8>], i: u32) -> &[u8] {
            keys.get(i as usize).map_or(&[], |v| v.as_slice())
        }
        // Album release year for chronological ordering within an artist (the
        // collector default S+ players use). Unknown year (0 = untagged) sorts
        // LAST (as `u16::MAX`) so the dated discography stays chronological and
        // undated albums trail it. An out-of-range id also sorts last.
        fn year_at(albums: &[AlbumMeta], i: u32) -> u16 {
            match albums.get(i as usize).map(|a| a.year) {
                Some(0) | None => u16::MAX,
                Some(y) => y,
            }
        }

        let title_order = sorted_ids(track_count, |a, b| {
            title_key[a].cmp(&title_key[b]).then(a.cmp(&b))
        });
        let artist_album_track = sorted_ids(track_count, |a, b| {
            let (x, y) = (&tracks[a], &tracks[b]);
            key_at(&artist_key, x.album_artist)
                .cmp(key_at(&artist_key, y.album_artist))
                .then_with(|| x.album_artist.cmp(&y.album_artist))
                .then_with(|| year_at(&input.albums, x.album).cmp(&year_at(&input.albums, y.album)))
                .then_with(|| key_at(&album_key, x.album).cmp(key_at(&album_key, y.album)))
                .then_with(|| x.album.cmp(&y.album))
                .then_with(|| (x.disc_no, x.track_no).cmp(&(y.disc_no, y.track_no)))
        });
        let album_order = sorted_ids(track_count, |a, b| {
            let (x, y) = (&tracks[a], &tracks[b]);
            key_at(&album_key, x.album)
                .cmp(key_at(&album_key, y.album))
                .then_with(|| x.album.cmp(&y.album))
                .then_with(|| (x.disc_no, x.track_no).cmp(&(y.disc_no, y.track_no)))
        });
        let folder_order = sorted_ids(track_count, |a, b| {
            let (x, y) = (&tracks[a], &tracks[b]);
            key_at(&folder_key, x.folder)
                .cmp(key_at(&folder_key, y.folder))
                .then_with(|| x.folder.cmp(&y.folder))
                .then_with(|| (x.disc_no, x.track_no).cmp(&(y.disc_no, y.track_no)))
        });

        // Section sizes + absolute offsets.
        let records_size = tracks.len() * RECORD_SIZE;
        let albums_size = n_albums * ALBUM_RECORD_SIZE;
        let artists_size = input.artists.len() * ARTIST_REF_SIZE;
        let folders_size = input.folders.len() * FOLDER_RECORD_SIZE;
        let order_size = AXIS_COUNT * tracks.len() * 4;
        let album_dir_size = n_albums * ALBUM_DIR_ENTRY;
        let album_runs_total: usize = album_tracks.iter().map(|r| r.len()).sum();
        let album_runs_size = album_runs_total * 4;
        let playlist_dir_size = input.playlists.len() * PLAYLIST_DIR_ENTRY;
        let playlist_runs_size: usize = input.playlists.iter().map(|p| p.tracks.len() * 4).sum();

        let records_off = HEADER_SIZE;
        let albums_off = records_off + records_size;
        let artists_off = albums_off + albums_size;
        let folders_off = artists_off + artists_size;
        let order_tables_off = folders_off + folders_size;
        let album_tracks_off = order_tables_off + order_size;
        let album_runs_off = album_tracks_off + album_dir_size;
        let playlists_off = album_runs_off + album_runs_size;
        let playlist_runs_off = playlists_off + playlist_dir_size;
        let string_heap_off = playlist_runs_off + playlist_runs_size;

        let mut out = Vec::with_capacity(string_heap_off + heap.len());

        // --- Header ---
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // reserved
        out.extend_from_slice(&track_count.to_le_bytes());
        out.extend_from_slice(&album_count.to_le_bytes());
        out.extend_from_slice(&artist_count.to_le_bytes());
        out.extend_from_slice(&folder_count.to_le_bytes());
        out.extend_from_slice(&playlist_count.to_le_bytes());
        out.extend_from_slice(&(records_off as u32).to_le_bytes());
        out.extend_from_slice(&(albums_off as u32).to_le_bytes());
        out.extend_from_slice(&(artists_off as u32).to_le_bytes());
        out.extend_from_slice(&(folders_off as u32).to_le_bytes());
        out.extend_from_slice(&(order_tables_off as u32).to_le_bytes());
        out.extend_from_slice(&(album_tracks_off as u32).to_le_bytes());
        out.extend_from_slice(&(playlists_off as u32).to_le_bytes());
        out.extend_from_slice(&(string_heap_off as u32).to_le_bytes());
        out.extend_from_slice(&(heap.len() as u32).to_le_bytes());
        debug_assert_eq!(out.len(), records_off);

        // --- Track records ---
        for (i, t) in tracks.iter().enumerate() {
            let (toff, tlen) = track_titles[i];
            let (foff, flen) = track_file_names[i];
            out.extend_from_slice(&(i as u32).to_le_bytes());
            out.extend_from_slice(&toff.to_le_bytes());
            out.extend_from_slice(&tlen.to_le_bytes());
            out.extend_from_slice(&t.artist.to_le_bytes());
            out.extend_from_slice(&t.album.to_le_bytes());
            out.extend_from_slice(&t.album_artist.to_le_bytes());
            out.extend_from_slice(&t.folder.to_le_bytes());
            out.extend_from_slice(&t.track_no.to_le_bytes());
            out.extend_from_slice(&t.disc_no.to_le_bytes());
            out.extend_from_slice(&t.duration_ms.to_le_bytes());
            out.extend_from_slice(&t.sample_rate.to_le_bytes());
            out.push(t.channels);
            let art_id = album_art_id.get(t.album as usize).copied().unwrap_or(0);
            out.extend_from_slice(&art_id.to_le_bytes()); // art_id (the track's album's)
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&0u32.to_le_bytes()); // extended_off
            out.extend_from_slice(&foff.to_le_bytes()); // file_name.off
            out.extend_from_slice(&flen.to_le_bytes()); // file_name.len
        }
        debug_assert_eq!(out.len(), albums_off);

        // --- Album records (real titles from the albums table) ---
        for (a, meta) in input.albums.iter().enumerate() {
            let (toff, tlen) = album_titles[a];
            out.extend_from_slice(&(a as u32).to_le_bytes());
            out.extend_from_slice(&toff.to_le_bytes());
            out.extend_from_slice(&tlen.to_le_bytes());
            out.extend_from_slice(&meta.album_artist.to_le_bytes());
            out.extend_from_slice(&meta.year.to_le_bytes());
            out.extend_from_slice(&meta.folder.to_le_bytes());
            let art_id = album_art_id.get(a).copied().unwrap_or(0);
            out.extend_from_slice(&art_id.to_le_bytes()); // art_id
        }
        debug_assert_eq!(out.len(), artists_off);

        // --- Artist name refs (StrRef per artist id) ---
        for (noff, nlen) in &artist_names {
            out.extend_from_slice(&noff.to_le_bytes());
            out.extend_from_slice(&nlen.to_le_bytes());
        }
        debug_assert_eq!(out.len(), folders_off);

        // --- Folder records (path StrRef + parent, per folder id) ---
        for (f, folder) in input.folders.iter().enumerate() {
            let (poff, plen) = folder_paths[f];
            out.extend_from_slice(&poff.to_le_bytes());
            out.extend_from_slice(&plen.to_le_bytes());
            out.extend_from_slice(&folder.parent.to_le_bytes());
        }
        debug_assert_eq!(out.len(), order_tables_off);

        // --- Order tables ---
        for order in [
            &title_order,
            &artist_album_track,
            &album_order,
            &folder_order,
        ] {
            for &id in order {
                out.extend_from_slice(&id.to_le_bytes());
            }
        }
        debug_assert_eq!(out.len(), album_tracks_off);

        // --- Album → tracks (directory then runs) ---
        let mut cursor = album_runs_off as u32;
        for run in &album_tracks {
            let count = run.len() as u32;
            out.extend_from_slice(&count.to_le_bytes());
            out.extend_from_slice(&cursor.to_le_bytes());
            cursor += count * 4;
        }
        for run in &album_tracks {
            for &id in run {
                out.extend_from_slice(&id.to_le_bytes());
            }
        }
        debug_assert_eq!(out.len(), playlists_off);

        // --- Playlists (directory then runs) ---
        let mut cursor = playlist_runs_off as u32;
        for (p, pl) in input.playlists.iter().enumerate() {
            let (noff, nlen) = playlist_names[p];
            let count = pl.tracks.len() as u32;
            out.extend_from_slice(&noff.to_le_bytes());
            out.extend_from_slice(&nlen.to_le_bytes());
            out.extend_from_slice(&count.to_le_bytes());
            out.extend_from_slice(&cursor.to_le_bytes());
            cursor += count * 4;
        }
        for pl in &input.playlists {
            for &id in &pl.tracks {
                out.extend_from_slice(&id.to_le_bytes());
            }
        }
        debug_assert_eq!(out.len(), string_heap_off);

        // --- String heap ---
        out.extend_from_slice(&heap);
        out
    }
}
