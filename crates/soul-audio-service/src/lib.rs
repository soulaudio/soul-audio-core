//! Headless desktop-player backend service: the command/event/DTO types, a
//! `soul-audio-tags` library scan, the app-command → `soul_audio_control::Player`
//! translation + state projection, and a threaded `AudioPlayer` actor. Owns all
//! backend logic; the Tauri shell (Plan 4) is thin plumbing over this crate.
//!
//! Sink-generic: the actor builds the sink on the pump thread, so a `!Send`
//! device sink works, and this crate stays free of `cpal` (Plan 4 injects it).
#![forbid(unsafe_code)]

pub mod actor;
pub mod art;
pub mod driver;
pub mod library;
pub mod types;

#[cfg(test)]
pub(crate) mod testutil;

pub use actor::AudioPlayer;
pub use art::{Cover, cover_bytes};
pub use driver::{ServicePlayer, apply_command, project_queue, project_tick};
pub use library::{Library, Track};
pub use types::{Command, Event, PlayState, QueueState, RepeatMode, Tick, TrackView};
