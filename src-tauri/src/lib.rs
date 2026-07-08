//! Lost Harness — Rust core (M0 bootstrap)
//!
//! M0: empty shell — modules are stubbed out, the app opens a window
//! showing the Svelte frontend which displays "Hello Lost Harness".
//!
//! Milestones (see `milestones.md` in the project vault):
//!   M1: TRM + agent loop + minimal chat
//!   M2: UI shell (tiling, profiles, command palette)
//!   M3+: tool registry, models, computer use, audio, ...

// ── Module declarations ──────────────────────────────────────────────────────
// Each module is a stub at M0. Real implementations land in their target
// milestone. Listing them here so `cargo build` resolves the structure
// and the architecture skeleton is visible in the source tree.

mod agent; // M1: agent loop, §7 gate, tool dispatch
mod trm; // M1: TRM engine, classification, logging
mod audio; // M6: Audio engine, VAD, TTS pipeline
mod ipc; // M1: Tauri command handlers
mod models; // M4: Model manager (local + cloud)
mod platform; // M5: cross-platform computer use (cfg'd submodules)
mod storage; // M1+: SQLite, sqlite-vec, sled/redb
mod tools; // M3: Tool registry + implementations

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Tauri entry point. Runs once on launch.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Structured logging. RUST_LOG controls verbosity (e.g. RUST_LOG=info, lhp=debug).
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();

    tracing::info!("Lost Harness — M0 bootstrap starting");

    tauri::Builder::default()
        .setup(|_app| {
            tracing::info!("Tauri app initialized; frontend should be loading");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ipc::get_app_version,
            ipc::get_active_profile,
            ipc::list_profiles,
            ipc::send_message,
            ipc::stream_token,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Lost Harness");
}
