# Desktop Player — Plan 2: `soul-audio-desktop` shared lib + CLI migration

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract the reusable std desktop-playback bits (cpal sink, owned file decoder, file sources) out of `soul-audio-cli` into a new `soul-audio-desktop` library crate, and migrate the CLI onto it — so the Tauri backend (Plan 3) and the CLI share one implementation.

**Architecture:** A new std workspace member `crates/soul-audio-desktop` holds three modules — `sources` (the file/tcp source ladder), `decoder` (`OwnedDecoder` + `open_track`), and `sink` (`CpalSink`, behind a `playback` feature). It depends on `soul-audio-core` only; the sink calls the already-shipped `soul_audio_core::convert`. The CLI keeps its own fixed-queue orchestration (`run`/`play`/`render`/`RenderSink` in `driver.rs`) and now imports the moved pieces from the library. This is a **mechanical migration**: the CLI's behavior and its e2e tests are unchanged.

**Tech Stack:** Rust (edition 2024, rust 1.85), `soul-audio-core`, `cpal` 0.15 + `ringbuf` 0.4 (behind `playback`), `embedded-io-adapters` (std), `anyhow`.

## Global Constraints

- Every crate is `#![forbid(unsafe_code)]`; edition 2024; rust-version 1.85 (all via `workspace`).
- `soul-audio-desktop` is **std** — a root-workspace **member and default-member**; it is NOT cross-built for `thumbv7em` (no xtask gate line).
- `soul-audio-desktop` depends on **`soul-audio-core` only** — NOT `soul-audio-engine` (the engine-using `run` stays in the CLI). Keep it lean.
- The migration is **mechanical**: the CLI's public behavior and its `tests/e2e.rs` are unchanged; the moved unit tests keep passing in their new home.
- `cargo xtask verify` must stay green: workspace-wide `clippy --all-targets -- -D warnings`, the full `cargo test`, and the unchanged `thumbv7em` builds of core/engine/queue/control/firmware.
- **Workspace-wide pre-commit clippy** ⇒ each crate's API change commits **atomically with its consumer (the CLI)**: every commit must leave the whole workspace compiling and clippy-clean.
- `cargo xtask verify` does NOT build the `playback` feature (cpal needs system audio libs; kept out of CI exactly as today). The `playback` path is validated by an explicit `--features playback` build in Task 3 on this (Windows) dev machine — do not add a playback build to `verify()`.
- No `Co-Authored-By: Claude` / `Claude-Session:` commit trailers; never `--no-verify`.

## File Structure

```
crates/soul-audio-desktop/
  Cargo.toml            NEW  — std lib; core dep; `playback` feature → cpal + ringbuf
  src/
    lib.rs              NEW  — #![forbid(unsafe_code)]; module decls + re-exports
    sources.rs          NEW  — moved from cli/src/sources.rs (file_source, tcp_source, tests)   [Task 1]
    decoder.rs          NEW  — moved from cli/src/driver.rs  (Src, OwnedDecoder, open_track)     [Task 2]
    sink.rs             NEW  — moved from cli/src/cpal_sink.rs (CpalSink), convert import swapped [Task 3]

crates/soul-audio-cli/
  Cargo.toml            MOD  — add soul-audio-desktop dep [T1]; drop embedded-io(-adapters) [T2]; drop cpal/ringbuf, forward playback [T3]
  src/main.rs           MOD  — drop `mod sources` + use desktop::file_source [T1]; drop `mod convert`/`mod cpal_sink` [T3]
  src/driver.rs         MOD  — import OwnedDecoder/open_track from desktop, delete the moved block [T2]; swap CpalSink import [T3]
  src/sources.rs        DEL  [T1]
  src/cpal_sink.rs      DEL  [T3]
  src/convert.rs        DEL  [T3]  (its math + tests already live in soul_audio_core::convert)

Cargo.toml (root)       MOD  — add soul-audio-desktop to members + default-members  [T1]
crates/architecture-tests/tests/layering.rs  MOD — add soul-audio-desktop to the shell check [T1]
```

**TDD note (refactor):** this is a pure extraction, so the "failing test first" cycle is replaced by *the existing suite stays green*. Each task's gate is the moved unit tests plus the unchanged CLI e2e. No new behavior, so no new failing tests are written; the value is proving behavior is preserved across the move.

---

### Task 1: Scaffold `soul-audio-desktop` + move the file-source ladder

**Files:**
- Create: `crates/soul-audio-desktop/Cargo.toml`
- Create: `crates/soul-audio-desktop/src/lib.rs`
- Create: `crates/soul-audio-desktop/src/sources.rs`
- Delete: `crates/soul-audio-cli/src/sources.rs`
- Modify: `crates/soul-audio-cli/src/main.rs` (remove `mod sources;`; call `soul_audio_desktop::file_source`)
- Modify: `crates/soul-audio-cli/Cargo.toml` (add the `soul-audio-desktop` dep)
- Modify: `Cargo.toml` (root — add to `members` + `default-members`)
- Modify: `crates/architecture-tests/tests/layering.rs` (add `soul-audio-desktop` to the shell check)

**Interfaces:**
- Produces: `soul_audio_desktop::file_source(file: std::fs::File, buf: &mut [u8]) -> soul_audio_core::BufReader<'_, embedded_io_adapters::std::FromStd<File>>` and `soul_audio_desktop::tcp_source(...)`. Consumed by the CLI's `decode` (this task) and available to later tasks.

- [ ] **Step 1: Create the crate manifest** — `crates/soul-audio-desktop/Cargo.toml`. (cpal/ringbuf are declared now though only `sink` (Task 3) uses them — an optional dep behind an off feature costs nothing and avoids editing this file three times.)

```toml
[package]
name = "soul-audio-desktop"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true
description = "Shared std desktop-playback building blocks for soul-audio (cpal sink, owned file decoder, file sources)."

[lints]
workspace = true

[dependencies]
soul-audio-core = { path = "../soul-audio-core" }
embedded-io = { workspace = true }
embedded-io-adapters = { workspace = true, features = ["std"] }
anyhow = "1"

[features]
default = []
playback = ["dep:cpal", "dep:ringbuf"]

[dependencies.cpal]
version = "0.15"
optional = true

[dependencies.ringbuf]
version = "0.4"
optional = true
```

- [ ] **Step 2: Create `src/lib.rs`** — only the `sources` module lands this task; `decoder`/`sink` are added in Tasks 2/3.

```rust
//! Shared **std desktop-playback** building blocks for `soul-audio`, extracted
//! from the CLI so the desktop app (Plan 3) and the CLI share one
//! implementation: concrete file sources, the owned decoder + `open_track`, and
//! (behind the `playback` feature) the cpal sink. Pure canonical-24-bit →
//! device-format conversion lives in `soul_audio_core::convert`; the sink calls
//! it, so a future DMA sink shares the same math.
#![forbid(unsafe_code)]

pub mod sources;

pub use sources::{file_source, tcp_source};
```

- [ ] **Step 3: Create `src/sources.rs`** — move the body of `crates/soul-audio-cli/src/sources.rs` **verbatim**, with one change: **remove** the `#[allow(dead_code)]` line above `tcp_source` (it is now a public library API, so it is not dead code). Keep the module doc, `file_source`, `tcp_source`, and the entire `#[cfg(test)] mod tests { ... }` unchanged.

- [ ] **Step 4: Delete `crates/soul-audio-cli/src/sources.rs`.**

- [ ] **Step 5: Rewire the CLI.** In `crates/soul-audio-cli/src/main.rs`:
  - Delete the module line `mod sources;` (currently the last of the `mod` declarations, line 20).
  - In `fn decode`, change the source construction to call the library:

```rust
    let source = soul_audio_desktop::file_source(file, &mut scratch);
```

- [ ] **Step 6: Add the dep** — in `crates/soul-audio-cli/Cargo.toml`, under `[dependencies]`, add:

```toml
soul-audio-desktop = { path = "../soul-audio-desktop" }
```

- [ ] **Step 7: Register the crate** — in the root `Cargo.toml`, add `"crates/soul-audio-desktop",` to **both** `members` and `default-members` (place it after `"crates/soul-audio-control",` in each list, mirroring the CLI's position).

- [ ] **Step 8: Extend the layering invariant** — in `crates/architecture-tests/tests/layering.rs`, in `fn shells_depend_on_core_not_the_reverse`, add `"soul-audio-desktop"` to the array so the loop reads:

```rust
    for shell in ["soul-audio-cli", "soul-audio-desktop", "soul-audio-firmware"] {
```

(This asserts `soul-audio-desktop` depends on `soul-audio-core` and that core does not depend on it — true from this task, since `sources` uses `soul_audio_core::BufReader`.)

- [ ] **Step 9: Verify the move.** Expected: all pass / clean.

```
cargo test -p soul-audio-desktop        # the 4 moved source-ladder tests pass
cargo test -p soul-audio-cli            # e2e unchanged (decode now uses the moved file_source)
cargo test -p architecture-tests        # layering passes (desktop → core)
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 10: Commit.**

```
git add -A
git commit -m "refactor(desktop): scaffold soul-audio-desktop, move the file-source ladder from the CLI"
```

---

### Task 2: Move `OwnedDecoder` + `open_track` into the library

**Files:**
- Create: `crates/soul-audio-desktop/src/decoder.rs`
- Modify: `crates/soul-audio-desktop/src/lib.rs` (add the `decoder` module + re-exports)
- Modify: `crates/soul-audio-cli/src/driver.rs` (delete the moved block; import from the library; prune now-unused imports)
- Modify: `crates/soul-audio-cli/Cargo.toml` (drop `embedded-io` + `embedded-io-adapters` — now unused in the CLI)

**Interfaces:**
- Consumes: nothing new.
- Produces: `soul_audio_desktop::OwnedDecoder` (a `Decoder + SeekableDecoder` over file sources; `pub` enum, opaque PCM/DSD variants), `soul_audio_desktop::open_track(path: &std::path::Path) -> anyhow::Result<OwnedDecoder>`, and the alias `soul_audio_desktop::Src`. The CLI's `run` builds `Deck::<K, OwnedDecoder, PRELOAD>` and calls `open_track` on the loader thread; Plan 3's backend consumes the same two.

- [ ] **Step 1: Create `src/decoder.rs`** — move the decoder half of the CLI's `driver.rs` (the `Src` alias, the `OwnedDecoder` enum + its `Decoder`/`SeekableDecoder` impls, and `open_track`) into this new module, made **public**. Full file:

```rust
//! The owned, `Send` file decoder the deck holds by value and the loader thread
//! moves across the channel: multi-bit PCM (WAV/FLAC/MP3/Ogg-FLAC/AAC/ALAC) or
//! DSD (DSF/DFF) decimated to PCM. Built off the pump thread by [`open_track`].

use std::fs::File;
use std::io::BufReader as IoBufReader;
use std::path::Path;

use anyhow::{Result, anyhow};
use embedded_io_adapters::std::FromStd;
use soul_audio_core::formats::dff::DffDecoder;
use soul_audio_core::formats::dsf::DsfDecoder;
use soul_audio_core::{
    AnyDecoder, DecimateAdapter, DecodeError, Decoder, Format, SeekableDecoder, StreamInfo, open,
    probe,
};

/// A fully **owned**, `Send` byte source: std buffers the file, so the decoder
/// built on it holds no borrowed scratch and can live in the deck and move
/// across threads.
pub type Src = FromStd<IoBufReader<File>>;

/// A playable owned decoder: multi-bit PCM (WAV/FLAC/MP3/Ogg-FLAC) or DSD
/// (DSF/DFF) decimated to PCM. DSD is decimated in-decoder (DSD64 → 352.8 kHz
/// 24-bit) and the deck then resamples it to the device rate like any other
/// source.
///
/// The PCM variant is `AnyDecoder` (~150 KiB by value, FLAC-dominated) — much
/// larger than the DSD variants — but the deck holds decoders by value on a
/// big-stack thread by design (matching the core's no-alloc `AnyDecoder`), so
/// the size gap is accepted rather than boxed.
#[allow(clippy::large_enum_variant)]
pub enum OwnedDecoder {
    Pcm(AnyDecoder<Src>),
    Dsf(DecimateAdapter<DsfDecoder<Src>>),
    Dff(DecimateAdapter<DffDecoder<Src>>),
}

impl Decoder for OwnedDecoder {
    fn stream_info(&self) -> StreamInfo {
        match self {
            OwnedDecoder::Pcm(d) => d.stream_info(),
            OwnedDecoder::Dsf(d) => d.stream_info(),
            OwnedDecoder::Dff(d) => d.stream_info(),
        }
    }
    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        match self {
            OwnedDecoder::Pcm(d) => d.decode_into(out),
            OwnedDecoder::Dsf(d) => d.decode_into(out),
            OwnedDecoder::Dff(d) => d.decode_into(out),
        }
    }
}

impl SeekableDecoder for OwnedDecoder {
    fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        match self {
            OwnedDecoder::Pcm(d) => d.seek_to(sample),
            // The decimator's PCM frame ↔ DSD sample mapping lives in its inherent
            // `seek_to_frame`.
            OwnedDecoder::Dsf(d) => d.seek_to_frame(sample),
            OwnedDecoder::Dff(d) => d.seek_to_frame(sample),
        }
    }
    fn total_frames(&self) -> Option<u64> {
        match self {
            OwnedDecoder::Pcm(d) => d.total_frames(),
            // The DSD length isn't surfaced through the decimator; streaming.
            OwnedDecoder::Dsf(_) | OwnedDecoder::Dff(_) => None,
        }
    }
}

/// Open `path` into an owned decoder: probe the container, then build the PCM
/// codec or wrap a DSD decoder in the DSD→PCM decimator.
pub fn open_track(path: &Path) -> Result<OwnedDecoder> {
    let file = File::open(path).map_err(|e| anyhow!("opening {}: {e}", path.display()))?;
    // 64 KiB of std buffering; the decoder reads through it via embedded-io.
    let mut src = FromStd::new(IoBufReader::with_capacity(64 * 1024, file));
    let format = probe(&mut src).map_err(|e| anyhow!("probing {}: {e:?}", path.display()))?;
    match format {
        Format::Dsf => Ok(OwnedDecoder::Dsf(DecimateAdapter::new(
            DsfDecoder::new(src).map_err(|e| anyhow!("DSF {}: {e:?}", path.display()))?,
        ))),
        Format::Dff => Ok(OwnedDecoder::Dff(DecimateAdapter::new(
            DffDecoder::new(src).map_err(|e| anyhow!("DFF {}: {e:?}", path.display()))?,
        ))),
        // WAV/FLAC/MP3/Ogg-FLAC — `open` re-probes (cheap) and builds the codec.
        _ => {
            Ok(OwnedDecoder::Pcm(open(src).map_err(|e| {
                anyhow!("decoding {}: {e:?}", path.display())
            })?))
        }
    }
}
```

- [ ] **Step 2: Export it** — in `crates/soul-audio-desktop/src/lib.rs`, add the module and re-export. The file becomes:

```rust
//! Shared **std desktop-playback** building blocks for `soul-audio`, extracted
//! from the CLI so the desktop app (Plan 3) and the CLI share one
//! implementation: concrete file sources, the owned decoder + `open_track`, and
//! (behind the `playback` feature) the cpal sink. Pure canonical-24-bit →
//! device-format conversion lives in `soul_audio_core::convert`; the sink calls
//! it, so a future DMA sink shares the same math.
#![forbid(unsafe_code)]

pub mod decoder;
pub mod sources;

pub use decoder::{OwnedDecoder, Src, open_track};
pub use sources::{file_source, tcp_source};
```

- [ ] **Step 3: Delete the moved block from the CLI's `driver.rs`.** Remove the `Src` type alias, the `OwnedDecoder` enum and its two `impl` blocks, and the `open_track` fn (the contiguous block from the `type Src = ...` line through the end of `open_track`). Keep `PRELOAD`, `run`, `RenderSink`, `render`, and `play`.

- [ ] **Step 4: Fix `driver.rs` imports.** Replace the entire top import block (from the first `use std::...` through the `#[cfg(feature = "playback")] use crate::cpal_sink::CpalSink;` line) with exactly:

```rust
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use soul_audio_core::model::SampleFormat;
use soul_audio_core::{Deck, Progress, Quality, Sink, SinkError, StreamInfo};
use soul_audio_desktop::{OwnedDecoder, open_track};
use soul_audio_engine::{Engine, LinearSequencer, Provided};

#[cfg(feature = "playback")]
use crate::cpal_sink::CpalSink;
```

(`File` stays — `RenderSink::new` uses `File::create`. `SampleFormat` stays — `RenderSink::stream_info`. The decoder-only imports — `IoBufReader`, `FromStd`, `AnyDecoder`, `DecimateAdapter`, `DecodeError`, `Decoder`, `Format`, `SeekableDecoder`, the `formats::*`, `open`, `probe` — are gone with the moved block.)

- [ ] **Step 5: Drop the now-unused CLI deps.** In `crates/soul-audio-cli/Cargo.toml`, delete the `embedded-io` and `embedded-io-adapters` lines from `[dependencies]` (the CLI no longer references `embedded_io*` directly — the only two uses were in the now-moved `sources.rs` and the decoder block).

- [ ] **Step 6: Verify.** Expected: pass / clean.

```
cargo build -p soul-audio-desktop
cargo build -p soul-audio-cli
cargo test -p soul-audio-cli            # e2e render/decode drive open_track via the binary — still green
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 7: Commit.**

```
git add -A
git commit -m "refactor(desktop): move OwnedDecoder + open_track into soul-audio-desktop"
```

---

### Task 3: Move `CpalSink` (feature `playback`) over `soul_audio_core::convert`; drop the CLI's `convert`/`cpal_sink`

**Files:**
- Create: `crates/soul-audio-desktop/src/sink.rs`
- Modify: `crates/soul-audio-desktop/src/lib.rs` (add the feature-gated `sink` module + re-export)
- Delete: `crates/soul-audio-cli/src/cpal_sink.rs`
- Delete: `crates/soul-audio-cli/src/convert.rs`
- Modify: `crates/soul-audio-cli/src/main.rs` (drop `mod convert;` + its comment, and `mod cpal_sink;`)
- Modify: `crates/soul-audio-cli/src/driver.rs` (swap the `CpalSink` import to the library)
- Modify: `crates/soul-audio-cli/Cargo.toml` (drop `cpal`/`ringbuf`; forward the `playback` feature)

**Interfaces:**
- Consumes: `soul_audio_core::convert::{fill_f32, fill_i16, CANONICAL_BITS}` (shipped in Plan 1).
- Produces: `soul_audio_desktop::CpalSink` (a `Sink` over the default output device; behind `playback`). Plan 3's pump thread constructs it.

- [ ] **Step 1: Create `src/sink.rs`** — move `crates/soul-audio-cli/src/cpal_sink.rs` **verbatim**, changing only the `convert` import to the shipped core module. That is, replace:

```rust
use crate::convert::{self, CANONICAL_BITS};
```

with:

```rust
use soul_audio_core::convert::{self, CANONICAL_BITS};
```

Everything else (the `CpalSink` struct, `new`, the `Sink` impl, the `convert::fill_f32`/`convert::fill_i16` callback wiring, `CANONICAL_BITS` in `stream_info`) is unchanged. `soul_audio_core::convert` exposes exactly `CANONICAL_BITS`, `fill_f32`, and `fill_i16` — same names, same signatures.

- [ ] **Step 2: Export it** — in `crates/soul-audio-desktop/src/lib.rs`, add the feature-gated module and re-export. Final file:

```rust
//! Shared **std desktop-playback** building blocks for `soul-audio`, extracted
//! from the CLI so the desktop app (Plan 3) and the CLI share one
//! implementation: concrete file sources, the owned decoder + `open_track`, and
//! (behind the `playback` feature) the cpal sink. Pure canonical-24-bit →
//! device-format conversion lives in `soul_audio_core::convert`; the sink calls
//! it, so a future DMA sink shares the same math.
#![forbid(unsafe_code)]

pub mod decoder;
pub mod sources;
#[cfg(feature = "playback")]
pub mod sink;

pub use decoder::{OwnedDecoder, Src, open_track};
pub use sources::{file_source, tcp_source};
#[cfg(feature = "playback")]
pub use sink::CpalSink;
```

- [ ] **Step 3: Delete the CLI copies.** Delete `crates/soul-audio-cli/src/cpal_sink.rs` and `crates/soul-audio-cli/src/convert.rs`. (The `convert` math and its two unit tests already live in `soul_audio_core::convert` as of Plan 1 — no coverage is lost.)

- [ ] **Step 4: Trim `main.rs`.** Remove the `convert`/`cpal_sink` module declarations and the `convert` dead-code comment. The `mod` region at the top of `main.rs` reduces to exactly:

```rust
mod driver;
```

- [ ] **Step 5: Swap the `CpalSink` import in `driver.rs`.** Change:

```rust
#[cfg(feature = "playback")]
use crate::cpal_sink::CpalSink;
```

to:

```rust
#[cfg(feature = "playback")]
use soul_audio_desktop::CpalSink;
```

- [ ] **Step 6: Forward the feature in `crates/soul-audio-cli/Cargo.toml`.** Replace the `[features]` section and delete the `[dependencies.cpal]` and `[dependencies.ringbuf]` sections. The `[features]` section becomes:

```toml
[features]
default = []
playback = ["soul-audio-desktop/playback"]
```

(The `soul-audio-desktop` dependency line added in Task 1 stays as-is — default-featured; the CLI's `playback` feature turns on the library's.)

- [ ] **Step 7: Verify — including the playback path.** Expected: all compile / pass / `verify: OK`.

```
cargo build -p soul-audio-desktop --features playback
cargo build -p soul-audio-cli --features playback
cargo build -p soul-audio-cli
cargo test -p soul-audio-cli
cargo xtask verify
```

`cargo xtask verify` runs the full gate (fmt, workspace clippy `-D warnings`, generated-table checks, the whole test suite, and the thumbv7em builds of core/engine/queue/control/firmware) and must print `verify: OK`. Allow ~10 minutes; run from a non-nested checkout so the firmware links.

- [ ] **Step 8: Commit.**

```
git add -A
git commit -m "refactor(desktop): move CpalSink to soul-audio-desktop over soul_audio_core::convert; drop CLI convert/cpal_sink"
```

---

## Self-Review

**Spec coverage (design §3, §7, §8):**
- §3 "extract `CpalSink`, `OwnedDecoder`+`open_track`, the file-source wiring" → Tasks 3/2/1. ✓
- §3 "convert math does NOT stay — it moved to core; `CpalSink` calls it" → Task 3 Step 1 (import swap to `soul_audio_core::convert`; CLI `convert.rs` deleted). ✓
- §3 "CLI depends on `soul-audio-desktop` and keeps its own `run`/`play`/`render`" → CLI `driver.rs` retains `run`/`RenderSink`/`render`/`play`; imports moved pieces. ✓
- §7 "`soul-audio-desktop` is a root-workspace member covered by `cargo xtask verify`" → root `members` + `default-members` (Task 1 Step 7); std-only, no thumbv7em line. ✓
- §8 "extraction keeps `verify` green; CLI e2e unchanged" → each task re-runs the CLI e2e; Task 3 runs full `verify`. ✓

**Placeholder scan:** none — every file body / import block / manifest section is given in full or as an exact line swap.

**Type consistency:** `OwnedDecoder`, `Src`, `open_track`, `file_source`, `tcp_source`, `CpalSink` are named identically in the library, its re-exports, and the CLI's imports. `soul_audio_core::convert::{fill_f32, fill_i16, CANONICAL_BITS}` matches the shipped signatures (verified against `crates/soul-audio-core/src/convert.rs`). The layering test's `shell` array and the root member lists both gain `soul-audio-desktop`.

**Atomicity:** every task ends with the whole workspace compiling + clippy-clean and the CLI e2e green — satisfying the workspace-wide-clippy atomic-commit constraint. Dependency prunes (`embedded-io*` in T2, `cpal`/`ringbuf` in T3) happen in the same task that removes their last use.
