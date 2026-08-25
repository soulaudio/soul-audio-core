# soul-audio-core / soul-audio-engine — Playback Engine Design

**Date:** 2026-08-25
**Status:** Proposed (design review)
**Supersedes:** the blocking `Player` in `2026-08-25-sink-player-design.md` (that
`Player::run` becomes the `Deck` below; the `Sink` trait carries over, refined).
**Research basis:** MPD (decoder/player/output split + gapless pipe swap),
GStreamer `about-to-finish`, mpv `open_thread`/prefetch, kira (rtrb + triple-buffer
+ deferred-drop RT discipline), rodio (mixer/queue), Bencina real-time-101,
STM32 SAI circular-DMA + `heapless::spsc`. See the session research log.

## 1. Goal

A **portable playback engine** — the "play a library of tracks" logic — that runs
identically on the STM32 DAP (`soul-listener`) and the desktop app
(`soul-audio-player`); only the **sink** and **track source** differ. Both are
downstream app projects; this repo ships the reusable libraries.

## 2. The invariant that drives everything

**The real-time boundary is a lock-free SPSC ring, and it lives inside the
`Sink`.** Only the ring's *consumer* is real-time.

- **RT consumer** — cpal's callback *or* the STM32 DMA half/complete ISR. Pops the
  ring, does bounded arithmetic (format-convert; the samples are already
  normalized + volumed), plays **silence on underrun**. Never allocates, locks,
  blocks, or does I/O.
- **Producer** — the **deck's `pump()`**, on a normal thread/task: decode →
  resample → normalize → volume → `push` to the ring (backpressure when full).
- **Control** — UI thread / buttons.

Because the deck and engine live entirely *off* the RT thread, **they use plain
sync `no_std` code** — the lock-free discipline is confined to the sink's ring
(`heapless::spsc` / `ringbuf::StaticRb` embedded; a desktop SPSC ring). Same
boundary on both platforms.

## 3. Crate layering

```
soul-audio-core   (no_std)  decode(+seek+duration) · resample · Sink trait · Deck
soul-audio-engine (no_std)  queue cursor · Normal/RepeatOne/RepeatAll · next/prev/goto
                            · emits NeedOpen(i) · installs provide(decoder)
app / platform              Sink impl (owns the ring + RT callback/ISR) ·
                            TrackProvider (open i) · threads: RT consumer · pump · loader
```

## 4. `soul-audio-core` additions

### 4.1 `AnyDecoder` seek + duration (step 1, prerequisite)
- `total_frames(&self) -> Option<u64>` on each concrete decoder (WAV/FLAC from
  headers, MP3 from Xing/Info) and dispatched on `AnyDecoder`.
- `seek_to(sample)` / `seek_to_time(secs)` on `AnyDecoder<S: SeekSource>`,
  dispatching to WAV/FLAC/MP3; the streaming **Ogg-FLAC** variant returns
  `Unsupported`.

### 4.2 `Deck` — the producer-side player mechanism
```
Deck<K: Sink, D: SeekableDecoder, const PRELOAD: usize>
    load(decoder)          // play now; discards any preload
    preload(decoder)       // fill the next free preload slot (gapless)
    advance()              // promote the front preload slot to current now (hard skip, reuses primed decoder)
    clear_preloads()
    play() / pause() / stop()
    seek(frames) / set_volume(f32)     // volume ramped, applied in the deck
    pump() -> Progress                 // one block: decode→resample→normalize→volume→sink.write
    state() / position_frames() / duration_frames() / sample_rate()
    free_preload_slots()
```
- Holds `current` + `[preload; PRELOAD]` decoders (each a full decoder — RAM =
  `(PRELOAD+1) × per-decoder`; `PRELOAD=1` on the DAP, more on desktop).
- **Takes opened decoders, not sources** — opening is slow I/O and happens
  off-thread (§5). `D: SeekableDecoder` = `Decoder + seek + total_frames`
  (`AnyDecoder` implements it; a mock implements it for tests).
- **Normalizes to canonical `i32` after resampling** (keeps f32 precision on the
  small source-scale values), so the sink is bit-depth-agnostic.
- **Volume is a ramped value applied in the deck** (~20 ms ramp, click-free); with
  a small ring the latency is negligible and it stays portable.
- On track end (or `advance`): swap `current ← preload[0]`, **rebuild the resampler
  + channel-map for the new track's rate/channels**, emit `TrackChanged`. Ask for
  another preload via `WantPreload` when a slot frees.
- `pump()` events: `Playing` / `WantPreload` / `TrackChanged` / `Ended` / `Error`.

### 4.3 `Sink` trait (carried over, refined)
Unchanged surface (`stream_info` / `write` / `flush`); the *implementation* owns
the SPSC ring + RT consumer. `write` is the producer push (backpressure). Pause is
a sink/output action (feed silence, keep clocks) exposed by the concrete sink; the
ring stays **small** for low pause latency.

## 5. `soul-audio-engine`

Owns the **queue cursor + play mode** (`Normal`/`RepeatOne`/`RepeatAll`; shuffle
deferred to app-supplied order via `goto`) and `next`/`prev`/`goto`. It **decides
which index** plays next and drives the deck — but does **no I/O**:

- On the deck's `WantPreload`, the engine computes the next index and emits
  `NeedOpen(index)`.
- The **driver** (app/platform) opens that index off-thread via a `TrackProvider`
  (`open(index) -> Decoder`) and calls `engine.provide(index, decoder)`, which
  installs it into the deck's preload.
- On the deck's `TrackChanged`, the engine advances its cursor.

`next`/`skip` in the UI = engine changes cursor → `NeedOpen`/`provide`, then
`deck.advance()` (reuse a primed slot) or `deck.load()` (fresh). This is
GStreamer's `about-to-finish` pattern with the open offloaded (MPD/mpv).

## 6. The driver (app / platform, NOT in these crates)

Wires three non-RT contexts + one RT:
- **RT:** the sink's callback / DMA ISR (pops the ring).
- **pump:** loops `deck.pump()` (or `engine`-driven) into the sink; paced by
  backpressure.
- **loader:** opens the next track (`TrackProvider`) off-thread and hands the
  decoder to the engine (`provide`) — a ~150 KB decoder crosses loader→pump as a
  **move** (pointer/pool-slot swap), never a decode on the pump thread.
- **control:** UI/buttons call `play/pause/seek/next` (synchronized with the pump
  via a mutex or command queue — both non-RT, so ordinary sync is fine).

Desktop = cpal sink + std threads; DAP = SAI-DMA sink + embassy tasks (the DMA ISR
wakes the pump/loader tasks). Same deck + engine.

## 7. Verification

- **core:** `AnyDecoder` seek/duration tested against fixtures; `Deck` tested
  against a **mock sink** + **mock `SeekableDecoder`s** (canned sources) in CI:
  play-to-end, gapless preload+swap, `advance` (skip reuses primed slot),
  `clear_preloads`, seek, rate/channel change across a boundary, volume ramp,
  underrun→silence, EOF→`Ended`.
- **engine:** tested with a **mock provider**: `next`/`prev`/`goto`, repeat modes,
  `NeedOpen`/`provide` timing, cursor advance on `TrackChanged`.
- **desktop:** the cpal sink + threaded driver, verified by ear (not CI).

## 8. Scope

**In:** core seek/duration; `Deck` (load/preload/advance/clear/transport/seek/
volume/pump, gapless, normalize, ramped volume); `soul-audio-engine` (queue +
`NeedOpen`/`provide`); the desktop sink + threaded driver (in the CLI or a small
desktop helper).

**Out (later / app):** the actual track list + tags/metadata + persistence
(`TrackProvider` is the seam); shuffle order (app-supplied); the STM32 SAI sink +
embassy driver (needs hardware); async-clock drift loop; cross-fade (distinct from
gapless).

## 9. Notes / risks

- **RAM:** `PRELOAD ≥ 1` holds ≥ 2 full decoders. `AnyDecoder` is ~150 KB
  (multichannel-FLAC-dominated). The DAP uses `PRELOAD=1`; a lighter "open header
  only, decode-on-swap" preload is a possible future optimization (mpv-style).
- **Loader↔pump handoff** of a large decoder: a move on the pump thread
  (microseconds) on desktop; a static-pool slot swap on embedded.
- **Volume placement:** deck-side (portable, ramped, small-ring latency) chosen
  over sink-callback-side (instant but per-platform). Revisit only if latency bites.
```
