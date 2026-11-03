# soul-audio-queue — Two-Level Playback Queue Design

**Date:** 2026-08-30
**Status:** Proposed (design review)
**Builds on:** `2026-08-25-playback-engine-design.md` (the `Deck` mechanism + the
`Engine` queue cursor). This design adds the **queue *policy*** layer above the
engine and refactors the engine to delegate "what plays next" to a trait.
**Research basis:** consumer streaming apps (Spotify, Apple Music, YouTube Music,
Amazon Music); local/audiophile players (foobar2000, MPD, mpv, Roon, VLC, Winamp,
Clementine/Strawberry); and the shuffle/repeat/previous/gapless literature
(Spotify Engineering, Martin Fiedler's *balanced shuffle*, Ruuda's optimal
interleave, the MPD protocol, Hydrogenaudio). Full findings + sources in §14.

---

## 1. Goal

A **portable Up-Next playback queue** — the "queues, play-next, shuffle, repeat"
brain — reusable by the desktop player and **native on the STM32 DAP**. It sits
above `soul-audio-engine` and decides *which track* plays next; the engine keeps
driving the gapless `Deck`. It must **scale to arbitrarily large libraries**
(millions of tracks) in **O(1) memory**, stay `no_std` + `forbid(unsafe_code)`,
and be no-alloc on the DAP (growable only behind an `alloc` feature on desktop).

## 2. Constraints

- **`no_std`, native on the DAP, reusable on desktop.** Bounded (`heapless`)
  storage on the DAP; growable (`alloc`) storage behind a feature on desktop.
- **Big-library scale.** "Shuffle all 2,000,000 tracks" must cost the same memory
  as shuffling 10. The queue may **not** materialize the source list or a
  permutation array.
- **Metadata-agnostic.** Like the rest of `soul-audio-core`, the queue traffics in
  **opaque `Copy` handles**, never paths/tags. Artist/album-aware behaviour
  (smart shuffle) is an app-layer concern where the metadata lives.
- **Codec-agnostic gapless.** The queue provides look-ahead; encoder delay/padding
  trimming stays in the decoder/`Deck`.

## 3. The split (chosen approach)

Three layers, acyclic (`core → engine → queue → app`); the engine never depends
on the queue:

```
soul-audio-core   (no_std)  Deck — gapless mechanism (unchanged)
soul-audio-engine (no_std)  gapless DRIVER: poll_open / provide / pump + preload slots.
                            Delegates "what's next?" to the `Sequencer` trait.
                            Ships LinearSequencer (= today's len/cursor/repeat) for DAP-simple.
soul-audio-queue  (no_std)  TwoLevelQueue: virtual source context + Up-Next + shuffle + history.
                            heapless-bounded; optional `alloc`/`serde` features. impls Sequencer.
app / platform              handle → openable source, metadata, smart-shuffle, persistence store,
                            the concrete Sink, and the threads.
```

Rationale for this split over the alternatives considered:

- **Fold into the engine (rejected):** couples the reusable gapless driver with
  growable queue storage; the simple DAP folder-player would pull in the whole
  machine.
- **Queue above an unchanged engine (rejected):** two components both believe they
  own "what's next" (the engine's `RepeatMode` auto-advance vs the queue), and the
  preload-peek seam becomes a gapless-bug source.
- **Chosen — delegate via a trait:** preserves the engine's tested gapless driver,
  turns "what's next" into a pluggable policy (the actual new requirement), and
  isolates the growable/`alloc` concerns in one crate both platforms share.

## 4. The `Sequencer` seam

Defined in `soul-audio-engine`; implemented by both the trivial linear player and
the rich queue. `no_std`, storage-free:

```rust
/// The "what plays next" policy. `H` is an opaque track handle the app maps to
/// an openable source (never a path or tags).
pub trait Sequencer {
    type Handle: Copy + Eq;

    /// The track playing now (None = nothing to play).
    fn current(&self) -> Option<Self::Handle>;

    /// The k-th track that WOULD play after `current` at natural boundaries,
    /// WITHOUT mutating (k = 1 is the immediate next). Reflects the Up-Next
    /// queue, shuffle order, and repeat mode. Drives gapless preload.
    fn upcoming(&self, k: usize) -> Option<Self::Handle>;

    /// Commit a boundary crossing. `Advance::Natural` vs `Skip` lets Repeat::One
    /// repeat on natural end but move on for a user skip. Pops the Up-Next front /
    /// advances the source cursor, pushes the old current onto history, returns
    /// the new current.
    fn advance(&mut self, trigger: Advance) -> Option<Self::Handle>;

    /// Step back through real play history; returns the new current.
    fn prev(&mut self) -> Option<Self::Handle>;

    /// Direct selection within the source context.
    fn goto(&mut self, pos: usize) -> Option<Self::Handle>;
}

pub enum Advance { Natural, Skip }
```

**The engine becomes self-correcting.** Instead of owning "what's next," it treats
the `Sequencer` as the single source of truth and **reconciles the deck to it each
`pump`/poll cycle**:

- Compare the deck's *current* + *primed preload slots* against `seq.current()` /
  `seq.upcoming(1..=PRELOAD)`.
- Match → nothing to do (gapless stays armed). Mismatch → reuse a primed slot that
  already holds the wanted handle (instant gapless swap), else clear stale
  preloads and re-open.

This single principle solves the hard case: when the user hits **Play Next** and
the immediate successor changes, the primed preload is now stale — next cycle the
engine notices `upcoming(1)` changed, discards the stale slot, primes the new one.
No dual-authority.

Consequences:

- `Engine<Sink, Decoder, Seq: Sequencer, const PRELOAD>` owns the deck **and** the
  sequencer. The app mutates the queue via `engine.sequencer_mut()`; the engine
  self-corrects, so mutation methods stay off the base trait (the trivial linear
  player stays tiny).
- `poll_open`/`provide` traffic in `Seq::Handle` instead of a bare `usize`.
- Today's `len`/`cursor`/`RepeatMode` becomes a `LinearSequencer` impl (~50 lines,
  in the engine) for DAP folder playback.

## 5. `TwoLevelQueue` model

```rust
TwoLevelQueue<C: Context, Up: QueueBuf<C::Handle>, Hist: QueueBuf<(C::Handle, usize)>> {
    ctx:        C,                         // virtual source context (positions → handles), O(1)
    pos:        usize,                     // cursor in PLAY order, 0..ctx.len()
    seed:       Option<u64>,              // None = linear; Some = keyed-permutation shuffle
    upnext:     Up,   // Deque<H>          // bounded, materialized (Play Next / Add to Queue)
    history:    Hist, // Deque<(H, usize)> // bounded; (handle, pos) → exact prev restore
    repeat:     Repeat,                    // Off | All | One
    stop_after: StopAfter,                 // None | Track | Queue
}
```

- **Virtual source context** — the queue never copies the track list. It stores a
  position and pulls handles on demand through `Context` (§8). O(1) in library
  size (a few hundred bytes regardless of N).
- **Up-Next** — the manual queue: `play_next` pushes front, `enqueue` pushes back
  (FIFO). Bounded + materialized (small, user-driven).
- **History** — `(handle, pos)` records for exact `prev` restore; bounded ring
  (drops oldest when full).

## 6. Semantics

**What's next:** *Up-Next wins on a normal advance; `Repeat::One` is the sole
override.*

- `advance(Natural)` **with `Repeat::One`** → **loop the current track**: `pos`,
  `upnext`, and `history` are all untouched (the queue is frozen). This is the only
  case where repeat overrides Up-Next.
- `advance(Natural | Skip)` **otherwise** → push current→history, then **if `upnext`
  non-empty pop its front** (Up-Next wins); else step the source: `Off` = `pos+1`
  (stop at end), `All` = `(pos+1) mod N` (wrap). A manual `Skip` under `Repeat::One`
  also lands here — an explicit skip overrides the freeze.
- `upcoming(k)` mirrors this without mutating: under `Repeat::One` it is `current`
  for every `k` (so the engine preloads the track for a gapless loop); otherwise it
  walks the `upnext` items in order, then source successors from `pos`.
- **Shuffle and `Repeat::All` apply to the source tier only.** Up-Next items are
  always one-shot FIFO — never shuffled, never repeated.
- `StopAfter::Track` (≈ MPD `single oneshot`) and `StopAfter::Queue` (≈ foobar
  "Stop After Queue") stop playback at the next track/queue boundary, then revert.

**New-context policy (the one genuine choice — no industry consensus):**
`set_source(ctx, start)` swaps context + `pos` + `seed` but **keeps `upnext`**; a
separate `clear_upnext()` clears it. This matches Apple (preserve + explicit clear)
and current Spotify (preserve manual, replace context) — the two best-regarded
queues. (YouTube/Amazon discard on new context; we deliberately don't.)

**Previous:** `prev()` pops the history stack and restores `(handle, pos)` exactly,
so shuffled and queued jumps are reversible — *not* index−1. The **~3 s
"restart current vs. go back" gate is a control-layer policy** (it needs live deck
position), so it lives in the driver/app with a **configurable threshold
(default 3000 ms)**; `queue.prev()` stays the pure "previous track."

## 7. Shuffle

Anti-clustering "smart shuffle" (Spotify/Fiedler/Ruuda) requires artist/album
metadata + materialization, which the queue can't have. Clean split:

- **The queue owns a keyed positional permutation** — a small invertible Feistel
  network over `[0, N)` seeded by `u64` (cycle-walking to a power-of-two domain):
  - `play→natural = feistel(seed, pos)`, `natural→play = feistel⁻¹(seed, nat)`.
  - **Toggle ON** anchors the current track for free via the inverse:
    `pos' = feistel⁻¹(seed, current_natural)` — current keeps playing, only the
    upcoming order changes. **OFF** resumes linear order from the current position.
  - **O(1) memory at any N**, and because it's a bijection a full cycle visits
    every track **exactly once with zero played-set bookkeeping** ("no repeats
    until the library is exhausted" is free — the property materialized players
    struggle with at scale).
- **Smart artist-spread shuffle is an app-layer choice.** The crate ships a
  `balanced_shuffle(n, group_of) -> order` utility (Ruuda's optimal interleave)
  the *app* calls for a **bounded** context (album/artist/playlist that fits),
  feeding the result as that context's natural order. Clustering matters most
  where it's cheap to fix (small, few-artist lists); at library scale
  keyed-permutation's rare clustering is unaffordable to fix anyway.

## 8. Storage, `Context`, and errors

**One codebase, two backings.** The only growable state (`upnext`, `history`) sits
behind an internal `QueueBuf<T>` trait, selected by feature:

```rust
trait QueueBuf<T> {                       // impl'd for heapless::Deque<T,N> and (alloc) VecDeque<T>
    fn push_back(&mut self, v: T) -> Result<(), Full>;   // enqueue    (Full only when bounded)
    fn push_front(&mut self, v: T) -> Result<(), Full>;  // play_next
    fn pop_front(&mut self) -> Option<T>;                // consume next
    fn push_back_evicting(&mut self, v: T);              // history: drop OLDEST when full (ring)
    fn pop_back(&mut self) -> Option<T>;                 // prev
    fn get(&self, i: usize) -> Option<&T>;
    fn len(&self) -> usize;
    fn capacity(&self) -> Option<usize>;                 // None = unbounded (alloc)
    fn clear(&mut self);
}
```

Ready aliases so users never touch the generics:

- `HeaplessQueue<C, const UP, const HIST>` — `no_std`, DAP. `play_next`/`enqueue`
  return **`Err(Full)`** at capacity; `history` silently evicts oldest.
- `AllocQueue<C>` *(feature `alloc`)* — desktop; `capacity() == None`, adds never
  fail.

**The `Context` contract (the app's obligation):**

```rust
pub trait Context {
    type Handle: Copy + Eq;
    fn len(&self) -> usize;                                     // N (finite; 0 = empty)
    fn handle_at(&self, natural_index: usize) -> Option<Self::Handle>;
}
```

- **Stable snapshot:** same index → same handle for the context's life, `len()`
  constant. Library edits require the app to re-seat via `set_source(...)` (the
  queue caches `pos` against `len`; silent mutation would desync).
- **Cheap + non-blocking:** `handle_at` is called from `upcoming()` on the pump
  path — an in-RAM index or a cached window; the slow open still happens in the
  loader via `provide`. Endless/radio contexts (unknown `len`) are **v2**.

## 9. Persistence

The queue stays `no_std` and does not mandate serde. It exposes a plain-data
snapshot (the `Context` is app-owned, stored as an opaque `context_id`):

```rust
struct Snapshot<H> {                 // #[derive(Serialize, Deserialize)] under feature `serde`
    context_id: u64, pos: usize, seed: Option<u64>,
    repeat: Repeat, stop_after: StopAfter,
    upnext: /* handles */, history: /* (handle, pos) */,
}
queue.snapshot() -> Snapshot<H>
TwoLevelQueue::restore(ctx, snapshot) -> Result<Self, RestoreError>   // validates pos < len, caps fit
```

Because we store a **seed, not a permutation array**, the whole thing is a few
dozen bytes — trivially `postcard`-serializable on the DAP. Full *resume* = the
queue `Snapshot` + the Deck's intra-track position + volume (the **engine** exposes
the combined snapshot). Save on meaningful events; batch flash writes on the DAP;
**pause must not mutate state** (a documented pitfall in other players).

## 10. Public API surface

**`soul-audio-engine`** (revised):

- `trait Sequencer` + `enum Advance { Natural, Skip }` (§4).
- `struct LinearSequencer` — today's `len`/`cursor`/`RepeatMode`, for DAP folder
  playback.
- `struct Engine<Sink, Decoder, Seq: Sequencer, const PRELOAD>` — owns deck +
  sequencer; `poll_open`/`provide`/`pump`, `play`/`pause`/`stop`/`seek`/
  `set_volume`, `next`/`prev`/`goto` (delegate + reconcile), `sequencer()` /
  `sequencer_mut()`, `state()` / `position()`. Reconcile-to-sequencer lives here.

**`soul-audio-queue`** (new; deps: `soul-audio-engine`, `heapless`; features
`alloc`, `serde`):

- `trait Context`; `struct TwoLevelQueue<…>` (impl `Sequencer`); aliases
  `HeaplessQueue` / `AllocQueue`.
- source: `set_source(ctx, start)`, `context()`, `source_len()`, `pos()`.
- transport: `set_repeat` / `repeat`, `set_stop_after`.
- shuffle: `set_shuffle(bool, seed)`, `is_shuffled`, `reshuffle(seed)`.
- up-next: `play_next` / `enqueue` (→ `Result<(), Full>`), `clear_upnext`,
  `upnext_len` / `upnext_iter`; `remove_upnext` / `move_upnext` = **v2**.
- persistence: `snapshot` / `restore`.
- `enum Repeat { Off, All, One }`; `enum StopAfter { None, Track, Queue }`.
- utils: `balanced_shuffle(n, group_of)` (Ruuda); `shuffle_index(seed, i, n)` +
  inverse (Feistel; `pub` for advanced use/tests).

The surface maps 1:1 onto `MPRIS` / Android `MediaSession` (`LoopStatus`
None/Track/Playlist, `Shuffle` bool, Next/Previous) so a desktop app wraps it with
no impedance mismatch.

## 11. Gapless / control-layer seams

- **Look-ahead = `upcoming(k)`** — the queue answering "what is N+1 before N ends"
  is exactly what the Deck needs to pre-open (mpv's source-side `prefetch-playlist`
  split from sink-side `gapless-audio`, which our `Deck`/`Sink` already mirror).
- **Encoder delay/padding trimming** (LAME `enc_delay`/`enc_padding`, `iTunSMPB`,
  CD 588-sample multiples) is a **decoder/Deck** concern — the queue stays
  codec-agnostic.
- **The 3 s previous-gate** and any "restart vs. previous" UX lives in the
  driver/app (it needs live deck position); the queue provides pure `prev`.

## 12. Testing

- **Feistel = bijection** (property tests): over many `(N, seed)` incl.
  `N ∈ {0, 1, 2, prime, 2ᵏ, large}`, the map on `[0, N)` is a permutation (covers
  all, no dupes) and `inv ∘ fwd == id`; cycle-walking terminates.
- **Queue semantics** (property + unit; `extern crate alloc` in tests): up-next
  wins; `play_next` front / `enqueue` back; `Repeat::One` freezes on `Natural`,
  advances on `Skip`; `All` wraps; `Off` stops; **`prev` exact-restore
  round-trips**; history bounded eviction; shuffle toggle keeps current invariant +
  full-cycle-covers-all-once; `set_source` keeps `upnext`; **snapshot → restore
  round-trip**; bounded `Full` errors.
- **Engine reconcile** (extend the existing engine tests with a mock `Sequencer`):
  the **"Play Next discards a stale preload and primes the new successor"** case
  (the correctness crux); gapless slot-reuse when `upcoming(1)` matches; goto/prev
  via `sequencer_mut` reconciled; stale-provide discard (already covered).
- **`LinearSequencer` parity**: port today's engine tests to prove no behaviour
  regression.

## 13. Scope

**In (v1):** the `Sequencer` seam + engine reconcile refactor; `LinearSequencer`;
`soul-audio-queue` with virtual `Context`, Up-Next (`play_next`/`enqueue`),
keyed-permutation shuffle, history-based `prev`, `Repeat`/`StopAfter`,
`snapshot`/`restore`, the `balanced_shuffle` utility, bounded + `alloc` backings.

**v2:** queue reorder/remove (`remove_upnext`/`move_upnext`, drag-to-edit UI);
endless/radio contexts (unknown length, auto-extend); MPD-style numeric priority
band; windowed metadata-aware shuffle at library scale.

**Out (app/platform):** the real track list + tags/metadata; the persistence store
itself; smart-shuffle grouping input; the concrete `Sink`; the 3 s previous-gate
threshold; encoder delay/padding trimming (decoder/Deck).

## 14. Research findings & sources

Condensed; each backed by the cited primary/authoritative source.

**Two-level model is universal.** Every serious player separates auto-continuation
/ base play-order from a manual override queue that plays first and falls back:
Spotify "Next in Queue" vs "Next from"; Apple "Playing Next" + AutoPlay; foobar2000
Playback Queue; Winamp JTFE; Clementine/Strawberry Queue Manager; MPD `prio` +
`addid +0`; Roon "Add Next". Refs: Apple `support.apple.com/en-us/109336` and
`/guide/music/musb1e6d1c76`; foobar2000 `foobar2000.org/components` (Queue Editor,
Stop After Queue, Keep Queue) + Hydrogenaudio wiki; MPD
`mpd.readthedocs.io/en/latest/protocol.html`; mpv `DOCS/man/options.rst`; Roon
`help.roonlabs.com`.

**Play Next = front, Add to Queue = back (FIFO); repeated Play Next = LIFO.**
Apple documents "Play Next" (top) vs "Play Last" (bottom); YouTube/Amazon match.
Spotify exposes only "Add to queue" (back of the manual sub-queue).

**Repeat-one loops the current track and does *not* consume the queue; repeat-all
loops the context, not the manual queue** (Spotify explicit; all four agree).
MPD's orthogonal `repeat` + `single` (+ `oneshot`, `consume`) is the most complete
formal model and maps to our `Repeat`/`StopAfter`.

**Previous ≈ 3 s restart-vs-previous gate** (Apple/iPod origin; Spotify same;
threshold not standardized — make it a caller knob). "Previous" should walk a real
history stack (Spotify) so shuffled/queued jumps are reversible, not index−1
(MPD/foobar step through the current order only).

**Shuffle must "feel random," not be uniform.** Naive Fisher–Yates clusters
same-artist tracks; Spotify (2014) used dithering/even-spacing (after Fiedler's
*balanced shuffle*, `keyj.emphy.de/balanced-shuffle`); Ruuda
(`ruuda.nl/2023/an-algorithm-for-shuffling-playlists`) gives a provably-optimal
interleave. Spotify's current (Nov 2025) approach scores N random candidates by
recency-freshness. Anti-clustering needs item metadata → app-layer here; the queue
uses a seeded deterministic permutation (the research's own `no_std`
recommendation: regenerate order from a stored seed, not a heap array).

**Toggle keeps the current item invariant; only upcoming order changes; off
resumes linear** (foobar2000, MPD `random`, Spotify Nov-2025 "select doesn't
reshuffle").

**Gapless = source-side look-ahead (open N+1 before N ends) + sink-side continuity
(keep the device open, reopen on format change)** — mpv `--prefetch-playlist` /
`--gapless-audio=weak`. Encoder delay/padding via LAME `enc_delay`/`enc_padding` +
`iTunSMPB`; Hydrogenaudio `Gapless_playback`.

**Persist queue + index + intra-track position + mode flags + shuffle seed;
save on meaningful events; pause must not mutate state.** Persistence is a policy
flag (foobar ephemeral-by-default vs opt-in "Keep Queue"); on the DAP, batch flash
writes. Map controls onto MPRIS / Android `MediaSession`.

## 15. Risks / notes

- **Feistel correctness is load-bearing** (a non-bijection silently drops/repeats
  tracks) → property-tested exhaustively for small N and sampled for large N.
- **`handle_at` cost** — if the app's context index lives on flash, an uncached
  `upcoming()` could hit storage on the pump path; the app must keep at least a
  small forward window cached. Documented in the `Context` contract.
- **Reconcile churn** — clearing/re-priming all preloads on any `upcoming`
  mismatch is simple and correct but re-opens; acceptable because mutations are
  user-driven and rare, and `PRELOAD = 1` on the DAP makes it a single slot.
- **Smart shuffle at true library scale** (windowed metadata-aware) is deferred to
  v2; v1 ships keyed-permutation (scalable) + `balanced_shuffle` (bounded).
