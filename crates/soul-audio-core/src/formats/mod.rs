//! Format decoders.
//!
//! Every format here is a complete, from-scratch decoder (no C bindings,
//! `no_std` + no-alloc). [`probe`](crate::probe) identifies the container and
//! [`open`](crate::open) dispatches to the right one via
//! [`AnyDecoder`](crate::dispatch::AnyDecoder).
//!
//! - PCM formats implement [`crate::Decoder`]: [`wav`], [`aiff`] (AIFF/AIFF-C),
//!   [`flac`] (incl. FLAC-in-Ogg), [`mp3`] (Layers I/II/III), [`aac`] (AAC-LC,
//!   HE-AAC/SBR, HE-AACv2/PS), [`alac`], [`vorbis`], [`opus`].
//! - DSD containers ([`dsf`], [`dff`]) implement [`crate::DsdDecoder`]; [`dst`]
//!   is the lossless DSD-stream decompressor they feed through.

pub mod aac;
pub mod aiff;
pub mod alac;
pub mod dff;
pub mod dsf;
pub mod dst;
pub mod flac;
pub mod mp3;
pub mod mp3_codec;
pub mod mp3_l12;
pub mod opus;
pub mod vorbis;
pub mod wav;

/// Maximum DSD channels for the DSF planar reader (up to 6 — SACD's multichannel
/// maximum). The `channels × 4096`-byte planar block-row buffer is sized from
/// this (6 × 4096 = 24 KiB); lower it to reduce footprint. The DoP and decimation
/// adapters carry their own (independent) 8-channel caps.
pub(crate) const MAX_DSD_CHANNELS: usize = 6;
