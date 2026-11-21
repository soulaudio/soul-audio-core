//! The platform-neutral **exclusive-output engine**: the state machine that takes a
//! device exclusively, switches it to each track's native rate and widest physical format,
//! renders canonical 24-bit PCM into it, and hands it back exactly as it found it.
//!
//! The engine talks to hardware only through the small [`Hal`] trait, so the same logic is
//! driven by:
//! - [`CoreAudioHal`](crate::coreaudio_exclusive::CoreAudioHal) on macOS (the shipped sink,
//!   `CoreAudioExclusiveSink = ExclusiveEngine<CoreAudioHal>`), and
//! - [`FakeHal`](crate::fake_hal::FakeHal) in tests — a scriptable device model loaded from
//!   recorded **device-profile fixtures** (MacBook speakers, USB DACs, HDMI, AirPods, …)
//!   with fault injection (rate change refused, format change times out, hog held by
//!   another process, device unplugged mid-play). That lets every Apple device class and
//!   failure path be tested deterministically on any OS, without owning the hardware.
//!
//! Lifecycle (mirrors mpv's `ao_coreaudio_exclusive`):
//! 1. resolve the device **once** and pin it (macOS moves the system default off a hogged
//!    device, so re-resolving "default" would chase our own side effect);
//! 2. read its physical formats + current nominal rate (to restore later);
//! 3. take hog mode — refusing (never stealing) if another process holds it;
//! 4. per track: stop the stream, set the nominal rate ([`choose_rate`]), set the widest
//!    physical format the driver accepts ([`pick_physical_format`], best-effort down the
//!    ladder), start a fresh stream at that rate;
//! 5. on drop: stop, restore the original nominal rate (+ best format there), release hog
//!    mode only if it is still ours.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use ringbuf::HeapRb;
use ringbuf::traits::{Observer, Split};
use soul_audio_core::convert;
use soul_audio_core::model::SampleFormat as CoreFmt;
use soul_audio_core::{Sink, SinkError, StreamInfo};

use crate::exclusive_policy::{PhysFormat, choose_rate, pick_physical_format, supported_rates};
use crate::sink::{CpalRing, DISCARD_FADE_MS, Fill, push_blocking};

/// ~200 ms of buffering, like [`CpalSink`](crate::CpalSink).
pub const BUFFER_MS: usize = 200;

/// "Nobody holds hog mode" — CoreAudio's `kAudioDevicePropertyHogMode` value for free.
pub const HOG_FREE: i32 = -1;

/// Why opening the device in exclusive mode failed — lets the host fall back to shared
/// (or stop, in strict mode). Same shape as the Windows `ExclusiveError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExclusiveError {
    /// The device offers no usable physical format / rate, or its driver refuses hog mode
    /// (AirPlay, Bluetooth, aggregate devices) — bit-perfect is impossible there.
    Unsupported,
    /// Another process holds the device in hog mode.
    DeviceInUse,
    /// The named (or default) output device wasn't found.
    NoDevice,
    /// Any other driver failure (message for diagnostics).
    Other(String),
}

impl core::fmt::Display for ExclusiveError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ExclusiveError::Unsupported => {
                write!(f, "device has no exclusive format for this track")
            }
            ExclusiveError::DeviceInUse => write!(f, "device is in use by another app"),
            ExclusiveError::NoDevice => write!(f, "output device not found"),
            ExclusiveError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ExclusiveError {}

/// Live observations of one device for the sink's lifetime (CoreAudio's
/// `kAudioDevicePropertyDeviceIsAlive` + `kAudioDevicePropertyNominalSampleRate` listeners).
pub trait DeviceWatch {
    /// Whether the device is still present.
    fn is_alive(&self) -> bool;
    /// The most recent nominal-rate change observed since the last call, if any —
    /// including ones we made ourselves (the engine ignores those).
    fn take_rate_change(&mut self) -> Option<u32>;
}

/// The hardware operations the engine needs. Every method is a single, synchronous driver
/// call (the real implementation waits for CoreAudio's async rate / format changes to land
/// before returning). Errors are messages; the engine decides what they mean.
pub trait Hal {
    /// A device handle (CoreAudio `AudioDeviceID`).
    type Device: Copy + Eq + core::fmt::Debug;
    /// A running render stream; dropping it stops rendering.
    type Stream;
    /// A liveness watch on one device.
    type Watch: DeviceWatch;

    /// This process's id (to recognise our own hog ownership).
    fn pid(&self) -> i32;
    /// Find an output device by name, or the system default for `None`.
    fn resolve(&self, name: Option<&str>) -> Option<Self::Device>;
    /// The device's physical output formats (LPCM only).
    fn physical_formats(&self, dev: Self::Device) -> Result<Vec<PhysFormat>, String>;
    /// The device's current nominal sample rate.
    fn nominal_rate(&self, dev: Self::Device) -> Option<u32>;
    /// Current hog-mode owner pid, or [`HOG_FREE`]. `Err` when the device has no hog-mode
    /// property at all (aggregate / multi-output devices).
    fn hog_owner(&self, dev: Self::Device) -> Result<i32, String>;
    /// Toggle hog mode (CoreAudio semantics: free → ours, ours → free, someone else's →
    /// unchanged). Returns the owner afterwards.
    fn toggle_hog(&self, dev: Self::Device) -> Result<i32, String>;
    /// Set the nominal sample rate and wait for it to take.
    fn set_nominal_rate(&self, dev: Self::Device, rate: u32) -> Result<(), String>;
    /// Set the physical format `fmt` at `rate` and wait for it to take.
    fn set_physical_format(
        &self,
        dev: Self::Device,
        fmt: &PhysFormat,
        rate: u32,
    ) -> Result<(), String>;
    /// Start watching the device's liveness.
    fn watch(&self, dev: Self::Device) -> Self::Watch;
    /// Start a render stream at `rate` / `channels`; the driver's real-time thread calls
    /// [`Renderer::render_f32`] for every buffer.
    fn start(
        &self,
        dev: Self::Device,
        rate: u32,
        channels: u16,
        renderer: Renderer,
    ) -> Result<Self::Stream, String>;
    /// `(client_rate, device_rate)` of a running stream — equal ⇔ no hidden SRC.
    fn stream_rates(&self, stream: &Self::Stream) -> Option<(u32, u32)>;
}

/// The real-time side of the engine: pops canonical-24 PCM from the ring through the
/// shared [`Fill`] (volume ramp + seek fade-out) and writes Float32 (`±2^23 → ±1.0`,
/// exact for every 24-bit value). Owned by the driver's render callback.
pub struct Renderer {
    cons: CpalRing,
    fill: Fill,
    flush: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
}

impl Renderer {
    /// Fill one interleaved Float32 device buffer (silence on underrun). Allocation-free.
    pub fn render_f32(&mut self, out: &mut [f32]) {
        let Self {
            cons,
            fill,
            flush,
            volume,
        } = self;
        fill.begin(flush, cons);
        convert::fill_f32(
            || {
                let target = f32::from_bits(volume.load(Ordering::Relaxed));
                fill.next(cons, flush, target)
            },
            out,
        );
    }
}

/// Exclusive, bit-perfect output over any [`Hal`]. See the module docs for the lifecycle.
pub struct ExclusiveEngine<H: Hal> {
    hal: H,
    prod: <HeapRb<i32> as Split>::Prod,
    flush: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    /// The running stream. `None` only transiently while switching, or after a failed
    /// switch could not even restore the previous rate.
    stream: Option<H::Stream>,
    watch: H::Watch,
    device: H::Device,
    formats: Vec<PhysFormat>,
    /// The nominal rate the device had before we touched it (restored on drop).
    original_rate: Option<u32>,
    /// The rate the hardware is actually at now (what to compare on restore).
    hw_rate: Option<u32>,
    /// Whether WE took hog mode (and so must release it).
    hogged: bool,
    rate: u32,
    channels: u16,
    format: CoreFmt,
}

impl<H: Hal> ExclusiveEngine<H> {
    /// Open `name` (or the system default) exclusively for `channels`-channel playback at
    /// (the closest supported match for) `rate`. On any failure the device is left as it
    /// was found (rate restored, hog released) and a typed [`ExclusiveError`] returned.
    pub fn open_with(
        hal: H,
        name: Option<&str>,
        rate: u32,
        channels: u16,
    ) -> Result<Self, ExclusiveError> {
        let device = hal.resolve(name).ok_or(ExclusiveError::NoDevice)?;
        let formats = hal
            .physical_formats(device)
            .map_err(|e| ExclusiveError::Other(format!("read physical formats: {e}")))?;
        if supported_rates(&formats, u32::from(channels)).is_empty() {
            return Err(ExclusiveError::Unsupported);
        }
        let original_rate = hal.nominal_rate(device);
        let hogged = take_hog(&hal, device)?;
        let watch = hal.watch(device);
        let mut engine = Self {
            prod: HeapRb::<i32>::new(1).split().0, // placeholder; `start_at` installs the real ring
            flush: Arc::new(AtomicBool::new(false)),
            volume: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            stream: None,
            watch,
            device,
            formats,
            original_rate,
            hw_rate: original_rate,
            hogged,
            rate,
            channels,
            format: CoreFmt::Float { bits: 32 },
            hal,
        };
        // On error `engine` drops here, which restores the rate and releases hog mode.
        engine.start_at(rate)?;
        Ok(engine)
    }

    /// The HAL this engine drives (tests inspect the fake through it).
    pub fn hal(&self) -> &H {
        &self.hal
    }

    /// The pinned device.
    pub fn device(&self) -> H::Device {
        self.device
    }

    /// `(client_rate, device_rate)` of the running stream — equal exactly when nothing
    /// resamples between us and the hardware. `None` while no stream runs.
    pub fn stream_rates(&self) -> Option<(u32, u32)> {
        self.hal.stream_rates(self.stream.as_ref()?)
    }

    /// Switch the (hogged) device to the best rate for `requested` and its widest accepted
    /// physical format there, then start a fresh stream + ring.
    fn start_at(&mut self, requested: u32) -> Result<(), ExclusiveError> {
        let ch = u32::from(self.channels);
        let rate = choose_rate(&supported_rates(&self.formats, ch), requested)
            .ok_or(ExclusiveError::Unsupported)?;
        if pick_physical_format(&self.formats, rate, ch).is_none() {
            return Err(ExclusiveError::Unsupported);
        }

        // Stop rendering BEFORE touching the device's rate / format.
        self.stream = None;
        self.hal
            .set_nominal_rate(self.device, rate)
            .map_err(|e| ExclusiveError::Other(format!("set nominal rate {rate}: {e}")))?;
        self.hw_rate = Some(rate);
        let _ = self.watch.take_rate_change(); // swallow the notification of our own change

        // Best-effort physical format, widest first: some drivers refuse (or time out on)
        // a format change, so keep the first one the device accepts.
        let picked = format_ladder(&self.formats, rate, ch)
            .into_iter()
            .find(|f| self.hal.set_physical_format(self.device, f, rate).is_ok())
            .ok_or_else(|| {
                ExclusiveError::Other(format!("device refused every physical format at {rate} Hz"))
            })?;

        let cap = (rate as usize * self.channels as usize * BUFFER_MS) / 1000;
        let (prod, cons) = HeapRb::<i32>::new(cap.max(1)).split();
        self.flush.store(false, Ordering::Release);
        let fade_len = ((u64::from(DISCARD_FADE_MS) * u64::from(rate) * u64::from(self.channels)
            + 500)
            / 1000) as u32;
        let renderer = Renderer {
            cons: CpalRing(cons),
            fill: Fill::new(fade_len),
            flush: Arc::clone(&self.flush),
            volume: Arc::clone(&self.volume),
        };
        let stream = self
            .hal
            .start(self.device, rate, self.channels, renderer)
            .map_err(|e| ExclusiveError::Other(format!("start stream at {rate} Hz: {e}")))?;
        self.stream = Some(stream);
        self.prod = prod;
        self.rate = rate;
        self.format = picked.report();
        Ok(())
    }
}

/// Every physical format usable at `rate` for `ch` channels, best first (the order
/// [`pick_physical_format`] ranks by).
pub fn format_ladder(formats: &[PhysFormat], rate: u32, ch: u32) -> Vec<PhysFormat> {
    let mut v: Vec<PhysFormat> = formats
        .iter()
        .copied()
        .filter(|f| f.supports_rate(rate) && f.channels >= ch && f.fidelity() > 0)
        .collect();
    v.sort_by_key(|f| std::cmp::Reverse((f.fidelity(), f.channels == ch)));
    v
}

/// Take hog mode on `dev`: `Ok(true)` = taken now (release on drop), `Ok(false)` = this
/// process already held it. Never steals: another owner → [`ExclusiveError::DeviceInUse`];
/// a driver that won't hog at all → [`ExclusiveError::Unsupported`].
fn take_hog<H: Hal>(hal: &H, dev: H::Device) -> Result<bool, ExclusiveError> {
    let me = hal.pid();
    // No hog-mode property (aggregate / multi-output devices) ⇒ exclusive is impossible.
    let owner = hal
        .hog_owner(dev)
        .map_err(|_| ExclusiveError::Unsupported)?;
    if owner == me {
        return Ok(false);
    }
    if owner != HOG_FREE {
        return Err(ExclusiveError::DeviceInUse);
    }
    let now = hal
        .toggle_hog(dev)
        .map_err(|e| ExclusiveError::Other(format!("take hog mode: {e}")))?;
    if now == me {
        Ok(true)
    } else if now == HOG_FREE {
        Err(ExclusiveError::Unsupported) // driver refuses hog mode
    } else {
        Err(ExclusiveError::DeviceInUse) // lost a race to another process
    }
}

impl<H: Hal> Drop for ExclusiveEngine<H> {
    fn drop(&mut self) {
        // 1. Stop rendering.
        self.stream = None;
        // 2. Hand the device back at the rate (and best format) we found it at.
        if let Some(orig) = self.original_rate
            && self.hw_rate != Some(orig)
            && self.watch.is_alive()
        {
            let _ = self.hal.set_nominal_rate(self.device, orig);
            let ch = u32::from(self.channels);
            if let Some(best) = pick_physical_format(&self.formats, orig, ch) {
                let _ = self.hal.set_physical_format(self.device, &best, orig);
            }
        }
        // 3. Release hog mode — only if it is still ours.
        if self.hogged && self.hal.hog_owner(self.device).ok() == Some(self.hal.pid()) {
            let _ = self.hal.toggle_hog(self.device);
        }
    }
}

impl<H: Hal> Sink for ExclusiveEngine<H> {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: self.rate,
            channels: self.channels,
            format: self.format,
        }
    }

    fn exclusive(&self) -> bool {
        true
    }

    /// Switch the hogged device to (the best match for) `rate` in place, keeping hog mode
    /// throughout so no other app can grab the device mid-switch. If the switch fails the
    /// previous rate is restored, so the sink is never left closed.
    fn reopen(&mut self, rate: u32) -> Result<(), SinkError> {
        if !self.watch.is_alive() {
            return Err(SinkError::Closed);
        }
        let ch = u32::from(self.channels);
        let target = choose_rate(&supported_rates(&self.formats, ch), rate).unwrap_or(self.rate);
        if target == self.rate && self.stream.is_some() {
            return Ok(()); // already there — the deck resamples the remainder, if any
        }
        let previous = self.rate;
        for try_rate in [target, previous] {
            if self.start_at(try_rate).is_ok() {
                return Ok(());
            }
        }
        Err(SinkError::Unsupported)
    }

    fn write(&mut self, frames: &[i32]) -> Result<(), SinkError> {
        if self.stream.is_none() || !self.watch.is_alive() {
            return Err(SinkError::Closed);
        }
        // Something changed the hardware rate under us (a driver-initiated switch, or the
        // HAL forcing a rate) — put OUR rate back; we hold hog mode, so we are entitled to
        // it. If the device won't go back, stop honestly rather than play at a wrong speed.
        if let Some(r) = self.watch.take_rate_change()
            && r != self.rate
        {
            self.hw_rate = Some(r);
            self.start_at(self.rate).map_err(|_| SinkError::Closed)?;
            let _ = self.watch.take_rate_change(); // our own re-assert
        }
        let watch = &self.watch;
        push_blocking(&mut self.prod, frames, || !watch.is_alive())
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        // A gone device never "played out" anything — even with an empty ring, report it.
        if !self.watch.is_alive() || self.stream.is_none() {
            return Err(SinkError::Closed);
        }
        while self.prod.occupied_len() > 0 {
            if !self.watch.is_alive() || self.stream.is_none() {
                return Err(SinkError::Closed);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Ok(())
    }

    fn discard(&mut self) -> Result<(), SinkError> {
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
