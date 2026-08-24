//! The decode seam: [`Decoder`] (PCM) and [`DsdDecoder`] (native 1-bit DSD),
//! plus [`DecodeError`]. Stream description lives in [`crate::model`].

use crate::model::StreamInfo;

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

/// A streaming **PCM** decoder producing interleaved `i32` samples into a
/// caller-provided buffer. Implementors allocate nothing.
///
/// This is the uniform seam every multi-bit format converges on — WAV, FLAC,
/// MP3, and DSD once wrapped by a [`crate::dsd`] adapter.
pub trait Decoder {
    /// Stream parameters, known after construction.
    fn stream_info(&self) -> StreamInfo;

    /// Decode up to `out.len()` interleaved samples into `out`.
    ///
    /// Returns the number of samples written; `Ok(0)` means end of stream.
    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError>;
}

/// A streaming **native DSD** decoder producing packed 1-bit samples (8 per
/// byte) into a caller-provided buffer. Implementors allocate nothing.
///
/// DSD is not multi-bit PCM, so it is a *segregated* seam. To reach the PCM
/// pipeline, wrap a `DsdDecoder` in an explicit [`crate::dsd`] adapter
/// (`DopAdapter` / `DecimateAdapter`); to play it natively, feed `read_dsd`
/// output straight to a DSD-capable DAC via SAI.
pub trait DsdDecoder {
    /// Stream parameters; [`StreamInfo::format`] is
    /// [`SampleFormat::Dsd`](crate::model::SampleFormat::Dsd).
    fn stream_info(&self) -> StreamInfo;

    /// Read up to `out.len()` bytes of packed 1-bit DSD (8 samples per byte).
    ///
    /// Returns the number of bytes written; `Ok(0)` means end of stream.
    fn read_dsd(&mut self, out: &mut [u8]) -> Result<usize, DecodeError>;
}

/// Random access for a [`DsdDecoder`]. Implemented only for seekable sources
/// (gated on [`SeekSource`](crate::io::SeekSource) at the concrete type), so a
/// streaming DSD source cannot name it.
///
/// Seeks are byte/block granular — bit-accuracy is meaningless at MHz DSD rates
/// and would force bit-shifting the packed stream.
pub trait DsdSeek {
    /// Seek to per-channel DSD `sample` (snapped to the format's granularity).
    /// Returns the sample actually landed on.
    fn seek_to_sample(&mut self, sample: u64) -> Result<u64, DecodeError>;
}
