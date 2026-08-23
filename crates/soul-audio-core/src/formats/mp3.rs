//! MP3 decoder — **stub**.

use embedded_io::Read;

use crate::decoder::DecodeError;

/// Stub MP3 decoder. See [`crate::formats::wav::WavDecoder`] for the pattern to
/// follow when implementing a real decoder.
pub struct Mp3Decoder;

impl Mp3Decoder {
    /// Construct a decoder from a byte source.
    ///
    /// # Errors
    /// Currently always returns [`DecodeError::Unsupported`] — not yet
    /// implemented.
    pub fn new<R: Read>(_reader: R) -> Result<Self, DecodeError> {
        Err(DecodeError::Unsupported)
    }
}
