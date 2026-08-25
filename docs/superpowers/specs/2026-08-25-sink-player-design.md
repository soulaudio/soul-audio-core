# soul-audio-core — Sink + Player Design (desktop first)

**Date:** 2026-08-25
**Status:** Implemented (desktop path) — as specified. Verified end to end:
`play` decodes an 8 kHz WAV, resamples 8000→48000 through the `Player`, and plays
it on the default cpal device. Core `Player`+`Sink` covered in CI via a mock
sink. The SAI/I²S sink remains deferred.
**Depends on:** `Decoder` (decode → interleaved i32) and `Resampler` (rate
conversion). Completes the playback pipeline: **decode → resample → sink**.

## 1. Purpose

Turn "decode + resample" into an actual **player**. Introduce the output
abstraction (`Sink`) and the pipeline driver (`Player`) that feeds it, plus a
working desktop **cpal** sink and a `play` command. The embedded SAI/I²S sink
implements the same trait later, on hardware.

The concrete sinks cannot live in `soul-audio-core`: cpal is std-only and SAI is
embassy/hardware-specific, both incompatible with the crate's `no_std`/no-alloc
rules. So the **device I/O is shell code**; the core holds only the *trait* and
the platform-agnostic *pipeline*.

## 2. Fixed decisions

| Decision | Choice | Rationale |
|---|---|---|
| Split | `Sink` trait + `Player` in core; concrete sinks in shells | Device I/O is platform-specific; the decode→resample→write loop is not. |
| Sink write | **Blocking write-all** with backpressure | The device buffer filling blocks the producer → paces decode to real time. On embedded, block = wait-for-DMA. |
| Sample domain | Interleaved **i32** in the source's scale end-to-end; the sink normalizes to its device format | Matches the decoder/resampler convention; keeps f32 precision (no early left-justify). |
| Rate matching | `Player` inserts a `Resampler` only when `decoder.rate ≠ sink.rate` | Zero-cost pass-through at matching rates. |
| Channels | Pass-through when equal; **mono → stereo by duplication**; error otherwise | Covers the common cases; downmix/arbitrary remap deferred. |
| Ratio | Fixed (`max_rel_ratio = 1.0`) for v1 | No varispeed/drift on desktop yet. |
| Desktop ring | `ringbuf` (lock-free SPSC) in the CLI | cpal's audio callback must be lock-free. |
| Testability | `Player` + `Sink` tested via a **mock sink** in CI; cpal sink is thin, verified by ear | No audio device in CI. |

## 3. Core additions (`soul-audio-core`)

### 3.1 `Sink` trait

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkError {
    /// The output device/stream is gone.
    Closed,
    /// The requested stream configuration is unsupported.
    Unsupported,
}

pub trait Sink {
    /// The rate / channels / format the device consumes.
    fn stream_info(&self) -> StreamInfo;
    /// Write all `frames` (interleaved i32, len = frame_count × channels),
    /// blocking as needed for space (backpressure). Returns on completion.
    fn write(&mut self, frames: &[i32]) -> Result<(), SinkError>;
    /// Play out any buffered audio and stop (end of stream).
    fn flush(&mut self) -> Result<(), SinkError>;
}
```

### 3.2 `Player`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerError {
    /// Sink channel count is incompatible with the source (and not mono→stereo).
    ChannelMismatch,
    /// Resampler could not be constructed for the rate pair.
    Resample(ResampleError),
    /// A decode error.
    Decode(DecodeError),
    /// A sink error.
    Sink(SinkError),
}

pub struct Player<D, K> { /* decoder, sink, optional resampler, fixed scratch */ }

impl<D: Decoder, K: Sink> Player<D, K> {
    /// Inspect both ends, choose the channel map, and build a resampler if the
    /// rates differ. `quality` selects the resampler tier (ignored at matching
    /// rates).
    pub fn new(decoder: D, sink: K, quality: Quality) -> Result<Self, PlayerError>;

    /// Decode → (resample) → (channel-map) → write, until end of stream, then
    /// flush the resampler tail and the sink. Blocks (paced by the sink).
    pub fn run(&mut self) -> Result<(), PlayerError>;
}
```

**Pipeline (inside `run`)**, all with fixed const-sized scratch (no alloc):
- Maintain a small FIFO of decoded interleaved i32. Fill it with `decode_into`.
- If a resampler is present: while the FIFO holds ≥ `resampler.input_frames_next()`
  frames, feed exactly that many, get one fixed output block, channel-map it,
  `sink.write` it, and drop the consumed input frames.
- If no resampler: channel-map decoded frames directly and `sink.write` them.
- At EOF: feed the resampler `latency()` silent frames to flush its tail, write
  the remainder, then `sink.flush()`.
- Channel map: equal → copy; mono source + stereo sink → duplicate each sample.

**Buffer sizing:** `OUTPUT_FRAMES` (resampler block, e.g. 1024) and the input FIFO
(≥ max `input_frames_next` + one `decode_into` chunk) are consts; scratch lives in
the `Player` struct. `MAX_CHANNELS` = 8.

## 4. Desktop sink (`soul-audio-cli`)

### 4.1 `CpalSink`

- `CpalSink::new(source_bits: u8) -> Result<Self>`: open the default output
  device, take its default config (rate, channels, sample format), build a
  `ringbuf` SPSC (producer in the sink, consumer moved into the callback), start
  the stream. Holds `source_bits` for normalization.
- `Sink::stream_info` → the device's rate/channels (bit depth is informational).
- `Sink::write` → push each i32 into the ring producer, parking briefly when full
  (backpressure) until the callback drains space.
- The **cpal callback** (consumer) pops i32, converts to the device sample format
  — `f32` = `sample as f32 / 2^(source_bits-1)`, clamped to [-1, 1]; `i16` =
  `(sample >> (source_bits-16))` clamped — and fills the output buffer; underruns
  write silence.
- `Sink::flush` → wait until the ring drains, then stop the stream.

### 4.2 `play` command

`soul-audio-cli play <input>` behind `--features playback` (existing cpal dep).
Opens the file → `open()` decoder → reads `stream_info()` → `CpalSink::new(bits)`
→ `Player::new(decoder, sink, Quality::Hq)` → `run()`. Runs on an 8 MiB-stack
thread (as `decode` does, for the large `AnyDecoder`).

## 5. Verification

**Core (`Player` + `Sink`), CI, via a mock sink** that records writes and reports
a configurable rate/channels:
1. **Pass-through** — equal rate + channels → sink receives exactly the decoded
   PCM.
2. **Rate conversion** — differing rates → sink receives ≈ `in_len × out/in`
   frames, matching a direct `Resampler` run.
3. **Mono → stereo** — mono decoder + stereo sink → each sample duplicated.
4. **Channel mismatch** — e.g. stereo source + mono sink → `new` returns
   `ChannelMismatch`.
5. **Flush** — the resampler tail is emitted and `flush` is called once at EOF.

**cpal sink** — thin glue; not CI-testable headless (no audio device). Verified by
playing a real file (`play`) and confirming clean audio. Any `play` E2E test is
`#[ignore]`d by default (run manually on a machine with an output device), so CI
stays green without hardware.

## 6. Scope

**In (v1):** core `Sink` trait + `Player` (rate conversion, mono→stereo, flush),
cpal `CpalSink` + `play`, tested via a mock sink.

**Out (own specs / later):**
- The **SAI/I²S DMA sink** (embedded) — implements `Sink` on hardware.
- The **async-clock DLL loop** — drives `set_ratio` from ring fill; needs the SAI
  sink + a live clock.
- Arbitrary channel **downmix / remap**, volume/EQ/DSP, gapless/crossfade.
- **Transport controls** (pause/seek during playback) beyond running to EOF.
- Device **format negotiation** beyond the device default config.

## 7. Risks / open questions

- **cpal callback real-time safety**: the ring must be lock-free (`ringbuf`); the
  `write` backpressure must park without holding a lock the callback needs.
- **Bit-depth normalization**: the sink scales by the *source* bit depth, passed
  at construction; a 32-bit source normalizes by `2^31` (some f32 low-bit loss,
  acceptable).
- **Underrun on slow decode**: the callback writes silence when the ring is empty
  (a brief gap), which is the correct fail-soft; the backpressure prevents
  overrun.
