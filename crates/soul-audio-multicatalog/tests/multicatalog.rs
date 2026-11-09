//! Acceptance gate for `MultiCatalog` (written red-first).
//!
//! Member catalogs are built with the real `.soulidx` baker (`build` feature)
//! and read back over an in-memory `SliceSource`, so the merge is tested against
//! the *actual* collation order the device would see — not a stub. Test data
//! uses globally-distinct, collation-non-colliding names so every order is
//! unambiguous.

use soul_audio_catalog::{BrowseAxis, Catalog, CatalogError, NO_FOLDER, StrRef};
use soul_audio_core::SliceSource;
use soul_audio_index::IndexReader;
use soul_audio_index::build::write_index;
use soul_audio_index::build::{AlbumMeta, CatalogInput, FolderInput, PlaylistInput, TrackInput};
use soul_audio_multicatalog::{MAX_LOCAL_ID, MultiCatalog, pack, unpack};

const AXES: [BrowseAxis; 4] = [
    BrowseAxis::Title,
    BrowseAxis::ArtistAlbumTrack,
    BrowseAxis::Album,
    BrowseAxis::Folder,
];

// --- Neutral test-data description (Clone-able; drives BOTH the per-member
// inputs and the single union input, so their content is identical) ---

#[derive(Clone, Copy)]
struct TrackData {
    title: &'static str,
    artist: u32,
    album: u32,
    album_artist: u32,
    folder: u32,
    disc: u16,
    track: u16,
    file: &'static str,
}

#[derive(Clone)]
struct MemberData {
    artists: Vec<&'static str>,
    /// (title, album_artist idx, folder idx)
    albums: Vec<(&'static str, u32, u32)>,
    /// (path, parent idx or NO_FOLDER)
    folders: Vec<(&'static str, u32)>,
    tracks: Vec<TrackData>,
    /// (name, local track indices)
    playlists: Vec<(&'static str, Vec<u32>)>,
}

/// Three sources, each a self-contained library with distinct names.
fn dataset() -> Vec<MemberData> {
    vec![
        MemberData {
            artists: vec!["alice"],
            albums: vec![("mercury", 0, 1)],
            folders: vec![("", NO_FOLDER), ("rock/alice", 0)],
            tracks: vec![
                TrackData {
                    title: "foxtrot",
                    artist: 0,
                    album: 0,
                    album_artist: 0,
                    folder: 1,
                    disc: 1,
                    track: 1,
                    file: "foxtrot.flac",
                },
                TrackData {
                    title: "echo",
                    artist: 0,
                    album: 0,
                    album_artist: 0,
                    folder: 1,
                    disc: 1,
                    track: 2,
                    file: "echo.flac",
                },
            ],
            playlists: vec![("alice-mix", vec![1, 0])],
        },
        MemberData {
            artists: vec!["bob"],
            albums: vec![("neptune", 0, 1)],
            folders: vec![("", NO_FOLDER), ("jazz/bob", 0)],
            tracks: vec![
                TrackData {
                    title: "golf",
                    artist: 0,
                    album: 0,
                    album_artist: 0,
                    folder: 1,
                    disc: 1,
                    track: 1,
                    file: "golf.flac",
                },
                TrackData {
                    title: "hotel",
                    artist: 0,
                    album: 0,
                    album_artist: 0,
                    folder: 1,
                    disc: 1,
                    track: 2,
                    file: "hotel.flac",
                },
            ],
            playlists: vec![("bob-mix", vec![0, 1])],
        },
        MemberData {
            artists: vec!["carol"],
            albums: vec![("venus", 0, 1)],
            folders: vec![("", NO_FOLDER), ("pop/carol", 0)],
            tracks: vec![TrackData {
                title: "india",
                artist: 0,
                album: 0,
                album_artist: 0,
                folder: 1,
                disc: 1,
                track: 1,
                file: "india.flac",
            }],
            playlists: vec![],
        },
    ]
}

fn track_input(t: &TrackData) -> TrackInput {
    TrackInput {
        title: t.title.into(),
        artist: t.artist,
        album: t.album,
        album_artist: t.album_artist,
        folder: t.folder,
        track_no: t.track,
        disc_no: t.disc,
        duration_ms: 0,
        sample_rate: 44_100,
        channels: 2,
        file_name: t.file.into(),
    }
}

/// One member's baker input, ids local to that member.
fn to_input(m: &MemberData) -> CatalogInput {
    CatalogInput {
        tracks: m.tracks.iter().map(track_input).collect(),
        artists: m.artists.iter().map(|a| (*a).into()).collect(),
        albums: m
            .albums
            .iter()
            .map(|(title, aa, folder)| AlbumMeta {
                title: (*title).into(),
                album_artist: *aa,
                year: 0,
                folder: *folder,
            })
            .collect(),
        folders: m
            .folders
            .iter()
            .map(|(path, parent)| FolderInput {
                path: (*path).into(),
                parent: *parent,
            })
            .collect(),
        playlists: m
            .playlists
            .iter()
            .map(|(name, tracks)| PlaylistInput {
                name: (*name).into(),
                tracks: tracks.clone(),
            })
            .collect(),
        covers: Vec::new(),
    }
}

/// The union of all members in ONE index, with ids offset per source — the exact
/// same content the MultiCatalog aggregates, baked as a single catalog.
fn to_union(ms: &[MemberData]) -> CatalogInput {
    let mut u = CatalogInput::default();
    let (mut artist_base, mut album_base, mut folder_base) = (0u32, 0u32, 0u32);
    for m in ms {
        for t in &m.tracks {
            let mut ti = track_input(t);
            ti.artist += artist_base;
            ti.album += album_base;
            ti.album_artist += artist_base;
            ti.folder += folder_base;
            u.tracks.push(ti);
        }
        for a in &m.artists {
            u.artists.push((*a).into());
        }
        for (title, aa, folder) in &m.albums {
            u.albums.push(AlbumMeta {
                title: (*title).into(),
                album_artist: *aa + artist_base,
                year: 0,
                folder: *folder + folder_base,
            });
        }
        for (path, parent) in &m.folders {
            let parent = if *parent == NO_FOLDER {
                NO_FOLDER
            } else {
                *parent + folder_base
            };
            u.folders.push(FolderInput {
                path: (*path).into(),
                parent,
            });
        }
        artist_base += m.artists.len() as u32;
        album_base += m.albums.len() as u32;
        folder_base += m.folders.len() as u32;
    }
    u
}

/// Bake `input` and wrap the reader as a `'static` boxed `Catalog` (leaks the
/// bytes — fine in a test).
fn boxed_member(input: &CatalogInput) -> Box<dyn Catalog + Send> {
    let bytes: &'static [u8] = Vec::leak(write_index(input));
    Box::new(IndexReader::new(SliceSource::new(bytes)))
}

fn reader(input: &CatalogInput) -> IndexReader<SliceSource<'static>> {
    let bytes: &'static [u8] = Vec::leak(write_index(input));
    IndexReader::new(SliceSource::new(bytes))
}

// --- resolution helpers that work for ANY Catalog (compound-id multi OR the
// global-id union), since they only use the trait surface ---

fn resolve(cat: &dyn Catalog, s: StrRef) -> String {
    let mut buf = [0u8; 256];
    let n = cat.resolve_str(s, &mut buf).expect("resolve_str");
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

fn artist(cat: &dyn Catalog, id: u32) -> String {
    let mut buf = [0u8; 256];
    let n = cat.artist_name(id, &mut buf).expect("artist_name");
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

fn folder_path(cat: &dyn Catalog, id: u32) -> String {
    let mut buf = [0u8; 256];
    let n = cat.folder_path(id, &mut buf).expect("folder_path");
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

fn album_title(cat: &dyn Catalog, album: u32) -> String {
    let a = cat.album(album).expect("album");
    resolve(cat, a.title)
}

/// The *resolved sort fields* for `id` on `axis` — what actually determines
/// order. Compared instead of raw ids (compound vs global id spaces differ).
fn fields(cat: &dyn Catalog, axis: BrowseAxis, id: u32) -> String {
    let t = cat.track(id).expect("track");
    match axis {
        BrowseAxis::Title => resolve(cat, t.title),
        BrowseAxis::ArtistAlbumTrack => format!(
            "{}|{}|{}|{}",
            artist(cat, t.album_artist),
            album_title(cat, t.album),
            t.disc_no,
            t.track_no
        ),
        BrowseAxis::Album => {
            format!("{}|{}|{}", album_title(cat, t.album), t.disc_no, t.track_no)
        }
        BrowseAxis::Folder => {
            format!(
                "{}|{}|{}",
                folder_path(cat, t.folder),
                t.disc_no,
                t.track_no
            )
        }
    }
}

fn order_fields(cat: &dyn Catalog, axis: BrowseAxis) -> Vec<String> {
    (0..cat.order_len(axis))
        .map(|i| fields(cat, axis, cat.order(axis, i).expect("order")))
        .collect()
}

/// THE INVARIANT: the aggregate's k-way-merged order equals baking the union,
/// for every browse axis — compared by resolved sort fields.
#[test]
fn merged_order_equals_union_bake() {
    let ms = dataset();
    let multi = MultiCatalog::new(ms.iter().map(|m| boxed_member(&to_input(m))).collect());
    let union = reader(&to_union(&ms));

    for axis in AXES {
        let got = order_fields(&multi, axis);
        let want = order_fields(&union, axis);
        assert_eq!(
            got, want,
            "merged order diverges from the union bake on axis {axis:?}"
        );
    }

    // Guard the fixtures are actually unambiguous (no duplicate field tuples that
    // could mask a real divergence).
    for axis in AXES {
        let mut v = order_fields(&union, axis);
        let n = v.len();
        v.sort();
        v.dedup();
        assert_eq!(v.len(), n, "fixture ambiguity on axis {axis:?}");
    }
}

/// Reads route to the owning source, and every returned id/StrRef is re-packed
/// with that source so follow-up reads resolve correctly.
#[test]
fn routing() {
    let ms = dataset();
    let multi = MultiCatalog::new(ms.iter().map(|m| boxed_member(&to_input(m))).collect());

    // Counts are the sum over members.
    assert_eq!(multi.track_count(), 5);
    assert_eq!(multi.album_count(), 3);
    assert_eq!(multi.artist_count(), 3);
    // 2 folders per member (root + album dir) × 3 members.
    assert_eq!(multi.folder_count(), 6);
    assert_eq!(multi.playlist_count(), 2);

    // "golf" is member 1's local track 0.
    let gid = pack(1, 0);
    let g = multi.track(gid).expect("golf");
    assert_eq!(resolve(&multi, g.title), "golf");
    // Every returned id carries source 1.
    assert_eq!(unpack(g.id), (1, 0));
    assert_eq!(unpack(g.album).0, 1);
    assert_eq!(unpack(g.album_artist).0, 1);
    assert_eq!(unpack(g.folder).0, 1);
    assert_eq!(unpack(g.title.off).0, 1);
    assert_eq!(unpack(g.file_name.off).0, 1);

    // unpack(folder) points at the right folder, whose path routes back.
    assert_eq!(folder_path(&multi, g.folder), "jazz/bob");
    let f = multi.folder(g.folder).expect("folder");
    assert_eq!(resolve(&multi, f.path), "jazz/bob");
    assert_eq!(unpack(f.id).0, 1);

    // album / artist route correctly.
    assert_eq!(album_title(&multi, g.album), "neptune");
    assert_eq!(artist(&multi, g.album_artist), "bob");

    // album_track routes AND re-packs the returned track id (source 1).
    assert_eq!(multi.album_track_count(g.album), 2);
    let a0 = multi.album_track(g.album, 0).expect("album_track 0");
    let a1 = multi.album_track(g.album, 1).expect("album_track 1");
    assert_eq!(unpack(a0).0, 1);
    assert_eq!(unpack(a1).0, 1);
    // disc/track order within the album: golf (t1) then hotel (t2).
    assert_eq!(resolve(&multi, multi.track(a0).unwrap().title), "golf");
    assert_eq!(resolve(&multi, multi.track(a1).unwrap().title), "hotel");

    // A track from source 0 and source 2 also route.
    assert_eq!(
        resolve(&multi, multi.track(pack(0, 0)).unwrap().title),
        "foxtrot"
    );
    assert_eq!(
        resolve(&multi, multi.track(pack(2, 0)).unwrap().title),
        "india"
    );

    // Folder parent re-packing: album dir's parent is the member's root folder.
    let root = multi.folder(f.parent).expect("root folder");
    assert_eq!(root.path.len, 0);
    assert_eq!(root.parent, NO_FOLDER);
    assert_eq!(unpack(f.parent).0, 1);

    // Playlists route by source and re-pack their track ids.
    let bob_mix = pack(1, 0); // member 1's local playlist 0
    let mut nbuf = [0u8; 64];
    let n = multi.playlist_name(bob_mix, &mut nbuf).unwrap();
    assert_eq!(&nbuf[..n], b"bob-mix");
    assert_eq!(multi.playlist_len(bob_mix), 2);
    let p0 = multi.playlist_track(bob_mix, 0).unwrap();
    assert_eq!(unpack(p0).0, 1);
    assert_eq!(resolve(&multi, multi.track(p0).unwrap().title), "golf");
}

/// The desktop moves the aggregate across threads inside a `Mutex` (`Arc<Library>`
/// over the actor + event-pump threads), so it must be `Send`. It is deliberately
/// not `Sync` — the member readers are single-threaded (`RefCell` source) — so the
/// desktop serializes access behind its own lock.
#[test]
fn multicatalog_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<MultiCatalog>();
}

/// `unpack(pack(s, l)) == (s, l)` across the valid ranges.
#[test]
fn pack_unpack_roundtrip() {
    for s in 0..16u32 {
        for &l in &[0u32, 1, 2, 100, 12_345, 1_000_000, MAX_LOCAL_ID] {
            assert_eq!(unpack(pack(s, l)), (s, l), "roundtrip s={s} l={l}");
        }
    }
}

// ===========================================================================
// Comprehensive / realistic coverage. These use *globally unique track titles*
// as a precise order oracle (`order_track_titles`): unlike the resolved
// group-fields (album/disc/track), unique titles pin the EXACT track sequence,
// so a divergence where two same-collation-key albums/artists/folders across
// sources get interleaved instead of grouped is actually observable. Each
// `assert_merge_equals_union` dataset is also baked as a single union catalog
// (`to_union`) — the ground truth for "aggregate N == bake the union".
// ===========================================================================

/// Compact `TrackData` ctor: artist == album_artist, file == title (titles are
/// unique per fixture, so the file name is too).
fn trk(
    title: &'static str,
    album_artist: u32,
    album: u32,
    folder: u32,
    disc: u16,
    track: u16,
) -> TrackData {
    TrackData {
        title,
        artist: album_artist,
        album,
        album_artist,
        folder,
        disc,
        track,
        file: title,
    }
}

/// Resolved track titles in `axis` order — the precise order oracle.
fn order_track_titles(cat: &dyn Catalog, axis: BrowseAxis) -> Vec<String> {
    (0..cat.order_len(axis))
        .map(|i| {
            let id = cat.order(axis, i).expect("order");
            resolve(cat, cat.track(id).expect("track").title)
        })
        .collect()
}

/// THE core property, by the unique-title oracle: the aggregate's merged order
/// equals baking the union of the same content, on every axis. Also asserts the
/// titles are globally unique so the oracle can't silently alias a divergence.
fn assert_merge_equals_union(ms: &[MemberData]) {
    let multi = MultiCatalog::new(ms.iter().map(|m| boxed_member(&to_input(m))).collect());
    let union = reader(&to_union(ms));
    for axis in AXES {
        let got = order_track_titles(&multi, axis);
        let want = order_track_titles(&union, axis);

        let mut uniq = want.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(
            uniq.len(),
            want.len(),
            "fixture has non-unique track titles; the oracle can't observe order on {axis:?}"
        );

        assert_eq!(
            got, want,
            "merged order diverges from union bake on {axis:?}"
        );
    }
}

fn root() -> (&'static str, u32) {
    ("", NO_FOLDER)
}

/// Two albums that collate to the SAME key on different sources must stay
/// GROUPED (all of one album's tracks, then the other's) — not interleaved by
/// disc/track. Track numbers are arranged so a (disc,track)-first merge would
/// reorder them, exposing the bug. (Album axis.)
#[test]
fn same_album_title_across_sources_stays_grouped() {
    let ms = vec![
        MemberData {
            artists: vec!["anna"],
            albums: vec![("Greatest Hits", 0, 1)],
            folders: vec![root(), ("a", 0)],
            // track 5 then track 1: a (disc,track)-first merge would pull src1's
            // track 3 between them.
            tracks: vec![trk("a_late", 0, 0, 1, 1, 5), trk("a_early", 0, 0, 1, 1, 1)],
            playlists: vec![],
        },
        MemberData {
            artists: vec!["bill"],
            albums: vec![("Greatest Hits", 0, 1)],
            folders: vec![root(), ("b", 0)],
            tracks: vec![trk("b_mid", 0, 0, 1, 1, 3)],
            playlists: vec![],
        },
    ];
    assert_merge_equals_union(&ms);
}

/// Same artist AND same album title on two sources (you own an album locally and
/// it's also on your server): each source's copy stays grouped.
/// (ArtistAlbumTrack axis.)
#[test]
fn same_artist_and_album_across_sources_stays_grouped() {
    let ms = vec![
        MemberData {
            artists: vec!["Muse"],
            albums: vec![("Absolution", 0, 1)],
            folders: vec![root(), ("local", 0)],
            tracks: vec![trk("local_t4", 0, 0, 1, 1, 4)],
            playlists: vec![],
        },
        MemberData {
            artists: vec!["Muse"],
            albums: vec![("Absolution", 0, 1)],
            folders: vec![root(), ("server", 0)],
            tracks: vec![trk("server_t2", 0, 0, 1, 1, 2)],
            playlists: vec![],
        },
    ];
    assert_merge_equals_union(&ms);
}

/// Two sources with an identically-named folder path keep each folder's tracks
/// grouped rather than interleaved. (Folder axis.)
#[test]
fn same_folder_path_across_sources_stays_grouped() {
    let ms = vec![
        MemberData {
            artists: vec!["x"],
            albums: vec![("xa", 0, 1)],
            folders: vec![root(), ("Rock/Live", 0)],
            tracks: vec![trk("x_t9", 0, 0, 1, 1, 9)],
            playlists: vec![],
        },
        MemberData {
            artists: vec!["y"],
            albums: vec![("ya", 0, 1)],
            folders: vec![root(), ("Rock/Live", 0)],
            tracks: vec![trk("y_t2", 0, 0, 1, 1, 2)],
            playlists: vec![],
        },
    ];
    assert_merge_equals_union(&ms);
}

/// Collation itself survives the cross-source merge: leading articles fold and
/// numbers sort naturally even when the colliding entries live on different
/// sources. (No grouping collision — this is the "merge respects the key" case.)
#[test]
fn collation_articles_and_numbers_merge_across_sources() {
    let ms = vec![
        MemberData {
            artists: vec!["The Kinks"],
            albums: vec![("One for the Road", 0, 1)],
            folders: vec![root(), ("kinks", 0)],
            tracks: vec![
                trk("Track 10", 0, 0, 1, 1, 10),
                trk("Track 2", 0, 0, 1, 1, 2),
            ],
            playlists: vec![],
        },
        MemberData {
            artists: vec!["ABBA"],
            albums: vec![("Arrival", 0, 1)],
            folders: vec![root(), ("abba", 0)],
            tracks: vec![trk("Track 1", 0, 0, 1, 1, 1)],
            playlists: vec![],
        },
    ];
    assert_merge_equals_union(&ms);
}

/// Unicode + case folding across sources (collation lower-cases).
#[test]
fn unicode_and_case_fold_merge_across_sources() {
    let ms = vec![
        MemberData {
            artists: vec!["Zebra"],
            albums: vec![("Z", 0, 1)],
            folders: vec![root(), ("z", 0)],
            tracks: vec![
                trk("Éclair", 0, 0, 1, 1, 1),
                trk("ABBA song", 0, 0, 1, 1, 2),
            ],
            playlists: vec![],
        },
        MemberData {
            artists: vec!["mango"],
            albums: vec![("M", 0, 1)],
            folders: vec![root(), ("m", 0)],
            tracks: vec![
                trk("éclair twin", 0, 0, 1, 1, 1),
                trk("abba tune", 0, 0, 1, 1, 2),
            ],
            playlists: vec![],
        },
    ];
    assert_merge_equals_union(&ms);
}

/// Folder isolation (same album title in two folders of ONE source = two albums)
/// composed with a second source that shares that title. The within-source split
/// must survive, and both sources stay grouped.
#[test]
fn folder_isolation_within_source_survives_merge() {
    let ms = vec![
        MemberData {
            artists: vec!["band"],
            // Two DISTINCT albums, same title "Live", different folders.
            albums: vec![("Live", 0, 1), ("Live", 0, 2)],
            folders: vec![root(), ("band/2001", 0), ("band/2002", 0)],
            tracks: vec![
                trk("live01_a", 0, 0, 1, 1, 1),
                trk("live02_a", 0, 1, 2, 1, 1),
            ],
            playlists: vec![],
        },
        MemberData {
            artists: vec!["other"],
            albums: vec![("Live", 0, 1)],
            folders: vec![root(), ("other/live", 0)],
            tracks: vec![trk("live_other", 0, 0, 1, 1, 1)],
            playlists: vec![],
        },
    ];
    assert_merge_equals_union(&ms);
}

/// Multi-disc albums order by (disc, track) within the album, across the merge.
#[test]
fn multi_disc_ordering_across_the_merge() {
    let ms = vec![
        MemberData {
            artists: vec!["composer"],
            albums: vec![("Symphony Box", 0, 1)],
            folders: vec![root(), ("box", 0)],
            tracks: vec![
                trk("d2t1", 0, 0, 1, 2, 1),
                trk("d1t2", 0, 0, 1, 1, 2),
                trk("d1t1", 0, 0, 1, 1, 1),
            ],
            playlists: vec![],
        },
        MemberData {
            artists: vec!["soloist"],
            albums: vec![("Recital", 0, 1)],
            folders: vec![root(), ("recital", 0)],
            tracks: vec![trk("recital1", 0, 0, 1, 1, 1)],
            playlists: vec![],
        },
    ];
    assert_merge_equals_union(&ms);
}

/// N = 1 (the DAP-equivalent path): the aggregate is a faithful pass-through of
/// the single member's order, byte for byte.
#[test]
fn single_source_is_passthrough() {
    let ms = dataset();
    assert_merge_equals_union(&ms[..1]);

    let multi = MultiCatalog::new(vec![boxed_member(&to_input(&ms[0]))]);
    let direct = reader(&to_input(&ms[0]));
    for axis in AXES {
        assert_eq!(
            order_track_titles(&multi, axis),
            order_track_titles(&direct, axis),
            "N=1 not a pass-through on {axis:?}"
        );
    }
}

/// A configured-but-empty source (0 tracks) contributes nothing to any order and
/// doesn't perturb the merge of the others.
#[test]
fn empty_source_in_the_mix() {
    let ms = vec![
        MemberData {
            artists: vec!["alpha"],
            albums: vec![("A1", 0, 1)],
            folders: vec![root(), ("al", 0)],
            tracks: vec![trk("alpha_song", 0, 0, 1, 1, 1)],
            playlists: vec![],
        },
        // Empty: present (an entry exists) but holds no tracks yet.
        MemberData {
            artists: vec!["ghost"],
            albums: vec![("G1", 0, 1)],
            folders: vec![root(), ("gh", 0)],
            tracks: vec![],
            playlists: vec![],
        },
        MemberData {
            artists: vec!["omega"],
            albums: vec![("O1", 0, 1)],
            folders: vec![root(), ("om", 0)],
            tracks: vec![trk("omega_song", 0, 0, 1, 1, 1)],
            playlists: vec![],
        },
    ];
    assert_merge_equals_union(&ms);

    let multi = MultiCatalog::new(ms.iter().map(|m| boxed_member(&to_input(m))).collect());
    assert_eq!(multi.track_count(), 2, "empty source adds no tracks");
    for axis in AXES {
        assert_eq!(
            multi.order_len(axis),
            2,
            "{axis:?} length ignores empty source"
        );
    }
}

/// Every source empty: zero-length orders, and an in-range-looking index still
/// reports out of range.
#[test]
fn all_sources_empty() {
    let empty = || MemberData {
        artists: vec!["a"],
        albums: vec![("x", 0, 1)],
        folders: vec![root(), ("f", 0)],
        tracks: vec![],
        playlists: vec![],
    };
    let ms = [empty(), empty()];
    let multi = MultiCatalog::new(ms.iter().map(|m| boxed_member(&to_input(m))).collect());
    assert_eq!(multi.track_count(), 0);
    for axis in AXES {
        assert_eq!(multi.order_len(axis), 0);
        assert_eq!(multi.order(axis, 0), Err(CatalogError::OutOfRange));
    }
}

/// The source-count boundary: exactly `MAX_SOURCES` (16) members merge and route.
#[test]
fn max_sources_boundary() {
    // 16 single-track sources with distinct, natural-number-sorted names.
    const NAMES: [&str; 16] = [
        "n00", "n01", "n02", "n03", "n04", "n05", "n06", "n07", "n08", "n09", "n10", "n11", "n12",
        "n13", "n14", "n15",
    ];
    let mk = |i: usize| MemberData {
        artists: vec![NAMES[i]],
        albums: vec![(NAMES[i], 0, 1)],
        folders: vec![root(), (NAMES[i], 0)],
        tracks: vec![trk(NAMES[i], 0, 0, 1, 1, 1)],
        playlists: vec![],
    };
    let ms: Vec<MemberData> = (0..16).map(mk).collect();
    let multi = MultiCatalog::new(ms.iter().map(|m| boxed_member(&to_input(m))).collect());
    assert_eq!(multi.track_count(), 16);
    assert_merge_equals_union(&ms);

    // Highest source index (15) still routes.
    let t = multi.track(pack(15, 0)).expect("source 15 track");
    assert_eq!(resolve(&multi, t.title), "n15");
}

/// One past the boundary panics rather than silently truncating a source index.
#[test]
#[should_panic(expected = "at most")]
fn seventeen_sources_panics() {
    let one = || {
        boxed_member(&to_input(&MemberData {
            artists: vec!["a"],
            albums: vec![("x", 0, 1)],
            folders: vec![root(), ("f", 0)],
            tracks: vec![trk("t", 0, 0, 1, 1, 1)],
            playlists: vec![],
        }))
    };
    let members: Vec<Box<dyn Catalog + Send>> = (0..17).map(|_| one()).collect();
    let _ = MultiCatalog::new(members);
}

/// Bad source indices and bad local ids are rejected, not misrouted.
#[test]
fn out_of_range_ids_are_rejected() {
    let ms = dataset(); // 3 sources
    let multi = MultiCatalog::new(ms.iter().map(|m| boxed_member(&to_input(m))).collect());
    let mut buf = [0u8; 64];

    // Unknown source (only 0..3 exist).
    assert_eq!(multi.track(pack(7, 0)), Err(CatalogError::OutOfRange));
    assert_eq!(multi.album(pack(9, 0)), Err(CatalogError::OutOfRange));
    assert_eq!(multi.folder(pack(5, 0)), Err(CatalogError::OutOfRange));
    assert_eq!(
        multi.artist_name(pack(8, 0), &mut buf),
        Err(CatalogError::OutOfRange)
    );
    assert_eq!(
        multi.resolve_str(
            StrRef {
                off: pack(6, 0),
                len: 1
            },
            &mut buf
        ),
        Err(CatalogError::OutOfRange)
    );

    // Valid source, out-of-range local id.
    assert_eq!(multi.track(pack(0, 9_999)), Err(CatalogError::OutOfRange));

    // Out-of-range order index.
    for axis in AXES {
        assert_eq!(multi.order(axis, 9_999), Err(CatalogError::OutOfRange));
    }
}

/// The lazy per-axis order cache returns identical results on repeat reads and
/// across interleaved axes (no cross-axis cache bleed, no first-vs-later drift).
#[test]
fn order_cache_is_stable() {
    let ms = dataset();
    let multi = MultiCatalog::new(ms.iter().map(|m| boxed_member(&to_input(m))).collect());

    // Materialize axes in an interleaved pattern, then compare whole sequences to
    // a fresh full read.
    for axis in AXES {
        let first: Vec<u32> = (0..multi.order_len(axis))
            .map(|i| multi.order(axis, i).unwrap())
            .collect();
        // Poke another axis in between to catch cache-slot confusion.
        let _ = multi.order(BrowseAxis::Album, 0);
        let second: Vec<u32> = (0..multi.order_len(axis))
            .map(|i| multi.order(axis, i).unwrap())
            .collect();
        assert_eq!(first, second, "cache drift on {axis:?}");
    }
}

/// A broader, mixed library: three sources, several artists/albums each, varied
/// disc/track, and a "Live" album shared by all three sources — the general
/// confidence case that merge == union across every axis at once.
#[test]
fn larger_mixed_library_equals_union() {
    let ms = vec![
        MemberData {
            artists: vec!["Portishead", "Massive Attack"],
            albums: vec![("Dummy", 0, 1), ("Mezzanine", 1, 2), ("Live", 0, 3)],
            folders: vec![
                root(),
                ("trip/dummy", 0),
                ("trip/mezz", 0),
                ("trip/live", 0),
            ],
            tracks: vec![
                trk("mysterons", 0, 0, 1, 1, 1),
                trk("sour_times", 0, 0, 1, 1, 2),
                trk("angel", 1, 1, 2, 1, 1),
                trk("teardrop", 1, 1, 2, 1, 3),
                trk("s1_live_b", 0, 2, 3, 1, 2),
                trk("s1_live_a", 0, 2, 3, 1, 1),
            ],
            playlists: vec![("mix1", vec![0, 2])],
        },
        MemberData {
            artists: vec!["Air"],
            albums: vec![("Moon Safari", 0, 1), ("Live", 0, 2)],
            folders: vec![root(), ("air/moon", 0), ("air/live", 0)],
            tracks: vec![
                trk("la_femme", 0, 0, 1, 1, 1),
                trk("sexy_boy", 0, 0, 1, 1, 2),
                trk("kelly", 0, 0, 1, 1, 10),
                trk("s2_live_x", 0, 1, 2, 2, 1),
                trk("s2_live_y", 0, 1, 2, 1, 1),
            ],
            playlists: vec![],
        },
        MemberData {
            artists: vec!["Bonobo"],
            albums: vec![("Black Sands", 0, 1), ("Live", 0, 2)],
            folders: vec![root(), ("bonobo/bs", 0), ("bonobo/live", 0)],
            tracks: vec![
                trk("kiara", 0, 0, 1, 1, 1),
                trk("eyesdown", 0, 0, 1, 1, 3),
                trk("s3_live_1", 0, 1, 2, 1, 1),
            ],
            playlists: vec![("mix3", vec![1, 0])],
        },
    ];
    assert_merge_equals_union(&ms);
}

/// Playlists on multiple sources each route to their owner and re-pack track ids.
#[test]
fn playlists_route_across_sources() {
    let ms = [
        MemberData {
            artists: vec!["a"],
            albums: vec![("a", 0, 1)],
            folders: vec![root(), ("a", 0)],
            tracks: vec![trk("a1", 0, 0, 1, 1, 1), trk("a2", 0, 0, 1, 1, 2)],
            playlists: vec![("A-list", vec![1, 0])],
        },
        MemberData {
            artists: vec!["b"],
            albums: vec![("b", 0, 1)],
            folders: vec![root(), ("b", 0)],
            tracks: vec![trk("b1", 0, 0, 1, 1, 1)],
            playlists: vec![("B-list", vec![0])],
        },
    ];
    let multi = MultiCatalog::new(ms.iter().map(|m| boxed_member(&to_input(m))).collect());
    assert_eq!(multi.playlist_count(), 2);

    // Source 0's playlist: order [1,0] -> a2, a1, both carrying source 0.
    let pa = pack(0, 0);
    let mut nbuf = [0u8; 64];
    let n = multi.playlist_name(pa, &mut nbuf).unwrap();
    assert_eq!(&nbuf[..n], b"A-list");
    assert_eq!(multi.playlist_len(pa), 2);
    let a_first = multi.playlist_track(pa, 0).unwrap();
    assert_eq!(unpack(a_first).0, 0);
    assert_eq!(resolve(&multi, multi.track(a_first).unwrap().title), "a2");

    // Source 1's playlist routes to source 1.
    let pb = pack(1, 0);
    let n = multi.playlist_name(pb, &mut nbuf).unwrap();
    assert_eq!(&nbuf[..n], b"B-list");
    let b_first = multi.playlist_track(pb, 0).unwrap();
    assert_eq!(unpack(b_first).0, 1);
    assert_eq!(resolve(&multi, multi.track(b_first).unwrap().title), "b1");
}

/// `art_id` is a per-source ref (into that source's `art.blob`): a non-zero
/// `art_id` must be re-packed with its owning source (like every other id/off),
/// while the 0 sentinel ("no art") must stay 0 rather than being packed into a
/// bogus non-zero id.
#[test]
fn art_id_source_routes_when_nonzero_stays_zero_when_zero() {
    let ms = dataset(); // source 0 = plain (art_id 0); source 1 = covered (art_id 7)
    // Member 0: baked via write_index (art_id 0 everywhere).
    let m0 = boxed_member(&to_input(&ms[0]));
    // Member 1: bake with a forced non-zero art_id on its album(s).
    let input1 = to_input(&ms[1]);
    let album_art_id = vec![7u32; input1.albums.len()];
    let bytes1: &'static [u8] = Vec::leak(soul_audio_index::build::write_index_with_art(
        &input1,
        &album_art_id,
    ));
    let m1: Box<dyn Catalog + Send> = Box::new(IndexReader::new(SliceSource::new(bytes1)));
    let multi = MultiCatalog::new(vec![m0, m1]);

    // Source 1's album + its tracks carry art_id packed with source 1.
    let album1 = multi.album(pack(1, 0)).unwrap();
    assert_eq!(unpack(album1.art_id), (1, 7));
    let t1 = multi.track(pack(1, 0)).unwrap();
    assert_eq!(unpack(t1.art_id), (1, 7));

    // Source 0 has no art → art_id stays 0 (not packed into a bogus source).
    let t0 = multi.track(pack(0, 0)).unwrap();
    assert_eq!(t0.art_id, 0);
    // extended_off is always 0 today → stays 0 on both.
    assert_eq!(t1.extended_off, 0);
}
