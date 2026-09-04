//! soul-player binary entry point — a shim over the `soul_player` library so
//! integration tests can reach the command/actor modules.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    soul_player::run();
}
