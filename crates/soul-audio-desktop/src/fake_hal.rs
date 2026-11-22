//! A scriptable, in-memory [`Hal`] — the test double the exclusive engine is developed
//! against. It models a small "audio world" of devices built from [`DeviceProfile`]s
//! (recorded from real hardware, see `tests/fixtures/coreaudio/`), behaves like CoreAudio
//! where it matters (hog-mode toggle semantics, the system default moving off a hogged
//! device, rate/format changes, liveness), and lets a test inject the faults real drivers
//! produce:
//!
//! - hog mode refused by the driver (AirPlay / Bluetooth / aggregate) or held by another pid;
//! - a nominal-rate change refused, or a specific physical format refused / timing out;
//! - the device unplugged mid-play (and its id going stale);
//! - stream start failing.
//!
//! Rendering is **pulled by the test** ([`FakeHal::render`]), so output samples are exact
//! and deterministic — bit-perfect checks need no device and no clock. Every driver call
//! is appended to an ordered [`FakeHal::log`] so tests can assert *sequencing* (e.g. the
//! stream is stopped before the rate changes; hog is released last).
#![cfg(any(test, feature = "test-util"))]

use std::sync::{Arc, Mutex, MutexGuard};

use crate::exclusive_engine::{DeviceWatch, HOG_FREE, Hal, Renderer};
use crate::exclusive_policy::PhysFormat;

/// A device as CoreAudio would describe it, plus how its driver behaves.
#[derive(Debug, Clone)]
pub struct DeviceProfile {
    pub name: String,
    /// `kAudioDevicePropertyTransportType`, informational ("built-in", "usb", "hdmi",
    /// "displayport", "bluetooth", "airplay", "aggregate", "virtual", "thunderbolt").
    pub transport: String,
    pub formats: Vec<PhysFormat>,
    /// Nominal rate at power-on.
    pub nominal_rate: u32,
    /// How the driver handles hog mode.
    pub hog: HogSupport,
    /// The driver refuses to change its nominal rate at all (fixed-rate endpoints).
    pub fixed_rate: bool,
}

/// A driver's hog-mode behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HogSupport {
    /// Normal: toggle takes / releases it.
    Supported,
    /// The property exists but the driver never grants it (stays free).
    Refused,
    /// No hog-mode property at all (aggregate / multi-output devices).
    Absent,
}

/// Faults a test can switch on at any time.
#[derive(Debug, Clone, Default)]
pub struct Faults {
    /// Nominal rates the driver refuses to switch to.
    pub refuse_rates: Vec<u32>,
    /// Physical formats `(float, bits)` the driver refuses (or times out) to switch to.
    pub refuse_formats: Vec<(bool, u32)>,
    /// `start` fails.
    pub fail_start: bool,
}

#[derive(Debug)]
struct Dev {
    /// Rate-change notifications not yet taken by the watch (newest last).
    rate_events: Vec<u32>,
    id: u32,
    profile: DeviceProfile,
    nominal: u32,
    physical: Option<(PhysFormat, u32)>,
    hog: i32,
    alive: bool,
    present: bool,
}

struct World {
    devices: Vec<Dev>,
    default: Option<u32>,
    pid: i32,
    faults: Faults,
    log: Vec<String>,
    /// The active stream's renderer (the "real-time thread" the test drives).
    active: Option<(u64, Renderer, u32, u16)>,
    next_stream: u64,
    /// The device the system default was moved OFF because we hogged it (restored when
    /// that hog is released) — macOS behaviour.
    default_moved_from: Option<u32>,
}

/// The fake HAL. Cheap to clone — clones share one world, so a test keeps a handle to
/// inspect and mutate the world while the engine owns another.
#[derive(Clone)]
pub struct FakeHal(Arc<Mutex<World>>);

impl FakeHal {
    /// A world containing `profiles` (ids 1..), the first one the system default, seen by
    /// a process with pid 4242.
    pub fn new(profiles: Vec<DeviceProfile>) -> Self {
        let devices = profiles
            .into_iter()
            .enumerate()
            .map(|(i, p)| Dev {
                id: i as u32 + 1,
                nominal: p.nominal_rate,
                profile: p,
                rate_events: Vec::new(),
                physical: None,
                hog: HOG_FREE,
                alive: true,
                present: true,
            })
            .collect::<Vec<_>>();
        let default = devices.first().map(|d| d.id);
        Self(Arc::new(Mutex::new(World {
            devices,
            default,
            pid: 4242,
            faults: Faults::default(),
            log: Vec::new(),
            active: None,
            next_stream: 1,
            default_moved_from: None,
        })))
    }

    fn w(&self) -> MutexGuard<'_, World> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn with_dev<T>(&self, name: &str, f: impl FnOnce(&mut Dev) -> T) -> T {
        let mut w = self.w();
        let d = w
            .devices
            .iter_mut()
            .find(|d| d.profile.name == name)
            .unwrap_or_else(|| panic!("no fake device {name:?}"));
        f(d)
    }

    // ---- test controls ----------------------------------------------------------

    /// Replace the active fault set.
    pub fn set_faults(&self, f: Faults) {
        self.w().faults = f;
    }
    /// Another process (pid) takes hog mode on `name`.
    pub fn hog_by(&self, name: &str, pid: i32) {
        self.with_dev(name, |d| d.hog = pid);
    }
    /// That process exits: CoreAudio frees its hog.
    pub fn release_foreign_hog(&self, name: &str) {
        self.with_dev(name, |d| d.hog = HOG_FREE);
    }
    /// Something other than us switches `name`'s nominal rate (e.g. AirPods dropping to
    /// SCO 16 kHz when the mic opens). Rendering continues at the wrong rate until the
    /// engine notices.
    pub fn external_rate_change(&self, name: &str, rate: u32) {
        self.with_dev(name, |d| {
            d.nominal = rate;
            d.rate_events.push(rate);
        });
        self.w().log.push(format!("external rate {name} {rate}"));
    }
    /// The device has NO hog-mode property (aggregate / multi-output devices).
    fn hog_absent(d: &Dev) -> bool {
        d.profile.hog == HogSupport::Absent
    }
    /// Unplug `name`: it stops being alive and disappears from enumeration.
    pub fn unplug(&self, name: &str) {
        let mut w = self.w();
        let id = {
            let d = w
                .devices
                .iter_mut()
                .find(|d| d.profile.name == name)
                .expect("device");
            d.alive = false;
            d.present = false;
            d.hog = HOG_FREE;
            d.id
        };
        if w.default == Some(id) {
            w.default = w.devices.iter().find(|d| d.present).map(|d| d.id);
        }
        w.log.push(format!("unplug {name}"));
    }

    // ---- observations -----------------------------------------------------------

    /// Ordered log of every driver call.
    pub fn log(&self) -> Vec<String> {
        self.w().log.clone()
    }
    pub fn clear_log(&self) {
        self.w().log.clear();
    }
    /// This process's pid in the fake world.
    pub fn our_pid(&self) -> i32 {
        self.w().pid
    }
    pub fn nominal(&self, name: &str) -> u32 {
        self.with_dev(name, |d| d.nominal)
    }
    pub fn hog(&self, name: &str) -> i32 {
        self.with_dev(name, |d| d.hog)
    }
    /// The physical format currently set (and the rate it was set at), if we set one.
    pub fn physical(&self, name: &str) -> Option<(PhysFormat, u32)> {
        self.with_dev(name, |d| d.physical)
    }
    /// Name of the current system-default output.
    pub fn default_name(&self) -> Option<String> {
        let w = self.w();
        let id = w.default?;
        w.devices
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.profile.name.clone())
    }
    /// Whether a stream is currently running.
    pub fn streaming(&self) -> Option<(u32, u16)> {
        self.w().active.as_ref().map(|(_, _, r, c)| (*r, *c))
    }

    /// Pull `frames` frames from the running stream, exactly as the driver's real-time
    /// thread would. Empty if no stream runs.
    pub fn render(&self, frames: usize) -> Vec<f32> {
        let mut w = self.w();
        let Some((_, r, _, ch)) = w.active.as_mut() else {
            return Vec::new();
        };
        let mut out = vec![0.0f32; frames * *ch as usize];
        r.render_f32(&mut out);
        out
    }

    /// Re-elect the system default the way macOS does around hog mode: a hogged device
    /// can't be the default, so the default moves to the first other present device; when
    /// that hog is released, the default moves back.
    fn reelect_default(w: &mut World, dev: u32, now_hogged: bool) {
        if now_hogged && w.default == Some(dev) {
            if let Some(other) = w.devices.iter().find(|d| d.present && d.id != dev) {
                w.default = Some(other.id);
                w.default_moved_from = Some(dev);
            }
        } else if !now_hogged && w.default_moved_from == Some(dev) {
            w.default = Some(dev);
            w.default_moved_from = None;
        }
    }
}

/// Live view of one fake device.
pub struct FakeWatch(FakeHal, u32);

impl DeviceWatch for FakeWatch {
    fn is_alive(&self) -> bool {
        let w = self.0.w();
        w.devices.iter().any(|d| d.id == self.1 && d.alive)
    }
    fn take_rate_change(&mut self) -> Option<u32> {
        let mut w = self.0.w();
        let d = w.devices.iter_mut().find(|d| d.id == self.1)?;
        let last = d.rate_events.last().copied();
        d.rate_events.clear();
        last
    }
}

/// A running fake stream; dropping it stops "rendering".
pub struct FakeStream {
    hal: FakeHal,
    id: u64,
    rate: u32,
}

impl Drop for FakeStream {
    fn drop(&mut self) {
        let mut w = self.hal.w();
        if w.active.as_ref().map(|a| a.0) == Some(self.id) {
            w.active = None;
        }
        w.log.push(format!("stop {}", self.rate));
    }
}

/// A present device by id (an unplugged device's id is a stale "bad object").
fn find(w: &mut World, id: u32) -> Option<&mut Dev> {
    w.devices.iter_mut().find(|d| d.id == id && d.present)
}

impl Hal for FakeHal {
    type Device = u32;
    type Stream = FakeStream;
    type Watch = FakeWatch;

    fn pid(&self) -> i32 {
        self.w().pid
    }

    fn resolve(&self, name: Option<&str>) -> Option<u32> {
        let w = self.w();
        match name {
            Some(n) => w
                .devices
                .iter()
                .find(|d| d.present && d.profile.name == n)
                .map(|d| d.id),
            None => w.default,
        }
    }

    fn physical_formats(&self, dev: u32) -> Result<Vec<PhysFormat>, String> {
        let mut w = self.w();
        let d = find(&mut w, dev).ok_or("bad object")?;
        Ok(d.profile.formats.clone())
    }

    fn nominal_rate(&self, dev: u32) -> Option<u32> {
        let mut w = self.w();
        find(&mut w, dev).map(|d| d.nominal)
    }

    fn hog_owner(&self, dev: u32) -> Result<i32, String> {
        let mut w = self.w();
        let d = find(&mut w, dev).ok_or("bad object")?;
        if FakeHal::hog_absent(d) {
            return Err("unknown property 'oink'".into());
        }
        Ok(d.hog)
    }

    fn toggle_hog(&self, dev: u32) -> Result<i32, String> {
        let mut w = self.w();
        let pid = w.pid;
        let d = find(&mut w, dev).ok_or("bad object")?;
        if FakeHal::hog_absent(d) {
            return Err("unknown property 'oink'".into());
        }
        if d.profile.hog == HogSupport::Refused {
            let name = d.profile.name.clone();
            w.log.push(format!("hog refused {name}"));
            return Ok(HOG_FREE);
        }
        let (now, name) = if d.hog == HOG_FREE {
            d.hog = pid;
            (pid, d.profile.name.clone())
        } else if d.hog == pid {
            d.hog = HOG_FREE;
            (HOG_FREE, d.profile.name.clone())
        } else {
            (d.hog, d.profile.name.clone())
        };
        w.log.push(format!("hog {name} -> {now}"));
        // macOS moves the system default off a hogged device and back on release.
        FakeHal::reelect_default(&mut w, dev, now == pid);
        Ok(now)
    }

    fn set_nominal_rate(&self, dev: u32, rate: u32) -> Result<(), String> {
        let mut w = self.w();
        let refused = w.faults.refuse_rates.contains(&rate);
        let d = find(&mut w, dev).ok_or("bad object")?;
        let supported = d.profile.formats.iter().any(|f| f.supports_rate(rate));
        if refused || !supported || (d.profile.fixed_rate && rate != d.nominal) {
            let name = d.profile.name.clone();
            w.log.push(format!("rate refused {name} {rate}"));
            return Err(format!("unsupported sample rate {rate}"));
        }
        if d.nominal != rate {
            d.rate_events.push(rate);
        }
        d.nominal = rate;
        let name = d.profile.name.clone();
        w.log.push(format!("rate {name} {rate}"));
        Ok(())
    }

    fn set_physical_format(&self, dev: u32, fmt: &PhysFormat, rate: u32) -> Result<(), String> {
        let mut w = self.w();
        let refused = w.faults.refuse_formats.contains(&(fmt.float, fmt.bits));
        let d = find(&mut w, dev).ok_or("bad object")?;
        let name = d.profile.name.clone();
        if refused || !d.profile.formats.contains(fmt) || !fmt.supports_rate(rate) {
            w.log.push(format!(
                "format refused {name} {}{}@{rate}",
                if fmt.float { "f" } else { "i" },
                fmt.bits
            ));
            return Err("format change timed out".into());
        }
        d.physical = Some((*fmt, rate));
        d.nominal = rate;
        w.log.push(format!(
            "format {name} {}{}@{rate}",
            if fmt.float { "f" } else { "i" },
            fmt.bits
        ));
        Ok(())
    }

    fn watch(&self, dev: u32) -> FakeWatch {
        FakeWatch(self.clone(), dev)
    }

    fn start(
        &self,
        dev: u32,
        rate: u32,
        channels: u16,
        renderer: Renderer,
    ) -> Result<FakeStream, String> {
        let mut w = self.w();
        if w.faults.fail_start {
            w.log.push("start failed".into());
            return Err("AUHAL start failed".into());
        }
        let d = find(&mut w, dev).ok_or("bad object")?;
        if !d.alive {
            return Err("device not alive".into());
        }
        if d.nominal != rate {
            // The engine must set the hardware rate BEFORE starting a stream at it — a
            // mismatch here is exactly the hidden-SRC bug the fake exists to catch.
            return Err(format!(
                "stream at {rate} but device nominal is {} (would resample)",
                d.nominal
            ));
        }
        let id = w.next_stream;
        w.next_stream += 1;
        w.active = Some((id, renderer, rate, channels));
        w.log.push(format!("start {rate}"));
        Ok(FakeStream {
            hal: self.clone(),
            id,
            rate,
        })
    }

    fn stream_rates(&self, s: &FakeStream) -> Option<(u32, u32)> {
        let w = self.w();
        let dev_rate = w
            .devices
            .iter()
            .find(|d| d.alive && d.physical.is_some_and(|(_, r)| r == s.rate))
            .map(|d| d.nominal)?;
        Some((s.rate, dev_rate))
    }
}
