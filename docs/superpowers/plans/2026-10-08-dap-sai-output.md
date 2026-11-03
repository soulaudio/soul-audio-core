# DAP SAI/I²S Output (sub-project A of #32) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:executing-plans. Steps use checkbox (`- [ ]`). **This is firmware with NO board available — the only automated gate is the thumbv7em cross-build; every "audible" check is a DEFERRED on-hardware step the owner runs with a Nucleo-H743ZI + PCM5102.** Do not claim audio works from a green build.

**Goal:** STM32H743 SAI/I²S master-TX output via embassy's DMA ring, proven (on-hardware, later) by a 48 kHz 440/659 Hz test tone.

**Architecture:** `main` configures the H7 clock tree for a 12.288 MHz SAI kernel clock (256·48 kHz), builds a `Sai` in asynchronous-with-MCLK mode over a DMA ring buffer, and an async loop streams a tone-LUT into it. A firmware-only `sai_out` module wraps the `Sai` as `async fn play(&[i32])` — the seam phase C's deck integration will drive.

**Tech Stack:** Rust `no_std`/`no_main`, embassy-stm32 0.6 (SAI + RCC + DMA), embassy-executor, defmt-rtt, PCM5102 I²S DAC.

## Global Constraints

- `#![no_std]` + `#![no_main]`; crate is **build-gated, not clippy-gated** (no `[lints] workspace`).
- **No `unsafe`** beyond what embassy macros emit (`bind_interrupts!`). **No `libm`/`powf`/`sinf`** — the tone uses a precomputed integer sine LUT (same discipline as `soul-audio-core`).
- The sole CI gate: `cargo build -p soul-audio-firmware --target thumbv7em-none-eabihf` + the `cargo xtask verify` firmware-link step, both green. Build from the **main checkout** (the known nested-worktree doubled-`.cargo` rustflags FLASH-region pitfall — see [[nested-worktree-firmware-link-fail]]).
- embassy-stm32 0.6 SAI facts (verified against the vendored crate source):
  - `let (sai_a, _sai_b) = Sai::split_subblocks(p.SAI1);` then
    `Sai::new_asynchronous_with_mclk(sai_a, sck, sd, fs, mclk, dma, &mut DMA_BUF, Irqs, config)`.
  - Word type `W = u32` for 24-bit data in 32-bit slots; `sai.write(&[u32]).await`.
  - `Config` is raw register fields (NO I²S preset) — see Task 3 for the exact Philips-I²S-24-bit field set.

## HARDWARE UNKNOWNS to resolve on the bench (flagged, not guessable here)

1. **SAI1 pin mapping** on the Nucleo-H743ZI (which CN header pins expose SAI1 SCK_A/SD_A/FS_A/MCLK_A) + the matching `AlternateFunction` — pick from embassy's `SckPin<SAI1,A>` etc. impls; confirm against the Nucleo pinout for physical wiring to the PCM5102.
2. **PLL divisors** that land SAI1 kernel clock exactly on 12.288 MHz, and the `master_clock_divider` that yields MCLK = 256·fs — compute, then **scope-verify the MCLK/BCLK/LRCK frequencies on the bench**.
3. **PCM5102 strap wiring** (SCK/MCLK, FMT=I²S, XSMT un-mute, FLT/DEMP/filter pins) — document in `sai_out.rs`.
4. **DMA-SRAM placement + H7 cache**: the ring buffer must live in a DMA-reachable SRAM; if the D-cache is enabled, use a non-cacheable MPU region or cache-maintenance. Default H743 boot has D-cache off — keep it off for A, note it.

---

## Task 1: Dependencies + DMA interrupt binding

**Files:** Modify `crates/soul-audio-firmware/Cargo.toml`.

- [ ] **Step 1: Confirm SAI + DMA are available (no new crate, chip feature already on)**

embassy-stm32's `sai` module compiles for `stm32h743zi` already. SAI1 uses a DMA channel; the binding uses `embassy_stm32::bind_interrupts!`. No new dependency is needed beyond what's present (`embassy-stm32` 0.6 with `stm32h743zi`, `embassy-executor`, `embassy-time`). Verify by building after Task 3.

- [ ] **Step 2: (If the linker needs it) add a DMA-capable SRAM section** — only if Task 3's static `DMA_BUF` placement needs an explicit `#[link_section]`; otherwise AXI SRAM (default `.bss`) is DMA-reachable on the H743 with D-cache off. Decide in Task 3.

---

## Task 2: Clock tree for a 12.288 MHz SAI kernel clock

**Files:** Modify `crates/soul-audio-firmware/src/main.rs` (the `embassy_stm32::init` config).

**Interfaces:** Produces a configured `embassy_stm32::Config` whose SAI1 kernel clock is 12.288 MHz.

- [ ] **Step 1: Build the RCC config**

Read `embassy_stm32::rcc` (vendored source) for the exact H7 field names, then set a PLL (PLL2 or PLL3) whose P (or Q) output is **12.288 MHz** and route SAI1's kernel mux to it. Skeleton (field names to confirm against the installed `embassy-stm32` 0.6 `rcc` module):

```rust
use embassy_stm32::rcc::*;
let mut config = embassy_stm32::Config::default();
// Core/bus clocks: a conventional H743 setup (HSE or HSI → PLL1 → ~400 MHz SYSCLK).
// SAI kernel clock: a dedicated PLL leg at 12.288 MHz (= 256 × 48 kHz, MCLK).
//   e.g. PLL3: DIVM=?, mul N=?, DIVP=? → 12.288 MHz  (compute from the PLL input).
// config.rcc.pll3 = Some(Pll { prediv: .., mul: .., divp: Some(..), divq: None, divr: None });
// config.rcc.mux.sai1sel = Saisel::PLL3_P;   // confirm enum/field name in rcc::mux
```

> **Bench-verify:** scope MCLK ≈ 12.288 MHz, LRCK ≈ 48 kHz, BCLK ≈ 48 kHz·64 (24-bit×2 slots×32). Adjust PLL divisors + `master_clock_divider` until exact.

- [ ] **Step 2: Cross-build** `cargo build -p soul-audio-firmware --target thumbv7em-none-eabihf` — must compile (gates the rcc field names/types).

---

## Task 3: `sai_out` module — SAI master-TX I²S over a DMA ring

**Files:** Create `crates/soul-audio-firmware/src/sai_out.rs`; add `mod sai_out;` to `main.rs`.

**Interfaces:** Produces `SaiOut<'d>` with `fn new(sai1, sck, sd, fs, mclk, dma, &'d mut [u32], Irqs) -> Self` and `async fn play(&mut self, frames: &[i32])`.

- [ ] **Step 1: Write the module (exact embassy API + Philips-I²S-24-bit Config)**

```rust
//! SAI1 master-transmit I²S output for a PCM5102 DAC (firmware-only).
//!
//! PCM5102 wiring (bench): SCK←MCLK (or SCK=GND + nodiv), BCK←SAI BCLK, LRCK←FS,
//! DIN←SD; FMT=GND (I²S), XSMT=3V3 (un-mute). 24-bit data in 32-bit slots.
use embassy_stm32::sai::{
    BitOrder, ClockStrobe, Config, DataSize, FrameSyncDefinition, FrameSyncOffset,
    FrameSyncPolarity, MasterClockDivider, Mode, Protocol, Sai, SlotSize, StereoMono, TxRx,
};

/// Philips I²S, 24-bit data in 32-bit slots, stereo, MCLK = 256·fs.
pub fn i2s_config() -> Config {
    let mut c = Config::default(); // Master, Transmitter
    c.mode = Mode::Master;
    c.tx_rx = TxRx::Transmitter;
    c.protocol = Protocol::Free;
    c.slot_size = SlotSize::Channel32;      // 32-bit slot  (confirm variant name)
    c.data_size = DataSize::Data24;
    c.stereo_mono = StereoMono::Stereo;
    c.bit_order = BitOrder::MsbFirst;       // I²S is MSB-first
    c.frame_sync_offset = FrameSyncOffset::BeforeFirstBit; // Philips: FS one BCLK early
    c.frame_sync_polarity = FrameSyncPolarity::ActiveLow;  // LRCK low = left
    c.frame_sync_definition = FrameSyncDefinition::ChannelIdentification;
    c.frame_length = 64;                    // 2 × 32-bit slots
    c.frame_sync_active_level_length = embassy_stm32::sai::word::U7(32);
    c.clock_strobe = ClockStrobe::Falling;  // I²S data clocked on falling edge
    c.master_clock_divider = MasterClockDivider::Div2; // tune for 256·fs MCLK
    c
}
```

(Confirm each variant name against the vendored `sai/mod.rs` enums — `SlotSize`, `MasterClockDivider` values especially — during the cross-build; fix compile errors.)

- [ ] **Step 2: `SaiOut` wrapper**

```rust
pub struct SaiOut<'d> {
    sai: Sai<'d, embassy_stm32::peripherals::SAI1, u32>,
}
impl<'d> SaiOut<'d> {
    // signature mirrors Sai::new_asynchronous_with_mclk; take the split SubBlock,
    // pins, dma, a &'d mut [u32] ring buffer, and the Irqs binding.
    // ... construct via Sai::split_subblocks(sai1) + new_asynchronous_with_mclk(...)
    /// Stream one chunk. Canonical pipeline `i32` (24-bit in the low bits) → the
    /// SAI 32-bit slot word (left-justified 24-bit). Back-pressure paces to the
    /// audio clock.
    pub async fn play(&mut self, frames: &[i32]) -> Result<(), embassy_stm32::sai::Error> {
        // convert into a scratch [u32; N], then self.sai.write(&buf).await
    }
}
```

- [ ] **Step 3: Cross-build** — iterate on compile errors until the embassy API + Config variants are exactly right.

---

## Task 4: Test-tone `main`

**Files:** Modify `crates/soul-audio-firmware/src/main.rs`.

- [ ] **Step 1: Replace the stub body**

Remove the stale "decoders are stubs" `SliceSource`/`open` block and the idle heartbeat. Add: a `const SINE_LUT: [i32; N]` (precomputed, no libm), a static DMA ring buffer `static mut DMA_BUF: [u32; 2*1024]` (or a `StaticCell`), construct `SaiOut`, and an async loop that fills ~1024-frame chunks with 440 Hz-L / 659.25 Hz-R from the LUT and `sai_out.play(&chunk).await`.

- [ ] **Step 2: Cross-build + the firmware-link gate**

Run `cargo build -p soul-audio-firmware --target thumbv7em-none-eabihf` and `cargo xtask verify` (firmware-link step) — both green. This is the full automated acceptance for A.

- [ ] **Step 3: Commit**

```bash
git add crates/soul-audio-firmware/
git commit -m "feat(firmware): SAI/I2S master-TX output + test tone (#32 sub-project A)"
```

---

## Task 5: On-hardware test procedure (documentation — DEFERRED, owner runs on a board)

**Files:** Append to `crates/soul-audio-firmware/src/sai_out.rs` module docs (or a short `README`).

- [ ] Document: the PCM5102 breakout wiring + jumpers; the exact SAI1 pins used; `probe-rs run --chip STM32H743ZITx`; and the expected result (continuous 440 Hz-left / 659.25 Hz-right tone). Note the three bench-verify items (MCLK/LRCK/BCLK frequencies; I²S bit-order/FS-polarity if the tone is distorted/swapped; DMA-SRAM/cache if the DMA faults).

---

## Self-Review

- **Spec coverage:** clock (T2), SAI master-TX I²S DMA ring + `SaiOut` seam (T3), test tone (T4), DMA-SRAM/cache note (T1 S2 + T3), thumbv7em gate (T4 S2), on-hardware procedure (T5). All present.
- **Placeholders:** the `?` PLL divisors (T2) and a few `confirm the variant name` notes are **deliberate bench/compile-resolved unknowns**, not vague content — each has a concrete resolution step (read rcc source / fix compile error / scope the clock). They cannot be pinned without the datasheet+board and are flagged as such, per the no-hardware constraint.
- **Scope:** sub-project A only; SD (B) and deck/`Sink` integration + budget (C) are explicitly out.
- **Types:** `W = u32` consistent across `Config` (Data24/Slot32), `DMA_BUF: [u32]`, and `play`'s i32→u32 conversion.
