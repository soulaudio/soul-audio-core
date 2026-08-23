//! `soul-audio-cli` — desktop harness that drives `soul-audio-core`.
//!
//! TEMPLATE: decoders are stubs, so `decode` currently reports that the format
//! is unimplemented. The wiring (arg parsing → open file → `embedded-io`
//! adapter → core) is complete; fill in the streaming loop once a decoder
//! implements [`soul_audio_core::Decoder`].

use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use embedded_io_adapters::std::FromStd;
use soul_audio_core::formats::wav::WavDecoder;

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
    // Bridge std::io::Read → embedded_io::Read so the same core code runs on
    // desktop and on the STM32 (which uses `SliceReader` instead).
    let reader = FromStd::new(file);

    match WavDecoder::new(reader) {
        Ok(_decoder) => {
            // TODO: stream samples into `out` once the decoder implements
            // `soul_audio_core::Decoder`:
            //
            //   use soul_audio_core::Decoder;
            //   use std::io::{BufWriter, Write};
            //   let mut w = BufWriter::new(File::create(out)?);
            //   let mut buf = [0i32; 1024];
            //   loop {
            //       let n = _decoder.decode_into(&mut buf)?;
            //       if n == 0 { break; }
            //       for &s in &buf[..n] { w.write_all(&s.to_le_bytes())?; }
            //   }
            //   w.flush()?;
            println!("decoder ready; would write PCM to {}", out.display());
            Ok(())
        }
        Err(e) => Err(anyhow!(
            "cannot decode {}: {e:?} — decoders are stubs in this template",
            input.display()
        )),
    }
}
