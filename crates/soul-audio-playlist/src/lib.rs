//! Portable **playlist** model, shared by the desktop host (`soul-player-ng`)
//! and the DAP firmware (`soul-listener`).
//!
//! A [`TrackLocator`] durably references a track (a local path, or a remote
//! server + relative path) independent of any catalog's volatile internal
//! id, which can shift on rescan or re-aggregation. A [`Playlist`] is an
//! ordered list of [`PlaylistEntry`] (a locator plus a point-in-time
//! [`TrackSnapshot`]), with pure reorder/remove editing and a `slugify`
//! helper for a legible on-disk name. [`select_cover`]/[`choose_playlist_cover`]
//! pick a playlist's thumbnail (mosaic/single/placeholder/custom) from
//! caller-resolved entries; this crate never touches the filesystem or a live
//! library itself.
//!
//! This crate is `no_std` + `alloc`. The optional `serde` feature derives
//! (de)serialization for the locator/snapshot/playlist types (with `alloc`,
//! no `std`), so a host can persist a playlist (e.g. to TOML) and the DAP can
//! read it back.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(unconditional_recursion)]

extern crate alloc;

mod cover;
mod locator;
mod model;

pub use cover::{
    PlaylistCover, choose_playlist_cover, entries_fingerprint, first_distinct_album_indices,
    select_cover,
};
pub use locator::{TrackLocator, TrackSnapshot};
pub use model::{Playlist, PlaylistEntry, PlaylistId, slugify};
