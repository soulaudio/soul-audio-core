//! WASAPI **exclusive-mode** output — bit-perfect playback that bypasses the OS
//! mixer. The device is opened with `AUDCLNT_SHAREMODE_EXCLUSIVE` at the source's
//! native rate (no resample, no shared mixer), event-driven, on a dedicated render
//! thread (the COM objects are not `Send`, so the whole WASAPI client lives there).
//!
//! The [`Deck`](soul_audio_core::Deck) pushes canonical 24-bit PCM through the same
//! SPSC ring + real-time [`Fill`] as [`CpalSink`](crate::CpalSink); the render thread
//! pops from the ring and packs each sample into the device's native format.
//!
//! When the device can't open exclusive at the source format (unsupported, or in use),
//! [`WasapiExclusiveSink::open`] returns a typed [`ExclusiveError`] so the host can fall
//! back to shared mode — never a silent downgrade, never a hard stop (per the design).
#![cfg(all(windows, feature = "wasapi-exclusive"))]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use ringbuf::HeapRb;
use ringbuf::traits::{Observer, Producer, Split};
use soul_audio_core::model::SampleFormat as CoreFmt;
use soul_audio_core::{Sink, SinkError, StreamInfo};
use wasapi::{
    AudioClient, AudioRenderClient, DeviceEnumerator, Direction, Handle, SampleType, StreamMode,
    WaveFormat, initialize_mta,
};

use crate::sink::{CpalRing, DISCARD_FADE_MS, Fill};

/// ~200 ms of buffering, like [`CpalSink`](crate::CpalSink).
const BUFFER_MS: usize = 200;

/// Candidate exclusive formats, widest first: `(store_bits, valid_bits, type)`. The
/// first the device accepts exclusively at the source rate wins — so a 24-bit source
/// on a 24-bit DAC stays 24-bit (bit-perfect), falling to 16-bit only if nothing wider
/// is offered.
const CANDIDATES: &[(usize, usize, SampleType)] = &[
    (32, 32, SampleType::Float),
    (32, 32, SampleType::Int),
    (32, 24, SampleType::Int),
    (24, 24, SampleType::Int),
    (16, 16, SampleType::Int),
];

/// Why opening the device in exclusive mode failed — lets the host fall back to shared.
#[derive(Debug, Clone)]
pub enum ExclusiveError {
    /// No exclusive format matched the source at its rate (bit-perfect impossible here).
    Unsupported,
    /// Another application holds the device exclusively (`AUDCLNT_E_DEVICE_IN_USE`).
    DeviceInUse,
    /// The named (or default) device wasn't found.
    NoDevice,
    /// Any other COM/WASAPI failure (message for diagnostics).
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

/// Interleaved canonical-24 PCM fed to a WASAPI exclusive-mode stream.
pub struct WasapiExclusiveSink {
    prod: <HeapRb<i32> as Split>::Prod,
    flush: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    rate: u32,
    channels: u16,
    format: CoreFmt,
}

/// Handed back from the render thread once the device is open: the ring producer plus
/// the negotiated stream shape.
struct Setup {
    prod: <HeapRb<i32> as Split>::Prod,
    rate: u32,
    channels: u16,
    format: CoreFmt,
}

impl WasapiExclusiveSink {
    /// Open `name` (or the default device when `None`) in exclusive mode at `rate` /
    /// `channels` (the source's), choosing the widest bit depth the device supports.
    /// Returns [`ExclusiveError`] if the device refuses exclusive at that format.
    pub fn open(name: Option<String>, rate: u32, channels: u16) -> Result<Self, ExclusiveError> {
        let flush = Arc::new(AtomicBool::new(false));
        let volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (cf, cv, cs) = (Arc::clone(&flush), Arc::clone(&volume), Arc::clone(&stop));
        let thread = thread::Builder::new()
            .name("wasapi-exclusive".into())
            .spawn(move || render_thread(name, rate, channels, cf, cv, cs, &tx))
            .map_err(|e| ExclusiveError::Other(format!("spawn render thread: {e}")))?;
        match rx.recv() {
            Ok(Ok(s)) => Ok(Self {
                prod: s.prod,
                flush,
                volume,
                stop,
                thread: Some(thread),
                rate: s.rate,
                channels: s.channels,
                format: s.format,
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(ExclusiveError::Other(
                    "render thread exited during setup".into(),
                ))
            }
        }
    }
}

impl Drop for WasapiExclusiveSink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Sink for WasapiExclusiveSink {
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

    fn write(&mut self, frames: &[i32]) -> Result<(), SinkError> {
        let mut off = 0;
        while off < frames.len() {
            off += self.prod.push_slice(&frames[off..]);
            if off < frames.len() {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        while self.prod.occupied_len() > 0 {
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

/// Find an output device by its friendly name, or `None` to fall through to the caller.
fn device_by_name(name: &str) -> Option<wasapi::Device> {
    let enumerator = DeviceEnumerator::new().ok()?;
    let collection = enumerator.get_device_collection(&Direction::Render).ok()?;
    let count = collection.get_nbr_devices().ok()?;
    for i in 0..count {
        if let Ok(dev) = collection.get_device_at_index(i)
            && dev.get_friendlyname().map(|n| n == name).unwrap_or(false)
        {
            return Some(dev);
        }
    }
    None
}

/// Open + configure the exclusive client (all COM, on the render thread). Returns the
/// live client/render/event plus the negotiated format shape.
#[allow(clippy::type_complexity)]
fn open_client(
    name: Option<String>,
    rate: u32,
    channels: u16,
) -> Result<
    (
        AudioClient,
        AudioRenderClient,
        Handle,
        usize,
        SampleType,
        u32,
        u16,
        CoreFmt,
    ),
    ExclusiveError,
> {
    initialize_mta()
        .ok()
        .map_err(|e| ExclusiveError::Other(format!("COM initialize: {e}")))?;
    let device = match name {
        Some(n) => device_by_name(&n).ok_or(ExclusiveError::NoDevice)?,
        None => DeviceEnumerator::new()
            .and_then(|e| e.get_default_device(&Direction::Render))
            .map_err(|_| ExclusiveError::NoDevice)?,
    };
    let mut client = device
        .get_iaudioclient()
        .map_err(|e| ExclusiveError::Other(format!("get client: {e}")))?;

    // Pick the widest exclusive format the device accepts at the source rate.
    let mut picked: Option<WaveFormat> = None;
    for &(store, valid, st) in CANDIDATES {
        let want = WaveFormat::new(store, valid, &st, rate as usize, channels as usize, None);
        if let Ok(ok) = client.is_supported_exclusive_with_quirks(&want) {
            picked = Some(ok);
            break;
        }
    }
    let fmt = picked.ok_or(ExclusiveError::Unsupported)?;

    let (_default_period, min_period) = client
        .get_device_period()
        .map_err(|e| ExclusiveError::Other(format!("device period: {e}")))?;
    let period = client
        .calculate_aligned_period_near(3 * min_period / 2, Some(128), &fmt)
        .map_err(|e| ExclusiveError::Other(format!("align period: {e}")))?;
    client
        .initialize_client(
            &fmt,
            &Direction::Render,
            &StreamMode::EventsExclusive { period_hns: period },
        )
        .map_err(|e| map_init_error(&e))?;

    let h_event = client
        .set_get_eventhandle()
        .map_err(|e| ExclusiveError::Other(format!("event handle: {e}")))?;
    let render = client
        .get_audiorenderclient()
        .map_err(|e| ExclusiveError::Other(format!("render client: {e}")))?;

    // Read the negotiated format back (the quirks probe may have adjusted it).
    let store_bits = fmt.get_bitspersample() as usize;
    let valid_bits = fmt.get_validbitspersample();
    let st = fmt
        .get_subformat()
        .map_err(|e| ExclusiveError::Other(format!("subformat: {e}")))?;
    let dev_rate = fmt.get_samplespersec();
    let dev_channels = fmt.get_nchannels();
    let core_fmt = match st {
        SampleType::Float => CoreFmt::Float { bits: 32 },
        SampleType::Int => CoreFmt::Int {
            bits: valid_bits as u8,
        },
    };
    Ok((
        client,
        render,
        h_event,
        store_bits,
        st,
        dev_rate,
        dev_channels,
        core_fmt,
    ))
}

/// Map a WASAPI initialize error to a typed [`ExclusiveError`] so the host knows whether
/// to fall back (device-in-use) vs. report unsupported.
fn map_init_error(e: &dyn core::fmt::Display) -> ExclusiveError {
    let msg = e.to_string();
    let lower = msg.to_ascii_lowercase();
    if lower.contains("in use") || lower.contains("0x8889000a") {
        ExclusiveError::DeviceInUse
    } else if lower.contains("unsupported") || lower.contains("0x88890008") {
        ExclusiveError::Unsupported
    } else {
        ExclusiveError::Other(msg)
    }
}

/// The render thread: opens the device, hands the ring producer back, then event-driven
/// writes the device's native-format bytes until stopped.
fn render_thread(
    name: Option<String>,
    rate: u32,
    channels: u16,
    flush: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    tx: &mpsc::Sender<Result<Setup, ExclusiveError>>,
) {
    let (client, render, h_event, store, st, dev_rate, dev_channels, core_fmt) =
        match open_client(name, rate, channels) {
            Ok(v) => v,
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        };

    // Now the real shape is known, size the ring (~200 ms) and hand the producer back.
    let cap = ((dev_rate as usize) * (dev_channels as usize) * BUFFER_MS) / 1000;
    let (prod, cons) = HeapRb::<i32>::new(cap.max(1)).split();
    let mut cons = CpalRing(cons);
    let fade_len = ((u64::from(DISCARD_FADE_MS) * u64::from(dev_rate) * u64::from(dev_channels)
        + 500)
        / 1000) as u32;
    let mut fill = Fill::new(fade_len);
    if tx
        .send(Ok(Setup {
            prod,
            rate: dev_rate,
            channels: dev_channels,
            format: core_fmt,
        }))
        .is_err()
    {
        return; // caller dropped
    }

    // Prefill the device buffer, start, then pace by the device event.
    if let Ok(frames) = client.get_available_space_in_frames() {
        write_block(
            &render,
            frames,
            store,
            st,
            dev_channels,
            &mut fill,
            &mut cons,
            &flush,
            &volume,
        );
    }
    if client.start_stream().is_err() {
        return;
    }
    while !stop.load(Ordering::Relaxed) {
        if h_event.wait_for_event(100).is_err() {
            continue; // timeout / spurious — re-check stop
        }
        let frames = client.get_available_space_in_frames().unwrap_or(0);
        if frames > 0 {
            write_block(
                &render,
                frames,
                store,
                st,
                dev_channels,
                &mut fill,
                &mut cons,
                &flush,
                &volume,
            );
        }
    }
    let _ = client.stop_stream();
}

/// Pack one canonical-24 sample (`±2^23`) into `out` (length = the format's
/// bytes-per-sample) in the device's native little-endian format. Device-free — this is
/// the bit-perfect arithmetic, unit-tested without hardware.
fn pack_sample(s: i32, st: SampleType, store_bits: usize, out: &mut [u8]) {
    match (st, store_bits) {
        // IEEE float: canonical peak ±2^23 → ±1.0.
        (SampleType::Float, _) => {
            let f = s as f32 / (1i32 << 23) as f32;
            out[..4].copy_from_slice(&f.to_le_bytes());
        }
        // 24 valid bits packed in 3 bytes — canonical is already 24-bit (low 3 bytes LE).
        (SampleType::Int, 24) => {
            let b = s.to_le_bytes();
            out[..3].copy_from_slice(&b[0..3]);
        }
        // 24-or-32 valid bits in a 32-bit container, left-justified (×2^8).
        (SampleType::Int, 32) => {
            out[..4].copy_from_slice(&s.wrapping_shl(8).to_le_bytes());
        }
        // 16-bit: canonical 24 → 16 (drop the low 8 bits), clamped.
        (SampleType::Int, _) => {
            let v = (s >> 8).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
            out[..2].copy_from_slice(&v.to_le_bytes());
        }
    }
}

/// Fill one device buffer: pop `frames × channels` canonical-24 samples through [`Fill`]
/// (volume + seek fade) and pack each into the device's native sample format.
#[allow(clippy::too_many_arguments)]
fn write_block(
    render: &AudioRenderClient,
    frames: u32,
    store_bits: usize,
    st: SampleType,
    channels: u16,
    fill: &mut Fill,
    cons: &mut CpalRing,
    flush: &Arc<AtomicBool>,
    volume: &Arc<AtomicU32>,
) {
    fill.begin(flush, cons);
    let n = frames as usize * channels as usize;
    let bps = store_bits / 8;
    let mut buf = vec![0u8; n * bps];
    for i in 0..n {
        let target = f32::from_bits(volume.load(Ordering::Relaxed));
        let s = fill.next(cons, flush, target).unwrap_or(0); // canonical-24 i32
        let off = i * bps;
        pack_sample(s, st, store_bits, &mut buf[off..off + bps]);
    }
    let _ = render.write_to_device(frames as usize, &buf, None);
}

#[cfg(test)]
mod tests {
    //! Device-free checks of the bit-perfect sample packing — the exact bytes a DAC
    //! receives, verified without any WASAPI device.
    use super::*;

    #[test]
    fn pack_sample_is_bit_perfect_per_format() {
        // Int 24 packed: canonical IS 24-bit → the low 3 bytes, little-endian.
        let mut b3 = [0u8; 3];
        pack_sample(0x12_34_56, SampleType::Int, 24, &mut b3);
        assert_eq!(b3, [0x56, 0x34, 0x12], "24-bit positive → low 3 bytes LE");
        pack_sample(-1, SampleType::Int, 24, &mut b3);
        assert_eq!(
            b3,
            [0xFF, 0xFF, 0xFF],
            "24-bit −1 → two's-complement 0xFFFFFF"
        );

        // Int 32: 24 bits left-justified in a 32-bit container (×2^8).
        let mut b4 = [0u8; 4];
        pack_sample(0x12_34_56, SampleType::Int, 32, &mut b4);
        assert_eq!(
            i32::from_le_bytes(b4),
            0x12_34_56_00,
            "24→32 left-justified"
        );

        // Int 16: canonical 24 → 16 (drop the low 8 bits).
        let mut b2 = [0u8; 2];
        pack_sample(256_000, SampleType::Int, 16, &mut b2);
        assert_eq!(i16::from_le_bytes(b2), 1000, "24→16 is >>8");
        pack_sample(12_000_000, SampleType::Int, 16, &mut b2);
        assert_eq!(i16::from_le_bytes(b2), i16::MAX, "over-range clamps");

        // Float32: canonical peak ±2^23 → ±1.0.
        let mut bf = [0u8; 4];
        pack_sample(8_388_607, SampleType::Float, 32, &mut bf);
        assert!(
            (f32::from_le_bytes(bf) - 0.999_999_9).abs() < 1e-3,
            "+peak → ~+1.0"
        );
        pack_sample(-8_388_608, SampleType::Float, 32, &mut bf);
        assert!((f32::from_le_bytes(bf) + 1.0).abs() < 1e-6, "−peak → −1.0");
    }
}
