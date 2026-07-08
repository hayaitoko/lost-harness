// Lost Harness — M0 bootstrap entry point.
// Tauri 2: runtime lives in lib.rs (so the same code can be reused for
// mobile targets later), main.rs is a thin shim that calls into it.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    lost_harness_product_lib::run()
}
