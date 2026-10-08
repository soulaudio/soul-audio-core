//! Tier-1 product-e2e: real actor/engine over NullSink, driven through the
//! app's command→event flow against the committed fixtures.
mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{capture_events, mock_app, wait_for_tick, wait_for_tick_since, write_wav};
use soul_audio_service::{Command, Event, QueueState};
use soul_player::actor::PlayerState;
use soul_player::commands::{load_folder, track_art};
use tauri::Manager;

fn testdata(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(name)
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

#[test]
fn per_format_durations_are_reported() {
    // Every supported container's TrackView carries the right duration — with
    // one documented exception: `tone.aac` is a bare ADTS elementary stream
    // (no container wrapper), and `soul-audio-tags::locate::sniff` has no
    // magic-byte case for raw ADTS, so it falls into `Container::Unknown` and
    // `read_owned` returns `Err(Unsupported)` (see the parity note in
    // `crates/soul-audio-tags/tests/differential.rs`'s `our_fields`). The core
    // decoder still plays it fine (full ADTS demuxer), but `Library::scan`'s
    // "any tag-read error degrades to 0.0/None" policy means its TrackView
    // duration is `0.0` — expected, not a bug.
    let dir = library_of(&[
        "tone.wav",
        "tone.flac",
        "tone.mp3",
        "tone.opus",
        "tone.m4a",
        "tone.aac",
        "tone.dsf",
        "tone-dsd128.dsf",
        "tone.dff",
        "tone-vorbis.ogg",
    ]);
    let app = mock_app();
    let state = app.state::<PlayerState>();
    let views = load_folder(state.inner(), app.handle(), dir.path()).unwrap();

    // Assert durations as a multiset (the library sorts by path, so we don't
    // rely on per-file ordering): three DSD @ ~0.5 s (dsf, dsd128, dff), seven
    // containered/scanned-PCM @ ~2.0 s — including tone.aac (ADTS frame-scan)
    // and tone-vorbis.ogg (Ogg-granule duration). No untagged fixtures remain.
    let mut secs: Vec<f64> = views.iter().map(|v| v.duration_secs).collect();
    secs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let dsd = secs.iter().filter(|d| (**d - 0.5).abs() < 0.06).count();
    let pcm = secs.iter().filter(|d| (**d - 2.0).abs() < 0.16).count();
    let untagged = secs.iter().filter(|d| **d == 0.0).count();
    assert_eq!(dsd, 3, "three DSD fixtures ~0.5s: {secs:?}");
    assert_eq!(pcm, 7, "seven PCM fixtures ~2.0s (incl. ADTS + Vorbis): {secs:?}");
    assert_eq!(untagged, 0, "ADTS now has a frame-scan duration: {secs:?}");
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
    t.duration_secs
        .map(|d| (d - secs).abs() < 0.1)
        .unwrap_or(false)
}

#[test]
fn transport_commands_keep_the_actor_alive() {
    // NullSink free-runs, so we cannot catch a track mid-play to assert a seeked
    // position (that is covered deterministically by soul-audio-service's
    // driver.rs and core seek.rs). Instead prove the app→actor command path
    // handles EVERY transport variant — Seek included — without panicking the
    // pump thread: fire them all, then confirm the actor still processes a fresh
    // command (a SetVolume it must reflect) = liveness.
    let dir = library_of(&["tone.wav"]);
    let app = mock_app();
    let events = capture_events(&app);
    let state = app.state::<PlayerState>();
    load_folder(state.inner(), app.handle(), dir.path()).unwrap();
    wait_for_tick(&events, |t| t.now_playing_id.is_some(), TIMEOUT);

    for cmd in [
        Command::Seek { secs: 1.0 },
        Command::Pause,
        Command::Play,
        Command::Next,
        Command::Prev,
        Command::Stop,
        Command::Goto(0),
    ] {
        state.inner().send_command(app.handle(), cmd);
    }
    // Liveness: the pump survived every command and still reflects a new one.
    state
        .inner()
        .send_command(app.handle(), Command::SetVolume(0.5));
    let t = wait_for_tick(&events, |t| (t.volume - 0.5).abs() < 1e-6, TIMEOUT);
    assert!(
        (t.volume - 0.5).abs() < 1e-6,
        "actor alive and reflecting commands"
    );
}

#[test]
fn volume_is_set_and_clamped() {
    let dir = library_of(&["tone.wav"]);
    let app = mock_app();
    let events = capture_events(&app);
    let state = app.state::<PlayerState>();
    load_folder(state.inner(), app.handle(), dir.path()).unwrap();
    wait_for_tick(&events, |t| t.now_playing_id.is_some(), TIMEOUT);

    // Snapshot the buffer length before each mutating command and only match
    // events at index >= that snapshot. Without this, `SetVolume(2.0)`'s
    // assertion would be vacuous: the actor's initial `volume = 1.0` is
    // emitted (as part of the first Tick, before any SetVolume) and would
    // satisfy `(t.volume - 1.0).abs() < 1e-6` even if the upper clamp did
    // nothing at all.
    let since = events.lock().unwrap().len();
    state
        .inner()
        .send_command(app.handle(), Command::SetVolume(0.25));
    let t = wait_for_tick_since(&events, since, |t| (t.volume - 0.25).abs() < 1e-6, TIMEOUT);
    assert!((t.volume - 0.25).abs() < 1e-6);

    let since = events.lock().unwrap().len();
    state
        .inner()
        .send_command(app.handle(), Command::SetVolume(2.0));
    let t = wait_for_tick_since(&events, since, |t| (t.volume - 1.0).abs() < 1e-6, TIMEOUT);
    assert_eq!(t.volume, 1.0);

    let since = events.lock().unwrap().len();
    state
        .inner()
        .send_command(app.handle(), Command::SetVolume(-1.0));
    let t = wait_for_tick_since(&events, since, |t| (t.volume - 0.0).abs() < 1e-6, TIMEOUT);
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

    state.inner().send_command(
        app.handle(),
        Command::SetRepeat(soul_audio_service::RepeatMode::All),
    );
    wait_for_tick(
        &events,
        |t| t.repeat == soul_audio_service::RepeatMode::All,
        TIMEOUT,
    );
    state
        .inner()
        .send_command(app.handle(), Command::SetShuffle(true));
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

    state
        .inner()
        .send_command(app.handle(), Command::Enqueue(views[2].id));
    let q = wait_for_queue(&events, |q| q.up_next_ids.contains(&views[2].id), TIMEOUT);
    assert!(q.up_next_ids.contains(&views[2].id));

    // Snapshot the buffer length before ClearQueue and only match events at
    // index >= that snapshot. Without this, the assertion would be vacuous:
    // the actor emits a Queue event on every tick, so the initial
    // `PlayContext` load already emitted `up_next_ids: []` BEFORE the
    // Enqueue above — a bare `wait_for_queue` reverse-scan would match that
    // stale pre-Enqueue empty state even if ClearQueue did nothing.
    let since = events.lock().unwrap().len();
    state
        .inner()
        .send_command(app.handle(), Command::ClearQueue);
    let q = wait_for_queue_since(&events, since, |q| q.up_next_ids.is_empty(), TIMEOUT);
    assert!(
        q.up_next_ids.is_empty(),
        "post-ClearQueue Queue event has empty up_next"
    );
}

fn wait_for_queue(
    events: &std::sync::Arc<std::sync::Mutex<Vec<Event>>>,
    pred: impl Fn(&QueueState) -> bool,
    timeout: Duration,
) -> QueueState {
    wait_for_queue_since(events, 0, pred, timeout)
}

/// Like `wait_for_queue`, but only considers events captured at index `>=
/// since` — use this when `pred` could also match a QueueState emitted
/// BEFORE the command under test. Snapshot `since` via
/// `events.lock().unwrap().len()` immediately before sending the command.
fn wait_for_queue_since(
    events: &std::sync::Arc<std::sync::Mutex<Vec<Event>>>,
    since: usize,
    pred: impl Fn(&QueueState) -> bool,
    timeout: Duration,
) -> QueueState {
    let start = std::time::Instant::now();
    loop {
        if let Some(q) = events.lock().unwrap()[since..]
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::Queue(q) if pred(q) => Some(q.clone()),
                _ => None,
            })
        {
            return q;
        }
        assert!(
            start.elapsed() < timeout,
            "timed out waiting for QueueState since index {since}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn track_art_resolves_embedded_and_missing() {
    let dir = library_of(&["arted.flac", "tone.wav"]);
    let app = mock_app();
    let state = app.state::<PlayerState>();
    let views = load_folder(state.inner(), app.handle(), dir.path()).unwrap();

    // arted.flac (title "Arted Track" from the fixture's metadata) carries an
    // embedded cover; its art resolves to a data URL. (Fixtures are selected by
    // title now that the `has_art` DTO flag is gone — issue #33.)
    let arted = views
        .iter()
        .find(|v| v.title.as_deref() == Some("Arted Track"))
        .expect("arted.flac view");
    let url = track_art(app.state::<PlayerState>(), arted.id).unwrap();
    assert!(url.unwrap().starts_with("data:image/"));

    // The plain WAV (no tags → no title) has no art.
    let plain = views
        .iter()
        .find(|v| v.title.is_none())
        .expect("plain wav view");
    assert_eq!(
        track_art(app.state::<PlayerState>(), plain.id).unwrap(),
        None
    );

    // Unknown id → None.
    assert_eq!(track_art(app.state::<PlayerState>(), 9999).unwrap(), None);
}
