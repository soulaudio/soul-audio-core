//! Path-template file organizer (std).
//!
//! Computes and (in a later layer) executes placing audio files into a
//! destination tree described by a path template such as
//! `{AlbumArtist}/{Album}/{TrackNo} - {Title}`. The destination path is derived
//! from a file's tags, with each path segment made safe for the host filesystem
//! (Windows-illegal characters, trailing dots/spaces, and reserved device names
//! are all handled).
//!
//! This crate is the "managed library / file organization" seam reserved in the
//! architecture (`docs/ARCHITECTURE.md` §9). It is a std adapter: the heavy,
//! platform-specific filesystem work lives here, never on the DAP.
//!
//! Task A (this layer) is **pure logic** — filename sanitization, the
//! [`Template`] renderer, and the [`presets`] catalog. Planning a move/copy of
//! real files and applying it (reading tags off disk, collision handling) is the
//! IO layer added on top later.
#![forbid(unsafe_code)]

mod sanitize;
mod template;

pub use template::{Template, presets};

/// Whether organizing a file **copies** it to the destination (leaving the
/// original in place) or **moves** it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// Copy the file to the destination, keeping the source.
    Copy,
    /// Move the file to the destination, removing the source.
    Move,
}
