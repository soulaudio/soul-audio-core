# soul-audio-core

A cross-platform Rust **template** for an audio decode/playback library that
runs on an STM32H743 (bare-metal, Embassy) **and** on the desktop — from one
strict-`no_std`, no-alloc core. The decoders (WAV, MP3, FLAC, DSD) are
**stubs**: the whole project is wired, builds, tests, and enforces the embedded
discipline, so you can drop real decoders straight into `formats/`.

## Layout

| Crate | Role |
|---|---|
| `crates/soul-audio-core` | The library. `#![no_std]`, no-alloc, `forbid(unsafe_code)`. Defines the `Decoder` boundary over `embedded-io::Read`. |
| `crates/soul-audio-firmware` | STM32H743 + Embassy shell. Builds for `thumbv7em-none-eabihf`. |
| `crates/soul-audio-cli` | Desktop test harness (`clap`). |
| `crates/architecture-tests` | Runtime no-alloc + dependency-layering invariants. |
| `xtask` | Pure-Rust automation (`verify` / `ci` / `install-hooks`). |
| `testdata/` | Committed fixtures (add your own clips + golden PCM here). |

## Common commands

| Command | What it does |
|---|---|
| `cargo test` | Host tests: integration, E2E, and architecture invariants. |
| `cargo check-embedded` | Build the core `no_std` for STM32 — the discipline gate. |
| `cargo fw` | Build the firmware for `thumbv7em-none-eabihf`. |
| `cargo fw-run` | Flash + run on hardware via `probe-rs` (needs a board). |
| `cargo xtask verify` | Full local gate: fmt, clippy, tests, both embedded builds. |
| `cargo xtask install-hooks` | Install a pre-commit hook that runs `verify`. |

## How the cross-platform split works

- **`soul-audio-core` is sync, `no_std`, and allocates nothing.** Decoders read
  from any `embedded-io::Read` and write PCM into caller-provided buffers.
- On the **desktop**, `soul-audio-cli` wraps a `std::fs::File` with
  `embedded-io-adapters`; on the **STM32**, the firmware uses
  `soul_audio_core::SliceReader` over in-flash bytes. Same core code path.
- Async lives only in the **firmware shell** (Embassy, for DMA/I2S); the core
  stays runtime-agnostic.

## STM32-build enforcement (layered)

0. `rust-toolchain.toml` auto-installs the `thumbv7em-none-eabihf` target.
1. **Compile-time:** core built for `thumbv7em --no-default-features` — any
   `std`/`alloc` creep fails to compile (`cargo check-embedded`).
2. **Runtime:** `architecture-tests` asserts zero allocations during core work.
3. **Structural:** `architecture-tests` asserts the dependency layering.
4. **Local:** `cargo xtask verify` + the pre-commit hook.
5. **Remote:** GitHub Actions (`host` + `embedded` jobs) block merges on failure.

## Target chip

Defaults to **STM32H743ZI** (Nucleo-H743ZI2). To change it, edit the
`embassy-stm32` chip feature in `crates/soul-audio-firmware/Cargo.toml` and the
`--chip` in `.cargo/config.toml`'s `probe-rs` runner.

## Prerequisites

- Rust (the toolchain + STM32 target install automatically from
  `rust-toolchain.toml`).
- For flashing: `cargo install probe-rs-tools` and an STM32H743 board.

## Adding a real decoder

Implement it under `crates/soul-audio-core/src/formats/` (parse in `new`,
implement `Decoder` to stream PCM into caller buffers, no allocation). Add a
clip + independently-generated golden PCM under `testdata/`, then extend the
integration test in `crates/soul-audio-core/tests/decode.rs` and the CLI/E2E.
No boundary or tooling changes needed.
