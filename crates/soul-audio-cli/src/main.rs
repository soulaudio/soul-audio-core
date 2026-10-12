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

mod driver;

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use soul_audio_core::demux::MatroskaDemuxer;
use soul_audio_core::formats::matroska::{MatroskaDecoder, MatroskaVorbisOgg};
use soul_audio_core::formats::vorbis::VorbisDecoder;
use soul_audio_core::{AnyDecoder, DecodeError, Decoder, Format, open, probe};

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
    // Generous stack: the worker owns the engine/deck, which embed an
    // `OwnedDecoder` by value (no-alloc) — and moves it through several call
    // frames (`Engine::provide` → `Deck::load`/`preload` → `Track::new`) that,
    // unoptimized (debug build), each keep their own copy rather than eliding
    // the move. The desktop-profile `Vorbis` variant is by far the largest
    // (~350 KiB: a 4-channel, 8192-sample-block DSP scratch — see
    // `soul_audio_desktop::OwnedDecoder`'s doc comment) — several such copies
    // overflowed a 16 MiB stack (the previous size, sized for `AnyDecoder`'s
    // ~180 KiB HE-AAC variant); 64 MiB leaves a comfortable margin.
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
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
    let mut source = soul_audio_desktop::file_source(file, &mut scratch);

    // Stream interleaved i32 LE PCM into `out`.
    let mut w =
        BufWriter::new(File::create(out).with_context(|| format!("creating {}", out.display()))?);

    // Vorbis needs a caller-provided arena (`VorbisDecoder<S, A>`), so it is
    // not an `AnyDecoder` variant `open` can build — special-cased here,
    // mirroring `soul_audio_desktop::open_track`'s DSF/DFF/Vorbis split.
    // `probe` consumes nothing (it peeks the buffered prefix), so falling
    // through to `open`'s own internal probe for every other outcome —
    // including a probe failure — reproduces the exact original dispatch and
    // error message for every non-Vorbis input, unchanged.
    let fail = |e| anyhow!("cannot decode {}: {e:?}", input.display());
    match probe(&mut source) {
        Ok(Format::Vorbis) => {
            let mut decoder =
                VorbisDecoder::new(source, soul_audio_desktop::vorbis_arena()).map_err(fail)?;
            stream_pcm(&mut decoder, &mut w).map_err(fail)?;
        }
        // Matroska: `A_VORBIS` needs the arena (consumer-dispatched like bare
        // Vorbis), so peek the track codec and route it over the Ogg-synthesizing
        // adapter; Opus/FLAC/AAC build from the same demuxer via `AnyDecoder`.
        Ok(Format::Matroska) => {
            let demux = MatroskaDemuxer::new(source).map_err(fail)?;
            if demux.codec_id() == b"A_VORBIS" {
                let adapter = MatroskaVorbisOgg::new(demux).map_err(fail)?;
                let mut decoder = VorbisDecoder::new(adapter, soul_audio_desktop::vorbis_arena())
                    .map_err(fail)?;
                stream_pcm(&mut decoder, &mut w).map_err(fail)?;
            } else {
                let mut decoder =
                    AnyDecoder::Matroska(MatroskaDecoder::from_demuxer(demux).map_err(fail)?);
                stream_pcm(&mut decoder, &mut w).map_err(fail)?;
            }
        }
        _ => {
            let mut decoder = open(source).map_err(fail)?;
            stream_pcm(&mut decoder, &mut w).map_err(fail)?;
        }
    }
    w.flush()?;
    println!("decoded {} → {}", input.display(), out.display());
    Ok(())
}

/// Stream `decoder`'s full remaining output as interleaved i32 LE PCM into
/// `w`. Shared by every branch of [`decode`] so the two decoder types
/// (`AnyDecoder` vs the arena-backed `VorbisDecoder`) drain identically.
fn stream_pcm(decoder: &mut dyn Decoder, w: &mut BufWriter<File>) -> Result<(), DecodeError> {
    let mut buf = [0i32; 1024];
    loop {
        let n = decoder.decode_into(&mut buf)?;
        if n == 0 {
            break;
        }
        for &s in &buf[..n] {
            w.write_all(&s.to_le_bytes()).map_err(|_| DecodeError::Io)?;
        }
    }
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
