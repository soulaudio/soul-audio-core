# Desktop Player E2E Test Suite — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a three-tier end-to-end test suite for the `soul-player`
desktop app (headless Rust product-e2e, Vitest browser-mode frontend tests, and
a local WebDriver UI smoke pass) that guards the folder-switch, per-format
duration, seek, volume, queue, and cover-art behaviours.

**Architecture:** One shared foundation (a library target for `app/src-tauri`,
a "pick vs load" split of the folder-open command, an `e2e` cargo feature, and
committed fixtures) under three test tiers. Tiers 1 & 2 run in CI; Tier 3 runs
locally.

**Tech Stack:** Rust (edition 2024) + `tauri` `test` mock runtime; Vitest
browser mode + `vitest-browser-svelte` + Playwright/Chromium; WebdriverIO 9 +
`tauri-driver` (Edge WebView2).

Design spec: `docs/superpowers/specs/2026-09-04-desktop-e2e-test-suite-design.md`.

## Global Constraints

- **No AI trailers in commits** — lefthook rejects `Co-Authored-By: Claude` /
  `Claude-Session` lines. Commits are GPG-signed (the environment signs by
  default; do not pass `--no-gpg-sign`).
- **`app/src-tauri` is a separate Cargo workspace** — build/test it with
  `cargo …` run *inside* `app/src-tauri` (or `--manifest-path app/src-tauri/Cargo.toml`).
  Root `cargo test` / `cargo xtask verify` do not reach it.
- **Device-free** — every automated test sets `SOUL_PLAYER_OFFLINE=1` so the
  actor uses `NullSink` (no audio device, faster-than-real-time).
- **Condition-based waiting, never fixed sleeps** — poll captured `Tick` events
  until the asserted state appears, with a bounded timeout.
- **Durations differ per fixture** — the PCM tones are **2.0 s**; the DSD
  fixtures are **0.5 s**. Assert per-fixture expected durations, never a blanket
  value. Tolerance: ±0.05 s lossless, ±0.15 s lossy (MP3/AAC encoder delay).
- **Edition 2024:** `std::env::set_var` is `unsafe` — wrap it, and only ever
  *set* `SOUL_PLAYER_OFFLINE` (never unset; all tests want offline).
- **Preserve real-user behaviour** — `open_folder` (native dialog) keeps working
  exactly as before; the split only extracts a pure inner function.

---

## File Structure

**Foundation (Task 1):**
- Create `app/src-tauri/src/lib.rs` — library target exposing `pub mod actor / commands / ipc / offline` and `pub fn run()`.
- Modify `app/src-tauri/src/main.rs` — reduce to a shim calling `soul_player::run()`.
- Modify `app/src-tauri/src/commands.rs` — extract `load_folder`, add feature-gated `load_folder_path`.
- Modify `app/src-tauri/src/ipc.rs` — conditionally register `load_folder_path`.
- Modify `app/src-tauri/Cargo.toml` — `[lib]`/`[[bin]]`, `[features] e2e`, `tauri` `test` dev-dep.
- Create `app/src-tauri/tests/common/mod.rs` — mock-app builder, event capture, `write_wav`, `wait_for_tick`.
- Create `app/src-tauri/tests/foundation.rs` — the Task-1 smoke tests.

**Fixtures (Task 2):**
- Modify `testdata/generate.sh` — add `tone.opus` and `arted.flac` (embedded cover).
- Modify `testdata/generate_dsd.py` — accept a rate arg; emit `tone-dsd128.dsf`.
- Create `testdata/tone.opus`, `testdata/arted.flac`, `testdata/tone-dsd128.dsf` (committed binaries).
- Create `app/src-tauri/tests/fixtures.rs` — validates every fixture probes/decodes.

**Tier 1 (Task 3):**
- Create `app/src-tauri/tests/e2e.rs` — the product-e2e cases.

**Tier 2 (Tasks 4–5):**
- Modify `app/package.json` — devDeps + `test`/`test:watch` scripts.
- Create `app/vitest.config.ts`, `app/src/lib/__fixtures__/Smoke.svelte` + sample test (Task 4).
- Create `app/src/**/*.{svelte.,}test.ts` — the real frontend tests (Task 5).

**Tier 3 (Task 6):**
- Create `app/e2e/` — `package.json`, `wdio.conf.ts`, `tsconfig.json`, `test/specs/*.e2e.ts`, `README.md`.
- Modify `app/package.json` — an `e2e` convenience script.

**CI (Task 7):**
- Modify `.github/workflows/ci.yml` — `app` (Rust) + `frontend` (Vitest) jobs.
- Create `.github/workflows/webdriver.yml.disabled` — the shipped-but-off Windows job.

---

## Task 1: Foundation — library target, folder-open split, `e2e` feature, test harness

**Files:**
- Create: `app/src-tauri/src/lib.rs`
- Modify: `app/src-tauri/src/main.rs`, `app/src-tauri/src/commands.rs`, `app/src-tauri/src/ipc.rs`, `app/src-tauri/Cargo.toml`
- Create: `app/src-tauri/tests/common/mod.rs`, `app/src-tauri/tests/foundation.rs`

**Interfaces:**
- Produces: `soul_player::commands::load_folder(state: &PlayerState, app: &AppHandle, dir: &Path) -> Result<Vec<TrackView>, String>`; feature-gated command `soul_player::commands::load_folder_path`; `soul_player::actor::PlayerState`; test helpers `common::{mock_app, capture_events, write_wav, wait_for_tick}`.

- [ ] **Step 1: Add the library target and feature to `Cargo.toml`**

Add to `app/src-tauri/Cargo.toml` (keep the existing `[package]`, runtime deps, and `[profile.dev]` blocks):

```toml
[lib]
name = "soul_player"
path = "src/lib.rs"

[[bin]]
name = "soul-player"
path = "src/main.rs"

[features]
# Exposes the test-only `load_folder_path` command (bypasses the native folder
# dialog for WebDriver/e2e). Off in release.
e2e = []

[dev-dependencies]
# The mock runtime for Rust product-e2e (Tier 1). Dev-only: `cargo build
# --release` never compiles dev-deps, so the `test` feature can't leak into a
# shipped binary.
tauri = { version = "2", features = ["test"] }
```

- [ ] **Step 2: Create `src/lib.rs`, move the app bootstrap out of `main.rs`**

`app/src-tauri/src/lib.rs`:

```rust
//! soul-player library surface. The binary (`main.rs`) is a thin shim over
//! [`run`]; exposing the modules as a lib lets `tests/` drive the real command
//! and actor code (a binary-only crate's modules are private to the bin).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

pub mod actor;
pub mod commands;
pub mod ipc;
pub mod offline;

use actor::PlayerState;

/// Build and run the Tauri app (moved verbatim from the old `main`).
pub fn run() {
    let bindings = ipc::builder();
    #[cfg(debug_assertions)]
    {
        ipc::export(&bindings);
        if std::env::var_os("SOUL_PLAYER_CODEGEN").is_some() {
            println!("bindings.ts regenerated");
            return;
        }
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(PlayerState::new())
        .invoke_handler(bindings.invoke_handler())
        .run(tauri::generate_context!())
        .expect("error while running soul-player");
}
```

Replace `app/src-tauri/src/main.rs` entirely with:

```rust
//! soul-player binary entry point — a shim over the `soul_player` library so
//! integration tests can reach the command/actor modules.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    soul_player::run();
}
```

- [ ] **Step 3: Run it to verify the split compiles and the app still boots**

Run (inside `app/src-tauri`): `cargo build`
Expected: builds clean; `cargo run` still opens the app (manual, optional). `npm run tauri dev` from `app/` still works.

- [ ] **Step 4: Extract `load_folder` and add `load_folder_path` in `commands.rs`**

In `app/src-tauri/src/commands.rs`, add `use std::path::Path;` to the imports, then replace the `open_folder` fn (lines 15–43) with the extracted pair and add the gated command. Keep everything else (`library_views`, `send_command`, `track_art`, the base64 helpers, tests) unchanged:

```rust
/// Scan `dir` into a `Library`, hand it to the actor, and start playing from
/// track 0. The pure half of `open_folder` — no dialog — so tests and the e2e
/// command can drive it with a fixture directory.
pub fn load_folder(
    state: &PlayerState,
    app: &AppHandle,
    dir: &Path,
) -> Result<Vec<TrackView>, String> {
    let lib = Arc::new(Library::scan(dir).map_err(|e| format!("scan failed: {e:#}"))?);
    let views = lib.views();
    if views.is_empty() {
        return Err("no supported audio files in that folder".to_string());
    }
    state.set_library(lib);
    let ids: Vec<u32> = views.iter().map(|v| v.id).collect();
    state.send_command(app, Command::PlayContext { ids, start: 0 });
    Ok(views)
}

/// Ask the user for a folder (native dialog), then load it.
#[tauri::command]
#[specta::specta]
pub fn open_folder(
    state: State<'_, PlayerState>,
    app: AppHandle,
) -> Result<Vec<TrackView>, String> {
    let picked = app
        .dialog()
        .file()
        .blocking_pick_folder()
        .ok_or_else(|| "no folder selected".to_string())?;
    let dir = picked.into_path().map_err(|e| e.to_string())?;
    load_folder(&state, &app, &dir)
}

/// Test-only: load a folder by path, bypassing the native dialog. Enabled by the
/// `e2e` cargo feature (off in release); used by the WebDriver smoke suite.
#[cfg(feature = "e2e")]
#[tauri::command]
#[specta::specta]
pub fn load_folder_path(
    state: State<'_, PlayerState>,
    app: AppHandle,
    dir: String,
) -> Result<Vec<TrackView>, String> {
    load_folder(&state, &app, Path::new(&dir))
}
```

- [ ] **Step 5: Conditionally register `load_folder_path` in `ipc.rs`**

In `app/src-tauri/src/ipc.rs`, replace the single `.commands(collect_commands![…])` call in `builder()` with a cfg-selected binding (only one arm compiles per build, so the differing types are fine):

```rust
pub fn builder() -> Builder<Wry> {
    #[cfg(not(feature = "e2e"))]
    let commands = collect_commands![
        commands::library_views,
        commands::open_folder,
        commands::send_command,
        commands::track_art,
    ];
    #[cfg(feature = "e2e")]
    let commands = collect_commands![
        commands::library_views,
        commands::open_folder,
        commands::send_command,
        commands::track_art,
        commands::load_folder_path,
    ];
    Builder::<Wry>::new()
        .typ::<soul_audio_service::Event>()
        .commands(commands)
}
```

- [ ] **Step 6: Verify both feature configs compile**

Run (inside `app/src-tauri`):
`cargo build` then `cargo build --features e2e`
Expected: both clean. `cargo build --features e2e` additionally exposes the command.

- [ ] **Step 7: Write the shared test harness `tests/common/mod.rs`**

```rust
//! Shared Tier-1 test harness: a mock Tauri app over `NullSink`, `"player"`
//! event capture, a WAV fixture writer, and a bounded Tick poller.
#![allow(dead_code)] // not every test uses every helper

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use soul_audio_service::{Event, Tick};
use soul_player::actor::PlayerState;
use tauri::test::{MockRuntime, mock_builder, mock_context, noop_assets};
use tauri::{App, Listener, Manager};

static OFFLINE: Once = Once::new();

/// Build a mock app with `PlayerState` managed and `SOUL_PLAYER_OFFLINE=1` so
/// the lazily-built actor uses `NullSink`. All Tier-1 tests want offline, so the
/// env var is only ever set (never unset).
pub fn mock_app() -> App<MockRuntime> {
    OFFLINE.call_once(|| {
        // SAFETY: set once, before any actor is built; value all tests share.
        unsafe { std::env::set_var("SOUL_PLAYER_OFFLINE", "1") };
    });
    mock_builder()
        .manage(PlayerState::new())
        .build(mock_context(noop_assets()))
        .expect("build mock app")
}

/// Subscribe to the `"player"` channel and collect deserialized `Event`s.
pub fn capture_events(app: &App<MockRuntime>) -> Arc<Mutex<Vec<Event>>> {
    let sink = Arc::new(Mutex::new(Vec::new()));
    let s = Arc::clone(&sink);
    app.listen("player", move |ev| {
        if let Ok(e) = serde_json::from_str::<Event>(ev.payload()) {
            s.lock().unwrap().push(e);
        }
    });
    sink
}

/// Poll captured events until a `Tick` satisfies `pred`, or panic after
/// `timeout`. Returns the matching Tick.
pub fn wait_for_tick(
    events: &Arc<Mutex<Vec<Event>>>,
    pred: impl Fn(&Tick) -> bool,
    timeout: Duration,
) -> Tick {
    let start = Instant::now();
    loop {
        if let Some(t) = events
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::Tick(t) if pred(t) => Some(t.clone()),
                _ => None,
            })
        {
            return t;
        }
        assert!(start.elapsed() < timeout, "timed out waiting for a matching Tick");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Write a `secs`-second 44.1 kHz stereo 16-bit WAV of a quiet 440/659 Hz tone
/// to `dir/name`. Used to build distinct-duration fixture folders at test time.
pub fn write_wav(dir: &Path, name: &str, secs: f32) {
    let rate = 44_100u32;
    let ch = 2u16;
    let n = (secs * rate as f32) as u32;
    let data_len = n * u32::from(ch) * 2;
    let mut f = std::fs::File::create(dir.join(name)).unwrap();
    // RIFF header
    f.write_all(b"RIFF").unwrap();
    f.write_all(&(36 + data_len).to_le_bytes()).unwrap();
    f.write_all(b"WAVEfmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
    f.write_all(&ch.to_le_bytes()).unwrap();
    f.write_all(&rate.to_le_bytes()).unwrap();
    f.write_all(&(rate * u32::from(ch) * 2).to_le_bytes()).unwrap();
    f.write_all(&(ch * 2).to_le_bytes()).unwrap();
    f.write_all(&16u16.to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&data_len.to_le_bytes()).unwrap();
    for i in 0..n {
        let t = i as f32 / rate as f32;
        let l = (0.3 * (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 32767.0) as i16;
        let r = (0.3 * (2.0 * std::f32::consts::PI * 659.25 * t).sin() * 32767.0) as i16;
        f.write_all(&l.to_le_bytes()).unwrap();
        f.write_all(&r.to_le_bytes()).unwrap();
    }
}
```

> **Risk note for the implementer:** if `App<MockRuntime>` does not deliver
> `emit("player")` → `listen("player")` in-process (no event loop running),
> capture will stay empty and `wait_for_tick` will time out. Verify with Step 9
> first. If it fails, the fallback is a `#[cfg(feature = "e2e")]` event tap in
> `actor::build_actor_with` that also pushes `Event`s into a shared
> `OnceLock<Mutex<Vec<Event>>>` the tests read — implement that only if the
> listen path proves not to work, and report it as a `DONE_WITH_CONCERNS`.

- [ ] **Step 8: Write `tests/foundation.rs`**

```rust
//! Task-1 smoke: the folder-open split works over the mock runtime + NullSink.
mod common;

use std::time::Duration;

use common::{capture_events, mock_app, wait_for_tick, write_wav};
use soul_player::commands::load_folder;
use soul_player::actor::PlayerState;
use tauri::Manager;

#[test]
fn load_folder_scans_and_autoplays() {
    let app = mock_app();
    let events = capture_events(&app);
    let dir = tempfile::tempdir().unwrap();
    write_wav(dir.path(), "a.wav", 2.0);
    write_wav(dir.path(), "b.wav", 2.0);

    let state = app.state::<PlayerState>();
    let views = load_folder(state.inner(), app.handle(), dir.path()).unwrap();
    assert_eq!(views.len(), 2);

    // Auto-PlayContext → a Tick with now_playing set and Playing.
    let tick = wait_for_tick(
        &events,
        |t| t.now_playing_id.is_some(),
        Duration::from_secs(5),
    );
    assert_eq!(tick.now_playing_id, Some(views[0].id));
}

#[test]
fn empty_folder_errors() {
    let app = mock_app();
    let dir = tempfile::tempdir().unwrap();
    let state = app.state::<PlayerState>();
    let err = load_folder(state.inner(), app.handle(), dir.path()).unwrap_err();
    assert!(err.contains("no supported audio files"));
}

#[test]
fn send_command_before_folder_is_noop() {
    let app = mock_app();
    let state = app.state::<PlayerState>();
    // No panic, no library → the actor is never built.
    state.inner().send_command(app.handle(), soul_audio_service::Command::TogglePlay);
}
```

Add `tempfile` to `[dev-dependencies]` in `app/src-tauri/Cargo.toml`:
`tempfile = "3"`

- [ ] **Step 9: Run the foundation tests**

Run (inside `app/src-tauri`): `cargo test --test foundation`
Expected: 3 passed. (If `load_folder_scans_and_autoplays` times out, apply the Step-7 fallback.)

- [ ] **Step 10: Verify the whole app crate + regenerate bindings are unaffected**

Run (inside `app/src-tauri`): `cargo test` then `SOUL_PLAYER_CODEGEN=1 cargo run` (the codegen path still works).
Expected: existing `commands` unit tests + new foundation tests pass; bindings regenerate.

- [ ] **Step 11: Commit**

```bash
git add app/src-tauri
git commit -m "test(app): library target + folder-open split + e2e feature + Tier-1 harness"
```

---

## Task 2: Fixtures — Opus, embedded-art, and DSD128

**Files:**
- Modify: `testdata/generate.sh`, `testdata/generate_dsd.py`
- Create: `testdata/tone.opus`, `testdata/arted.flac`, `testdata/tone-dsd128.dsf`
- Create: `app/src-tauri/tests/fixtures.rs`

**Prerequisite:** `ffmpeg` (with `libopus`, `flac`) and `python3 + numpy` on PATH
— the same tools that produced the existing fixtures. If `ffmpeg` is absent,
this task is BLOCKED pending its install (escalate; do not fake the binaries).

**Interfaces:**
- Produces committed fixtures: `testdata/tone.opus` (Opus, ~2.0 s), `testdata/arted.flac` (FLAC with embedded cover, ~0.2 s), `testdata/tone-dsd128.dsf` (DSD128, 0.5 s).

- [ ] **Step 1: Extend `generate.sh` — Opus + embedded-art fixtures**

In `testdata/generate.sh`, after the `tagged.flac` block (line 32) and before the
DSD block, add:

```bash
# Opus (48 kHz internal): exercises the last-Ogg-page-granule duration path.
# There is otherwise NO .opus fixture (tone.ogg is FLAC-in-Ogg).
ffmpeg -y -hide_banner -loglevel error -i tone.wav -c:a libopus -b:a 96k tone.opus

# FLAC with an embedded cover picture: exercises embedded-art extraction. A tiny
# solid-red 16x16 PNG is attached as the front cover.
ffmpeg -y -hide_banner -loglevel error -f lavfi -i color=c=red:s=16x16:d=1 -frames:v 1 _cover.png
ffmpeg -y -hide_banner -loglevel error -i tone.wav -i _cover.png -t 0.2 \
  -map 0:a -map 1:v -c:a flac -c:v copy -disposition:v:0 attached_pic \
  -metadata title="Arted Track" arted.flac
rm -f _cover.png
```

Update the final `ls -la` line to include `tone.opus arted.flac`.

- [ ] **Step 2: Parameterize `generate_dsd.py` by rate and add a DSD128 call**

In `testdata/generate_dsd.py`, change the `RATE` constant into a CLI-overridable
value. Replace the `RATE = 2_822_400  # DSD64` line and the `main()` head:

```python
DSD64 = 2_822_400
# main() reads the rate from argv[2] (Hz) if given, else DSD64.
```

and in `main()`:

```python
def main() -> None:
    out = sys.argv[1] if len(sys.argv) > 1 else "tone.dsf"
    rate = int(sys.argv[2]) if len(sys.argv) > 2 else DSD64
    n = int(DUR * rate)
    t = np.arange(n) / rate
```

Replace every remaining use of the old `RATE` inside `main()` with the local
`rate` (the `struct.pack("<I", RATE)` in `fmt_chunk` becomes `rate`). Leave the
sigma-delta modulator untouched.

In `generate.sh`, replace the single DSD line with both rates:

```bash
python3 generate_dsd.py tone.dsf                 # DSD64
python3 generate_dsd.py tone-dsd128.dsf 5644800  # DSD128
```

- [ ] **Step 3: Regenerate the fixtures**

Run: `bash testdata/generate.sh`
Expected: prints "generated:" and lists the files including `tone.opus`,
`arted.flac`, `tone.dsf`, `tone-dsd128.dsf`.

- [ ] **Step 4: Write `app/src-tauri/tests/fixtures.rs` (validation)**

This asserts every fixture opens with the real decoder and reports the expected
duration, so a bad regenerate fails loudly. Uses the desktop decoder on a
big-stack thread (the `OwnedDecoder` is ~150 KiB).

```rust
//! Validates the committed testdata fixtures decode and report sane durations.
use std::path::PathBuf;

use soul_audio_core::SeekableDecoder;
use soul_audio_desktop::decoder::open_track;

fn testdata(name: &str) -> PathBuf {
    // app/src-tauri/tests -> repo-root/testdata
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata").join(name)
}

fn duration_secs(name: &str) -> f64 {
    let path = testdata(name);
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let d = open_track(&path).expect("open");
            let si = d.stream_info();
            let frames = d.total_frames().expect("declares length");
            frames as f64 / si.sample_rate as f64
        })
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn pcm_fixtures_are_two_seconds() {
    for (name, tol) in [
        ("tone.wav", 0.05),
        ("tone.flac", 0.05),
        ("tone.mp3", 0.15),
        ("tone.opus", 0.15),
        ("tone.m4a", 0.05),
        ("tone-aac.m4a", 0.15),
        ("tone.aac", 0.15),
    ] {
        let d = duration_secs(name);
        assert!((d - 2.0).abs() < tol, "{name}: {d}s not ~2.0s");
    }
}

#[test]
fn dsd_fixtures_are_half_second() {
    for name in ["tone.dsf", "tone-dsd128.dsf"] {
        let d = duration_secs(name);
        assert!((d - 0.5).abs() < 0.05, "{name}: {d}s not ~0.5s");
    }
}
```

Add `soul-audio-desktop` and `soul-audio-core` as `[dev-dependencies]` of
`app/src-tauri` if not already present (path deps into the root workspace —
mirror how the runtime deps reference them; check `Cargo.toml`).

- [ ] **Step 5: Run the fixture validation**

Run (inside `app/src-tauri`): `cargo test --test fixtures`
Expected: both tests pass — every fixture decodes with the right duration.
(`tone-aac.m4a` may report ~2.0 ± priming; the ±0.15 tolerance covers it.)

- [ ] **Step 6: Commit (fixtures are binary — commit them)**

```bash
git add testdata/generate.sh testdata/generate_dsd.py testdata/tone.opus testdata/arted.flac testdata/tone-dsd128.dsf app/src-tauri/tests/fixtures.rs app/src-tauri/Cargo.toml
git commit -m "test(fixtures): add Opus, embedded-art FLAC, and DSD128 fixtures + validation"
```

---

## Task 3: Tier 1 — Rust product-e2e cases

**Files:**
- Create: `app/src-tauri/tests/e2e.rs`

**Interfaces:**
- Consumes: `common::{mock_app, capture_events, wait_for_tick, write_wav}`, `commands::load_folder`, `commands::track_art`, `PlayerState`, the `testdata/` fixtures.

Each case sets up a mock app + event capture, drives commands, and polls Ticks.
Use a shared per-format fixture folder built by symlinking/copying the
`testdata/` tones into a tempdir (copy — symlinks are awkward on Windows).

- [ ] **Step 1: Write `tests/e2e.rs` scaffolding + a fixture-folder helper**

```rust
//! Tier-1 product-e2e: real actor/engine over NullSink, driven through the
//! app's command→event flow against the committed fixtures.
mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{capture_events, mock_app, wait_for_tick, write_wav};
use soul_audio_service::{Command, Event, QueueState};
use soul_player::actor::PlayerState;
use soul_player::commands::{load_folder, track_art};
use tauri::Manager;

fn testdata(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata").join(name)
}

/// Copy the named testdata files into a fresh tempdir library folder.
fn library_of(names: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for n in names {
        std::fs::copy(testdata(n), dir.path().join(n)).unwrap();
    }
    dir
}

const TIMEOUT: Duration = Duration::from_secs(8);
```

- [ ] **Step 2: Cases — write each `#[test]` (complete code)**

```rust
#[test]
fn per_format_durations_are_reported() {
    // Every supported container's TrackView carries the right duration.
    let dir = library_of(&[
        "tone.wav", "tone.flac", "tone.mp3", "tone.opus",
        "tone.m4a", "tone.aac", "tone.dsf", "tone-dsd128.dsf",
    ]);
    let app = mock_app();
    let state = app.state::<PlayerState>();
    let views = load_folder(state.inner(), app.handle(), dir.path()).unwrap();

    // Assert durations as a multiset (the library sorts by path, so we don't
    // rely on per-file ordering): two DSD @ ~0.5 s, six PCM @ ~2.0 s.
    let mut secs: Vec<f64> = views.iter().map(|v| v.duration_secs).collect();
    secs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    // Two DSD @ 0.5 s, six PCM @ 2.0 s.
    let dsd = secs.iter().filter(|d| (**d - 0.5).abs() < 0.06).count();
    let pcm = secs.iter().filter(|d| (**d - 2.0).abs() < 0.16).count();
    assert_eq!(dsd, 2, "two DSD fixtures ~0.5s: {secs:?}");
    assert_eq!(pcm, 6, "six PCM fixtures ~2.0s: {secs:?}");
}

#[test]
fn folder_switch_loads_the_new_library() {
    // libA tracks are 2.0 s; libB tracks are 1.0 s. After switching to libB and
    // it auto-plays, the now-playing duration must be libB's (1.0 s) — proving
    // the loader resolves ids against the NEW library, not the stale one.
    let a = tempfile::tempdir().unwrap();
    write_wav(a.path(), "a1.wav", 2.0);
    let b = tempfile::tempdir().unwrap();
    write_wav(b.path(), "b1.wav", 1.0);

    let app = mock_app();
    let events = capture_events(&app);
    let state = app.state::<PlayerState>();

    load_folder(state.inner(), app.handle(), a.path()).unwrap();
    wait_for_tick(&events, |t| dur_is(t, 2.0), TIMEOUT);

    load_folder(state.inner(), app.handle(), b.path()).unwrap();
    let t = wait_for_tick(&events, |t| dur_is(t, 1.0), TIMEOUT);
    assert!(dur_is(&t, 1.0), "after switch, duration is libB's 1.0s");
}

fn dur_is(t: &soul_audio_service::Tick, secs: f64) -> bool {
    t.duration_secs.map(|d| (d - secs).abs() < 0.1).unwrap_or(false)
}

#[test]
fn seek_advances_position() {
    let dir = library_of(&["tone.wav"]); // 2.0 s
    let app = mock_app();
    let events = capture_events(&app);
    let state = app.state::<PlayerState>();
    load_folder(state.inner(), app.handle(), dir.path()).unwrap();
    wait_for_tick(&events, |t| t.now_playing_id.is_some(), TIMEOUT);

    state.inner().send_command(app.handle(), Command::Seek { secs: 1.0 });
    let t = wait_for_tick(&events, |t| t.position_secs >= 1.0, TIMEOUT);
    assert!(t.position_secs >= 1.0 && t.position_secs <= 2.1);
}

#[test]
fn volume_is_set_and_clamped() {
    let dir = library_of(&["tone.wav"]);
    let app = mock_app();
    let events = capture_events(&app);
    let state = app.state::<PlayerState>();
    load_folder(state.inner(), app.handle(), dir.path()).unwrap();
    wait_for_tick(&events, |t| t.now_playing_id.is_some(), TIMEOUT);

    state.inner().send_command(app.handle(), Command::SetVolume(0.25));
    let t = wait_for_tick(&events, |t| (t.volume - 0.25).abs() < 1e-6, TIMEOUT);
    assert!((t.volume - 0.25).abs() < 1e-6);

    state.inner().send_command(app.handle(), Command::SetVolume(2.0));
    let t = wait_for_tick(&events, |t| (t.volume - 1.0).abs() < 1e-6, TIMEOUT);
    assert_eq!(t.volume, 1.0);

    state.inner().send_command(app.handle(), Command::SetVolume(-1.0));
    let t = wait_for_tick(&events, |t| (t.volume - 0.0).abs() < 1e-6, TIMEOUT);
    assert_eq!(t.volume, 0.0);
}

#[test]
fn repeat_and_shuffle_reflect() {
    let dir = library_of(&["tone.wav"]);
    let app = mock_app();
    let events = capture_events(&app);
    let state = app.state::<PlayerState>();
    load_folder(state.inner(), app.handle(), dir.path()).unwrap();
    wait_for_tick(&events, |t| t.now_playing_id.is_some(), TIMEOUT);

    state.inner().send_command(app.handle(), Command::SetRepeat(soul_audio_service::RepeatMode::All));
    wait_for_tick(&events, |t| t.repeat == soul_audio_service::RepeatMode::All, TIMEOUT);
    state.inner().send_command(app.handle(), Command::SetShuffle(true));
    wait_for_tick(&events, |t| t.shuffle, TIMEOUT);
}

#[test]
fn queue_ops_update_up_next() {
    // Build a 3-track library; Enqueue then read the QueueState.
    let dir = tempfile::tempdir().unwrap();
    write_wav(dir.path(), "1.wav", 2.0);
    write_wav(dir.path(), "2.wav", 2.0);
    write_wav(dir.path(), "3.wav", 2.0);
    let app = mock_app();
    let events = capture_events(&app);
    let state = app.state::<PlayerState>();
    let views = load_folder(state.inner(), app.handle(), dir.path()).unwrap();

    state.inner().send_command(app.handle(), Command::Enqueue(views[2].id));
    let q = wait_for_queue(&events, |q| q.up_next_ids.contains(&views[2].id), TIMEOUT);
    assert!(q.up_next_ids.contains(&views[2].id));

    state.inner().send_command(app.handle(), Command::ClearQueue);
    wait_for_queue(&events, |q| q.up_next_ids.is_empty(), TIMEOUT);
}

fn wait_for_queue(
    events: &std::sync::Arc<std::sync::Mutex<Vec<Event>>>,
    pred: impl Fn(&QueueState) -> bool,
    timeout: Duration,
) -> QueueState {
    let start = std::time::Instant::now();
    loop {
        if let Some(q) = events.lock().unwrap().iter().rev().find_map(|e| match e {
            Event::Queue(q) if pred(q) => Some(q.clone()),
            _ => None,
        }) {
            return q;
        }
        assert!(start.elapsed() < timeout, "timed out waiting for QueueState");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn track_art_resolves_embedded_and_missing() {
    let dir = library_of(&["arted.flac", "tone.wav"]);
    let app = mock_app();
    let state = app.state::<PlayerState>();
    let views = load_folder(state.inner(), app.handle(), dir.path()).unwrap();

    // The arted.flac view has_art; its art resolves to a data URL.
    let arted = views.iter().find(|v| v.has_art).expect("arted has_art");
    let url = track_art(app.state::<PlayerState>(), arted.id).unwrap();
    assert!(url.unwrap().starts_with("data:image/"));

    // The plain WAV has no art.
    let plain = views.iter().find(|v| !v.has_art).expect("wav artless");
    assert_eq!(track_art(app.state::<PlayerState>(), plain.id).unwrap(), None);

    // Unknown id → None.
    assert_eq!(track_art(app.state::<PlayerState>(), 9999).unwrap(), None);
}
```

> Note on `library_views` mirroring and IPC-boundary coverage: add one case that
> drives `track_art` through `tauri::test::get_ipc_response` (serialized args) to
> cover the IPC boundary once — see the research snippet in the spec. Keep the
> rest calling the fns directly.

- [ ] **Step 3: Run Tier 1**

Run (inside `app/src-tauri`): `cargo test --test e2e`
Expected: all cases pass. Investigate any timeout via the Step-7/Task-1 fallback
(event delivery) before loosening assertions.

- [ ] **Step 4: Commit**

```bash
git add app/src-tauri/tests/e2e.rs
git commit -m "test(app): Tier-1 product-e2e — durations, folder-switch, seek, volume, queue, art"
```

---

## Task 4: Tier 2 — Vitest browser-mode config spike

**Files:**
- Modify: `app/package.json`
- Create: `app/vitest.config.ts`, `app/src/lib/__fixtures__/Smoke.svelte`, `app/src/lib/__fixtures__/smoke.svelte.test.ts`

**Why a spike first:** the app is on Vite 8; the Vitest + `vitest-browser-svelte`
+ Playwright toolchain must resolve to a Vite-8-compatible set. Prove one green
browser test before writing real ones.

- [ ] **Step 1: Add devDeps + scripts to `app/package.json`**

Add to `devDependencies` (install with `npm install`, letting npm pick the
latest Vite-8-compatible versions; pin what resolves):

```
"vitest": "latest",
"@vitest/browser": "latest",
"vitest-browser-svelte": "latest",
"playwright": "latest",
"@tauri-apps/api": "^2.11.1"   // already present; ensure >= 2.7 for shouldMockEvents
```

Add scripts:

```
"test": "vitest run",
"test:watch": "vitest"
```

Then run `npx playwright install chromium`.

> If the toolchain cannot resolve against Vite 8, report BLOCKED with the npm
> error; do not downgrade the app's Vite. Options to try, in order: newer
> `vitest`/`@vitest/browser` majors; `@sveltejs/vite-plugin-svelte` is already
> v7 (Vite-8-capable).

- [ ] **Step 2: Create `app/vitest.config.ts`**

```ts
import { defineConfig } from 'vitest/config';
import { svelte } from '@sveltejs/vite-plugin-svelte';

export default defineConfig({
  plugins: [svelte()],
  test: {
    projects: [
      {
        extends: true,
        test: {
          name: 'client',
          include: ['src/**/*.svelte.{test,spec}.{js,ts}'],
          browser: {
            enabled: true,
            provider: 'playwright',
            headless: true,
            instances: [{ browser: 'chromium' }],
          },
        },
      },
      {
        extends: true,
        test: {
          name: 'node',
          include: ['src/**/*.{test,spec}.{js,ts}'],
          exclude: ['src/**/*.svelte.{test,spec}.{js,ts}'],
          environment: 'node',
        },
      },
    ],
  },
});
```

- [ ] **Step 3: Create a trivial component + browser test**

`app/src/lib/__fixtures__/Smoke.svelte`:

```svelte
<script lang="ts">
  let { label = 'smoke' }: { label?: string } = $props();
  let n = $state(0);
</script>

<button onclick={() => (n += 1)}>{label}: {n}</button>
```

`app/src/lib/__fixtures__/smoke.svelte.test.ts`:

```ts
import { expect, test } from 'vitest';
import { render } from 'vitest-browser-svelte';
import Smoke from './Smoke.svelte';

test('renders and reacts to a rune click', async () => {
  const screen = render(Smoke, { label: 'hi' });
  const btn = screen.getByRole('button');
  await expect.element(btn).toHaveTextContent('hi: 0');
  await btn.click();
  await expect.element(btn).toHaveTextContent('hi: 1');
});
```

- [ ] **Step 4: Run the spike**

Run (inside `app/`): `npm test`
Expected: the `client` (browser) project runs the smoke test green; the `node`
project reports no tests. If green, the toolchain is proven.

- [ ] **Step 5: Commit**

```bash
git add app/package.json app/package-lock.json app/vitest.config.ts app/src/lib/__fixtures__
git commit -m "test(ui): Vitest browser-mode toolchain + smoke test"
```

---

## Task 5: Tier 2 — Frontend logic/component tests

**Files:**
- Create: `app/src/lib/art-cache.svelte.test.ts`, `app/src/lib/ipc.test.ts`, and component tests `app/src/views/PlayerBar.svelte.test.ts`, `app/src/views/Library.svelte.test.ts`, plus a player-store test (path per the store's actual location — read `App.svelte`/`ipc.ts` to find where Tick/Queue state lives).

**Interfaces:**
- Consumes: `@tauri-apps/api/mocks` (`mockIPC`, `clearMocks`), `vitest-browser-svelte` `render`, the app's own `ipc.ts`, `art-cache.svelte.ts`, and views.

**Read first:** the implementer must read `app/src/lib/ipc.ts`,
`app/src/lib/art-cache.svelte.ts`, `app/src/views/PlayerBar.svelte`,
`app/src/views/Library.svelte`, and `app/src/App.svelte` to bind tests to the
real exports, selectors, and store shape. The cases below are the required
coverage; adapt names/selectors to the actual code.

- [ ] **Step 1: `ipc.ts` wrapper tests (node project) via `mockIPC`**

Cover: `trackArt(id)` returns the data URL the backend sends; `openFolder()`
returns `null` when the mocked `open_folder` throws "no folder selected";
`sendCommand(cmd)` forwards the command name/args to `invoke`. Include the jsdom
WebCrypto note only if run in node env (browser project has real crypto).

```ts
import { afterEach, expect, test, vi } from 'vitest';
import { mockIPC, clearMocks } from '@tauri-apps/api/mocks';
import * as ipc from './ipc';

afterEach(() => clearMocks());

test('trackArt returns the backend data URL', async () => {
  mockIPC((cmd, args) => {
    if (cmd === 'track_art') return 'data:image/png;base64,AAAA';
  });
  await expect(ipc.trackArt(1)).resolves.toBe('data:image/png;base64,AAAA');
});

test('sendCommand forwards to invoke', async () => {
  const seen: any[] = [];
  mockIPC((cmd, args) => { seen.push([cmd, args]); });
  await ipc.sendCommand({ TogglePlay: undefined } as any); // adapt to real Command shape
  expect(seen.some(([c]) => c === 'send_command')).toBe(true);
});
```

- [ ] **Step 2: `art-cache` tests (browser project, `.svelte.test.ts`)**

Cover: keys by `album ?? String(id)`; a second request for the same key does not
re-invoke; the `inView` action gates the fetch until visible. Use `mockIPC` to
count `track_art` calls.

- [ ] **Step 3: Player-store test — applying synthetic events**

Feed a `Tick` and a `Queue` event through the app's event subscription
(`onPlayerEvent`) using `mockIPC(() => {}, { shouldMockEvents: true })` +
`emit('player', …)`, and assert the store's now-playing/position/duration/volume
and up-next update.

- [ ] **Step 4: `PlayerBar` seek-hold test (browser project)**

Render `PlayerBar`, scrub the seek input, assert `seekHold` holds the displayed
position for 300 ms and clears on `nowPlayingId` change — drive time with
`vi.useFakeTimers()`.

- [ ] **Step 5: `Library` render test (browser project)**

Render `Library` with a few `TrackView`s; assert one row per track and that a
row with `has_art=false` does not request art (mock `track_art`, assert not
called for it).

- [ ] **Step 6: Run Tier 2 + commit**

Run (inside `app/`): `npm test`
Expected: all client + node tests pass.

```bash
git add app/src
git commit -m "test(ui): Tier-2 frontend tests — ipc, art-cache, player store, seek-hold, library"
```

---

## Task 6: Tier 3 — WebDriver UI smoke (local)

**Files:**
- Create: `app/e2e/package.json`, `app/e2e/wdio.conf.ts`, `app/e2e/tsconfig.json`, `app/e2e/test/specs/player.e2e.ts`, `app/e2e/README.md`
- Modify: `app/package.json` (an `e2e` convenience script)

**Prerequisite (documented in README):** `cargo install tauri-driver --locked`;
on Windows a matching `msedgedriver.exe` on PATH (`cargo install --git
https://github.com/chippers/msedgedriver-tool` then run it). Not run in CI.

- [ ] **Step 1: `app/e2e/package.json`**

```json
{
  "name": "soul-player-e2e",
  "private": true,
  "type": "module",
  "scripts": { "test": "wdio run ./wdio.conf.ts" },
  "devDependencies": {
    "@wdio/cli": "^9.19.0",
    "@wdio/local-runner": "^9.19.0",
    "@wdio/mocha-framework": "^9.19.0",
    "@wdio/spec-reporter": "^9.19.0",
    "ts-node": "^10",
    "typescript": "~6.0.3"
  }
}
```

- [ ] **Step 2: `app/e2e/wdio.conf.ts`**

```ts
import os from 'os';
import path from 'path';
import { spawn, spawnSync, type ChildProcess } from 'child_process';
import { fileURLToPath } from 'url';

const __dirname = fileURLToPath(new URL('.', import.meta.url));
const APP = path.resolve(__dirname, '../src-tauri/target/debug/soul-player.exe');
let tauriDriver: ChildProcess | undefined;
let exiting = false;

export const config: WebdriverIO.Config = {
  host: '127.0.0.1',
  port: 4444,
  specs: ['./test/specs/**/*.e2e.ts'],
  maxInstances: 1,
  capabilities: [{ maxInstances: 1, 'tauri:options': { application: APP } } as any],
  reporters: ['spec'],
  framework: 'mocha',
  mochaOpts: { ui: 'bdd', timeout: 120000 },
  autocompileOpts: { autoCompile: true, tsNodeOpts: { transpileOnly: true } },

  // Build the debug, unbundled binary WITH the e2e feature (load_folder_path)
  // and run the actor device-free.
  onPrepare: () => {
    process.env.SOUL_PLAYER_OFFLINE = '1';
    const r = spawnSync(
      'npm',
      ['run', 'tauri', 'build', '--', '--debug', '--no-bundle', '--features', 'e2e'],
      { cwd: path.resolve(__dirname, '..'), stdio: 'inherit', shell: true },
    );
    if (r.status !== 0) throw new Error('tauri build failed');
  },
  beforeSession: () => {
    tauriDriver = spawn(path.resolve(os.homedir(), '.cargo', 'bin', 'tauri-driver'), [], {
      stdio: [null, process.stdout, process.stderr],
      env: { ...process.env, SOUL_PLAYER_OFFLINE: '1' },
    });
    tauriDriver.on('exit', (code) => {
      if (!exiting) { console.error('tauri-driver exited early:', code); process.exit(1); }
    });
  },
  afterSession: () => { exiting = true; tauriDriver?.kill(); },
};
```

- [ ] **Step 3: `app/e2e/tsconfig.json`**

```json
{ "compilerOptions": { "module": "ESNext", "moduleResolution": "Bundler", "target": "ES2022", "types": ["@wdio/globals/types", "@wdio/mocha-framework"], "strict": true } }
```

- [ ] **Step 4: `app/e2e/test/specs/player.e2e.ts`**

Injects the fixture folder via the `e2e` command (WebDriver can't drive the OS
dialog), then asserts the UI. Point `FIXTURE` at a repo folder of tones; use
forward slashes or escape backslashes for the JSON arg.

```ts
import path from 'path';

const FIXTURE = path.resolve(process.cwd(), '../../testdata').replace(/\\/g, '/');

describe('soul-player smoke', () => {
  it('renders the shell', async () => {
    await expect($('body')).toBeExisting();
  });

  it('loads a folder and shows tracks + transport', async () => {
    await browser.execute((dir) => {
      // @ts-ignore — page context
      return window.__TAURI_INTERNALS__.invoke('load_folder_path', { dir });
    }, FIXTURE);

    // Library rows appear (adapt the selector to Library.svelte's markup).
    await browser.waitUntil(async () => (await $$('[data-testid="track-row"]')).length > 0, {
      timeout: 10000,
      timeoutMsg: 'no track rows rendered after load_folder_path',
    });

    // Now-playing title shows in the player bar (adapt selector).
    await expect($('[data-testid="now-playing-title"]')).toBeExisting();
  });

  it('clicking a row changes now-playing', async () => {
    const rows = await $$('[data-testid="track-row"]');
    const title = await $('[data-testid="now-playing-title"]');
    const before = await title.getText();
    await rows[rows.length - 1].click();
    await browser.waitUntil(async () => (await title.getText()) !== before, { timeout: 5000 });
  });
});
```

> The selectors above assume `data-testid` hooks. Add them to `Library.svelte`
> (each row) and `PlayerBar.svelte` (now-playing title) as part of this task —
> the minimal, non-invasive way to make the UI addressable. Keep them stable.

- [ ] **Step 5: `app/e2e/README.md`** — document the prerequisites (tauri-driver,
  msedgedriver-tool on Windows), how to run (`npm install` then `npm test` in
  `app/e2e/`), and that it is local-only.

- [ ] **Step 6: Add `app/package.json` convenience script**

`"e2e": "cd e2e && npm test"`

- [ ] **Step 7: Run locally (developer machine with the prereqs) + commit**

Run (inside `app/e2e/`): `npm install` then `npm test`
Expected: the app builds with `--features e2e`, launches under tauri-driver, and
the three specs pass. (If tauri-driver/msedgedriver are absent, this step is a
manual/local gate — commit the suite regardless; CI does not run it.)

```bash
git add app/e2e app/package.json app/src/views/Library.svelte app/src/views/PlayerBar.svelte
git commit -m "test(e2e): local WebDriver UI smoke suite (WDIO + tauri-driver)"
```

---

## Task 7: CI — wire Tiers 1 & 2, ship the disabled Windows job

**Files:**
- Modify: `.github/workflows/ci.yml`
- Create: `.github/workflows/webdriver.yml.disabled`

- [ ] **Step 1: Add an `app` (Rust product-e2e) job to `ci.yml`**

Append under `jobs:` (the app crate is a separate workspace, so it needs its own
step; `SOUL_PLAYER_OFFLINE=1` keeps it device-free):

```yaml
  app:
    name: app (soul-player rust tests)
    runs-on: ubuntu-latest
    env:
      SOUL_PLAYER_OFFLINE: "1"
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
        with:
          workspaces: app/src-tauri
      - name: Tauri Linux deps
        run: sudo apt-get update && sudo apt-get install -y libwebkit2gtk-4.1-dev libayatana-appindicator3-dev
      - name: cargo test (app)
        working-directory: app/src-tauri
        run: cargo test
```

- [ ] **Step 2: Add a `frontend` (Vitest) job to `ci.yml`**

```yaml
  frontend:
    name: frontend (vitest)
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with:
          node-version: 24
      - name: install
        working-directory: app
        run: npm ci
      - name: playwright chromium
        working-directory: app
        run: npx playwright install --with-deps chromium
      - name: vitest
        working-directory: app
        run: npm test
```

- [ ] **Step 3: Ship the disabled Windows WebDriver workflow**

Create `.github/workflows/webdriver.yml.disabled` (the `.disabled` suffix means
GitHub/Forgejo Actions ignore it; rename to `.yml` to enable when a Windows
runner with WebView2 exists). Contents: a `windows-latest` job that installs
`msedgedriver-tool`, `cargo install tauri-driver --locked`, builds `--features
e2e`, and runs `app/e2e`'s `npm test`. Base it on the official Tauri webdriver CI
(see the spec's research references).

- [ ] **Step 4: Verify the YAML is well-formed**

Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml'))"`
Expected: no error. (CI itself runs on push.)

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/ci.yml .github/workflows/webdriver.yml.disabled
git commit -m "ci: run app (rust e2e) + frontend (vitest); ship disabled windows webdriver job"
```

---

## Self-review notes (for the executor)

- **Event delivery over MockRuntime** is the top risk (Task 1, Step 7). Verify
  it before building Tier-1 on top; use the documented fallback if needed.
- **Fixture tooling** (ffmpeg/opus, python/numpy) is a hard prerequisite for
  Task 2 — BLOCKED, not faked, if missing.
- **Vite 8 toolchain resolution** for Vitest is the Task-4 spike's whole point;
  do it before Task 5.
- **`data-testid` hooks** for Tier 3 are added in Task 6; keep them stable and
  minimal.
- Every task ends green and committed; run the relevant `cargo test`/`npm test`
  before committing, and never add AI trailers to commit messages.
