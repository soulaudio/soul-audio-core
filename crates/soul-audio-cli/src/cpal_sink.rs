//! Desktop [`Sink`] backed by cpal — the default output device fed through a
//! lock-free SPSC ring.
//!
//! The player thread pushes interleaved `i32` (source scale) via
//! [`Sink::write`]; cpal's real-time callback pops from the ring and converts to
//! the device sample format, normalized by the source bit depth. `write` blocks
//! when the ring is full (backpressure), pacing decode to the device clock; an
//! empty ring plays silence (a soft underrun).

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use soul_audio_core::model::SampleFormat as CoreFmt;
use soul_audio_core::{Sink, SinkError, StreamInfo};

/// A cpal output stream presented as a [`Sink`].
pub struct CpalSink {
    prod: <HeapRb<i32> as Split>::Prod,
    // Held to keep the stream alive; dropping it stops playback.
    _stream: Stream,
    rate: u32,
    channels: u16,
    bits: u8,
}

impl CpalSink {
    /// Open the default output device. `source_bits` is the decoded stream's bit
    /// depth, used to normalize `i32` → the device format.
    pub fn new(source_bits: u8) -> Result<Self> {
        let device = cpal::default_host()
            .default_output_device()
            .context("no default output device")?;
        let supported = device
            .default_output_config()
            .context("querying default output config")?;
        let sample_format = supported.sample_format();
        let config: StreamConfig = supported.config();
        let channels = config.channels;
        let rate = config.sample_rate.0;

        // ~1 second of buffering.
        let cap = rate as usize * channels as usize;
        let (prod, mut cons) = HeapRb::<i32>::new(cap).split();
        let bits = source_bits;
        let err_fn = |e| eprintln!("cpal stream error: {e}");

        let stream = match sample_format {
            SampleFormat::F32 => {
                // Full-scale divisor: 2^(bits-1) maps the source's peak to ±1.0.
                let scale = 2f32.powi(i32::from(bits) - 1);
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [f32], _| {
                            for s in data.iter_mut() {
                                *s = cons.try_pop().unwrap_or(0) as f32 / scale;
                            }
                        },
                        err_fn,
                        None,
                    )
                    .context("building f32 output stream")?
            }
            SampleFormat::I16 => {
                let shift = i32::from(bits) - 16;
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [i16], _| {
                            for s in data.iter_mut() {
                                let v = cons.try_pop().unwrap_or(0);
                                let scaled = if shift > 0 { v >> shift } else { v << (-shift) };
                                *s = scaled.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
                            }
                        },
                        err_fn,
                        None,
                    )
                    .context("building i16 output stream")?
            }
            other => return Err(anyhow!("unsupported device sample format: {other:?}")),
        };
        stream.play().context("starting stream")?;
        Ok(Self {
            prod,
            _stream: stream,
            rate,
            channels,
            bits,
        })
    }
}

impl Sink for CpalSink {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: self.rate,
            channels: self.channels,
            format: CoreFmt::Int { bits: self.bits },
        }
    }

    fn write(&mut self, frames: &[i32]) -> Result<(), SinkError> {
        let mut off = 0;
        while off < frames.len() {
            off += self.prod.push_slice(&frames[off..]);
            if off < frames.len() {
                // Ring full — wait for the callback to drain some.
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        // Wait for the callback to play out what's buffered.
        while self.prod.occupied_len() > 0 {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Ok(())
    }
}
