//! `MultiCatalog` — compose `N` member [`Catalog`]s into one aggregate
//! `Catalog`.
//!
//! This is the desktop's "library = union of sources" (committed architecture
//! §2.6): the desktop aggregates `{ local dirs… } ∪ { remote servers… }`; the
//! DAP is the `N = 1` case and never links this crate. It is a **std adapter**
//! (it allocates the merged order on the desktop side) — the shared `no_std`
//! core is untouched.
//!
//! # Compound ids
//!
//! Every id the aggregate hands out packs its owning **source** into the high
//! bits of the `u32`: 4 bits source (≤ [`MAX_SOURCES`]), 28 bits payload (see
//! [`pack`]/[`unpack`]). This applies to **every** id (`TrackId`/`AlbumId`/
//! `ArtistId`/`FolderId`/`PlaylistId`) **and** to the `off` of every [`StrRef`]
//! in a returned record. The aggregate routes each read by unpacking the source
//! and delegating to `members[source]` with the local id/offset.
//!
//! Local ids and string-heap offsets must fit in 28 bits (`< 2^28`,
//! [`MAX_LOCAL_ID`]). That is ~268M — far above any real single-source library.
//!
//! # Browse order is a k-way merge
//!
//! Each member is already pre-sorted per axis (its baked `.soulidx` order
//! table). The aggregate order is the **k-way merge** of the `N` sorted streams,
//! comparing heads by the *same* collation key the baker sorted by
//! ([`soul_audio_index::collation::key`]) so the merged order is identical to
//! baking the union. Ties break deterministically by `(source, local_track_id)`.
//! Each axis is materialized lazily into a cached `Vec` of compound ids.
#![forbid(unsafe_code)]

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Mutex;

use soul_audio_catalog::{
    AlbumId, AlbumRec, ArtistId, BrowseAxis, Catalog, CatalogError, FolderId, FolderRec, NO_FOLDER,
    PlaylistId, StrRef, TrackId, TrackRec,
};
use soul_audio_index::collation::key;

/// Bits of a packed id reserved for the source index.
pub const SOURCE_BITS: u32 = 4;
/// Bits of a packed id reserved for the local id / string-heap offset.
pub const PAYLOAD_BITS: u32 = 32 - SOURCE_BITS; // 28
/// Mask selecting the local-payload bits of a packed id.
pub const PAYLOAD_MASK: u32 = (1 << PAYLOAD_BITS) - 1; // 0x0FFF_FFFF
/// Maximum number of member sources a `MultiCatalog` can address (`2^SOURCE_BITS`).
pub const MAX_SOURCES: usize = 1 << SOURCE_BITS; // 16
/// Largest local id / string offset that round-trips through [`pack`]
/// (`2^PAYLOAD_BITS - 1`). Real single-source libraries stay far below this.
pub const MAX_LOCAL_ID: u32 = PAYLOAD_MASK;

/// Number of pre-sorted browse axes (`BrowseAxis` variants). Keep in sync with
/// [`axis_index`].
const AXIS_COUNT: usize = 4;

/// Pack a `(source, local)` pair into a compound id: source in the high
/// [`SOURCE_BITS`], `local` in the low [`PAYLOAD_BITS`].
///
/// `local` beyond [`MAX_LOCAL_ID`] is silently masked (documented invariant:
/// callers keep local ids `< 2^28`).
#[must_use]
pub fn pack(source: u32, local: u32) -> u32 {
    (source << PAYLOAD_BITS) | (local & PAYLOAD_MASK)
}

/// Split a compound id back into `(source, local)` — the inverse of [`pack`] for
/// any `local <= MAX_LOCAL_ID`.
#[must_use]
pub fn unpack(id: u32) -> (u32, u32) {
    (id >> PAYLOAD_BITS, id & PAYLOAD_MASK)
}

fn axis_index(a: BrowseAxis) -> usize {
    match a {
        BrowseAxis::Title => 0,
        BrowseAxis::ArtistAlbumTrack => 1,
        BrowseAxis::Album => 2,
        BrowseAxis::Folder => 3,
    }
}

/// An aggregate [`Catalog`] over `N` member catalogs (the *selected* sources —
/// selection is the caller's concern). See the crate docs for the compound-id
/// and merge model.
pub struct MultiCatalog {
    // `+ Send` (not `Sync`): the desktop shares one aggregate across threads
    // (`Arc<Library>` over the actor + event-pump threads) by wrapping it in a
    // `Mutex`, which only needs `Send`. It can't be `Sync` — the member readers
    // use `RefCell` interior mutability (the no_std/no-alloc reader is
    // single-threaded on the DAP) — so the desktop serializes access behind its
    // own lock. The DAP never links this crate, so the bound costs it nothing.
    members: Vec<Box<dyn Catalog + Send>>,
    /// Lazily materialized merged order per axis (compound ids). A `Mutex` (not a
    /// `RefCell`) so the cache stays sound under the desktop's shared lock;
    /// materialized at most once per axis, then read under a brief lock.
    order_cache: Mutex<[Option<Vec<u32>>; AXIS_COUNT]>,
}

impl MultiCatalog {
    /// Compose `members` (the selected sources) into one aggregate catalog.
    ///
    /// # Panics
    ///
    /// Panics if `members.len() > MAX_SOURCES` — only [`MAX_SOURCES`] sources
    /// fit in the [`SOURCE_BITS`] of a compound id.
    #[must_use]
    pub fn new(members: Vec<Box<dyn Catalog + Send>>) -> Self {
        assert!(
            members.len() <= MAX_SOURCES,
            "MultiCatalog addresses at most {MAX_SOURCES} sources (SOURCE_BITS={SOURCE_BITS}), got {}",
            members.len()
        );
        const NONE_ORDER: Option<Vec<u32>> = None;
        Self {
            members,
            order_cache: Mutex::new([NONE_ORDER; AXIS_COUNT]),
        }
    }

    /// Borrow member `s`, or `OutOfRange` if the source index is unknown.
    fn member(&self, s: u32) -> Result<&(dyn Catalog + Send), CatalogError> {
        self.members
            .get(s as usize)
            .map(|b| &**b)
            .ok_or(CatalogError::OutOfRange)
    }

    /// Materialize the full merged order for `axis` as compound ids, by a k-way
    /// min-heap merge over the members' per-axis streams.
    fn materialize(&self, axis: BrowseAxis) -> Result<Vec<u32>, CatalogError> {
        let total = self.order_len(axis) as usize;
        let mut out = Vec::with_capacity(total);
        let mut cursors = vec![0u32; self.members.len()];
        let mut heap: BinaryHeap<Reverse<Head>> = BinaryHeap::new();

        // Seed the heap with each member's first (smallest) head.
        for (idx, m) in self.members.iter().enumerate() {
            if m.order_len(axis) > 0 {
                let local = m.order(axis, 0)?;
                heap.push(Reverse(self.head_key(axis, idx as u32, local)?));
                cursors[idx] = 1;
            }
        }

        // Pop the global minimum, emit it, and advance that member's stream.
        while let Some(Reverse(head)) = heap.pop() {
            let src = head.source;
            out.push(pack(src, head.local));
            let idx = src as usize;
            let m = &*self.members[idx];
            let cur = cursors[idx];
            if cur < m.order_len(axis) {
                let local = m.order(axis, cur)?;
                cursors[idx] = cur + 1;
                heap.push(Reverse(self.head_key(axis, src, local)?));
            }
        }
        Ok(out)
    }

    /// Build the comparison key for one head track `(s, local)` on `axis`,
    /// resolving its strings from `members[s]` and collating them exactly as the
    /// baker does. `(source, local)` are the final, deterministic tiebreak.
    fn head_key(&self, axis: BrowseAxis, s: u32, local: u32) -> Result<Head, CatalogError> {
        let m = &*self.members[s as usize];
        let rec = m.track(local)?;
        // Reproduce the baker's per-axis tiebreak chain. The baker groups by the
        // grouping entity's id BEFORE descending to (disc, track); across sources
        // the union assigns those ids in (source, local) order, so `source` plus
        // the entity's *local* id stand in for the union id — and must sort
        // before (disc, track), else two same-collation-key albums/artists/
        // folders on different sources would interleave instead of staying
        // grouped. `secondary`/`group1` are the 2nd collation level used only by
        // `ArtistAlbumTrack` (album key + album id under the album-artist).
        let (primary, group0, secondary, group1, disc, track) = match axis {
            BrowseAxis::Title => {
                let title = resolve_ref(m, rec.title)?;
                (key(&title), local, Vec::new(), 0u32, 0u16, 0u16)
            }
            BrowseAxis::ArtistAlbumTrack => {
                let album_artist = resolve_name(|buf| m.artist_name(rec.album_artist, buf))?;
                let album = m.album(rec.album)?;
                let album_title = resolve_ref(m, album.title)?;
                (
                    key(&album_artist),
                    rec.album_artist,
                    key(&album_title),
                    rec.album,
                    rec.disc_no,
                    rec.track_no,
                )
            }
            BrowseAxis::Album => {
                let album = m.album(rec.album)?;
                let album_title = resolve_ref(m, album.title)?;
                (
                    key(&album_title),
                    rec.album,
                    Vec::new(),
                    0u32,
                    rec.disc_no,
                    rec.track_no,
                )
            }
            BrowseAxis::Folder => {
                let path = resolve_name(|buf| m.folder_path(rec.folder, buf))?;
                (
                    key(&path),
                    rec.folder,
                    Vec::new(),
                    0u32,
                    rec.disc_no,
                    rec.track_no,
                )
            }
        };
        Ok(Head {
            primary,
            source: s,
            group0,
            secondary,
            group1,
            disc,
            track,
            local,
        })
    }
}

/// A heap entry for the k-way merge. Derived `Ord` compares fields in
/// declaration order — `primary` (collation key), `source`, `group0` (the
/// primary grouping entity's local id), `secondary` (2nd collation key),
/// `group1` (2nd entity's local id), `disc`, `track`, `local` — which
/// reproduces the baker's per-axis order. Across sources, `(source, group*)`
/// stands in for the union's grouping-entity id (assigned in source-then-local
/// order); `source` precedes `(disc, track)` so one same-keyed album/artist/
/// folder's tracks stay contiguous rather than interleaving with another
/// source's. `local` (the track id) is the final deterministic tiebreak and the
/// id emitted for this head. (`Title` has no grouping entity, so `group0` is the
/// track id itself and `secondary`/`group1`/`disc`/`track` are inert.)
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Head {
    primary: Vec<u8>,
    source: u32,
    group0: u32,
    secondary: Vec<u8>,
    group1: u32,
    disc: u16,
    track: u16,
    local: u32,
}

/// Resolve a heap `StrRef` (exact length known) into an owned `String`.
fn resolve_ref(cat: &dyn Catalog, s: StrRef) -> Result<String, CatalogError> {
    let mut buf = vec![0u8; s.len as usize];
    let n = cat.resolve_str(s, &mut buf)?;
    buf.truncate(n);
    String::from_utf8(buf).map_err(|_| CatalogError::Corrupt)
}

/// Resolve a name whose length isn't known up front (artist name, folder path),
/// growing the buffer until it fits.
fn resolve_name<F: Fn(&mut [u8]) -> Result<usize, CatalogError>>(
    f: F,
) -> Result<String, CatalogError> {
    let mut buf = vec![0u8; 256];
    loop {
        match f(&mut buf) {
            Ok(n) => {
                buf.truncate(n);
                return String::from_utf8(buf).map_err(|_| CatalogError::Corrupt);
            }
            Err(CatalogError::Truncated) => {
                if buf.len() >= 1 << 20 {
                    return Err(CatalogError::Truncated);
                }
                buf.resize(buf.len() * 2, 0);
            }
            Err(e) => return Err(e),
        }
    }
}

impl Catalog for MultiCatalog {
    fn track_count(&self) -> u32 {
        self.members.iter().map(|m| m.track_count()).sum()
    }

    fn album_count(&self) -> u32 {
        self.members.iter().map(|m| m.album_count()).sum()
    }

    fn artist_count(&self) -> u32 {
        self.members.iter().map(|m| m.artist_count()).sum()
    }

    fn track(&self, id: TrackId) -> Result<TrackRec, CatalogError> {
        let (s, local) = unpack(id);
        let mut r = self.member(s)?.track(local)?;
        r.id = pack(s, r.id);
        r.artist = pack(s, r.artist);
        r.album = pack(s, r.album);
        r.album_artist = pack(s, r.album_artist);
        r.folder = pack(s, r.folder);
        r.title.off = pack(s, r.title.off);
        r.file_name.off = pack(s, r.file_name.off);
        // `art_id` and `extended_off` are per-source offsets (into `art.blob` /
        // `extended.blob`) and are deliberately NOT re-packed: both are always 0
        // today (artbake + extended-TLV unimplemented), and `pack(s, 0)` would
        // wrongly turn a "no art" / "no extended" 0 into a non-zero id. When
        // those blobs land, route them through `pack(s, _)` *behind a sentinel
        // guard* (as `folder.parent`/`NO_FOLDER` is guarded below) so they
        // resolve against the owning source.
        Ok(r)
    }

    fn album(&self, id: AlbumId) -> Result<AlbumRec, CatalogError> {
        let (s, local) = unpack(id);
        let mut r = self.member(s)?.album(local)?;
        r.id = pack(s, r.id);
        r.album_artist = pack(s, r.album_artist);
        r.folder = pack(s, r.folder);
        r.title.off = pack(s, r.title.off);
        // `art_id` not re-packed — see the note in `track()` (artbake follow-up).
        Ok(r)
    }

    fn artist_name(&self, id: ArtistId, buf: &mut [u8]) -> Result<usize, CatalogError> {
        let (s, local) = unpack(id);
        self.member(s)?.artist_name(local, buf)
    }

    fn resolve_str(&self, s: StrRef, buf: &mut [u8]) -> Result<usize, CatalogError> {
        let (src, off) = unpack(s.off);
        self.member(src)?
            .resolve_str(StrRef { off, len: s.len }, buf)
    }

    fn order_len(&self, axis: BrowseAxis) -> u32 {
        self.members.iter().map(|m| m.order_len(axis)).sum()
    }

    fn order(&self, axis: BrowseAxis, i: u32) -> Result<TrackId, CatalogError> {
        let ax = axis_index(axis);
        // Hold the lock across materialize: it reads the members' streams, never
        // this cache, so there is no re-entrancy on `order_cache` and no deadlock.
        let mut cache = self.order_cache.lock().expect("order-cache mutex poisoned");
        if cache[ax].is_none() {
            cache[ax] = Some(self.materialize(axis)?);
        }
        cache[ax]
            .as_ref()
            .expect("materialized above")
            .get(i as usize)
            .copied()
            .ok_or(CatalogError::OutOfRange)
    }

    fn album_track(&self, album: AlbumId, i: u32) -> Result<TrackId, CatalogError> {
        let (s, local) = unpack(album);
        let tid = self.member(s)?.album_track(local, i)?;
        Ok(pack(s, tid))
    }

    fn album_track_count(&self, album: AlbumId) -> u32 {
        let (s, local) = unpack(album);
        self.member(s).map_or(0, |m| m.album_track_count(local))
    }

    fn folder_count(&self) -> u32 {
        self.members.iter().map(|m| m.folder_count()).sum()
    }

    fn folder(&self, id: FolderId) -> Result<FolderRec, CatalogError> {
        let (s, local) = unpack(id);
        let mut r = self.member(s)?.folder(local)?;
        r.id = pack(s, r.id);
        r.path.off = pack(s, r.path.off);
        // Leave the "no parent" sentinel intact; only re-pack a real parent id.
        if r.parent != NO_FOLDER {
            r.parent = pack(s, r.parent);
        }
        Ok(r)
    }

    fn folder_path(&self, id: FolderId, buf: &mut [u8]) -> Result<usize, CatalogError> {
        let (s, local) = unpack(id);
        self.member(s)?.folder_path(local, buf)
    }

    fn playlist_count(&self) -> u32 {
        self.members.iter().map(|m| m.playlist_count()).sum()
    }

    fn playlist_name(&self, p: PlaylistId, buf: &mut [u8]) -> Result<usize, CatalogError> {
        let (s, local) = unpack(p);
        self.member(s)?.playlist_name(local, buf)
    }

    fn playlist_len(&self, p: PlaylistId) -> u32 {
        let (s, local) = unpack(p);
        self.member(s).map_or(0, |m| m.playlist_len(local))
    }

    fn playlist_track(&self, p: PlaylistId, i: u32) -> Result<TrackId, CatalogError> {
        let (s, local) = unpack(p);
        let tid = self.member(s)?.playlist_track(local, i)?;
        Ok(pack(s, tid))
    }
}
