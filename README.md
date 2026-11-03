# soul-audio-core

[![CI](https://github.com/soulaudio/soul-audio-core/actions/workflows/ci-rust.yml/badge.svg)](https://github.com/soulaudio/soul-audio-core/actions/workflows/ci-rust.yml)
[![Security](https://github.com/soulaudio/soul-audio-core/actions/workflows/ci-security.yml/badge.svg)](https://github.com/soulaudio/soul-audio-core/actions/workflows/ci-security.yml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

A cross-platform, **`no_std` / no-alloc / `forbid(unsafe_code)`** audio
**decode + resample + playback** library in Rust — one core that runs identically
on the desktop and on an STM32H743 (bare-metal, Embassy).

The pipeline is **decode → resample → sink**:

- **Decode** WAV, FLAC (incl. FLAC-in-Ogg), MP3 (Layers I/II/III), AAC (AAC-LC,
  HE-AAC/SBR, HE-AACv2/PS), ALAC, Ogg Vorbis, Opus, and DSD to interleaved `i32`
  PCM. Every codec is a from-scratch Rust decoder — no C bindings.
- **Resample** to any rate with a windowed-sinc converter (anti-aliased both
  directions, Fast/HQ tiers, runtime-variable ratio).
- **Play** through a `Sink` — a cpal output device on the desktop (an SAI/I²S DMA
  sink on the STM32 is implemented, pending on-hardware bring-up).

Every codec is validated against external references (ffmpeg, minimp3, kjmp2,
libopus, and the RFC 8251 Opus test vectors) bit-exactly where the arithmetic
allows, and the whole decode + seek surface is fuzzed for panic/hang safety.

## Supported formats

| Container / codec | Details |
|---|---|
| **WAV** | PCM integer (8/16/24/32), IEEE float (32/64), G.711 A-law/µ-law, IMA/DVI-ADPCM, RF64/BW64 (>4 GiB), RIFX (big-endian), up to 8 channels |
| **FLAC** | Full decode, optional CRC-8/16 verification, up to 8 channels, sample-accurate seeking, **FLAC-in-Ogg** (also sample-accurate seeking) |
| **MP3** | MPEG-1/2/2.5 **Layers I, II, and III**, VBR (Xing/Info/VBRI), frame-granular + time seeking |
| **AAC** | **AAC-LC** (mono … 7.1, incl. 5.1 multichannel), **HE-AAC** (SBR), **HE-AACv2** (Parametric Stereo); MP4/M4A and raw ADTS (`.aac`); sample-accurate seeking |
| **ALAC** | Apple Lossless, MP4/M4A container, sample-accurate seeking |
| **Vorbis** | Ogg Vorbis, full decode (floor 1), up to 8 channels (5.1/7.1), page-granular seeking |
| **Opus** | From-scratch RFC 6716 decoder — SILK, CELT, and hybrid modes; Ogg-encapsulated (RFC 7845), RFC 8251-conformant; mono/stereo sample-accurate seeking, plus multichannel surround (5.1/7.1, mapping family 1/255; decode-only) |
| **DSD** | DSF and DFF/DSDIFF, DoP passthrough, decimation to PCM, DST decompression |

Output is interleaved `i32` in the source's sample range (16/24/32-bit), matching
across every format.

`open` handles WAV, FLAC, Ogg-FLAC, MP3, AAC (MP4 + ADTS), ALAC, and Opus
directly. **DSD** and **Ogg Vorbis** are constructed by hand instead of through
`open` — DSD needs an explicit output mode (native / DoP / decimate) and Vorbis
needs a caller-provided decode arena (see the DSD section below).

---

## Using the CLI

```sh
# Decode any supported file to interleaved i32 LE PCM
cargo run --release -p soul-audio-cli -- decode song.flac --out song.pcm

# Render files through the full gapless engine pipeline to a PCM file, offline
# (decode → resample → normalize → channel-map; no audio device needed)
cargo run --release -p soul-audio-cli -- render a.flac b.mp3 --out mix.pcm --rate 48000 --channels 2

# Play one or more files gaplessly on the default output device
cargo run --release -p soul-audio-cli --features playback -- play a.flac b.mp3 c.wav
```

`render` is the offline counterpart of `play` — the same `Engine`/`Deck` pipeline
into a file `Sink` instead of cpal. Because it needs no hardware, it is how the
playback pipeline is tested end-to-end in CI (`crates/soul-audio-cli/tests/e2e.rs`
asserts gapless concatenation, resampling, and channel mapping on the rendered
output).

`play` needs the `playback` feature (pulls in cpal + a lock-free ring buffer); the
default build omits it and prints a hint. Tracks play back-to-back with no gap
(the next one is decoded ahead on a loader thread), e.g.:

```
device: 48000 Hz 2ch (canonical 24-bit) — 3 track(s)
▶ track 0
▶ track 1 (gapless)
▶ track 2 (gapless)
done
```

---

## Using the library

The crate is not on crates.io yet; depend on it by git:

```toml
[dependencies]
soul-audio-core = { git = "https://github.com/soulaudio/soul-audio-core" }
```

### Decode a file to PCM

`open` probes the container and returns an `AnyDecoder` that streams interleaved
`i32` into your buffer. On the desktop the simplest source is a `SliceSource` over
the file bytes:

```rust
use soul_audio_core::{open, Decoder, SliceSource};

let bytes = std::fs::read("song.flac")?;
let mut decoder = open(SliceSource::new(&bytes))?;

let info = decoder.stream_info();
println!("{} Hz, {} channels, {} bits",
    info.sample_rate, info.channels, info.bits_per_sample());

let mut buf = [0i32; 4096];
loop {
    let n = decoder.decode_into(&mut buf)?;   // interleaved samples written
    if n == 0 { break; }                       // end of stream
    let pcm = &buf[..n];
    // … consume `pcm` (interleaved i32, `info.channels` per frame) …
}
```

The buffer can be any size — the decoder emits what fits and keeps the rest.
`decode_into` never allocates. For very large files, stream the bytes instead of
reading them all: wrap a `std::fs::File` with
[`embedded-io-adapters`](https://crates.io/crates/embedded-io-adapters) and the
crate's `BufReader` (see `crates/soul-audio-cli/src/sources.rs`).

### Resample to another rate

`Resampler` converts interleaved `i32` at a runtime-variable ratio. It uses a
**fixed-output-block** model: you ask how many input frames it needs, hand it
exactly that many, and it fills a fixed output block.

```rust
use soul_audio_core::{Resampler, Quality};

// stereo, 44.1 kHz → 48 kHz, no runtime ratio change, 1024-frame output blocks
let mut rs = Resampler::new(2, 44_100, 48_000, 1.0, 1024, Quality::Hq)?;

let mut out = [0i32; 1024 * 2];
loop {
    let need = rs.input_frames_next();          // input frames for the next block
    let input = /* fetch exactly `need` interleaved stereo frames */;
    rs.process(input, &mut out)?;               // fills exactly 1024 output frames
    // … consume `out` …
}
```

- `Quality::Fast` (~90 dB, compact) or `Quality::Hq` (~120 dB) tiers.
- `set_ratio(out_over_in, ramp)` changes the ratio at runtime (e.g. varispeed or
  clock-drift correction), slewed to avoid artifacts, bounded by the
  `max_rel_ratio` you passed to `new`.
- `latency()` reports the group delay in output frames.

### Playback (Deck + Engine + Sink)

Playback is a portable mechanism plus a per-platform shell. The library gives you
two pieces; you supply the concrete `Sink` and the track source:

- **`Deck<Sink, Decoder, PRELOAD>`** — the producer-side player. Per block it does
  **decode → resample → normalize → volume → channel-map → write**, resampling only
  when rates differ and duplicating mono → stereo automatically. It holds `PRELOAD`
  already-opened decoders and swaps to one at end-of-track for **gapless**
  transitions. It normalizes every track to canonical 24-bit, so the sink is
  bit-depth-agnostic. Transport: `load` / `preload` / `advance` / `play` / `pause` /
  `stop` / `seek` / `set_volume`; `pump()` does one block and returns `Progress`.
- **`soul-audio-engine`'s `Engine`** — the queue on top of a `Deck`: a cursor,
  `Normal`/`RepeatOne`/`RepeatAll`, and `next`/`prev`/`goto`. It decides which track
  index plays next and drives the deck's gapless preload, doing **no I/O**: it emits
  `poll_open() -> Option<index>` and you hand the opened decoder back via
  `provide(index, decoder)`. Opening (slow) happens on your loader thread.

Both are `no_std`, no-alloc, sync — the same code runs on the STM32 and the desktop.
Output goes to any `Sink`:

```rust
use soul_audio_core::{Sink, SinkError, StreamInfo};

pub trait Sink {
    fn stream_info(&self) -> StreamInfo;              // the device's rate/channels
    fn write(&mut self, frames: &[i32]) -> Result<(), SinkError>; // canonical 24-bit, backpressure
    fn flush(&mut self) -> Result<(), SinkError>;
}
```

`Sink::write` applies backpressure (blocks when the device buffer is full), which
paces the pump to the device clock — the same shape works for an embedded DMA sink,
where "block" means "wait for the next DMA buffer". The concrete sink owns a
lock-free SPSC ring whose consumer is the real-time callback / DMA ISR.

The desktop wiring is a worked example: the cpal `Sink` is in
[`crates/soul-audio-cli/src/cpal_sink.rs`](crates/soul-audio-cli/src/cpal_sink.rs),
and the pump-thread + loader-thread driver over the `Engine` is in
[`crates/soul-audio-cli/src/driver.rs`](crates/soul-audio-cli/src/driver.rs) — copy
them, or implement `Sink` + a loader for your own platform.

### Seeking

The `AnyDecoder` that `open` returns seeks over a **seekable** source (one
backed by a slice or file — not a live stream; the `SeekSource` bound enforces
this at compile time), and so do the concrete decoders directly:

```rust,ignore
use soul_audio_core::{open, SliceSource};

let bytes = std::fs::read("song.m4a")?;
let mut dec = open(SliceSource::new(&bytes))?;    // AnyDecoder
let landed = dec.seek_to(1_323_000)?;             // per-channel sample index
// dec.seek_to_time(30.0)?;                        // …or seconds
```

Every format `open` produces seeks: WAV, FLAC (native **and** FLAC-in-Ogg),
AAC (AAC-LC/HE-AAC/HE-AACv2, MP4 + ADTS), ALAC, and Opus are **sample-accurate**;
MP3 is **frame-granular** (a short warm-up transient after a seek, inherent to
the format). An out-of-range target clamps to the stream length rather than
erroring. (DSD and Ogg Vorbis, constructed directly rather than via `open`,
expose their own `seek_to` — Vorbis page-granular.)

### DSD

DSD needs an explicit output mode (native, DoP, or decimate-to-PCM), so it is
*not* routed through `open`. Construct `DsfDecoder` / `DffDecoder` and wrap it:

```rust,ignore
use soul_audio_core::formats::dsf::DsfDecoder;
use soul_audio_core::{DecimateAdapter, Decoder, SliceSource};

let bytes = std::fs::read("track.dsf")?;
let dsd = DsfDecoder::new(SliceSource::new(&bytes))?;
let mut pcm = DecimateAdapter::new(dsd);          // DSD → multi-bit PCM, plays anywhere
let mut buf = [0i32; 4096];
let n = pcm.decode_into(&mut buf)?;               // via the Decoder trait
```

`DopAdapter` instead wraps the 1-bit stream in DoP frames for a DoP-capable DAC.

---

## Architecture

```
                          soul-audio-core (no_std)          soul-audio-engine   app / platform
bytes ─▶ probe ─▶ Decoder ─▶ [Resampler] ─▶ normalize ─▶ Deck ──▶ Engine (queue) ──▶ Sink ─▶ device
        (open)    i32 PCM     to sink rate    +volume    (gapless)  next/prev/repeat   cpal / SAI-DMA
```

- **`soul-audio-core`** is sync, `no_std`, and allocates nothing. Decoders read
  through the `Source` capability ladder (`Source` → `BufSource` → `SeekSource`,
  owned super-traits over [`embedded-io`](https://crates.io/crates/embedded-io))
  and write PCM into caller-provided buffers. It holds the `Sink` trait and the
  **`Deck`** (the gapless playback mechanism).
- **`soul-audio-engine`** (also `no_std`) is the portable queue on top of a `Deck`:
  cursor + repeat modes + `next`/`prev`/`goto`, driving gapless preload with no I/O.
- The **concrete sinks and track sources** live in the shells because they need
  std / hardware — cpal on the desktop, SAI/I²S DMA on the STM32. The `Deck` +
  `Engine` above them are identical on both.
- Async lives only in the firmware shell (Embassy, for DMA/I²S); the core stays
  runtime-agnostic.
- The products built on this engine live in their own repositories — the
  [desktop player][soul-player] (cpal sink + Tauri/Svelte UI) and the
  [STM32 DAP firmware][soul-dap] (SAI/I²S DMA sink) — so this repo stays the
  reusable, platform-agnostic core.

[soul-player]: https://github.com/soulaudio/soul-player-ng
[soul-dap]: https://github.com/soulaudio/soul-dap-ng

## Project layout

| Crate | Role |
|---|---|
| `crates/soul-audio-core` | The library: decoders, resampler, `Sink` trait, `Deck`. `#![no_std]`, no-alloc, `forbid(unsafe_code)`. |
| `crates/soul-audio-engine` | Portable playback queue (cursor + repeat + gapless) over a `Deck`. `#![no_std]`, depends only on core. |
| `crates/soul-audio-queue` | Two-level queue: source context + Up-Next/history, shuffle/repeat (`#![no_std]`; `alloc` feature). |
| `crates/soul-audio-control` | Portable `no_std` `Player` facade (transport intents, prev-3s rule) over engine + queue. |
| `crates/soul-audio-tags` | `no_std` tag + cover-art reader (ID3v2, VorbisComment, MP4 `ilst`, RIFF `INFO`) with duration facts. |
| `crates/soul-audio-desktop` | Desktop building blocks: owned file decoder + cpal `CpalSink` (behind the `playback` feature). |
| `crates/soul-audio-cli` | Desktop harness: `decode` / `render` / gapless `play` (cpal sink + threaded driver). |
| `crates/architecture-tests` | Runtime no-alloc + dependency-layering invariants. |
| `xtask` | Pure-Rust automation (`verify`, the commit-msg policy, generated-table checks). |
| `testdata/` | Committed decode/resample fixtures. |

### Related repositories

This repo is the reusable engine; the products built on it live separately:

- **[soul-player]** — the cross-platform **desktop player** (Tauri 2 + Svelte 5)
  over the engine.
- **[soul-dap]** — the **STM32H743 DAP firmware** (Embassy) that runs this same
  `no_std` core on hardware.

## Building & developing

```sh
cargo test                       # host tests: unit + integration + DSP + robustness
cargo run -p soul-audio-cli --features playback -- play <file>...   # gapless
cargo xtask verify               # full gate: fmt, clippy -D warnings, tests, no_std cross-builds
cargo build -p soul-audio-core --target thumbv7em-none-eabihf --no-default-features  # no_std gate
```

### Testing & QA

The playback pipeline is verified end-to-end **without an audio device**: the
`render` command runs the same engine/deck path as `play` into a file, and
`crates/soul-audio-cli/tests/e2e.rs` asserts on the rendered PCM — gapless
concatenation, resampling (up/down), channel mapping, plus FFT signal analysis
for pitch, level, cleanliness (SFDR) and stereo routing. Real per-format
fixtures in `testdata/` are decoded in the same suite and checked bit-exact
(FLAC/Ogg-FLAC vs the reference WAV). All of this runs in `cargo test`.

```sh
cargo xtask qa-audio             # by-ear QA: play every testdata/ fixture + a gapless queue
```

`qa-audio` is the one check tests can't make — that it actually *sounds* right
through a real device. It plays the same signal in each format (WAV/FLAC/MP3/
Ogg-FLAC, plus a DSD64 DSF decimated to PCM) with what to listen for, then a
gapless WAV→FLAC queue. Needs the `playback` feature (pulled in automatically).
See `testdata/README.md`.

Git hooks (via [lefthook](https://github.com/evilmartians/lefthook)): a
`commit-msg` hook enforces [Conventional Commits](https://www.conventionalcommits.org)
and a clean history (no tool-attribution trailers), pre-commit runs fmt + clippy
on staged files, and pre-push runs the full `cargo xtask verify`. Install the
lefthook binary and run `lefthook install` once.

The STM32 discipline is enforced at several layers — the pinned `thumbv7em`
toolchain target, a compile-time `no_std` gate, a runtime no-alloc proof, and a
dependency-layering test — all run by `cargo xtask verify`. See
`docs/superpowers/specs/` for the design specs of each subsystem.

## Credits & acknowledgements

Every codec here is a from-scratch Rust implementation, but it stands on decades
of prior work. With thanks to:

**Reference implementations** studied and cross-checked against:

- [minimp3](https://github.com/lieff/minimp3) — the MP3 Layer III approach.
- [kjmp2](https://keyj.emphy.de/kjmp2/) by Martin J. Fiedler — MP3 Layers I/II.
- [libsamplerate](https://github.com/libsndfile/libsamplerate) ("Secret Rabbit
  Code") by Erik de Castro Lopo, and Julius O. Smith's
  [windowed-sinc resampling](https://ccrma.stanford.edu/~jos/resample/) notes
  (CCRMA) — the resampler.
- [FFmpeg](https://ffmpeg.org) — decode goldens and an independent fixed-point
  cross-check for AAC.
- [libopus](https://github.com/xiph/opus) — the Opus reference decoder.
- Apple's [ALAC](https://github.com/macosforge/alac) reference and David Bryant's
  [WavPack](https://github.com/dbry/WavPack).

**Specifications** implemented against:

- Opus — [RFC 6716](https://www.rfc-editor.org/rfc/rfc6716),
  [RFC 7845](https://www.rfc-editor.org/rfc/rfc7845) (Ogg encapsulation) and
  [RFC 8251](https://www.rfc-editor.org/rfc/rfc8251) (corrections).
- [FLAC format](https://xiph.org/flac/format.html) and the
  [Vorbis I specification](https://xiph.org/vorbis/doc/Vorbis_I_spec.html).
- AAC — ISO/IEC 13818-7 and 14496-3 (LC, SBR, PS).
- Matroska / WebM — [matroska.org](https://www.matroska.org) and
  [RFC 9559](https://www.rfc-editor.org/rfc/rfc9559).
- Apple AIFF-C and Core Audio Format (CAF); Sony/Philips DSF & DSDIFF (DSD);
  ITU-T G.711 (A-law / µ-law).

**Conformance & test corpus:**

- The Opus [RFC 8251](https://www.rfc-editor.org/rfc/rfc8251) test vectors and the
  ISO/IEC MPEG audio conformance vectors (used locally; not redistributed here).
- Real-music fixtures from the
  [Open Goldberg Variations](http://opengoldbergvariations.org/) — J.S. Bach
  performed by Kimiko Ishizaka, released into the public domain (**CC0**) by a
  [MuseScore](https://musescore.org) & Kimiko Ishizaka project. Thank you for
  freeing this music.

## License

Dual-licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT)
at your option. Unless you state otherwise, any contribution you intentionally
submit for inclusion is dual-licensed as above, with no additional terms.
