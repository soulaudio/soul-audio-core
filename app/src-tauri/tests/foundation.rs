//! Task-1 smoke: the folder-open split works over the mock runtime + NullSink.
mod common;

use std::time::Duration;

use common::{capture_events, mock_app, wait_for_tick, write_wav};
use soul_player::actor::PlayerState;
use soul_player::commands::load_folder;
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
    state
        .inner()
        .send_command(app.handle(), soul_audio_service::Command::TogglePlay);
}
