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
mod email; // Email round (stage 1): Gmail OAuth (PKCE) + REST client — toolized in stage 2
mod hooks; // M3: Hook chain — privacy filter + permission + sandbox + first-use
mod ipc; // M1: Tauri command handlers
mod models; // M4: Model manager (local + cloud)
mod packs; // M7 (Wave 4.5): Capability Packs — installable skill+agent+cron bundles
mod platform; // M5: cross-platform computer use (cfg'd submodules)
mod queue; // M4 (Wave 4.4): the one-queue-model substrate (deferred work)
mod secrets; // provider credentials: OS keychain + test seam + legacy migration
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

            // Provider credentials are held by the OS credential store. Move
            // any legacy plaintext endpoint blobs before hydrating clients;
            // migration is idempotent and clears each blob only after the
            // corresponding keychain write succeeds.
            let provider_secrets: Arc<dyn crate::secrets::ProviderSecretStore> =
                Arc::new(crate::secrets::OsProviderSecretStore::new());
            let secret_migration = crate::secrets::migrate_legacy_provider_secrets(
                storage.global(),
                provider_secrets.as_ref(),
            );
            if secret_migration.failed > 0 {
                tracing::warn!(
                    migrated = secret_migration.migrated,
                    failed = secret_migration.failed,
                    "some legacy provider keys could not be moved to the OS keychain"
                );
            }

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
            hydrate_providers_from_storage(
                &storage,
                &model_manager,
                provider_secrets.as_ref(),
            );

            // M8 S4 boot pass (right after provider hydration, same discipline
            // as crash-recovery): reap sidecars orphaned by a hard crash of the
            // previous run, then integrity-sweep the model catalog — a missing
            // or truncated model file is quarantined, never silently served
            // (verified-before-runnable corollary 3). Cheap existence+size
            // checks only; full re-hash is an opt-in Settings action. Does NOT
            // spawn anything — spawn stays lazy. Best-effort, never bricks boot.
            #[cfg(feature = "local-runner")]
            {
                let pid_dir = base_path.join("models").join("local");
                let reaped = crate::models::runner::reap_orphan_sidecars(&pid_dir);
                if reaped > 0 {
                    tracing::warn!(reaped, "reaped orphaned sidecar process(es) at boot");
                }
                let report =
                    crate::models::runner::sweep_local_model_integrity_at_boot(&storage, false);
                if !report.quarantined.is_empty() {
                    tracing::warn!(
                        quarantined = ?report.quarantined,
                        "boot integrity sweep quarantined local model(s)"
                    );
                }
            }

            // M8 S4: the bundled llama.cpp sidecar context. `None` when no
            // binary resolves (e.g. a build without the vendored tree) — local
            // models then need an external runner, exactly the pre-S4 behavior.
            #[cfg(feature = "local-runner")]
            let local_runner_ctx: Option<Arc<crate::models::runner::LocalRunnerContext>> =
                match resolve_sidecar_bin(app) {
                    Some(bin) => {
                        tracing::info!(bin = %bin.display(), "bundled llama.cpp sidecar available");
                        Some(Arc::new(crate::models::runner::LocalRunnerContext {
                            supervisor: Arc::new(crate::models::runner::LocalRunnerSupervisor::real(
                                base_path.join("models").join("local"),
                            )),
                            paths: crate::models::runner::SidecarPaths { bin },
                        }))
                    }
                    None => {
                        tracing::info!(
                            "no sidecar binary resolved — local models need an external runner"
                        );
                        None
                    }
                };

            // Privacy classifier. The trained ONNX ensemble (bge-small +
            // distilbert, INT8) fused with the layer-0 rules, loaded from
            // <storage>/models/classifier/ if its models are installed;
            // otherwise the rules-only classifier (classifier/rules.rs — layer
            // 0: structured PII + confidentiality cues, recall-biased, with span
            // offsets). Both implement `Classifier`, so the message-level §7
            // gate and the tool gating chain classify identically either way.
            // Shared via Arc; a missing model dir never breaks boot.
            // C-01: ONE shared health flag, read by every gate AND by the IPC
            // layer (`get_classifier_health` → the UI's degraded banner). A
            // degraded flag nobody can observe is not a fix, so this is
            // deliberately shared state rather than a per-gate bool.
            let classifier_models = base_path.join("models").join("classifier");
            let classifier_health: Arc<crate::classifier::ClassifierHealth>;
            let classifier: Arc<dyn crate::classifier::Classifier> =
                match crate::classifier::EnsembleClassifier::load(&classifier_models) {
                    Ok(ensemble) => {
                        tracing::info!(
                            target: "lhp::classifier",
                            path = %classifier_models.display(),
                            "loaded trained ONNX ensemble classifier"
                        );
                        classifier_health = crate::classifier::ClassifierHealth::healthy();
                        Arc::new(ensemble)
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "lhp::classifier",
                            reason = %e,
                            "trained classifier unavailable — fail-closed (degraded mode)"
                        );
                        classifier_health =
                            crate::classifier::ClassifierHealth::degraded_with(e.to_string());
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

            // §7 Privacy Gate for message egress. C-01: shares the boot health
            // flag, so a failed trained-classifier load keeps Auto+cloud local.
            let gate = PrivacyGate::with_health(
                Arc::clone(&classifier),
                Arc::clone(&classifier_health),
            );

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

            // The email round's needs-reconnect set, created ONCE here and
            // shared between the agent tool path (threaded into
            // `build_tool_dispatcher` → `EmailToolDeps`, below) and the
            // screen IPC path (`ipc::EmailRuntime`, at `AppState`
            // construction) — a dead Gmail grant hit from EITHER path must
            // flip the SAME flag, or the reconnect banner only ever lights
            // from the screen.
            let email_needs_reconnect =
                Arc::new(parking_lot::Mutex::new(std::collections::HashSet::new()));

            // §3 tool spine: workspace-confined read-only tools behind the
            // unified pretooluse hook chain, filtered by this body's caps.
            // State-changing tools (a later round) route through the approval
            // spine wired here. `storage` is passed in so the dispatcher
            // can build a StorageAuditWriter for the per-tool-call audit
            // log (item 5 / Q5 do-now).
            let tools = Arc::new(build_tool_dispatcher(
                &base_path,
                Arc::clone(&classifier),
                // C-01: the tool-hook chain's gate must degrade with the same
                // flag as the message gate — otherwise the tool path silently
                // bypasses fail-closed handling.
                Arc::clone(&classifier_health),
                Arc::clone(&ledger),
                Some(Arc::clone(&prompter)),
                (*storage).clone(),
                embedder.clone(),
                Some(app.handle().clone()),
                Some(Arc::clone(&human_prompter)),
                Arc::clone(&model_manager),
                Arc::clone(&provider_secrets),
                Arc::clone(&email_needs_reconnect),
            ));

            // C4: register every ALREADY-APPROVED skill as a callable Tool at
            // boot — skills approved in a prior session stay callable across
            // restarts. Best-effort (a bad row is skipped, never bricks boot);
            // the wrapper re-checks approval from storage at every call anyway.
            match storage.global().list_approved_skills() {
                Ok(skills) => {
                    for skill in &skills {
                        if let Some(tool) = crate::tools::skills::SkillTool::for_skill(
                            skill,
                            Arc::clone(&storage),
                        ) {
                            tools.hot_register(Box::new(tool));
                        }
                    }
                    if !skills.is_empty() {
                        tracing::info!(count = skills.len(), "registered approved skills as tools");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "couldn't load approved skills as tools"),
            }

            // C3: the live MCP runtime (spawned children, derived session state).
            let mcp_runtime = Arc::new(crate::tools::mcp_stdio::McpRuntime::new());

            // Clone BEFORE the loop takes ownership — `PrivacyGate::clone` shares
            // the health flag and the confirmation store, so `AppState.gate` and
            // the loop's gate are the same gate for observation/confirmation.
            let gate_for_state = gate.clone();
            let agent_loop = AgentLoop::new(
                gate,
                Arc::clone(&model_manager),
                Arc::clone(&storage),
                Arc::clone(&tools),
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
                crate::agent::skill_reflect::LocalModelDrafter::new(
                    Arc::clone(&model_manager),
                    Arc::clone(&storage),
                ),
            ));
            // M8 S4: hand the loop the lazy-spawn seam (when a sidecar resolved).
            #[cfg(feature = "local-runner")]
            let agent_loop = match &local_runner_ctx {
                Some(ctx) => agent_loop.with_local_runner(Arc::clone(ctx)),
                None => agent_loop,
            };
            let agent_loop = Arc::new(agent_loop);

            // Wave 4.3c/4.4: the background runner that drains `work_items`
            // and actually executes a `delegate` dispatch (see
            // `tools::delegate` + `agent::work_runner` module docs for why
            // this is a separate loop rather than `delegate` running things
            // itself). Fire-and-forget: runs for the life of the process.
            crate::agent::work_runner::spawn_work_runner(
                Arc::clone(&agent_loop),
                Arc::clone(&storage),
            );

            // M8 S4: periodic idle sweep — an unused sidecar shuts down after
            // ~10 min (in-flight-guarded; a long generation is never killed).
            #[cfg(feature = "local-runner")]
            if let Some(ctx) = &local_runner_ctx {
                let sup = Arc::clone(&ctx.supervisor);
                tauri::async_runtime::spawn(async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                        sup.idle_sweep().await;
                    }
                });
            }

            // Retention sweep: routing decisions for 7 days, terminal work for
            // 30 days, and redacted tool audit observations for 90 days. Usage
            // events are intentionally retained because they back month-to-date
            // budgets and spend history. Best-effort across every profile DB:
            // never brick boot, run once immediately, then hourly.
            {
                const TRM_RETENTION_SECS: i64 = 7 * 24 * 60 * 60;
                const WORK_RETENTION_SECS: i64 = 30 * 24 * 60 * 60;
                const TOOL_AUDIT_RETENTION_SECS: i64 = 90 * 24 * 60 * 60;
                let storage_retention = Arc::clone(&storage);
                tauri::async_runtime::spawn(async move {
                    loop {
                        let now = chrono::Utc::now().timestamp();
                        if let Ok(names) = storage_retention.list_profile_names() {
                            for name in names {
                                if let Ok(db) = storage_retention.open_profile(&name) {
                                    let results = [
                                        (
                                            "TRM log",
                                            db.purge_trm_logs_older_than(now - TRM_RETENTION_SECS),
                                        ),
                                        (
                                            "terminal work-item",
                                            db.purge_terminal_work_items_older_than(
                                                now - WORK_RETENTION_SECS,
                                            ),
                                        ),
                                        (
                                            "tool-audit",
                                            db.purge_tool_audit_older_than(
                                                now - TOOL_AUDIT_RETENTION_SECS,
                                            ),
                                        ),
                                    ];
                                    for (kind, result) in results {
                                        if let Err(e) = result {
                                            tracing::warn!(
                                                target: "lhp::retention",
                                                profile = %name, kind, error = %e,
                                                "retention purge failed for profile"
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                    }
                });
            }

            // C3: a storage handle for the background MCP boot task below (the
            // AppState construction MOVES `storage` into its field).
            let storage_mcp = Arc::clone(&storage);

            // A4: probe hardware ONCE at boot and cache it (probe() shells out to
            // system_profiler — hundreds of ms; the model IPC reads this snapshot).
            let hardware = Arc::new(crate::models::hardware::probe());
            let state = AppState {
                agent_loop,
                email: Arc::new(crate::ipc::EmailRuntime::with_shared_reconnect(Arc::clone(
                    &email_needs_reconnect,
                ))),
                model_manager,
                storage,
                provider_secrets,
                approvals,
                ask_human,
                classifier: Arc::clone(&classifier),
                // C-01 / H-12: same gate the loop enforces with (Arc-shared
                // health flag + one-send confirmation store).
                gate: gate_for_state,
                embedder,
                // C4: the live dispatcher, for hot-(un)registering skill tools.
                tools: Arc::clone(&tools),
                // C3: the live MCP server registry.
                mcp: Arc::clone(&mcp_runtime),
                hardware,
                #[cfg(feature = "local-runner")]
                local_runner: local_runner_ctx,
            };
            app.manage(state);

            // C3: bring persisted MCP servers back up in the background
            // (best-effort — a missing server binary logs and skips; the row
            // stays listed as not-running so the user can see + fix it).
            {
                let tools_mcp = Arc::clone(&tools);
                let runtime_mcp = Arc::clone(&mcp_runtime);
                tauri::async_runtime::spawn(async move {
                    let rows = match storage_mcp.global().list_mcp_servers() {
                        Ok(rows) => rows,
                        Err(e) => {
                            tracing::warn!(error = %e, "couldn't load persisted MCP servers");
                            return;
                        }
                    };
                    for row in rows.into_iter().filter(|r| r.enabled) {
                        match crate::tools::mcp_stdio::bring_up_server(&row, &tools_mcp, &runtime_mcp)
                            .await
                        {
                            Ok(tools) => {
                                // Review fix (boot-vs-remove race): the user may
                                // have REMOVED this server while the sequential
                                // bring-up was still working through earlier rows
                                // (each can burn a full RPC timeout). If the row
                                // is gone, tear the live half straight back down —
                                // never a running, invisible server without a row.
                                match storage_mcp.global().get_mcp_server(&row.id) {
                                    Ok(Some(_)) => tracing::info!(
                                        server = %row.name,
                                        tools = tools.len(),
                                        "MCP server up"
                                    ),
                                    _ => {
                                        tracing::warn!(
                                            server = %row.name,
                                            "MCP server row removed during boot bring-up — tearing down"
                                        );
                                        crate::tools::mcp_stdio::tear_down_server(
                                            &row.id,
                                            &tools_mcp,
                                            &runtime_mcp,
                                        )
                                        .await;
                                    }
                                }
                            }
                            Err(e) => tracing::warn!(
                                server = %row.name,
                                error = %e,
                                "MCP server failed to start (listed as not-running)"
                            ),
                        }
                    }
                });
            }

            tracing::info!("Tauri app initialized; M1 commands registered");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ipc::get_app_version,
            ipc::get_active_profile,
            ipc::set_active_profile,
            ipc::list_profiles,
            ipc::list_conversations,
            ipc::create_conversation,
            ipc::set_conversation_binding,
            ipc::get_messages,
            ipc::list_providers,
            ipc::add_provider,
            ipc::update_provider,
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
            ipc::probe_hardware,
            ipc::search_models,
            ipc::get_model_detail,
            ipc::calculate_model_fit,
            ipc::get_sandbox_config,
            ipc::set_sandbox_config,
            ipc::get_budget_settings,
            ipc::set_budget_settings,
            ipc::reset_budget_settings,
            ipc::cancel_message,
            ipc::register_mcp_server,
            ipc::list_mcp_servers,
            ipc::remove_mcp_server,
            ipc::download_model,
            ipc::list_local_models,
            ipc::remove_local_model,
            ipc::list_tool_rules,
            ipc::delete_tool_rule,
            ipc::list_cron_jobs,
            ipc::set_cron_job_enabled,
            ipc::delete_cron_job,
            ipc::list_workspace_files,
            ipc::gmail_setup_status,
            ipc::set_gmail_client,
            ipc::gmail_begin_connect,
            ipc::gmail_finish_connect,
            ipc::gmail_disconnect,
            ipc::list_email,
            ipc::read_email,
            ipc::send_email,
            ipc::list_calendar_events,
            ipc::create_calendar_event,
            ipc::delete_calendar_event,
            ipc::list_google_tasks,
            ipc::create_google_task,
            ipc::set_google_task_completed,
            ipc::delete_google_task,
            ipc::get_classifier_settings,
            ipc::set_classifier_settings,
            ipc::set_redaction_enabled,
            ipc::reset_classifier_settings,
            ipc::explain_classification,
            // C-01 / H-12: the read side of the degraded flag + the one-send
            // confirmation grant.
            ipc::get_classifier_health,
            ipc::confirm_public_send,
            ipc::list_memory,
            ipc::save_memory,
            ipc::delete_memory,
            ipc::set_memory_pinned,
            ipc::get_memory_settings,
            ipc::set_memory_settings,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Lost Harness")
        .run(|app_handle, event| {
            // Exit teardown — never a zombie: on app exit, best-effort stop of
            // every live sidecar (M8 S4; the pidfile reap at next boot is the
            // net for a hard crash that never reaches this) and every spawned
            // MCP child (C3).
            if matches!(event, tauri::RunEvent::Exit) {
                use tauri::Manager;
                if let Some(state) = app_handle.try_state::<AppState>() {
                    #[cfg(feature = "local-runner")]
                    if let Some(ctx) = &state.local_runner {
                        let sup = Arc::clone(&ctx.supervisor);
                        tauri::async_runtime::block_on(async move { sup.stop_all().await });
                    }
                    // C3 (review nit): kill spawned MCP children on exit —
                    // kill_on_drop only fires if the Child is dropped, and a
                    // process exit doesn't drop AppState; a stdin-EOF-ignoring
                    // server would otherwise outlive the app.
                    let entries: Vec<_> = state.mcp.servers.lock().drain().collect();
                    if !entries.is_empty() {
                        tauri::async_runtime::block_on(async move {
                            for (_, entry) in entries {
                                entry.transport.shutdown().await;
                            }
                        });
                    }
                }
            }
            let _ = (&app_handle, &event);
        });
}

/// Resolve the vendored `llama-server` binary (M8 S4). Order: the
/// `LHP_LLAMA_SERVER_BIN` env override (dev/live-test) → the app bundle's
/// resources (`bundle.resources` maps `vendor/llama-cpp` → `llama-cpp`) → the
/// repo's vendor tree (debug/`tauri dev` fallback). `None` = no sidecar; local
/// models then need an external runner (honest degradation, logged).
#[cfg(feature = "local-runner")]
fn resolve_sidecar_bin(app: &tauri::App) -> Option<PathBuf> {
    use tauri::Manager;
    let bin = 'find: {
        if let Some(p) = std::env::var_os("LHP_LLAMA_SERVER_BIN").map(PathBuf::from) {
            if p.is_file() {
                break 'find Some(p);
            }
            tracing::warn!(path = %p.display(), "LHP_LLAMA_SERVER_BIN set but not a file — ignoring");
        }
        if let Ok(p) = app.path().resolve(
            "llama-cpp/macos-arm64/llama-server",
            tauri::path::BaseDirectory::Resource,
        ) {
            if p.is_file() {
                break 'find Some(p);
            }
        }
        let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("vendor/llama-cpp/macos-arm64/llama-server");
        if dev.is_file() {
            break 'find Some(dev);
        }
        None
    }?;
    // Finding #4: a `bundle.resources` copy can lose its executable bit through
    // the packaging pipeline (unlike Tauri's `externalBin`, which the design
    // deliberately can't use — the sidecar is 9 dylibs, not one file). Set +x
    // defensively so the lazy spawn doesn't fail with EACCES on a shipped build.
    // (Gatekeeper/notarization coverage of `Resources/` is the design's flagged
    // build-verify item — it requires a real packaged build to check, so it's
    // out of headless scope.)
    ensure_executable(&bin);
    Some(bin)
}

/// Ensure `path` has the owner-executable bit set (best-effort; unix only).
#[cfg(unix)]
fn ensure_executable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        let mode = perms.mode();
        if mode & 0o111 != 0o111 {
            perms.set_mode(mode | 0o755);
            let _ = std::fs::set_permissions(path, perms);
        }
    }
}

#[cfg(not(unix))]
fn ensure_executable(_path: &std::path::Path) {}

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
    secrets: &dyn crate::secrets::ProviderSecretStore,
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
        let api_key = if ep.has_keychain_secret() {
            match secrets.get(&ep.id) {
                Ok(secret) => secret,
                Err(e) => {
                    tracing::warn!(endpoint = %ep.id, error = %e, "provider keychain read failed");
                    None
                }
            }
        } else {
            None
        };
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
    // C-01: the SAME health flag the message-egress gate holds.
    classifier_health: Arc<crate::classifier::ClassifierHealth>,
    ledger: Arc<crate::hooks::ApprovalLedger>,
    approver: Option<Arc<dyn crate::hooks::ApprovalPrompter>>,
    storage: crate::storage::Storage,
    embedder: Option<Arc<crate::embedder::EmbedderHandle>>,
    app_handle: Option<tauri::AppHandle>,
    human_prompter: Option<Arc<dyn crate::tools::ask_human::HumanPrompter>>,
    model_manager: Arc<ModelManager>,
    provider_secrets: Arc<dyn crate::secrets::ProviderSecretStore>,
    // Shared with `ipc::EmailRuntime` — see the call site's comment in `run()`.
    email_needs_reconnect: Arc<parking_lot::Mutex<std::collections::HashSet<String>>>,
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

    // C2: keep a handle for the durability journal (Storage is Arc-backed —
    // cheap clone), taken up front before `storage` is moved below.
    let journal_storage = storage.clone();

    let workspace = base_path.join("workspace");
    if let Err(e) = std::fs::create_dir_all(&workspace) {
        tracing::warn!(
            error = %e,
            path = %workspace.display(),
            "failed to create the tool workspace directory"
        );
    }
    // M7 Tier-P: one-time, idempotent migration of any LEGACY shared workspace
    // (loose files written before per-profile confinement, which pooled directly
    // at `workspace/*`) into the default profile's subtree, so a pre-upgrade
    // user's files stay reachable instead of being stranded outside every
    // re-rooted fs tool. Default = "personal" (matches `ipc::get_active_profile`).
    // Moves regular files ONLY — never a directory — so a profile tree can never
    // be mis-attributed (see the fn's structural-invariant docs). Runs before any
    // tool is registered/used; a failure here must not block boot.
    if let Err(e) = crate::tools::fs::migrate_legacy_workspace(&workspace, "personal") {
        tracing::warn!(
            error = %e,
            path = %workspace.display(),
            "legacy-workspace migration failed; pre-upgrade files may be unreachable until resolved"
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
    // The email round (2026-07-24, M7-Q2): Gmail tools over the user's OWN
    // OAuth client (per-user; per-profile connection). search/read are
    // External (off-box egress with a surfaced destination — the F2 gate
    // means a Private turn can't reach them even on a local model); send is
    // Dangerous (irreversible; Once-only Ask + the C2 journal). If the
    // production token endpoint can't construct (a TLS-stack failure — never
    // observed), email tools are simply absent rather than half-wired.
    match crate::email::oauth::HttpTokenEndpoint::new() {
        Ok(endpoint) => {
            let deps = crate::tools::email::EmailToolDeps {
                secrets: provider_secrets,
                endpoint: Arc::new(endpoint),
                needs_reconnect: email_needs_reconnect,
            };
            registry.register(Box::new(crate::tools::email::EmailSearchTool::new(deps.clone())));
            registry.register(Box::new(crate::tools::email::EmailReadTool::new(deps.clone())));
            registry.register(Box::new(crate::tools::email::EmailSendTool::new(deps.clone())));
            let productivity = crate::tools::productivity::ProductivityToolDeps::new(deps);
            registry.register(Box::new(
                crate::tools::productivity::CalendarListTool::new(productivity.clone()),
            ));
            registry.register(Box::new(
                crate::tools::productivity::CalendarCreateTool::new(productivity.clone()),
            ));
            registry.register(Box::new(
                crate::tools::productivity::CalendarDeleteTool::new(productivity.clone()),
            ));
            registry.register(Box::new(
                crate::tools::productivity::TaskListTool::new(productivity.clone()),
            ));
            registry.register(Box::new(
                crate::tools::productivity::TaskCreateTool::new(productivity.clone()),
            ));
            registry.register(Box::new(
                crate::tools::productivity::TaskCompleteTool::new(productivity.clone()),
            ));
            registry.register(Box::new(
                crate::tools::productivity::TaskDeleteTool::new(productivity),
            ));
        }
        Err(e) => tracing::warn!(error = %e, "email tools unavailable (token endpoint failed to build)"),
    }
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
    registry.register(Box::new(
        crate::tools::exec::ShellExecTool::new(
            hook_workspace_root.clone(),
            tmp_root,
            spawner,
            std::time::Duration::from_secs(120),
        )
        // M7 Tier-K Slice 2: enforce the caller's per-profile sandbox_config
        // network ceiling at run time.
        .with_storage(storage.clone()),
    ));

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
        // C-01: previously `PrivacyGate::new(classifier)` — a fresh NON-degraded
        // gate, so the tool path never participated in fail-closed handling.
        PrivacyGate::with_health(classifier, classifier_health),
        Box::new(layered),
        &pre_trusted_refs,
        Arc::clone(&ledger),
        Some(hook_workspace_root),
    );
    chain.register_observer(Box::new(AuditObserverHook::new(Arc::clone(
        &audit_writer,
    ))));

    // C6 / M5: the `ui_*` act tools + the OnScreenActionHook. macOS receives
    // the concrete Accessibility/Quartz backend; other platforms keep the
    // explicit unavailable fallback until an equivalent native backend lands.
    // The hook appends AFTER generic gates so an irreversible target's
    // `covers_once` floor is checked before its Once grant is consumed.
    #[cfg(feature = "computer-use")]
    {
        use crate::tools::computer_backend::ComputerBackend;
        use crate::tools::computer_tools::{
            UiClickTool, UiDragTool, UiKeyTool, UiScrollTool, UiTypeTool,
        };
        #[cfg(target_os = "macos")]
        let backend: Arc<dyn ComputerBackend> =
            Arc::new(crate::platform::macos::MacOsComputerBackend::new());
        #[cfg(not(target_os = "macos"))]
        let backend: Arc<dyn ComputerBackend> =
            Arc::new(crate::tools::computer_backend::UnavailableBackend);
        registry.register(Box::new(UiScrollTool::new(Arc::clone(&backend))));
        registry.register(Box::new(UiClickTool::new(Arc::clone(&backend))));
        registry.register(Box::new(UiTypeTool::new(Arc::clone(&backend))));
        registry.register(Box::new(UiKeyTool::new(Arc::clone(&backend))));
        registry.register(Box::new(UiDragTool::new(Arc::clone(&backend))));
        chain.register_gating(Box::new(
            crate::hooks::on_screen_action::OnScreenActionHook::new(backend)
                .with_ledger(Arc::clone(&ledger)),
        ));
    }

    ToolDispatcher::new(registry, chain, env)
        .with_approval(ledger, approver)
        .with_audit_writer(audit_writer)
        .with_rule_writer(rule_writer)
        // C2: the durability journal — every mutating tool execution writes a
        // work_items row before the effect (idempotency-keyed, crash-reconciled).
        .with_journal(Arc::new(journal_storage))
}

// ── C-01: the TOOL path must degrade with the message path ──────────────────

/// This module exists for exactly one reason: `build_tool_dispatcher` builds the
/// SECOND `PrivacyGate`, and before this packet it built it with
/// `PrivacyGate::new(classifier)` — a fresh, never-degraded gate. So every tool
/// call bypassed C-01's fail-closed rule while chat messages honoured it. The
/// test drives the REAL dispatcher (real registry, real hook chain) rather than
/// asserting on a constructor, so re-introducing the bug fails the suite.
#[cfg(test)]
mod tool_path_degraded_tests {
    use std::sync::Arc;

    use crate::agent::gate::Binding;
    use crate::classifier::ClassifierHealth;
    use crate::tools::calling::ToolCall;
    use crate::tools::dispatch::ToolOutcome;
    use crate::tools::ExecCtx;

    fn dispatcher(health: Arc<ClassifierHealth>) -> (crate::tools::ToolDispatcher, std::path::PathBuf) {
        let mut dir = std::env::temp_dir();
        dir.push(format!("lhp-tool-degraded-{}", uuid::Uuid::new_v4()));
        let storage = crate::storage::Storage::open(&dir).expect("temp storage");
        let d = super::build_tool_dispatcher(
            &dir,
            Arc::new(crate::classifier::RulesClassifier::new()),
            health,
            Arc::new(crate::hooks::ApprovalLedger::new()),
            None,
            storage,
            None,
            None,
            None,
            Arc::new(crate::models::ModelManager::new()),
            Arc::new(crate::secrets::MemoryProviderSecretStore::default()),
            Arc::new(parking_lot::Mutex::new(std::collections::HashSet::new())),
        );
        (d, dir)
    }

    /// `system_status` is `RiskClass::Safe` (pre-trusted, no approval prompt) and
    /// its args are benign, so the ONLY thing that can hold it back on a cloud
    /// endpoint under `Auto` is the privacy filter's degraded fail-closed rule.
    async fn status_outcome(health: Arc<ClassifierHealth>) -> ToolOutcome {
        let (d, _dir) = dispatcher(health);
        let call = ToolCall {
            name: "system_status".to_string(),
            args: serde_json::json!({}),
        };
        let ctx = ExecCtx {
            conversation_id: "conv-degraded".to_string(),
            profile: "personal".to_string(),
            ..Default::default()
        };
        d.dispatch(&call, &ctx, Binding::Auto, true).await
    }

    #[tokio::test]
    async fn a_degraded_classifier_holds_tool_calls_off_cloud_too() {
        // CONTROL: healthy ⇒ the call runs on a cloud-endpoint turn.
        let healthy = status_outcome(ClassifierHealth::healthy()).await;
        assert!(
            matches!(healthy, ToolOutcome::Ok(_)),
            "control: a healthy classifier must let a Safe tool run, got {healthy:?}"
        );

        // C-01: degraded ⇒ the dispatcher reports the call must move on-device.
        // `NeedsLocalReroute` (not `Denied`) is the correct shape: the tool was
        // NOT run, and the caller may retry against a local endpoint.
        let degraded = status_outcome(ClassifierHealth::degraded_with("models missing")).await;
        assert!(
            matches!(degraded, ToolOutcome::NeedsLocalReroute { .. }),
            "a degraded classifier must not let the TOOL path egress, got {degraded:?}"
        );
    }
}
