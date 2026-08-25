//! `soul-audio-cli` — desktop harness that drives `soul-audio-core`.
//!
//! `decode` streams any supported file to interleaved i32 LE PCM. `play` (built
//! with `--features playback`) decodes → resamples to the device rate → plays via
//! cpal, using the core [`Player`](soul_audio_core::Player) + a cpal
//! [`Sink`](soul_audio_core::Sink). The same core path runs on the STM32.

#[cfg(feature = "playback")]
mod cpal_sink;
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
    /// Play an audio file on the default output device (needs `--features playback`).
    Play {
        /// Input file path.
        input: PathBuf,
    },
}

fn main() -> Result<()> {
    // `AnyDecoder` + `Player` embed large fixed buffers by value (no-alloc), which
    // exceeds the default main-thread stack on some platforms (Windows = 1 MiB),
    // so run the work on a thread with a generous stack. On embedded these live
    // in a static/BSS instead.
    let job: Box<dyn FnOnce() -> Result<()> + Send> = match Cli::parse().cmd {
        Cmd::Decode { input, out } => Box::new(move || decode(&input, &out)),
        Cmd::Play { input } => Box::new(move || play(&input)),
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

/// Decode → resample-to-device-rate → play via cpal.
#[cfg(feature = "playback")]
fn play(input: &Path) -> Result<()> {
    use soul_audio_core::{Player, Quality, Sink};

    let file = File::open(input).with_context(|| format!("opening {}", input.display()))?;
    let mut scratch = [0u8; 8192];
    let source = sources::file_source(file, &mut scratch);
    let decoder = open(source).map_err(|e| anyhow!("cannot decode {}: {e:?}", input.display()))?;

    let si = decoder.stream_info();
    let sink = cpal_sink::CpalSink::new(si.bits_per_sample())?;
    let dev = sink.stream_info();
    println!(
        "playing {} — {} Hz {}ch → device {} Hz {}ch",
        input.display(),
        si.sample_rate,
        si.channels,
        dev.sample_rate,
        dev.channels
    );
    let mut player =
        Player::new(decoder, sink, Quality::Hq).map_err(|e| anyhow!("player init: {e:?}"))?;
    player.run().map_err(|e| anyhow!("playback: {e:?}"))?;
    println!("done");
    Ok(())
}

/// Fallback when the `playback` feature is not built.
#[cfg(not(feature = "playback"))]
fn play(_input: &Path) -> Result<()> {
    Err(anyhow!(
        "playback support not built — rebuild with `--features playback`"
    ))
}
