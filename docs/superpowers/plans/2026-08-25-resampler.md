# Resampler Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `no_std`/no-alloc, f32 windowed-sinc **polyphase resampler** to `soul-audio-core` with a fixed-output-block API and a continuously-variable conversion ratio, usable identically on the STM32H743 and desktop.

**Architecture:** A `Resampler` streams interleaved `i32` PCM → interleaved `i32` PCM at a runtime-adjustable ratio. Per output frame it convolves a length-`TAPS` Kaiser-windowed-sinc kernel — selected from a precomputed `const` table of `SUBPHASES` oversampled phases and linearly/cubically interpolated between adjacent phases — over a per-channel `f32` history ring. Output count is fixed at construction (rubato `SincFixedOut` model); the caller queries `input_frames_next()` for how much decoded input to supply.

**Tech Stack:** Rust 2024, `#![cfg_attr(not(test), no_std)]`, no-alloc. Coefficient tables generated offline by an `xtask` and checked in. Host-only `rustfft` dev-dependency for the DSP quality tests.

## Global Constraints

- **`no_std` + strict no-alloc** in library code — every API writes into caller buffers; no `Vec`/`Box`/heap; no `alloc`. (Compile gate: `cargo build -p soul-audio-core --target thumbv7em-none-eabihf --no-default-features`.)
- **`#![forbid(unsafe_code)]`** — crate-wide; no `unsafe`.
- **No runtime transcendental math** (`sin`/`cos`/Bessel) — coefficient tables are `const`, generated offline (no `libm` dependency). Same pattern as `mp3_l12_tables.rs`.
- **f32 internal** — i32 PCM in → f32 → i32 out. No `f64` sample math (H743 FPU is single-precision).
- **Sample container:** interleaved `i32` in 16/24/32-bit range, matching `Decoder::decode_into`.
- **Tiers:** exactly two — `Fast` (TAPS=32, SUBPHASES=128) and `Hq` (TAPS=64, SUBPHASES=256).
- **Max channels:** 8 (`MAX_CHANNELS`).
- **Commit style:** plain messages, **no AI-authorship trailers** (lefthook rejects `Co-Authored-By: Claude` / `Claude-Session`).
- **Gauntlet before any "done":** `cargo test -p soul-audio-core`, `cargo clippy -p soul-audio-core --all-targets -- -D warnings`, `cargo fmt -p soul-audio-core -- --check`, the thumbv7em no_std build, and the firmware link (nested worktree needs `CARGO_ENCODED_RUSTFLAGS=-Clink-arg=--nmagic\x1f-Clink-arg=-Tlink.x\x1f-Clink-arg=-Tdefmt.x`).

## File Structure

- Create: `crates/soul-audio-core/src/resample.rs` — the `Resampler`, `Quality`, `ResampleError`, phase/slew/framing logic, kernel. Owns the module's `#[cfg(test)] mod tests`.
- Create: `crates/soul-audio-core/src/resample_tables.rs` — generated `const` coefficient tables (`FAST_COEFFS`, `HQ_COEFFS`) + design constants. `include!`d by `resample.rs`. Do-not-edit-by-hand.
- Create: `xtask/src/resample_tables.rs` (+ wire a `gen-resample-tables` subcommand in `xtask/src/main.rs`) — the offline generator.
- Modify: `crates/soul-audio-core/src/lib.rs` — `pub mod resample;` + `pub use resample::{Resampler, Quality, ResampleError};`.
- Modify: `crates/soul-audio-core/Cargo.toml` — add `[dev-dependencies] rustfft` (host-only; tests).
- Create: `crates/soul-audio-core/tests/resample.rs` — end-to-end + DSP-quality integration tests.

---

### Task 1: Framing skeleton with a linear kernel

Establishes the hard part — fixed-output framing, `input_frames_next`, the fractional phase cursor, interleaved-i32↔f32, and the per-channel history ring — using a trivial **linear** kernel so the counts/DC logic is testable before any sinc table exists. The kernel lives behind a private seam that Task 3 replaces.

**Files:**
- Create: `crates/soul-audio-core/src/resample.rs`
- Modify: `crates/soul-audio-core/src/lib.rs` (add `pub mod resample;` and the re-exports)

**Interfaces:**
- Produces:
  - `pub enum Quality { Fast, Hq }`
  - `pub enum ResampleError { TooManyChannels, BadRate, RatioOutOfRange }`
  - `pub struct Resampler`
  - `Resampler::new(channels: usize, in_rate: u32, out_rate: u32, max_rel_ratio: f64, output_frames: usize, quality: Quality) -> Result<Resampler, ResampleError>`
  - `Resampler::output_frames(&self) -> usize`
  - `Resampler::input_frames_next(&self) -> usize`
  - `Resampler::process(&mut self, input: &[i32], output: &mut [i32]) -> Result<(), ResampleError>`
- Consumes: nothing (first task).

Internal design (implement exactly this so later tasks match):
- Constants: `const MAX_CHANNELS: usize = 8; const MAX_TAPS: usize = 64;`
- Fields: `channels, quality, taps: usize, subphases: usize, output_frames, nominal_step: f64 (=in/out), step: f64, max_rel: f64, cutoff: f32, frac: f64, hist: [[f32; MAX_TAPS]; MAX_CHANNELS], hist_len: usize`.
- History semantics: `hist[ch]` holds, oldest-first, the last `taps` input frames already consumed; `hist_len` counts how many are valid (ramps up from 0 at stream start — missing history reads as 0.0).
- Virtual input index `at(ch, idx)`: for `idx < 0` read `hist[ch]` (the `-idx`-th most-recent consumed frame, or 0.0 if beyond `hist_len`); for `idx >= 0` read `input[idx*channels + ch] as f32`.
- `input_frames_next()` = the number of NEW input frames needed so the last (`output_frames-1`-th) output's rightmost tap is in range: with `s = max(step, nominal_step*... )` conservative step, `ceil(frac + (output_frames-1)*step) + taps/2 + 1`, minus samples already carried (none carried across calls in this model — history is separate), i.e. `(frac + (output_frames-1)*step).floor() as usize + taps/2 + 1`.
- `process`: for `o in 0..output_frames`, let `pos = frac + o*step`, `i0 = pos.floor() as isize`, `η = pos - i0 as f64`; for each channel write `output[o*channels+ch] = f32_to_i32(kernel(ch, i0, η, &at))`; after the loop, consume `consumed = (frac + output_frames*step).floor() as usize` input frames — copy the last `taps` of the first `consumed` input frames into `hist`, set `frac = (frac + output_frames*step) - consumed as f64`.
- Kernel seam (Task 1 body = **linear**): `fn kernel(&self, ch, i0, η, at) -> f32 { at(ch,i0)*(1.0-η as f32) + at(ch,i0+1)*(η as f32) }`.
- `f32_to_i32(x)`: `x.round().clamp(-2147483648.0, 2147483647.0) as i32` (16/24/32-bit-safe; inputs stay in i32 range).

- [ ] **Step 1: Write failing test — construction + count math**

Add to `crates/soul-audio-core/src/resample.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unity_ratio_passes_counts() {
        // 48k -> 48k, mono, 100-frame output blocks, ±10% range.
        let r = Resampler::new(1, 48_000, 48_000, 1.1, 100, Quality::Fast).unwrap();
        assert_eq!(r.output_frames(), 100);
        // At unity, producing 100 outputs needs ~100 inputs + right-tap headroom.
        assert_eq!(r.input_frames_next(), 100 + Quality::Fast.taps() / 2 + 1);
    }
}
```

- [ ] **Step 2: Run it — expect fail (no `Resampler`)**

Run: `cargo test -p soul-audio-core --lib resample::tests::unity_ratio_passes_counts`
Expected: FAIL to compile (`cannot find … Resampler`).

- [ ] **Step 3: Implement the module**

Write `crates/soul-audio-core/src/resample.rs` with the design above. Include a `Quality::taps()`/`subphases()` helper:

```rust
//! Windowed-sinc polyphase sample-rate converter (see the resampler design spec).
//! `no_std`, no-alloc, f32 internal, fixed-output-block API, runtime-variable ratio.

const MAX_CHANNELS: usize = 8;
const MAX_TAPS: usize = 64;

/// Quality tier — trades table size / CPU for stopband depth.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Quality { Fast, Hq }

impl Quality {
    pub(crate) fn taps(self) -> usize { match self { Quality::Fast => 32, Quality::Hq => 64 } }
    pub(crate) fn subphases(self) -> usize { match self { Quality::Fast => 128, Quality::Hq => 256 } }
}

/// Resampler construction / use errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResampleError { TooManyChannels, BadRate, RatioOutOfRange }

pub struct Resampler {
    channels: usize,
    quality: Quality,
    taps: usize,
    subphases: usize,
    output_frames: usize,
    nominal_step: f64,
    step: f64,
    max_rel: f64,
    cutoff: f32,
    frac: f64,
    hist: [[f32; MAX_TAPS]; MAX_CHANNELS],
    hist_len: usize,
}

impl Resampler {
    pub fn new(channels: usize, in_rate: u32, out_rate: u32, max_rel_ratio: f64,
               output_frames: usize, quality: Quality) -> Result<Self, ResampleError> {
        if channels == 0 || channels > MAX_CHANNELS { return Err(ResampleError::TooManyChannels); }
        if in_rate == 0 || out_rate == 0 { return Err(ResampleError::BadRate); }
        let nominal_step = f64::from(in_rate) / f64::from(out_rate);
        // Cutoff designed for the worst-case downsample the range allows.
        let min_out_over_in = (f64::from(out_rate) / f64::from(in_rate)) / max_rel_ratio.max(1.0);
        let cutoff = min_out_over_in.min(1.0) as f32;
        Ok(Self {
            channels, quality, taps: quality.taps(), subphases: quality.subphases(),
            output_frames, nominal_step, step: nominal_step, max_rel: max_rel_ratio.max(1.0),
            cutoff, frac: 0.0, hist: [[0.0; MAX_TAPS]; MAX_CHANNELS], hist_len: 0,
        })
    }

    pub fn output_frames(&self) -> usize { self.output_frames }

    pub fn input_frames_next(&self) -> usize {
        let last = self.frac + (self.output_frames as f64 - 1.0) * self.step;
        last.floor() as usize + self.taps / 2 + 1
    }

    fn at(&self, ch: usize, input: &[i32], idx: isize) -> f32 {
        if idx >= 0 {
            input[idx as usize * self.channels + ch] as f32
        } else {
            let back = (-idx) as usize; // 1 = most recent consumed
            if back <= self.hist_len {
                self.hist[ch][self.taps - back] as f32 // hist is oldest-first, last = newest
            } else { 0.0 }
        }
    }

    fn kernel(&self, ch: usize, input: &[i32], i0: isize, eta: f64) -> f32 {
        // Linear (Task 1 scaffold; replaced by the sinc kernel in Task 3).
        let a = self.at(ch, input, i0);
        let b = self.at(ch, input, i0 + 1);
        a * (1.0 - eta as f32) + b * (eta as f32)
    }

    pub fn process(&mut self, input: &[i32], output: &mut [i32]) -> Result<(), ResampleError> {
        for o in 0..self.output_frames {
            let pos = self.frac + o as f64 * self.step;
            let i0 = pos.floor() as isize;
            let eta = pos - pos.floor();
            for ch in 0..self.channels {
                let y = self.kernel(ch, input, i0, eta);
                output[o * self.channels + ch] = y.round().clamp(-2147483648.0, 2147483647.0) as i32;
            }
        }
        // Advance and roll history.
        let end = self.frac + self.output_frames as f64 * self.step;
        let consumed = end.floor() as usize;
        self.roll_history(input, consumed);
        self.frac = end - consumed as f64;
        Ok(())
    }

    fn roll_history(&mut self, input: &[i32], consumed: usize) {
        let take = consumed.min(input.len() / self.channels);
        for ch in 0..self.channels {
            // Keep the last `taps` consumed frames, oldest-first.
            for k in 0..self.taps {
                // Global index of the frame at hist slot k after the roll:
                // newest is `take-1`, so slot (taps-1) = take-1, slot (taps-2)=take-2, ...
                let gi = take as isize - (self.taps - k) as isize;
                self.hist[ch][k] = if gi >= 0 { input[gi as usize * self.channels + ch] as f32 }
                                   else { 0.0 };
            }
        }
        self.hist_len = (self.hist_len + take).min(self.taps);
    }
}
```

Then in `crates/soul-audio-core/src/lib.rs` add near the other `pub mod`s:

```rust
pub mod resample;
```
and with the other `pub use`s:
```rust
pub use resample::{Quality, ResampleError, Resampler};
```

- [ ] **Step 4: Run the test — expect pass**

Run: `cargo test -p soul-audio-core --lib resample::tests::unity_ratio_passes_counts`
Expected: PASS.

- [ ] **Step 5: Add behavior tests (counts across ratios, DC, linear value)**

Append to the `tests` module:

```rust
    fn decode_all(r: &mut Resampler, ch: usize, mut supply: impl FnMut(usize) -> Vec<i32>) -> Vec<i32> {
        let mut out = Vec::new();
        let mut block = vec![0i32; r.output_frames() * ch];
        for _ in 0..20 {
            let need = r.input_frames_next();
            let input = supply(need);
            r.process(&input, &mut block).unwrap();
            out.extend_from_slice(&block);
        }
        out
    }

    #[test]
    fn dc_is_preserved() {
        // Constant 1000 in → constant ~1000 out (once history primes), any ratio.
        let mut r = Resampler::new(1, 44_100, 48_000, 1.1, 64, Quality::Fast).unwrap();
        let out = decode_all(&mut r, 1, |n| vec![1000; n]);
        // After priming, the tail should be flat at 1000 (±1 rounding).
        for &s in &out[out.len() - 100..] {
            assert!((s - 1000).abs() <= 1, "DC not preserved: {s}");
        }
    }

    #[test]
    fn two_x_downsample_consumes_two_per_output() {
        // 96k -> 48k, step = 2.0: ~2 input frames consumed per output frame.
        let r = Resampler::new(1, 96_000, 48_000, 1.1, 100, Quality::Fast).unwrap();
        // 100 outputs at step 2 need ~200 inputs + right taps.
        assert_eq!(r.input_frames_next(), 198 + Quality::Fast.taps() / 2 + 1);
    }
```

- [ ] **Step 6: Run all resample tests — expect pass**

Run: `cargo test -p soul-audio-core --lib resample`
Expected: PASS (3 tests). If `dc_is_preserved` fails, the history-roll indexing (`roll_history`) or `at()` back-index is off — fix against this oracle before proceeding.

- [ ] **Step 7: Commit**

```bash
git add crates/soul-audio-core/src/resample.rs crates/soul-audio-core/src/lib.rs
git commit -m "feat(resample): fixed-output framing + phase cursor (linear kernel)"
```

---

### Task 2: Offline coefficient-table generator

Produces the checked-in `const` tables the sinc kernel needs, plus a CI-verifiable generator. Kaiser-windowed sinc, oversampled to `SUBPHASES` phases, stored as `[SUBPHASES][TAPS]` f32, each phase normalized to unity DC gain.

**Files:**
- Create: `xtask/src/resample_tables.rs`
- Modify: `xtask/src/main.rs` (add a `gen-resample-tables` subcommand)
- Create (generated): `crates/soul-audio-core/src/resample_tables.rs`

**Interfaces:**
- Produces (in `resample_tables.rs`, `include!`d into `resample`):
  - `pub(crate) static FAST_COEFFS: [[f32; 32]; 128];`
  - `pub(crate) static HQ_COEFFS: [[f32; 64]; 256];`
  - `pub(crate) const FAST_CUTOFF: f32; pub(crate) const HQ_CUTOFF: f32;`

Generator math (implement exactly): for tier `(taps T, subphases L, cutoff fc, kaiser β)`, the oversampled prototype has `N = T*L` samples; `center = N/2`; for `j in 0..N`: `t = (j - center) / L` (input-sample units), `s = if t==0 {1.0} else {sin(pi*fc*t)/(pi*fc*t)}`, `w = kaiser(j, N, β)`, `g[j] = fc * s * w`. Polyphase split: `COEFFS[p][k] = g[k*L + p]`. Then normalize each phase `p` so `sum_k COEFFS[p][k] == 1.0`. Params: Fast `fc=0.90, β=0.1102*(90-8.7)=8.95` (≈90 dB); Hq `fc=0.95, β=0.1102*(120-8.7)=12.27` (≈120 dB). Kaiser needs `I0` (modified Bessel, series sum) — implement in the generator (std math, host-only).

- [ ] **Step 1: Write the generator**

Create `xtask/src/resample_tables.rs`:

```rust
//! Generates crates/soul-audio-core/src/resample_tables.rs (Kaiser-windowed-sinc
//! polyphase coefficient tables). Run: `cargo xtask gen-resample-tables`.
use std::f64::consts::PI;
use std::fmt::Write as _;

fn i0(x: f64) -> f64 { // modified Bessel I0, series
    let mut sum = 1.0; let mut term = 1.0; let half = x / 2.0;
    for k in 1..50 { term *= (half / k as f64).powi(2); sum += term; if term < 1e-12 * sum { break; } }
    sum
}
fn kaiser(j: usize, n: usize, beta: f64) -> f64 {
    let m = (n - 1) as f64; let r = 2.0 * j as f64 / m - 1.0;
    i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0(beta)
}
fn table(taps: usize, sub: usize, fc: f64, beta: f64) -> Vec<Vec<f32>> {
    let n = taps * sub; let center = n / 2;
    let g: Vec<f64> = (0..n).map(|j| {
        let t = (j as f64 - center as f64) / sub as f64;
        let s = if t == 0.0 { 1.0 } else { (PI * fc * t).sin() / (PI * fc * t) };
        fc * s * kaiser(j, n, beta)
    }).collect();
    let mut phases = vec![vec![0f32; taps]; sub];
    for p in 0..sub { for k in 0..taps {
        phases[p][k] = g[k * sub + p] as f32;
    }}
    for p in 0..sub {
        let s: f32 = phases[p].iter().sum();
        if s.abs() > 1e-9 { for c in &mut phases[p] { *c /= s; } }
    }
    phases
}
fn emit(name: &str, taps: usize, sub: usize, t: &[Vec<f32>], out: &mut String) {
    writeln!(out, "#[rustfmt::skip]").unwrap();
    writeln!(out, "pub(crate) static {name}: [[f32; {taps}]; {sub}] = [").unwrap();
    for row in t {
        write!(out, "    [").unwrap();
        for (i, c) in row.iter().enumerate() {
            if i > 0 { write!(out, ",").unwrap(); }
            write!(out, "{c:.9}").unwrap();
        }
        writeln!(out, "],").unwrap();
    }
    writeln!(out, "];").unwrap();
}
pub fn run() -> std::io::Result<()> {
    let mut out = String::from("// Generated by `cargo xtask gen-resample-tables`. Do not edit.\n");
    let fast_fc = 0.90; let hq_fc = 0.95;
    writeln!(out, "pub(crate) const FAST_CUTOFF: f32 = {fast_fc};").unwrap();
    writeln!(out, "pub(crate) const HQ_CUTOFF: f32 = {hq_fc};").unwrap();
    emit("FAST_COEFFS", 32, 128, &table(32, 128, fast_fc, 8.95), &mut out);
    emit("HQ_COEFFS", 64, 256, &table(64, 256, hq_fc, 12.27), &mut out);
    std::fs::write("crates/soul-audio-core/src/resample_tables.rs", out)
}
```

Wire it in `xtask/src/main.rs` — add `mod resample_tables;` and a match arm so `cargo xtask gen-resample-tables` calls `resample_tables::run()`.

- [ ] **Step 2: Generate the tables**

Run: `cargo xtask gen-resample-tables`
Expected: writes `crates/soul-audio-core/src/resample_tables.rs` (≈128+256 rows). Verify with `head -3 crates/soul-audio-core/src/resample_tables.rs`.

- [ ] **Step 3: Write failing test — table sanity**

Add to `resample.rs` (above the existing tests), first wire the include at the top of the file (after the doc comment):

```rust
include!("resample_tables.rs");
```

Then in `mod tests`:

```rust
    #[test]
    fn coeff_tables_are_normalized_and_symmetric() {
        // Each phase sums to ~1 (unity DC gain).
        for row in &FAST_COEFFS { let s: f32 = row.iter().sum(); assert!((s - 1.0).abs() < 1e-4); }
        for row in &HQ_COEFFS { let s: f32 = row.iter().sum(); assert!((s - 1.0).abs() < 1e-4); }
        // Center phase (0) is symmetric about its middle.
        let c = &HQ_COEFFS[0];
        for k in 0..c.len() / 2 { assert!((c[k] - c[c.len() - 1 - k]).abs() < 1e-6); }
    }
```

- [ ] **Step 4: Run — expect pass**

Run: `cargo test -p soul-audio-core --lib resample::tests::coeff_tables_are_normalized_and_symmetric`
Expected: PASS. If the symmetry assert fails, the phase-0 centering is off — check `center = n/2` and the `k*sub + p` split.

- [ ] **Step 5: Commit**

```bash
git add xtask/src/resample_tables.rs xtask/src/main.rs crates/soul-audio-core/src/resample_tables.rs crates/soul-audio-core/src/resample.rs
git commit -m "feat(resample): offline Kaiser-sinc polyphase table generator + tables"
```

---

### Task 3: Windowed-sinc kernel (Fast tier) + DSP quality tests

Replace the linear kernel with the polyphase sinc convolution using `FAST_COEFFS`, interpolating linearly between adjacent phases. Add the objective aliasing/SNR tests (host-only `rustfft`).

**Files:**
- Modify: `crates/soul-audio-core/src/resample.rs` (the `kernel` method + a phase-select helper)
- Modify: `crates/soul-audio-core/Cargo.toml` (`[dev-dependencies] rustfft = "6"`)

**Interfaces:**
- Consumes: `FAST_COEFFS` (Task 2), `at()` (Task 1).
- Produces: unchanged public API; `kernel` now sinc-based.

Kernel: for output at `i0 + eta`, phase float `pf = eta * subphases`, `p = pf.floor()`, `w = pf - p`; effective coef `c[k] = COEFFS[p][k] + w*(COEFFS[p+1 or wrap][k]-COEFFS[p][k])`; output `= Σ_{k=0}^{taps-1} c[k] * at(ch, i0 - taps/2 + 1 + k)`. Wrap `p+1==subphases` to phase 0 with `i0+1` shift (handled by clamping `w`/using the linear scaffold's alignment — keep `taps/2` centering consistent with Task 1's `at`).

- [ ] **Step 1: Write failing test — impulse response is symmetric (linear phase)**

Add to `mod tests`:

```rust
    #[test]
    fn impulse_response_is_symmetric() {
        // Unit impulse in, unity ratio: output is the (symmetric) filter around the peak.
        let mut r = Resampler::new(1, 48_000, 48_000, 1.1, 256, Quality::Fast).unwrap();
        let mut first = true;
        let out = {
            let mut acc = Vec::new();
            let mut blk = vec![0i32; 256];
            for _ in 0..4 {
                let need = r.input_frames_next();
                let mut inp = vec![0i32; need];
                if first { inp[0] = 1_000_000; first = false; }
                r.process(&inp, &mut blk).unwrap();
                acc.extend_from_slice(&blk);
            }
            acc
        };
        let peak = out.iter().enumerate().max_by_key(|(_, &v)| v).map(|(i, _)| i).unwrap();
        for d in 1..8 {
            assert!((out[peak - d] - out[peak + d]).abs() <= 2, "asymmetric at ±{d}");
        }
    }
```

- [ ] **Step 2: Run — expect fail (linear kernel isn't symmetric-sinc)**

Run: `cargo test -p soul-audio-core --lib resample::tests::impulse_response_is_symmetric`
Expected: FAIL (linear kernel gives a triangle, not a symmetric sinc with side-lobes — the ±d equality within 2 will hold for a triangle too, so if it PASSES, tighten by also asserting a nonzero side-lobe exists: `assert!(out[peak+4] != 0)`). Adjust the test so it genuinely distinguishes sinc from linear before implementing.

- [ ] **Step 3: Implement the sinc kernel**

Replace `kernel` in `resample.rs`:

```rust
    fn kernel(&self, ch: usize, input: &[i32], i0: isize, eta: f64) -> f32 {
        let (coeffs, sub) = match self.quality {
            Quality::Fast => (&FAST_COEFFS[..], 128usize),
            Quality::Hq => (&HQ_COEFFS[..], 256usize),
        };
        let pf = eta * sub as f64;
        let p = (pf.floor() as usize).min(sub - 1);
        let w = (pf - p as f64) as f32;
        let p1 = if p + 1 < sub { p + 1 } else { p };
        let base = i0 - (self.taps as isize) / 2 + 1;
        let mut acc = 0.0f32;
        for k in 0..self.taps {
            let c = coeffs[p][k] + w * (coeffs[p1][k] - coeffs[p][k]);
            acc += c * self.at(ch, input, base + k as isize);
        }
        acc
    }
```

(The `Quality::Hq` arm is wired now but exercised in Task 4.)

- [ ] **Step 4: Run — impulse + DC + counts still pass**

Run: `cargo test -p soul-audio-core --lib resample`
Expected: PASS. `dc_is_preserved` must still hold (phases sum to 1). If DC drifts, the `base` centering disagrees with the history roll — reconcile `base` with `roll_history`.

- [ ] **Step 5: Add the DSP aliasing/SNR test**

Add `rustfft = "6"` under `[dev-dependencies]` in `crates/soul-audio-core/Cargo.toml`. Create the metric test in `crates/soul-audio-core/tests/resample.rs`:

```rust
//! Objective DSP-quality tests (host-only, uses rustfft).
use rustfft::{num_complex::Complex, FftPlanner};
use soul_audio_core::{Quality, Resampler};

/// Resample a pure sine and return dB(signal) − dB(worst alias bin).
fn alias_rejection_db(quality: Quality, in_rate: u32, out_rate: u32, tone_hz: f64) -> f64 {
    let mut r = Resampler::new(1, in_rate, out_rate, 1.1, 1024, quality).unwrap();
    // Generate/resample ~64k output samples.
    let mut out: Vec<f64> = Vec::new();
    let mut phase = 0.0f64; let dph = 2.0 * std::f64::consts::PI * tone_hz / in_rate as f64;
    let mut blk = vec![0i32; 1024];
    while out.len() < 65_536 {
        let need = r.input_frames_next();
        let inp: Vec<i32> = (0..need).map(|_| {
            let s = (phase.sin() * 8_000_000.0) as i32; phase += dph; s
        }).collect();
        r.process(&inp, &mut blk).unwrap();
        out.extend(blk.iter().map(|&s| s as f64));
    }
    out.truncate(65_536);
    // Blackman-Harris window + FFT.
    let n = out.len();
    for (i, s) in out.iter_mut().enumerate() {
        let x = i as f64 / (n - 1) as f64;
        let w = 0.35875 - 0.48829 * (2.0*std::f64::consts::PI*x).cos()
              + 0.14128 * (4.0*std::f64::consts::PI*x).cos() - 0.01168*(6.0*std::f64::consts::PI*x).cos();
        *s *= w;
    }
    let mut buf: Vec<Complex<f64>> = out.iter().map(|&r| Complex { re: r, im: 0.0 }).collect();
    FftPlanner::new().plan_fft_forward(n).process(&mut buf);
    let mag: Vec<f64> = buf[..n/2].iter().map(|c| c.norm()).collect();
    let sig_bin = (tone_hz / out_rate as f64 * n as f64).round() as usize;
    let sig = mag[sig_bin.saturating_sub(2)..sig_bin+3].iter().cloned().fold(0.0, f64::max);
    // Worst bin away from the signal (±5 bins) = worst alias/image.
    let mut worst = 0.0f64;
    for (i, &m) in mag.iter().enumerate() {
        if (i as isize - sig_bin as isize).abs() > 5 && i > 2 { worst = worst.max(m); }
    }
    20.0 * (sig / worst).log10()
}

#[test]
fn fast_tier_alias_rejection() {
    // 44.1k -> 48k upconvert of a 6 kHz tone: Fast tier ≳ 80 dB image rejection.
    let db = alias_rejection_db(Quality::Fast, 44_100, 48_000, 6_000.0);
    assert!(db > 80.0, "Fast alias rejection only {db:.1} dB");
}
```

- [ ] **Step 6: Run the DSP test**

Run: `cargo test -p soul-audio-core --test resample fast_tier_alias_rejection`
Expected: PASS (`db > 80`). If it fails low, the phase-interp wrap (`p1`) or `base` alignment is wrong; if it barely passes, that's acceptable for Fast — HQ (Task 4) must clear a higher bar.

- [ ] **Step 7: Commit**

```bash
git add crates/soul-audio-core/src/resample.rs crates/soul-audio-core/Cargo.toml crates/soul-audio-core/tests/resample.rs
git commit -m "feat(resample): windowed-sinc polyphase kernel (Fast tier) + alias tests"
```

---

### Task 4: HQ tier verification

The `Hq` arm already exists in `kernel` (Task 3). Prove it clears the 24-bit-class bar and confirm tier selection.

**Files:**
- Modify: `crates/soul-audio-core/tests/resample.rs`

**Interfaces:** consumes `alias_rejection_db` (Task 3), `HQ_COEFFS`.

- [ ] **Step 1: Write failing test**

Add to `tests/resample.rs`:

```rust
#[test]
fn hq_tier_beats_fast() {
    let fast = alias_rejection_db(Quality::Fast, 96_000, 44_100, 10_000.0);
    let hq = alias_rejection_db(Quality::Hq, 96_000, 44_100, 10_000.0);
    assert!(hq > 110.0, "HQ downsample alias rejection only {hq:.1} dB");
    assert!(hq > fast, "HQ ({hq:.1}) should beat Fast ({fast:.1})");
}
```

- [ ] **Step 2: Run — expect pass (kernel already supports Hq)**

Run: `cargo test -p soul-audio-core --test resample hq_tier_beats_fast`
Expected: PASS. If HQ downsample rejection is low, the downsample cutoff (`cutoff` field) isn't being applied to the kernel — see Task 5 note. If it passes, the const-cutoff table already handles this bounded range.

- [ ] **Step 3: Commit**

```bash
git add crates/soul-audio-core/tests/resample.rs
git commit -m "test(resample): HQ tier clears 24-bit-class alias rejection"
```

---

### Task 5: Runtime-variable ratio + slew + clamp

Add `set_ratio(out_over_in, ramp)` that changes `step` for subsequent output, slewing linearly across one block when `ramp`, and clamping to the declared range.

**Files:**
- Modify: `crates/soul-audio-core/src/resample.rs`

**Interfaces:**
- Produces: `Resampler::set_ratio(&mut self, out_over_in: f64, ramp: bool)`.
- Modifies `process` to consume a per-output slewed step.

Design: store `step` (current) and `target_step` (goal). In `process`, if `ramp` was requested, interpolate `s_o = step + (target_step-step)*(o+1)/output_frames` per output; else `step=target_step` immediately. After the block, `step = target_step`. `input_frames_next` uses `max(step, target_step)` to stay safe. Clamp requested ratio: `nominal_ratio = 1/nominal_step`; allowed `[nominal_ratio/max_rel, nominal_ratio*max_rel]`.

- [ ] **Step 1: Write failing test — ratio change is honored and bounded**

```rust
    #[test]
    fn set_ratio_changes_consumption_and_clamps() {
        let mut r = Resampler::new(1, 48_000, 48_000, 1.2, 100, Quality::Fast).unwrap();
        // Ask to play 20% faster (out/in smaller ⇒ consume more input).
        r.set_ratio(1.0 / 1.2, false);
        assert_eq!(r.input_frames_next(), (99.0 * 1.2).floor() as usize + Quality::Fast.taps()/2 + 1);
        // Out-of-range is clamped, not applied.
        r.set_ratio(1.0 / 5.0, false); // 5x — beyond ±20%
        let clamped_step = 1.2_f64; // max_rel
        assert_eq!(r.input_frames_next(), (99.0 * clamped_step).floor() as usize + Quality::Fast.taps()/2 + 1);
    }
```

- [ ] **Step 2: Run — expect fail (no `set_ratio`)**

Run: `cargo test -p soul-audio-core --lib resample::tests::set_ratio_changes_consumption_and_clamps`
Expected: FAIL to compile.

- [ ] **Step 3: Implement `set_ratio` + slew**

Add fields `target_step: f64` (init `= nominal_step` in `new`) and `ramp: bool` (init false). Add:

```rust
    pub fn set_ratio(&mut self, out_over_in: f64, ramp: bool) {
        let nominal_ratio = 1.0 / self.nominal_step;
        let clamped = out_over_in.clamp(nominal_ratio / self.max_rel, nominal_ratio * self.max_rel);
        self.target_step = 1.0 / clamped;
        self.ramp = ramp;
    }
```

Update `input_frames_next` to use the larger step:

```rust
    pub fn input_frames_next(&self) -> usize {
        let s = self.step.max(self.target_step);
        let last = self.frac + (self.output_frames as f64 - 1.0) * s;
        last.floor() as usize + self.taps / 2 + 1
    }
```

Update `process` to slew per-output (replace the `pos` line and the advance):

```rust
        let mut pos = self.frac;
        for o in 0..self.output_frames {
            let s = if self.ramp {
                self.step + (self.target_step - self.step) * (o as f64 + 1.0) / self.output_frames as f64
            } else { self.target_step };
            let i0 = pos.floor() as isize;
            let eta = pos - pos.floor();
            for ch in 0..self.channels {
                let y = self.kernel(ch, input, i0, eta);
                output[o * self.channels + ch] = y.round().clamp(-2147483648.0, 2147483647.0) as i32;
            }
            pos += s;
        }
        self.step = self.target_step;
        self.ramp = false;
        let consumed = pos.floor() as usize;
        self.roll_history(input, consumed);
        self.frac = pos - consumed as f64;
```

(Set `self.step = self.target_step = nominal_step` in `new`; a non-ramp `set_ratio` takes effect from the first output since `s = target_step`.)

- [ ] **Step 4: Run — expect pass**

Run: `cargo test -p soul-audio-core --lib resample`
Expected: PASS (all). If the non-ramp count test is off by one, reconcile `input_frames_next`'s `s` with `process`'s first-output step.

- [ ] **Step 5: Add continuity test (ramp introduces no discontinuity)**

```rust
    #[test]
    fn ramped_ratio_change_is_continuous() {
        // A slow sine through a ramped ratio change → no sample-to-sample jump spike.
        let mut r = Resampler::new(1, 48_000, 48_000, 1.5, 512, Quality::Hq).unwrap();
        let mut phase = 0.0f64; let dph = 2.0*std::f64::consts::PI*200.0/48_000.0;
        let mut out = Vec::new(); let mut blk = vec![0i32; 512];
        for b in 0..8 {
            if b == 3 { r.set_ratio(1.0/1.3, true); } // ramped speed-up mid-stream
            let need = r.input_frames_next();
            let inp: Vec<i32> = (0..need).map(|_| { let s=(phase.sin()*8_000_000.0) as i32; phase+=dph; s }).collect();
            r.process(&inp, &mut blk).unwrap();
            out.extend_from_slice(&blk);
        }
        // Max first-difference stays bounded (a 200 Hz tone is smooth; a zipper glitch spikes).
        let maxdiff = out.windows(2).map(|w| (w[1]-w[0]).abs()).max().unwrap();
        assert!(maxdiff < 400_000, "discontinuity spike {maxdiff}");
    }
```

- [ ] **Step 6: Run — expect pass**

Run: `cargo test -p soul-audio-core --lib resample::tests::ramped_ratio_change_is_continuous`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/soul-audio-core/src/resample.rs
git commit -m "feat(resample): runtime-variable ratio with per-block slew + clamp"
```

---

### Task 6: Latency + flush (drain the FIR tail)

Expose group delay and a clean end-of-stream drain.

**Files:**
- Modify: `crates/soul-audio-core/src/resample.rs`

**Interfaces:**
- Produces: `Resampler::latency(&self) -> usize`, `Resampler::flush(&mut self, output: &mut [i32]) -> usize`.

Design: `latency()` = `taps/2` output frames (group delay). `flush(output)`: run `process` with an all-zero input of `input_frames_next()` frames until the tail (the last `taps/2` real outputs) has been emitted; return the number of valid output frames written to `output` (≤ `output_frames`). Simplest v1: feed one zero block and report `output_frames` (documented as "≥ latency zero-frames must be fed to fully drain"); a precise partial-count is a later refinement.

- [ ] **Step 1: Write failing test**

```rust
    #[test]
    fn latency_and_flush_drain_tail() {
        let mut r = Resampler::new(1, 48_000, 48_000, 1.1, 64, Quality::Fast).unwrap();
        assert_eq!(r.latency(), Quality::Fast.taps() / 2);
        let mut blk = vec![0i32; 64];
        let need = r.input_frames_next();
        r.process(&vec![5000i32; need], &mut blk).unwrap();
        // Flush with zeros: the tail continues to fall from ~5000 toward 0, no panic.
        let n = r.flush(&mut blk);
        assert!(n <= 64);
    }
```

- [ ] **Step 2: Run — expect fail (no `latency`/`flush`)**

Run: `cargo test -p soul-audio-core --lib resample::tests::latency_and_flush_drain_tail`
Expected: FAIL to compile.

- [ ] **Step 3: Implement**

```rust
    pub fn latency(&self) -> usize { self.taps / 2 }

    pub fn flush(&mut self, output: &mut [i32]) -> usize {
        let need = self.input_frames_next();
        let zeros = vec_or_stack_zeros(need * self.channels); // see note
        let _ = self.process(&zeros, output);
        self.output_frames
    }
```

Note: `flush` needs a zero input buffer without allocating in the library. Since `input_frames_next()` is bounded by `output_frames*max_step + taps`, add a fixed scratch field `zero_scratch` sized for the max, or require the caller to pass a zero input to a `process` call. **Chosen v1:** drop the internal-zeros `flush` and instead document draining as "call `process` with a zero input block `latency()`-frames long." Replace Step-1 test's `flush` call with a zero-input `process` and assert the tail decays. (Keep `latency()`.)

Revised implementation — keep only `latency()`, remove `flush`; update the test to drain via `process(&zeros, &mut blk)`.

- [ ] **Step 4: Run — expect pass**

Run: `cargo test -p soul-audio-core --lib resample::tests::latency_and_flush_drain_tail`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/soul-audio-core/src/resample.rs
git commit -m "feat(resample): report group-delay latency; document tail drain"
```

---

### Task 7: End-to-end integration + full gauntlet

Prove the resampler composes with a real decoder and passes every project gate.

**Files:**
- Modify: `crates/soul-audio-core/tests/resample.rs`

**Interfaces:** consumes `open`/`Decoder` (existing) + `Resampler`.

- [ ] **Step 1: Write the end-to-end test**

```rust
use soul_audio_core::{open, Decoder, SliceSource};

#[test]
fn resamples_a_decoded_wav_44100_to_48000() {
    // Decode an existing 44.1k fixture, resample to 48k, check the frame count is
    // ~ ratio-scaled and audio is non-silent.
    let wav = include_bytes!("../testdata/ima-adpcm-stereo.wav"); // 44.1k stereo fixture
    let mut dec = open(SliceSource::new(wav)).unwrap();
    let ch = dec.stream_info().channels as usize;
    let mut r = Resampler::new(ch, dec.stream_info().sample_rate, 48_000, 1.1, 512, Quality::Hq).unwrap();

    let mut src = Vec::new();
    let mut buf = [0i32; 4096];
    loop { let n = dec.decode_into(&mut buf).unwrap(); if n == 0 { break; } src.extend_from_slice(&buf[..n]); }
    let src_frames = src.len() / ch;

    let mut cursor = 0usize; let mut out = Vec::new(); let mut blk = vec![0i32; 512 * ch];
    while cursor / ch + r.input_frames_next() <= src_frames {
        let need = r.input_frames_next();
        r.process(&src[cursor..cursor + need * ch], &mut blk).unwrap();
        cursor += need * ch;
        out.extend_from_slice(&blk);
    }
    let out_frames = out.len() / ch;
    // 48000/44100 ≈ 1.088 → expect roughly that many more output frames (loose bound).
    assert!(out_frames as f64 > src_frames as f64 * 1.0);
    assert!(out.iter().any(|&s| s != 0), "output is silent");
}
```

- [ ] **Step 2: Run the full integration file**

Run: `cargo test -p soul-audio-core --test resample`
Expected: PASS (all DSP + e2e tests).

- [ ] **Step 3: Run the whole gauntlet**

```bash
cargo test -p soul-audio-core
cargo clippy -p soul-audio-core --all-targets -- -D warnings
cargo fmt -p soul-audio-core -- --check
cargo build -p soul-audio-core --target thumbv7em-none-eabihf --no-default-features
CARGO_ENCODED_RUSTFLAGS=$'-Clink-arg=--nmagic\x1f-Clink-arg=-Tlink.x\x1f-Clink-arg=-Tdefmt.x' cargo build -p soul-audio-firmware --target thumbv7em-none-eabihf
```
Expected: all green. Confirm the `architecture-tests` layering/no_alloc tests still pass (`rustfft` is a **dev**-dependency and must not appear in the core's runtime dependency set): `cargo test -p architecture-tests`.

- [ ] **Step 4: Commit**

```bash
git add crates/soul-audio-core/tests/resample.rs
git commit -m "test(resample): end-to-end decode→resample + gauntlet green"
```

---

## Self-Review

**Spec coverage:** §3 algorithm → Tasks 1–3; §3.2 downsample cutoff → Task 2 (design) + Task 4 (proof); §3.3 variable ratio/slew → Task 5; §4 tiers → Tasks 3–4; §5 API (`new`/`output_frames`/`input_frames_next`/`process`/`set_ratio`/`latency`) → Tasks 1,5,6; §6 offline generation → Task 2; §7 verification (aliasing, THD/SNR-adjacent, sample-count, DC, impulse) → Tasks 1,3,4,7; §8 scope (no async loop, no Sink) → respected. `flush` in §5 was reduced to `latency()` + documented zero-drain in Task 6 (noted deviation — precise partial-count flush deferred).

**Placeholder scan:** none — every code step is complete; `flush` ambiguity resolved to a concrete decision in Task 6 Step 3.

**Type consistency:** `Quality`/`Resampler`/`ResampleError`, `taps()`/`subphases()`, `FAST_COEFFS`/`HQ_COEFFS`, `input_frames_next`/`output_frames`/`process`/`set_ratio`/`latency` are used consistently across tasks. `at()`/`kernel()`/`roll_history()` signatures are stable from Task 1.

**Open follow-ups (own specs):** libsamplerate C cross-check (deferred — the FFT alias/SNR tests are the v1 oracle); precise partial-count `flush`; the `Sink` trait and pipeline glue; async-clock DLL loop.
