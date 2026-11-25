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
//! Layers: [`Template`] (pure rendering, incl. the multi-disc tokens and the
//! [`presets`]), [`plan`]/[`plan_with`] (album-aware planning: groups inputs into
//! albums with the importer's identity rule, plans each album's side files —
//! logs, cues, covers, scans, lyrics — and never mutates), and [`apply`] (the
//! only mutating call: copy/move, never overwrite, prune emptied source folders
//! after a Move).
#![forbid(unsafe_code)]
#![deny(unconditional_recursion)]

mod album;
mod apply;
mod paths;
mod plan;
mod sanitize;
mod side;
mod tags;
mod template;

pub use apply::{Progress, Summary, apply};
pub use plan::{
    Action, EntryKind, LeftInPlace, OrganizePlan, PlanEntry, PlanOptions, SideFilesLeft, plan,
    plan_with,
};
pub use template::{Template, preset, presets};

/// Whether organizing a file **copies** it to the destination (leaving the
/// original in place) or **moves** it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// Copy the file to the destination, keeping the source.
    Copy,
    /// Move the file to the destination, removing the source.
    Move,
}
