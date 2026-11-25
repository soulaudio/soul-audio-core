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
//! albums with the importer's identity rule, plans each album's side files
//! (logs, cues, covers, scans, lyrics), points a cue's `FILE` lines at the
//! renamed tracks, and never mutates), and [`apply`] (the only mutating call:
//! copy/move, never overwrite, prune emptied source folders after a Move).
//!
//! Paths are compared host-aware: a Windows verbatim prefix (`\\?\…`) is
//! ignored and, on Windows/macOS, letter case is folded — for the source root
//! vs. the inputs, and for every destination collision check (so `Abc` and
//! `ABC` album folders, or `Cover.jpg` and `cover.jpg`, never land on one
//! file). On Linux paths compare as-is.
//!
//! # Known limitations
//!
//! - **Interrupted Move.** Audio entries are applied before side files. If a
//!   Move stops between the two, the album's side files stay orphaned in the
//!   source album root: a fresh plan over the (now moved) audio no longer sees
//!   that root. Re-apply the **same** plan (idempotent — finished entries are
//!   skipped, emptied folders still pruned; a cue written but not yet removed
//!   from the source is finished). Do **not** re-plan instead: the tracks
//!   already moved are no longer inputs, so a fresh plan cannot point a cue's
//!   `FILE` lines at them — the cue would come out half-rewritten, naming
//!   source files that are gone.
//! - **Check-then-rename race.** [`apply`] checks that a destination does not
//!   exist and then copies/renames onto it; a file created at that path by
//!   another process in between may be overwritten (a rename replaces on every
//!   platform). Do not organize into a folder something else is writing to.
//! - **Disc folders named after the album.** Only folders whose name *starts*
//!   with `CD`/`Disc`/`Disk`/`DVD`/`Side` + a number are disc folders
//!   (`soul_audio_tags::disc_folder`). Sibling folders like `Album CD1/` and
//!   `Album CD2/` are two album roots — two albums (the second gets an
//!   `Album (2)` folder) — in the organizer and the importer alike. Tag the
//!   discs (`DISCNUMBER`), or rename the folders to `Album/CD1`, `Album/CD2`.
//! - A plan reports, but cannot fix, a multi-disc album whose disc numbers come
//!   only from folder names when the template has no `{DiscDir}` directory
//!   segment ([`OrganizePlan::folder_inferred_discs`]).
//! - **Cue sheets left behind.** A `.cue` is rewritten only when its album's
//!   side files move with the album. One left in place
//!   ([`OrganizePlan::side_files_left`]) keeps naming the old paths, and one
//!   that cannot be rewritten safely (legacy encoding with non-ASCII names) is
//!   moved unchanged and listed in [`OrganizePlan::cues_not_rewritten`].
//! - **Unicode normalization.** Names are compared as stored, never
//!   normalized: on macOS a cue's `FILE "Café.flac"` written in NFC does not
//!   match a file name stored in NFD (the line is kept as-is).
//! - **Legacy cues that happen to be valid UTF-8** are treated as UTF-8 (a
//!   Windows-1252 cue whose bytes form valid UTF-8 is indistinguishable).
//! - **Partial selections.** Selecting only some tracks of an album (or an
//!   album folder holding audio this build never lists as an input) moves its
//!   tracks but leaves all its side files in place
//!   ([`SideFilesLeft::PartialSelection`]).
#![forbid(unsafe_code)]
#![deny(unconditional_recursion)]

mod album;
mod apply;
mod cue;
mod paths;
mod plan;
mod sanitize;
mod side;
mod tags;
mod template;

pub use apply::{Progress, Summary, apply};
pub use cue::CueNotRewritten;
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
