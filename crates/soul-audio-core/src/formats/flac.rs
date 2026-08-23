//! FLAC decoder — **stub**.

use embedded_io::Read;

use crate::decoder::DecodeError;

/// Stub FLAC decoder. See [`crate::formats::wav::WavDecoder`] for the pattern to
/// follow when implementing a real decoder.
pub struct FlacDecoder;

impl FlacDecoder {
    /// Construct a decoder from a byte source.
    ///
    /// # Errors
    /// Currently always returns [`DecodeError::Unsupported`] — not yet
    /// implemented.
    pub fn new<R: Read>(_reader: R) -> Result<Self, DecodeError> {
        Err(DecodeError::Unsupported)
    }
}
