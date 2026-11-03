# soul-audio-core — Demuxer, Slice 1: WAV + DSD verticals

**Date:** 2026-08-24
**Status:** Approved — implementing
**Builds on:** the source model + the format model/wiring (probe, `Decoder`/
`DsdDecoder`, `DopAdapter`, `AnyDecoder`, `open`).
**Scope:** the demuxer (container-parsing) layer, delivered as **two working
verticals** — WAV decoding end-to-end, and raw DSD (DSF/DFF) normalized to feed
the existing DoP adapter. FLAC + MP3 demuxers are **slice 2**.

## 1. Research grounding (see the four agent reports)

Mature libraries (Symphonia, FFmpeg, dr_libs, miniaudio, libsndfile) converge on
demuxer/codec separation; our no-alloc constraint changes *who owns the bytes*,
not the model:

- **Packet = a `Copy` descriptor + a borrowed payload slice** (Symphonia's
  `Packet.data: Box<[u8]>` is the one thing we can't copy).
- **Demuxer (container) is separate from codec (samples).** Our existing
  `Decoder`/`DsdDecoder` are the *codec* seam.
- **Single-track simplification:** all our formats are single audio stream — we
  skip Symphonia's multi-`Track` machinery; one `StreamInfo` suffices.
- **Seeking gated by `S: SeekSource`** (compile-time, not a runtime flag).

## 2. The demuxer abstraction (`src/demux/`)

```rust
/// A demuxed unit of one stream — a Copy descriptor over a borrowed payload.
pub struct Packet<'a> {
    pub ts: u64,        // presentation timestamp, in frames
    pub dur: u32,       // frames in this packet
    pub data: &'a [u8], // borrows caller scratch
}

/// A container parser: header → StreamInfo, then a pull-stream of packets.
/// Owns its Source (matches our decoders). No allocation.
pub trait Demuxer {
    fn stream_info(&self) -> StreamInfo;
    fn next_packet<'a>(&mut self, scratch: &'a mut [u8])
        -> Result<Option<Packet<'a>>, DecodeError>; // Ok(None) = end of stream
}
```

Seeking is added only on concrete demuxers whose source is seekable:
`impl<S: SeekSource> WavDemuxer<S> { fn seek_to(&mut self, frame: u64) -> Result<u64, DecodeError> }`.

## 3. WAV vertical (PCM, end-to-end)

### 3a. `WavDemuxer<S: Source>` (`src/demux/wav.rs`)
Parse RIFF/WAVE per the research (all little-endian; chunk = `id|size:u32|body|pad(size&1)`):
- Validate `RIFF`…`WAVE`. Walk chunks, **advancing `size + (size & 1)`** (the #1
  desync bug). Capture `fmt `; on `data`, record `data_start`/`data_len` and stop.
- `fmt `: `wFormatTag`, channels, rate, `nBlockAlign`, `wBitsPerSample`; if
  `WAVE_FORMAT_EXTENSIBLE` (0xFFFE), unwrap the SubFormat GUID's first `u16` to
  get the real tag (0x0001 PCM / 0x0003 float). Reject non-PCM/float here.
- Surface `StreamInfo { sample_rate, channels, format }` where `format =
  Int{bits}` (or `Float{bits}`, decoded later). `bits = wBitsPerSample`.
- `next_packet`: fill `scratch` with the next PCM bytes from the data region
  (bounded by `data_len`; aligned to `nBlockAlign`); `dur = bytes/blockAlign`.
- `seek_to(frame)` (SeekSource): `data_start + frame * nBlockAlign`.
- Edge cases handled: unknown-size `data` (`0xFFFFFFFF` → read to EOF when not
  seekable / to stream end when seekable), unknown chunks skipped, 8-bit unsigned
  vs ≥9-bit signed, 24-bit packing.

### 3b. PCM codec → `WavDecoder<S: Source>: Decoder` (`src/formats/wav.rs`, real)
Composes `WavDemuxer` + PCM widening into the existing `Decoder` seam:
- `decode_into(out: &mut [i32])`: read PCM bytes from the demuxer's data region
  and **widen to i32** per `bits` — 8-bit `unsigned→signed` (`b - 128`), 16-bit
  `i16` sign-extend, 24-bit 3-byte LE sign-extend, 32-bit `i32`. Widened value
  keeps the true depth (not rescaled); `StreamInfo.bits` carries the depth.
- `Ok(0)` at end of data. Float PCM → `Unsupported` (slice 2).

## 4. DSD vertical (DSF/DFF → normalized MSB-first → DoP)

Both DSD demuxers normalize to **MSB-first "channel bytes"** (bit 7 = oldest
sample), the canonical form our `DopAdapter` already consumes.

### 4a. `DsfDecoder<S: Source>: DsdDecoder` (`src/formats/dsf.rs`, real)
DSF is Sony/little-endian; chunk sizes **include** the 12-byte header. Fixed
layout: `DSD ` (28) → `fmt ` (52) → `data`. Capture channel num, sampling
frequency (→ `DsdRate`), sample count, block size (=4096). Audio at offset 92.
- **Normalization:** DSF is planar (per-channel 4096-byte blocks) and **LSB-first**.
  `read_dsd` transposes planar→interleaved and **bit-reverses each byte** via a
  `const BIT_REVERSE: [u8; 256]`, emitting clustered frames (one byte/channel in
  order). Trim trailing padding via `sample_count` (`ceil(count/8)` valid
  bytes/channel). Holds an inline block-row buffer `[u8; MAX_CH * 4096]`.
- **Scope cap:** support ≤ `MAX_DSD_CHANNELS = 2` (stereo — the common case);
  reject more with `Unsupported`. Multichannel is a follow-up.

### 4b. `DffDecoder<S: Source>: DsdDecoder` (`src/formats/dff.rs`, real)
DFF is Philips/big-endian IFF; `ckDataSize` **excludes** the 12-byte header
(advance `12 + size + (size & 1)`). Walk `FRM8`/`DSD ` → `PROP`(`SND `) recursing
over `FS ` (rate), `CHNL` (channels/order), `CMPR` (`DSD `=raw / `DST `=compressed)
→ `DSD ` sound-data chunk. DFF is already clustered + **MSB-first**, so `read_dsd`
is a passthrough. **`DST` (compressed) → `Unsupported`** (needs a DST decoder).

### 4c. End-to-end
`DsfDecoder`/`DffDecoder` (`DsdDecoder`) → wrap in the existing
`DopAdapter` (`Decoder`) → interleaved `i32` DoP frames. This makes raw DSD →
DoP work end-to-end. (Native SAI + decimation paths already have their seams.)

## 5. Integration

- `open()` for WAV now returns a **working** `WavDecoder` (probe → construct →
  real `decode_into`). `AnyDecoder::Wav` decodes real PCM.
- DSD stays out of `open()` (needs an explicit `DsdMode`); this slice exposes the
  concrete `DsfDecoder`/`DffDecoder` + `DopAdapter` path for callers to compose
  explicitly. A unified `open_dsd(src, mode)` is deferred (return type varies by
  mode).
- `probe` unchanged (already identifies all five).

## 6. Testing (thorough, per format — hand-built byte fixtures, self-contained)

- **WAV demux:** header → StreamInfo for PCM16, EXTENSIBLE (float + 24-bit),
  odd-size chunk padding (desync guard), unknown-chunk skipping, unknown-size
  `data`. `next_packet` byte counts. `seek_to` offset math.
- **WAV decode (end-to-end):** hand-built 16-bit and 24-bit WAV byte arrays →
  assert exact widened `i32` output (incl. sign for negatives); `open()` path.
- **DSF:** planar LSB-first fixture → assert `read_dsd` yields the correct
  bit-reversed, interleaved MSB-first bytes; padding trimmed via `sample_count`;
  rate mapping; >2ch rejected.
- **DFF:** IFF fixture (`FRM8`/`PROP`/`FS `/`CHNL`/`CMPR`/`DSD `) → StreamInfo +
  passthrough bytes; `DST ` → `Unsupported`.
- **DSD → DoP end-to-end:** `DsfDecoder` + `DopAdapter` → assert DoP frames carry
  the expected markers + normalized DSD payload.
- **Architecture (unchanged):** `layering` (core dep allowlist = `embedded-io`),
  `no_alloc` (demuxers use inline/caller buffers), `thumbv7em --no-default-features`.

## 7. Out of scope (later slices)

FLAC + MP3 demuxers (slice 2); FLAC/MP3 *codec* decode; DST decode; DSD
decimation FIR; multichannel DSD (>2ch); float PCM decode; WAV RF64/RIFX; DSD
seeking; a unified `open_dsd`.

## 8. Success criteria

- `open(wav_bytes)` decodes real interleaved `i32` PCM (16- and 24-bit) matching
  hand-computed values.
- `DsfDecoder`/`DffDecoder` yield correct normalized MSB-first DSD bytes; DSF is
  bit-reversed + de-interleaved; `DopAdapter` over them produces valid DoP frames.
- Host tests + clippy `-D warnings` + fmt clean; core builds `thumbv7em
  --no-default-features`; firmware links.
- Adding FLAC/MP3 later touches only their `demux/` + `formats/` files.
