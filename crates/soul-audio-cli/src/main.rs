//! `soul-audio-cli` — desktop harness that drives `soul-audio-core`.
//!
//! - `decode` streams any supported file to interleaved i32 LE PCM.
//! - `render` runs the full gapless engine pipeline (decode → resample →
//!   normalize → channel-map) over one or more files into a raw i32 LE PCM file —
//!   the same pipeline `play` uses, but to a file, so it is testable without an
//!   audio device.
//! - `play` (built with `--features playback`) plays files **gaplessly** on the
//!   default device via the cpal [`Sink`](soul_audio_core::Sink).
//!
//! The engine/deck run identically on the STM32 — only the sink and source swap.

// `convert` is used by the cpal callback (playback) and by its own tests; when
// playback is off it is exercised only by tests, so allow dead code there.
#[cfg_attr(not(feature = "playback"), allow(dead_code))]
mod convert;
#[cfg(feature = "playback")]
mod cpal_sink;
mod driver;
mod sources;

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use soul_audio_core::{Decoder, open};

#[derive(Parser)]
#[command(name = "soul-audio-cli", about = "soul-audio-core desktop harness")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Decode an audio file to interleaved i32 LE PCM.
    Decode {
        /// Input file path.
        input: PathBuf,
        /// Output .pcm path.
        #[arg(long)]
        out: PathBuf,
    },
    /// Render one or more files gaplessly through the full engine pipeline to a
    /// raw interleaved i32 LE PCM file at the given rate/channels. No device
    /// needed — the offline counterpart of `play`.
    Render {
        /// Input file paths, rendered in order.
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// Output .pcm path.
        #[arg(long)]
        out: PathBuf,
        /// Output sample rate (Hz).
        #[arg(long, default_value_t = 48_000)]
        rate: u32,
        /// Output channel count.
        #[arg(long, default_value_t = 2)]
        channels: u16,
    },
    /// Play one or more audio files gaplessly on the default output device
    /// (needs `--features playback`).
    Play {
        /// Input file paths, played in order.
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
    },
}

fn main() -> Result<()> {
    // `AnyDecoder` + the `Deck` embed large fixed buffers by value (no-alloc),
    // which exceeds the default main-thread stack on some platforms (Windows =
    // 1 MiB), so run the work on a thread with a generous stack. On embedded
    // these live in a static/BSS instead.
    let job: Box<dyn FnOnce() -> Result<()> + Send> = match Cli::parse().cmd {
        Cmd::Decode { input, out } => Box::new(move || decode(&input, &out)),
        Cmd::Render {
            inputs,
            out,
            rate,
            channels,
        } => Box::new(move || driver::render(&inputs, &out, rate, channels)),
        Cmd::Play { inputs } => Box::new(move || play(&inputs)),
    };
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(job)
        .expect("spawn worker thread")
        .join()
        .expect("worker thread panicked")
}

fn decode(input: &Path, out: &Path) -> Result<()> {
    let file = File::open(input).with_context(|| format!("opening {}", input.display()))?;
    // A file is a full-ladder source (Source + BufSource + SeekSource). The same
    // core path runs on the STM32 over a `SliceSource` / SD-card source.
    let mut scratch = [0u8; 8192];
    let source = sources::file_source(file, &mut scratch);

    let mut decoder =
        open(source).map_err(|e| anyhow!("cannot decode {}: {e:?}", input.display()))?;

    // Stream interleaved i32 LE PCM into `out`.
    let mut w =
        BufWriter::new(File::create(out).with_context(|| format!("creating {}", out.display()))?);
    let mut buf = [0i32; 1024];
    loop {
        let n = decoder
            .decode_into(&mut buf)
            .map_err(|e| anyhow!("cannot decode {}: {e:?}", input.display()))?;
        if n == 0 {
            break;
        }
        for &s in &buf[..n] {
            w.write_all(&s.to_le_bytes())?;
        }
    }
    w.flush()?;
    println!("decoded {} → {}", input.display(), out.display());
    Ok(())
}

/// Play one or more files gaplessly via the engine + cpal driver.
#[cfg(feature = "playback")]
fn play(inputs: &[PathBuf]) -> Result<()> {
    driver::play(inputs)
}

/// Fallback when the `playback` feature is not built.
#[cfg(not(feature = "playback"))]
fn play(_inputs: &[PathBuf]) -> Result<()> {
    Err(anyhow!(
        "playback support not built — rebuild with `--features playback`"
    ))
}
