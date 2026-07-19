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
mod embedder; // PLAN §9: on-device text embedder (memory's meaning-search lane)
mod audio; // M6: Audio engine, VAD, TTS pipeline
mod hooks; // M3: Hook chain — privacy filter + permission + sandbox + first-use
mod ipc; // M1: Tauri command handlers
mod models; // M4: Model manager (local + cloud)
mod packs; // M7 (Wave 4.5): Capability Packs — installable skill+agent+cron bundles
mod platform; // M5: cross-platform computer use (cfg'd submodules)
mod queue; // M4 (Wave 4.4): the one-queue-model substrate (deferred work)
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
use crate::classifier::RulesClassifier;

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

            // Crash-recovery boot pass (Q3 do-now item 4): terminalize any
            // conversation left mid-tool-call by an unclean shutdown of the
            // previous run, before the agent loop or any IPC command touches
            // it. Deliberately NOT `?`-propagated — a reconciliation failure
            // must not brick app boot.
            match crate::agent::crash_recovery::run_boot_pass(&storage) {
                Ok(report) if !report.interrupted.is_empty() => tracing::warn!(
                    count = report.interrupted.len(),
                    "crash-recovery: reconciled interrupted tool calls from a previous run"
                ),
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "crash-recovery boot pass failed; continuing startup"),
            }

            // Seed the built-in agent-type personas (Wave 4.3), idempotently.
            // Best-effort: a seed failure must not brick boot.
            if let Err(e) = storage
                .global()
                .ensure_builtin_agent_types(chrono::Utc::now().timestamp())
            {
                tracing::warn!(error = %e, "seeding built-in agent types failed; continuing");
            }

            // Load persisted providers from global.db::endpoints and
            // hydrate the in-memory ModelManager.
            let model_manager = Arc::new(ModelManager::new());
            hydrate_providers_from_storage(&storage, &model_manager);

            // Privacy classifier. The trained ONNX ensemble (bge-small +
            // distilbert, INT8) fused with the layer-0 rules, loaded from
            // <storage>/models/classifier/ if its models are installed;
            // otherwise the rules-only classifier (classifier/rules.rs — layer
            // 0: structured PII + confidentiality cues, recall-biased, with span
            // offsets). Both implement `Classifier`, so the message-level §7
            // gate and the tool gating chain classify identically either way.
            // Shared via Arc; a missing model dir never breaks boot.
            let classifier_models = base_path.join("models").join("classifier");
            let classifier: Arc<dyn crate::classifier::Classifier> =
                match crate::classifier::EnsembleClassifier::load(&classifier_models) {
                    Ok(ensemble) => {
                        tracing::info!(
                            target: "lhp::classifier",
                            path = %classifier_models.display(),
                            "loaded trained ONNX ensemble classifier"
                        );
                        Arc::new(ensemble)
                    }
                    Err(e) => {
                        tracing::info!(
                            target: "lhp::classifier",
                            reason = %e,
                            "trained classifier unavailable — using rules-only classifier"
                        );
                        Arc::new(RulesClassifier::new())
                    }
                };

            // Memory's meaning-lane embedder (PLAN §9): stock bge-small-en-v1.5
            // INT8 ONNX from <storage>/models/embedder/. Wrapped in an
            // `EmbedderHandle` so the ~34 MB model loads LAZILY and only when a
            // profile with semantic memory search enabled actually needs it
            // (Wave 1.2 — "loads only when the user's memory settings enable
            // it"). The dir may not exist; the handle loads on first use and
            // falls back to keyword-only if absent — so `Some` here means "a
            // model dir is configured," not "already loaded."
            let embedder_models = base_path.join("models").join("embedder");
            let embedder: Option<Arc<crate::embedder::EmbedderHandle>> =
                Some(crate::embedder::EmbedderHandle::lazy(embedder_models));

            // Boot-time backfill: embed any fact saved before the embedder was
            // installed (or whose embed-on-save failed), for profiles that have
            // semantic search on — sweeping the shared store and every walled
            // profile's own DB. Best-effort on a blocking thread; the handle
            // only forces the model load if there's actually work to do.
            if let Some(handle) = embedder.clone() {
                let storage_bf = Arc::clone(&storage);
                tauri::async_runtime::spawn_blocking(move || {
                    backfill_memory_embeddings(&storage_bf, &handle);
                });
            }

            // §7 Privacy Gate for message egress.
            let gate = PrivacyGate::new(Arc::clone(&classifier));

            // §3.5 approval spine: the shared grant ledger, the pending-prompt
            // registry, and the Tauri prompter that raises an in-app
            // confirmation and waits (deny-by-default after 5 min) for the
            // answer. The dispatcher and the gating chain share one ledger.
            let ledger = Arc::new(crate::hooks::ApprovalLedger::new());
            let approvals = Arc::new(crate::ipc::approval::ApprovalRegistry::new());
            let prompter: Arc<dyn crate::hooks::ApprovalPrompter> =
                Arc::new(crate::ipc::approval::TauriApprovalPrompter::new(
                    app.handle().clone(),
                    Arc::clone(&approvals),
                    std::time::Duration::from_secs(300),
                ));

            // The blocking `ask_human` tool's prompter + its pending-question
            // registry (shared with `resolve_ask_human`). Longer timeout than
            // an approval: answering a question is deliberative.
            let ask_human = Arc::new(crate::ipc::ask_human::AskHumanRegistry::new());
            let human_prompter: Arc<dyn crate::tools::ask_human::HumanPrompter> =
                Arc::new(crate::ipc::ask_human::TauriHumanPrompter::new(
                    app.handle().clone(),
                    Arc::clone(&ask_human),
                    std::time::Duration::from_secs(600),
                ));

            // §3 tool spine: workspace-confined read-only tools behind the
            // unified pretooluse hook chain, filtered by this body's caps.
            // State-changing tools (a later round) route through the approval
            // spine wired here. `storage` is passed in so the dispatcher
            // can build a StorageAuditWriter for the per-tool-call audit
            // log (item 5 / Q5 do-now).
            let tools = Arc::new(build_tool_dispatcher(
                &base_path,
                Arc::clone(&classifier),
                Arc::clone(&ledger),
                Some(Arc::clone(&prompter)),
                (*storage).clone(),
                embedder.clone(),
                Some(app.handle().clone()),
                Some(Arc::clone(&human_prompter)),
                Arc::clone(&model_manager),
            ));

            let agent_loop = Arc::new(
                AgentLoop::new(
                    gate,
                    Arc::clone(&model_manager),
                    Arc::clone(&storage),
                    tools,
                )
                .with_embedder(embedder.clone())
                // Wave 3.5: enable the pre-compaction flush (local-model durable
                // -fact extraction over about-to-be-trimmed turns).
                .with_flush_classifier(Arc::clone(&classifier))
                // Wave 4.2: enable autonomous skill drafting (local-model
                // reflection over a finished conversation → a Pending draft).
                // Gated at runtime by the global `skill_reflect_enabled` toggle
                // (default off); drafts are always Pending (human-reviewed).
                .with_skill_drafter(Arc::new(
                    crate::agent::skill_reflect::LocalModelDrafter::new(Arc::clone(&model_manager)),
                )),
            );

            // Wave 4.3c/4.4: the background runner that drains `work_items`
            // and actually executes a `delegate` dispatch (see
            // `tools::delegate` + `agent::work_runner` module docs for why
            // this is a separate loop rather than `delegate` running things
            // itself). Fire-and-forget: runs for the life of the process.
            crate::agent::work_runner::spawn_work_runner(
                Arc::clone(&agent_loop),
                Arc::clone(&storage),
            );

            let state = AppState {
                agent_loop,
                model_manager,
                storage,
                approvals,
                ask_human,
                classifier: Arc::clone(&classifier),
                embedder,
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
            ipc::resolve_tool_approval,
            ipc::resolve_ask_human,
            ipc::get_usage_summary,
            ipc::list_skills,
            ipc::set_skill_approval,
            ipc::delete_skill,
            ipc::get_skill_reflect_enabled,
            ipc::set_skill_reflect_enabled,
            ipc::list_seat_bindings,
            ipc::set_seat_binding,
            ipc::delete_seat_binding,
            ipc::list_agent_types,
            ipc::set_agent_type_approval,
            ipc::delete_agent_type,
            ipc::install_pack,
            ipc::list_tool_rules,
            ipc::delete_tool_rule,
            ipc::get_classifier_settings,
            ipc::set_classifier_settings,
            ipc::set_redaction_enabled,
            ipc::reset_classifier_settings,
            ipc::explain_classification,
            ipc::list_memory,
            ipc::save_memory,
            ipc::delete_memory,
            ipc::set_memory_pinned,
            ipc::get_memory_settings,
            ipc::set_memory_settings,
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
        let provider = Provider::new(ep.id, ep.name, ep.base_url, api_key, kind)
            .with_native_tools(ep.supports_native_tools);
        mm.add_provider(provider);
    }
    tracing::info!(count = mm.list_providers().len(), "hydrated providers from storage");
}

/// Build the M3 tool dispatcher for the desktop app body.
///
/// The filesystem tools are confined to a `workspace/` directory under the
/// storage root, registered into the capability-filtered registry
/// (`BodyEnv::app_default`), and placed behind the standard pretooluse gating
/// chain: privacy filter → non-overridable sandbox floor → permissions →
/// first-use confirm. Gating is derived from each tool's `RiskClass`:
/// read-only tools (read/list/search) are whole-tool `Allow`ed + pre-trusted,
/// while state-changing tools (write/edit/delete) are `Ask` and route through
/// the approval spine (the confirmation dialog).
/// Embed any memory fact that has no vector yet (saved before the embedder was
/// installed, or whose embed-on-save failed) — the meaning lane's catch-up
/// pass, run once per boot on a blocking thread. Bounded per bucket per boot
/// so a huge archive can't stall anything; the remainder catches up next boot.
fn backfill_memory_embeddings(
    storage: &crate::storage::Storage,
    embedder: &Arc<crate::embedder::EmbedderHandle>,
) {
    // Per-profile "semantic search on?" cache — a fact is only embedded if its
    // origin profile has the meaning lane enabled (Wave 1.2), so the "hard off
    // switch for computing a meaning fingerprint" is honored on the catch-up
    // path too.
    let mut enabled: std::collections::HashMap<String, bool> = std::collections::HashMap::new();

    // The shared store (facts from every non-walled profile live here).
    backfill_one_memory_db(storage.global(), embedder, storage, &mut enabled);

    // Each walled profile's own memory DB (§7 / Wave 1.5).
    if let Ok(names) = storage.list_profile_names() {
        for name in names {
            let walled = storage
                .open_profile(&name)
                .and_then(|db| db.memory_settings())
                .map(|s| s.walled)
                .unwrap_or(false);
            if !walled {
                continue;
            }
            if let Ok(mem) = storage.memory_db_for_profile(&name) {
                backfill_one_memory_db(&mem, embedder, storage, &mut enabled);
            }
        }
    }
}

/// Backfill embeddings for one memory DB, skipping facts whose origin profile
/// has semantic search off. The embedder loads (once, memoized) only when the
/// first eligible fact is reached — so a fully-disabled/absent setup never
/// forces the model load.
fn backfill_one_memory_db(
    mem: &crate::storage::GlobalDb,
    embedder: &Arc<crate::embedder::EmbedderHandle>,
    storage: &crate::storage::Storage,
    enabled: &mut std::collections::HashMap<String, bool>,
) {
    use crate::storage::MemoryBucket;
    const BACKFILL_CAP_PER_BUCKET: usize = 512;

    for bucket in [MemoryBucket::Shared, MemoryBucket::PrivateLocal] {
        let pending = match mem.facts_missing_embedding(bucket, BACKFILL_CAP_PER_BUCKET) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(target: "lhp::memory", error = %e, "embedding backfill: listing failed");
                continue;
            }
        };
        if pending.is_empty() {
            continue;
        }
        let mut done = 0usize;
        let total = pending.len();
        for fact in pending {
            let on = *enabled
                .entry(fact.origin_profile.clone())
                .or_insert_with(|| {
                    crate::tools::memory::semantic_search_enabled(storage, &fact.origin_profile)
                });
            if !on {
                continue;
            }
            // Load the model on the first eligible fact; absent ⇒ stop (nothing
            // to embed anywhere this pass).
            let Some(emb) = embedder.get() else { return };
            match emb.embed_passage(&fact.content) {
                Ok(v) => match mem.upsert_memory_embedding(bucket, &fact.id, &v) {
                    Ok(()) => done += 1,
                    Err(e) => tracing::warn!(target: "lhp::memory", error = %e, fact = %fact.id,
                        "embedding backfill: store failed"),
                },
                Err(e) => tracing::warn!(target: "lhp::memory", error = %e, fact = %fact.id,
                    "embedding backfill: embed failed"),
            }
        }
        tracing::info!(target: "lhp::memory", ?bucket, done, total, "embedding backfill pass");
    }
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)] // wiring seam: one place threads every tool dep.
fn build_tool_dispatcher(
    base_path: &std::path::Path,
    classifier: Arc<dyn crate::classifier::Classifier>,
    ledger: Arc<crate::hooks::ApprovalLedger>,
    approver: Option<Arc<dyn crate::hooks::ApprovalPrompter>>,
    storage: crate::storage::Storage,
    embedder: Option<Arc<crate::embedder::EmbedderHandle>>,
    app_handle: Option<tauri::AppHandle>,
    human_prompter: Option<Arc<dyn crate::tools::ask_human::HumanPrompter>>,
    model_manager: Arc<ModelManager>,
) -> ToolDispatcher {
    use crate::hooks::{
        build_pretooluse_chain_full, AuditObserverHook, AuditWriter, InMemoryPolicySource,
        LayeredPolicySource, PermissionMode, SqlitePolicySource, StorageAuditWriter,
        StorageToolRuleWriter, ToolRuleWriter,
    };
    use crate::tools::fs::{
        DeleteFileTool, EditFileTool, ListDirTool, ReadFileTool, SearchFilesTool, WriteFileTool,
    };
    use crate::tools::{BodyEnv, RiskClass, ToolRegistry};

    let workspace = base_path.join("workspace");
    if let Err(e) = std::fs::create_dir_all(&workspace) {
        tracing::warn!(
            error = %e,
            path = %workspace.display(),
            "failed to create the tool workspace directory"
        );
    }
    // Captured before `workspace` is moved into `DeleteFileTool::new` below,
    // so the protected-path floor can resolve a call's `path` arg the same
    // way the fs tools do (following symlinks) and catch an in-workspace
    // symlink aliasing a protected dir.
    let hook_workspace_root = workspace.clone();

    let mut registry = ToolRegistry::new();
    registry.register(Box::new(ReadFileTool::new(workspace.clone())));
    registry.register(Box::new(ListDirTool::new(workspace.clone())));
    registry.register(Box::new(SearchFilesTool::new(workspace.clone())));
    registry.register(Box::new(WriteFileTool::new(&workspace)));
    registry.register(Box::new(EditFileTool::new(&workspace)));
    registry.register(Box::new(DeleteFileTool::new(workspace)));

    // Memory tools (PLAN §9). `recall_memory` is the always-available pinned
    // search tool — read-only (Safe → pre-trusted), searches the SHARED store
    // only so it can never surface a private-local fact into model context.
    // `remember` saves a fact routed by sensitivity — Write-risk, so it goes
    // through the approval spine (non-silent, gated).
    registry.register(Box::new(
        crate::tools::memory::RecallMemoryTool::new(storage.clone())
            .with_embedder(embedder.clone()),
    ));
    registry.register(Box::new(
        crate::tools::memory::RememberMemoryTool::new(storage.clone(), Arc::clone(&classifier))
            .with_embedder(embedder)
            .with_app_handle(app_handle),
    ));
    // Wave 2.1: the agent's recall over past conversations (read-only, Safe,
    // profile-scoped) — distinct from memory (`recall_memory`).
    registry.register(Box::new(crate::tools::session_search::SessionSearchTool::new(
        storage.clone(),
    )));
    // Wave 2.1: a read-only local status snapshot (OS/arch, profiles, model
    // install state). Safe → pre-trusted.
    registry.register(Box::new(crate::tools::system_status::SystemStatusTool::new(
        storage.clone(),
    )));
    // Wave 2.1: cron management — profile-scoped scheduled-job CRUD. Listing is
    // read-only (Safe → pre-trusted); mutating (create/enable/disable/delete)
    // is Write, so it routes through the approval spine. No scheduler runs these
    // yet (that's the one-queue-model pass, Wave 4.4) — this is the intent CRUD.
    registry.register(Box::new(crate::tools::cron::ListCronJobsTool::new(
        storage.clone(),
    )));
    registry.register(Box::new(crate::tools::cron::ManageCronTool::new(
        storage.clone(),
    )));
    // Wave 2.1: the web-content tool (the "headless browser" slot at v1 — an
    // SSRF-guarded HTTP GET + readable-text extraction). The FIRST External
    // (egress) tool: RiskClass::External → approval spine + a surfaced
    // destination; every hop re-validated (scheme + private-host + DNS/IP
    // block-list) so it can never reach localhost/RFC-1918/metadata.
    registry.register(Box::new(crate::tools::fetch::FetchUrlTool::new()));
    // Wave 2.1: the single blocking "ask the user" tool. Safe → pre-trusted
    // (asking a question has no side effect); it blocks the loop until the user
    // answers via `resolve_ask_human`. `None` prompter (no UI) ⇒ it reports
    // "no interactive user" instead of hanging.
    registry.register(Box::new(crate::tools::ask_human::AskHumanTool::new(
        human_prompter,
    )));
    // Wave 4.1: skills — reusable playbooks as tools. `search_skills` (Safe →
    // pre-trusted) loads a relevant approved skill's body; `save_skill` (Write →
    // approval spine) stores a new one (the prompt showing the content is the
    // review). A skill's body re-gates whatever tools it drives.
    registry.register(Box::new(crate::tools::skills::SearchSkillsTool::new(
        storage.clone(),
    )));
    registry.register(Box::new(crate::tools::skills::SaveSkillTool::new(
        storage.clone(),
    )));
    // Wave 4.3c: `delegate` — dispatch a bounded, approved-persona helper
    // sub-agent. Only ENQUEUES a `work_items` row (RiskClass::Dangerous —
    // always-shown Once-only Ask); the background `WorkQueueRunner`
    // (spawned in `run()`, after the `AgentLoop` Arc exists) drains it and
    // actually runs the helper. See `tools::delegate` module docs for why
    // this tool can't hold an `Arc<AgentLoop>` itself.
    registry.register(Box::new(crate::tools::delegate::DelegateTool::new(
        storage.clone(),
        model_manager,
    )));

    // Item 7: the guarded shell executor. Confined to `workspace/` + a `tmp/`
    // scratch dir, network off by default, killed on timeout, OS-sandboxed via
    // Seatbelt on macOS (UnsupportedSandbox — a hard error — elsewhere until a
    // Linux/Windows backend lands). RiskClass::Dangerous, so every call
    // re-prompts (no standing grant — see dispatch's Approve arm).
    let tmp_root = base_path.join("tmp");
    if let Err(e) = std::fs::create_dir_all(&tmp_root) {
        tracing::warn!(
            error = %e,
            path = %tmp_root.display(),
            "failed to create the tool tmp scratch dir"
        );
    }
    #[cfg(target_os = "macos")]
    let spawner: Arc<dyn crate::tools::exec::SandboxedSpawn> =
        Arc::new(crate::tools::exec::MacSeatbeltSpawn);
    #[cfg(not(target_os = "macos"))]
    let spawner: Arc<dyn crate::tools::exec::SandboxedSpawn> =
        Arc::new(crate::tools::exec::UnsupportedSandbox);
    registry.register(Box::new(crate::tools::exec::ShellExecTool::new(
        hook_workspace_root.clone(),
        tmp_root,
        spawner,
        std::time::Duration::from_secs(120),
    )));

    // Policy DERIVED from each tool's RiskClass, so adding a tool can't
    // accidentally skip the gate: read-only (Safe) tools are whole-tool
    // `Allow`ed AND pre-trusted (no prompt); every state-changing tool is
    // `Ask`, so it routes through the approval spine (the confirmation dialog)
    // and is deliberately NOT pre-confirmed.
    let env = BodyEnv::app_default();
    let mut policy = InMemoryPolicySource::new();
    let mut pre_trusted: Vec<String> = Vec::new();
    for tool in registry.available_tools(&env) {
        match tool.risk() {
            RiskClass::Safe => {
                policy.set_mode(tool.name(), PermissionMode::Allow);
                pre_trusted.push(tool.name().to_string());
            }
            RiskClass::Write | RiskClass::External | RiskClass::Dangerous => {
                policy.set_mode(tool.name(), PermissionMode::Ask);
            }
        }
    }
    let pre_trusted_refs: Vec<&str> = pre_trusted.iter().map(String::as_str).collect();

    // Q5 do-now (item 5): build the post-tool-use audit writer once, share
    // the same `Arc<dyn AuditWriter>` between (a) the dispatcher's direct
    // write_audit path and (b) the chain's observer registry, so the
    // migration of dispatch to `notify_observers` later is just a call-site
    // refactor — the writer side is already in place. The dispatcher's
    // `write_audit` is the path actually used today; the observer
    // registration is for structural completeness.
    let audit_writer: Arc<dyn AuditWriter> = Arc::new(StorageAuditWriter::new(storage.clone()));
    // Q8: the durable per-profile `tool_rules` writer for "Always allow".
    let rule_writer: Arc<dyn ToolRuleWriter> =
        Arc::new(StorageToolRuleWriter::new(storage.clone()));

    // Q8: persisted per-profile `tool_rules` (SqlitePolicySource) layered OVER
    // the risk-derived in-memory defaults. `mode_for` still comes from the
    // defaults; persisted rules are read live on the gating path and resolved
    // through the same deny>ask>allow / most-specific-wins path.
    let layered = LayeredPolicySource::new(
        Box::new(policy),
        Box::new(SqlitePolicySource::new(storage)),
    );

    let mut chain = build_pretooluse_chain_full(
        PrivacyGate::new(classifier),
        Box::new(layered),
        &pre_trusted_refs,
        Arc::clone(&ledger),
        Some(hook_workspace_root),
    );
    chain.register_observer(Box::new(AuditObserverHook::new(Arc::clone(
        &audit_writer,
    ))));

    ToolDispatcher::new(registry, chain, env)
        .with_approval(ledger, approver)
        .with_audit_writer(audit_writer)
        .with_rule_writer(rule_writer)
}
