//! Desktop **driver** — the app-side wiring the platform owns (not the portable
//! libraries): a concrete [`CpalSink`], a file-backed track source, and the
//! threads that connect them to the [`Engine`].
//!
//! Two non-real-time threads plus cpal's real-time callback (inside the sink):
//! - **pump** (this thread): owns the [`Engine`] → [`Deck`] → [`CpalSink`] and
//!   loops [`Engine::pump`], paced by the ring's backpressure. It also decides
//!   which track to open next ([`Engine::poll_open`]) and installs opened
//!   decoders ([`Engine::provide`]).
//! - **loader**: opens the requested track off the pump thread (slow I/O) and
//!   sends the owned decoder back. A ~150 KB decoder crosses loader→pump as a
//!   move over a channel — never a decode on the pump thread.
//!
//! The DAP firmware swaps only these pieces (SAI-DMA sink, SD-card source,
//! embassy tasks); the [`Engine`]/[`Deck`] above are identical.

use std::fs::File;
use std::io::BufReader as IoBufReader;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use embedded_io_adapters::std::FromStd;
use soul_audio_core::{AnyDecoder, Deck, Progress, Quality, Sink, open};
use soul_audio_engine::{Engine, Provided};

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

/// Play `paths` gaplessly on the default output device, in order, then stop.
pub fn play(paths: &[PathBuf]) -> Result<()> {
    let sink = CpalSink::new()?;
    let dev = sink.stream_info();
    println!(
        "device: {} Hz {}ch (canonical 24-bit) — {} track(s)",
        dev.sample_rate,
        dev.channels,
        paths.len()
    );

    let deck = Deck::<CpalSink, OwnedDecoder, PRELOAD>::new(sink, Quality::Hq);
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

        // Pump loop (owns the engine, sink, and cpal stream).
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

        // End the loader (drops req_tx → its loop exits), then play out the ring.
        drop(req_tx);
        engine.deck_mut().sink_mut().flush().ok();
        println!("done");
        Ok(())
    })
}
