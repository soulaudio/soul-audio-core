# soul-audio-core — Sample-Rate Conversion (Resampler) Design

**Date:** 2026-08-25
**Status:** Implemented (see as-built note)

> **AS-BUILT NOTE:** the coefficient *layout* changed during implementation. A
> fixed `[SUBPHASES][TAPS]` polyphase table (§3.1) cannot lower its cutoff for
> downsampling without runtime filter design, so it aliased when downsampling
> (e.g. 96→44.1). The shipped kernel uses **libsamplerate's layout** instead: one
> oversampled *half* of the windowed-sinc prototype, walked from each side of the
> output with the table index advancing by `L·min(1,out/in)` per input sample —
> which scales cutoff and tap count with the ratio, anti-aliasing both directions
> from a single const table. Everything else (fixed-output API, variable ratio +
> slew, Fast/HQ tiers, no-alloc streaming, offline table generation) is as
> specified. Downsample range is bounded by the per-channel history budget
> (`RatioRangeTooWide` on construction otherwise), and the generator is
> CI-guarded via `cargo xtask check-resample-tables`.
**Depends on:** the decode core (`Decoder` → interleaved `i32` PCM). Feeds the future
`Sink` abstraction (cpal on desktop, SAI/I²S DMA on STM32).

## 1. Purpose

A decoder emits PCM at the **source** rate (44.1/48/88.2/96/176.4/192 kHz, plus the
DSD-decimated rates like 352 800 / 176 400 Hz). Every output — a cpal device or a
DAC on a fixed I²S clock — consumes at *its own* fixed rate. The two rarely match, so
the pipeline is:

```
Decoder (src rate) → [ Resampler: src → sink rate ] → Sink (fixed rate)
```

The resampler is a pure PCM→PCM stage: format-agnostic (runs after any decoder),
`no_std` / no-alloc (same contract as the decoders), and fully host-testable. It is
the one component that unblocks *both* the cpal sink and the SAI sink, so it comes
first; `Sink` is the next subsystem.

**It must support a continuously-variable conversion ratio** (runtime-adjustable),
which is the superset of fixed conversion. This enables later varispeed and
async-clock drift correction without a redesign.

## 2. Fixed decisions

| Decision | Choice | Rationale |
|---|---|---|
| Algorithm | **Windowed-sinc polyphase** (Kaiser prototype, oversampled table, interpolate between sub-phases) | The universal SoTA core (libsamplerate, soxr, Speex, rubato). Textbook (JOS/CCRMA) — algorithm ported, no code lifted. |
| Sample domain | **f32 internal** | STM32H743 M7 has a hardware single-precision FPU (f64 is software-emulated → avoid). ~24-bit mantissa ≫ 20-bit audio noise floor. i32 PCM in → f32 → i32 out. |
| Ratio | **Continuously variable**, runtime-adjustable, slewed | User requirement. Fixed conversion = never changing it. |
| I/O model | **Fixed output block** (rubato `SincFixedOut`) | cpal callbacks and DAC/DMA periods both demand an exact frame count per call. Input consumed varies; caller queries `input_frames_next()`. |
| Buffer layout | **Interleaved i32**, per-channel f32 history | Matches the decoders' output; avoids rubato's planar `Vec<Vec>` (which needs alloc). |
| Coefficients | **Precomputed `const` tables** (generated offline) | Same pattern as the DSD Kaiser FIR / MP3 / kjmp2 tables. No runtime `sin`/Bessel, no `libm` dependency, deterministic. |
| Portability | **One implementation for device + desktop** | Project's founding goal: identical behavior on STM32 and desktop. Ours is `no_std`, so it runs everywhere. `rubato` (std+heap) is not used. |
| Quality | **Two tiers: Fast, HQ** | Fast for the tight embedded budget; HQ for desktop / RAM-rich. A third tier is a later add. |

## 3. Algorithm

### 3.1 Table-driven windowed-sinc (rubato-style)

A Kaiser-windowed sinc **prototype** low-pass filter of length `TAPS`, sampled at
`SUBPHASES` oversampled sub-positions, is stored as a `const` table
`COEFFS[SUBPHASES][TAPS]` (f32). Producing one output frame:

1. The read position `pos` (in input-frame units) has an **integer part** `n`
   (which input frames) and a **fractional part** `frac ∈ [0, 1)`.
2. `frac` selects a sub-phase: `sp = frac · SUBPHASES`, with integer `si` and
   remainder `η`.
3. The effective coefficients are **interpolated between adjacent sub-phases**:
   `coef[k] = COEFFS[si][k] + η · (COEFFS[si+1][k] − COEFFS[si][k])` (linear; HQ may
   use cubic). This raises effective phase resolution without a huge table — the
   libsamplerate/rubato trick.
4. Output frame (per channel) = Σ over `TAPS` of `coef[k] · history[n − k…]`.
5. Advance: `pos += step`, where `step = in_rate / out_rate` (the current ratio's
   reciprocal). The integer carry consumes input frames.

### 3.2 Anti-aliasing on downsample

When `out < in` (downsampling, `step > 1`), the prototype cutoff must drop to the
destination Nyquist or the fold-back aliases. The prototype's cutoff is **designed at
construction** for the worst-case downsample the declared ratio range allows
(`cutoff = min(1, out_min/in) `), and the output is **gain-renormalized** by the
cutoff factor. Small runtime ratio changes (varispeed/drift, the realistic case) keep
the fixed cutoff valid; a requested ratio outside the declared range is clamped
(and reported). *(Large arbitrary downsample sweeps are out of v1 scope — see §8.)*

### 3.3 Variable ratio + slew

- The ratio is `out_over_in: f64`, settable at runtime via `set_ratio`.
- On change, the effective `step` is **linearly slewed across the next output
  block** (libsamplerate/rubato `ramp` behavior) so the ratio never steps
  discontinuously → no zipper artifacts.
- The permitted range is declared at construction (`max_rel_ratio`, e.g. 1.1 for
  ±10 %); the cutoff (§3.2) is designed for it. Out-of-range → clamp + flag.

## 4. Quality tiers

Concrete parameters (Kaiser: `β = 0.1102·(A−8.7)`; taps `≈ (A−7.95)/(14.36·Δf)`):

| Tier | `TAPS` × `SUBPHASES` | inter-phase interp | Table (f32) | Target | ~M7 cost (stereo) |
|---|---|---|---|---|---|
| **Fast** (embedded default) | 32 × 128 | linear | ~16 KB | ~97 dB SNR, ~90 % BW (16-bit transparent) | ~3–5 % @ 96 kHz |
| **HQ** (desktop / RAM-rich) | 64 × 256 | cubic | ~64 KB | ~120 dB SNR, ~95 % BW (24-bit class) | ~10–20 % @ 192 kHz |

Tables live in flash (`const` rodata) on embedded — the H743 has ~2 MB flash, so 64 KB
is comfortable — and are feature-gateable so a build only pays for the tiers it uses.
Per-channel f32 history is a fixed array ring (`MAX_CHANNELS × TAPS × 4 B`, sub-kB).

## 5. Public API (shape)

```rust
pub enum Quality { Fast, Hq }

pub struct Resampler { /* const table ref, f32 history rings, phase state, ratio+slew */ }

impl Resampler {
    /// `channels`, nominal `in_rate`/`out_rate`, `max_rel_ratio` (ratio-variation
    /// bound), fixed `output_frames` per call, and quality tier. No allocation.
    pub fn new(channels: usize, in_rate: u32, out_rate: u32,
               max_rel_ratio: f64, output_frames: usize, quality: Quality)
        -> Result<Self, ResampleError>;

    /// How many input frames the caller must supply for the next `process` call
    /// (varies with the current ratio).
    pub fn input_frames_next(&self) -> usize;

    /// Fixed output-frame count per call (from `new`).
    pub fn output_frames(&self) -> usize;

    /// Consume exactly `input_frames_next() * channels` interleaved i32 samples,
    /// produce exactly `output_frames() * channels` interleaved i32 samples.
    pub fn process(&mut self, input: &[i32], output: &mut [i32]) -> Result<(), ResampleError>;

    /// Set the conversion ratio (out/in) for subsequent output; `ramp` slews it
    /// smoothly across the next block. Clamped to the declared range.
    pub fn set_ratio(&mut self, out_over_in: f64, ramp: bool);

    /// Group delay in output frames (≈ filter half-length) — for A/V sync.
    pub fn latency(&self) -> usize;

    /// Emit the remaining FIR tail at end-of-stream (feeds internal zeros).
    pub fn flush(&mut self, output: &mut [i32]) -> usize;
}
```

**Data flow with a `Sink`:** the sink wants `output_frames` (its DMA/cpal block);
the pipeline asks `input_frames_next()`, pulls that many frames from the decoder
(buffering across decoder-frame boundaries — the pipeline's job, not the
resampler's), calls `process`, and hands the fixed block to the sink.

## 6. Coefficient generation

An **offline generator** (checked-in xtask or script, mirroring the kjmp2 N/D
generation) computes each tier's Kaiser-windowed sinc table and emits the `const`
arrays into a `resample_tables.rs` (like `mp3_l12_tables.rs`). No build-time codegen;
the tables are committed and the generator + parameters documented so they're
reproducible. The generated header is verified against the generator in CI (regenerate
→ diff must be empty) to prevent drift.

## 7. Verification

Objective DSP metrics as automated tests with dB thresholds (the infinitewave.ca
methodology, in code):

1. **Aliasing/imaging** — resample a swept sine, FFT the output, assert energy above
   the destination Nyquist (fold-back) is **below the tier's stopband target**
   (Fast ≥ ~90 dB down, HQ ≥ ~120 dB).
2. **THD/SNR** — resample a 1 kHz tone, assert harmonic + noise floor below target.
3. **Passband flatness** — assert magnitude flat within tolerance up to the tier's BW.
4. **Sample-count math** — `input_frames_next` / `process` produce exactly the
   promised counts across a spread of ratios (incl. up, down, and a runtime change).
5. **DC / impulse** — DC preserved; impulse response symmetric (linear phase).
6. **Cross-check vs libsamplerate** — compile SRC as a C reference (as done for
   kjmp2), resample identical signals, compare SNR/spectra within tolerance.

The FFT for tests is a dev-dependency (host-only); the library itself pulls in
nothing new.

## 8. Scope

**In (v1):** fixed *and* continuously-variable ratio (bounded range), Fast + HQ
tiers, interleaved i32, `no_std`/no-alloc, fixed-output API, one implementation for
device + desktop.

**Out (v2+):**
- The **async-clock control loop** (Adriaensen DLL, ~0.05 Hz loop, ±50 ppm drift) —
  a *consumer* of `set_ratio`, belonging with the buffer/`Sink` layer, not the
  resampler core. The `set_ratio` + slew it needs is in v1.
- **Large arbitrary downsample sweeps** (cutoff re-design at runtime). v1 designs the
  cutoff for a bounded range; wide async downsampling would need a re-designed or
  FFT-based path.
- A **Linear/rough tier** and a **>120 dB reference tier** — later adds.
- The **`Sink`** trait and the **pipeline glue** (decoder→resampler→sink) — the next
  spec.

## 9. Rubato inspiration mapping

| Rubato | Here |
|---|---|
| `SincFixedOut` (fixed output, variable input) | the I/O model (§2, §5) |
| `SincInterpolationParameters { sinc_len, oversampling_factor, interpolation, window }` | the tier parameters (§4) |
| `set_resample_ratio(ratio, ramp)` | `set_ratio(out_over_in, ramp)` (§3.3) |
| `max_resample_ratio_relative` (bound at construction) | `max_rel_ratio` (§3.2, §5) |
| `input_frames_next()` / `process_into_buffer` (no alloc in steady state) | `input_frames_next()` / `process` (§5) |
| planar `Vec<Vec<f32>>` | replaced by interleaved i32 + fixed f32 history rings (no alloc) (§2) |

## 10. Risks / open questions

- **HQ cost on embedded**: HQ at 192 kHz stereo is ~10–20 % of the M7 by estimate;
  the numbers are first-order (need on-target DWT measurement). Fast is the embedded
  default; HQ is opt-in.
- **Cutoff-for-range vs quality**: designing the cutoff for the worst-case downsample
  slightly narrows passband at the nominal ratio. Acceptable for bounded ranges;
  documented.
- **Table flash budget**: two tiers = ~80 KB flash. Feature-gate per tier so an
  embedded build can drop HQ.
```
