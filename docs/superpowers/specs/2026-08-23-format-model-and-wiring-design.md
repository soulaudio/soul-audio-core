# soul-audio-core — Format Model & Decoder Wiring

**Date:** 2026-08-23
**Status:** Approved — implementing
**Builds on:** the source model (`Source` / `BufSource` / `SeekSource`).
**Scope this slice:** the **sample/format model** + the **wiring** that connects a
`Source` to a decoder (probe → `Format` → uniform decoder). The real decoder /
demuxer *internals* (container parsing, codec decode, decimation FIR) are the
**next** discussion and are stubbed here.

## 1. Principles this encodes

Two orthogonal axes — **Source** (where bytes come from) and **Format** (how to
interpret them) — stay decoupled and meet only at typed seams:

- **ISP:** each operation binds the *minimum* source capability (`probe` needs
  `BufSource`; streaming decode needs `Source`; seeking needs `SeekSource`).
- **DIP:** decoders are generic over the `Source` abstraction, never a concrete
  backend; the pipeline depends on the `Decoder` abstraction, never a concrete
  format.
- **OCP:** new format = new type implementing the trait; new DSD behaviour = new
  adapter. No edits to existing code.
- **LSP + Adapter:** PCM decoders are uniformly substitutable via
  `decode_into(&mut [i32])`. Native 1-bit DSD is a *different* data kind, so it is
  **segregated** into its own trait and re-joined to the PCM seam by explicit
  **adapters** (not a fat trait with optional methods).
- **Compile-time (source × format) validity:** a decoder's capability bound makes
  invalid cells unrepresentable (e.g. a seek-requiring decoder cannot be built
  over a non-seekable stream).

## 2. The model types (`src/model.rs`)

```rust
/// What a decoder emits.
pub enum SampleFormat {
    /// Signed integer PCM, `bits` significant bits, widened into i32.
    Int   { bits: u8 },
    /// IEEE float PCM (WAV float / MP3 float). Modelled now; float decode later.
    Float { bits: u8 },
    /// Native 1-bit DSD, packed 8 samples/byte.
    Dsd   { rate: DsdRate },
}

/// DSD bit-rate family (44.1 kHz base × 64/128/256).
pub enum DsdRate { Dsd64, Dsd128, Dsd256 } // 2.8224 / 5.6448 / 11.2896 MHz

/// Format-independent description of a stream.
pub struct StreamInfo {
    pub sample_rate: u32,      // PCM frame rate; for DSD, the base-family rate
    pub channels:    u16,
    pub format:      SampleFormat,
}

/// Container identity returned by `probe`.
pub enum Format { Wav, Flac, Mp3, Dsf, Dff }

/// Explicit DSD output mode (chosen by the caller; see §4 adapters).
pub enum DsdMode { Native, Dop, Decimate }
```

`StreamInfo` replaces the old `bits_per_sample: u16` with `format: SampleFormat`
(a `bits_per_sample()` helper preserves the convenience). `DsdRate::hz()` returns
the bit rate.

## 3. The decoder seam (two segregated domains)

```rust
/// PCM domain — WAV / FLAC / MP3 implement this. (Unchanged shape.)
pub trait Decoder {
    fn stream_info(&self) -> StreamInfo;
    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError>;
}

/// DSD domain — DSF / DFF implement this. Raw 1-bit, packed 8 samples/byte.
pub trait DsdDecoder {
    fn stream_info(&self) -> StreamInfo;                 // format = Dsd { rate }
    fn read_dsd(&mut self, out: &mut [u8]) -> Result<usize, DecodeError>;
}
```

## 4. DSD → PCM adapters (explicit mode = which adapter you construct)

Native SAI and DoP both need the raw bits; only decimation makes real PCM. The
mode is chosen by *constructing* the matching adapter — no runtime flag:

```rust
/// DoP: 0x05/0xFA marker + 16 DSD bits per 24-bit frame. Deterministic bit-packing.
pub struct DopAdapter<D: DsdDecoder> { .. }        // impl Decoder — BUILT + TESTED here
/// Decimation FIR → real i32 PCM.
pub struct DecimateAdapter<D: DsdDecoder> { .. }   // impl Decoder — STUB (Unsupported) here
```

- `DsdMode::Native` → use the `DsdDecoder` directly (shell clocks bits to SAI).
- `DsdMode::Dop` → `DopAdapter` (implemented + tested against a fake `DsdDecoder`).
- `DsdMode::Decimate` → `DecimateAdapter` (stub; FIR is its own future slice).

## 5. Source → Format → Decoder wiring

```rust
/// Identify the container by peeking magic bytes — consumes nothing, so the
/// chosen decoder still sees byte 0. (This is why `probe` binds `BufSource`.)
pub fn probe<S: BufSource>(src: &mut S) -> Result<Format, DecodeError>;

/// No-alloc stand-in for `Box<dyn Decoder>`: a runtime-format decoder that
/// delegates the trait to the active variant.
pub enum AnyDecoder<S: Source> { Wav(WavDecoder<S>), Flac(FlacDecoder<S>), Mp3(Mp3Decoder<S>) }
impl<S: Source> Decoder for AnyDecoder<S> { /* match self → delegate */ }

/// probe, then construct the PCM decoder. Returns `Unsupported` until the leaf
/// decoders are implemented — but the wiring is complete and exercised.
pub fn open<S: BufSource>(src: S) -> Result<AnyDecoder<S>, DecodeError>;
```

Magic bytes: `RIFF`…`WAVE` (WAV), `fLaC` (FLAC), `ID3` / MPEG sync `0xFF 0xEx`
(MP3), `DSD ` (DSF), `FRM8` (DFF). DSD formats are **identified** by `probe` but
**not** routed through `open` (they need an explicit `DsdMode`); their dispatch
is deferred to the decoder discussion. `open` handles the PCM formats.

## 6. Leaf decoders (wiring only; internals deferred)

The stubs are refactored to sit on the seam so `AnyDecoder` can wrap them:

- `WavDecoder<S: Source>`, `FlacDecoder<S: Source>`, `Mp3Decoder<S: Source>`
  become generic and `impl Decoder`; `DsfDecoder<S>`, `DffDecoder<S>` become
  generic and `impl DsdDecoder`.
- `new(src) -> Result<Self, DecodeError>` still returns `Unsupported` (no header
  parsing yet), so `open` returns `Unsupported` for now — **the leaf logic is the
  next slice**; only the wiring lands here.

## 7. Testing

- **Model** (`model.rs` unit): `DsdRate::hz`, `SampleFormat`/`StreamInfo`
  round-trips + `bits_per_sample()` helper.
- **Probe** (unit): each magic-byte pattern → correct `Format`; unknown → error;
  peeking leaves byte 0 intact (a follow-up read still sees the header).
- **AnyDecoder** (unit): a fake `Decoder` wrapped in `AnyDecoder` delegates
  `stream_info`/`decode_into` correctly.
- **DopAdapter** (unit): against a fake `DsdDecoder` emitting known bits, assert
  the DoP frames carry the right `0x05`/`0xFA` markers and 16-bit payloads.
- **Architecture unchanged:** `layering` (allowlist = `embedded-io`), `no_alloc`
  (probe + adapters use caller buffers), and the `thumbv7em --no-default-features`
  `no_std` gate stay green.

## 8. Out of scope (next discussions)

Real container parsing / demuxers, real codec decode (WAV PCM, FLAC, MP3), the
decimation FIR, DSD `open`/mode dispatch, float decode path, resampling, and the
audio sink.

## 9. Success criteria

- Host tests green (model + probe + AnyDecoder + DopAdapter + unchanged arch).
- clippy `-D warnings` + rustfmt clean; core builds `thumbv7em --no-default-features`.
- Public API exposes `SampleFormat`, `DsdRate`, `StreamInfo`, `Format`, `DsdMode`,
  `Decoder`, `DsdDecoder`, `AnyDecoder`, `DopAdapter`, `probe`, `open`.
- Adding a real decoder later touches only that leaf `formats/` type — no changes
  to the model, probe, `AnyDecoder`, or the wiring.
