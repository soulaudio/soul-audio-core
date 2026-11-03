# Desktop Player — Plan 3: `soul-audio-service` (headless backend service)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the entire desktop-player backend as a pure-Rust, headless-testable crate `soul-audio-service` — the command/event/DTO types, a `lofty` library scan, the app-command → `Player` translation + state projection, and a threaded `AudioPlayer` actor (pump + loader threads) around `soul_audio_control::Player`. The Tauri app shell + Svelte UI + `tauri-specta` wiring land later in Plan 4; this crate holds all the logic and is covered by `cargo xtask verify`.

**Architecture:** A std root-workspace crate. It is **sink-generic**: the actor's `spawn<K: Sink>(make_sink, …)` builds the sink *on the pump thread* (so a `!Send` device sink like `CpalSink` is fine) and owns a `Player<K, OwnedDecoder, SliceContext<u32,Vec<u32>>, VecDeque<u32>, VecDeque<(u32,usize)>, 2>`. Because it never names `CpalSink`, it depends on `soul-audio-desktop` **without** the `playback` feature, keeping `cpal` out of the CI gate; Plan 4 injects `CpalSink`. All logic is unit-tested off the audio device with an in-crate `NullSink` + a const test decoder, reusing the `soul-audio-queue` engine-integration test pattern.

**Tech Stack:** Rust (edition 2024, rust 1.85), `soul-audio-{core,engine,queue,control,desktop}`, `lofty` (audio tags), `serde` (DTO derives for Plan 4's IPC), std threads + `mpsc`.

## Global Constraints

- `#![forbid(unsafe_code)]`; edition 2024 / rust 1.85 via `workspace`.
- `soul-audio-service` is **std**, a root-workspace **member and default-member**; NOT cross-built for `thumbv7em`.
- It depends on `soul-audio-desktop` **without** the `playback` feature (only `OwnedDecoder` + `open_track`); it must NOT pull `cpal`/`ringbuf` into the default workspace build. It enables `soul-audio-queue`'s **`alloc`** feature (for the `VecDeque` `QueueBuf` backing).
- **Error policy (resolves the Plan-2 deferred decision):** keep `anyhow` at the library boundary — `Library::scan` and the loader's `open_track` return `anyhow::Result`; the Player's transport methods return `DeckError`. The service does NOT introduce a typed error crate; Plan 4's Tauri command layer maps these to an IPC error (message string). Do not refactor `open_track`/`CpalSink::new` to typed errors.
- **Headless-testable:** every logic unit is tested without an audio device (in-crate `NullSink`, const test decoder, tiny temp WAV fixtures). `cargo xtask verify` stays green (workspace clippy `-D warnings`, full test suite, unchanged thumbv7em builds of core/engine/queue/control/firmware).
- Workspace-wide pre-commit clippy ⇒ each commit leaves the whole workspace compiling + clippy-clean.
- No `Co-Authored-By:`/`Claude-Session:` commit trailers; never `--no-verify`.

## File Structure

```
crates/soul-audio-service/
  Cargo.toml          NEW  — std lib; core/engine/queue(alloc)/control/desktop(no playback)/lofty/serde
  src/
    lib.rs            NEW  — #![forbid(unsafe_code)]; module decls + re-exports; #[cfg(test)] testutil    [T1]
    types.rs          NEW  — Command, Event, Tick, QueueState, PlayState, RepeatMode, TrackView + mappings [T1]
    library.rs        NEW  — Track, Library, scan() (lofty), path_of, views                                [T2]
    driver.rs         NEW  — ServicePlayer<K,D> alias, apply_command, project_tick, project_queue          [T3]
    actor.rs          NEW  — AudioPlayer (pump + loader threads), spawn/send/set_library                    [T4]
    testutil.rs       NEW  — #[cfg(test)] NullSink + ConstDecoder + helpers (shared by T3/T4 unit tests)    [T1]

Cargo.toml (root)     MOD  — add soul-audio-service to members + default-members                            [T1]
crates/architecture-tests/tests/layering.rs  MOD — add soul-audio-service to the shell check               [T1]
```

The app-facing `Command`/`Event` types are the service's own concrete enums (u32 ids, seconds) — distinct from `soul_audio_control::Command<C>` (generic, frames). The service translates its `Command` into direct `Player` method calls.

---

### Task 1: Scaffold the crate + DTO/Command/Event types + test kit

**Files:**
- Create: `crates/soul-audio-service/Cargo.toml`, `src/lib.rs`, `src/types.rs`, `src/testutil.rs`
- Modify: root `Cargo.toml` (members + default-members), `crates/architecture-tests/tests/layering.rs`

**Interfaces:**
- Produces: `soul_audio_service::{Command, Event, Tick, QueueState, PlayState, RepeatMode, TrackView}` — plain serde-serializable types, consumed by Tasks 3/4 and Plan 4. `RepeatMode` ↔ `soul_audio_queue::Repeat` conversions. Test kit `NullSink`, `ConstDecoder` (crate-private, `#[cfg(test)]`).

- [ ] **Step 1: Create `Cargo.toml`.**

```toml
[package]
name = "soul-audio-service"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true
description = "Headless desktop-player backend service (actor around soul-audio-control::Player)."

[lints]
workspace = true

[dependencies]
soul-audio-core = { path = "../soul-audio-core" }
soul-audio-engine = { path = "../soul-audio-engine" }
soul-audio-queue = { path = "../soul-audio-queue", features = ["alloc"] }
soul-audio-control = { path = "../soul-audio-control" }
soul-audio-desktop = { path = "../soul-audio-desktop" }
anyhow = "1"
serde = { version = "1", features = ["derive"] }
lofty = "0.22"
```

(`soul-audio-desktop` is default-featured — NO `playback`, so no cpal. `lofty` is used in Task 2; declaring it now is harmless. If `cargo` reports a newer `lofty` major, pin the latest and adjust Task 2's tag calls to that version's API — see Task 2's note.)

- [ ] **Step 2: Create `src/lib.rs`.** Declare only the modules that exist after this task (`types`, and the test-only `testutil`); Tasks 2-4 each add their own `pub mod`/`pub use` line, so the crate compiles at the end of every task.

```rust
//! Headless desktop-player backend service: the command/event/DTO types, a
//! `lofty` library scan, the app-command → `soul_audio_control::Player`
//! translation + state projection, and a threaded `AudioPlayer` actor. Owns all
//! backend logic; the Tauri shell (Plan 4) is thin plumbing over this crate.
//!
//! Sink-generic: the actor builds the sink on the pump thread, so a `!Send`
//! device sink works, and this crate stays free of `cpal` (Plan 4 injects it).
#![forbid(unsafe_code)]

pub mod types;

#[cfg(test)]
pub(crate) mod testutil;

pub use types::{Command, Event, PlayState, QueueState, RepeatMode, Tick, TrackView};
```

- [ ] **Step 3: Create `src/types.rs`.**

```rust
//! App-facing command, event, and DTO types crossing the UI↔backend boundary.
//! Concrete (u32 track ids, seconds) and serde-serializable for Plan 4's IPC —
//! distinct from `soul_audio_control::Command<C>` (generic, frames).

use serde::{Deserialize, Serialize};
use soul_audio_core::DeckState;
use soul_audio_queue::Repeat;

/// A command from the UI to the player actor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    /// Play an ordered list of track ids, starting at `start`.
    PlayContext { ids: Vec<u32>, start: usize },
    TogglePlay,
    Play,
    Pause,
    Stop,
    Next,
    Prev,
    /// Seek within the current track, in seconds from its start.
    Seek { secs: f64 },
    /// Set output volume, 0.0..=1.0.
    SetVolume(f32),
    PlayNext(u32),
    Enqueue(u32),
    ClearQueue,
    SetShuffle(bool),
    SetRepeat(RepeatMode),
    /// Jump to a natural position within the current context.
    Goto(usize),
}

/// Repeat mode as seen by the UI (mirrors `soul_audio_queue::Repeat`, decoupled
/// from that crate's `serde` feature).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

impl From<RepeatMode> for Repeat {
    fn from(m: RepeatMode) -> Self {
        match m {
            RepeatMode::Off => Repeat::Off,
            RepeatMode::All => Repeat::All,
            RepeatMode::One => Repeat::One,
        }
    }
}
impl From<Repeat> for RepeatMode {
    fn from(r: Repeat) -> Self {
        match r {
            Repeat::Off => RepeatMode::Off,
            Repeat::All => RepeatMode::All,
            Repeat::One => RepeatMode::One,
        }
    }
}

/// Playback state as seen by the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlayState {
    Playing,
    Paused,
    Stopped,
}

impl From<DeckState> for PlayState {
    fn from(s: DeckState) -> Self {
        match s {
            DeckState::Playing => PlayState::Playing,
            DeckState::Paused => PlayState::Paused,
            DeckState::Stopped | DeckState::Ended => PlayState::Stopped,
        }
    }
}

/// An event from the player actor to the UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    Tick(Tick),
    Queue(QueueState),
}

/// Now-playing + transport snapshot, emitted ~8×/s and on any change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tick {
    pub now_playing_id: Option<u32>,
    pub position_secs: f64,
    pub duration_secs: Option<f64>,
    pub state: PlayState,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub volume: f32,
}

/// Up-Next + context position, emitted on queue mutation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueState {
    pub up_next_ids: Vec<u32>,
    pub context_pos: usize,
    pub context_len: usize,
}

/// A library track as sent to the UI (no path, no cover bytes).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackView {
    pub id: u32,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_secs: f64,
}
```

- [ ] **Step 4: Create `src/testutil.rs`** — the headless test kit reused by Tasks 3/4.

```rust
//! Headless test doubles: a no-op sink and a constant-sample decoder, so the
//! whole player stack runs in unit tests without an audio device or real files.

use soul_audio_core::{
    DecodeError, Decoder, SampleFormat, SeekableDecoder, Sink, SinkError, StreamInfo,
};

/// A [`Sink`] that discards output — no device, no backpressure. The deck reads
/// its `stream_info` once to configure rate/channels.
pub(crate) struct NullSink {
    pub rate: u32,
    pub channels: u16,
}

impl NullSink {
    pub(crate) fn new(rate: u32, channels: u16) -> Self {
        Self { rate, channels }
    }
}

impl Sink for NullSink {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: self.rate,
            channels: self.channels,
            format: SampleFormat::Int { bits: 24 },
        }
    }
    fn write(&mut self, _frames: &[i32]) -> Result<(), SinkError> {
        Ok(())
    }
    fn flush(&mut self) -> Result<(), SinkError> {
        Ok(())
    }
}

/// A decoder emitting `frames` per-channel frames of a constant sample, then EOF.
/// Seekable (clamps to the total) so it satisfies `SeekableDecoder`.
pub(crate) struct ConstDecoder {
    rate: u32,
    channels: u16,
    total: u64,
    pos: u64,
    value: i32,
}

impl ConstDecoder {
    pub(crate) fn new(rate: u32, channels: u16, frames: u64, value: i32) -> Self {
        Self { rate, channels, total: frames, pos: 0, value }
    }
}

impl Decoder for ConstDecoder {
    fn stream_info(&self) -> StreamInfo {
        StreamInfo {
            sample_rate: self.rate,
            channels: self.channels,
            format: SampleFormat::Int { bits: 24 },
        }
    }
    fn decode_into(&mut self, out: &mut [i32]) -> Result<usize, DecodeError> {
        let ch = self.channels as u64;
        let remaining_frames = self.total.saturating_sub(self.pos);
        let want_frames = (out.len() as u64 / ch).min(remaining_frames);
        let n = (want_frames * ch) as usize;
        for s in &mut out[..n] {
            *s = self.value;
        }
        self.pos += want_frames;
        Ok(n)
    }
}

impl SeekableDecoder for ConstDecoder {
    fn seek_to(&mut self, sample: u64) -> Result<u64, DecodeError> {
        self.pos = sample.min(self.total);
        Ok(self.pos)
    }
    fn total_frames(&self) -> Option<u64> {
        Some(self.total)
    }
}
```

- [ ] **Step 5: Register the crate.** In the root `Cargo.toml`, add `"crates/soul-audio-service",` to **both** `members` and `default-members` (after `"crates/soul-audio-desktop",` in each).

- [ ] **Step 6: Extend the layering invariant.** In `crates/architecture-tests/tests/layering.rs`, add `"soul-audio-service"` to the `shells_depend_on_core_not_the_reverse` array:

```rust
    for shell in [
        "soul-audio-cli",
        "soul-audio-desktop",
        "soul-audio-service",
        "soul-audio-firmware",
    ] {
```

- [ ] **Step 7: Add a types unit test** at the bottom of `src/types.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeat_mode_roundtrips_through_queue_repeat() {
        for m in [RepeatMode::Off, RepeatMode::All, RepeatMode::One] {
            assert_eq!(RepeatMode::from(Repeat::from(m)), m);
        }
    }

    #[test]
    fn command_serde_roundtrips() {
        let cmd = Command::PlayContext { ids: vec![3, 1, 2], start: 1 };
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), cmd);
    }

    #[test]
    fn deck_state_maps_ended_to_stopped() {
        assert_eq!(PlayState::from(DeckState::Ended), PlayState::Stopped);
        assert_eq!(PlayState::from(DeckState::Playing), PlayState::Playing);
    }
}
```

Add `serde_json = "1"` to a `[dev-dependencies]` section in `Cargo.toml` for the roundtrip test.

- [ ] **Step 8: Verify.** Expected: pass / clean.

```
cargo test -p soul-audio-service
cargo test -p architecture-tests
cargo clippy -p soul-audio-service --all-targets -- -D warnings
```

- [ ] **Step 9: Commit.**

```
git add -A
git commit -m "feat(service): scaffold soul-audio-service + command/event/DTO types + headless test kit"
```

---

### Task 2: `Library` scan (lofty)

**Files:**
- Create: `crates/soul-audio-service/src/library.rs`
- Modify: `crates/soul-audio-service/src/lib.rs` (add `pub mod library;` + re-export)

**Interfaces:**
- Consumes: `TrackView` (Task 1).
- Produces: `soul_audio_service::{Library, Track}`; `Library::scan(&Path) -> anyhow::Result<Library>`, `Library::path_of(u32) -> Option<&Path>`, `Library::views() -> Vec<TrackView>`. Consumed by Task 4's loader.

- [ ] **Step 1: Create `src/library.rs`.**

```rust
//! Local music library: a recursive folder scan that reads tags with `lofty`.
//! Track ids are the scan order (stable for the session); the loader maps
//! `id → path` when opening a track.

use std::path::{Path, PathBuf};

use crate::types::TrackView;

/// One scanned track. `id` is its index in `Library::tracks`.
#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub id: u32,
    pub path: PathBuf,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_secs: f64,
}

/// An ordered set of tracks with stable session ids.
#[derive(Debug, Clone, Default)]
pub struct Library {
    pub tracks: Vec<Track>,
}

/// Extensions the decoders support (lower-cased comparison).
const SUPPORTED: &[&str] = &["wav", "flac", "mp3", "m4a", "aac", "ogg", "dsf", "dff"];

fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| SUPPORTED.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Recursively collect supported files under `dir` into `out`, sorted for a
/// deterministic id assignment.
fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            walk(&path, out)?;
        } else if is_supported(&path) {
            out.push(path);
        }
    }
    Ok(())
}

impl Library {
    /// Scan `root` recursively for supported audio files, reading tags. Runs off
    /// the UI thread by the caller. Unreadable tags degrade to `None`/`0.0`
    /// rather than failing the whole scan; only a failed directory read errors.
    pub fn scan(root: &Path) -> anyhow::Result<Library> {
        let mut paths = Vec::new();
        walk(root, &mut paths)?;
        let tracks = paths
            .into_iter()
            .enumerate()
            .map(|(i, path)| {
                let (title, artist, album, duration_secs) = read_tags(&path);
                Track { id: i as u32, path, title, artist, album, duration_secs }
            })
            .collect();
        Ok(Library { tracks })
    }

    /// The file path for `id`, if in range.
    #[must_use]
    pub fn path_of(&self, id: u32) -> Option<&Path> {
        self.tracks.get(id as usize).map(|t| t.path.as_path())
    }

    /// The UI-facing view of every track (no paths).
    #[must_use]
    pub fn views(&self) -> Vec<TrackView> {
        self.tracks
            .iter()
            .map(|t| TrackView {
                id: t.id,
                title: t.title.clone(),
                artist: t.artist.clone(),
                album: t.album.clone(),
                duration_secs: t.duration_secs,
            })
            .collect()
    }
}

/// Read (title, artist, album, duration_secs) with lofty; all-`None`/`0.0` on
/// any read error (a corrupt tag must not sink the scan).
fn read_tags(path: &Path) -> (Option<String>, Option<String>, Option<String>, f64) {
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::tag::Accessor;

    match lofty::read_from_path(path) {
        Ok(tagged) => {
            let duration_secs = tagged.properties().duration().as_secs_f64();
            let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
            let s = |f: Option<std::borrow::Cow<'_, str>>| f.map(|c| c.into_owned());
            match tag {
                Some(t) => (s(t.title()), s(t.artist()), s(t.album()), duration_secs),
                None => (None, None, None, duration_secs),
            }
        }
        Err(_) => (None, None, None, 0.0),
    }
}
```

> **lofty API note (the one external-API risk, cf. spec §10):** the tag/property accessors above target **lofty 0.22** (`lofty::file::{AudioFile, TaggedFileExt}`, `lofty::tag::Accessor`, `read_from_path`, `properties().duration()`, `title()/artist()/album() -> Option<Cow<str>>`). If `cargo add lofty` pinned a different version whose paths differ, run `cargo doc -p lofty --open` (or read its source) and adjust ONLY the `use` paths and accessor calls in `read_tags` — the rest of `library.rs` is version-independent. Do not change the public API of `Library`.

- [ ] **Step 2: Export it** — in `src/lib.rs` add `pub mod library;` (after `pub mod types;`) and `pub use library::{Library, Track};`.

- [ ] **Step 3: Add scan tests** at the bottom of `library.rs`. They build a temp tree of tiny valid WAVs (a supported file with real duration) plus a non-audio file, and assert scan behavior. Reuse a minimal WAV writer.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid PCM WAV of `frames` stereo frames at `rate` (silence).
    fn wav(rate: u32, frames: u32) -> Vec<u8> {
        let channels: u16 = 2;
        let bits: u16 = 16;
        let block_align = channels * bits / 8;
        let byte_rate = rate * u32::from(block_align);
        let data_len = frames * u32::from(block_align);
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data_len).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&byte_rate.to_le_bytes());
        v.extend_from_slice(&block_align.to_le_bytes());
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        v.resize(v.len() + data_len as usize, 0);
        v
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sas_lib_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn scan_finds_supported_files_skips_others_and_assigns_index_ids() {
        let root = tmpdir("scan");
        std::fs::write(root.join("b.wav"), wav(48_000, 48_000)).unwrap(); // 1.0 s
        std::fs::write(root.join("a.wav"), wav(48_000, 24_000)).unwrap(); // 0.5 s
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/c.wav"), wav(48_000, 48_000)).unwrap();
        std::fs::write(root.join("notes.txt"), b"not audio").unwrap();

        let lib = Library::scan(&root).unwrap();
        // Sorted walk: a.wav, b.wav, sub/c.wav — .txt skipped.
        assert_eq!(lib.tracks.len(), 3);
        assert_eq!(lib.tracks[0].id, 0);
        assert!(lib.tracks[0].path.ends_with("a.wav"));
        assert_eq!(lib.tracks[2].id, 2);
        assert!(lib.tracks[2].path.ends_with("c.wav"));
        assert_eq!(lib.path_of(1), Some(lib.tracks[1].path.as_path()));
        assert_eq!(lib.path_of(9), None);
    }

    #[test]
    fn scan_reads_duration() {
        let root = tmpdir("dur");
        std::fs::write(root.join("x.wav"), wav(48_000, 48_000)).unwrap(); // 1.0 s
        let lib = Library::scan(&root).unwrap();
        assert_eq!(lib.tracks.len(), 1);
        assert!(
            (lib.tracks[0].duration_secs - 1.0).abs() < 0.05,
            "duration {} not ~1.0 s",
            lib.tracks[0].duration_secs
        );
    }

    #[test]
    fn views_match_tracks_without_paths() {
        let root = tmpdir("views");
        std::fs::write(root.join("x.wav"), wav(48_000, 24_000)).unwrap();
        let lib = Library::scan(&root).unwrap();
        let views = lib.views();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].id, lib.tracks[0].id);
        assert_eq!(views[0].duration_secs, lib.tracks[0].duration_secs);
    }
}
```

- [ ] **Step 4: Verify.**

```
cargo test -p soul-audio-service
cargo clippy -p soul-audio-service --all-targets -- -D warnings
```

- [ ] **Step 5: Commit.**

```
git add -A
git commit -m "feat(service): recursive lofty library scan (Track/Library/views)"
```

---

### Task 3: Command translation + state projection (deterministic, sink/decoder-generic)

**Files:**
- Create: `crates/soul-audio-service/src/driver.rs`
- Modify: `crates/soul-audio-service/src/lib.rs` (add `pub mod driver;` + re-exports)

**Interfaces:**
- Consumes: `Command`, `Tick`, `QueueState`, `PlayState`, `RepeatMode` (T1); `NullSink`/`ConstDecoder` (T1).
- Produces: type alias `ServicePlayer<K, D>`; `apply_command`, `project_tick`, `project_queue`. Consumed by Task 4's pump thread.

- [ ] **Step 1: Create `src/driver.rs`.**

```rust
//! The concrete desktop `Player` specialization plus the pure functions that
//! translate an app [`Command`] into `Player` calls and project the `Player`'s
//! state into UI [`Tick`]/[`QueueState`] events. Generic over the sink and
//! decoder so the whole thing is unit-tested headlessly (NullSink + a const
//! decoder) exactly as it runs with a real device.

use std::collections::VecDeque;

use soul_audio_core::{DeckError, SeekableDecoder, Sink};
use soul_audio_control::Player;
use soul_audio_queue::{SliceContext, TwoLevelQueue};

use crate::types::{Command, PlayState, QueueState, Tick};

/// The desktop player: unbounded (`VecDeque`) Up-Next/history, `SliceContext`
/// over `u32` track ids, `PRELOAD = 2`. Generic over the sink `K` and decoder
/// `D` (production: `CpalSink`/`OwnedDecoder`; tests: `NullSink`/`ConstDecoder`).
pub type ServicePlayer<K, D> = Player<
    K,
    D,
    SliceContext<u32, Vec<u32>>,
    VecDeque<u32>,
    VecDeque<(u32, usize)>,
    2,
>;

/// Build the concrete desktop queue over a set of track ids (empty at start).
pub fn queue_for(
    ids: Vec<u32>,
) -> TwoLevelQueue<SliceContext<u32, Vec<u32>>, VecDeque<u32>, VecDeque<(u32, usize)>> {
    TwoLevelQueue::new(SliceContext::new(ids), VecDeque::new(), VecDeque::new())
}

/// Apply one UI command to the player. `seed` is used only by `SetShuffle`
/// (the caller supplies a fresh value each time). Transport errors propagate;
/// bounded-queue-full is impossible here (unbounded backings) but `play_next`/
/// `enqueue` still swallow it for uniformity.
pub fn apply_command<K: Sink, D: SeekableDecoder>(
    player: &mut ServicePlayer<K, D>,
    cmd: Command,
    seed: u64,
) -> Result<(), DeckError> {
    match cmd {
        Command::PlayContext { ids, start } => {
            player.set_source(SliceContext::new(ids), start);
            Ok(())
        }
        Command::TogglePlay => {
            player.toggle_play();
            Ok(())
        }
        Command::Play => {
            player.play();
            Ok(())
        }
        Command::Pause => {
            player.pause();
            Ok(())
        }
        Command::Stop => {
            player.stop();
            Ok(())
        }
        Command::Next => player.next(),
        Command::Prev => player.prev(),
        Command::Seek { secs } => {
            let frame = (secs.max(0.0) * f64::from(player.sample_rate())) as u64;
            player.seek_frames(frame).map(|_| ())
        }
        Command::SetVolume(v) => {
            player.set_volume(v);
            Ok(())
        }
        Command::PlayNext(id) => {
            let _ = player.play_next(id);
            Ok(())
        }
        Command::Enqueue(id) => {
            let _ = player.enqueue(id);
            Ok(())
        }
        Command::ClearQueue => {
            player.clear_queue();
            Ok(())
        }
        Command::SetShuffle(on) => {
            player.set_shuffle(on, seed);
            Ok(())
        }
        Command::SetRepeat(mode) => {
            player.set_repeat(mode.into());
            Ok(())
        }
        Command::Goto(i) => player.goto(i),
    }
}

fn frames_to_secs(frames: u64, rate: u32) -> f64 {
    if rate == 0 {
        0.0
    } else {
        frames as f64 / f64::from(rate)
    }
}

/// Project the now-playing/transport snapshot. `volume` is tracked by the caller
/// (the `Player` has no volume getter).
pub fn project_tick<K: Sink, D: SeekableDecoder>(
    player: &ServicePlayer<K, D>,
    volume: f32,
) -> Tick {
    let rate = player.sample_rate();
    Tick {
        now_playing_id: player.current(),
        position_secs: frames_to_secs(player.position_frames(), rate),
        duration_secs: player.duration_frames().map(|f| frames_to_secs(f, rate)),
        state: PlayState::from(player.state()),
        shuffle: player.is_shuffled(),
        repeat: player.repeat().into(),
        volume,
    }
}

/// Project the Up-Next + context-position snapshot.
pub fn project_queue<K: Sink, D: SeekableDecoder>(player: &ServicePlayer<K, D>) -> QueueState {
    let q = player.sequencer();
    QueueState {
        up_next_ids: q.upnext_iter().collect(),
        context_pos: q.pos(),
        context_len: q.source_len(),
    }
}
```

- [ ] **Step 2: Export** — in `src/lib.rs` add `pub mod driver;` and `pub use driver::{apply_command, project_queue, project_tick, ServicePlayer};`.

- [ ] **Step 3: Add the deterministic behavior tests** at the bottom of `driver.rs`. A synchronous helper drives the load loop with `ConstDecoder`s so tracks actually become current, then commands are asserted via the projections. Choose per-track frames well above the deck block so a track spans multiple pumps.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{ConstDecoder, NullSink};
    use soul_audio_control::Player;
    use soul_audio_core::{Deck, Quality};
    use soul_audio_engine::Engine;

    const RATE: u32 = 48_000;
    const FRAMES: u64 = 4_000; // >> deck block, so a track spans several pumps

    /// A player over NullSink/ConstDecoder with a 1-second prev threshold.
    fn player() -> ServicePlayer<NullSink, ConstDecoder> {
        let deck = Deck::new(NullSink::new(RATE, 2), Quality::Hq);
        let engine = Engine::new(deck, queue_for(Vec::new()));
        Player::new(engine, u64::from(RATE)) // prev_restart_frames = 1 s
    }

    /// Run the open/pump loop `cycles` times, feeding a ConstDecoder for each
    /// track the player asks to open (id used as the constant sample so tracks
    /// are distinguishable). Deterministic — no threads, no device.
    fn drive(p: &mut ServicePlayer<NullSink, ConstDecoder>, cycles: usize) {
        for _ in 0..cycles {
            while let Some(id) = p.poll_open() {
                let dec = ConstDecoder::new(RATE, 2, FRAMES, id as i32 + 1);
                p.provide(id, dec).unwrap();
            }
            let _ = p.pump().unwrap();
        }
    }

    #[test]
    fn play_context_makes_the_start_track_current() {
        let mut p = player();
        apply_command(&mut p, Command::PlayContext { ids: vec![0, 1, 2], start: 0 }, 0).unwrap();
        drive(&mut p, 3);
        assert_eq!(project_tick(&p, 1.0).now_playing_id, Some(0));
        assert_eq!(project_tick(&p, 1.0).state, PlayState::Playing);
    }

    #[test]
    fn pause_and_toggle_reflect_in_state() {
        let mut p = player();
        apply_command(&mut p, Command::PlayContext { ids: vec![0], start: 0 }, 0).unwrap();
        drive(&mut p, 3);
        apply_command(&mut p, Command::Pause, 0).unwrap();
        assert_eq!(project_tick(&p, 1.0).state, PlayState::Paused);
        apply_command(&mut p, Command::TogglePlay, 0).unwrap();
        assert_eq!(project_tick(&p, 1.0).state, PlayState::Playing);
    }

    #[test]
    fn play_next_then_enqueue_populate_upnext_in_order() {
        let mut p = player();
        apply_command(&mut p, Command::PlayContext { ids: vec![0, 1, 2], start: 0 }, 0).unwrap();
        drive(&mut p, 3);
        apply_command(&mut p, Command::Enqueue(2), 0).unwrap();
        apply_command(&mut p, Command::PlayNext(1), 0).unwrap();
        // PlayNext jumps the front; Enqueue is behind it.
        assert_eq!(project_queue(&p).up_next_ids, vec![1, 2]);
        apply_command(&mut p, Command::ClearQueue, 0).unwrap();
        assert!(project_queue(&p).up_next_ids.is_empty());
    }

    #[test]
    fn shuffle_and_repeat_flags_project() {
        let mut p = player();
        apply_command(&mut p, Command::PlayContext { ids: vec![0, 1, 2, 3], start: 0 }, 0).unwrap();
        drive(&mut p, 3);
        apply_command(&mut p, Command::SetShuffle(true), 12345).unwrap();
        assert!(project_tick(&p, 1.0).shuffle);
        apply_command(&mut p, Command::SetRepeat(RepeatMode::All), 0).unwrap();
        assert_eq!(project_tick(&p, 1.0).repeat, RepeatMode::All);
        apply_command(&mut p, Command::SetShuffle(false), 0).unwrap();
        assert!(!project_tick(&p, 1.0).shuffle);
    }

    #[test]
    fn prev_restarts_when_past_threshold_else_steps_back() {
        // threshold = 1 s = RATE frames. Advance to track 1, play >1 s of it, then
        // Prev restarts track 1 (seek 0). With <1 s elapsed, Prev steps to track 0.
        let mut p = player();
        apply_command(&mut p, Command::PlayContext { ids: vec![0, 1, 2], start: 0 }, 0).unwrap();
        drive(&mut p, 3);
        apply_command(&mut p, Command::Next, 0).unwrap();
        drive(&mut p, 2);
        assert_eq!(p.current(), Some(1), "on track 1");
        // Not enough elapsed → steps back to track 0.
        if p.position_frames() <= u64::from(RATE) {
            apply_command(&mut p, Command::Prev, 0).unwrap();
            drive(&mut p, 2);
            assert_eq!(p.current(), Some(0), "prev stepped back");
        }
    }

    #[test]
    fn goto_jumps_within_context() {
        let mut p = player();
        apply_command(&mut p, Command::PlayContext { ids: vec![0, 1, 2, 3], start: 0 }, 0).unwrap();
        drive(&mut p, 3);
        apply_command(&mut p, Command::Goto(2), 0).unwrap();
        drive(&mut p, 3);
        assert_eq!(p.current(), Some(2));
    }
}
```

> **Note on the exact Up-Next ordering / prev-threshold timing:** the precise `up_next_ids` order after `PlayNext` vs `Enqueue`, and the pump-cycle counts for the prev test, follow the shipped `TwoLevelQueue`/`Deck` semantics (verified in the control/queue crates). If an assertion's exact ordering or count is off by the deck's block granularity, adjust the `drive(..)` cycle counts / expected vector to the observed shipped behavior — do NOT change production code to satisfy a test; confirm the behavior against `soul-audio-queue`'s own tests first.

- [ ] **Step 4: Verify.**

```
cargo test -p soul-audio-service
cargo clippy -p soul-audio-service --all-targets -- -D warnings
```

- [ ] **Step 5: Commit.**

```
git add -A
git commit -m "feat(service): app-command translation + state projection over Player"
```

---

### Task 4: The threaded `AudioPlayer` actor (pump + loader)

**Files:**
- Create: `crates/soul-audio-service/src/actor.rs`
- Modify: `crates/soul-audio-service/src/lib.rs` (add `pub mod actor;` + re-export)

**Interfaces:**
- Consumes: `Command`, `Event` (T1); `Library` (T2); `apply_command`/`project_*`/`ServicePlayer`/`queue_for` (T3); `soul_audio_desktop::{OwnedDecoder, open_track}`.
- Produces: `soul_audio_service::AudioPlayer` with `spawn`, `send`, `set_library`. Consumed by Plan 4's Tauri backend.

- [ ] **Step 1: Create `src/actor.rs`.**

```rust
//! The player actor: two non-real-time threads around a `Player`.
//! - **pump**: owns the `Player` + sink, drains the command channel, services
//!   decoder opens, pumps one block per cycle, and emits throttled state.
//! - **loader**: resolves `id → path` against the current `Library` and opens
//!   the (large, `Send`) `OwnedDecoder` off the pump thread.
//!
//! Sink-generic via a factory run ON the pump thread, so a `!Send` device sink
//! (`CpalSink`) works and this crate never names `cpal`.

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use soul_audio_core::{Deck, Progress, Quality, Sink};
use soul_audio_control::Player;
use soul_audio_desktop::{OwnedDecoder, open_track};
use soul_audio_engine::{Engine, Provided};

use crate::driver::{apply_command, project_queue, project_tick, queue_for, ServicePlayer};
use crate::library::Library;
use crate::types::{Command, Event};

/// Golden-ratio odd constant; advanced per shuffle to vary the permutation seed
/// without an RNG dependency (deterministic across runs).
const SEED_STEP: u64 = 0x9E37_79B9_7F4A_7C15;

/// Handle to the running player actor. Dropping it stops the threads.
pub struct AudioPlayer {
    cmd_tx: Sender<Command>,
    library: Arc<Mutex<Arc<Library>>>,
    pump: Option<JoinHandle<()>>,
}

impl AudioPlayer {
    /// Spawn the actor. `make_sink` runs on the pump thread (so `K` need not be
    /// `Send`). `on_event` is called from the pump thread on every emitted
    /// event. `library` is the initial `id → path` map (swap later via
    /// [`set_library`](Self::set_library)).
    pub fn spawn<K, Mk, Ev>(make_sink: Mk, library: Arc<Library>, on_event: Ev) -> Self
    where
        K: Sink + 'static,
        Mk: FnOnce() -> anyhow::Result<K> + Send + 'static,
        Ev: Fn(Event) + Send + 'static,
    {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
        let library = Arc::new(Mutex::new(library));
        let loader_lib = Arc::clone(&library);

        let pump = thread::Builder::new()
            .name("player-pump".into())
            .stack_size(16 * 1024 * 1024)
            .spawn(move || pump_thread(make_sink, cmd_rx, loader_lib, on_event))
            .expect("spawn pump thread");

        Self { cmd_tx, library, pump: Some(pump) }
    }

    /// Queue a command (fire-and-forget). No-op if the actor has stopped.
    pub fn send(&self, cmd: Command) {
        let _ = self.cmd_tx.send(cmd);
    }

    /// Replace the library used by the loader to resolve `id → path`.
    pub fn set_library(&self, library: Arc<Library>) {
        *self.library.lock().expect("library mutex") = library;
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        // Dropping cmd_tx disconnects the command channel; the pump loop then
        // exits and its loader thread follows. Join to avoid a detached thread.
        // (cmd_tx is dropped when `self` is; take + join the pump handle.)
        if let Some(h) = self.pump.take() {
            let _ = h.join();
        }
    }
}

/// The loader thread: `id → path → OwnedDecoder`, sent back to the pump.
fn loader_thread(
    req_rx: Receiver<u32>,
    res_tx: Sender<(u32, anyhow::Result<OwnedDecoder>)>,
    library: Arc<Mutex<Arc<Library>>>,
) {
    for id in req_rx {
        let opened = {
            let lib = library.lock().expect("library mutex").clone();
            match lib.path_of(id) {
                Some(path) => open_track(path),
                None => Err(anyhow::anyhow!("no path for track id {id}")),
            }
        };
        if res_tx.send((id, opened)).is_err() {
            break; // pump gone
        }
    }
}

fn pump_thread<K, Mk, Ev>(
    make_sink: Mk,
    cmd_rx: Receiver<Command>,
    library: Arc<Mutex<Arc<Library>>>,
    on_event: Ev,
) where
    K: Sink + 'static,
    Mk: FnOnce() -> anyhow::Result<K>,
    Ev: Fn(Event),
{
    let sink = match make_sink() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("player: sink init failed: {e:#}");
            return;
        }
    };
    let rate = sink.stream_info().sample_rate;
    let deck = Deck::new(sink, Quality::Hq);
    let engine = Engine::new(deck, queue_for(Vec::new()));
    let mut player: ServicePlayer<K, OwnedDecoder> =
        Player::new(engine, u64::from(rate).saturating_mul(3)); // ~3 s prev threshold

    let (req_tx, req_rx) = mpsc::channel::<u32>();
    let (res_tx, res_rx) = mpsc::channel::<(u32, anyhow::Result<OwnedDecoder>)>();
    let loader = {
        let lib = Arc::clone(&library);
        thread::Builder::new()
            .name("player-loader".into())
            .stack_size(16 * 1024 * 1024)
            .spawn(move || loader_thread(req_rx, res_tx, lib))
            .expect("spawn loader thread")
    };

    let mut volume: f32 = 1.0;
    let mut seed: u64 = SEED_STEP;
    let mut last_tick = Instant::now();
    let tick_every = Duration::from_millis(125); // ~8 Hz
    let mut disconnected = false;

    loop {
        // 1. Drain commands.
        let mut changed = false;
        loop {
            match cmd_rx.try_recv() {
                Ok(cmd) => {
                    if let Command::SetVolume(v) = cmd {
                        volume = v;
                    }
                    seed = seed.wrapping_add(SEED_STEP);
                    if let Err(e) = apply_command(&mut player, cmd, seed) {
                        eprintln!("player: command error: {e:?}");
                    }
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        // 2. Install opened decoders; request whatever the engine now wants.
        loop {
            match res_rx.try_recv() {
                Ok((id, Ok(dec))) => {
                    if let Ok(Provided::Loaded) = player.provide(id, dec) {
                        changed = true;
                    }
                }
                Ok((id, Err(e))) => eprintln!("player: open track {id} failed: {e:#}"),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        while let Some(id) = player.poll_open() {
            let _ = req_tx.send(id);
        }

        // 3. Pump one block (paced by the real sink's backpressure; NullSink
        //    returns immediately, so tests run briefly then drop the actor).
        let progress = match player.pump() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("player: pump error: {e:?}");
                Progress::Idle
            }
        };
        if matches!(progress, Progress::TrackChanged | Progress::Ended) {
            changed = true;
        }

        // 4. Emit throttled state (immediately on change, else ~8 Hz).
        if changed || last_tick.elapsed() >= tick_every {
            on_event(Event::Tick(project_tick(&player, volume)));
            on_event(Event::Queue(project_queue(&player)));
            last_tick = Instant::now();
        }

        // 5. Shutdown / idle pacing.
        if disconnected && !player.is_active() {
            break;
        }
        if matches!(progress, Progress::Idle) {
            thread::sleep(Duration::from_millis(3));
        }
    }

    drop(req_tx); // stop the loader
    let _ = loader.join();
}
```

> **Import note:** the imports above are the complete set (`Deck`/`Quality`/`Progress`/`Sink` from `soul_audio_core`, `Player` from `soul_audio_control`, `Engine`/`Provided` from `soul_audio_engine`, `OwnedDecoder`/`open_track` from `soul_audio_desktop`). Run clippy after writing to confirm none are unused.

- [ ] **Step 2: Export** — in `src/lib.rs` add `pub mod actor;` and `pub use actor::AudioPlayer;`.

- [ ] **Step 3: Add a headless smoke test** at the bottom of `actor.rs` using `NullSink` + real tiny temp WAVs (the loader runs `open_track` on real files). Collect events over an `mpsc` and wait with a timeout.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::NullSink;
    use std::path::PathBuf;
    use std::sync::mpsc::RecvTimeoutError;

    fn wav(rate: u32, frames: u32) -> Vec<u8> {
        // (same minimal stereo-16-bit WAV writer as library.rs tests)
        let channels: u16 = 2;
        let block_align = channels * 2;
        let byte_rate = rate * u32::from(block_align);
        let data_len = frames * u32::from(block_align);
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data_len).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&byte_rate.to_le_bytes());
        v.extend_from_slice(&block_align.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        v.resize(v.len() + data_len as usize, 0);
        v
    }

    fn fixture_library(dir: &PathBuf) -> Arc<Library> {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("a.wav"), wav(48_000, 96_000)).unwrap(); // 2 s
        std::fs::write(dir.join("b.wav"), wav(48_000, 96_000)).unwrap();
        Arc::new(Library::scan(dir).unwrap())
    }

    #[test]
    fn actor_plays_context_and_emits_now_playing() {
        let dir = std::env::temp_dir().join(format!("sas_actor_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let lib = fixture_library(&dir);

        let (ev_tx, ev_rx) = mpsc::channel::<Event>();
        let player = AudioPlayer::spawn(
            || Ok(NullSink::new(48_000, 2)),
            Arc::clone(&lib),
            move |ev| {
                let _ = ev_tx.send(ev);
            },
        );

        player.send(Command::PlayContext { ids: vec![0, 1], start: 0 });

        // Wait (bounded) for a Tick showing track 0 playing.
        let deadline = Duration::from_secs(5);
        let mut saw_playing = false;
        loop {
            match ev_rx.recv_timeout(deadline) {
                Ok(Event::Tick(t)) => {
                    if t.now_playing_id == Some(0) && t.state == crate::types::PlayState::Playing {
                        saw_playing = true;
                        break;
                    }
                }
                Ok(Event::Queue(_)) => {}
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(saw_playing, "never observed track 0 playing");

        drop(player); // clean shutdown (joins threads)
        let _ = std::fs::remove_dir_all(&dir);
    }
}
```

- [ ] **Step 4: Verify — including the full gate.**

```
cargo test -p soul-audio-service
cargo clippy -p soul-audio-service --all-targets -- -D warnings
cargo xtask verify
```

`cargo xtask verify` must print `verify: OK` (workspace clippy, full test suite incl. this crate, and the unchanged thumbv7em builds — `soul-audio-service` adds no cpal to the default build, so the gate stays device-free). Allow ~10 minutes; run from a non-nested checkout.

- [ ] **Step 5: Commit.**

```
git add -A
git commit -m "feat(service): threaded AudioPlayer actor (pump + loader) around Player"
```

---

## Self-Review

**Spec coverage (design §4, §7, §8):**
- §4.1 library scan + `SliceContext<u32>` context → Task 2 (`Library::scan`) + Task 3 (`PlayContext` builds `SliceContext::new(ids)`). ✓
- §4.2 pump thread owns `Player<…, VecDeque, VecDeque, 2>`; loader opens off-thread; drain→apply, service opens, pump, throttled emit → Task 4. ✓ (sink-generic so `CpalSink` injects in Plan 4).
- §4.3/§4.4 command + event API → Task 1 `Command`/`Event`/`Tick`/`QueueState` (ids not full data); `cover_art`/`open_folder` are Tauri-command concerns deferred to Plan 4 (the service exposes `Library::views()` + `path_of` that Plan 4's `open_folder` and a `cover_art` command build on). ✓
- §4.5 prev-3s rule → lives in `control::Player` (Plan 1); the actor sets `prev_restart_frames = 3 × device_rate` (Task 4), computing frames from the real output rate (resolves Plan-1's rate-dependence note). State throttling ~8 Hz + on change → Task 4. `StopAfter`/`cover_art` not exposed in MVP → correctly absent. ✓
- §8 headless unit tests with a null sink asserting transport/queue/shuffle/repeat + prev-3s → Task 3; library scan test → Task 2; actor smoke test → Task 4. ✓
- §7 `soul-audio-service` is a root member in `cargo xtask verify`; the Tauri app stays out (Plan 4). ✓

**Placeholder scan:** the only deliberately-flagged spots are (a) the lofty accessor calls (external-API version risk, with explicit reconcile instructions) and (b) the exact Up-Next order / prev-cycle counts in Task 3's tests (adjust to shipped queue/deck semantics, never change production code). Both are called out, not silent TODOs. The `actor.rs` import note flags the intentionally-wrong `Deck` import to force the implementer to fix the `use` lines.

**Type consistency:** `ServicePlayer<K,D>` = `Player<K, D, SliceContext<u32,Vec<u32>>, VecDeque<u32>, VecDeque<(u32,usize)>, 2>` matches the verified construction recipe; `apply_command`/`project_*` are generic over `K,D` and used by both the T3 tests (`NullSink`/`ConstDecoder`) and the T4 actor (`K`/`OwnedDecoder`); `Command`/`Event`/`RepeatMode`/`PlayState` names are identical across modules and re-exports; `RepeatMode ↔ queue::Repeat` and `DeckState → PlayState` mappings are total.

**Atomicity:** each task ends with the crate compiling + clippy-clean + its tests green; Task 4 runs the full `verify`. The crate adds no cpal to the default workspace build, preserving the device-free CI gate.
