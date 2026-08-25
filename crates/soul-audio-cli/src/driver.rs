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
use std::io::{BufReader as IoBufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use embedded_io_adapters::std::FromStd;
use soul_audio_core::model::SampleFormat;
use soul_audio_core::{AnyDecoder, Deck, Progress, Quality, Sink, SinkError, StreamInfo, open};
use soul_audio_engine::{Engine, Provided};

#[cfg(feature = "playback")]
use crate::cpal_sink::CpalSink;

/// Gapless preload depth. `1` = the next track is primed one ahead (matches the
/// DAP's RAM budget); desktop could afford more.
const PRELOAD: usize = 1;

/// A fully **owned**, `Send` decoder: std buffers the file, so the decoder holds
/// no borrowed scratch and can live in the deck and move across threads.
type OwnedDecoder = AnyDecoder<FromStd<IoBufReader<File>>>;

/// Open `path` into an owned decoder (probes the container, builds the codec).
fn open_track(path: &Path) -> Result<OwnedDecoder> {
    let file = File::open(path).map_err(|e| anyhow!("opening {}: {e}", path.display()))?;
    // 64 KiB of std buffering; the decoder reads through it via embedded-io.
    let source = FromStd::new(IoBufReader::with_capacity(64 * 1024, file));
    open(source).map_err(|e| anyhow!("decoding {}: {e:?}", path.display()))
}

/// Run the gapless pipeline: play `paths` in order into `sink`, then flush.
///
/// Generic over the sink so the same threads/engine drive a live device or a
/// file render. A live sink paces the loop via backpressure; a file sink returns
/// immediately, so it renders faster than real time.
pub fn run<K: Sink>(sink: K, paths: &[PathBuf]) -> Result<()> {
    let info = sink.stream_info();
    println!(
        "output: {} Hz {}ch (canonical 24-bit) — {} track(s)",
        info.sample_rate,
        info.channels,
        paths.len()
    );

    let deck = Deck::<K, OwnedDecoder, PRELOAD>::new(sink, Quality::Hq);
    let mut engine = Engine::new(deck);
    engine.set_queue(paths.len(), 0);

    // pump → loader: "open track i"; loader → pump: the opened decoder (or error).
    let (req_tx, req_rx) = mpsc::channel::<usize>();
    let (res_tx, res_rx) = mpsc::channel::<Result<(usize, OwnedDecoder)>>();

    thread::scope(move |scope| -> Result<()> {
        // Loader thread: serve open requests until the pump drops `req_tx`. A
        // generous stack — an `AnyDecoder` is ~150 KiB by value (no-alloc) and is
        // built and moved here, which overflows the default Windows 1 MiB stack.
        thread::Builder::new()
            .name("loader".into())
            .stack_size(8 * 1024 * 1024)
            .spawn_scoped(scope, move || {
                for i in req_rx {
                    let opened = open_track(&paths[i]).map(|d| (i, d));
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
                    println!("▶ track {} (gapless)", engine.cursor().unwrap_or(0));
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
#[cfg(feature = "playback")]
pub fn play(paths: &[PathBuf]) -> Result<()> {
    run(CpalSink::new()?, paths)
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
pub fn render(paths: &[PathBuf], out: &Path, rate: u32, channels: u16) -> Result<()> {
    run(RenderSink::new(out, rate, channels)?, paths)
}
