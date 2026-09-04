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
