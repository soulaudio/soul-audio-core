//! CoreAudio **exclusive-mode** output (macOS) — bit-perfect playback that keeps the OS
//! mixer out of the signal path. The macOS counterpart of the WASAPI exclusive sink on
//! Windows.
//!
//! This module is the thin **real-hardware adapter**: [`CoreAudioHal`] implements the
//! engine's [`Hal`] trait over `coreaudio-rs`, and [`CoreAudioExclusiveSink`] is
//! [`ExclusiveEngine`]`<CoreAudioHal>`. All decisions (rate / format choice, fallbacks,
//! restore-on-drop, hog ownership) live in the platform-neutral engine, which is
//! test-driven against the fake HAL and recorded device profiles.
//!
//! What "exclusive" means on CoreAudio (the same recipe mpv's `coreaudio_exclusive`,
//! Audirvana, etc. use):
//!
//! 1. **Hog mode** — take `kAudioDevicePropertyHogMode` so no other process can play
//!    through (or mix into) the device while we own it.
//! 2. **Native rate** — switch the device's *nominal sample rate* to the track's rate
//!    (per track, via [`Sink::reopen`]), so nothing resamples. A rate the DAC can't run is
//!    mapped to the closest same-family rate ([`choose_rate`]) and the deck resamples to
//!    that, honestly reported through [`Sink::stream_info`].
//! 3. **Widest physical format** — set the device's *physical* stream format to its most
//!    faithful word at that rate ([`pick_physical_format`]: 32/24-bit integer, then
//!    Float32, then 20/16-bit), so the canonical 24-bit stream reaches the DAC untouched.
//!
//! Audio is rendered through an AUHAL output unit at the device rate. The unit is fed
//! interleaved Float32: a canonical 24-bit sample (`±2^23`) scaled to `±1.0` is *exact* in
//! Float32 (24-bit mantissa), and the HAL's float→integer conversion to the physical
//! word reproduces the original integer — so the path is bit-perfect at unity volume.
//!
//! The device is resolved **once** at [`open`](ExclusiveEngine::open_with) and pinned by
//! id. That matters for the system default: while a device is hogged macOS moves the
//! system-default output to another device, and moves it back on release — a sink that
//! re-resolved "default" (or a host that rebuilt on a default-device change) would chase
//! its own side effect.
//!
//! Everything goes through `coreaudio-rs`'s **safe** `macos_helpers`, so this crate stays
//! `forbid(unsafe_code)`. On drop the device is handed back as we found it: original
//! nominal rate restored, physical format reset to the best one at that rate, hog mode
//! released. (CoreAudio also releases hog mode itself if the process dies.)
#![cfg(all(target_os = "macos", feature = "coreaudio-exclusive"))]

use coreaudio::audio_unit::audio_format::LinearPcmFlags;
use coreaudio::audio_unit::macos_helpers as hal;
use coreaudio::audio_unit::render_callback::{self, data};
use coreaudio::audio_unit::{AudioUnit, SampleFormat as AuFmt, Scope, StreamFormat};
use coreaudio::sys::{
    AudioDeviceID, AudioStreamBasicDescription, kAudioFormatFlagIsFloat,
    kAudioFormatFlagIsSignedInteger, kAudioFormatLinearPCM,
};

pub use crate::exclusive_engine::ExclusiveError;
use crate::exclusive_engine::{DeviceWatch, ExclusiveEngine, Hal, Renderer};
use crate::exclusive_policy::PhysFormat;

/// The shipped macOS exclusive sink.
pub type CoreAudioExclusiveSink = ExclusiveEngine<CoreAudioHal>;

impl CoreAudioExclusiveSink {
    /// Open `name` (or the default output device when `None`) exclusively — hog mode,
    /// nominal rate switched to (the closest supported match for) `rate`, widest physical
    /// format — for `channels`-channel playback.
    pub fn open(name: Option<String>, rate: u32, channels: u16) -> Result<Self, ExclusiveError> {
        Self::open_with(CoreAudioHal, name.as_deref(), rate, channels)
    }

    /// `(client_rate, device_rate)` of the running AUHAL unit — equal ⇔ no hidden SRC.
    pub fn auhal_rates(&self) -> Option<(u32, u32)> {
        self.stream_rates()
    }

    /// The CoreAudio device id this sink hogs.
    pub fn device_id(&self) -> AudioDeviceID {
        self.device()
    }
}

/// The real CoreAudio HAL, via `coreaudio-rs`'s safe helpers.
#[derive(Debug, Clone, Copy, Default)]
pub struct CoreAudioHal;

/// `kAudioDevicePropertyDeviceIsAlive` + `kAudioDevicePropertyNominalSampleRate`
/// listeners. Boxed: CoreAudio keeps a pointer to each, so they must never move once
/// registered.
pub struct CoreAudioWatch {
    alive: Box<hal::AliveListener>,
    rate: Box<hal::RateListener>,
}

impl DeviceWatch for CoreAudioWatch {
    fn is_alive(&self) -> bool {
        self.alive.is_alive()
    }
    fn take_rate_change(&mut self) -> Option<u32> {
        self.rate.drain_values().last().map(|r| r.round() as u32)
    }
}

impl Hal for CoreAudioHal {
    type Device = AudioDeviceID;
    type Stream = AudioUnit;
    type Watch = CoreAudioWatch;

    fn pid(&self) -> i32 {
        std::process::id() as i32
    }

    fn resolve(&self, name: Option<&str>) -> Option<AudioDeviceID> {
        resolve_device(name)
    }

    fn physical_formats(&self, dev: AudioDeviceID) -> Result<Vec<PhysFormat>, String> {
        Ok(raw_physical_formats(dev)?
            .into_iter()
            .map(|(p, _)| p)
            .collect())
    }

    fn nominal_rate(&self, dev: AudioDeviceID) -> Option<u32> {
        device_nominal_rate(dev)
    }

    fn hog_owner(&self, dev: AudioDeviceID) -> Result<i32, String> {
        hal::get_hogging_pid(dev).map_err(|e| format!("{e:?}"))
    }

    fn toggle_hog(&self, dev: AudioDeviceID) -> Result<i32, String> {
        hal::toggle_hog_mode(dev).map_err(|e| format!("{e:?}"))
    }

    fn set_nominal_rate(&self, dev: AudioDeviceID, rate: u32) -> Result<(), String> {
        hal::set_device_sample_rate(dev, f64::from(rate)).map_err(|e| format!("{e:?}"))
    }

    fn set_physical_format(
        &self,
        dev: AudioDeviceID,
        fmt: &PhysFormat,
        rate: u32,
    ) -> Result<(), String> {
        // Re-find the driver's own ASBD for this format (keeps its exact flags / packing /
        // alignment), re-pointed at `rate`.
        let mut asbd = raw_physical_formats(dev)?
            .into_iter()
            .find(|(p, _)| p == fmt)
            .map(|(_, a)| a)
            .ok_or("format no longer offered")?;
        asbd.mSampleRate = f64::from(rate);
        hal::set_device_physical_stream_format(dev, asbd).map_err(|e| format!("{e:?}"))
    }

    fn watch(&self, dev: AudioDeviceID) -> CoreAudioWatch {
        let mut alive = Box::new(hal::AliveListener::new(dev));
        let mut rate = Box::new(hal::RateListener::new(dev, None));
        // A failed registration only costs unplug / rate-drift detection, not playback.
        let _ = alive.register();
        let _ = rate.register();
        CoreAudioWatch { alive, rate }
    }

    fn start(
        &self,
        dev: AudioDeviceID,
        rate: u32,
        channels: u16,
        mut renderer: Renderer,
    ) -> Result<AudioUnit, String> {
        let e = |what: &str, err: coreaudio::Error| format!("{what}: {err:?}");
        let mut unit = hal::audio_unit_from_device_id(dev, false).map_err(|x| e("AUHAL", x))?;
        unit.uninitialize().map_err(|x| e("uninitialize", x))?;
        let fmt = StreamFormat {
            sample_rate: f64::from(rate),
            sample_format: AuFmt::F32,
            flags: LinearPcmFlags::IS_FLOAT | LinearPcmFlags::IS_PACKED,
            channels: u32::from(channels),
        };
        unit.set_stream_format(fmt, Scope::Input)
            .map_err(|x| e("set AUHAL input format", x))?;
        unit.set_render_callback(move |args: render_callback::Args<data::Interleaved<f32>>| {
            renderer.render_f32(args.data.buffer);
            Ok(())
        })
        .map_err(|x| e("set render callback", x))?;
        unit.initialize().map_err(|x| e("initialize AUHAL", x))?;
        unit.start().map_err(|x| e("start AUHAL", x))?;
        Ok(unit)
    }

    fn stream_rates(&self, unit: &AudioUnit) -> Option<(u32, u32)> {
        let client = unit.input_stream_format().ok()?.sample_rate.round() as u32;
        let device = unit.output_stream_format().ok()?.sample_rate.round() as u32;
        Some((client, device))
    }
}

/// Resolve `name` (or the system default output) to a CoreAudio device id.
fn resolve_device(name: Option<&str>) -> Option<AudioDeviceID> {
    match name {
        Some(n) => output_device_ids()
            .into_iter()
            .find(|&id| hal::get_device_name(id).is_ok_and(|d| d == n)),
        None => hal::get_default_device_id(false),
    }
}

/// Every device with an output scope.
fn output_device_ids() -> Vec<AudioDeviceID> {
    hal::get_audio_device_ids()
        .unwrap_or_default()
        .into_iter()
        .filter(|&id| hal::get_audio_device_supports_scope(id, Scope::Output).unwrap_or(false))
        .collect()
}

/// Map a CoreAudio ASBD to the policy's [`PhysFormat`] (`None` for non-LPCM or unsigned
/// formats we never render, e.g. AC-3 passthrough on an HDMI receiver).
pub(crate) fn phys_from_asbd(
    asbd: &AudioStreamBasicDescription,
    rate_min: f64,
    rate_max: f64,
) -> Option<PhysFormat> {
    if asbd.mFormatID != kAudioFormatLinearPCM {
        return None;
    }
    let float = asbd.mFormatFlags & kAudioFormatFlagIsFloat != 0;
    let signed = asbd.mFormatFlags & kAudioFormatFlagIsSignedInteger != 0;
    if float == signed {
        return None; // unsigned int, or a nonsense both-flags format
    }
    // Some drivers report a 0..0 range meaning "only mSampleRate".
    let (lo, hi) = if rate_max >= 1.0 {
        (rate_min, rate_max)
    } else {
        (asbd.mSampleRate, asbd.mSampleRate)
    };
    Some(PhysFormat {
        rate_min: lo.round() as u32,
        rate_max: hi.round() as u32,
        float,
        bits: asbd.mBitsPerChannel,
        channels: asbd.mChannelsPerFrame,
    })
}

/// The device's physical output formats with the driver's raw ASBDs.
fn raw_physical_formats(
    dev: AudioDeviceID,
) -> Result<Vec<(PhysFormat, AudioStreamBasicDescription)>, String> {
    let raw = hal::get_supported_physical_stream_formats(dev).map_err(|e| format!("{e:?}"))?;
    Ok(raw
        .iter()
        .filter_map(|r| {
            phys_from_asbd(
                &r.mFormat,
                r.mSampleRateRange.mMinimum,
                r.mSampleRateRange.mMaximum,
            )
            .map(|p| (p, r.mFormat))
        })
        .collect())
}

/// The device's current nominal sample rate, read through a throwaway AUHAL unit's
/// device-side (output-scope) format.
fn device_nominal_rate(dev: AudioDeviceID) -> Option<u32> {
    let unit = hal::audio_unit_from_device_id(dev, false).ok()?;
    let sf = unit.output_stream_format().ok()?;
    Some(sf.sample_rate.round() as u32)
}

// ---- diagnostics (device report, tests) ----------------------------------------

/// Names of every CoreAudio output device (the same names cpal reports).
pub fn coreaudio_output_device_names() -> Vec<String> {
    output_device_ids()
        .into_iter()
        .filter_map(|id| hal::get_device_name(id).ok())
        .collect()
}

/// Current hog-mode owner of the named (or default) device: `Some(-1)` = free,
/// `Some(pid)` = held, `None` = device not found / no hog-mode property.
pub fn hog_owner(name: Option<&str>) -> Option<i32> {
    hal::get_hogging_pid(resolve_device(name)?).ok()
}

/// Current nominal sample rate of the named (or default) device.
pub fn nominal_rate(name: Option<&str>) -> Option<u32> {
    device_nominal_rate(resolve_device(name)?)
}

/// The physical formats the named (or default) device advertises.
pub fn physical_format_ladder(name: Option<&str>) -> Vec<PhysFormat> {
    resolve_device(name)
        .and_then(|id| CoreAudioHal.physical_formats(id).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    //! Device-free checks of the CoreAudio → policy mapping.
    use super::*;

    fn asbd(flags: u32, bits: u32, rate: f64) -> AudioStreamBasicDescription {
        AudioStreamBasicDescription {
            mSampleRate: rate,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: flags,
            mBytesPerPacket: 8,
            mFramesPerPacket: 1,
            mBytesPerFrame: 8,
            mChannelsPerFrame: 2,
            mBitsPerChannel: bits,
            mReserved: 0,
        }
    }

    #[test]
    fn maps_the_real_device_flag_words() {
        // 0x9 = float|packed (MacBook speakers); 0x4 = signed int (24-in-32 DP monitor);
        // 0xc = signed|packed (16-bit).
        let f = phys_from_asbd(&asbd(0x9, 32, 48_000.0), 48_000.0, 48_000.0).unwrap();
        assert!(f.float && f.bits == 32);
        let i = phys_from_asbd(&asbd(0x4, 24, 44_100.0), 44_100.0, 44_100.0).unwrap();
        assert!(!i.float && i.bits == 24 && i.rate_min == 44_100);
        let s = phys_from_asbd(&asbd(0xc, 16, 32_000.0), 32_000.0, 32_000.0).unwrap();
        assert_eq!((s.float, s.bits), (false, 16));
    }

    #[test]
    fn skips_non_lpcm_and_unsigned() {
        let mut ac3 = asbd(0x4, 16, 48_000.0);
        ac3.mFormatID = u32::from_be_bytes(*b"ac-3");
        assert_eq!(phys_from_asbd(&ac3, 48_000.0, 48_000.0), None);
        assert_eq!(
            phys_from_asbd(&asbd(0x8, 8, 48_000.0), 48_000.0, 48_000.0),
            None
        );
    }

    #[test]
    fn zero_range_falls_back_to_the_format_rate() {
        let p = phys_from_asbd(&asbd(0x4, 24, 96_000.0), 0.0, 0.0).unwrap();
        assert_eq!((p.rate_min, p.rate_max), (96_000, 96_000));
    }
}
