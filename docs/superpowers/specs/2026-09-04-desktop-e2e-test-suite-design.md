# Desktop Player E2E Test Suite — Design

**Status:** Approved design (2026-09-04). Next step: implementation plan.

**Goal:** Give the `soul-player` desktop app (Tauri 2 + Svelte 5 over the
soul-audio Rust engine) an end-to-end test suite that guards the behaviours
fixed this cycle — folder-switch correctness, per-format durations, seek,
volume, queue, and cover art — across three layers: headless Rust product
tests, frontend logic/component tests, and a WebDriver UI smoke pass.

**Why now:** The app crate (`app/src-tauri`) is an *isolated Cargo workspace*
that root `cargo test`, `cargo xtask verify`, and CI never build or test. The
frontend has zero test tooling. Every desktop bug this cycle (stale tracks
after re-opening a folder, wrong DSD/MP3/Opus durations, seek/volume lag) lived
in exactly this untested layer.

---

## Global Constraints

- **No AI trailers in commits.** lefthook rejects `Co-Authored-By: Claude` /
  `Claude-Session` lines. Commits are GPG-signed.
- **`app/src-tauri` is a separate workspace.** Anything added there needs its
  own `cargo test` invocation; it is not reached by root `cargo test` / `xtask
  verify` / the current CI jobs.
- **Device-free.** Every automated test runs without an audio device:
  `SOUL_PLAYER_OFFLINE=1` swaps the real `CpalSink` for `NullSink`
  (`app/src-tauri/src/offline.rs`), which discards output and applies no
  backpressure (playback runs faster than real time).
- **Condition-based waiting, never fixed sleeps.** The actor emits `Tick`
  ~8×/s and on change; tests poll captured events until the asserted state
  appears, with a bounded timeout.
- **Reuse committed fixtures.** `testdata/` already holds a 2 s A4-left/E5-right
  tone in `wav/flac/mp3/ogg/m4a/aac/dsf` plus `tagged.flac`; extend the
  generators rather than inventing new signals.
- **Windows-first.** Primary target is Edge WebView2 on Windows 11.

## Resolved decisions

- **Tier 2 frontend env:** Vitest **browser mode** (`vitest-browser-svelte` +
  Playwright/Chromium), not jsdom — Svelte 5 runes (`$state`/`$effect`, and the
  external `art-cache.svelte.ts` universal state) only behave faithfully in a
  real browser; browser mode avoids `flushSync()` gymnastics.
- **Tier 3 WebDriver runs locally**, via `npm run e2e`, analogous to the
  existing by-ear `cargo xtask qa-audio`. A `windows-latest` workflow file is
  shipped but left disabled/optional until a Windows CI runner with WebView2
  exists (the remote is self-hosted Forgejo at `git.datadir.dev`). Tiers 1 & 2
  run in CI now.

---

## Architecture

Three tiers over one shared foundation, mapped to the Tauri testing pyramid
(Rust command tests → frontend IPC-mock tests → WebDriver smoke).

```
Foundation:  split open_folder (pick vs load) · e2e feature · fixtures
   │
   ├─ Tier 1  Rust product-e2e   app/src-tauri/tests/e2e.rs   (mock runtime + NullSink)   → CI
   ├─ Tier 2  Frontend logic     app/src/**/*.svelte.test.ts  (Vitest browser + mockIPC)  → CI
   └─ Tier 3  WebDriver smoke     app/e2e/**                   (WDIO + tauri-driver)        → local
```

### Foundation (shared)

**F1. Split the folder-open command.** `open_folder` today couples the native
`blocking_pick_folder()` dialog with the scan→load→play logic
(`app/src-tauri/src/commands.rs:17`). WebDriver cannot operate an OS-native
dialog, and the Rust mock runtime does not execute native dialog libs. Refactor
per Tauri's recommended "split pick vs load" pattern:

- Extract the pure half into
  `load_folder(state: &PlayerState, app: &AppHandle, dir: &Path) -> Result<Vec<TrackView>, String>`
  containing the existing `Library::scan` → `set_library` →
  `Command::PlayContext { ids, start: 0 }` → `views` logic.
- `open_folder` = native pick, then `load_folder(&state, &app, &dir)`. Behaviour
  unchanged for real users.
- Add a **test-only** command `load_folder_path(state, app, dir: String)` gated
  behind a new `e2e` Cargo feature that calls `load_folder`. Tier 3 injects a
  fixture directory via `invoke('load_folder_path', { dir })`; the `e2e` feature
  is off in release builds.

**F2. `e2e` Cargo feature** in `app/src-tauri/Cargo.toml` — registers
`load_folder_path` in the invoke handler and specta bindings only when enabled.
Tier-3 build: `tauri build --debug --no-bundle --features e2e`.

**F3. Test dependency:** `tauri = { version = "2", features = ["test"] }` as a
`[dev-dependencies]` entry (the runtime dep keeps `features = []`) so Tier 1 can
use `tauri::test::mock_builder` / `get_ipc_response`.

**F4. Fixtures** (`app/src-tauri/tests/fixtures/` + `testdata/` extensions):

- **Per-format library folder** referencing the existing `testdata/` tones
  (`wav/flac/mp3/ogg/m4a/aac/dsf`) **plus a new `testdata/tone.opus`** — there
  is currently *no* `.opus` fixture anywhere (`tone.ogg` is FLAC-in-Ogg), so the
  Opus-duration path (last-Ogg-page granule) has no coverage. Extend
  `testdata/generate.sh` to encode `tone.opus` (2 s, same tone) with ffmpeg.
- **Two distinct folders `libA`/`libB` with different track durations**,
  written as synthetic WAVs at test time (the `engine_e2e` tests already build
  WAVs in-memory) so the folder-switch test can prove — via
  `Tick.duration_secs` — that the *correct* file loads after a re-open. (All
  `testdata/` tones are 2 s, so they cannot distinguish libraries.)
- **Embedded-art coverage:** verify `tagged.flac` carries an embedded picture;
  if not, add a fixture with one (extend the generator to embed a tiny PNG).
  Also keep one artless fixture. Used by the `track_art` cases.

### Tier 1 — Rust product-e2e (backbone, in CI)

**File:** `app/src-tauri/tests/e2e.rs`. Build a mock app with
`mock_builder().manage(PlayerState::new())`, register the real command handlers,
set `SOUL_PLAYER_OFFLINE=1` so the lazily-built actor uses `NullSink`. Invoke
commands (via `get_ipc_response` or by calling the command fns with the mock
`State`/`AppHandle`) and capture `"player"` events with `app.listen`. Poll
captured Ticks until the asserted condition holds (bounded timeout helper).

Cases (each maps to a shipped fix):

1. **load folder** → returned `TrackView`s match the fixture (count + titles);
   `library_views` returns the same; auto-`PlayContext` yields a first Tick with
   `now_playing_id == Some(first id)`, `state == Playing`.
2. **folder-switch** → `load_folder(libA)`, then `load_folder(libB)`; the
   now-playing track resolves to *libB*'s file, asserted through its distinct
   `duration_secs`. *(Guards the stale-library-id loader bug.)*
3. **per-format durations** → a folder of all formats; every
   `TrackView.duration_secs ≈ 2.0` (± tolerance) for
   `wav/flac/mp3/opus/m4a/aac/dsf`, and `Tick.duration_secs` matches when each
   plays. *(Guards MP3 Xing/Info/VBRI, Opus granule, DSD sample-count.)*
4. **seek** → play, `Seek { secs: t }`, a later Tick has `position_secs ≥ t`
   and `< duration`.
5. **volume** → `SetVolume(x)` → a later Tick has `volume == x`; `SetVolume(2.0)`
   clamps to `1.0`, `SetVolume(-1.0)` to `0.0`.
6. **queue** → `Enqueue(id)` / `PlayNext(id)` reflected in
   `Event::Queue.up_next_ids`; `ClearQueue` empties; `Next`/`Prev`/`Goto` move
   `now_playing_id`.
7. **repeat/shuffle** → `SetRepeat(One|All)` / `SetShuffle(true)` reflected in
   Tick `repeat`/`shuffle`.
8. **track_art** → embedded-art fixture → `Ok(Some("data:image/…"))`; artless →
   `Ok(None)`; unknown id → `Ok(None)`.
9. **guard** → `send_command` before any folder is opened is a no-op (returns
   the "open a folder first" path without panicking).

### Tier 2 — Frontend logic/component (Vitest browser mode, in CI)

**Tooling** in `app/`: `vitest`, `@vitest/browser`, `vitest-browser-svelte`,
`playwright` (Chromium), `@tauri-apps/api/mocks`. A `vitest.config.ts` with a
browser project for `*.svelte.test.ts` (component/rune tests) and a node project
for pure-logic specs. `@tauri-apps/api` must be ≥ 2.7 for `shouldMockEvents`
(the app is on 2.11). New `npm run test` / `test:watch` scripts.

Cases:

- **art-cache** (`app/src/lib/art-cache.svelte.ts`) — keys by
  `album ?? String(id)`; a second request for the same key does not re-`invoke`;
  the `inView` action gates fetching until a row is visible.
- **player store** — applying a synthetic `Event::Tick` updates
  now-playing/position/duration/volume; `Event::Queue` updates up-next. Fed via
  `mockIPC(..., { shouldMockEvents: true })` + `emit('player', …)`.
- **seek-hold** (`PlayerBar.svelte`) — a scrub sets `seekHold`; it clears after
  300 ms and immediately on `nowPlayingId` change (the seek-jump fix), driven
  with fake timers.
- **Library** — renders a row per `TrackView`; `has_art` gates the art request.
- **ipc.ts wrappers** — `openFolder` returns `null` on dialog cancel;
  `sendCommand` forwards; `trackArt` returns the data URL — all via `mockIPC`
  (including a mocked `plugin:dialog|open`).

### Tier 3 — WebDriver UI smoke (WDIO + tauri-driver, local)

**Dir:** `app/e2e/` (WDIO ^9, Mocha). `onPrepare` builds
`tauri build --debug --no-bundle --features e2e`; capability
`tauri:options.application` → `../src-tauri/target/debug/soul-player.exe`;
launch env `SOUL_PLAYER_OFFLINE=1`. `beforeSession` spawns `tauri-driver`
(installed `--locked`); Windows uses `msedgedriver.exe` matched to WebView2 via
`msedgedriver-tool`. ~1–2 specs, happy path only:

1. App launches; root UI renders (empty-library state).
2. `browser.execute` → `invoke('load_folder_path', { dir: <fixture> })` →
   library rows render (N = fixture count); transport shows playing; the first
   track's title appears in the player bar.
3. Click a track row → player bar's now-playing title updates.
4. Play/pause toggle → reflected in the DOM.
5. Seek bar and volume slider → position/volume reflect.

The native "Add folder" dialog stays a **documented manual check**, out of the
automated path.

### CI

Extend `.github/workflows/ci.yml`:

- **Rust-app** (closes the coverage gap): `cargo test` in `app/src-tauri` with
  `SOUL_PLAYER_OFFLINE=1` (Ubuntu is fine — no device needed). Requires the
  `tauri` `test` dev-dependency (F3).
- **Frontend**: Node, `npm ci` in `app/`, `npx playwright install --with-deps
  chromium`, `npm run test` (Vitest browser mode).
- **Windows e2e** (`app/e2e/.github-workflow.disabled` or a job with
  `if: false`): shipped but off — installs `msedgedriver-tool` (pins the driver
  to WebView2), `cargo install tauri-driver --locked`, runs WDIO on
  `windows-latest`. Enable when a Windows runner is available.

---

## Version notes / known pain points

- **`msedgedriver` ↔ WebView2 drift** is the #1 cause of hung Windows e2e — pin
  via `msedgedriver-tool` (reads the installed Edge version, downloads the match).
- **`tauri-driver`** install with `--locked`.
- **`shouldMockEvents`** needs `@tauri-apps/api ≥ 2.7` (app is on 2.11 — fine).
- **Vite 8 alignment:** the app is on Vite 8; confirm the `vite-plugin-svelte` /
  `vitest` / `vitest-browser-svelte` versions used for the test project share a
  compatible Vite peer range. First Tier-2 task is a config spike that proves a
  green sample test before writing real ones.
- **Tauri mock runtime** does not execute native webview/dialog libs — this is
  exactly why the F1 split exists; Tier 1 never touches the native dialog.

## Risks / open items

- **Vite 8 + Vitest browser toolchain** may need specific version pins; the
  config spike de-risks this before the bulk of Tier 2.
- **Windows CI runner** availability is unconfirmed; Tier 3 is local-only until
  then (decided).
- **`tagged.flac` embedded art** must be verified; add an art fixture if absent.
- **Determinism under `NullSink`:** because playback runs faster than real time,
  seek/position assertions must tolerate that a track may advance quickly — use
  ranges (`position_secs ≥ t`), not equality, and bounded-timeout polling.

## Out of scope

- Audio-output correctness by ear (covered by `cargo xtask qa-audio`) and DSP
  golden checks (covered by `soul-audio-cli/tests/e2e.rs`,
  `soul-audio-engine/tests/engine_e2e.rs`).
- macOS/Linux WebDriver legs (Windows-first; the WDIO service could add them
  later).
- Packaging/installer e2e and the standalone-repo extraction (tracked
  separately).
