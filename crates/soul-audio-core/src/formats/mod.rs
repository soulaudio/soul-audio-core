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
pub mod wav;

/// Maximum DSD channels supported by the no-alloc DSD readers (stereo). The
/// planar-transpose buffer is sized from this; multichannel DSD is a follow-up.
pub(crate) const MAX_DSD_CHANNELS: usize = 2;
