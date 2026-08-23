#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! `soul-audio-core` — a strict `no_std`, no-alloc audio decode core.
//!
//! Decoders read from any [`embedded_io::Read`] source and write PCM samples
//! into caller-provided buffers. No allocation happens anywhere in this crate.

pub mod decoder;
pub mod formats;
pub mod io;

pub use decoder::{DecodeError, Decoder, StreamInfo};
pub use io::SliceReader;
