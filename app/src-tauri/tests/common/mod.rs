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
use tauri::{App, Listener};

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
///
/// NOTE: this scans the ENTIRE captured buffer (oldest included), so it can
/// match a stale Tick emitted before whatever command you just sent — fine
/// when `pred` targets a value that could only appear after a fresh mutation,
/// but vacuous if `pred` also matches the actor's initial state. When in
/// doubt, snapshot `events.lock().unwrap().len()` before sending the command
/// and use `wait_for_tick_since` instead.
pub fn wait_for_tick(
    events: &Arc<Mutex<Vec<Event>>>,
    pred: impl Fn(&Tick) -> bool,
    timeout: Duration,
) -> Tick {
    wait_for_tick_since(events, 0, pred, timeout)
}

/// Like `wait_for_tick`, but only considers events captured at index `>=
/// since` — use this when `pred` could also match a Tick emitted BEFORE the
/// command under test (e.g. a value equal to the actor's initial default),
/// which would make the assertion pass regardless of whether the command
/// actually did anything. Snapshot `since` via
/// `events.lock().unwrap().len()` immediately before sending the command.
pub fn wait_for_tick_since(
    events: &Arc<Mutex<Vec<Event>>>,
    since: usize,
    pred: impl Fn(&Tick) -> bool,
    timeout: Duration,
) -> Tick {
    let start = Instant::now();
    loop {
        if let Some(t) = events.lock().unwrap()[since..]
            .iter()
            .rev()
            .find_map(|e| match e {
                Event::Tick(t) if pred(t) => Some(t.clone()),
                _ => None,
            })
        {
            return t;
        }
        assert!(
            start.elapsed() < timeout,
            "timed out waiting for a matching Tick since index {since}"
        );
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
    f.write_all(&(rate * u32::from(ch) * 2).to_le_bytes())
        .unwrap();
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
