//! Desktop **driver** — the app-side wiring the platform owns (not the portable
//! libraries): a concrete [`Sink`], a file-backed track source, and the threads
//! that connect them to the [`Engine`].
//!
//! Two non-real-time threads (plus, for a live device, the sink's real-time
//! callback):
//! - **pump** (this thread): owns the [`Engine`] → [`Deck`] → sink and loops
//!   [`Engine::pump`], paced by the sink's backpressure. It decides which track
//!   to open next ([`Engine::poll_open`]) and installs opened decoders
//!   ([`Engine::provide`]).
//! - **loader**: opens the requested track off the pump thread (slow I/O) and
//!   sends the owned decoder back. A ~150 KB decoder crosses loader→pump as a
//!   move over a channel — never a decode on the pump thread.
//!
//! The pipeline ([`run`]) is generic over the [`Sink`], so the same code drives
//! a live cpal device ([`play`]) or renders to a file ([`render`]) — the latter
//! makes the whole pipeline testable end-to-end without audio hardware. The DAP
//! firmware swaps the same seam (SAI-DMA sink, SD-card source, embassy tasks);
//! the [`Engine`]/[`Deck`] above are identical.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use soul_audio_core::model::{DsdMode, SampleFormat};
use soul_audio_core::{Deck, Decoder, Progress, Quality, Sink, SinkError, StreamInfo};
use soul_audio_desktop::{OwnedDecoder, open_track_dsd};
use soul_audio_engine::{Engine, LinearSequencer, Provided};

#[cfg(feature = "playback")]
use soul_audio_desktop::CpalSink;

/// Gapless preload depth. `1` = the next track is primed one ahead (matches the
/// DAP's RAM budget); desktop could afford more.
const PRELOAD: usize = 1;

/// Run the gapless pipeline: play `paths` in order into `sink`, then flush.
///
/// Generic over the sink so the same threads/engine drive a live device or a
/// file render. A live sink paces the loop via backpressure; a file sink returns
/// immediately, so it renders faster than real time.
///
/// `dsd` selects how a **DSD** source is delivered — see [`open_track_dsd`].
/// Every non-DSD input ignores it. [`DsdMode::Dop`] only reaches a DAC intact if
/// `sink` is opened at the carrier rate, exclusively, at ≥ 24 bits, with no
/// gain, resampling or DSP; [`render`] enforces the rate/channel half of that,
/// and [`play`] refuses the mode outright because a shared device cannot.
pub fn run<K: Sink>(sink: K, paths: &[PathBuf], dsd: DsdMode) -> Result<()> {
    let info = sink.stream_info();
    println!(
        "output: {} Hz {}ch (canonical 24-bit) — {} track(s)",
        info.sample_rate,
        info.channels,
        paths.len()
    );

    let deck = Deck::<K, OwnedDecoder, PRELOAD>::new(sink, Quality::Hq);
    let mut engine = Engine::new(deck, LinearSequencer::new());
    engine.sequencer_mut().set_queue(paths.len(), 0);
    engine.rearm();

    // pump → loader: "open track i"; loader → pump: the opened decoder (or error).
    let (req_tx, req_rx) = mpsc::channel::<usize>();
    let (res_tx, res_rx) = mpsc::channel::<Result<(usize, OwnedDecoder)>>();

    thread::scope(move |scope| -> Result<()> {
        // Loader thread: serve open requests until the pump drops `req_tx`. A
        // generous stack — `OwnedDecoder` is large by value (no-alloc) and is
        // built and moved here (and again into the `(i, d)` tuple / `Result`
        // sent to the pump). The desktop-profile Vorbis variant is the
        // largest (~350 KiB — see `OwnedDecoder`'s doc comment), and a debug
        // build's unoptimized moves keep several such copies live across the
        // call chain; 64 MiB (up from 16, which overflowed once Vorbis raised
        // the channel cap from 2 to 4) leaves a comfortable margin.
        thread::Builder::new()
            .name("loader".into())
            .stack_size(64 * 1024 * 1024)
            .spawn_scoped(scope, move || {
                for i in req_rx {
                    let opened = open_track_dsd(&paths[i], dsd).map(|d| (i, d));
                    if res_tx.send(opened).is_err() {
                        break; // pump thread gone
                    }
                }
            })
            .expect("spawn loader thread");

        // Pump loop (owns the engine and sink).
        loop {
            // 1. Install any decoders the loader has finished opening.
            loop {
                match res_rx.try_recv() {
                    Ok(Ok((i, decoder))) => {
                        if engine.provide(i, decoder)? == Provided::Loaded {
                            println!("▶ track {i}");
                        }
                    }
                    Ok(Err(e)) => return Err(e), // unreadable file → surface + stop
                    Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => break,
                }
            }
            // 2. Ask the loader to open whatever the engine now wants next.
            while let Some(i) = engine.poll_open() {
                req_tx
                    .send(i)
                    .map_err(|_| anyhow!("loader thread stopped unexpectedly"))?;
            }
            // 3. Produce one block; backpressure in the sink paces this loop.
            match engine.pump()? {
                Progress::Ended => break,
                // A preloaded track took over at a gapless boundary.
                Progress::TrackChanged => {
                    println!("▶ track {} (gapless)", engine.current().unwrap_or(0));
                }
                // Waiting for the first/next decoder to arrive — yield briefly.
                Progress::Idle => thread::sleep(Duration::from_millis(3)),
                Progress::Played => {}
            }
        }

        // End the loader (drops req_tx → its loop exits), then flush the sink.
        drop(req_tx);
        engine
            .deck_mut()
            .sink_mut()
            .flush()
            .map_err(|e| anyhow!("sink flush: {e}"))?;
        println!("done");
        Ok(())
    })
}

/// Play `paths` gaplessly on the default output device, in order, then stop.
///
/// Always [`DsdMode::Decimate`]: this is cpal's **shared** device at its own
/// default config (f32 or i16 — see `CpalSink::build`), so the OS mixer rescales
/// every sample. DoP's marker byte is the top byte of the word, so a rescale
/// turns the stream into full-level noise into someone's headphones. Delivering
/// DoP needs an exclusive, ≥ 24-bit, no-DSP device path (`soul-audio-desktop`'s
/// `alsa_exclusive`/`wasapi_exclusive`/`coreaudio_exclusive` engines); until
/// this harness opens one, `render --dsd dop` is the way to verify the framer.
#[cfg(feature = "playback")]
pub fn play(paths: &[PathBuf]) -> Result<()> {
    run(CpalSink::new()?, paths, DsdMode::Decimate)
}

/// A [`Sink`] that renders the pipeline's canonical 24-bit output to a file as
/// raw interleaved `i32` little-endian PCM (one `i32` per sample, value in
/// `±2^23`). No backpressure — it renders as fast as the pump can decode, and it
/// makes the whole pipeline assertable end-to-end without an audio device.
pub struct RenderSink {
    writer: BufWriter<File>,
    rate: u32,
    channels: u16,
}

impl RenderSink {
    /// Create a render sink writing to `out` at `rate`/`channels`.
    fn new(out: &Path, rate: u32, channels: u16) -> Result<Self> {
        let file = File::create(out).map_err(|e| anyhow!("creating {}: {e}", out.display()))?;
        Ok(Self {
            writer: BufWriter::new(file),
            rate,
            channels,
        })
    }
}

impl Sink for RenderSink {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: self.rate,
            channels: self.channels,
            format: SampleFormat::Int { bits: 24 },
        }
    }

    fn write(&mut self, frames: &[i32]) -> Result<(), SinkError> {
        for &s in frames {
            self.writer
                .write_all(&s.to_le_bytes())
                .map_err(|_| SinkError::Closed)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        self.writer.flush().map_err(|_| SinkError::Closed)
    }
}

/// Render `paths` gaplessly to `out` (raw interleaved `i32` LE PCM) at
/// `rate`/`channels`, resampling and channel-mapping through the same pipeline
/// [`play`] uses. Runs without an audio device.
///
/// `rate`/`channels` are `None` when the caller did not ask for a specific
/// output format, which is what lets `--dsd dop` pick the carrier format the
/// stream requires instead of silently resampling it (see [`dop_format`]).
pub fn render(
    paths: &[PathBuf],
    out: &Path,
    rate: Option<u32>,
    channels: Option<u16>,
    dsd: DsdMode,
) -> Result<()> {
    let (rate, channels) = match dsd {
        DsdMode::Dop => dop_format(paths, rate, channels)?,
        _ => (
            rate.unwrap_or(DEFAULT_RENDER_RATE),
            channels.unwrap_or(DEFAULT_RENDER_CHANNELS),
        ),
    };
    run(RenderSink::new(out, rate, channels)?, paths, dsd)
}

/// Default render format when the caller names neither (`48 kHz` stereo).
const DEFAULT_RENDER_RATE: u32 = 48_000;
const DEFAULT_RENDER_CHANNELS: u16 = 2;

/// Resolve the output format for a `--dsd dop` render.
///
/// DoP survives only if nothing touches the samples, and the deck's resampler
/// and downmixer both do: a DoP stream rendered at any rate but its own
/// `dsd_rate / 16` carrier — or to any channel count but its own — arrives as
/// noise, every word's marker byte interpolated away. So the format is taken
/// **from the stream**, and an explicit `--rate`/`--channels` that contradicts
/// it is an error rather than a silently-ruined render.
///
/// Inputs that are not DSD (DoP is a no-op for them) do not constrain anything;
/// if none of the inputs is DSD this is just [`render`]'s normal default. Two
/// DSD inputs at different rates cannot both pass through one sink, so that is
/// an error too — the second would be resampled.
fn dop_format(
    paths: &[PathBuf],
    want_rate: Option<u32>,
    want_channels: Option<u16>,
) -> Result<(u32, u16)> {
    let mut carrier: Option<(u32, u16, &Path)> = None;
    for path in paths {
        let decoder = open_track_dsd(path, DsdMode::Dop)?;
        if !decoder.dsd_passthrough() {
            continue; // not DSD: nothing to carry
        }
        let info = decoder.stream_info();
        match carrier {
            None => carrier = Some((info.sample_rate, info.channels, path)),
            Some((rate, channels, first))
                if (rate, channels) != (info.sample_rate, info.channels) =>
            {
                return Err(anyhow!(
                    "cannot render these DSD files into one DoP stream: {} carries \
                     {} Hz {}ch but {} carries {} Hz {}ch — one render has a single \
                     output format, and resampling a DoP carrier destroys it",
                    first.display(),
                    rate,
                    channels,
                    path.display(),
                    info.sample_rate,
                    info.channels,
                ));
            }
            Some(_) => {}
        }
    }
    let Some((rate, channels, path)) = carrier else {
        // No DSD input — `--dsd dop` changed nothing, so neither does the format.
        return Ok((
            want_rate.unwrap_or(DEFAULT_RENDER_RATE),
            want_channels.unwrap_or(DEFAULT_RENDER_CHANNELS),
        ));
    };
    if let Some(want) = want_rate.filter(|w| *w != rate) {
        return Err(anyhow!(
            "--rate {want} would resample {}'s DoP carrier ({rate} Hz) and destroy it; \
             pass --rate {rate} or drop the flag",
            path.display(),
        ));
    }
    if let Some(want) = want_channels.filter(|w| *w != channels) {
        return Err(anyhow!(
            "--channels {want} would remix {}'s DoP carrier ({channels}ch) and destroy it; \
             pass --channels {channels} or drop the flag",
            path.display(),
        ));
    }
    println!(
        "DSD: DoP passthrough — carrier {rate} Hz {channels}ch, 24-bit. A device playing \
         this must be exclusive, ≥24-bit, with no gain, resampling or DSP."
    );
    Ok((rate, channels))
}
