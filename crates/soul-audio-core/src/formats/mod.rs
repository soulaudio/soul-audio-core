//! Format decoders.
//!
//! Every format is a **stub** in this slice — each `new` returns
//! [`crate::DecodeError::Unsupported`]. They implement the decode seam so the
//! wiring ([`probe`](crate::probe) → [`AnyDecoder`](crate::dispatch::AnyDecoder))
//! is real and exercised; the container parsing and codec logic are the next
//! slice.
//!
//! - PCM formats ([`wav`], [`flac`], [`mp3`]) implement [`crate::Decoder`].
//! - DSD containers ([`dsf`], [`dff`]) implement [`crate::DsdDecoder`].

pub mod dff;
pub mod dsf;
pub mod dst;
pub mod flac;
pub mod mp3;
pub mod mp3_codec;
pub mod mp3_l12;
pub mod wav;

/// Maximum DSD channels for the DSF planar reader (up to 6 — SACD's multichannel
/// maximum). The `channels × 4096`-byte planar block-row buffer is sized from
/// this (6 × 4096 = 24 KiB); lower it to reduce footprint. The DoP and decimation
/// adapters carry their own (independent) 8-channel caps.
pub(crate) const MAX_DSD_CHANNELS: usize = 6;
