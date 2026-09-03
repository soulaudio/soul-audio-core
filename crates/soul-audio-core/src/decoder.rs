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

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            DecodeError::UnexpectedEof => "unexpected end of stream",
            DecodeError::Malformed => "malformed stream",
            DecodeError::Unsupported => "unsupported format or variant",
            DecodeError::Io => "reader I/O error",
        })
    }
}

impl core::error::Error for DecodeError {}

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

/// A [`Decoder`] that can also seek and report its length — the capability the
/// playback [`Deck`](crate::deck::Deck) needs from a track. [`AnyDecoder`] with a
/// seekable source implements it.
pub trait SeekableDecoder: Decoder {
    /// Seek to per-channel `sample`; returns the frame actually landed on.
    ///
    /// # Errors
    /// [`DecodeError::Unsupported`] if the source cannot seek, else a decode/IO
    /// error.
    fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError>;
    /// Total per-channel frames, if the stream declares a length.
    fn total_frames(&self) -> Option<u64>;
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

    /// Total per-channel DSD sample count, if the container declares it (DSF
    /// stores it in its `fmt ` chunk). `None` for a length-unknown/streaming
    /// source. A PCM adapter divides this by its decimation ratio to report the
    /// track's PCM duration. Default `None` so streaming decoders opt out.
    fn total_samples(&self) -> Option<u64> {
        None
    }
}

/// Convert a time in seconds to a sample/frame index at `rate` samples/second,
/// rounding to nearest and clamping negatives to 0. Shared by the decoders'
/// `seek_to_time` convenience methods — the `f64` lives only at this boundary,
/// keeping the seek internals integer. `no_std`-safe (no `libm`).
#[must_use]
pub(crate) fn secs_to_samples(secs: f64, rate: u32) -> u64 {
    if secs <= 0.0 {
        return 0;
    }
    (secs * f64::from(rate) + 0.5) as u64
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secs_to_samples_clamps_and_rounds() {
        // Non-positive (and non-finite) inputs clamp to 0 — a seek can't go before
        // the start, and `NaN`/negative must not produce a garbage sample index.
        assert_eq!(secs_to_samples(0.0, 44_100), 0);
        assert_eq!(secs_to_samples(-1.0, 44_100), 0);
        assert_eq!(secs_to_samples(f64::NAN, 44_100), 0);
        // Exact and round-to-nearest.
        assert_eq!(secs_to_samples(1.0, 44_100), 44_100);
        assert_eq!(secs_to_samples(5.25, 2), 11); // 10.5 → 11 (round half up)
        assert_eq!(secs_to_samples(5.24, 2), 10); // 10.48 → 10
    }
}
