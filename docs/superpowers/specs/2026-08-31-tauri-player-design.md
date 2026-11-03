# soul-audio-player — Tauri Desktop Player Design

**Date:** 2026-08-31
**Status:** Proposed (design review)
**Builds on:** the shipped `soul-audio-core` (decode/resample/`Deck`/`Sink`),
`soul-audio-engine` (`Engine` + `Sequencer` reconcile, PR #15), and
`soul-audio-queue` (`TwoLevelQueue`, PR #16). This is the first *consumer app* on
top of that library.

## 1. Goal

A **simple, Spotify-style desktop music player** (Tauri v2 + Svelte 5) that plays
**local files** through the shipped engine + queue. MVP feature set: open a music
folder → browse a track list → gapless playback with a Spotify-style now-playing
footer → shuffle / repeat → an **Up-Next queue** (Play Next / Add to Queue). No
accounts, no streaming, no saved playlists — those are out of scope.

"Spotify-style" refers to the **chrome/layout**, not feature breadth: a rich shell
(left queue column flowing into the now-playing footer, a page-router main area),
with minimal *content* (one Library page) for now.

## 2. Architecture

```
 Svelte 5 UI (webview)  ──Tauri commands──►  Tauri Rust backend  ──mpsc Cmd──►  pump thread
        ▲                ◄──Tauri events──        (soul-audio-player)             owns Engine, runs pump loop
        │  reactive state (now-playing, live position, Up-Next)                        │  ┌ loader thread (open decoders, slow I/O)
        └───────────────────────────────────────────────────────────────────────────┘  └ CpalSink ──► cpal device

 Engine<CpalSink, OwnedDecoder, TwoLevelQueue<LibraryContext>, PRELOAD=2>              ← soul-audio-{core,engine,queue}
 soul-audio-desktop (NEW shared lib): CpalSink · OwnedDecoder+open_track · file sources · convert   ← shared by CLI + app
```

Deliverables (the app + a shared std lib, plus the §2.2 `no_std` additions that
grow the portable library):

1. **`crates/soul-audio-control`** — NEW `no_std` crate: the portable `Player`
   controller (§2.2).
2. **`crates/soul-audio-desktop`** — new shared *std* library crate; the reusable
   desktop-playback bits extracted from the CLI.
3. Additions to shipped crates: **`SliceContext`** in `soul-audio-queue`, the
   **sample-format convert** module in `soul-audio-core` (§2.2).
4. **`apps/player/src-tauri`** (`soul-audio-player`) — the Tauri backend: thin
   plumbing around `soul-audio-control::Player`.
5. **`apps/player/`** — the Svelte 5 frontend.

### 2.1 Backend concurrency — command-channel actor (chosen)

One **pump thread** *owns* the `Engine` and runs the block loop; a **loader
thread** opens decoders off the hot path; the UI talks to the pump thread over an
`mpsc` command channel and receives state via Tauri events. This matches the
engine's sync / single-owner / off-real-time design — **no locking on the pump
path**. (Rejected: `Arc<Mutex<Engine>>` shared with the commands — a mutex over a
big decoder-bearing struct on the hot path risks audio stutter.)

Both threads are spawned with a **16 MiB stack** (the `AnyDecoder` is large by
value, exactly as the CLI does).

### 2.2 Portable logic pulled into `no_std` (not the app)

To keep the app thin and grow the reusable library (the DAP reuses all of it), four
pieces that would otherwise live in the app go into portable crates:

- **`SliceContext<H>` → `soul-audio-queue`** — a generic `no_std` `Context` backed
  by an ordered handle list (`heapless` bounded on the DAP, `alloc` `Vec` on
  desktop). The app's "context = an ordered list of track ids" becomes a one-liner;
  no bespoke `LibraryContext` logic.
- **`soul-audio-control` (NEW `no_std` crate)** — a portable `Player` controller
  that owns `Engine<K, D, TwoLevelQueue<C, Up, Hist>, PRELOAD>` and applies
  high-level **`Command`** intents (`PlayContext`, `TogglePlay`, `Pause`, `Stop`,
  `Next`, `Prev`, `Seek`, `SetVolume`, `PlayNext`, `Enqueue`, `ClearQueue`,
  `SetShuffle`, `SetRepeat`, `Goto`) plus state getters (now-playing handle,
  position/duration frames, playing state, shuffle, repeat, volume). It holds the
  small **driver-layer policy** the queue design deferred — notably the **`prev`
  3-second rule** (`if deck.position_frames() > prev_restart_frames { seek(0) } else
  { prev() }`), configurable. `soul-audio-engine` stays a pure mechanism; the policy
  lives here, in one place, shared by the desktop backend and the future DAP.
- **Sample-format conversion → `soul-audio-core`** — the pure canonical-24-bit →
  device-format math (`convert.rs`) becomes a `no_std` core module so both the cpal
  `Sink` and a future SAI/I²S DMA sink share it.

All four are headless-testable (no audio device), reusing the pattern from
`soul-audio-queue`'s engine-integration test. Deferred: nothing further — a fuller
state machine beyond `Player` waits until a real second consumer needs it.

## 3. `crates/soul-audio-desktop` (shared lib)

Extract from `soul-audio-cli` into a normal workspace member (covered by
`cargo xtask verify`):

- `CpalSink` (the cpal output `Sink` + its lock-free ring), from `cpal_sink.rs`.
- `OwnedDecoder` + `open_track` (the owned, `Send` decoder over file sources;
  PCM/DSD variants), from `driver.rs`.
- The file-source wiring (`file_source`, the `Src` type), from `sources.rs`.

The pure **sample-format conversion** from `convert.rs` does NOT stay here — its
math moves to `soul-audio-core` (§2.2) and the `CpalSink` calls it, so a future DMA
sink shares the same conversion.

`soul-audio-cli` then depends on `soul-audio-desktop` and keeps only its own
fixed-queue `driver.rs` (`run`/`play`/`render`). This is a mechanical migration;
the CLI's behavior and tests are unchanged. Deps carried over: `cpal` + the
ring-buffer crate the sink already uses.

## 4. Backend player service (`soul-audio-player`)

### 4.1 Library + `LibraryContext`
- **`open_folder(path)`** scans recursively for supported files
  (wav/flac/mp3/m4a/aac/ogg/dsf/dff), reads tags with **`lofty`** →
  `Track { id: u32, path, title, artist, album, duration_secs }`, and builds
  `Arc<Library { tracks: Vec<Track> }>`. `id` = index (stable for the session).
  Runs off the UI thread (async command / `spawn_blocking`) and returns the list.
- The playback context is just **`soul_audio_queue::SliceContext<u32>`** (§2.2)
  over the view's ordered track ids — no bespoke `LibraryContext` type. It carries
  **no paths**; the loader maps `id → path` via `Arc<Library>`.
- Playing "this view from track N" = build a `SliceContext` from the view's ids and
  issue `Command::PlayContext { ctx, start: N }` to the `Player` (§2.2), which does
  `set_source(ctx, N)` + `rearm()` + `play()`.

### 4.2 The two threads
- **pump thread** owns a `soul_audio_control::Player<CpalSink, OwnedDecoder,
  SliceContext<u32>, /*Up*/ VecDeque, /*Hist*/ VecDeque, 2>` (desktop = unbounded
  `AllocQueue` backings; `PRELOAD = 2`). Loop: (1) drain the command channel
  (non-blocking) → `player.apply(cmd)`; (2) service opens — for each
  `player.poll_open(id)`, request the loader open it, install decoders via
  `player.provide`; (3) `player.pump()` one block (paced by the sink's
  backpressure); (4) emit throttled state from the `Player`'s getters. The pump
  thread holds **no playback logic** — it's plumbing around `Player`.
- **loader thread** receives ids, resolves `Arc<Library>[id].path`, builds
  `OwnedDecoder` via `open_track` (slow I/O), sends it back — same handoff as the
  CLI's loader.

### 4.3 Command API (frontend → backend)
`#[tauri::command]` handlers send a `Cmd` enum over the channel (fire-and-forget)
except `open_folder`, which returns data:
```
open_folder(path) -> Vec<TrackView>          // scan + tags (async), returns the library
play_context(ids: Vec<u32>, start: usize)    // set_source(ids, start) + rearm + play
toggle_play() · pause() · stop()
next() · prev()                              // prev applies the 3-second rule (§4.5)
seek(secs: f64) · set_volume(v: f32)         // 0..1
play_next(id) · enqueue(id) · clear_queue()
set_shuffle(on: bool) · set_repeat(mode: "off"|"all"|"one")
goto(natural_index: usize)                   // jump within the context
get_state() -> PlayerState                   // initial pull on load
cover_art(id: u32) -> Option<CoverArt>       // lazy per-track embedded cover (data URL / bytes + mime), None if absent
```
`TrackView` is `{ id, title, artist, album, duration_secs }` — **no art bytes**
(bundling covers for a whole library is too heavy). Art is fetched lazily via
`cover_art` (§4.5).

### 4.4 Event API (backend → frontend)
- **`player:tick`** ~8×/sec **and** immediately on any transport/queue change:
  `{ now_playing_id: Option<u32>, position_secs, duration_secs,
     state: "playing"|"paused"|"stopped", shuffle, repeat, volume }`.
- **`player:queue`** on queue mutation: `{ up_next_ids: Vec<u32>, context_pos,
  context_len }`.
Events carry **ids**, not full track data — the frontend already has the library
from `open_folder` and resolves **names** locally; the now-playing **cover** is
fetched lazily via `cover_art(id)` when the track changes (§4.5). Position/duration
come from the deck (`position_frames`/`duration_frames` ÷ `sample_rate`).

### 4.5 Driver-layer policies (the bits the queue deliberately leaves to the app)
- **`prev` 3-second rule** lives in `soul-audio-control::Player` (§2.2), not the
  app: if `deck.position_frames() > prev_restart_frames`, `Prev` restarts the
  current track (`seek(0)`); else `queue.prev()`. The threshold is configurable.
- **`PRELOAD = 2`** for robust desktop gapless.
- **State throttling**: `player:tick` at ~8 Hz for the progress bar, but emitted
  *immediately* on play/pause/track-change/seek.
- **Now-playing art**: the frontend calls `cover_art(id)` when the now-playing
  track changes; the backend reads the embedded cover with `lofty` and returns it
  (bytes + mime, or a `data:` URL), or `None` → the UI shows a placeholder. Fetched
  **lazily per track**, never bundled into the library payload. A full art grid is
  out of scope.
- **`StopAfter`** is not exposed in the MVP.

## 5. Type safety + code organization

### 5.1 End-to-end types via `tauri-specta`
Annotate commands/DTOs with `specta::Type`; `tauri-specta` emits a typed
`bindings.ts` the frontend imports — every `invoke`/event is fully typed and
can't drift from the Rust contract. (Fallback: a hand-written `shared/types.ts`
mirroring the DTOs — simpler, drift-prone; `tauri-specta` preferred.)

### 5.2 Vertical slices (feature-first, both sides)
Three MVP slices — **library**, **playback**, **queue** — each owning its UI +
state + typed backend boundary. No horizontal `components/`/`services/` split.

**Frontend (`apps/player/src/`):**
```
app/        App.svelte (grid shell + client-side router), theme tokens, global.css
shared/     ipc.ts (typed invoke/listen helpers over the tauri-specta bindings) · ui/ (Button, Slider, Icon)
features/
  library/  LibraryView.svelte · library.svelte.ts (runes store) · library.ipc.ts
  playback/ NowPlayingBar.svelte · playback.svelte.ts · playback.ipc.ts
  queue/    UpNextPanel.svelte · queue.svelte.ts · queue.ipc.ts
```
State is Svelte 5 **runes** (`$state`/`$derived`) in `.svelte.ts` modules; each
`*.ipc.ts` is the slice's only door to the backend. Cross-slice sharing only via
`shared/`.

**Backend (`apps/player/src-tauri/src/`):**
```
player/    the AudioPlayer service: owns Engine + pump/loader threads + the Cmd enum + state emit
library/   scan + lofty + the open_folder command
playback/  transport commands + the prev-3s policy
queue/     queue-mutation commands
ipc/       command registration + shared DTO/event types (specta::Type)
main.rs    Tauri builder wiring the slices
```
A slice is "done" only when its UI, its runes store, its `.ipc.ts`, and its
backend handler all exist and are typed — the vertical-slice completion bar.

### 5.3 Strict TypeScript + tooling
- **`tsconfig`**: `strict: true` + `noUncheckedIndexedAccess`,
  `exactOptionalPropertyTypes`, `noImplicitOverride`, `noUnusedLocals`,
  `noUnusedParameters`, `verbatimModuleSyntax`, `isolatedModules`.
- **Prettier** (+ `prettier-plugin-svelte`), **ESLint** flat config
  (`typescript-eslint` strict-type-checked + `eslint-plugin-svelte`),
  **`svelte-check`**.
- Scripts: `pnpm check` (svelte-check + `tsc --noEmit`), `pnpm lint`,
  `pnpm format`, `pnpm test` (Vitest for store logic), `pnpm verify` = check +
  lint + format:check. Rust side: `cargo fmt` + `clippy -D warnings` on the
  `src-tauri` crate.

## 6. UI — Spotify-style shell

```
┌──────────────────────────────────────────────────────────────────────────────┐
│ [◀ ▶]   top bar (minimal now: nav; search later)                               │
├────────────────┬───────────────────────────────────────────────────────────────┤
│  QUEUE         │  MAIN — page-browsing surface (client-side router)             │
│  (Up Next)     │   ▸ Library page (only page for now): the track list           │
│  ▪ Queued A    │      #  Title   Artist   Album   ⏱   ⋯                          │
│  [ Clear ]     │      …                                                          │
│  (flows ↓)     │                                                                │
├────────────────┼──────────────────────────────────────────┬────────────────────┤
│ [art] Title    │  🔀 ◀◀ ⏵/⏸ ▶▶ 🔁   ●━━━━ 1:23/3:45         │  🔊 ━●━   ≣ queue   │  ← Spotify-style footer
│       Artist   │        (transport + progress, centered)    │  (volume + toggles)│
└────────────────┴──────────────────────────────────────────┴────────────────────┘
  └─ left column: QUEUE (top) flows down into NOW-PLAYING (bottom-left of footer)
```

- **Left column = the `queue` slice** (Up-Next panel), **visually continuing down
  into the Now-Playing widget** at the footer's bottom-left — one connected left
  column, Spotify-style.
- **Footer = full-width Spotify 3-region bar** (`playback` slice): left =
  now-playing (art + title/artist, aligned under the sidebar) · center = transport
  (shuffle · prev · play/pause · next · repeat) + progress · right = volume +
  queue-toggle.
- **Main = page-browsing surface** owned by the `app/` shell + a small
  client-side router; only the **Library page** ships now (structure supports
  future Home/Album/Search).
- **Interactions:** double-click a track → `play_context(view ids, index)`; row
  `⋯`/right-click → `play_next`/`enqueue`; toggles → `set_shuffle`/`set_repeat`;
  seek drag → `seek`; volume → `set_volume`; ◀◀ → `prev` (3-s rule); ▶▶ → `next`.
  `player:tick` drives the seek bar + now-playing + toggles; `player:queue` drives
  the Up-Next panel + the "now playing" row highlight; the frontend resolves
  ids→names/art from its library copy.

## 7. Workspace / build layout

- `crates/soul-audio-desktop` — a **root-workspace member** (pure Rust; part of
  `cargo xtask verify`).
- `apps/player/` — the Tauri app as a **separate cargo workspace**
  (`apps/player/src-tauri` has its own `[workspace]`), path-depending into the core
  crates. The root `Cargo.toml` gets `exclude = ["apps/player/src-tauri"]`.
  Rationale: a Tauri crate's `build.rs` expects the built frontend (`dist/`), so a
  root membership would break the pure-Rust `cargo test`/`clippy` gate. The app
  builds via `pnpm tauri dev` / `pnpm tauri build`.
- Toolchain present: Node 22, pnpm, Tauri CLI v2 (2.9.6), WebView2 (Windows).

## 8. Verification

- **`soul-audio-desktop`** — the extraction must keep `cargo xtask verify` green
  (CLI still builds/tests; the crate builds host + is std-only, so no thumbv7em
  gate). CLI e2e (`render`/`play`) unchanged.
- **Backend** — unit-test the pure logic off the audio device: the `Library` scan
  (against a small fixture folder), `LibraryContext` (`Context` impl), the `Cmd`
  application on a headless player service with a **mock/null sink** (assert
  transport/queue/shuffle/repeat state transitions and the prev-3s rule) — reusing
  the pattern from `soul-audio-queue`'s engine-integration test.
- **Frontend** — Vitest on the runes stores (state reducers over mocked ipc);
  `svelte-check` + `tsc` + eslint clean; Prettier check.
- **Manual** — `pnpm tauri dev`: open a folder, play/pause/seek, gapless across
  tracks, Play Next re-primes gaplessly, shuffle/repeat, Up-Next add/clear.

## 9. Scope

**In (MVP):** the `no_std` additions (§2.2 — `SliceContext` in `soul-audio-queue`,
the `soul-audio-control` `Player` controller incl. prev-3s, the sample-format
`convert` module in `soul-audio-core`); the `soul-audio-desktop` shared lib + CLI
migration; the Tauri backend (library scan + tags, the actor plumbing around
`Player`, the command/event API); the Svelte shell (Spotify footer, left
queue→now-playing column, router-main with the Library page); shuffle/repeat/seek/
volume; Up-Next (Play Next / Add to Queue / clear); now-playing art thumbnail;
strict TS + `tauri-specta` + lint/format.

**Out (later):** streaming/accounts; saved/persisted playlists; library search
beyond a simple filter; additional pages (Home/Album/Search) beyond the router
scaffold; drag-reorder of the Up-Next queue; a full album-art grid; `StopAfter`
UI; cross-fade; gapless *tag* trimming (encoder delay/padding) beyond what the
`Deck` already does; the STM32 side (unrelated).

## 10. Risks / notes

- **Tauri-in-a-Rust-workspace friction** — mitigated by the separate-workspace +
  `exclude` layout (§7); the app is built by the Tauri CLI, not `cargo xtask`.
- **`open_folder` on a huge library** — the scan + per-file tag read runs off the
  UI thread and returns once; very large folders may want incremental/streamed
  results (a v2 refinement). MVP loads then returns.
- **Decoder RAM on the pump thread** — `AnyDecoder`/`OwnedDecoder` is large by
  value and the deck holds `PRELOAD+1` of them; the 16 MiB pump/loader stacks and
  the loader→pump *move* handoff (as in the CLI) keep this off the RT path.
- **`tauri-specta` version pinning** — must match the installed Tauri v2 line;
  if it fights the toolchain, fall back to the hand-written `shared/types.ts`
  (§5.1) without changing the architecture.
