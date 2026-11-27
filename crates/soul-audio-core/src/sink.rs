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

    /// Whether this sink drives the device in **exclusive** (bit-perfect) mode,
    /// bypassing the OS mixer so the device receives the stream untouched. Default
    /// `false` (shared mode, the OS may resample). The signal-path projection reads
    /// this to report whether output is truly bit-perfect.
    fn exclusive(&self) -> bool {
        false
    }
    /// Whether this sink **alters the samples** it is handed before they reach
    /// the device — a DSP decorator (EQ, crossfeed, convolution) wrapping
    /// another sink, as the desktop host's does. Default `false`: a plain
    /// device sink passes the deck's block through untouched.
    ///
    /// The deck cannot see past its sink, so a decorator that is processing has
    /// to say so itself; this is the term
    /// [`DecodeInfo::bit_perfect`](crate::deck::DecodeInfo::bit_perfect) uses
    /// to avoid claiming a bit-perfect path through an active DSP stage. A
    /// decorator that is *bypassed* (a bit-exact passthrough) should report
    /// `false` — the claim is about what it does to the audio, not about
    /// whether it is in the chain.
    fn modifies_samples(&self) -> bool {
        false
    }

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
    ///
    /// An implementation **must ignore a `NaN`** (an infinity is fine to clamp:
    /// `+inf` → 1.0 is unambiguous). `f32::clamp` does not help for the NaN —
    /// it returns `NaN` for a `NaN` input — and a NaN gain is not a transient
    /// glitch: a smoothed gain (`cur += (target - cur) * k`) stays
    /// NaN for the life of the stream once it has seen one, so a later sane
    /// volume cannot recover the audio. [`Deck::set_volume`](crate::Deck::set_volume)
    /// already refuses such a request, so this is the guard for a host that
    /// drives a sink directly.
    fn set_volume(&mut self, volume: f32) {
        let _ = volume;
    }

    /// Reopen the output device at `rate` **in place** — for native-rate /
    /// bit-perfect switching, where the engine plays each track at the device's own
    /// native rate instead of resampling to a fixed one.
    ///
    /// The default is a **no-op**: a shared-mode sink keeps its fixed rate (the deck
    /// keeps resampling to it). An **exclusive** sink must *release its device and
    /// re-acquire it* at `rate` — exclusive mode is one-client-per-device, so it
    /// cannot simply open a second stream alongside the current one (that fails with
    /// device-in-use). It should fall back to its previous rate if the device refuses
    /// the new one, so it is never left closed.
    ///
    /// After this call the caller re-reads [`stream_info`](Sink::stream_info) for the
    /// rate the sink actually ended at (which equals `rate` on success, or the
    /// previous rate on a refused switch).
    ///
    /// # Errors
    /// [`SinkError`] only if the device cannot be reacquired at all.
    fn reopen(&mut self, rate: u32) -> Result<(), SinkError> {
        let _ = rate;
        Ok(())
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
