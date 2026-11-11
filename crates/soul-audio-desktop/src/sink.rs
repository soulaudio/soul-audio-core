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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use soul_audio_core::model::SampleFormat as CoreFmt;
use soul_audio_core::{Sink, SinkError, StreamInfo};

use soul_audio_core::convert::{self, CANONICAL_BITS};

/// A cpal output stream presented as a [`Sink`] consuming canonical 24-bit PCM.
pub struct CpalSink {
    prod: <HeapRb<i32> as Split>::Prod,
    /// Seek-discard handshake with the real-time callback. [`Sink::discard`]
    /// stores `true` (a request); the callback clears the ring, then stores
    /// `false` (the ack). The producer waits for the ack, so post-seek audio it
    /// writes next is never caught by the clear.
    flush: Arc<AtomicBool>,
    /// Target output volume (`f32::to_bits`, default `1.0`), read and ramped
    /// toward per-sample by the real-time callback so changes are heard
    /// immediately (applied to already-buffered samples as they play) instead of
    /// lagging by the ring's ~200 ms depth.
    volume: Arc<AtomicU32>,
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
        Self::build(device)
    }

    /// Open a specific output device by name (one of [`output_device_names`]).
    /// Errors if no device matches — the caller can fall back to
    /// [`new`](Self::new) for the system default.
    pub fn new_on_device(name: &str) -> Result<Self> {
        let device = cpal::default_host()
            .output_devices()
            .context("enumerating output devices")?
            .find(|d| d.name().map(|n| n == name).unwrap_or(false))
            .with_context(|| format!("no output device named {name:?}"))?;
        Self::build(device)
    }

    /// Build a sink on an already-selected device. Input is always canonical
    /// 24-bit PCM (the deck normalizes every track to it).
    fn build(device: cpal::Device) -> Result<Self> {
        let supported = device
            .default_output_config()
            .context("querying default output config")?;
        let sample_format = supported.sample_format();
        let config: StreamConfig = supported.config();
        let channels = config.channels;
        let rate = config.sample_rate.0;

        // ~200 ms of buffering. Deep enough to ride out pump-thread scheduling
        // jitter (the pump decodes well ahead of real-time, including DSD at
        // ~3x), but shallow enough that a volume or pause change is heard
        // promptly instead of after a second of already-queued audio drains.
        // (A seek clears the ring outright via `discard`.)
        const BUFFER_MS: usize = 200;
        let cap = (rate as usize * channels as usize * BUFFER_MS) / 1000;
        let (prod, mut cons) = HeapRb::<i32>::new(cap).split();
        let err_fn = |e| eprintln!("cpal stream error: {e}");

        let flush = Arc::new(AtomicBool::new(false));
        let volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));

        // On a seek, `discard` requests a ring clear via `flush`. The callback
        // honors it here: clear the buffered pre-seek audio, THEN release the ack
        // (`store(false)`). Ordering matters — the ack's Release publishes the
        // clear to the producer's Acquire-load in `discard`, so the producer only
        // resumes writing (post-seek audio) once the clear has happened.
        let cb_flush = Arc::clone(&flush);
        let cb_volume = Arc::clone(&volume);

        // The callback pops from the ring, applies a smoothed gain (so a volume
        // change is heard on already-buffered samples — instantly, not after the
        // ring drains — without an audible click), and converts via the
        // unit-tested `convert` functions (silence on underrun).
        let stream = match sample_format {
            SampleFormat::F32 => {
                let mut cur_gain = 1.0f32;
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [f32], _| {
                            if cb_flush.load(Ordering::Acquire) {
                                cons.clear();
                                cb_flush.store(false, Ordering::Release);
                            }
                            convert::fill_f32(
                                || {
                                    let target = f32::from_bits(cb_volume.load(Ordering::Relaxed));
                                    cur_gain += (target - cur_gain) * 0.01;
                                    cons.try_pop().map(|s| (s as f32 * cur_gain) as i32)
                                },
                                data,
                            );
                        },
                        err_fn,
                        None,
                    )
                    .context("building f32 output stream")?
            }
            SampleFormat::I16 => {
                let mut cur_gain = 1.0f32;
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [i16], _| {
                            if cb_flush.load(Ordering::Acquire) {
                                cons.clear();
                                cb_flush.store(false, Ordering::Release);
                            }
                            convert::fill_i16(
                                || {
                                    let target = f32::from_bits(cb_volume.load(Ordering::Relaxed));
                                    cur_gain += (target - cur_gain) * 0.01;
                                    cons.try_pop().map(|s| (s as f32 * cur_gain) as i32)
                                },
                                data,
                            );
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
            flush,
            volume,
            _stream: stream,
            rate,
            channels,
        })
    }
}

/// Names of the available output devices on the default host, for a device
/// picker. Empty if the host exposes none (e.g. a headless CI machine).
pub fn output_device_names() -> Vec<String> {
    cpal::default_host()
        .output_devices()
        .map(|devs| devs.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

/// Name of the current system-default output device, if any — for preselecting
/// the picker's active entry.
pub fn default_output_device_name() -> Option<String> {
    cpal::default_host()
        .default_output_device()
        .and_then(|d| d.name().ok())
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

    fn discard(&mut self) -> Result<(), SinkError> {
        // Request a ring clear, then wait (bounded) for the callback's ack. The
        // wait guarantees the buffered pre-seek audio is gone before the pump
        // writes the post-seek block; the bound (~200 ms) means a dead stream
        // whose callback never runs can't hang the pump.
        self.flush.store(true, Ordering::Release);
        for _ in 0..100 {
            if !self.flush.load(Ordering::Acquire) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        Ok(())
    }

    fn set_volume(&mut self, volume: f32) {
        self.volume
            .store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
}
