//! ALSA **exclusive-mode** output (Linux) — bit-perfect playback that keeps PipeWire /
//! PulseAudio out of the signal path. The Linux counterpart of the WASAPI sink on
//! Windows and the CoreAudio one on macOS.
//!
//! This module is the thin **real-hardware adapter**: [`AlsaHal`] implements the engine's
//! [`Hal`] trait over the `alsa` crate, and [`AlsaExclusiveSink`] is
//! [`ExclusiveEngine`]`<AlsaHal>`. Every decision (rate / format choice, fallbacks,
//! restore-on-drop, exclusivity) lives in the platform-neutral engine, already
//! test-driven against the fake HAL.
//!
//! What "exclusive" means on Linux:
//!
//! 1. **The card itself, not a plugin.** The device is opened as `hw:CARD,DEV`, which is
//!    the raw hardware PCM. The usual `default` device routes through PipeWire or ALSA's
//!    `dmix`, both of which mix and may resample; `hw:` has neither, so the samples
//!    written are the samples clocked out.
//! 2. **Device reservation.** A desktop's audio server manages that card, so it is
//!    claimed first over D-Bus ([`alsa_reserve`](crate::alsa_reserve)) — the protocol
//!    PipeWire, WirePlumber and PulseAudio all honour. Without it a direct open often
//!    seems to work and then collides with the server the moment it wants the card back.
//! 3. **Rate and format chosen at open.** ALSA has no device-wide "nominal rate" to set;
//!    a PCM is configured when it is opened. So the engine's `set_nominal_rate` /
//!    `set_physical_format` steps *probe* the hardware and record the choice, and
//!    [`Hal::start`] opens the PCM with it. Probing is a real open of the device with
//!    the candidate `hw_params`, so a format the card merely advertises but cannot run is
//!    rejected here rather than failing later.
//!
//! The canonical 24-bit stream is converted to the card's chosen word in the writer
//! thread. At unity volume that conversion is exact for every supported width, so the
//! path is bit-perfect.
//!
//! On drop the stream stops and the reservation is released, which hands the card back
//! to the audio server. The bus releases the name if this process dies, so a crash
//! cannot leave the card stranded.
#![cfg(all(target_os = "linux", feature = "alsa-exclusive"))]

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};

use crate::alsa_reserve::{Reservation, ReserveError};
use crate::exclusive_engine::{
    DeviceWatch, ExclusiveEngine, ExclusiveError, HOG_FREE, Hal, Renderer,
};
use crate::exclusive_policy::PhysFormat;

/// The bit-perfect ALSA sink: the engine driving [`AlsaHal`].
pub type AlsaExclusiveSink = ExclusiveEngine<AlsaHal>;

impl AlsaExclusiveSink {
    /// Open `name` (or the first playback-capable card when `None`) exclusively for
    /// `channels`-channel playback at (the closest rate the card supports to) `rate`.
    ///
    /// `name` is an ALSA hardware device: `"hw:0,0"`, or a card name as
    /// [`output_device_names`](crate::output_device_names) reports it.
    pub fn open(name: Option<String>, rate: u32, channels: u16) -> Result<Self, ExclusiveError> {
        Self::open_with(AlsaHal::default(), name.as_deref(), rate, channels)
    }
}

/// An ALSA hardware PCM: card index and device index within it (`hw:CARD,DEV`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlsaDevice {
    pub card: i32,
    pub device: i32,
}

impl AlsaDevice {
    /// The ALSA PCM name this device opens under.
    #[must_use]
    pub fn pcm_name(&self) -> String {
        format!("hw:{},{}", self.card, self.device)
    }
}

/// Parse an ALSA hardware device name into its card / device indices.
///
/// Accepts `hw:CARD,DEV`, `hw:CARD` (device 0) and a bare `CARD` index. Returns `None`
/// for anything else — notably `default`, `pulse` and `pipewire`, which are plugins
/// rather than hardware and so can never be bit-perfect. Rejecting them here is the
/// point: silently accepting `default` would produce a sink that claims exclusivity
/// while feeding the very mixer this module exists to bypass.
#[must_use]
pub fn parse_hw_name(name: &str) -> Option<AlsaDevice> {
    let rest = name.strip_prefix("hw:").or_else(|| {
        // A bare index is only a device if it parses as one.
        name.chars().all(|c| c.is_ascii_digit()).then_some(name)
    })?;
    let (card, device) = match rest.split_once(',') {
        Some((c, d)) => (c, d.parse().ok()?),
        None => (rest, 0),
    };
    Some(AlsaDevice {
        card: card.parse().ok()?,
        device,
    })
}

/// The ALSA card id out of a plugin device name's `CARD=<id>` token.
///
/// Every name a device list offers is a plugin — `default`, `pipewire`,
/// `sysdefault:CARD=sofhdadsp` — and the hardware ones carry the card in that
/// token. Reading it is what lets a user's chosen device still open bit-perfect:
/// without it only the unnamed default ever resolves, so picking a device in
/// settings would quietly turn exclusive off.
///
/// Note the id is NOT the card's name: `sysdefault:CARD=sofhdadsp` is card id
/// `sofhdadsp`, whose name is `sof-hda-dsp`. Matching on the name would miss.
#[must_use]
pub fn card_token(name: &str) -> Option<&str> {
    let rest = name.split_once("CARD=")?.1;
    let id = rest.split([',', ':']).next()?;
    (!id.is_empty()).then_some(id)
}

/// The ALSA card whose id is `id`, if any.
fn card_by_id(id: &str) -> Option<alsa::card::Card> {
    alsa::card::Iter::new()
        .flatten()
        .find(|c| card_id(c).as_deref() == Some(id))
}

/// A card's short id (`sofhdadsp`), which is what device names reference. Read
/// through the control interface — the `Card` handle itself only exposes the
/// human-readable name and longname.
fn card_id(card: &alsa::card::Card) -> Option<String> {
    let ctl = alsa::ctl::Ctl::from_card(card, false).ok()?;
    let info = ctl.card_info().ok()?;
    info.get_id().ok().map(str::to_owned)
}

/// What the engine asked for, recorded between its probe calls and the real open.
///
/// ALSA configures rate and format when a PCM is opened, but the engine (shaped by
/// CoreAudio, where a device carries them as properties) sets them before starting the
/// stream. Holding the choice here is what bridges the two.
#[derive(Debug, Default, Clone, Copy)]
struct Pending {
    rate: Option<u32>,
    format: Option<PhysFormat>,
}

/// The real-hardware HAL.
#[derive(Default)]
pub struct AlsaHal {
    pending: RefCell<Pending>,
    reservation: RefCell<Option<Reservation>>,
}

/// Formats worth offering the engine, widest first. Only signed-integer words: ALSA
/// exposes float PCMs on some cards, but the canonical stream is 24-bit integer and a
/// DAC's native word is integer, so a float hop would only add a conversion.
const CANDIDATES: [(Format, u32); 3] = [
    (Format::s32(), 32),
    (Format::s24(), 24),
    (Format::s16(), 16),
];

/// Rates to probe. A card advertises a min/max range, but `hw_params` ranges can include
/// rates the hardware will refuse, so each is tested individually.
const PROBE_RATES: [u32; 10] = [
    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000,
];

/// A running stream: the writer thread plus its stop flag.
pub struct AlsaStream {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// What the card actually runs at, for [`Hal::stream_rates`].
    device_rate: u32,
    requested_rate: u32,
}

impl Drop for AlsaStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Liveness watch. ALSA has no device-removal signal to subscribe to, so this polls the
/// card's presence in the same way the engine's other HALs report it.
pub struct AlsaWatch {
    card: i32,
}

impl DeviceWatch for AlsaWatch {
    fn is_alive(&self) -> bool {
        alsa::card::Iter::new()
            .flatten()
            .any(|c| c.get_index() == self.card)
    }

    fn take_rate_change(&mut self) -> Option<u32> {
        // Nothing else can change the rate of a `hw:` PCM we hold open: the rate is
        // fixed for the stream's lifetime, and the reservation keeps the audio server
        // off the card.
        None
    }
}

impl Hal for AlsaHal {
    type Device = AlsaDevice;
    type Stream = AlsaStream;
    type Watch = AlsaWatch;

    fn pid(&self) -> i32 {
        std::process::id() as i32
    }

    fn resolve(&self, name: Option<&str>) -> Option<Self::Device> {
        if let Some(name) = name {
            if let Some(dev) = parse_hw_name(name) {
                return Some(dev);
            }
            // A plugin name carrying a `CARD=<id>` token — what a device list
            // actually offers. Resolve it to that card's raw hardware PCM, which
            // is the whole point: the user picked a device, and we play to it
            // bit-perfect rather than back through the plugin.
            if let Some(dev) = card_token(name).and_then(card_by_id).map(|c| AlsaDevice {
                card: c.get_index(),
                device: 0,
            }) {
                return Some(dev);
            }
            // Finally a human-readable card name or longname.
            return alsa::card::Iter::new()
                .flatten()
                .find(|c| {
                    c.get_name().is_ok_and(|n| n == name)
                        || c.get_longname().is_ok_and(|n| n == name)
                })
                .map(|c| AlsaDevice {
                    card: c.get_index(),
                    device: 0,
                });
        }
        // No name: the first card that HAS a playback device.
        alsa::card::Iter::new()
            .flatten()
            .map(|c| AlsaDevice {
                card: c.get_index(),
                device: 0,
            })
            .find(has_playback)
    }

    fn physical_formats(&self, dev: Self::Device) -> Result<Vec<PhysFormat>, String> {
        let pcm = PCM::new(&dev.pcm_name(), Direction::Playback, false)
            .map_err(|e| format!("open {}: {e}", dev.pcm_name()))?;
        let hwp = HwParams::any(&pcm).map_err(|e| format!("query hw params: {e}"))?;
        let channels = hwp.get_channels_max().unwrap_or(2).max(1);

        let mut out = Vec::new();
        for (fmt, bits) in CANDIDATES {
            if hwp.test_format(fmt).is_err() {
                continue;
            }
            for rate in PROBE_RATES {
                if hwp.test_rate(rate).is_ok() {
                    out.push(PhysFormat {
                        rate_min: rate,
                        rate_max: rate,
                        float: false,
                        bits,
                        channels,
                    });
                }
            }
        }
        if out.is_empty() {
            return Err(format!(
                "{} advertises no usable integer format/rate",
                dev.pcm_name()
            ));
        }
        Ok(out)
    }

    fn nominal_rate(&self, _dev: Self::Device) -> Option<u32> {
        // ALSA keeps no device-wide rate to read or restore: a PCM's rate belongs to the
        // open stream. Reporting `None` tells the engine there is nothing to put back.
        None
    }

    fn hog_owner(&self, _dev: Self::Device) -> Result<i32, String> {
        // Exclusivity here is the D-Bus reservation, which names no pid. Map it onto the
        // engine's pid-shaped question: ours once taken, free otherwise.
        Ok(if self.reservation.borrow().is_some() {
            self.pid()
        } else {
            HOG_FREE
        })
    }

    fn toggle_hog(&self, dev: Self::Device) -> Result<i32, String> {
        let mut held = self.reservation.borrow_mut();
        if held.is_some() {
            *held = None; // dropping releases the name, handing the card back
            return Ok(HOG_FREE);
        }
        match Reservation::acquire(dev.card) {
            Ok(r) => {
                *held = Some(r);
                Ok(self.pid())
            }
            // Another application holds the card. Report a foreign owner so the engine
            // refuses rather than interrupting it; the pid is unknowable over this
            // protocol, and the engine only compares it against our own.
            Err(ReserveError::InUse) => Ok(HOG_FREE.wrapping_sub(1)),
            // No session bus at all (a service, a headless box): nothing manages the
            // card, so there is nothing to negotiate with and a direct open is correct.
            Err(e @ ReserveError::NoBus(_)) => {
                tracing_warn(&format!("{e}; opening the card without a reservation"));
                Ok(self.pid())
            }
        }
    }

    fn set_nominal_rate(&self, dev: Self::Device, rate: u32) -> Result<(), String> {
        probe(dev, rate, None)?;
        self.pending.borrow_mut().rate = Some(rate);
        Ok(())
    }

    fn set_physical_format(
        &self,
        dev: Self::Device,
        fmt: &PhysFormat,
        rate: u32,
    ) -> Result<(), String> {
        probe(dev, rate, Some(fmt))?;
        self.pending.borrow_mut().format = Some(*fmt);
        Ok(())
    }

    fn watch(&self, dev: Self::Device) -> Self::Watch {
        AlsaWatch { card: dev.card }
    }

    fn start(
        &self,
        dev: Self::Device,
        rate: u32,
        channels: u16,
        renderer: Renderer,
    ) -> Result<Self::Stream, String> {
        let fmt = self
            .pending
            .borrow()
            .format
            .ok_or_else(|| "no physical format was accepted before start".to_string())?;
        let alsa_fmt = alsa_format(&fmt)
            .ok_or_else(|| format!("unsupported physical format {}-bit", fmt.bits))?;

        let pcm = PCM::new(&dev.pcm_name(), Direction::Playback, false)
            .map_err(|e| format!("open {}: {e}", dev.pcm_name()))?;
        let device_rate = {
            let hwp = HwParams::any(&pcm).map_err(|e| format!("query hw params: {e}"))?;
            hwp.set_channels(u32::from(channels))
                .map_err(|e| format!("set {channels} channels: {e}"))?;
            hwp.set_rate(rate, ValueOr::Nearest)
                .map_err(|e| format!("set rate {rate}: {e}"))?;
            hwp.set_format(alsa_fmt)
                .map_err(|e| format!("set format {}-bit: {e}", fmt.bits))?;
            hwp.set_access(Access::RWInterleaved)
                .map_err(|e| format!("set interleaved access: {e}"))?;
            pcm.hw_params(&hwp)
                .map_err(|e| format!("apply hw params: {e}"))?;
            hwp.get_rate().map_err(|e| format!("read back rate: {e}"))?
        };

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("alsa-exclusive".into())
            .spawn(move || write_loop(pcm, renderer, channels, fmt.bits, &thread_stop))
            .map_err(|e| format!("spawn the writer thread: {e}"))?;

        Ok(AlsaStream {
            stop,
            thread: Some(thread),
            device_rate,
            requested_rate: rate,
        })
    }

    fn stream_rates(&self, stream: &Self::Stream) -> Option<(u32, u32)> {
        Some((stream.requested_rate, stream.device_rate))
    }
}

/// Whether `dev` is a playback device that exists.
///
/// "Busy" counts as existing. The obvious test — open it and see — conflates a
/// card that is absent with one that is merely in use, and the card is in use
/// exactly when it matters: during our own reopen at a new rate, or while
/// another application has it. Treating that as "no device" made the default
/// device vanish mid-session.
fn has_playback(dev: &AlsaDevice) -> bool {
    match PCM::new(&dev.pcm_name(), Direction::Playback, false) {
        Ok(_) => true,
        // Busy means the device is real, someone is just using it. Mapped through
        // `std::io` rather than comparing a raw errno, so the intent is readable
        // and no platform constant is spelled out by hand.
        Err(e) => {
            std::io::Error::from_raw_os_error(e.errno()).kind() == std::io::ErrorKind::ResourceBusy
        }
    }
}

/// Open `dev` with `rate` (and `fmt` when given) to see whether the hardware really
/// accepts it, then close. A card's advertised `hw_params` ranges are a superset of what
/// it will actually run, so this is the only honest test.
fn probe(dev: AlsaDevice, rate: u32, fmt: Option<&PhysFormat>) -> Result<(), String> {
    let pcm = PCM::new(&dev.pcm_name(), Direction::Playback, false)
        .map_err(|e| format!("open {}: {e}", dev.pcm_name()))?;
    let hwp = HwParams::any(&pcm).map_err(|e| format!("query hw params: {e}"))?;
    hwp.set_rate(rate, ValueOr::Nearest)
        .map_err(|e| format!("rate {rate} refused: {e}"))?;
    if let Some(fmt) = fmt {
        let alsa_fmt =
            alsa_format(fmt).ok_or_else(|| format!("unsupported format {}-bit", fmt.bits))?;
        hwp.set_format(alsa_fmt)
            .map_err(|e| format!("format {}-bit refused: {e}", fmt.bits))?;
    }
    Ok(())
}

/// The ALSA word for a [`PhysFormat`], or `None` for one this module never offers.
fn alsa_format(fmt: &PhysFormat) -> Option<Format> {
    match (fmt.float, fmt.bits) {
        (false, 32) => Some(Format::s32()),
        (false, 24) => Some(Format::s24()),
        (false, 16) => Some(Format::s16()),
        _ => None,
    }
}

/// The writer thread: pull canonical-24 PCM from the engine's renderer, convert it to the
/// card's word, and block on ALSA until the card has taken it.
///
/// Blocking is deliberate — it is what paces playback to the hardware clock. An underrun
/// is recovered rather than fatal: a card that stalls (a suspend, a USB hiccup) should
/// resume, not end the track.
fn write_loop(pcm: PCM, mut renderer: Renderer, channels: u16, bits: u32, stop: &AtomicBool) {
    /// Frames per write. Small enough to keep latency low, large enough that the
    /// per-write syscall cost stays negligible.
    const CHUNK_FRAMES: usize = 512;

    let ch = usize::from(channels);
    let mut scratch = vec![0f32; CHUNK_FRAMES * ch];

    while !stop.load(Ordering::Acquire) {
        renderer.render_f32(&mut scratch);
        let wrote = match bits {
            32 => write_as(&pcm, &scratch, scale_to_i32),
            24 => write_as(&pcm, &scratch, |s| scale_to_i32(s) >> 8),
            _ => write_as_i16(&pcm, &scratch),
        };
        if let Err(e) = wrote {
            // `try_recover` handles XRUN / SUSPENDED; anything else is terminal.
            if pcm.try_recover(e, true).is_err() {
                break;
            }
        }
    }
    let _ = pcm.drain();
}

/// `±1.0` float to a canonical 24-bit sample promoted into the 32-bit word ALSA expects.
/// Exact for every value the deck produces: a canonical sample is an integer in
/// `±2^23`, which a Float32 mantissa holds without loss.
fn scale_to_i32(sample: f32) -> i32 {
    const FULL: f32 = 8_388_608.0; // 2^23
    let clamped = sample.clamp(-1.0, 1.0);
    ((clamped * FULL) as i32).clamp(-8_388_608, 8_388_607) << 8
}

fn write_as(pcm: &PCM, buf: &[f32], map: impl Fn(f32) -> i32) -> Result<(), alsa::Error> {
    let io = pcm.io_i32()?;
    let out: Vec<i32> = buf.iter().copied().map(map).collect();
    io.writei(&out).map(|_| ())
}

fn write_as_i16(pcm: &PCM, buf: &[f32]) -> Result<(), alsa::Error> {
    let io = pcm.io_i16()?;
    let out: Vec<i16> = buf
        .iter()
        .map(|s| (s.clamp(-1.0, 1.0) * 32_767.0) as i16)
        .collect();
    io.writei(&out).map(|_| ())
}

/// Best-effort diagnostic. The crate carries no logging dependency, so this goes to
/// stderr — the reservation falling back is worth seeing when someone asks why output
/// is not bit-perfect.
fn tracing_warn(msg: &str) {
    eprintln!("soul-audio-desktop: {msg}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `hw:` names in every shape ALSA writes them, and — the part that matters —
    /// plugin names REJECTED. Accepting `default` would hand back a device that routes
    /// through the mixer this module exists to bypass, while still reporting exclusive.
    #[test]
    fn parses_hardware_names_and_rejects_plugins() {
        assert_eq!(
            parse_hw_name("hw:0,0"),
            Some(AlsaDevice { card: 0, device: 0 })
        );
        assert_eq!(
            parse_hw_name("hw:2,3"),
            Some(AlsaDevice { card: 2, device: 3 })
        );
        // `hw:N` means device 0 on that card.
        assert_eq!(
            parse_hw_name("hw:1"),
            Some(AlsaDevice { card: 1, device: 0 })
        );
        assert_eq!(parse_hw_name("0"), Some(AlsaDevice { card: 0, device: 0 }));

        for plugin in [
            "default",
            "pulse",
            "pipewire",
            "plughw:0,0",
            "sysdefault",
            "",
        ] {
            assert_eq!(
                parse_hw_name(plugin),
                None,
                "{plugin} is not raw hardware and must not be accepted as exclusive"
            );
        }
    }

    /// Device names as the system actually reports them carry the card in a
    /// `CARD=<id>` token. Without reading it, every name a user can pick in a
    /// device list — they are all plugin names — resolves to nothing, and
    /// exclusive silently falls back to shared the moment someone chooses a
    /// device instead of leaving it on the default.
    #[test]
    fn extracts_the_card_token_from_plugin_names() {
        assert_eq!(card_token("sysdefault:CARD=sofhdadsp"), Some("sofhdadsp"));
        assert_eq!(card_token("front:CARD=PCH,DEV=0"), Some("PCH"));
        assert_eq!(card_token("iec958:CARD=Device,DEV=0"), Some("Device"));
        // Names with no card token at all: these can only mean "the default".
        assert_eq!(card_token("default"), None);
        assert_eq!(card_token("pipewire"), None);
        assert_eq!(card_token("pulse"), None);
        // Raw hardware is `parse_hw_name`'s job, not this one's.
        assert_eq!(card_token("hw:0,0"), None);
    }

    #[test]
    fn pcm_name_round_trips() {
        let dev = AlsaDevice { card: 3, device: 1 };
        assert_eq!(dev.pcm_name(), "hw:3,1");
        assert_eq!(parse_hw_name(&dev.pcm_name()), Some(dev));
    }

    /// Only the integer words this module offers map to an ALSA format; a float
    /// `PhysFormat` must be refused rather than silently opened as something else.
    #[test]
    fn maps_only_the_integer_words_it_offers() {
        let int = |bits| PhysFormat {
            rate_min: 48_000,
            rate_max: 48_000,
            float: false,
            bits,
            channels: 2,
        };
        assert!(alsa_format(&int(32)).is_some());
        assert!(alsa_format(&int(24)).is_some());
        assert!(alsa_format(&int(16)).is_some());
        assert!(alsa_format(&int(20)).is_none(), "20-bit is not offered");

        let float32 = PhysFormat {
            float: true,
            ..int(32)
        };
        assert!(
            alsa_format(&float32).is_none(),
            "float words are not offered"
        );
    }

    /// Unity-gain conversion must be exact: a canonical 24-bit sample has to arrive at
    /// the card as the same integer, left-justified into the 32-bit word. This is the
    /// whole claim of "bit-perfect", so it is checked at the boundaries and at zero.
    #[test]
    fn canonical_samples_convert_exactly() {
        // The deck's canonical full scale is ±2^23; the renderer hands it over as ±1.0.
        assert_eq!(scale_to_i32(0.0), 0);
        assert_eq!(scale_to_i32(1.0), 8_388_607 << 8);
        assert_eq!(scale_to_i32(-1.0), -8_388_608 << 8);

        // A mid-scale value round-trips to its exact integer.
        let half = 4_194_304i32; // 2^22
        let as_float = half as f32 / 8_388_608.0;
        assert_eq!(scale_to_i32(as_float) >> 8, half);
    }

    /// Out-of-range input is clamped, never wrapped: a wrap would turn a loud peak into
    /// full-scale noise of the opposite sign.
    #[test]
    fn out_of_range_input_clamps() {
        assert_eq!(scale_to_i32(2.0), 8_388_607 << 8);
        assert_eq!(scale_to_i32(-2.0), -8_388_608 << 8);
        assert_eq!(scale_to_i32(f32::INFINITY), 8_388_607 << 8);
    }
}
