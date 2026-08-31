//! Headless desktop-player backend service: the command/event/DTO types, a
//! `lofty` library scan, the app-command → `soul_audio_control::Player`
//! translation + state projection, and a threaded `AudioPlayer` actor. Owns all
//! backend logic; the Tauri shell (Plan 4) is thin plumbing over this crate.
//!
//! Sink-generic: the actor builds the sink on the pump thread, so a `!Send`
//! device sink works, and this crate stays free of `cpal` (Plan 4 injects it).
#![forbid(unsafe_code)]

pub mod types;

#[cfg(test)]
pub(crate) mod testutil;

pub use types::{Command, Event, PlayState, QueueState, RepeatMode, Tick, TrackView};
