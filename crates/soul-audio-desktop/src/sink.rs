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
use ringbuf::traits::{Observer, Split};
use soul_audio_core::model::SampleFormat as CoreFmt;
use soul_audio_core::{Sink, SinkError, StreamInfo};

use soul_audio_core::convert::{self, CANONICAL_BITS};

pub(crate) use crate::ring::{CpalRing, DISCARD_FADE_MS, Fill, push_blocking};

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
    /// The device's real output sample format (what the OS shared mixer accepts) —
    /// reported via [`Sink::stream_info`] so the signal path shows the true output
    /// depth, not the canonical-24 the deck feeds in.
    format: CoreFmt,
    /// Liveness of the device this stream opened on (see [`DeviceAlive`]).
    alive: DeviceAlive,
}

/// Whether the output device this sink opened on still exists.
///
/// macOS (with `coreaudio-exclusive`, which brings `coreaudio-rs`): a
/// `kAudioDevicePropertyDeviceIsAlive` listener on that device. On Apple silicon the
/// headphone jack, AirPods / Bluetooth and USB DACs are separate CoreAudio devices that
/// **disappear** on unplug — so a write fails with [`SinkError::Closed`] and the player
/// pauses (Apple Music behaviour) instead of the default-output stream silently moving
/// to the laptop speakers. Elsewhere: always alive (cpal's error callback is the only
/// signal there).
struct DeviceAlive {
    #[cfg(all(target_os = "macos", feature = "coreaudio-exclusive"))]
    listener: Option<Box<coreaudio::audio_unit::macos_helpers::AliveListener>>,
}

impl DeviceAlive {
    /// No watch (always alive) — the state before a constructor knows the device.
    fn unwatched() -> Self {
        Self {
            #[cfg(all(target_os = "macos", feature = "coreaudio-exclusive"))]
            listener: None,
        }
    }

    /// Watch the device cpal names `name` (the system default when `None`).
    #[cfg_attr(
        not(all(target_os = "macos", feature = "coreaudio-exclusive")),
        allow(unused_variables)
    )]
    fn watch(name: Option<&str>) -> Self {
        #[cfg(all(target_os = "macos", feature = "coreaudio-exclusive"))]
        {
            use coreaudio::audio_unit::macos_helpers as hal;
            let id = match name {
                Some(n) => hal::get_device_id_from_name(n),
                None => hal::get_default_device_id(false),
            };
            let listener = id.and_then(|id| {
                // Boxed: CoreAudio keeps a pointer to the listener.
                let mut l = Box::new(hal::AliveListener::new(id));
                l.register().ok().map(|_| l)
            });
            Self { listener }
        }
        #[cfg(not(all(target_os = "macos", feature = "coreaudio-exclusive")))]
        {
            Self {}
        }
    }

    fn gone(&self) -> bool {
        #[cfg(all(target_os = "macos", feature = "coreaudio-exclusive"))]
        {
            self.listener.as_ref().is_some_and(|l| !l.is_alive())
        }
        #[cfg(not(all(target_os = "macos", feature = "coreaudio-exclusive")))]
        {
            false
        }
    }
}

impl CpalSink {
    /// Open the default output device. Input is always canonical 24-bit PCM (the
    /// deck normalizes every track to it), so no per-source bit depth is needed.
    pub fn new() -> Result<Self> {
        let device = cpal::default_host()
            .default_output_device()
            .context("no default output device")?;
        let mut sink = Self::build(device)?;
        sink.alive = DeviceAlive::watch(None);
        Ok(sink)
    }

    /// Open a specific output device by name (one of [`output_device_names`]).
    /// Errors if no device matches — the caller can fall back to
    /// [`new`](Self::new) for the system default.
    pub fn new_on_device(name: &str) -> Result<Self> {
        // Resolve from the warm device cache first (skips cpal's slow re-enumeration on a
        // device switch). If the cached handle won't open — the device was unplugged since it
        // was listed — fall back to a fresh enumeration so a genuinely-gone device still errors
        // cleanly (and the caller can fall back to the system default).
        if let Some((_, device)) = cached_output_devices(false)
            .into_iter()
            .find(|(n, _)| n == name)
            && let Ok(mut sink) = Self::build(device)
        {
            sink.alive = DeviceAlive::watch(Some(name));
            return Ok(sink);
        }
        let device = cached_output_devices(true)
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, d)| d)
            .with_context(|| format!("no output device named {name:?}"))?;
        let mut sink = Self::build(device)?;
        sink.alive = DeviceAlive::watch(Some(name));
        Ok(sink)
    }

    /// Build a sink on an already-selected device. Input is always canonical
    /// 24-bit PCM (the deck normalizes every track to it).
    fn build(device: cpal::Device) -> Result<Self> {
        let supported = device
            .default_output_config()
            .context("querying default output config")?;
        let sample_format = supported.sample_format();
        // The device's real output format (shared-mode mixer format), reported as the
        // true output depth. The callback only renders F32 / I16 (below).
        let format = match sample_format {
            SampleFormat::F32 => CoreFmt::Float { bits: 32 },
            SampleFormat::I16 => CoreFmt::Int { bits: 16 },
            // Anything else is rejected when the stream is built below; fall back to
            // the canonical depth for the report so this stays total.
            _ => CoreFmt::Int {
                bits: CANONICAL_BITS,
            },
        };
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
        let (prod, cons) = HeapRb::<i32>::new(cap).split();
        let mut cons = CpalRing(cons);
        let err_fn = |e| eprintln!("cpal stream error: {e}");

        let flush = Arc::new(AtomicBool::new(false));
        let volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));

        // On a seek, `discard` requests a ring clear via `flush`. The callback
        // honors it through `Fill`: ramp the buffered pre-seek audio down to zero
        // over a short fade (killing the splice click), THEN clear the remainder
        // and release the ack (`store(false)`). Ordering matters — the ack's
        // Release publishes the clear to the producer's Acquire-load in `discard`,
        // so the producer only resumes writing (post-seek audio) once the clear
        // has happened.
        let cb_flush = Arc::clone(&flush);
        let cb_volume = Arc::clone(&volume);
        // Fade-out length in interleaved samples: `DISCARD_FADE_MS` at the device
        // rate × channels, rounded. Computed in u64 so no rate/channel pair can
        // overflow the intermediate product.
        let fade_len = ((u64::from(DISCARD_FADE_MS) * u64::from(rate) * u64::from(channels) + 500)
            / 1000) as u32;

        // The callback pops from the ring, applies a smoothed gain (so a volume
        // change is heard on already-buffered samples — instantly, not after the
        // ring drains — without an audible click) plus any seek fade-out, and
        // converts via the unit-tested `convert` functions (silence on underrun).
        // `Fill` holds that per-sample state and is unit-tested device-free.
        let stream = match sample_format {
            SampleFormat::F32 => {
                let mut fill = Fill::new(fade_len);
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [f32], _| {
                            fill.begin(&cb_flush, &mut cons);
                            convert::fill_f32(
                                || {
                                    let target = f32::from_bits(cb_volume.load(Ordering::Relaxed));
                                    fill.next(&mut cons, &cb_flush, target)
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
                let mut fill = Fill::new(fade_len);
                device
                    .build_output_stream(
                        &config,
                        move |data: &mut [i16], _| {
                            fill.begin(&cb_flush, &mut cons);
                            convert::fill_i16(
                                || {
                                    let target = f32::from_bits(cb_volume.load(Ordering::Relaxed));
                                    fill.next(&mut cons, &cb_flush, target)
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
            format,
            alive: DeviceAlive::unwatched(), // the constructors install the real watch
        })
    }
}

/// How long an enumerated output-device list is reused before re-enumerating. cpal's
/// WASAPI enumeration activates every endpoint (hundreds of ms to seconds, worst on a
/// Bluetooth endpoint), so re-running it on every device open/switch made the first play and
/// a live device switch wait out the whole enumeration. A desktop re-lists devices every few
/// seconds for its picker, which keeps this cache warm, so opening a device then resolves it
/// from the cache instead of re-enumerating. Short enough that a plugged/unplugged device is
/// reflected within a few seconds.
const DEVICE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(5);

struct DeviceCache {
    at: std::time::Instant,
    devices: Vec<(String, cpal::Device)>,
}

/// A process-wide cache of the last output-device enumeration (name + handle). cpal's
/// `Device` is a cheap, `Send + Sync + Clone` handle, so caching it is safe; a stale handle
/// (the device was unplugged) simply fails to open, and the caller re-enumerates. `None`
/// until the first enumeration.
static DEVICE_CACHE: std::sync::Mutex<Option<DeviceCache>> = std::sync::Mutex::new(None);

/// The output devices (name + handle) on the default host. Reuses a recent enumeration while
/// it is still fresh (within [`DEVICE_CACHE_TTL`]); otherwise re-enumerates and refreshes the
/// cache. `force` bypasses a fresh cache to re-enumerate now (used as the fallback when a
/// cached handle failed to open, e.g. the device was unplugged since it was listed).
fn cached_output_devices(force: bool) -> Vec<(String, cpal::Device)> {
    let mut guard = DEVICE_CACHE.lock().expect("device cache mutex");
    if !force
        && let Some(c) = guard.as_ref()
        && c.at.elapsed() < DEVICE_CACHE_TTL
    {
        return c.devices.clone();
    }
    let devices: Vec<(String, cpal::Device)> = cpal::default_host()
        .output_devices()
        .map(|devs| devs.filter_map(|d| d.name().ok().map(|n| (n, d))).collect())
        .unwrap_or_default();
    *guard = Some(DeviceCache {
        at: std::time::Instant::now(),
        devices: devices.clone(),
    });
    devices
}

/// Names of the available output devices on the default host, for a device
/// picker. Empty if the host exposes none (e.g. a headless CI machine). Served from the
/// short-lived device cache (see [`cached_output_devices`]) so a picker that polls this does
/// not re-run cpal's slow enumeration every time.
pub fn output_device_names() -> Vec<String> {
    cached_output_devices(false)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// Name of the current system-default output device, if any — for preselecting
/// the picker's active entry.
pub fn default_output_device_name() -> Option<String> {
    cpal::default_host()
        .default_output_device()
        .and_then(|d| d.name().ok())
}

/// The `(sample_rate, channels)` of an output device's default (shared-mixer)
/// config — `name` picks one of [`output_device_names`], `None` the system
/// default. `None` if the device is gone or won't report a config.
///
/// Used to open an exclusive-mode stream at the device's native rate: the
/// shared mix-format rate is the one virtually every DAC also accepts
/// exclusively, so it's the safe rate to request for bit-perfect output (a
/// rate-matched track then plays untouched; a mismatched one is reported as
/// resampling until per-track rate switching lands).
pub fn default_output_config(name: Option<&str>) -> Option<(u32, u16)> {
    let device = match name {
        // Resolve a named device from the warm cache (skips cpal's slow re-enumeration).
        Some(name) => cached_output_devices(false)
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, d)| d)?,
        None => cpal::default_host().default_output_device()?,
    };
    let cfg = device.default_output_config().ok()?;
    Some((cfg.sample_rate().0, cfg.channels()))
}

impl Sink for CpalSink {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: self.rate,
            channels: self.channels,
            format: self.format,
        }
    }

    fn write(&mut self, frames: &[i32]) -> Result<(), SinkError> {
        // Ring full → wait for the callback to drain some; device gone → Closed.
        let alive = &self.alive;
        push_blocking(&mut self.prod, frames, || alive.gone())
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        // Wait for the callback to play out what's buffered.
        while self.prod.occupied_len() > 0 {
            if self.alive.gone() {
                return Err(SinkError::Closed);
            }
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

    /// A `NaN` request is ignored: `clamp` passes a `NaN` through, and the
    /// renderer's smoothed gain keeps a `NaN` for the life of the stream (see
    /// the `Sink::set_volume` contract), so one bad value would mute output
    /// until the device is reopened. An infinity still clamps.
    fn set_volume(&mut self, volume: f32) {
        if volume.is_nan() {
            return;
        }
        self.volume
            .store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
}
