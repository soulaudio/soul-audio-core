//! The playlist data model: pure, in-memory, serde-friendly (behind the
//! optional `serde` feature). No filesystem or host I/O here (that is a
//! caller's on-disk store); this just holds the structure a playlist is
//! edited through (reorder, remove) and the `slugify` helper a folder
//! builder can use for a legible on-disk name.

use alloc::string::String;
use alloc::vec::Vec;

use crate::locator::{TrackLocator, TrackSnapshot};

/// A playlist's stable identity. Never reused across playlists, even after
/// one is deleted (so a stale reference, e.g. from a crashed write, can't
/// silently resolve to a different playlist).
pub type PlaylistId = u64;

/// One track in a playlist: the durable [`TrackLocator`] it resolves through
/// plus the [`TrackSnapshot`] captured when it was added (so the row renders
/// even if the source is unreachable or has since been retagged), and an
/// optional MusicBrainz id for future enrichment matching.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PlaylistEntry {
    pub locator: TrackLocator,
    pub snapshot: TrackSnapshot,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub mbid: Option<String>,
}

/// A user-created, ordered collection of tracks.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Playlist {
    pub id: PlaylistId,
    pub name: String,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub custom_cover: Option<String>,
    #[cfg_attr(feature = "serde", serde(default))]
    pub entries: Vec<PlaylistEntry>,
}

impl Playlist {
    /// Moves the entry at `from` to land at index `to`, shifting entries
    /// between the two positions. Both indices are validated against the
    /// current length; if either is out of range this is a no-op and
    /// returns `false`.
    pub fn move_entry(&mut self, from: usize, to: usize) -> bool {
        let len = self.entries.len();
        if from >= len || to >= len {
            return false;
        }
        let entry = self.entries.remove(from);
        self.entries.insert(to, entry);
        true
    }

    /// Removes and returns the entry at `index`, or `None` if `index` is out
    /// of range.
    pub fn remove_entry(&mut self, index: usize) -> Option<PlaylistEntry> {
        if index >= self.entries.len() {
            return None;
        }
        Some(self.entries.remove(index))
    }
}

/// Builds a filesystem-safe, legible slug from a display name: lowercased,
/// with every run of characters outside `[a-z0-9]` collapsed to a single
/// `-`, leading/trailing `-` trimmed, and the result capped at 40 bytes
/// (trimming a trailing `-` that truncation may have exposed). Returns `""`
/// if nothing legible remains (e.g. an all-whitespace or all-punctuation
/// name).
///
/// This is never the playlist's identity (that is [`PlaylistId`]); it is
/// only ever a human-legible on-disk name, so collisions and reserved
/// Windows device names (`CON`, `NUL`, ...) are handled by a caller's folder
/// builder, not here.
#[must_use]
pub fn slugify(name: &str) -> String {
    let lowered = name.to_lowercase();
    let mut slug = String::with_capacity(lowered.len());
    let mut pending_dash = false;
    for ch in lowered.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(ch);
        } else {
            pending_dash = true;
        }
    }
    if slug.len() > 40 {
        slug.truncate(40);
        while slug.ends_with('-') {
            slug.pop();
        }
    }
    slug
}

#[cfg(test)]
mod tests {
    use alloc::format;

    use super::*;

    fn pl(titles: &[&str]) -> Playlist {
        Playlist {
            id: 1,
            name: "test".into(),
            custom_cover: None,
            entries: titles
                .iter()
                .map(|title| PlaylistEntry {
                    locator: TrackLocator::Local {
                        path: format!("D:/music/{title}.flac"),
                    },
                    snapshot: TrackSnapshot {
                        title: (*title).into(),
                        artist: "Artist".into(),
                        album: "Album".into(),
                        duration_secs: 180,
                        track_no: None,
                        disc_no: None,
                    },
                    mbid: None,
                })
                .collect(),
        }
    }

    fn titles(p: &Playlist) -> Vec<&str> {
        p.entries
            .iter()
            .map(|e| e.snapshot.title.as_str())
            .collect()
    }

    #[test]
    fn slugify_sanitizes_illegal_and_unicode() {
        assert_eq!(slugify("Chill / Vibes: 2024!"), "chill-vibes-2024");
        assert_eq!(slugify("   "), "");
        assert_eq!(slugify("CON"), "con"); // reserved-name handling lives in a caller's folder builder, not here
        assert!(slugify(&"x".repeat(100)).len() <= 40);
    }

    #[test]
    fn move_entry_reorders() {
        let mut p = pl(&["a", "b", "c"]);
        assert!(p.move_entry(0, 2));
        assert_eq!(titles(&p), ["b", "c", "a"]);
        assert!(!p.move_entry(9, 0));
    }

    #[test]
    fn remove_entry_pulls_one() {
        let mut p = pl(&["a", "b", "c"]);
        assert_eq!(p.remove_entry(1).unwrap().snapshot.title, "b");
        assert_eq!(titles(&p), ["a", "c"]);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn playlist_json_roundtrips() {
        let p = pl(&["a", "b"]);
        let json = serde_json::to_string(&p).unwrap();
        let back: Playlist = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }
}
