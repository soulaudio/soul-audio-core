# DAP Firmware — SAI/I²S Output (sub-project A of #32) Design Spec

**Date:** 2026-10-08
**Issue:** #32 (DAP firmware: real on-device decoders + SAI/I²S DMA output) —
this spec is **sub-project A** only; B (SD bring-up) and C (SD→deck→Sink
integration + budget) are separate brainstorm→plan→execution cycles.

**Goal:** Stand up the STM32H743 **SAI/I²S audio output path** and prove it with
an in-flash test tone — the firmware's first audible output and the seam every
later phase feeds into.

## Hardware target & testability (read first)

- **Board:** Nucleo-H743ZI (STM32H743ZI, Cortex-M7, 1 MiB RAM, 2 MiB flash) —
  matches the crate's existing `stm32h743zi` embassy feature.
- **DAC:** external **PCM5102** I²S DAC (the common jumper-wired breakout). SAI
  is **master**, providing BCLK + LRCK + DATA + MCLK; **no I²C codec init** (the
  PCM5102 is configured by pin straps). Document the breakout wiring/jumpers
  (SCK/MCLK, FMT, XSMT) in the firmware module docs.
- **NO PHYSICAL HARDWARE is available yet.** Therefore the **only automated gate
  is a thumbv7em cross-build** (`cargo xtask verify`'s firmware link + a direct
  `cargo build --target thumbv7em-none-eabihf -p soul-audio-firmware`). "Plays an
  audible tone" is a **deferred, on-hardware acceptance** the owner runs when a
  board exists. The code is written against the real embassy-stm32 0.6 API and
  the real PCM5102/48 kHz parameters so it is flash-ready, not a sketch.

## Scope (A only)

**In:** H7 clock tree for a 48 kHz SAI kernel clock; SAI master-TX I²S init via
embassy's ring-buffered DMA; a firmware `SaiOut` module exposing an async
`play(&[i32])`; a test-tone `main` streaming a 440 Hz-L / 659 Hz-R tone; DMA-SRAM
placement + H7 cache rules; the thumbv7em build gate.

**Out (later phases):** SD card (B); real file decode + deck/`Sink`-trait
integration + the sync-`Sink`-vs-async bridge + `AnyDecoder` stack/flash budget
(C); 44.1 kHz / rate-switching; volume; gapless.

## Approach (decided)

Use **embassy-stm32's `RingBufferedSai`** (built-in double-buffered DMA ring) +
an **async streaming loop**, not a hand-rolled circular-DMA ISR (more `unsafe`,
easy to mis-clock) and not the core sync `Sink` trait yet (its `write` is
synchronous; bridging to async SAI DMA is a phase-C concern). This proves the
hard part — clock tree → SAI → MCLK/BCLK/LRCK → DAC — with the least custom code
and no `unsafe`.

## Components

- **`clock` config** (in `main`'s `embassy_stm32::Config`): a PLL producing a SAI
  kernel clock such that **MCLK = 256 · 48 000 = 12.288 MHz** (PLL2 or PLL3
  "P"/"Q" tuned to 12.288 MHz; SAI1 kernel mux → that PLL). The core/bus clocks
  stay at a sane H743 default (e.g. 400–480 MHz SYSCLK). Exact PLL divisors are
  computed and documented; their *correctness on silicon* is the owner's bench
  test. 44.1 kHz would need a 11.2896 MHz variant — noted, not implemented.
- **`sai_out.rs`** (firmware-only module): owns the `Sai` peripheral configured
  **master transmit, I²S Philips, 24-bit data in 32-bit slots, stereo, MCLK
  on**, wrapped as a `RingBufferedSai` over a DMA ring buffer (2 × 1024 stereo
  frames) placed in a **DMA-capable, non-cached (or cache-maintained) SRAM**
  region per H7 rules. Exposes:
  - `SaiOut::new(sai_peripheral, dma, pins…, &mut ring_buf) -> Self`
  - `async fn play(&mut self, frames: &[i32])` — converts the pipeline's
    canonical `i32` samples to the SAI 24-bit slot word and awaits the ring
    write (back-pressure = natural pacing to the audio clock).
- **`main`**: configure clocks → build `SaiOut` → loop generating a 48 kHz
  stereo tone (440 Hz L, 659.25 Hz E5 R — the same signal as the test fixtures,
  so a listener can recognize it) in ~1024-frame chunks and `play().await` them
  forever. Replaces the current idle heartbeat + the stale "decoders are stubs"
  SliceSource block.

## Data flow

`tone generator (i32 stereo @48k)` → `SaiOut::play` (i32 → 24-bit I²S slot) →
`RingBufferedSai` DMA ring → SAI FIFO → BCLK/LRCK/DATA → PCM5102 → line out.

## Error handling

- SAI/DMA init failure is a firmware bring-up bug → `defmt` error + panic (there
  is nothing to recover to on a bare-metal audio device); `panic-probe` prints it
  over RTT.
- `RingBufferedSai` write errors (overrun/underrun) are logged via `defmt` and
  the loop re-primes the ring (an underrun on a too-slow producer is the expected
  teething signal on-bench).
- No `unsafe` in the firmware crate beyond what embassy's own macros require;
  keep `#![no_std]`/`#![no_main]` and the crate's "build-gated, not clippy-gated"
  posture.

## Testing

- **Automated (here):** `cargo build -p soul-audio-firmware --target
  thumbv7em-none-eabihf` and the `cargo xtask verify` firmware-link step must be
  green. This gates API correctness, no_std-ness, and flash-region linking (watch
  the known nested-worktree doubled-`.cargo` rustflags FLASH pitfall — build from
  the main checkout).
- **Deferred (owner, on hardware):** flash via `probe-rs`; expect a continuous
  440 Hz-L / 659 Hz-R tone out of the PCM5102. The module docs give the exact
  breakout wiring + jumper settings and the `probe-rs run` command.

## Acceptance (A)

thumbv7em cross-build + firmware link green; `main` streams a tone through a
real-API `RingBufferedSai`; `sai_out.rs` + clock config documented with the
PCM5102 wiring and the on-hardware test procedure. (Audible output is the owner's
deferred bench check — explicitly not gated here.)
