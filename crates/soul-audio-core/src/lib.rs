#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! `soul-audio-core` — a strict `no_std`, no-alloc audio decode core.
//!
//! Decoders read bytes through the [`Source`] capability ladder ([`Source`] →
//! [`BufSource`] → [`SeekSource`]) and write PCM samples into caller-provided
//! buffers. No allocation happens anywhere in this crate.

pub mod decoder;
pub mod demux;
pub mod dispatch;
pub mod dsd;
pub mod formats;
pub mod io;
pub mod model;
pub mod probe;

pub use decoder::{DecodeError, Decoder, DsdDecoder, DsdSeek};
pub use demux::{Demuxer, FlacDemuxer, Mp3Demuxer, Packet, WavDemuxer};
pub use dispatch::{AnyDecoder, open};
pub use dsd::{DecimateAdapter, DopAdapter};
pub use io::{BufReader, BufSource, SeekSource, SliceSource, Source};
pub use model::{DsdMode, DsdRate, Format, SampleFormat, StreamInfo};
pub use probe::probe;
