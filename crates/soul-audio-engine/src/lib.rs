#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! `soul-audio-engine` — the portable playback queue on top of
//! [`soul_audio_core::Deck`]. The [`Engine`] drives a [`Deck`] from a
//! [`Sequencer`] policy (which track plays next); it does no I/O.

mod engine;
mod linear;
mod sequencer;

pub use engine::{Engine, Provided};
pub use linear::{LinearSequencer, RepeatMode};
pub use sequencer::{Advance, Sequencer};
