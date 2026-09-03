//! Headless sink for offline smoke runs (`SOUL_PLAYER_OFFLINE=1`): lets the
//! full command/event wiring run without an audio device, mirroring the
//! service crate's test NullSink. Playback "runs" instantly (no backpressure),
//! which is exactly what an offline wiring test wants.
#![forbid(unsafe_code)]

use soul_audio_core::{SampleFormat, Sink, SinkError, StreamInfo};

/// A [`Sink`] that discards output — no device, no backpressure.
pub struct NullSink {
    rate: u32,
    channels: u16,
}

impl NullSink {
    pub fn new(rate: u32, channels: u16) -> Self {
        Self { rate, channels }
    }
}

impl Sink for NullSink {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: self.rate,
            channels: self.channels,
            format: SampleFormat::Int { bits: 24 },
        }
    }
    fn write(&mut self, _frames: &[i32]) -> Result<(), SinkError> {
        Ok(())
    }
    fn flush(&mut self) -> Result<(), SinkError> {
        Ok(())
    }
}
