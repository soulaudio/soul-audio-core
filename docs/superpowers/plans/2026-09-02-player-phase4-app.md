# Desktop Player — Plan 4: Tauri app shell (`app/`, in-repo but isolated)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up the desktop-player **app shell** on top of the merged Plan 3 headless backend (`soul-audio-service`). A Tauri 2 application with a Svelte 5 + TypeScript frontend that (a) owns a single `AudioPlayer` actor over a real `CpalSink`, (b) exposes typed IPC to the frontend, and (c) renders library, transport, queue, and now-playing state from the actor's event stream. The app lives in an **in-repo `app/` directory that is deliberately NOT a member of the root Cargo workspace**, so `cpal`/`ringbuf`/Tauri never enter the root build or the `cargo xtask verify` / CI gate that Plans 1–3 protect. The `app/` tree is self-contained (own `Cargo.lock`, own frontend `package-lock.json`) and is designed so it can later be **moved into its own repo** by copying the directory and flipping path dependencies to git dependencies.

**Architecture:** Standard Tauri 2 layout inside `app/`:

- `app/src-tauri/` — the Rust side: its **own Cargo workspace** (a `[workspace]` header isolates it from the root workspace; the root `Cargo.toml` is NOT touched). The binary crate depends on `soul-audio-service` (path `../../crates/soul-audio-service`) and `soul-audio-desktop` **with the `playback` feature** (path, `features = ["playback"]`) — this is the only place `cpal`/`ringbuf` are pulled in, and the root gate never builds it.
- `app/src-tauri/src/main.rs` — thin Tauri entrypoint that builds `AudioPlayer::spawn(make_sink = CpalSink::new, library, on_event)` and stores it as managed state; forwards the actor's `Event`s to the frontend over the Tauri event system.
- `app/src/` — Svelte 5 + Vite + TypeScript frontend. Transport/library/queue UI subscribes to the typed event stream and issues typed commands.
- Typed IPC via **tauri-specta v2** (the v2 release-candidate line; v1 does not support Tauri v2). The Plan 3 DTOs (`Command`, `Event`, `Tick`, `QueueState`, `TrackView`, …) are the **single source of truth** for the wire types — the app must not duplicate them. How that composes with specta's `Type` derive is the one open sub-decision, resolved in Task 2 below (optional `specta` feature on `soul-audio-service`, off by default, so the headless crate stays pure).

**Why in-repo but isolated:** Plan 3's constraint — "no `cpal` in the default workspace build, `cargo xtask verify` stays device-free" — is a hard invariant of the CI gate (`.github/workflows/ci.yml` builds host + thumbv7em over workspace default-members; the pre-push lefthook runs `cargo xtask verify`). A Tauri app must link `cpal` and webview bindings, so it cannot be a workspace member. The user's direction (2026-09-02): put it **in `app/` now, move to its own repo later**.

---

## Global Constraints

- The root Cargo workspace and its `Cargo.lock`, `rust-toolchain.toml`, `.github/workflows/`, `xtask`, lefthook config are **untouched** by this plan except where explicitly listed (`.gitignore` additions only).
- `app/src-tauri/Cargo.toml` declares its own `[workspace]` (members = itself) so `cargo` inside `app/src-tauri` never tries to join the root workspace (the root `members` list must NOT include `app/...`; Tauri works fine as a nested standalone cargo project).
- `cpal`, `ringbuf`, `tauri`, `tauri-plugin-*` appear **only** in `app/`'s dependency graph — never in the root `Cargo.lock`. A verification step checks `cargo tree` at the root stays cpal-free.
- `#![forbid(unsafe_code)]` where feasible (Tauri runtime itself uses unsafe; our code files should be clean; check `tauri::generate_context!` macro requirement).
- The service DTOs remain the wire contract. **No duplicated Command/Event/DTO types in the app.**
- Rust edition 2024 / current stable toolchain (matches workspace `rust-toolchain.toml`; the app crate inherits nothing from the root workspace — it sets `edition.workspace` only within its own workspace, so write explicit `edition = "2024"`).
- Node 22 present locally; the frontend uses plain Vite + Svelte 5 (no SvelteKit — no server needed for a desktop shell).
- QA model: everything testable headlessly stays in `soul-audio-service` unit tests (already 18/18). The app itself is verified by (a) `cargo check`/`clippy` inside `app/src-tauri`, (b) `svelte-check` + `vite build` on the frontend, (c) **manual device QA** checklist — no audio device in CI. Optionally an `--offline` mode (NullSink) lets basic wiring be smoke-tested without a device.
- Every commit leaves the root `cargo xtask verify` green AND `app/src-tauri` compiling + clippy-clean and the frontend type-checking.

---

## File Structure

```
app/
  package.json            NEW  — frontend deps (svelte 5, vite, typescript, @tauri-apps/api)
  vite.config.ts          NEW  — Vite config for the Svelte frontend (Tauri dev port 1420)
  svelte.config.js        NEW  — Svelte compiler config
  tsconfig.json           NEW  — strict TS
  index.html              NEW  — Tauri loads this in the webview
  src/
    main.ts               NEW  — bootstrap
    App.svelte            NEW  — root component (layout: sidebar library + transport bar)
    lib/
      ipc.ts              NEW  — typed invoke/event bindings (from Task 2 export, hand-kept or generated)
      store.ts            NEW  — Svelte runes store fed by the event stream (Tick/QueueState)
    views/
      Library.svelte      NEW  — folder scan trigger + track list (TrackView)
      PlayerBar.svelte    NEW  — play/pause/stop/next/prev/seek/volume + repeat/shuffle
      Queue.svelte        NEW  — Up-Next list + context position
  src-tauri/
    Cargo.toml            NEW  — own [workspace]; bin soul-player; path deps on service + desktop(playback)
    build.rs              NEW  — tauri_build::build()
    tauri.conf.json       NEW  — window, bundle id, devUrl/distDir, withGlobalTauri
    capabilities/
      default.json        NEW  — Tauri 2 ACL: core:default + dialog:allow-open (folder picker)
    src/
      main.rs             NEW  — entrypoint: builder → .manage(ActorHandle) → run
      actor.rs            NEW  — ActorHandle wrapper: owns AudioPlayer, spawn with CpalSink factory,
                                 holds the current Library (Arc), set_library on folder open
      commands.rs         NEW  — #[tauri::command] fns: open_folder, library_views, send_command
      events.rs           NEW  — forward service Event → app.emit("player:{name}", payload)
      ipc_types.rs        NEW  — specta wiring for the DTO re-exports (Task 2 decision)
    icons/                NEW  — Tauri icons (generated once; min set)
.gitignore               MOD  — ignore app/node_modules, app/src-tauri/target, app/dist
```

The eventual move to a separate repo = `git subtree`/copy `app/` out, delete the `app/` paths above, and in `app/src-tauri/Cargo.toml` change the three path deps to `git = "https://git.datadir.dev/soulaudio/soul-audio-core.git"`. Nothing else couples the app to living inside this repo.

---

### Task 1: `app/` skeleton — Tauri 2 shell + Svelte 5 frontend booting together

**Files:** create the whole `app/` tree above (Rust workspace + minimal frontend); modify `.gitignore`.

**Goal:** a window that opens and shows a "soul-player" placeholder, proving the isolated-workspace layout, the Tauri dev loop, and that the root workspace/verify are untouched. No audio, no IPC yet.

- [ ] **Step 1: Create `app/src-tauri/Cargo.toml`** (own workspace, path deps; pin real current versions at implement time, note the researched minimums):

```toml
[workspace]
members = ["."]
resolver = "3"

[package]
name = "soul-player"
version = "0.1.0"
edition = "2024"
license.workspace = false
description = "Soul Audio desktop player shell (Tauri 2) over soul-audio-service."

[build-dependencies]
tauri-build = { version = "2", features = [] }

[dependencies]
tauri = { version = "2", features = [] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
# Plan 3 backend + the one place cpal enters the graph:
soul-audio-service = { path = "../../crates/soul-audio-service" }
soul-audio-desktop = { path = "../../crates/soul-audio-desktop", features = ["playback"] }
```

> If Tauri 2 stable's MSRV or lock resolution conflicts with the workspace toolchain (root `rust-toolchain.toml` pins `stable`), the app can use its own `rust-toolchain.toml` inside `app/` — isolation means no conflict is possible; prefer not to add one unless needed.

- [ ] **Step 2: `build.rs` + `tauri.conf.json` + minimal `main.rs`** that opens a window showing `index.html`. Use `tauri.conf.json` with `build.devUrl = "http://localhost:1420"`, `build.beforeDevCommand = "npm run dev"`, `build.frontendDist = "../dist"` (relative to `src-tauri`), `app.withGlobalTauri = true` (simplest for the frontend), `app.windows[0]` titled "soul-player", ~1100×720, `bundle.active = true`.

```rust
// src-tauri/src/main.rs — Task 1 (placeholder)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("error while running soul-player");
}
```

- [ ] **Step 3: Frontend scaffold.** `npm create vite@latest . -- --template svelte-ts` inside `app/` (or hand-write the 6 files above), add `@tauri-apps/api`. `App.svelte` renders a heading + version; `index.html` has `<script type="module" src="/src/main.ts">`. `vite.config.ts` with `server: { port: 1420, strictPort: true }` and the standard Tauri `clearScreen`/`envPrefix` bits.

- [ ] **Step 4: `.gitignore`** — add to the root `.gitignore`: `app/node_modules/`, `app/dist/`, `app/src-tauri/target/`. Verify with `git status` that only intended files appear.

- [ ] **Step 5: Verify isolation + boot.**

```
cd app/src-tauri && cargo check          # own workspace resolves path deps incl. cpal
cd app && npm install && npm run build   # frontend type-checks + builds
cargo xtask verify                       # root gate STILL green, and:
cargo tree -p soul-audio-service | rg -i "cpal|ringbuf|tauri"   # expect NO hits at root
```

Run the app once (`npm run tauri dev`) on the local machine — window opens with the placeholder (manual check).

- [ ] **Step 6: Commit** the whole `app/` tree + `.gitignore`:

```
git add app .gitignore
git commit -m "feat(app): Tauri 2 + Svelte 5 app shell skeleton (isolated app/ workspace)"
```

---

### Task 2: Wire contract — specta-typed IPC over the Plan 3 DTOs

**Files:** modify `crates/soul-audio-service/Cargo.toml` + DTO files (`types.rs`, `library.rs` re-export surface) **behind an off-by-default `specta` feature**; create `app/src-tauri/src/ipc_types.rs` + first `#[tauri::command]`.

**Goal:** decide + implement how the serde DTOs become specta `Type`s, export TS bindings, and prove a round-trip command. **Constraint: the service DTOs stay the single source of truth; no duplicated types.**

**The sub-decision (resolve with evidence at implement time):** tauri-specta v2 requires `specta::Type`. Two options:

- **(A — preferred)** Add to `soul-audio-service` an optional dep `specta = "2.0.0-rc.x"` and a `specta` feature; on each DTO/enum put `#[cfg_attr(feature = "specta", derive(specta::Type))]` (serde derives remain primary). Off by default → root `Cargo.toml` untouched for the gate; confirm `cargo tree` at root shows no specta when the feature is off, and that the root `Cargo.lock` either stays clean or (if cargo locks the optional dep) gains only inert entries — document which. App enables `soul-audio-service = { path = …, features = ["specta"] }`.
- **(B)** If (A) proves disruptive to the headless crate, generate the TS bindings with specta types *only in the app crate* by deriving `specta::Type` on app-local newtype wrappers and mapping — only acceptable if the mapping is mechanical and tested; prefer (A) to avoid drift.

- [x] **Step 1: service `specta` feature (option A).** Modify `crates/soul-audio-service/Cargo.toml`:

```toml
[features]
default = []
specta = ["dep:specta"]

[dependencies]
specta = { version = "2.0.0-rc", optional = true }
```

Add `#[cfg_attr(feature = "specta", derive(specta::Type))]` to: `Command`, `Event`, `Tick`, `QueueState`, `PlayState`, `RepeatMode`, `TrackView` (types.rs) and the re-exported `Track`/`Library` if needed by commands. Keep serde as the canonical serialization (specta's `Type` derive reads serde attrs).

- [x] **Step 2: `app/src-tauri/src/ipc_types.rs`** — the single bridge: re-export the service types and build the specta registry:

```rust
use specta::TypeCollection;
use tauri_specta::{collect_commands, ts};
use soul_audio_service::{Command, Event, Tick, QueueState, TrackView};

pub fn export_bindings() {
    ts::export(
        collect_commands![open_folder, library_views, send_command],
        TypeCollection::default(),
        "../../src/lib/bindings.ts", // generated TS for the frontend
    )
    .expect("ts export");
}
```

(Command signatures defined in Task 3; export can run in a `#[cfg(debug_assertions)]` build-script or a `bindings` xtask-style binary — pick whichever is least friction; the checked-in `bindings.ts` is authoritative.)

- [x] **Step 3: First command with no actor yet** — `library_views` returning `Vec<TrackView>` from an empty library proves the typed command + specta derive pipeline end to end; `#[tauri::command] #[specta::specta]` on each.

- [x] **Step 4: Verify.** `cargo check` in `app/src-tauri` (feature on); `npm run build` with the generated `bindings.ts` imported; root `cargo xtask verify` green; root `cargo tree` shows no cpal/specta in the default build. Run app: a button calls `library_views` and logs `[]` (manual).

- [x] **Step 5: Commit** (service feature + app bridge + generated bindings):

```
git add crates/soul-audio-service app/src-tauri
git commit -m "feat(ipc): specta-typed IPC over Plan 3 DTOs (service specta feature, off by default)"
```

---

### Task 3: Host the actor — `AudioPlayer::spawn` over `CpalSink`, commands + event bridge

**Files:** create `app/src-tauri/src/actor.rs`, `commands.rs`, `events.rs`; wire into `main.rs` with managed state + event forwarding. **No frontend work yet.**

**Goal:** one `AudioPlayer` per app run, constructed with `CpalSink::new` as the sink factory (runs on the pump thread — exactly the Plan 3 design), library swap on folder open, service `Event`s re-emitted to the webview.

- [x] **Step 1: `actor.rs`** — a `PlayerState` guard:

```rust
// app/src-tauri/src/actor.rs
use std::sync::{Arc, Mutex};
use soul_audio_desktop::sink::CpalSink;
use soul_audio_service::{AudioPlayer, Command, Event, Library};
use tauri::{AppHandle, Emitter, Manager};

pub struct PlayerState {
    pub player: Mutex<Option<AudioPlayer>>, // None until a folder/library is chosen
    pub library: Mutex<Option<Arc<Library>>>,
}

impl PlayerState {
    pub fn ensure(&self, app: &AppHandle) -> &AudioPlayer {
        // (re)build the actor with CpalSink::new as the factory; on_event forwards
        // every service Event to the frontend via app.emit("player", event)
    }
}
```

The `on_event` closure must `app.emit("player", event)` (serde-serializable `Event`). Because `Event` includes the throttled `Tick` (~8 Hz) + `Queue` and any transport errors, a single `"player"` channel with the enum payload is enough for the whole UI (match on `Event::Tick` / `Event::Queue` in the store).

- [x] **Step 2: `commands.rs`** (all `#[tauri::command]`, specta-annotated, returning `Result<T, String>` mapping anyhow errors to a message string — per the Plan 3 error policy):

```rust
/// Ask the user for a folder, scan it into a Library, hand it to the actor.
#[tauri::command]
fn open_folder(state: tauri::State<'_, PlayerState>, app: AppHandle) -> Result<Vec<TrackView>, String>;

/// The current library's views (used to render the list without re-scanning).
#[tauri::command]
fn library_views(state: tauri::State<'_, PlayerState>) -> Vec<TrackView>;

/// Fire one transport/queue command at the actor (fire-and-forget semantics).
#[tauri::command]
fn send_command(state: tauri::State<'_, PlayerState>, cmd: Command) -> Result<(), String>;
```

`open_folder` uses `tauri_plugin_dialog::DialogExt::file().pick_folder` (add `tauri-plugin-dialog`), then `Library::scan`, then `set_library` + `ensure`. If no folder has been chosen yet, `open_folder` itself can auto-start playback of the scanned context (v1 decision: opening a folder = play it from track 0; skip auto-play if that feels wrong — decide in review).

- [x] **Step 3: `main.rs` wiring** — register the plugin, manage state, register commands, run `export_bindings()` in debug. On app exit the `PlayerState` Drop stops the actor threads (Plan 3 `Drop` now returns promptly — regression test covers Repeat::All).

- [x] **Step 4: Verify.** `cargo check` + clippy in `app/src-tauri`; `cargo xtask verify` at root still green (nothing above touched the root build). Manual: launch, no folder chosen → UI idle; the actor only exists after first `open_folder` (avoid grabbing the audio device at startup — better UX and no spurious device errors).

- [x] **Step 5: Commit**:

```
git add app/src-tauri/src
git commit -m "feat(app): host AudioPlayer over CpalSink; folder-open command + event bridge"
```

---

### Task 4: Svelte 5 UI — library, transport, queue, now-playing

**Files:** `app/src/lib/store.ts`, `app/src/lib/ipc.ts`, `app/src/views/*.svelte`, `app/src/App.svelte`, generated `app/src/lib/bindings.ts` (from Task 2).

**Goal:** the actual player screen. Data flow is one-way: `open_folder` returns the initial `TrackView[]`; thereafter the store is fed **only** by the `"player"` event channel (`Event::Tick` → now-playing/position/state/volume/shuffle/repeat; `Event::Queue` → up-next/context pos). Transport buttons call `send_command(Command::…)`.

- [x] **Step 1: `store.ts`** — Svelte 5 runes store:

```ts
// app/src/lib/store.ts
export const library = $state<TrackView[]>([]);
export const now = $state<{ id: number | null; pos: number; dur: number } | null>(null);
export const playing = $state(false);
export const queue = $state<{ upNext: number[]; pos: number; len: number }>({ upNext: [], pos: 0, len: 0 });
export const volume = $state(1);
export const shuffle = $state(false);
export const repeat = $state<"Off" | "All" | "One">("Off");

// listen("player", e => dispatch Event payload into the above) — called once on mount
export function bindPlayerEvents(): void;
```

Match the service's `Event` enum shape exactly (derive in TS from bindings).

- [x] **Step 2: `App.svelte` layout** — a compact player: left = library list; top/bottom = transport bar; optional right = queue drawer. Matches the monday-match style discipline used elsewhere in the datadir org (dark-aware, host theme, no oversized chrome) but there is no host theme here — pick a clean neutral theme with `prefers-color-scheme` support.

- [x] **Step 3: `Library.svelte`** — "Open folder…" button (`open_folder`), list of `TrackView` (title/artist/album/duration), click row = `send_command(Command::PlayContext{ ids, start: i })`; selected row highlights from `now.id`.

- [x] **Step 4: `PlayerBar.svelte`** — prev / play-pause / next / stop; seek slider bound to `Tick.position_secs`/`duration_secs` (drag → `Command::Seek` on release, throttle); volume slider → `Command::SetVolume`; repeat cycle button → `SetRepeat`; shuffle toggle → `SetShuffle`. All reflect store state; all commands go through `send_command`.

- [x] **Step 5: `Queue.svelte`** — Up-Next chips (ids → look up in library for title), "context pos / len", and buttons to `PlayNext(id)`/`Enqueue(id)` from a row hover; `ClearQueue` when non-empty.

- [x] **Step 6: Verify.** `svelte-check` + `npm run build` clean; root verify green; **manual device QA checklist** (audio plays through the real sink, transport/seek/repeat/shuffle all behave, gapless next track, folder re-open swaps library, closing the window exits cleanly with no hang — exercises the Plan 3 Drop regression path in production).

- [x] **Step 7: Commit**:

```
git add app/src app/src/lib/bindings.ts
git commit -m "feat(ui): Svelte 5 player UI (library, transport, queue) over typed IPC"
```

---

### Task 5: Polish + packaging + move-readiness

**Files:** `tauri.conf.json` (bundle id/icons), optional `app/README.md`, root README note.

- [x] **Step 1: Bundle.** Set `identifier` (e.g. `co.soulaudio.soul-player`), generate a minimal icon set, run `npm run tauri build` and smoke-test the packaged binary on the local OS (Windows). Document that Linux/macOS bundling needs per-OS webview deps (not in the root CI).
- [x] **Step 2: Offline smoke (optional but recommended).** A `--offline` flag or env that swaps `CpalSink` for a headless sink (reuse a tiny in-app NullSink or a service test-helper) so the full command/event wiring can be exercised in CI later without a device — scope creep guard: only if it stays small.
- [x] **Step 3: Move-readiness.** `app/README.md` documenting: layout, dev loop (`npm run tauri dev`), and the exact move-to-own-repo steps (copy `app/`, flip 3 path deps to git deps, keep `[workspace]` isolation). Root `README.md` gets one line pointing at `app/`.
- [x] **Step 4: Full gate** — `cargo xtask verify` (root) green; `cargo clippy` in `app/src-tauri` clean; `svelte-check` clean; packaged binary QA'd once on device.
- [x] **Step 5: Commit**:

```
git add app/README.md README.md tauri.conf.json
git commit -m "feat(app): packaging + offline smoke + move-readiness docs"
```

---

## Self-Review

**Spec coverage (issue #20 + Plan 3 forward references):**

- Tauri command layer mapping `Command`/`Event` to IPC → Tasks 2–3. ✓
- `CpalSink` injection via `soul-audio-desktop` `playback` feature, sink built on the pump thread → Task 3 (`AudioPlayer::spawn` factory; the service never names cpal). ✓
- tauri-specta typed IPC from the service DTOs → Task 2 (single source of truth; optional service `specta` feature, off by default). ✓
- Svelte UI: library/transport/queue/now-playing → Task 4 (store fed solely by the event stream). ✓
- `cover_art` + `open_folder` (deferred by Plan 3) → `open_folder` = Task 3 command; `cover_art` deliberately OUT of scope for the MVP (needs a tag-art read path in the service — add a follow-up issue, do not sneak into this plan). ✓
- `cargo xtask verify` stays green / device-free → enforced by isolation (app not a workspace member) + Task 1 verification step. ✓

**The one genuinely open item (resolve in Task 2 with evidence, don't defer silently):** the exact mechanism for `specta::Type` on the service DTOs. Option A (optional service feature) keeps the headless crate pure and is preferred; if specta version churn makes it awkward, Option B (app-local wrappers) is acceptable only with a tested mapping. The implementer must record which was chosen and why in the task report.

**Risk register:**

- tauri-specta v2 is a release-candidate line (specta-rs docs: "lock your Specta version"); pin exact versions (`=`) in `app/src-tauri/Cargo.toml` to avoid silent breaking changes between tasks. Root is unaffected either way.
- The frontend binding file is generated; if the export step fights the build system, committing the generated `bindings.ts` and regenerating manually before UI work is fine (checked-in file authoritative).
- No audio device in CI ⇒ real playback correctness is only ever *unit-tested* (service) + *manually QA'd* (app). The `--offline` option in Task 5 narrows that gap without scope creep.
- Windows: Tauri needs WebView2 (present on this machine); cpal defaults to WASAPI shared mode. If the default device is busy, `CpalSink::new` errors visibly in the actor (eprintln) — UI should surface "no output device" rather than silently idle (Task 3 error mapping).

**Atomicity:** every task leaves the root workspace + verify gate green and the app compiling/type-checking; the app is only ever built by its own workspace. Each task ends with a commit; Task 5 is the final full gate.

**Move-readiness:** the whole point of the in-repo-but-isolated layout is that nothing in the app depends on *where* it lives except three relative path deps, and nothing in the root depends on the app. The eventual extraction is a directory copy + a three-line Cargo.toml edit.
