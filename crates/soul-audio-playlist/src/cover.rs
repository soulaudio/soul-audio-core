//! Pure cover-selection logic for a playlist's thumbnail: a 2x2 mosaic of the
//! first 4 distinct-album covers, a single cover for 1-3 distinct albums, or
//! a placeholder when none of the leading entries have art.
//!
//! This module never touches the filesystem or the live library itself; a
//! caller resolves each entry's locator to a volatile track id and an
//! art-presence check, then hands both in here.

use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;
use core::hash::{Hash, Hasher};

use crate::model::Playlist;

/// The thumbnail a playlist list row renders: a user-set custom cover image
/// (always wins when present), four distinct-album covers arranged as a 2x2
/// mosaic, one cover when fewer than 4 distinct albums have art, or a
/// placeholder when none do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaylistCover {
    Mosaic(Vec<u32>),
    Single(u32),
    Placeholder,
    Custom(String),
}

/// Walks `items` in order, collecting the index of each item that is both
/// `is_eligible` and the first-seen for its `album_key`, stopping once
/// `limit` indices have been collected.
///
/// This is the one "what counts as distinct" rule shared by [`select_cover`]
/// and a host's manifest-cover walk (every entry is eligible; only the album
/// dedupe applies).
pub fn first_distinct_album_indices<T>(
    items: &[T],
    limit: usize,
    is_eligible: impl Fn(&T) -> bool,
    album_key: impl Fn(&T) -> &str,
) -> Vec<usize> {
    // A `BTreeSet` (rather than a hash set) for dedup only -- no ordering is
    // read from it, so its lack of true-random-access iteration order is
    // irrelevant; it just needs `Ord`, which `&str` has.
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut out = Vec::new();
    for (i, item) in items.iter().enumerate() {
        if out.len() >= limit {
            break;
        }
        if !is_eligible(item) {
            continue;
        }
        if seen.insert(album_key(item)) {
            out.push(i);
        }
    }
    out
}

/// Pick up to 4 distinct-album cover track ids from resolved entries, in
/// order. `resolved`: `(track_id, album_key)` in playlist order. `has_art`:
/// does this id have cover art.
#[must_use]
pub fn select_cover(resolved: &[(u32, String)], has_art: impl Fn(u32) -> bool) -> PlaylistCover {
    let indices = first_distinct_album_indices(
        resolved,
        4,
        |(id, _)| has_art(*id),
        |(_, album)| album.as_str(),
    );
    let ids: Vec<u32> = indices.into_iter().map(|i| resolved[i].0).collect();
    match ids.len() {
        0 => PlaylistCover::Placeholder,
        1..=3 => PlaylistCover::Single(ids[0]),
        _ => PlaylistCover::Mosaic(ids),
    }
}

/// The full cover-selection precedence for a playlist row: a user-set custom
/// cover image always wins, even over what would otherwise be a mosaic;
/// absent that, fall back to [`select_cover`]'s auto mosaic/single/
/// placeholder pick. Pure (no filesystem or runtime access), so the
/// precedence itself is unit-tested on its own.
#[must_use]
pub fn choose_playlist_cover(
    custom: Option<String>,
    resolved: &[(u32, String)],
    has_art: impl Fn(u32) -> bool,
) -> PlaylistCover {
    match custom {
        Some(path) => PlaylistCover::Custom(path),
        None => select_cover(resolved, has_art),
    }
}

/// Deterministic `no_std` stand-in for `std`'s `DefaultHasher` (std-only,
/// unavailable here): a plain FNV-1a over the written bytes. [`entries_fingerprint`]
/// uses this only as a per-run memo key, never persisted or compared across
/// runs, so FNV-1a's weaker distribution than SipHash is irrelevant.
struct Fnv1a(u64);

impl Fnv1a {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn new() -> Self {
        Self(Self::OFFSET_BASIS)
    }
}

impl Hasher for Fnv1a {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// Stable fingerprint of a playlist's entry list (locator identity and
/// order only), for memoizing the cover decision: a caller recomputes
/// [`select_cover`] only when this changes, not on every render.
///
/// Deterministic within a single run (that is all memoization needs); it is
/// not meant to be stored or compared across process runs.
#[must_use]
pub fn entries_fingerprint(p: &Playlist) -> u64 {
    let mut hasher = Fnv1a::new();
    for entry in &p.entries {
        entry.locator.key().hash(&mut hasher);
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::locator::{TrackLocator, TrackSnapshot};
    use crate::model::PlaylistEntry;

    fn ids_albums(pairs: &[(u32, &str)]) -> Vec<(u32, String)> {
        pairs
            .iter()
            .map(|(id, album)| (*id, (*album).into()))
            .collect()
    }

    #[test]
    fn four_distinct_albums_make_a_mosaic() {
        let cov = select_cover(
            &ids_albums(&[(1, "A"), (2, "B"), (3, "C"), (4, "D"), (5, "E")]),
            |_| true,
        );
        assert_eq!(cov, PlaylistCover::Mosaic(alloc::vec![1, 2, 3, 4]));
    }

    #[test]
    fn duplicate_albums_are_skipped() {
        let cov = select_cover(&ids_albums(&[(1, "A"), (2, "A"), (3, "B")]), |_| true);
        assert_eq!(cov, PlaylistCover::Single(1)); // only 2 distinct -> single
    }

    #[test]
    fn one_to_three_distinct_is_single_cover() {
        let cov = select_cover(&ids_albums(&[(7, "A"), (8, "B"), (9, "C")]), |_| true);
        assert_eq!(cov, PlaylistCover::Single(7));
    }

    #[test]
    fn no_art_is_placeholder() {
        let cov = select_cover(&ids_albums(&[(1, "A"), (2, "B")]), |_| false);
        assert_eq!(cov, PlaylistCover::Placeholder);
    }

    #[test]
    fn custom_cover_takes_precedence_over_mosaic() {
        // Four distinct albums with art would normally mosaic, but a custom
        // cover always wins.
        let resolved = ids_albums(&[(1, "A"), (2, "B"), (3, "C"), (4, "D")]);
        let cov = choose_playlist_cover(Some("/x/cover.png".into()), &resolved, |_| true);
        assert!(matches!(cov, PlaylistCover::Custom(_)));
    }

    #[test]
    fn falls_back_to_mosaic_without_custom() {
        let resolved = ids_albums(&[(1, "A"), (2, "B"), (3, "C"), (4, "D")]);
        let cov = choose_playlist_cover(None, &resolved, |_| true);
        assert!(matches!(cov, PlaylistCover::Mosaic(_)));
    }

    #[test]
    fn art_gate_is_checked_before_counting_distinct_albums() {
        // Only ids 2 and 4 have art; their albums (B, D) are the only ones
        // that should count, even though 1/3/5 pass by first.
        let cov = select_cover(
            &ids_albums(&[(1, "A"), (2, "B"), (3, "C"), (4, "D"), (5, "E")]),
            |id| id % 2 == 0,
        );
        assert_eq!(cov, PlaylistCover::Single(2));
    }

    fn entry(title: &str, album: &str) -> PlaylistEntry {
        PlaylistEntry {
            locator: TrackLocator::Local {
                path: alloc::format!("D:/music/{album}/{title}.flac"),
            },
            snapshot: TrackSnapshot {
                title: title.into(),
                artist: "Artist".into(),
                album: album.into(),
                duration_secs: 180,
                track_no: None,
                disc_no: None,
            },
            mbid: None,
        }
    }

    fn playlist(entries: Vec<PlaylistEntry>) -> Playlist {
        Playlist {
            id: 1,
            name: "test".into(),
            custom_cover: None,
            entries,
        }
    }

    #[test]
    fn fingerprint_is_stable_for_the_same_entries() {
        let p = playlist(alloc::vec![entry("a", "A"), entry("b", "B")]);
        assert_eq!(entries_fingerprint(&p), entries_fingerprint(&p));
    }

    #[test]
    fn fingerprint_changes_when_order_changes() {
        let mut p = playlist(alloc::vec![entry("a", "A"), entry("b", "B")]);
        let before = entries_fingerprint(&p);
        assert!(p.move_entry(0, 1));
        assert_ne!(entries_fingerprint(&p), before);
    }

    #[test]
    fn fingerprint_changes_when_membership_changes() {
        let p1 = playlist(alloc::vec![entry("a", "A")]);
        let p2 = playlist(alloc::vec![entry("a", "A"), entry("b", "B")]);
        assert_ne!(entries_fingerprint(&p1), entries_fingerprint(&p2));
    }
}
