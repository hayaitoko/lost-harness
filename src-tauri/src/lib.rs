//! Lost Harness — Rust core (M1 surface)
//!
//! M1: classifier (heuristic) + privacy gate + agent loop + minimal chat via IPC.
//! M0 was a stub; M1 boots the storage tree, loads persisted providers,
//! and registers the full IPC surface for the Svelte frontend.
//!
//! Milestones (see `milestones.md` in the project vault):
//!   M0: empty shell
//!   M1: classifier + agent loop + minimal chat
//!   M2: UI shell (tiling, profiles, command palette)
//!   M3+: tool registry, models, computer use, audio, ...

// ── Module declarations ──────────────────────────────────────────────────────

mod agent; // M1: agent loop, §7 gate, tool dispatch
mod classifier; // M1: privacy classifier (heuristic + trained model)
mod audio; // M6: Audio engine, VAD, TTS pipeline
mod hooks; // M3: Hook chain — privacy filter + permission + sandbox + first-use
mod ipc; // M1: Tauri command handlers
mod models; // M4: Model manager (local + cloud)
mod platform; // M5: cross-platform computer use (cfg'd submodules)
mod storage; // M1+: SQLite, sqlite-vec, sled/redb
mod tools; // M3: Tool registry + implementations

use std::path::PathBuf;
use std::sync::Arc;

use tauri::Manager;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::agent::gate::PrivacyGate;
use crate::agent::loop_mod::AgentLoop;
use crate::ipc::AppState;
use crate::models::{ModelManager, Provider, ProviderKind};
use crate::storage::Storage;
use crate::tools::ToolDispatcher;
use crate::classifier::HeuristicClassifier;

/// Tauri entry point. Runs once on launch.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Structured logging. RUST_LOG controls verbosity (e.g. RUST_LOG=info, lhp=debug).
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();

    tracing::info!("Lost Harness — M1 starting");

    tauri::Builder::default()
        .setup(|app| {
            // Storage at ~/Documents/Lost-Harness/ (spec §2 default).
            // We resolve against $HOME for cross-platform safety; on macOS
            // this lands at the spec-mandated location.
            let base_path = default_storage_root();
            tracing::info!(path = %base_path.display(), "opening storage");
            let storage = Storage::open(&base_path)
                .map_err(|e| format!("failed to open storage at {}: {e}", base_path.display()))?;
            let storage = Arc::new(storage);

            // Load persisted providers from global.db::endpoints and
            // hydrate the in-memory ModelManager.
            let model_manager = Arc::new(ModelManager::new());
            hydrate_providers_from_storage(&storage, &model_manager);

            // Privacy classifier (heuristic fallback until the trained model
            // lands — see classifier/engine.rs). Shared between the message-level
            // §7 gate and the tool gating chain so both classify identically.
            let classifier: Arc<dyn crate::classifier::Classifier> =
                Arc::new(HeuristicClassifier::new());

            // §7 Privacy Gate for message egress.
            let gate = PrivacyGate::new(Arc::clone(&classifier));

            // §3 tool spine: workspace-confined read-only tools behind the
            // unified pretooluse hook chain, filtered by this body's caps.
            let tools = Arc::new(build_tool_dispatcher(&base_path, Arc::clone(&classifier)));

            let agent_loop = Arc::new(AgentLoop::new(
                gate,
                Arc::clone(&model_manager),
                Arc::clone(&storage),
                tools,
            ));

            let state = AppState {
                agent_loop,
                model_manager,
                storage,
            };
            app.manage(state);

            tracing::info!("Tauri app initialized; M1 commands registered");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ipc::get_app_version,
            ipc::get_active_profile,
            ipc::list_profiles,
            ipc::list_conversations,
            ipc::create_conversation,
            ipc::get_messages,
            ipc::list_providers,
            ipc::add_provider,
            ipc::remove_provider,
            ipc::list_models,
            ipc::send_message,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Lost Harness");
}

/// Resolve the storage root: `$HOME/Documents/Lost-Harness/`.
/// Falls back to `~/Documents/Lost-Harness/` if `HOME` is unset.
fn default_storage_root() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| dirs_home_fallback())
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Documents").join("Lost-Harness")
}

/// Best-effort HOME lookup for platforms where `$HOME` isn't set
/// (Windows-style `%USERPROFILE%` etc). Not used on macOS.
fn dirs_home_fallback() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}

/// Populate `mm` with the providers persisted in `global.db::endpoints`.
/// Existing in-memory providers (e.g. ones added in this session) are
/// preserved — `add_provider` replaces by id, so loading from disk is
/// a no-op for anything that matches by id.
fn hydrate_providers_from_storage(
    storage: &Storage,
    mm: &ModelManager,
) {
    let endpoints = match storage.global().list_endpoints() {
        Ok(eps) => eps,
        Err(e) => {
            tracing::warn!(error = %e, "failed to list persisted endpoints");
            return;
        }
    };
    for ep in endpoints {
        let kind = match ep.kind.as_str() {
            "local" => ProviderKind::Local,
            "cloud" => ProviderKind::Cloud,
            _ => ProviderKind::Custom,
        };
        // `api_key_encrypted` is stored as raw bytes (encryption is
        // M4+ work). Treat the bytes as UTF-8; fall back to None on
        // any decode error rather than crashing the app on a bad row.
        let api_key = ep
            .api_key_encrypted
            .and_then(|b| String::from_utf8(b).ok());
        let provider = Provider::new(ep.id, ep.name, ep.base_url, api_key, kind);
        mm.add_provider(provider);
    }
    tracing::info!(count = mm.list_providers().len(), "hydrated providers from storage");
}

/// Build the M3 tool dispatcher for the desktop app body.
///
/// The read-only filesystem tools are confined to a `workspace/` directory
/// under the storage root, registered into the capability-filtered registry
/// (`BodyEnv::app_default`), and placed behind the standard pretooluse
/// gating chain: privacy filter → non-overridable sandbox floor →
/// permissions → first-use confirm. These read-only tools are whole-tool
/// `Allow`ed; write/delete tools (a later M3 round) will instead route
/// through the approval spine rather than a blanket allow.
fn build_tool_dispatcher(
    base_path: &std::path::Path,
    classifier: Arc<dyn crate::classifier::Classifier>,
) -> ToolDispatcher {
    use crate::hooks::{build_pretooluse_chain_with_confirmed, InMemoryPolicySource, PermissionMode};
    use crate::tools::fs::{ListDirTool, ReadFileTool, SearchFilesTool};
    use crate::tools::{BodyEnv, ToolRegistry};

    let workspace = base_path.join("workspace");
    if let Err(e) = std::fs::create_dir_all(&workspace) {
        tracing::warn!(
            error = %e,
            path = %workspace.display(),
            "failed to create the tool workspace directory"
        );
    }

    let mut registry = ToolRegistry::new();
    registry.register(Box::new(ReadFileTool::new(workspace.clone())));
    registry.register(Box::new(ListDirTool::new(workspace.clone())));
    registry.register(Box::new(SearchFilesTool::new(workspace)));

    let mut policy = InMemoryPolicySource::new();
    policy.set_mode("read_file", PermissionMode::Allow);
    policy.set_mode("list_dir", PermissionMode::Allow);
    policy.set_mode("search_files", PermissionMode::Allow);

    // These are read-only and workspace-confined, so ship them pre-trusted
    // (no first-use prompt). State-changing tools won't get this treatment.
    let chain = build_pretooluse_chain_with_confirmed(
        PrivacyGate::new(classifier),
        Box::new(policy),
        &["read_file", "list_dir", "search_files"],
    );
    ToolDispatcher::new(registry, chain, BodyEnv::app_default())
}
