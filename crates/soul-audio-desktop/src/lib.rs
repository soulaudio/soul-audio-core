//! Shared **std desktop-playback** building blocks for `soul-audio`, extracted
//! from the CLI so the desktop app (Plan 3) and the CLI share one
//! implementation: concrete file sources, the owned decoder + `open_track`, and
//! (behind the `playback` feature) the cpal sink. Pure canonical-24-bit →
//! device-format conversion lives in `soul_audio_core::convert`; the sink calls
//! it, so a future DMA sink shares the same math.
#![forbid(unsafe_code)]

pub mod decoder;
#[cfg(feature = "playback")]
pub mod sink;
pub mod sources;

pub use decoder::{OwnedDecoder, Src, open_track, vorbis_arena};
#[cfg(feature = "playback")]
pub use sink::CpalSink;
pub use sources::{file_source, tcp_source};
