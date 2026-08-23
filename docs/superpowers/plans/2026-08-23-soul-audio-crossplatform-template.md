# soul-audio-core Cross-Platform Template Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up a modern Rust workspace where a strict-`no_std`, no-alloc audio library (`soul-audio-core`) builds and runs on both an STM32H743 (Embassy firmware) and a desktop host (CLI), with layered enforcement that the STM32 build can never silently regress.

**Architecture:** Single Cargo workspace (Rust 2024, resolver 3). A pure sync `no_std`/no-alloc core library defines the decode boundary over `embedded-io::Read`. Thin platform shells consume it: an Embassy STM32 firmware binary and a `std` CLI test harness. A dedicated `architecture-tests` crate plus an `xtask` runner and GitHub Actions enforce the no_std/no-alloc/layering invariants.

**Tech Stack:** Rust 2024 edition, `embedded-io`, Embassy (`embassy-executor`, `embassy-stm32`, `embassy-time`), `defmt`/`defmt-rtt`/`panic-probe`, `probe-rs`, `clap`, `assert_cmd`, `trybuild`, `cargo_metadata`.

## Global Constraints

- **Edition:** Rust 2024 for every crate (`edition = "2024"`). Workspace `resolver = "3"`.
- **Core purity:** `soul-audio-core` is `#![cfg_attr(not(test), no_std)]` and **allocates nothing** in library code (no `Vec`/`Box`/`String`/heap). It has `#![forbid(unsafe_code)]`.
- **Core deps allowlist:** `soul-audio-core`'s only normal dependency is `embedded-io`. It must never depend on `soul-audio-cli` or `soul-audio-firmware`.
- **STM32 target:** `thumbv7em-none-eabihf`. Chip: STM32H743ZI (`embassy-stm32` feature `stm32h743zi`; probe-rs chip `STM32H743ZITx`).
- **default-members** excludes `soul-audio-firmware`; firmware builds only via explicit `--target`.
- **Lints:** workspace `clippy::all = "warn"`; the verify/CI clippy gate runs `-D warnings` over default-members (host) only.
- **Sample model:** normalized `i32` container; `StreamInfo` carries `bits_per_sample`. (No separate `SampleFormat` enum — YAGNI; deviates intentionally from the spec's optional mention.)
- **Git:** work on branch `scaffold/soul-audio-template`. Commit after each task. Every commit message ends with the required trailers:
  ```
  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01EuP1r8VWudGTnrUpu1xdxh
  ```
- **Fixtures are static committed files.** Tests read committed `testdata/*.wav` and `testdata/*.pcm`; `cargo xtask fixtures` only regenerates them.

---

### Task 1: Workspace foundation + core crate skeleton (cross-compile proof)

Establishes the repo shell and proves the central premise — the same crate compiles for host and for the STM32 target.

**Files:**
- Create: `Cargo.toml` (workspace root)
- Create: `rust-toolchain.toml`
- Create: `.cargo/config.toml`
- Create: `.gitignore`
- Create: `crates/soul-audio-core/Cargo.toml`
- Create: `crates/soul-audio-core/src/lib.rs`

**Interfaces:**
- Produces: a resolvable workspace whose only member is `crates/soul-audio-core`; the crate exports nothing yet.

- [ ] **Step 1: Create the branch**

```bash
cd /d/dev/soulaudio/soul-audio-core
git checkout -b scaffold/soul-audio-template
```

- [ ] **Step 2: Write the workspace root `Cargo.toml`**

```toml
[workspace]
resolver = "3"
members = ["crates/soul-audio-core"]
default-members = ["crates/soul-audio-core"]

[workspace.package]
edition = "2024"
rust-version = "1.85"
license = "MIT OR Apache-2.0"
repository = "local"

[workspace.dependencies]
embedded-io = "0.6"
embedded-io-adapters = "0.6"

[workspace.lints.clippy]
all = { level = "warn", priority = -1 }

[profile.release]
opt-level = "s"
lto = "fat"
codegen-units = 1
debug = 2          # keep debuginfo for defmt/probe-rs; stripped from flash image
```

- [ ] **Step 3: Write `rust-toolchain.toml`**

```toml
[toolchain]
channel = "stable"
components = ["rustfmt", "clippy", "llvm-tools", "rust-src"]
targets = ["thumbv7em-none-eabihf"]
```

- [ ] **Step 4: Write `.cargo/config.toml`**

```toml
[alias]
xtask = "run --package xtask --"
fw = "build --package soul-audio-firmware --target thumbv7em-none-eabihf"
fw-run = "run --package soul-audio-firmware --target thumbv7em-none-eabihf"
check-embedded = "build --package soul-audio-core --target thumbv7em-none-eabihf --no-default-features"

[target.thumbv7em-none-eabihf]
runner = "probe-rs run --chip STM32H743ZITx"
rustflags = [
  "-C", "link-arg=--nmagic",
  "-C", "link-arg=-Tlink.x",
  "-C", "link-arg=-Tdefmt.x",
]
```

- [ ] **Step 5: Write `.gitignore`**

```gitignore
/target
**/*.rs.bk
/out.pcm
```

- [ ] **Step 6: Write `crates/soul-audio-core/Cargo.toml`**

```toml
[package]
name = "soul-audio-core"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true
description = "Cross-platform, no_std, no-alloc audio decode core (WAV reference; MP3/FLAC/DSD planned)."

[lints]
workspace = true

[dependencies]
embedded-io = { workspace = true }
```

- [ ] **Step 7: Write `crates/soul-audio-core/src/lib.rs` (skeleton)**

```rust
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! `soul-audio-core` — a strict `no_std`, no-alloc audio decode core.
//!
//! Decoders read from any [`embedded_io::Read`] source and write PCM samples
//! into caller-provided buffers. No allocation happens anywhere in this crate.
```

- [ ] **Step 8: Verify host build passes**

Run: `cargo build`
Expected: compiles `soul-audio-core` for the host with no errors.

- [ ] **Step 9: Verify the STM32 cross-build passes (the whole point)**

Run: `cargo build -p soul-audio-core --target thumbv7em-none-eabihf --no-default-features`
Expected: compiles cleanly for `thumbv7em-none-eabihf`.

- [ ] **Step 10: Commit**

```bash
git add -A
git commit -m "chore: workspace foundation + soul-audio-core skeleton (host + thumbv7 build)"
```

---

### Task 2: Core API — types, errors, Decoder trait, SliceReader

Defines the decode boundary and a reusable no-alloc byte reader used by tests and firmware.

**Files:**
- Create: `crates/soul-audio-core/src/decoder.rs`
- Create: `crates/soul-audio-core/src/io.rs`
- Modify: `crates/soul-audio-core/src/lib.rs`

**Interfaces:**
- Produces:
  - `StreamInfo { sample_rate: u32, channels: u16, bits_per_sample: u16 }` (Copy, Eq)
  - `enum DecodeError { UnexpectedEof, Malformed, Unsupported, Io }` (`#[non_exhaustive]`, Copy, Eq)
  - `trait Decoder { fn stream_info(&self) -> StreamInfo; fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError>; }`
  - `io::SliceReader<'a>` implementing `embedded_io::Read` (Error = `embedded_io::ErrorKind`)
  - `io::read_exact_buf`, `io::skip` helpers (crate-visible)

- [ ] **Step 1: Write the failing unit tests in `src/io.rs`**

```rust
//! No-alloc I/O helpers for decoders.

use embedded_io::{ErrorKind, ErrorType, Read};

use crate::decoder::DecodeError;

/// A zero-allocation [`embedded_io::Read`] over an in-memory byte slice.
///
/// Used by the firmware (`include_bytes!`) and tests; on desktop use
/// `embedded_io_adapters` to wrap a `std::fs::File` instead.
pub struct SliceReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> SliceReader<'a> {
    /// Wrap `data`, starting at offset 0.
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
}

impl ErrorType for SliceReader<'_> {
    type Error = ErrorKind;
}

impl Read for SliceReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        let remaining = &self.data[self.pos..];
        let n = remaining.len().min(buf.len());
        buf[..n].copy_from_slice(&remaining[..n]);
        self.pos += n;
        Ok(n)
    }
}

/// Read exactly `buf.len()` bytes, mapping errors to [`DecodeError`].
pub(crate) fn read_exact_buf<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<(), DecodeError> {
    r.read_exact(buf).map_err(|e| match e {
        embedded_io::ReadExactError::UnexpectedEof => DecodeError::UnexpectedEof,
        embedded_io::ReadExactError::Other(_) => DecodeError::Io,
    })
}

/// Read and discard `n` bytes without allocating.
pub(crate) fn skip<R: Read>(r: &mut R, mut n: u32) -> Result<(), DecodeError> {
    let mut scratch = [0u8; 64];
    while n > 0 {
        let take = (n as usize).min(scratch.len());
        read_exact_buf(r, &mut scratch[..take])?;
        n -= take as u32;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_reader_reads_then_eofs() {
        let data = [1u8, 2, 3, 4, 5];
        let mut r = SliceReader::new(&data);
        let mut buf = [0u8; 3];
        assert_eq!(r.read(&mut buf).unwrap(), 3);
        assert_eq!(buf, [1, 2, 3]);
        assert_eq!(r.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf[..2], &[4, 5]);
        assert_eq!(r.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn read_exact_buf_reports_eof() {
        let data = [1u8, 2];
        let mut r = SliceReader::new(&data);
        let mut buf = [0u8; 4];
        assert_eq!(read_exact_buf(&mut r, &mut buf), Err(DecodeError::UnexpectedEof));
    }

    #[test]
    fn skip_advances() {
        let data = [0u8; 100];
        let mut r = SliceReader::new(&data);
        skip(&mut r, 70).unwrap();
        let mut buf = [0u8; 40];
        // only 30 bytes left, so read_exact must fail
        assert_eq!(read_exact_buf(&mut r, &mut buf), Err(DecodeError::UnexpectedEof));
    }
}
```

- [ ] **Step 2: Write `src/decoder.rs`**

```rust
//! The decode boundary: [`Decoder`], [`StreamInfo`], [`DecodeError`].

/// Format-independent description of a decoded PCM stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamInfo {
    /// Samples per second (per channel), e.g. 44_100.
    pub sample_rate: u32,
    /// Number of interleaved channels.
    pub channels: u16,
    /// Source bit depth (samples are widened into `i32`).
    pub bits_per_sample: u16,
}

/// Errors a decoder can report. `#[non_exhaustive]` so new variants stay
/// non-breaking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecodeError {
    /// The stream ended before a complete unit could be read.
    UnexpectedEof,
    /// The stream is not valid for the claimed format.
    Malformed,
    /// A valid but unsupported variant (e.g. a format still stubbed out).
    Unsupported,
    /// The underlying reader failed.
    Io,
}

/// A streaming audio decoder producing interleaved `i32` PCM samples into a
/// caller-provided buffer. Implementors allocate nothing.
pub trait Decoder {
    /// Stream parameters, known after construction.
    fn stream_info(&self) -> StreamInfo;

    /// Decode up to `out.len()` interleaved samples into `out`.
    ///
    /// Returns the number of samples written; `Ok(0)` means end of stream.
    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError>;
}
```

- [ ] **Step 3: Wire modules in `src/lib.rs`**

Replace the file with:

```rust
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! `soul-audio-core` — a strict `no_std`, no-alloc audio decode core.
//!
//! Decoders read from any [`embedded_io::Read`] source and write PCM samples
//! into caller-provided buffers. No allocation happens anywhere in this crate.

pub mod decoder;
pub mod formats;
pub mod io;

pub use decoder::{DecodeError, Decoder, StreamInfo};
pub use io::SliceReader;
```

- [ ] **Step 4: Create a placeholder `src/formats/mod.rs` so `lib.rs` compiles**

```rust
//! Format decoders. WAV is implemented; the rest are stubs for now.
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p soul-audio-core`
Expected: the three `io` tests PASS.

- [ ] **Step 6: Verify the embedded build still compiles**

Run: `cargo check-embedded`
Expected: PASS (no_std, no-alloc core builds for thumbv7em).

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat(core): decode boundary (Decoder/StreamInfo/DecodeError) + SliceReader"
```

---

### Task 3: WAV reference decoder + format stubs

Implements the working reference decoder (16-bit PCM WAV) and the stub entry points for the formats to be built later.

**Files:**
- Create: `crates/soul-audio-core/src/formats/wav.rs`
- Create: `crates/soul-audio-core/src/formats/mp3.rs`
- Create: `crates/soul-audio-core/src/formats/flac.rs`
- Create: `crates/soul-audio-core/src/formats/dsd.rs`
- Modify: `crates/soul-audio-core/src/formats/mod.rs`

**Interfaces:**
- Consumes: `Decoder`, `StreamInfo`, `DecodeError`, `io::{read_exact_buf, skip}` (Task 2).
- Produces:
  - `formats::wav::WavDecoder<R: embedded_io::Read>` with `new(reader) -> Result<Self, DecodeError>`, implementing `Decoder`.
  - `formats::mp3::Mp3Decoder`, `formats::flac::FlacDecoder`, `formats::dsd::DsdDecoder`, each `new<R: Read>(reader) -> Result<Self, DecodeError>` returning `Err(Unsupported)`.

- [ ] **Step 1: Write the failing WAV tests in `src/formats/wav.rs`**

```rust
//! Minimal reference WAV decoder: PCM, 16-bit, any channel count / rate.

use embedded_io::Read;

use crate::decoder::{DecodeError, Decoder, StreamInfo};
use crate::io::{read_exact_buf, skip};

/// Streaming decoder for uncompressed 16-bit PCM WAV.
pub struct WavDecoder<R: Read> {
    reader: R,
    info: StreamInfo,
    data_remaining: u32,
}

impl<R: Read> WavDecoder<R> {
    /// Parse the RIFF/WAVE header, positioning the reader at PCM data.
    ///
    /// # Errors
    /// [`DecodeError::Malformed`] if not a RIFF/WAVE file,
    /// [`DecodeError::Unsupported`] if not 16-bit PCM.
    pub fn new(mut reader: R) -> Result<Self, DecodeError> {
        let mut hdr = [0u8; 12];
        read_exact_buf(&mut reader, &mut hdr)?;
        if &hdr[0..4] != b"RIFF" || &hdr[8..12] != b"WAVE" {
            return Err(DecodeError::Malformed);
        }

        let mut info: Option<StreamInfo> = None;
        loop {
            let mut chunk = [0u8; 8];
            read_exact_buf(&mut reader, &mut chunk)?;
            let id = [chunk[0], chunk[1], chunk[2], chunk[3]];
            let size = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);

            if &id == b"fmt " {
                let mut fmt = [0u8; 16];
                read_exact_buf(&mut reader, &mut fmt)?;
                let audio_format = u16::from_le_bytes([fmt[0], fmt[1]]);
                let channels = u16::from_le_bytes([fmt[2], fmt[3]]);
                let sample_rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
                let bits_per_sample = u16::from_le_bytes([fmt[14], fmt[15]]);
                if audio_format != 1 || bits_per_sample != 16 {
                    return Err(DecodeError::Unsupported);
                }
                info = Some(StreamInfo { sample_rate, channels, bits_per_sample });
                if size > 16 {
                    skip(&mut reader, size - 16)?;
                }
            } else if &id == b"data" {
                let info = info.ok_or(DecodeError::Malformed)?;
                return Ok(Self { reader, info, data_remaining: size });
            } else {
                skip(&mut reader, size)?;
                if size % 2 == 1 {
                    skip(&mut reader, 1)?; // RIFF chunks are word-aligned
                }
            }
        }
    }
}

impl<R: Read> Decoder for WavDecoder<R> {
    fn stream_info(&self) -> StreamInfo {
        self.info
    }

    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        let max_samples = (self.data_remaining / 2) as usize;
        let want = out.len().min(max_samples);
        let mut produced = 0;
        let mut two = [0u8; 2];
        while produced < want {
            read_exact_buf(&mut self.reader, &mut two)?;
            out[produced] = i32::from(i16::from_le_bytes(two));
            produced += 1;
            self.data_remaining -= 2;
        }
        Ok(produced)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::SliceReader;

    /// Build a tiny valid 16-bit PCM WAV in memory.
    fn make_wav(samples: &[i16], channels: u16, rate: u32) -> std::vec::Vec<u8> {
        let data_bytes = samples.len() * 2;
        let mut w = std::vec::Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&((36 + data_bytes) as u32).to_le_bytes());
        w.extend_from_slice(b"WAVE");
        w.extend_from_slice(b"fmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&channels.to_le_bytes());
        w.extend_from_slice(&rate.to_le_bytes());
        w.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
        w.extend_from_slice(&(channels * 2).to_le_bytes());
        w.extend_from_slice(&16u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(data_bytes as u32).to_le_bytes());
        for s in samples {
            w.extend_from_slice(&s.to_le_bytes());
        }
        w
    }

    #[test]
    fn parses_header() {
        let wav = make_wav(&[1, -1, 100, -100], 2, 44_100);
        let dec = WavDecoder::new(SliceReader::new(&wav)).unwrap();
        assert_eq!(
            dec.stream_info(),
            StreamInfo { sample_rate: 44_100, channels: 2, bits_per_sample: 16 }
        );
    }

    #[test]
    fn decodes_all_samples() {
        let src = [1i16, -1, 32_767, -32_768, 5, 6];
        let wav = make_wav(&src, 2, 48_000);
        let mut dec = WavDecoder::new(SliceReader::new(&wav)).unwrap();
        let mut out = [0i32; 4];
        let mut got = std::vec::Vec::new();
        loop {
            let n = dec.decode_into(&mut out).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&out[..n]);
        }
        let expected: std::vec::Vec<i32> = src.iter().map(|&s| i32::from(s)).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn rejects_non_riff() {
        let bad = [0u8; 44];
        assert_eq!(
            WavDecoder::new(SliceReader::new(&bad)).map(|_| ()),
            Err(DecodeError::Malformed)
        );
    }

    #[test]
    fn skips_unknown_chunk() {
        // Insert a "LIST" chunk of 3 bytes (odd → padded) before "data".
        let data_samples = [7i16, 8];
        let mut w = std::vec::Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&0u32.to_le_bytes()); // size not validated
        w.extend_from_slice(b"WAVE");
        w.extend_from_slice(b"fmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&8_000u32.to_le_bytes());
        w.extend_from_slice(&16_000u32.to_le_bytes());
        w.extend_from_slice(&2u16.to_le_bytes());
        w.extend_from_slice(&16u16.to_le_bytes());
        w.extend_from_slice(b"LIST");
        w.extend_from_slice(&3u32.to_le_bytes());
        w.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        w.push(0x00); // pad byte
        w.extend_from_slice(b"data");
        w.extend_from_slice(&4u32.to_le_bytes());
        for s in data_samples {
            w.extend_from_slice(&s.to_le_bytes());
        }
        let mut dec = WavDecoder::new(SliceReader::new(&w)).unwrap();
        let mut out = [0i32; 8];
        let n = dec.decode_into(&mut out).unwrap();
        assert_eq!(&out[..n], &[7, 8]);
    }
}
```

- [ ] **Step 2: Write the three stubs**

`src/formats/mp3.rs`:

```rust
//! MP3 decoder — not yet implemented.

use embedded_io::Read;

use crate::decoder::DecodeError;

/// Placeholder MP3 decoder. Construction currently fails with
/// [`DecodeError::Unsupported`]; real decoding lands here later.
pub struct Mp3Decoder;

impl Mp3Decoder {
    /// # Errors
    /// Always returns [`DecodeError::Unsupported`] for now.
    pub fn new<R: Read>(_reader: R) -> Result<Self, DecodeError> {
        Err(DecodeError::Unsupported)
    }
}
```

`src/formats/flac.rs`:

```rust
//! FLAC decoder — not yet implemented.

use embedded_io::Read;

use crate::decoder::DecodeError;

/// Placeholder FLAC decoder. See [`crate::formats::mp3::Mp3Decoder`].
pub struct FlacDecoder;

impl FlacDecoder {
    /// # Errors
    /// Always returns [`DecodeError::Unsupported`] for now.
    pub fn new<R: Read>(_reader: R) -> Result<Self, DecodeError> {
        Err(DecodeError::Unsupported)
    }
}
```

`src/formats/dsd.rs`:

```rust
//! DSD decoder — not yet implemented.

use embedded_io::Read;

use crate::decoder::DecodeError;

/// Placeholder DSD decoder. See [`crate::formats::mp3::Mp3Decoder`].
pub struct DsdDecoder;

impl DsdDecoder {
    /// # Errors
    /// Always returns [`DecodeError::Unsupported`] for now.
    pub fn new<R: Read>(_reader: R) -> Result<Self, DecodeError> {
        Err(DecodeError::Unsupported)
    }
}
```

- [ ] **Step 3: Update `src/formats/mod.rs`**

```rust
//! Format decoders. WAV is implemented; the rest are stubs for now.

pub mod dsd;
pub mod flac;
pub mod mp3;
pub mod wav;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p soul-audio-core`
Expected: all `io` and `wav` tests PASS.

- [ ] **Step 5: Verify the embedded build still compiles**

Run: `cargo check-embedded`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(core): 16-bit PCM WAV decoder + mp3/flac/dsd stubs"
```

---

### Task 4: `xtask` crate + fixture generation

Adds the pure-Rust automation crate and generates the committed test fixtures (independently of `soul-audio-core`).

**Files:**
- Create: `xtask/Cargo.toml`
- Create: `xtask/src/main.rs`
- Create: `testdata/README.md`
- Modify: `Cargo.toml` (add `xtask` to members + default-members)
- Generated & committed: `testdata/sine_stereo_44100.wav`, `testdata/sine_stereo_44100.pcm`

**Interfaces:**
- Produces: `cargo xtask fixtures` (writes `testdata/`), plus `verify`/`ci`/`install-hooks` subcommands finalized in Task 9.

- [ ] **Step 1: Add `xtask` to the workspace in root `Cargo.toml`**

Change `members`/`default-members`:

```toml
members = ["crates/soul-audio-core", "xtask"]
default-members = ["crates/soul-audio-core", "xtask"]
```

- [ ] **Step 2: Write `xtask/Cargo.toml`**

```toml
[package]
name = "xtask"
version = "0.0.0"
edition.workspace = true
rust-version.workspace = true
publish = false

[lints]
workspace = true
```

- [ ] **Step 3: Write `xtask/src/main.rs` (fixtures command first)**

```rust
//! Repo automation. Run via the cargo alias: `cargo xtask <command>`.

use std::f64::consts::PI;
use std::fs;
use std::path::Path;
use std::process::exit;

fn main() {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    let result = match cmd.as_str() {
        "fixtures" => gen_fixtures(),
        other => {
            eprintln!("unknown xtask command: {other:?}");
            eprintln!("usage: cargo xtask <fixtures>");
            exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("xtask failed: {e}");
        exit(1);
    }
}

/// Generate `testdata/` fixtures. Independent of `soul-audio-core` so tests
/// validate the decoder against an external oracle, not against itself.
fn gen_fixtures() -> Result<(), String> {
    let sample_rate: u32 = 44_100;
    let channels: u16 = 2;
    let frames: usize = 220; // ~5 ms

    // Source samples: a 440 Hz sine, identical on both channels.
    let mut samples: Vec<i16> = Vec::with_capacity(frames * channels as usize);
    for i in 0..frames {
        let t = i as f64 / f64::from(sample_rate);
        let v = (2.0 * PI * 440.0 * t).sin();
        let s = (v * 10_000.0) as i16;
        for _ in 0..channels {
            samples.push(s);
        }
    }

    let wav = build_wav(&samples, channels, sample_rate);
    let mut pcm = Vec::with_capacity(samples.len() * 4);
    for &s in &samples {
        pcm.extend_from_slice(&i32::from(s).to_le_bytes());
    }

    let dir = Path::new("testdata");
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    fs::write(dir.join("sine_stereo_44100.wav"), &wav).map_err(|e| e.to_string())?;
    fs::write(dir.join("sine_stereo_44100.pcm"), &pcm).map_err(|e| e.to_string())?;
    println!(
        "wrote testdata/sine_stereo_44100.wav ({} bytes) and .pcm ({} bytes)",
        wav.len(),
        pcm.len()
    );
    Ok(())
}

fn build_wav(samples: &[i16], channels: u16, rate: u32) -> Vec<u8> {
    let data_bytes = samples.len() * 2;
    let mut w = Vec::with_capacity(44 + data_bytes);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&((36 + data_bytes) as u32).to_le_bytes());
    w.extend_from_slice(b"WAVE");
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&channels.to_le_bytes());
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
    w.extend_from_slice(&(channels * 2).to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(data_bytes as u32).to_le_bytes());
    for &s in samples {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}
```

- [ ] **Step 4: Write `testdata/README.md`**

```markdown
# testdata

Static, committed fixtures used by integration, E2E, and architecture tests.

- `sine_stereo_44100.wav` — 16-bit PCM, stereo, 44.1 kHz, 220 frames (~5 ms) of a
  440 Hz sine (amplitude 10000), identical on both channels.
- `sine_stereo_44100.pcm` — the expected decoder output: the same samples as
  interleaved little-endian **i32** (each 16-bit source sample widened to i32).

Both files are produced by `cargo xtask fixtures`, which computes the samples
from the sine formula directly — it does **not** call `soul-audio-core`. That
keeps the golden output an independent oracle for the decoder under test.
Regenerate with `cargo xtask fixtures`; commit the results.
```

- [ ] **Step 5: Generate the fixtures**

Run: `cargo xtask fixtures`
Expected: prints the byte counts and creates both files under `testdata/`.

- [ ] **Step 6: Sanity-check the fixtures exist and are non-empty**

Run: `ls -l testdata/`
Expected: `sine_stereo_44100.wav` (~924 bytes) and `sine_stereo_44100.pcm` (1760 bytes).

- [ ] **Step 7: Commit (including generated fixtures)**

```bash
git add -A
git commit -m "chore(xtask): fixture generator + committed testdata"
```

---

### Task 5: Core integration test (decode → golden)

Proves the decoder against the committed fixtures via the public API.

**Files:**
- Create: `crates/soul-audio-core/tests/decode.rs`

**Interfaces:**
- Consumes: `WavDecoder`, `SliceReader`, `Decoder` (public API); `testdata/` fixtures (Task 4).

- [ ] **Step 1: Write the integration test**

```rust
//! Integration: decode the committed WAV fixture and compare to golden PCM.

use soul_audio_core::formats::wav::WavDecoder;
use soul_audio_core::{Decoder, SliceReader};

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(name)
}

#[test]
fn decodes_wav_fixture_to_golden_pcm() {
    let wav = std::fs::read(fixture("sine_stereo_44100.wav")).unwrap();
    let golden = std::fs::read(fixture("sine_stereo_44100.pcm")).unwrap();

    let mut dec = WavDecoder::new(SliceReader::new(&wav)).unwrap();
    let info = dec.stream_info();
    assert_eq!(info.sample_rate, 44_100);
    assert_eq!(info.channels, 2);
    assert_eq!(info.bits_per_sample, 16);

    let mut out = [0i32; 128];
    let mut got: Vec<u8> = Vec::new();
    loop {
        let n = dec.decode_into(&mut out).unwrap();
        if n == 0 {
            break;
        }
        for &s in &out[..n] {
            got.extend_from_slice(&s.to_le_bytes());
        }
    }

    assert_eq!(got, golden, "decoded PCM must match golden fixture");
}
```

- [ ] **Step 2: Run the integration test**

Run: `cargo test -p soul-audio-core --test decode`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "test(core): integration decode-to-golden against fixtures"
```

---

### Task 6: `soul-audio-cli` + E2E test

The desktop test harness and a black-box E2E test that runs the built binary.

**Files:**
- Create: `crates/soul-audio-cli/Cargo.toml`
- Create: `crates/soul-audio-cli/src/main.rs`
- Create: `crates/soul-audio-cli/tests/e2e.rs`
- Modify: `Cargo.toml` (add cli to members + default-members)

**Interfaces:**
- Consumes: `WavDecoder`, `Decoder` (core); `embedded_io_adapters::std::FromStd` to wrap `File`.
- Produces: binary `soul-audio-cli` with `decode <input> --out <output>` writing interleaved i32 LE PCM.

- [ ] **Step 1: Add cli to the workspace in root `Cargo.toml`**

```toml
members = ["crates/soul-audio-core", "crates/soul-audio-cli", "xtask"]
default-members = ["crates/soul-audio-core", "crates/soul-audio-cli", "xtask"]
```

- [ ] **Step 2: Write `crates/soul-audio-cli/Cargo.toml`**

```toml
[package]
name = "soul-audio-cli"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true
description = "Desktop test harness for soul-audio-core."

[lints]
workspace = true

[dependencies]
soul-audio-core = { path = "../soul-audio-core" }
embedded-io = { workspace = true }
embedded-io-adapters = { workspace = true, features = ["std"] }
clap = { version = "4", features = ["derive"] }
anyhow = "1"

[features]
default = []
playback = ["dep:cpal"]

[dependencies.cpal]
version = "0.15"
optional = true

[dev-dependencies]
assert_cmd = "2"
predicates = "3"
```

- [ ] **Step 3: Write `crates/soul-audio-cli/src/main.rs`**

```rust
//! `soul-audio-cli` — drives `soul-audio-core` on the desktop for testing.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use embedded_io_adapters::std::FromStd;
use soul_audio_core::formats::wav::WavDecoder;
use soul_audio_core::Decoder;

#[derive(Parser)]
#[command(name = "soul-audio-cli", about = "soul-audio-core desktop harness")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Decode a WAV file to interleaved i32 LE PCM.
    Decode {
        /// Input .wav path.
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

fn decode(input: &PathBuf, out: &PathBuf) -> Result<()> {
    let file = File::open(input).with_context(|| format!("opening {input:?}"))?;
    let reader = FromStd::new(file);
    let mut dec = WavDecoder::new(reader).map_err(|e| anyhow::anyhow!("decode init: {e:?}"))?;

    let info = dec.stream_info();
    eprintln!(
        "{} Hz, {} ch, {}-bit",
        info.sample_rate, info.channels, info.bits_per_sample
    );

    let out_file = File::create(out).with_context(|| format!("creating {out:?}"))?;
    let mut writer = BufWriter::new(out_file);
    let mut buf = [0i32; 1024];
    let mut total = 0usize;
    loop {
        let n = dec
            .decode_into(&mut buf)
            .map_err(|e| anyhow::anyhow!("decode: {e:?}"))?;
        if n == 0 {
            break;
        }
        for &s in &buf[..n] {
            writer.write_all(&s.to_le_bytes())?;
        }
        total += n;
    }
    writer.flush()?;
    eprintln!("decoded {total} samples");
    Ok(())
}
```

- [ ] **Step 4: Write `crates/soul-audio-cli/tests/e2e.rs`**

```rust
//! Black-box E2E: run the built CLI on the fixture and diff against golden.

use std::path::{Path, PathBuf};

use assert_cmd::Command;

fn testdata(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(name)
}

#[test]
fn cli_decode_matches_golden() {
    let out = std::env::temp_dir().join("soul_audio_cli_e2e_out.pcm");
    let _ = std::fs::remove_file(&out);

    Command::cargo_bin("soul-audio-cli")
        .unwrap()
        .arg("decode")
        .arg(testdata("sine_stereo_44100.wav"))
        .arg("--out")
        .arg(&out)
        .assert()
        .success();

    let produced = std::fs::read(&out).unwrap();
    let golden = std::fs::read(testdata("sine_stereo_44100.pcm")).unwrap();
    assert_eq!(produced, golden, "CLI output must match golden fixture");
}
```

- [ ] **Step 5: Run the E2E test**

Run: `cargo test -p soul-audio-cli`
Expected: `cli_decode_matches_golden` PASS.

- [ ] **Step 6: Confirm default build stays audio-device-free**

Run: `cargo build -p soul-audio-cli`
Expected: builds without pulling `cpal` (playback feature off by default).

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat(cli): decode harness + black-box E2E test"
```

---

### Task 7: `architecture-tests` (no-alloc, layering, compile-fail)

Cross-cutting invariant tests that lock in the design rules.

**Files:**
- Create: `crates/architecture-tests/Cargo.toml`
- Create: `crates/architecture-tests/src/lib.rs`
- Create: `crates/architecture-tests/tests/no_alloc.rs`
- Create: `crates/architecture-tests/tests/layering.rs`
- Create: `crates/architecture-tests/tests/compile_fail.rs`
- Create: `crates/architecture-tests/tests/compile_fail/exhaustive_match.rs`
- Modify: `Cargo.toml` (add to members + default-members)

**Interfaces:**
- Consumes: `soul-audio-core` public API; `cargo_metadata`; `trybuild`.

- [ ] **Step 1: Add to the workspace in root `Cargo.toml`**

```toml
members = ["crates/soul-audio-core", "crates/soul-audio-cli", "crates/architecture-tests", "xtask"]
default-members = ["crates/soul-audio-core", "crates/soul-audio-cli", "crates/architecture-tests", "xtask"]
```

- [ ] **Step 2: Write `crates/architecture-tests/Cargo.toml`**

```toml
[package]
name = "architecture-tests"
version = "0.0.0"
edition.workspace = true
rust-version.workspace = true
publish = false

# NOTE: no `[lints] workspace = true` here — the no_alloc test needs an
# unsafe GlobalAlloc impl and must not be denied.

[dev-dependencies]
soul-audio-core = { path = "../soul-audio-core" }
embedded-io = { workspace = true }
cargo_metadata = "0.18"
trybuild = "1"
```

- [ ] **Step 3: Write `crates/architecture-tests/src/lib.rs`**

```rust
//! Home for cross-cutting architecture/invariant tests (see `tests/`).
```

- [ ] **Step 4: Write `tests/no_alloc.rs` (runtime no-alloc proof)**

```rust
//! Prove `soul-audio-core` allocates nothing during a decode by installing a
//! counting global allocator and asserting the counter does not move.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use soul_audio_core::formats::wav::WavDecoder;
use soul_audio_core::{Decoder, SliceReader};

static ALLOCS: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: forwarding to the system allocator with the same layout.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarding to the system allocator with the same ptr/layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GA: Counting = Counting;

fn make_wav() -> Vec<u8> {
    // 4 samples of 16-bit PCM stereo — allocation here happens BEFORE measuring.
    let src: [i16; 4] = [1, -1, 32_767, -32_768];
    let data_bytes = src.len() * 2;
    let mut w = Vec::new();
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&((36 + data_bytes) as u32).to_le_bytes());
    w.extend_from_slice(b"WAVE");
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&44_100u32.to_le_bytes());
    w.extend_from_slice(&176_400u32.to_le_bytes());
    w.extend_from_slice(&4u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(data_bytes as u32).to_le_bytes());
    for s in src {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

#[test]
fn decode_does_not_allocate() {
    let wav = make_wav();
    let mut out = [0i32; 64]; // stack buffer, no heap

    // Construct the decoder, then measure only the decode loop.
    let mut dec = WavDecoder::new(SliceReader::new(&wav)).unwrap();

    let before = ALLOCS.load(Ordering::SeqCst);
    loop {
        let n = dec.decode_into(&mut out).unwrap();
        if n == 0 {
            break;
        }
    }
    let after = ALLOCS.load(Ordering::SeqCst);

    assert_eq!(before, after, "soul-audio-core allocated during decode");
}
```

- [ ] **Step 5: Write `tests/layering.rs` (dependency-direction rules)**

```rust
//! Enforce architectural layering via the resolved dependency graph.

use cargo_metadata::MetadataCommand;

/// Normal (non-dev, non-build) dependency names `soul-audio-core` may have.
const CORE_ALLOWLIST: &[&str] = &["embedded-io"];

fn pkg<'a>(md: &'a cargo_metadata::Metadata, name: &str) -> &'a cargo_metadata::Package {
    md.packages
        .iter()
        .find(|p| p.name == name)
        .unwrap_or_else(|| panic!("package {name} not found in workspace metadata"))
}

fn normal_dep_names(p: &cargo_metadata::Package) -> Vec<String> {
    p.dependencies
        .iter()
        .filter(|d| d.kind == cargo_metadata::DependencyKind::Normal)
        .map(|d| d.name.clone())
        .collect()
}

#[test]
fn core_only_depends_on_allowlist() {
    let md = MetadataCommand::new().exec().unwrap();
    let core = pkg(&md, "soul-audio-core");
    for dep in normal_dep_names(core) {
        assert!(
            CORE_ALLOWLIST.contains(&dep.as_str()),
            "soul-audio-core has disallowed normal dependency: {dep}"
        );
    }
}

#[test]
fn shells_depend_on_core_not_the_reverse() {
    let md = MetadataCommand::new().exec().unwrap();
    let core = pkg(&md, "soul-audio-core");
    for forbidden in ["soul-audio-cli", "soul-audio-firmware"] {
        assert!(
            !normal_dep_names(core).contains(&forbidden.to_string()),
            "soul-audio-core must not depend on {forbidden}"
        );
    }
    for shell in ["soul-audio-cli", "soul-audio-firmware"] {
        let p = pkg(&md, shell);
        assert!(
            normal_dep_names(p).contains(&"soul-audio-core".to_string()),
            "{shell} should depend on soul-audio-core"
        );
    }
}
```

- [ ] **Step 6: Write `tests/compile_fail.rs` and the failing case**

`tests/compile_fail.rs`:

```rust
//! Things that MUST NOT compile. `DecodeError` is `#[non_exhaustive]`, so an
//! external exhaustive match without a wildcard is a compile error.

#[test]
fn compile_fail_cases() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/*.rs");
}
```

`tests/compile_fail/exhaustive_match.rs`:

```rust
use soul_audio_core::DecodeError;

fn main() {
    let e = DecodeError::Io;
    // Missing wildcard arm on a #[non_exhaustive] enum from another crate.
    match e {
        DecodeError::UnexpectedEof => {}
        DecodeError::Malformed => {}
        DecodeError::Unsupported => {}
        DecodeError::Io => {}
    }
}
```

- [ ] **Step 7: Generate the trybuild `.stderr` snapshot, then verify**

Run: `TRYBUILD=overwrite cargo test -p architecture-tests --test compile_fail`
Then run: `cargo test -p architecture-tests`
Expected: after the overwrite run creates `tests/compile_fail/exhaustive_match.stderr`, all architecture tests PASS. (The `.stderr` is committed; it is stable because the toolchain is pinned in `rust-toolchain.toml`.)

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -m "test(arch): no-alloc, layering, and compile-fail invariant tests"
```

---

### Task 8: `soul-audio-firmware` (STM32H743 + Embassy)

The embedded shell — builds for `thumbv7em-none-eabihf`, runs the core against an embedded fixture, logs over RTT.

**Files:**
- Create: `crates/soul-audio-firmware/Cargo.toml`
- Create: `crates/soul-audio-firmware/src/main.rs`
- Modify: `Cargo.toml` (add firmware to **members only**, NOT default-members)

**Interfaces:**
- Consumes: `WavDecoder`, `SliceReader`, `Decoder` (core); Embassy runtime; the committed WAV fixture via `include_bytes!`.

- [ ] **Step 1: Add firmware to `members` (not default-members) in root `Cargo.toml`**

```toml
members = [
  "crates/soul-audio-core",
  "crates/soul-audio-cli",
  "crates/architecture-tests",
  "crates/soul-audio-firmware",
  "xtask",
]
default-members = [
  "crates/soul-audio-core",
  "crates/soul-audio-cli",
  "crates/architecture-tests",
  "xtask",
]
```

- [ ] **Step 2: Create `crates/soul-audio-firmware/Cargo.toml`, then resolve exact dep versions with `cargo add`**

Start with this file (bin-only, no tests/doctests):

```toml
[package]
name = "soul-audio-firmware"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true
description = "STM32H743 Embassy firmware shell for soul-audio-core."

# Intentionally NO `[lints] workspace = true`: Embassy macros/HAL trip
# host-oriented clippy lints; the firmware is gated by *building*, not clippy.

[[bin]]
name = "soul-audio-firmware"
test = false
bench = false
doctest = false

[dependencies]
soul-audio-core = { path = "../soul-audio-core" }
embedded-io = { workspace = true }
cortex-m = { version = "0.7", features = ["inline-asm"] }
cortex-m-rt = "0.7"
defmt = "0.3"
defmt-rtt = "0.4"
panic-probe = { version = "0.3", features = ["print-defmt"] }
# Embassy versions move fast — pin whatever `cargo add` resolves (Step 3).
embassy-executor = { version = "0.6", features = ["arch-cortex-m", "executor-thread", "defmt"] }
embassy-time = { version = "0.3", features = ["defmt"] }
embassy-stm32 = { version = "0.1", features = ["stm32h743zi", "memory-x", "time-driver-any", "defmt"] }
```

Then pin current versions (embassy releases often; the numbers above may be stale):

Run:
```bash
cargo add -p soul-audio-firmware embassy-executor --features arch-cortex-m,executor-thread,defmt
cargo add -p soul-audio-firmware embassy-time --features defmt
cargo add -p soul-audio-firmware embassy-stm32 --features stm32h743zi,memory-x,time-driver-any,defmt
```
Expected: `cargo add` rewrites the three embassy lines to the latest compatible versions. If `time-driver-any` is rejected, substitute a concrete driver (e.g. `time-driver-tim2`).

- [ ] **Step 3: Write `crates/soul-audio-firmware/src/main.rs`**

```rust
#![no_std]
#![no_main]

use defmt::info;
use defmt_rtt as _; // global defmt logger over RTT
use panic_probe as _; // panic handler

use embassy_executor::Spawner;
use soul_audio_core::formats::wav::WavDecoder;
use soul_audio_core::{Decoder, SliceReader};

/// The committed WAV fixture, embedded into flash.
static WAV: &[u8] = include_bytes!("../../../testdata/sine_stereo_44100.wav");

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    // Bring up the HAL with default clocks. NOTE: real audio needs an explicit
    // H7 clock/SAI config — that's deliberately out of template scope.
    let _p = embassy_stm32::init(Default::default());
    info!("soul-audio-firmware up");

    let mut dec = match WavDecoder::new(SliceReader::new(WAV)) {
        Ok(d) => d,
        Err(_) => {
            defmt::error!("failed to parse embedded WAV");
            return;
        }
    };

    let si = dec.stream_info();
    info!(
        "stream: {} Hz, {} ch, {} bits",
        si.sample_rate, si.channels, si.bits_per_sample
    );

    let mut buf = [0i32; 256];
    let mut total: usize = 0;
    loop {
        match dec.decode_into(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                // TODO: feed `buf[..n]` to SAI/I2S DMA (board/codec-specific).
            }
            Err(_) => {
                defmt::error!("decode error");
                break;
            }
        }
    }
    info!("decoded {} samples from flash", total);

    // Idle. TODO: replace with a real SAI DMA streaming loop.
    loop {
        embassy_time::Timer::after_millis(1000).await;
    }
}
```

- [ ] **Step 4: Build the firmware for the STM32 target**

Run: `cargo fw`
Expected: `soul-audio-firmware` links successfully for `thumbv7em-none-eabihf`. (No hardware needed to build. If linking complains about `memory.x`, confirm the `memory-x` feature is enabled on `embassy-stm32`.)

- [ ] **Step 5: Confirm host default build is unaffected**

Run: `cargo build`
Expected: builds only default-members (core, cli, architecture-tests, xtask) — firmware is skipped.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat(firmware): STM32H743 Embassy shell decoding an embedded WAV"
```

---

### Task 9: `xtask verify`/`ci`/`install-hooks` + CI workflow + README

Ties the enforcement together: one command locally, a matching CI gate, a pre-commit hook, and top-level docs.

**Files:**
- Modify: `xtask/src/main.rs` (add `verify`, `ci`, `install-hooks`)
- Create: `.github/workflows/ci.yml`
- Create: `README.md`

**Interfaces:**
- Consumes: everything above.
- Produces: `cargo xtask verify`, `cargo xtask ci`, `cargo xtask install-hooks`.

- [ ] **Step 1: Extend `xtask/src/main.rs` with a command runner and the new subcommands**

Replace the `main` dispatch and add helpers (keep `gen_fixtures`/`build_wav` from Task 4):

```rust
use std::process::Command;

fn main() {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    let result = match cmd.as_str() {
        "fixtures" => gen_fixtures(),
        "verify" | "ci" => verify(),
        "install-hooks" => install_hooks(),
        other => {
            eprintln!("unknown xtask command: {other:?}");
            eprintln!("usage: cargo xtask <fixtures|verify|ci|install-hooks>");
            exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("xtask failed: {e}");
        exit(1);
    }
}

/// Run a cargo subcommand, returning an error string on non-zero exit.
fn run(args: &[&str]) -> Result<(), String> {
    println!("+ cargo {}", args.join(" "));
    let status = Command::new(env!("CARGO"))
        .args(args)
        .status()
        .map_err(|e| format!("spawning cargo: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`cargo {}` failed ({status})", args.join(" ")))
    }
}

/// Full local gate: format, lint, host tests, the no_std core build, firmware.
fn verify() -> Result<(), String> {
    run(&["fmt", "--all", "--", "--check"])?;
    run(&["clippy", "--all-targets", "--", "-D", "warnings"])?;
    run(&["test"])?;
    run(&[
        "build",
        "-p",
        "soul-audio-core",
        "--target",
        "thumbv7em-none-eabihf",
        "--no-default-features",
    ])?;
    run(&[
        "build",
        "-p",
        "soul-audio-firmware",
        "--target",
        "thumbv7em-none-eabihf",
    ])?;
    println!("verify: OK");
    Ok(())
}

/// Install a git pre-commit hook that runs `cargo xtask verify`.
fn install_hooks() -> Result<(), String> {
    let hook = Path::new(".git/hooks/pre-commit");
    fs::write(
        hook,
        "#!/bin/sh\n# installed by `cargo xtask install-hooks`\nexec cargo xtask verify\n",
    )
    .map_err(|e| format!("writing {}: {e}", hook.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(hook).map_err(|e| e.to_string())?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(hook, perms).map_err(|e| e.to_string())?;
    }
    println!("installed .git/hooks/pre-commit");
    Ok(())
}
```

- [ ] **Step 2: Write `.github/workflows/ci.yml`**

```yaml
name: CI
on:
  push:
  pull_request:

jobs:
  host:
    name: host (fmt, clippy, test)
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all -- --check
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo test

  embedded:
    name: embedded (thumbv7em build)
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: thumbv7em-none-eabihf
      - uses: Swatinem/rust-cache@v2
      - name: core builds no_std for STM32
        run: cargo build -p soul-audio-core --target thumbv7em-none-eabihf --no-default-features
      - name: firmware builds for STM32
        run: cargo build -p soul-audio-firmware --target thumbv7em-none-eabihf
```

- [ ] **Step 3: Write `README.md`**

```markdown
# soul-audio-core

Cross-platform, strict-`no_std`, no-alloc audio decode core that runs on an
STM32H743 (Embassy firmware) and on the desktop (CLI harness). WAV is
implemented as a reference; MP3/FLAC/DSD are stubbed for future work.

## Layout

- `crates/soul-audio-core` — the library (`no_std`, no-alloc, `forbid(unsafe_code)`).
- `crates/soul-audio-firmware` — STM32H743 + Embassy shell (`thumbv7em-none-eabihf`).
- `crates/soul-audio-cli` — desktop test harness.
- `crates/architecture-tests` — no-alloc / layering / compile-fail invariants.
- `xtask` — automation. `testdata/` — committed fixtures.

## Common commands

| Command | What it does |
|---|---|
| `cargo test` | Host tests: integration, E2E, architecture. |
| `cargo check-embedded` | Build the core `no_std` for STM32 (the discipline gate). |
| `cargo fw` | Build the firmware for `thumbv7em-none-eabihf`. |
| `cargo fw-run` | Flash + run on hardware via `probe-rs` (needs a board + `probe-rs`). |
| `cargo xtask verify` | Full local gate (fmt, clippy, test, embedded builds). |
| `cargo xtask install-hooks` | Install a pre-commit hook running `verify`. |
| `cargo xtask fixtures` | Regenerate `testdata/`. |

## Prerequisites

- Rust (toolchain + `thumbv7em-none-eabihf` target auto-installed via `rust-toolchain.toml`).
- For flashing: `cargo install probe-rs-tools` and an STM32H743 board (e.g. Nucleo-H743ZI2).

## Adding a decoder

Implement it under `crates/soul-audio-core/src/formats/`, add fixtures via
`xtask`, and extend the integration test. No boundary or tooling changes needed.
```

- [ ] **Step 4: Run the full gate**

Run: `cargo xtask verify`
Expected: every stage passes, ending with `verify: OK`.

- [ ] **Step 5: Install the hook and confirm it exists**

Run: `cargo xtask install-hooks && cat .git/hooks/pre-commit`
Expected: the hook file prints and contains `exec cargo xtask verify`.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "chore: xtask verify/ci/install-hooks, GitHub Actions gate, README"
```

- [ ] **Step 7: Final sanity — clean host test + embedded build**

Run: `cargo test && cargo fw && cargo check-embedded`
Expected: all green.

---

## Self-Review

**1. Spec coverage:**
- Strict no-alloc core → Task 2/3 (caller buffers), enforced Task 7 (`no_alloc`) + `cargo check-embedded`. ✓
- Embassy firmware on STM32H743 → Task 8. ✓
- CLI test harness → Task 6. ✓
- `embedded-io` boundary + std interop → Task 2 (`SliceReader`) + Task 6 (`FromStd`). ✓
- WAV working, MP3/FLAC/DSD stubs → Task 3. ✓
- Enforcement CI+local (rust-toolchain, xtask, aliases, pre-commit, GH Actions) → Tasks 1, 4, 9. ✓
- Three test kinds (integration, E2E, architecture) + fixtures → Tasks 4/5/6/7. ✓
- `src/` + `tests/` structure, testdata → all tasks. ✓
- Rust 2024, resolver 3, single workspace, default-members excludes firmware → Task 1/8. ✓
- Out of scope (real decoders, SAI DMA, mobile) → left as documented stubs/TODOs. ✓
- Deviation: no `SampleFormat` enum (used `bits_per_sample`); noted in Global Constraints. ✓

**2. Placeholder scan:** No "TBD/TODO" in *requirements*. The two `TODO`s in firmware are intentional, documented scope boundaries (SAI DMA / clock config), not plan gaps. Embassy versions resolved via explicit `cargo add` step rather than guessed. ✓

**3. Type consistency:** `Decoder::decode_into(&mut [i32]) -> Result<usize, DecodeError>`, `StreamInfo { sample_rate, channels, bits_per_sample }`, `SliceReader::new`, `WavDecoder::new`, and `FromStd::new` are used identically across Tasks 2–8. ✓
