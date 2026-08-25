//! Desktop [`Sink`] backed by cpal — the default output device fed through a
//! lock-free SPSC ring.
//!
//! The [`Deck`](soul_audio_core::Deck) pushes interleaved `i32` **canonical
//! 24-bit** PCM (`±2^23`) via [`Sink::write`]; cpal's real-time callback pops
//! from the ring and converts to the device sample format. Because the deck
//! already normalized every track to the same 24-bit scale, the sink is
//! bit-depth-agnostic — it never needs to know the source depth. `write` blocks
//! when the ring is full (backpressure), pacing the pump to the device clock; an
//! empty ring plays silence (a soft underrun).

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use soul_audio_core::model::SampleFormat as CoreFmt;
use soul_audio_core::{Sink, SinkError, StreamInfo};

/// Canonical input bit depth the deck normalizes to (`±2^23`).
const CANONICAL_BITS: u8 = 24;

/// A cpal output stream presented as a [`Sink`] consuming canonical 24-bit PCM.
pub struct CpalSink {
    prod: <HeapRb<i32> as Split>::Prod,
    // Held to keep the stream alive; dropping it stops playback.
    _stream: Stream,
    rate: u32,
    channels: u16,
}

impl CpalSink {
    /// Open the default output device. Input is always canonical 24-bit PCM (the
    /// deck normalizes every track to it), so no per-source bit depth is needed.
    pub fn new() -> Result<Self> {
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
        let err_fn = |e| eprintln!("cpal stream error: {e}");

        let stream = match sample_format {
            SampleFormat::F32 => {
                // 2^(24-1) maps the canonical peak (±2^23) to ±1.0 full scale.
                let scale = 2f32.powi(i32::from(CANONICAL_BITS) - 1);
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
                // 24-bit → 16-bit: drop the low 8 bits.
                let shift = i32::from(CANONICAL_BITS) - 16;
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [i16], _| {
                            for s in data.iter_mut() {
                                let v = cons.try_pop().unwrap_or(0) >> shift;
                                *s = v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
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
        })
    }
}

impl Sink for CpalSink {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: self.rate,
            channels: self.channels,
            format: CoreFmt::Int {
                bits: CANONICAL_BITS,
            },
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
