# soul-audio-core — Cross-Platform Embedded Rust Template

**Date:** 2026-08-23
**Status:** Approved design, pre-implementation
**Repo root:** `D:\dev\soulaudio\soul-audio-core` (existing git repo)

## 1. Purpose

Establish a modern, cross-platform Rust workspace that hosts an audio
decode/playback library (`soul-audio-core`) able to run **identically on an
STM32 (bare-metal, `no_std`) and on a desktop host**. Right now the deliverable
is the **template**: the workspace layout, the platform boundaries, the build
tooling, the test harness, and — critically — enforcement that the STM32 build
never regresses. The actual DSD/MP3/WAV/FLAC decoders are future work that will
fill in the boundaries this template defines; **WAV is implemented now as a
working reference vertical slice** to prove the whole pipeline end-to-end.

## 2. Fixed decisions (from brainstorming)

| Decision | Choice | Rationale |
|---|---|---|
| Core memory model | **Strict no-alloc** | Caller provides all buffers; no `Vec`/`Box`/heap in the library. Predictable RAM on STM32. |
| Runtime for shells | **Embassy (async)** on firmware | Modern async embedded standard; DMA/I2S ergonomics. Core stays sync/pure. |
| Desktop shell | **CLI test harness** (sync `std`) | Testing is via CLI for now — no desktop/mobile app. Core is sync, so the CLI drives it in a simple loop. |
| Mobile | **Deferred** | Core stays portable; no mobile shells/FFI yet. A non-event to add later. |
| STM32 target chip | **STM32H743ZI** (Nucleo-H743ZI2) | H7 = audio-capable (SAI), ~1 MB RAM (pairs well with no-alloc buffers), fast M7. Chip feature is a one-line swap. |
| Enforcement | **CI + local, both** | GitHub Actions gate + local `xtask`/aliases + pre-commit hook. |
| Workspace style | **Single Cargo workspace** (resolver 3) | Unified lockfile; resolver 3 stops host `std` features bleeding into the firmware build. |

## 3. Toolchain & standards

- **Rust 2024 edition**, workspace `resolver = "3"`.
- Toolchain pinned via `rust-toolchain.toml`, which also declares
  `targets = ["thumbv7em-none-eabihf"]` and components
  (`clippy`, `rustfmt`, `llvm-tools`, `rust-src`) — so a fresh checkout
  auto-installs the STM32 target for everyone (enforcement layer 0).
- Target for STM32H743 (Cortex-M7F): **`thumbv7em-none-eabihf`** (already installed).
- Shared `[workspace.lints]` (rust + clippy) applied to all crates.

## 4. Workspace layout

```
soul-audio-core/                       # repo root = workspace root
├── Cargo.toml                         # [workspace], resolver "3", shared lints/profiles, default-members
├── rust-toolchain.toml                # pin toolchain + thumbv7em target + components
├── .cargo/
│   └── config.toml                    # aliases + [target.thumbv7em-none-eabihf] linker cfg + probe-rs runner
├── crates/
│   ├── soul-audio-core/               # THE LIBRARY — #![cfg_attr(not(test), no_std)], strict no-alloc
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── decoder.rs             # Decoder trait, StreamInfo, DecodeError, SampleBuf
│   │   │   ├── io.rs                  # re-export/adapt embedded-io Read+Seek boundary
│   │   │   └── formats/
│   │   │       ├── mod.rs
│   │   │       ├── wav.rs             # WORKING reference decoder (16-bit PCM)
│   │   │       ├── mp3.rs             # stub: Unsupported + docs
│   │   │       ├── flac.rs            # stub
│   │   │       └── dsd.rs             # stub
│   │   └── tests/
│   │       └── decode.rs              # integration: decode fixtures via public API → golden PCM
│   ├── soul-audio-firmware/           # STM32H743 + Embassy (excluded from default build set)
│   │   ├── src/main.rs                # embassy init → decode include_bytes! WAV → defmt log; SAI DMA = documented TODO
│   │   ├── build.rs                   # linker script wiring
│   │   └── memory.x                   # (or embassy-stm32 `memory-x` feature) AXI SRAM for DMA
│   ├── soul-audio-cli/                # desktop test harness (std)
│   │   ├── src/main.rs                # clap; File → embedded-io-adapters → core → out.pcm; optional cpal playback
│   │   └── tests/
│   │       └── e2e.rs                 # black-box: run built binary via assert_cmd on fixture → check exit + output
│   └── architecture-tests/            # cross-cutting invariant tests (host)
│       ├── src/lib.rs                 # empty
│       └── tests/
│           ├── no_alloc.rs            # counting #[global_allocator]; assert 0 allocs during a decode
│           ├── layering.rs            # cargo_metadata: dependency-direction + core dep allowlist
│           └── compile_fail.rs        # trybuild: cases that MUST NOT compile
├── xtask/
│   └── src/main.rs                    # verify | ci | install-hooks | fixtures
├── testdata/
│   ├── sine_stereo_44100.wav          # tiny (few-ms) valid WAV; also include_bytes!'d by firmware
│   ├── sine_stereo_44100.pcm          # golden reference output (generated independently)
│   └── README.md                      # what each fixture is + how it was generated
├── .github/workflows/ci.yml
├── docs/superpowers/specs/            # this document
├── .gitignore
└── README.md
```

**`default-members`** = core, cli, architecture-tests, xtask (host crates). The
firmware is a workspace member but **excluded from the default set**, so
`cargo build`/`cargo test` at the root operate on host crates only. The firmware
builds via an explicit target (`cargo fw`).

## 5. Component design

### 5.1 `soul-audio-core` (the library)

- `#![cfg_attr(not(test), no_std)]` — strictly `no_std` in real builds; host unit
  tests may use `std`.
- **Strict no-alloc:** every decode API writes into a caller-provided
  `&mut [Sample]` and returns the number of samples produced. No allocation in
  library code, ever.
- **I/O boundary:** the [`embedded-io`](https://crates.io/crates/embedded-io)
  crate (`Read + Seek`). It is `no_std`-native and bridges to `std::io` via
  `embedded-io-adapters`, so the *same* decoder reads from an SD card on STM32
  and a `File` on desktop with no code change.
- **Public API (shape; exact types finalized in the plan):**
  - `SampleFormat`, `StreamInfo { sample_rate, channels, bits_per_sample }`.
  - Sample container: normalized **`i32`** (holds up to 32-bit / high-res;
    `StreamInfo` carries the true bit depth).
  - `trait Decoder { fn stream_info(&self) -> StreamInfo; fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError>; }`
  - `enum DecodeError { UnexpectedEof, Malformed, Unsupported, Io, ... }` (no_std, no `alloc`).
  - Format entry points: `formats::wav::WavDecoder::new(reader) -> Result<Self, DecodeError>` (working); `mp3`/`flac`/`dsd` return `Unsupported`.
- **No async, no runtime dependency** — pure sync so it drops into Embassy tasks
  or a CLI loop unchanged.

### 5.2 `soul-audio-firmware` (STM32H743 shell)

- `#![no_std] #![no_main]`, Embassy: `embassy-executor`, `embassy-stm32`
  (feature `stm32h743zi`), `embassy-time`; `cortex-m-rt`; logging via `defmt` +
  `defmt-rtt`; `panic-probe`.
- Memory layout via embassy-stm32 `memory-x` feature (or explicit `memory.x`),
  using **AXI SRAM** (DMA-accessible) for buffers.
- `build.rs` wires linker args; `.cargo/config.toml` supplies `-Tlink.x` /
  `-Tdefmt.x` and the `probe-rs run --chip STM32H743ZITx` runner.
- **Demonstration pipeline:** `include_bytes!("../../testdata/sine_stereo_44100.wav")`
  → wrap as an `embedded-io` cursor → `soul-audio-core` decode → `defmt::info!`
  sample stats (RMS/frame count). **Real SAI/I2S DMA output is a clearly-marked,
  documented `TODO`** (board/codec-specific; out of template scope). Compiles
  today; runs and logs over RTT if the board is attached.

### 5.3 `soul-audio-cli` (desktop test harness)

- Sync `std` binary, `clap` (derive). `soul-audio-cli decode <in.wav> --out <out.pcm>`.
- Wraps `std::fs::File` via `embedded-io-adapters`, drives the sync core in a
  loop, writes interleaved `i32` LE PCM to the output file.
- Optional `--features playback` pulls in `cpal`; **default build has zero
  audio-device dependencies** (keeps CI light and cross-platform).
- *Deliberate simplicity:* async matters on the STM32 side; the CLI stays sync.
  Running the exact async pipeline on desktop (Embassy `arch-std`) is a possible
  later addition, not part of this template.

### 5.4 `xtask` (automation)

Pure-Rust task runner (works identically on Windows and CI — no `make`/`just`):

- `cargo xtask verify` → `fmt --check` + `clippy -D warnings` + host `cargo test`
  + **`soul-audio-core` built for `thumbv7em-none-eabihf --no-default-features`**
  (the compile-time no_std/no-alloc gate) + firmware build.
- `cargo xtask ci` → the same, shaped for CI output.
- `cargo xtask install-hooks` → installs a git `pre-commit` hook running `verify`.
- `cargo xtask fixtures` → (re)generates `testdata/` fixtures + golden PCM.

## 6. Enforcement of the STM32 build (layered)

0. `rust-toolchain.toml` auto-installs the `thumbv7em` target on checkout.
1. **Compile-time gate:** `soul-audio-core` built for `thumbv7em --no-default-features`
   with no global allocator — any `std`/`alloc` creep fails to compile. Run by
   `xtask verify` and CI.
2. **Runtime gate:** `architecture-tests/no_alloc.rs` counts allocations during a
   real decode and asserts zero.
3. **Structural gate:** `architecture-tests/layering.rs` asserts core never
   depends on the shells and only on an allowlist of `no_std`-friendly crates.
4. **Local gate:** `cargo xtask install-hooks` → pre-commit runs `verify`.
5. **Remote gate:** GitHub Actions, two jobs:
   - `host`: `fmt --check`, `clippy -D warnings`, `cargo test`.
   - `embedded`: install `thumbv7em`, build `soul-audio-core` (no_std) **and**
     `soul-audio-firmware` for the target. PRs cannot merge if either breaks.

`.cargo/config.toml` aliases: `cargo fw` (build firmware), `cargo fw-run`
(flash+run via probe-rs), `cargo check-embedded` (the no_std gate alone).

## 7. Test strategy (three kinds, co-located by purpose)

1. **Integration** — `soul-audio-core/tests/decode.rs`: decode a `testdata/` WAV
   through the public API into caller buffers; assert bytes equal the golden
   `.pcm`. Proves decoder correctness.
2. **E2E** — `soul-audio-cli/tests/e2e.rs`: spawn the *real* CLI binary
   (`assert_cmd` + `predicates`) on a fixture; assert exit code and that the
   emitted file matches golden. Proves the whole desktop path.
3. **Architecture** — `architecture-tests/`: `no_alloc` (runtime no-alloc proof),
   `layering` (dependency-direction via `cargo_metadata`), `compile_fail`
   (`trybuild` cases that must not compile). Enforces the design invariants.

**Fixtures** (`testdata/`): tiny (a few ms) files kept small so the repo/CI stay
light. The golden `.pcm` is generated **independently** of `soul-audio-core`
(documented method in `testdata/README.md`) so tests can't validate the decoder
against its own output.

## 8. Out of scope (explicitly deferred)

- Real MP3/FLAC/DSD decoders (stubs only; the point is the boundaries).
- Real SAI/I2S DMA audio output on the STM32 (documented TODO).
- Desktop/mobile GUI apps and any FFI/UniFFI mobile layer.
- On-target `defmt-test` device tests (noted as a natural future add).

## 9. Success criteria

- `cargo test` (root) green: integration + E2E + architecture tests pass.
- `cargo xtask verify` green, including the `thumbv7em` no_std build of the core.
- `cargo fw` builds `soul-audio-firmware` for `thumbv7em-none-eabihf`.
- CI runs both `host` and `embedded` jobs and blocks merges on failure.
- `soul-audio-cli decode testdata/sine_stereo_44100.wav --out out.pcm` produces
  output byte-identical to the golden fixture.
- Adding a real decoder later requires touching only `formats/` + fixtures —
  no boundary or tooling changes.
```
