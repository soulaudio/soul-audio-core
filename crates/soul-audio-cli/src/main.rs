//! `soul-audio-cli` — desktop harness that drives `soul-audio-core`.
//!
//! TEMPLATE: leaf decoders are stubs, so `decode` probes + wires the pipeline
//! (arg parsing → open file → `embedded-io` adapter → caller-buffered
//! `BufReader` → `open` → `Decoder`) and then reports that decoding is
//! unimplemented. Fill in the streaming loop once a decoder produces PCM.

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
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Decode { input, out } => decode(&input, &out),
    }
}

fn decode(input: &Path, out: &Path) -> Result<()> {
    let file = File::open(input).with_context(|| format!("opening {}", input.display()))?;
    // A file is a full-ladder source (Source + BufSource + SeekSource). The same
    // core path runs on the STM32 over a `SliceSource` / SD-card source.
    let mut scratch = [0u8; 8192];
    let source = sources::file_source(file, &mut scratch);

    let mut decoder =
        open(source).map_err(|e| anyhow!("cannot decode {}: {e:?}", input.display()))?;

    // Stream interleaved i32 LE PCM into `out`. WAV decodes for real; other
    // formats' codecs are still stubbed and surface a clean `Unsupported` error.
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
