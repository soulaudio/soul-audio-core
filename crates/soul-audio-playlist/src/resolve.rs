//! Resolving a playlist's durable entries against a live catalog into the
//! view-model both heads render from.
//!
//! A [`PlaylistEntry`]'s [`TrackLocator`](crate::TrackLocator) is durable, but a
//! catalog's internal track ids are not (a rescan renumbers them), so a playlist
//! must be *resolved* against the current catalog before it is shown or played:
//! which entries still point at a present track, the ordered list of those
//! available ids (the "play all" / click-to-play context), and the total duration.
//!
//! [`ResolvedPlaylist::resolve`] does this by STREAMING the catalog once, holding
//! only the playlist's own locator keys. Memory is O(playlist), never O(catalog),
//! and nothing from the stream is retained past the single pass, so the same
//! `no_std` + `alloc` code runs on the desktop and on a RAM-limited DAP streaming
//! its SD-card `.soulidx` straight through.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::model::PlaylistEntry;

/// A playlist's entries resolved against a catalog: the view-model the desktop
/// detail page and the DAP both render from, meant to be computed **once** per
/// (playlist, catalog) change and then indexed, never rebuilt per frame.
///
/// - `resolved[i]` is entry `i`'s current id, or `None` when it is unavailable
///   (moved / deleted / offline) and must render greyed from its snapshot.
/// - `play_ids` is the available context in playlist order — what "Play all"
///   enqueues and what a row's click-to-play resumes into — with the unavailable
///   entries skipped. A track repeated in the playlist appears once per occurrence.
/// - `play_index_of[i]` maps entry `i` to its slot in `play_ids` (`None` when the
///   entry is unavailable), so a row knows where in the context it plays.
/// - `total_secs` sums each entry's **live** duration where it resolves, falling
///   back to the entry snapshot's captured duration otherwise, so the header total
///   stays stable even while some tracks are unavailable.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResolvedPlaylist {
    /// Per-entry current id, `None` = unavailable. Same length/order as the entries.
    pub resolved: Vec<Option<u32>>,
    /// The available track ids, in playlist order (the play/enqueue context).
    pub play_ids: Vec<u32>,
    /// Entry index -> its slot in [`play_ids`](Self::play_ids), `None` if unavailable.
    pub play_index_of: Vec<Option<usize>>,
    /// Total duration in seconds (live where resolved, else the snapshot's).
    pub total_secs: f64,
}

impl ResolvedPlaylist {
    /// Resolve `entries` by streaming the catalog ONCE. `tracks` yields every catalog
    /// track's `(locator_key, id, duration_secs)`, where `locator_key` is
    /// [`TrackLocator::key`](crate::TrackLocator::key) for the track's current locator
    /// and `duration_secs` its tag length (`None` if unknown).
    ///
    /// Memory is **O(playlist)**, not O(catalog): only the playlist's own locator
    /// keys are held (so a RAM-limited DAP can stream a huge SD-card index straight
    /// through), and nothing from the stream is retained past the pass, which stops
    /// early once every entry has resolved.
    #[must_use]
    pub fn resolve<K, I>(entries: &[PlaylistEntry], tracks: I) -> Self
    where
        K: AsRef<str>,
        I: IntoIterator<Item = (K, u32, Option<f64>)>,
    {
        let n = entries.len();
        // Each wanted locator key -> the entry indices that want it (a track can sit
        // in a playlist more than once). This is the ONLY O(playlist) allocation;
        // the catalog itself is never collected.
        let mut wanted: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, e) in entries.iter().enumerate() {
            wanted.entry(e.locator.key()).or_default().push(i);
        }

        let mut resolved: Vec<Option<u32>> = vec![None; n];
        let mut live_dur: Vec<Option<f64>> = vec![None; n];
        let mut remaining = n;
        for (key, id, dur) in tracks {
            if remaining == 0 {
                break; // every entry already resolved; don't scan the rest
            }
            if let Some(idxs) = wanted.get(key.as_ref()) {
                for &i in idxs {
                    if resolved[i].is_none() {
                        remaining -= 1;
                    }
                    resolved[i] = Some(id);
                    live_dur[i] = dur;
                }
            }
        }

        let mut play_ids = Vec::new();
        let mut play_index_of = Vec::with_capacity(n);
        let mut total_secs = 0.0_f64;
        for (i, e) in entries.iter().enumerate() {
            match resolved[i] {
                Some(id) => {
                    play_index_of.push(Some(play_ids.len()));
                    play_ids.push(id);
                    // Live duration where the catalog gave one, else the entry's
                    // captured snapshot duration (stable for a resolved-but-unmeasured
                    // track).
                    total_secs +=
                        live_dur[i].unwrap_or_else(|| f64::from(e.snapshot.duration_secs));
                }
                None => {
                    play_index_of.push(None);
                    total_secs += f64::from(e.snapshot.duration_secs);
                }
            }
        }

        Self {
            resolved,
            play_ids,
            play_index_of,
            total_secs,
        }
    }

    /// The number of entries whose track currently resolves (the playable count).
    #[must_use]
    pub fn available_count(&self) -> usize {
        self.play_ids.len()
    }

    /// Whether entry `i` currently resolves to a playable track.
    #[must_use]
    pub fn is_available(&self, i: usize) -> bool {
        self.play_index_of.get(i).copied().flatten().is_some()
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use alloc::vec;

    use super::*;
    use crate::locator::{TrackLocator, TrackSnapshot};

    /// The catalog-side locator key for a local path, as a head would yield it.
    fn key(path: &str) -> String {
        TrackLocator::Local { path: path.into() }.key()
    }

    fn entry(path: &str, snap_dur: u32) -> PlaylistEntry {
        PlaylistEntry {
            locator: TrackLocator::Local { path: path.into() },
            snapshot: TrackSnapshot {
                title: path.into(),
                artist: String::new(),
                album: String::new(),
                duration_secs: snap_dur,
                track_no: None,
                disc_no: None,
            },
            mbid: None,
        }
    }

    #[test]
    fn resolve_builds_availability_play_context_and_total() {
        let entries = vec![entry("a", 1), entry("b", 7), entry("c", 5)];
        // Catalog stream, in catalog order (independent of the playlist), with an
        // unrelated track mixed in: a -> 10 (100s live), c -> 30 (no live duration).
        let catalog = vec![
            (key("c"), 30u32, None),
            (key("zz"), 99u32, Some(9.0)),
            (key("a"), 10u32, Some(100.0)),
        ];
        let r = ResolvedPlaylist::resolve(&entries, catalog);

        assert_eq!(r.resolved, vec![Some(10), None, Some(30)]);
        assert_eq!(r.play_ids, vec![10, 30]); // "b" (missing) skipped
        assert_eq!(r.play_index_of, vec![Some(0), None, Some(1)]);
        // 100 live (a) + 7 snapshot (b, unavailable) + 5 snapshot (c, no live dur).
        assert_eq!(r.total_secs, 112.0);
        assert_eq!(r.available_count(), 2);
        assert!(r.is_available(0));
        assert!(!r.is_available(1));
        assert!(r.is_available(2));
        assert!(!r.is_available(99));
    }

    #[test]
    fn resolve_of_empty_is_empty() {
        let r = ResolvedPlaylist::resolve(&[], Vec::<(String, u32, Option<f64>)>::new());
        assert!(r.resolved.is_empty());
        assert!(r.play_ids.is_empty());
        assert!(r.play_index_of.is_empty());
        assert_eq!(r.total_secs, 0.0);
        assert_eq!(r.available_count(), 0);
    }

    #[test]
    fn a_track_repeated_in_the_playlist_resolves_every_occurrence() {
        let entries = vec![entry("a", 1), entry("a", 1), entry("b", 2)];
        let catalog = vec![(key("a"), 10u32, Some(50.0))]; // "b" absent
        let r = ResolvedPlaylist::resolve(&entries, catalog);
        assert_eq!(r.resolved, vec![Some(10), Some(10), None]);
        assert_eq!(r.play_ids, vec![10, 10]); // both occurrences play
        assert_eq!(r.play_index_of, vec![Some(0), Some(1), None]);
        assert_eq!(r.total_secs, 50.0 + 50.0 + 2.0);
    }

    #[test]
    fn all_unavailable_totals_from_snapshots() {
        let entries = vec![entry("x", 3), entry("y", 4)];
        let r = ResolvedPlaylist::resolve(&entries, vec![(key("z"), 1u32, Some(9.0))]);
        assert_eq!(r.resolved, vec![None, None]);
        assert!(r.play_ids.is_empty());
        assert_eq!(r.total_secs, 7.0); // 3 + 4 from snapshots
    }
}
