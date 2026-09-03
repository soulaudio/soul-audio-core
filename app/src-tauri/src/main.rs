//! soul-player — Tauri 2 app shell over `soul-audio-service`.
//!
//! Plan 4: hosts a lazily-spawned `AudioPlayer` actor over `CpalSink` (first
//! folder open), routes transport commands through it, and forwards the
//! backend's `Event`s to the webview.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actor;
mod commands;
mod ipc;
mod offline;

use actor::PlayerState;

fn main() {
    // Debug builds regenerate the TS bindings from the live command signatures.
    let bindings = ipc::builder();
    #[cfg(debug_assertions)]
    {
        ipc::export(&bindings);
        // Headless codegen: SOUL_PLAYER_CODEGEN=1 exports bindings.ts and exits
        // without opening a window (used by CI / the dev loop).
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
