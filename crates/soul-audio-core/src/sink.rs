//! Audio **output** abstraction.
//!
//! A [`Sink`] is where decoded, rate-matched, normalized PCM goes — a desktop
//! cpal stream, an embedded SAI/I²S DMA, or a test double. Concrete sinks live in
//! the platform shells (they need std / hardware); this crate holds only the
//! trait. The [`Deck`](crate::Deck) is what drives a decoder into a sink.
//!
//! The trait is `no_std`: [`write`](Sink::write) applies backpressure so the
//! producer (the deck's pump) paces to the device's real-time clock. A concrete
//! sink typically owns a lock-free SPSC ring whose consumer is the real-time
//! callback / DMA ISR — that ring is the only real-time boundary.

use crate::model::StreamInfo;

/// Output-device errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkError {
    /// The output device / stream is gone.
    Closed,
    /// The requested stream configuration is unsupported.
    Unsupported,
}

impl core::fmt::Display for SinkError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            SinkError::Closed => "output device closed",
            SinkError::Unsupported => "unsupported stream configuration",
        })
    }
}

impl core::error::Error for SinkError {}

/// An audio output that consumes interleaved `i32` PCM at a fixed rate.
///
/// The [`Deck`](crate::Deck) writes **canonical 24-bit** samples (`±2^23`), so a
/// sink is bit-depth-agnostic: it converts that one scale to whatever the device
/// wants.
pub trait Sink {
    /// The rate / channels / format the device consumes.
    fn stream_info(&self) -> StreamInfo;
    /// Write **all** of `frames` (interleaved `i32`, length = frame count ×
    /// channels), blocking as needed for buffer space. Returns on completion.
    ///
    /// # Errors
    /// [`SinkError`] if the device is gone or the write cannot complete.
    fn write(&mut self, frames: &[i32]) -> Result<(), SinkError>;
    /// Play out any buffered audio and stop (end of stream).
    ///
    /// # Errors
    /// [`SinkError`] if the device is gone.
    fn flush(&mut self) -> Result<(), SinkError>;

    /// Discard any buffered-but-unplayed audio so frames written *after* this
    /// call are heard promptly. Called on a seek (a playback discontinuity):
    /// without it the producer's already-queued pre-seek PCM plays out before
    /// the new position, which is audible as a lag between the scrub and the
    /// sound. Unlike [`flush`](Sink::flush), which *waits for* the buffer to
    /// play out, this *throws it away*.
    ///
    /// Default: a no-op — an unbuffered or test sink has nothing to drop.
    ///
    /// # Errors
    /// [`SinkError`] if the device is gone.
    fn discard(&mut self) -> Result<(), SinkError> {
        Ok(())
    }

    /// Set output volume 0.0..=1.0. A sink that applies gain at its output stage
    /// (e.g. a device callback) overrides this so changes are heard immediately;
    /// the default is a no-op for sinks without an output-side gain.
    fn set_volume(&mut self, volume: f32) {
        let _ = volume;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate alloc;
    use alloc::string::ToString;

    #[test]
    fn sink_error_displays() {
        use core::error::Error;
        let e = SinkError::Closed;
        assert_eq!(e.to_string(), "output device closed");
        assert_eq!(
            SinkError::Unsupported.to_string(),
            "unsupported stream configuration"
        );
        // It is a std/core error (so it flows through `?` into anyhow etc.).
        assert!(e.source().is_none());
    }
}
