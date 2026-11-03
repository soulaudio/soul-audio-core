#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! `soul-audio-control` — a portable `no_std` playback controller. [`Player`]
//! owns a [`soul_audio_engine::Engine`] over a
//! [`soul_audio_queue::TwoLevelQueue`] and turns high-level `Command` intents
//! (transport + queue + the prev-3-second-restart policy) into engine/queue
//! calls. The desktop app and the future DAP firmware share it.

mod command;
mod player;

pub use command::Command;
pub use player::Player;
