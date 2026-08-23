//! WAV (RIFF/WAVE) decoder — **stub**.

use embedded_io::Read;

use crate::decoder::DecodeError;

/// Stub WAV decoder.
///
/// Replace this with real RIFF/WAVE parsing: read the header in [`Self::new`],
/// store the reader + parsed `StreamInfo` + remaining byte count as fields, and
/// implement [`crate::Decoder`] to stream PCM samples into caller buffers.
pub struct WavDecoder;

impl WavDecoder {
    /// Construct a decoder from a byte source.
    ///
    /// # Errors
    /// Currently always returns [`DecodeError::Unsupported`] — not yet
    /// implemented.
    pub fn new<R: Read>(_reader: R) -> Result<Self, DecodeError> {
        // TODO: parse the RIFF/WAVE header, capture StreamInfo, then implement
        // `Decoder` for this type.
        Err(DecodeError::Unsupported)
    }
}
