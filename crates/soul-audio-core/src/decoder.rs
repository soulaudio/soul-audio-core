//! The decode boundary: [`Decoder`], [`StreamInfo`], [`DecodeError`].

/// Format-independent description of a decoded PCM stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamInfo {
    /// Samples per second (per channel), e.g. 44_100.
    pub sample_rate: u32,
    /// Number of interleaved channels.
    pub channels: u16,
    /// Source bit depth (samples are widened into `i32`).
    pub bits_per_sample: u16,
}

/// Errors a decoder can report. `#[non_exhaustive]` so new variants stay
/// non-breaking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeError {
    /// The stream ended before a complete unit could be read.
    UnexpectedEof,
    /// The stream is not valid for the claimed format.
    Malformed,
    /// A valid but unsupported variant (e.g. a format still stubbed out).
    Unsupported,
    /// The underlying reader failed.
    Io,
}

/// A streaming audio decoder producing interleaved `i32` PCM samples into a
/// caller-provided buffer. Implementors allocate nothing.
pub trait Decoder {
    /// Stream parameters, known after construction.
    fn stream_info(&self) -> StreamInfo;

    /// Decode up to `out.len()` interleaved samples into `out`.
    ///
    /// Returns the number of samples written; `Ok(0)` means end of stream.
    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError>;
}
