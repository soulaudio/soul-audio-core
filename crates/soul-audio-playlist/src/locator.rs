//! A durable reference to a track: where it lives, independent of any
//! catalog's volatile internal id (which can shift on rescan/re-aggregation).
//! Playlists store these (plus a [`TrackSnapshot`]) rather than a track id, so
//! a playlist entry survives a rescan, a library edit, or the track moving
//! between catalogs.

use alloc::format;
use alloc::string::String;

/// A durable reference to a track: where it lives, independent of any
/// catalog's volatile internal id (which can shift on rescan/re-aggregation).
/// Playlists store these (plus a [`TrackSnapshot`]) rather than a track id, so
/// a playlist entry survives a rescan, a library edit, or the track moving
/// between catalogs.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "kind", rename_all = "lowercase"))]
pub enum TrackLocator {
    /// A file on local disk, by absolute path (normalized, not necessarily
    /// canonicalized).
    Local { path: String },
    /// A file on a remote `soul-server`, by the server's base URL and the
    /// track's path relative to that server's root.
    Remote { server: String, rel: String },
}

impl TrackLocator {
    /// Normalized comparison key: backslashes become forward slashes and the
    /// whole string is lowercased, unconditionally (not gated on OS), so two
    /// locators that name the same file under different casing/separator
    /// conventions compare equal regardless of which platform produced them.
    #[must_use]
    pub fn key(&self) -> String {
        let raw = match self {
            TrackLocator::Local { path } => path.clone(),
            TrackLocator::Remote { server, rel } => format!("{server}/{rel}"),
        };
        raw.replace('\\', "/").to_lowercase()
    }
}

/// A point-in-time metadata snapshot of a track, carried alongside its
/// [`TrackLocator`] in a playlist entry: enough to render the row (title,
/// artist, album, duration, track/disc number) even if the source file is
/// temporarily unreachable or has since been retagged.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TrackSnapshot {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_secs: u32,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub track_no: Option<u32>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub disc_no: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_key_is_case_and_separator_normalized() {
        let a = TrackLocator::Local {
            path: r"D:\Music\A.flac".into(),
        };
        let b = TrackLocator::Local {
            path: "d:/music/a.flac".into(),
        };
        assert_eq!(a.key(), b.key());
    }

    #[test]
    fn remote_key_joins_server_and_rel() {
        let loc = TrackLocator::Remote {
            server: "https://h".into(),
            rel: "x/y.flac".into(),
        };
        assert_eq!(loc.key(), "https://h/x/y.flac");
    }

    // The serde round-trip only needs to prove the derives work, not any
    // particular wire format; `serde_json` is the crate's one dev-dependency
    // (`toml` is not `no_std`, so it stays out of this crate's surface).
    #[cfg(feature = "serde")]
    #[test]
    fn locator_serde_roundtrips_both_variants() {
        #[derive(serde::Serialize, serde::Deserialize)]
        struct Wrap {
            loc: TrackLocator,
        }

        let local = TrackLocator::Local {
            path: "D:/Music/a.flac".into(),
        };
        let remote = TrackLocator::Remote {
            server: "https://h".into(),
            rel: "x/y.flac".into(),
        };
        for loc in [local, remote] {
            let s = serde_json::to_string(&Wrap { loc: loc.clone() }).unwrap();
            let back: Wrap = serde_json::from_str(&s).unwrap();
            assert_eq!(back.loc, loc);
        }
    }

    #[cfg(feature = "serde")]
    #[test]
    fn snapshot_serde_roundtrips() {
        let snap = TrackSnapshot {
            title: "Title".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            duration_secs: 180,
            track_no: Some(3),
            disc_no: None,
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert_eq!(serde_json::from_str::<TrackSnapshot>(&json).unwrap(), snap);
    }
}
