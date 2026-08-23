//! Format decoders.
//!
//! Every format is a **stub** in this template — each `new` returns
//! [`crate::DecodeError::Unsupported`]. To implement one: give the decoder the
//! fields it needs, parse the container in `new`, and implement
//! [`crate::Decoder`] to stream PCM into caller-provided buffers. No allocation.

pub mod dsd;
pub mod flac;
pub mod mp3;
pub mod wav;
