//! P1 IPC-args contract test — regression lock for the struct-arg wrapping
//! bug fixed in M1.
//!
//! Background: Tauri v2 nests a command's struct parameter under the
//! parameter's own name in the JSON payload. For every command here whose
//! signature is `fn cmd(state: State, args: SomeArgs)`, the frontend MUST
//! call `invoke("cmd", { args: { ...snake_case_fields } })`. Early M1 code
//! called `invoke("cmd", { ...camelCaseFields })` (flat, no wrapper,
//! camelCase) — that shape silently failed to deserialize and every
//! struct-arg command was broken end-to-end. See `src/lib/api/tauri.ts`
//! header comment for the frontend-side contract this test locks in.
//!
//! `agent::loop_tests` already covers the agent loop's *business logic* by
//! re-implementing `process_message`'s body against a fake model streamer —
//! it never goes through `tauri::generate_handler!`'s argument
//! deserialization, so it would NOT have caught the wrapping bug. This
//! module closes that gap: it builds a real `App<MockRuntime>` with the
//! actual `invoke_handler` from `lib.rs` and the actual `AppState`, then
//! drives IPC through `tauri::test::get_ipc_response` with raw JSON bodies
//! shaped exactly like what the JS bridge sends (or, in the "broken shape"
//! tests, exactly like what the pre-fix bridge used to send).
//!
//! Commands covered are the model-free ones named in the M1 handoff:
//! `create_conversation`, `list_conversations`, `get_messages`,
//! `add_provider`, `list_models`. `send_message` is excluded here — it
//! needs a live/fake model stream and is already covered end-to-end by
//! `agent::loop_tests`.

use std::sync::Arc;

use serde_json::{json, Value};
use tauri::ipc::{CallbackFn, InvokeBody, InvokeResponseBody};
use tauri::test::{get_ipc_response, mock_context, mock_builder, noop_assets, MockRuntime};
use tauri::webview::InvokeRequest;
use tauri::{App, Manager, WebviewWindow, WebviewWindowBuilder};

use crate::agent::gate::PrivacyGate;
use crate::agent::loop_mod::AgentLoop;
use crate::ipc::{self, AppState};
use crate::models::ModelManager;
use crate::storage::Storage;
use crate::classifier::HeuristicClassifier;

// ── Harness ─────────────────────────────────────────────────────────────

/// A fresh tempdir per test, mirroring `agent::loop_tests::tempdir`. Kept
/// local (not shared) since that helper isn't `pub`.
fn tempdir() -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("lhp-ipc-contract-{}", uuid::Uuid::new_v4()));
    p
}

/// Build a real `App<MockRuntime>` wired exactly like `lib.rs::run()`:
/// real `AppState` (temp on-disk storage, empty `ModelManager`, a real
/// `AgentLoop` + `PrivacyGate`), and the full production
/// `invoke_handler` command table. No fakes at the command-dispatch
/// layer — only the storage root differs (tempdir instead of
/// `~/Documents/Lost-Harness`).
fn test_app() -> App<MockRuntime> {
    let dir = tempdir();
    let storage = Storage::open(&dir).expect("open temp storage");
    let storage = Arc::new(storage);

    let model_manager = Arc::new(ModelManager::new());
    let gate = PrivacyGate::new(Arc::new(HeuristicClassifier::new()));
    // `send_message` (the only command that uses the dispatcher for real work)
    // isn't registered in this harness, so an inert dispatcher is enough. C4:
    // shared between the loop and AppState (skill hot-registration commands).
    let tools = Arc::new(crate::tools::ToolDispatcher::empty());
    let agent_loop = Arc::new(AgentLoop::new(
        gate,
        Arc::clone(&model_manager),
        Arc::clone(&storage),
        Arc::clone(&tools),
    ));

    let state = AppState {
        agent_loop,
        model_manager,
        storage,
        provider_secrets: Arc::new(crate::secrets::MemoryProviderSecretStore::default()),
        approvals: Arc::new(crate::ipc::approval::ApprovalRegistry::new()),
        ask_human: Arc::new(crate::ipc::ask_human::AskHumanRegistry::new()),
        classifier: Arc::new(HeuristicClassifier::new()),
        embedder: None,
        tools,
        mcp: Arc::new(crate::tools::mcp_stdio::McpRuntime::new()),
        // Default profile (total_ram 0) — the calculator contract test only
        // checks the command dispatches + returns a CalcOutput shape, not fit.
        hardware: Arc::new(Default::default()),
        #[cfg(feature = "local-runner")]
        local_runner: None,
    };

    let app = mock_builder()
        // Same command list as `lib.rs::run()`, minus `send_message`.
        // `send_message` takes a bare (non-generic) `AppHandle`, which
        // hard-codes it to the production `Wry` runtime — it can't be
        // registered against `MockRuntime` at all (a `CommandArg<'_,
        // MockRuntime>` bound fails to compile), and it needs a live/fake
        // model stream anyway. It's exercised end-to-end by
        // `agent::loop_tests` instead. Every other command here is
        // model-free and generic over the runtime via `State`/`AppHandle`
        // erasure, so they register fine under `MockRuntime`.
        .invoke_handler(tauri::generate_handler![
            ipc::get_app_version,
            ipc::get_active_profile,
            ipc::set_active_profile,
            ipc::list_profiles,
            ipc::list_conversations,
            ipc::create_conversation,
            ipc::get_messages,
            ipc::list_providers,
            ipc::add_provider,
            ipc::update_provider,
            ipc::remove_provider,
            ipc::list_models,
            ipc::get_classifier_settings,
            ipc::set_classifier_settings,
            ipc::set_redaction_enabled,
            ipc::reset_classifier_settings,
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
            // B8: the rest of the registered surface (every command except the
            // two that take a bare `AppHandle` — send_message, download_model —
            // which structurally can't register under MockRuntime).
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
            ipc::list_model_catalog,
            ipc::list_local_models,
            ipc::remove_local_model,
            ipc::list_tool_rules,
            ipc::delete_tool_rule,
            ipc::list_cron_jobs,
            ipc::set_cron_job_enabled,
            ipc::delete_cron_job,
            ipc::explain_classification,
            ipc::list_memory,
            ipc::save_memory,
            ipc::delete_memory,
            ipc::set_memory_pinned,
            ipc::get_memory_settings,
            ipc::set_memory_settings,
        ])
        .build(mock_context(noop_assets()))
        .expect("failed to build mock app");

    app.manage(state);
    app
}

/// One throwaway webview window per test — `get_ipc_response` dispatches
/// through it exactly as the real webview would. Kept as `WebviewWindow`
/// (not unwrapped to `Webview`) because `get_ipc_response` needs the
/// `AsRef<Webview<R>>` impl that lives on `WebviewWindow`, not on `Webview`
/// itself.
fn test_webview(app: &App<MockRuntime>) -> WebviewWindow<MockRuntime> {
    WebviewWindowBuilder::new(app, "main", Default::default())
        .build()
        .expect("failed to build mock webview window")
}

fn invoke_request(cmd: &str, body: Value) -> InvokeRequest {
    InvokeRequest {
        cmd: cmd.into(),
        callback: CallbackFn(0),
        error: CallbackFn(1),
        // Must be the platform-correct *local* origin (see
        // `tauri::test`'s own doctest) — `windows`/`android` use
        // `http://tauri.localhost`, everything else (including this
        // macOS dev box) uses `tauri://localhost`. Getting this wrong
        // makes Tauri treat the request as remote, which forces ACL
        // enforcement even for an app with no ACL manifest and produces
        // a misleading "Plugin not found" rejection that has nothing to
        // do with the `args` wrapping this test is actually about.
        url: if cfg!(any(windows, target_os = "android")) {
            "http://tauri.localhost"
        } else {
            "tauri://localhost"
        }
        .parse()
        .unwrap(),
        body: InvokeBody::Json(body),
        headers: Default::default(),
        invoke_key: tauri::test::INVOKE_KEY.to_string(),
    }
}

fn call(
    webview: &WebviewWindow<MockRuntime>,
    cmd: &str,
    body: Value,
) -> Result<InvokeResponseBody, Value> {
    get_ipc_response(webview, invoke_request(cmd, body))
}

/// True if `msg` looks like an IPC-layer arg-deserialization rejection
/// (`crate::Error::InvalidArgs`'s Display: `` invalid args `{key}` for
/// command `{name}`: {serde_json::Error} ``) rather than a domain-level
/// error returned by a command body that actually ran.
fn is_ipc_arg_rejection(msg: &str) -> bool {
    msg.contains("invalid args") && msg.contains("missing required key")
}

// ── create_conversation ────────────────────────────────────────────────

#[test]
fn create_conversation_correct_shape_dispatches_and_succeeds() {
    let app = test_app();
    let webview = test_webview(&app);

    let body = json!({
        "args": {
            "name": "Test Conversation",
            "binding": "auto",
            "profile": "personal",
        }
    });
    let res = call(&webview, "create_conversation", body);
    let ok = res.expect("correctly-wrapped args must dispatch and succeed");
    let value: Value = ok.deserialize().expect("response must be valid JSON");
    assert_eq!(value["name"], "Test Conversation");
    assert_eq!(value["binding"], "auto");
    assert!(value["id"].is_string(), "expected a generated id: {value:?}");
}

#[test]
fn create_conversation_old_broken_shape_is_rejected() {
    let app = test_app();
    let webview = test_webview(&app);

    // OLD broken shape: flat top-level fields, camelCase, no `args`
    // wrapper — what the pre-fix frontend sent.
    let body = json!({
        "name": "Test Conversation",
        "binding": "auto",
        "profile": "personal",
    });
    let res = call(&webview, "create_conversation", body);
    let err = res.expect_err("flat/unwrapped args must NOT dispatch");
    let msg = err.as_str().unwrap_or_default();
    assert!(
        is_ipc_arg_rejection(msg),
        "expected an IPC-level arg-deserialization rejection, got: {msg}"
    );
}

// ── list_conversations ─────────────────────────────────────────────────

#[test]
fn list_conversations_correct_shape_dispatches_and_succeeds() {
    let app = test_app();
    let webview = test_webview(&app);

    // Seed one conversation through the real IPC path so the list isn't
    // vacuously empty.
    call(
        &webview,
        "create_conversation",
        json!({"args": {"name": "Seed", "binding": "auto", "profile": "personal"}}),
    )
    .expect("seed create_conversation must succeed");

    let res = call(
        &webview,
        "list_conversations",
        json!({"args": {"profile": "personal"}}),
    );
    let ok = res.expect("correctly-wrapped args must dispatch and succeed");
    let value: Value = ok.deserialize().expect("response must be valid JSON");
    let list = value.as_array().expect("expected a JSON array");
    assert_eq!(list.len(), 1, "expected the seeded conversation: {list:?}");
    assert_eq!(list[0]["name"], "Seed");
}

#[test]
fn list_conversations_old_broken_shape_is_rejected() {
    let app = test_app();
    let webview = test_webview(&app);

    let body = json!({"profile": "personal"});
    let res = call(&webview, "list_conversations", body);
    let err = res.expect_err("flat/unwrapped args must NOT dispatch");
    let msg = err.as_str().unwrap_or_default();
    assert!(
        is_ipc_arg_rejection(msg),
        "expected an IPC-level arg-deserialization rejection, got: {msg}"
    );
}

// ── get_messages ────────────────────────────────────────────────────────

#[test]
fn get_messages_correct_shape_dispatches_and_succeeds() {
    let app = test_app();
    let webview = test_webview(&app);

    let created = call(
        &webview,
        "create_conversation",
        json!({"args": {"name": "Convo", "binding": "auto", "profile": "personal"}}),
    )
    .expect("seed create_conversation must succeed")
    .deserialize::<Value>()
    .unwrap();
    let conversation_id = created["id"].as_str().unwrap().to_string();

    let res = call(
        &webview,
        "get_messages",
        json!({"args": {"profile": "personal", "conversation_id": conversation_id}}),
    );
    let ok = res.expect("correctly-wrapped args must dispatch and succeed");
    let value: Value = ok.deserialize().expect("response must be valid JSON");
    assert_eq!(
        value.as_array().expect("expected a JSON array").len(),
        0,
        "fresh conversation should have no messages yet"
    );
}

#[test]
fn get_messages_old_broken_shape_is_rejected() {
    let app = test_app();
    let webview = test_webview(&app);

    // Flat, camelCase, no `args` wrapper — matches the exact pre-fix bug
    // shape (`conversationId` instead of nested snake_case `conversation_id`).
    let body = json!({"profile": "personal", "conversationId": "conv-1"});
    let res = call(&webview, "get_messages", body);
    let err = res.expect_err("flat/unwrapped args must NOT dispatch");
    let msg = err.as_str().unwrap_or_default();
    assert!(
        is_ipc_arg_rejection(msg),
        "expected an IPC-level arg-deserialization rejection, got: {msg}"
    );
}

// ── add_provider ────────────────────────────────────────────────────────

#[test]
fn add_provider_correct_shape_dispatches_and_succeeds() {
    let app = test_app();
    let webview = test_webview(&app);

    let body = json!({
        "args": {
            "name": "OpenAI",
            "base_url": "https://api.openai.com/v1",
            "api_key": "sk-test-secret",
            "kind": "cloud",
        }
    });
    let res = call(&webview, "add_provider", body);
    let ok = res.expect("correctly-wrapped args must dispatch and succeed");
    let raw = match &ok {
        InvokeResponseBody::Json(s) => s.clone(),
        InvokeResponseBody::Raw(_) => panic!("expected a JSON response"),
    };
    // The API key must never round-trip back to the frontend (ProviderInfo
    // omits it) — assert on the raw JSON text, not just the parsed value,
    // so a stray extra field would still be caught.
    assert!(!raw.contains("sk-test-secret"), "api key leaked: {raw}");
    let value: Value = ok.deserialize().expect("response must be valid JSON");
    assert_eq!(value["name"], "OpenAI");
    // `ProviderKind` serializes lowercase (`#[serde(rename_all = "lowercase")]`)
    // so it matches what the frontend sends and compares against
    // (`p.kind === "local"` in ProviderSettings.svelte, ModelPicker.svelte,
    // provider-catalog.ts). Guards against a regression to PascalCase "Cloud".
    assert_eq!(value["kind"], "cloud");
    assert!(value["id"].is_string());
}

#[test]
fn add_provider_old_broken_shape_is_rejected() {
    let app = test_app();
    let webview = test_webview(&app);

    // Flat, camelCase, no `args` wrapper.
    let body = json!({
        "name": "OpenAI",
        "baseUrl": "https://api.openai.com/v1",
        "apiKey": "sk-test-secret",
        "kind": "cloud",
    });
    let res = call(&webview, "add_provider", body);
    let err = res.expect_err("flat/unwrapped args must NOT dispatch");
    let msg = err.as_str().unwrap_or_default();
    assert!(
        is_ipc_arg_rejection(msg),
        "expected an IPC-level arg-deserialization rejection, got: {msg}"
    );
    // Belt-and-suspenders: the secret must not have been echoed back in
    // any form (it wouldn't have been, since the command body never ran,
    // but this pins that invariant explicitly).
    assert!(!msg.contains("sk-test-secret"));
}

// ── update_provider ─────────────────────────────────────────────────────

#[test]
fn update_provider_correct_shape_dispatches_and_keeps_stored_key() {
    let app = test_app();
    let webview = test_webview(&app);

    // Seed a provider to edit.
    let added = call(
        &webview,
        "add_provider",
        json!({
            "args": {
                "name": "OpenAI",
                "base_url": "https://api.openai.com/v1",
                "api_key": "sk-test-secret",
                "kind": "cloud",
            }
        }),
    )
    .expect("seed add_provider must succeed");
    let added: Value = added.deserialize().expect("seed response must be valid JSON");
    let id = added["id"].as_str().expect("seed id").to_string();

    // Edit with NO api_key — mirrors the Settings edit form, which never
    // echoes the stored key back into the field.
    let body = json!({
        "args": {
            "id": id,
            "name": "Renamed",
            "base_url": "http://10.0.0.100:8000/v1",
            "kind": "local",
            "supports_native_tools": true,
        }
    });
    let ok = call(&webview, "update_provider", body)
        .expect("correctly-wrapped args must dispatch and succeed");
    let raw = match &ok {
        InvokeResponseBody::Json(s) => s.clone(),
        InvokeResponseBody::Raw(_) => panic!("expected a JSON response"),
    };
    // Same invariant as add_provider: the key never round-trips back.
    assert!(!raw.contains("sk-test-secret"), "api key leaked: {raw}");
    let value: Value = ok.deserialize().expect("response must be valid JSON");
    assert_eq!(value["id"], id.as_str());
    assert_eq!(value["name"], "Renamed");
    assert_eq!(value["kind"], "local");
    assert_eq!(value["supports_native_tools"], true);

    // Absent api_key means "keep the stored key", not "clear it" — pin
    // that in both the in-memory manager and the persisted endpoint row.
    let state = app.state::<AppState>();
    let provider = state
        .model_manager
        .get_provider(&id)
        .expect("provider still registered");
    assert_eq!(provider.api_key.as_deref(), Some("sk-test-secret"));
    assert_eq!(
        state.provider_secrets.get(&id).unwrap().as_deref(),
        Some("sk-test-secret"),
        "the provider secret is held by the credential-store seam"
    );
    assert_eq!(provider.base_url, "http://10.0.0.100:8000/v1");
    assert!(provider.supports_native_tools);
    let ep = state
        .storage
        .global()
        .get_endpoint(&id)
        .expect("endpoint query")
        .expect("endpoint row persisted");
    assert_eq!(ep.name, "Renamed");
    assert_eq!(ep.base_url, "http://10.0.0.100:8000/v1");
    assert_eq!(ep.kind, "local");
    assert!(ep.supports_native_tools);
    assert_eq!(
        ep.api_key_marker.as_deref(),
        Some(crate::secrets::KEYCHAIN_MARKER),
        "SQLite stores only the keychain marker, never the provider secret"
    );
}

#[test]
fn update_provider_unknown_id_is_domain_error() {
    let app = test_app();
    let webview = test_webview(&app);

    let body = json!({
        "args": {
            "id": "no-such-provider",
            "name": "X",
            "base_url": "http://localhost:1234/v1",
            "kind": "custom",
        }
    });
    let res = call(&webview, "update_provider", body);
    let err = res.expect_err("unknown id must fail in the command body");
    let msg = err.as_str().unwrap_or_default();
    // A domain error, NOT an arg-shape rejection — proves the command is
    // registered and the args deserialized.
    assert!(
        !is_ipc_arg_rejection(msg),
        "expected a domain error, got an arg rejection: {msg}"
    );
    assert!(msg.contains("unknown provider"), "unexpected error: {msg}");
}

// ── list_models ─────────────────────────────────────────────────────────
//
// `list_models` is `async` and, on success, makes a real HTTP call via
// `ModelManager::list_models_for`. To keep this test deterministic and
// network-free we target an unregistered provider id: `get_client`
// returns `None` before any HTTP client is built, so the command body
// runs to completion and returns a domain error synchronously. That's
// enough to prove the args were deserialized and the command dispatched
// — the point of this test is the IPC arg-shape boundary, not the model
// listing itself (which `models::tests` / `models::client` presumably
// cover separately).

#[test]
fn list_models_correct_shape_dispatches_and_reaches_command_body() {
    let app = test_app();
    let webview = test_webview(&app);

    let body = json!({"args": {"provider_id": "does-not-exist"}});
    let res = call(&webview, "list_models", body);
    let err = res.expect_err("unknown provider id should be a domain-level error, not a dispatch failure");
    let msg = err.as_str().unwrap_or_default();
    assert!(
        !is_ipc_arg_rejection(msg),
        "correctly-wrapped args must not be rejected at the IPC boundary, got: {msg}"
    );
    assert!(
        msg.contains("unknown provider"),
        "expected the ModelManager's domain error to surface, got: {msg}"
    );
}

#[test]
fn list_models_old_broken_shape_is_rejected() {
    let app = test_app();
    let webview = test_webview(&app);

    // Flat, camelCase, no `args` wrapper.
    let body = json!({"providerId": "does-not-exist"});
    let res = call(&webview, "list_models", body);
    let err = res.expect_err("flat/unwrapped args must NOT dispatch");
    let msg = err.as_str().unwrap_or_default();
    assert!(
        is_ipc_arg_rejection(msg),
        "expected an IPC-level arg-deserialization rejection, got: {msg}"
    );
    // And must NOT be the domain error — proves it never reached
    // `ModelManager::list_models_for`.
    assert!(!msg.contains("unknown provider"), "leaked into command body: {msg}");
}

// ── active_profile (set → get round-trip) ──────────────────────────────
//
// The regression this locks: `get_active_profile` used to be a hardcoded
// "personal" stub, so a restart always reset the UI to Personal. It now reads
// back what `set_active_profile` persisted — this exercises that end-to-end
// through the real IPC boundary (a fresh `test_app()` = a simulated restart).

#[test]
fn active_profile_round_trips_through_real_ipc() {
    let app = test_app();
    let webview = test_webview(&app);

    // Fresh install: nothing persisted yet → the default.
    let got = call(&webview, "get_active_profile", json!({}))
        .expect("get_active_profile must dispatch");
    let got: Value = got.deserialize().expect("valid JSON");
    assert_eq!(got, "personal", "a fresh install defaults to personal");

    // Switch to "work" and persist it.
    call(&webview, "set_active_profile", json!({ "args": { "id": "work" } }))
        .expect("set_active_profile must dispatch and succeed");

    // Read it back through IPC — this is exactly what boot-time `hydrate()`
    // sees. Before the fix this stayed "personal"; now it's the stored choice.
    let got = call(&webview, "get_active_profile", json!({}))
        .expect("get_active_profile must dispatch");
    let got: Value = got.deserialize().expect("valid JSON");
    assert_eq!(got, "work", "the persisted profile must survive a (simulated) restart");
}

#[test]
fn set_active_profile_rejects_a_confusable_name() {
    let app = test_app();
    let webview = test_webview(&app);

    // A whitespace-padded name maps to a distinct `.db` file — the allowlist
    // rejects it as a DOMAIN error (the command ran and validated), not an
    // arg-shape rejection. Proves `validate_profile_name` guards this writer.
    let res = call(&webview, "set_active_profile", json!({ "args": { "id": "work " } }));
    let err = res.expect_err("a padded/confusable name must be rejected");
    let msg = err.as_str().unwrap_or_default();
    assert!(
        !is_ipc_arg_rejection(msg),
        "expected a domain-level validation error, got an arg rejection: {msg}"
    );
    assert!(msg.contains("invalid profile name"), "expected the validator's message, got: {msg}");

    // A rejected set persists nothing — the read still returns the default.
    let got = call(&webview, "get_active_profile", json!({}))
        .expect("get_active_profile must dispatch");
    let got: Value = got.deserialize().expect("valid JSON");
    assert_eq!(got, "personal", "a rejected set must not have written a row");
}

// ── scheduled jobs (the ScheduledJobs screen surface) ──────────────────

#[test]
fn cron_jobs_list_toggle_delete_round_trip_through_real_ipc() {
    let app = test_app();
    let webview = test_webview(&app);

    // Empty profile → empty list (correct nested-args shape dispatches).
    let got = call(&webview, "list_cron_jobs", json!({ "args": { "profile": "personal" } }))
        .expect("list_cron_jobs must dispatch");
    let got: Value = got.deserialize().expect("valid JSON");
    assert_eq!(got, json!([]), "a fresh profile has no scheduled jobs");

    // Seed one job directly at the storage layer (creation is agent-driven
    // via the Dangerous manage_cron tool, not this IPC surface).
    {
        let state: tauri::State<'_, crate::AppState> = app.state();
        let db = state.storage.open_profile("personal").expect("open profile");
        db.insert_cron_job(&crate::storage::CronJob {
            id: "cj-1".into(),
            name: "morning brief".into(),
            prompt: "summarize my day".into(),
            schedule: "0 7 * * *".into(),
            enabled: true,
            last_run_at: None,
            last_status: None,
            target_conversation_id: None,
        })
        .expect("insert cron job");
    }

    // The list surfaces it.
    let got = call(&webview, "list_cron_jobs", json!({ "args": { "profile": "personal" } }))
        .expect("list_cron_jobs must dispatch");
    let got: Value = got.deserialize().expect("valid JSON");
    assert_eq!(got.as_array().map(|a| a.len()), Some(1));
    assert_eq!(got[0]["name"], "morning brief");
    assert_eq!(got[0]["enabled"], true);

    // Pause it through IPC; the change is visible on re-read.
    let ok = call(
        &webview,
        "set_cron_job_enabled",
        json!({ "args": { "profile": "personal", "id": "cj-1", "enabled": false } }),
    )
    .expect("set_cron_job_enabled must dispatch");
    let ok: Value = ok.deserialize().expect("valid JSON");
    assert_eq!(ok, true);
    let got = call(&webview, "list_cron_jobs", json!({ "args": { "profile": "personal" } }))
        .expect("list_cron_jobs must dispatch");
    let got: Value = got.deserialize().expect("valid JSON");
    assert_eq!(got[0]["enabled"], false, "the pause must persist");

    // Delete it; the list is empty again. An unknown id reports false.
    let ok = call(
        &webview,
        "delete_cron_job",
        json!({ "args": { "profile": "personal", "id": "cj-1" } }),
    )
    .expect("delete_cron_job must dispatch");
    let ok: Value = ok.deserialize().expect("valid JSON");
    assert_eq!(ok, true);
    let got = call(&webview, "list_cron_jobs", json!({ "args": { "profile": "personal" } }))
        .expect("list_cron_jobs must dispatch");
    let got: Value = got.deserialize().expect("valid JSON");
    assert_eq!(got, json!([]));
    let ok = call(
        &webview,
        "delete_cron_job",
        json!({ "args": { "profile": "personal", "id": "cj-1" } }),
    )
    .expect("delete_cron_job must dispatch even for an unknown id");
    let ok: Value = ok.deserialize().expect("valid JSON");
    assert_eq!(ok, false, "deleting a gone id reports false, not an error");
}

// ── classifier settings (PLAN §11) ─────────────────────────────────────

#[test]
fn classifier_settings_round_trip_through_real_ipc() {
    let app = test_app();
    let webview = test_webview(&app);

    // Defaults for a fresh profile (strictness 50, medium).
    let got: Value = call(
        &webview,
        "get_classifier_settings",
        json!({"args": {"profile": "personal"}}),
    )
    .expect("get_classifier_settings must dispatch")
    .deserialize()
    .expect("valid JSON");
    assert_eq!(got["strictness"], 50);
    assert_eq!(got["uncertainty_band"], "medium");

    // Set a strict config, then read it back through a fresh get.
    let set: Value = call(
        &webview,
        "set_classifier_settings",
        json!({"args": {"profile": "personal", "strictness": 100, "uncertainty_band": "wide"}}),
    )
    .expect("set_classifier_settings must dispatch")
    .deserialize()
    .expect("valid JSON");
    assert_eq!(set["strictness"], 100);
    assert_eq!(set["uncertainty_band"], "wide");

    let reget: Value = call(
        &webview,
        "get_classifier_settings",
        json!({"args": {"profile": "personal"}}),
    )
    .expect("get after set")
    .deserialize()
    .expect("valid JSON");
    assert_eq!(reget["strictness"], 100, "persisted strictness must survive a re-read");
    assert_eq!(reget["uncertainty_band"], "wide");

    // Toggling redaction preserves the thresholds (and vice versa).
    let red: Value = call(
        &webview,
        "set_redaction_enabled",
        json!({"args": {"profile": "personal", "enabled": false}}),
    )
    .expect("set_redaction_enabled must dispatch")
    .deserialize()
    .expect("valid JSON");
    assert_eq!(red["redaction_enabled"], false);
    assert_eq!(red["strictness"], 100, "redaction toggle preserved thresholds");
    assert_eq!(red["uncertainty_band"], "wide");

    // Reset → back to defaults (thresholds AND redaction on).
    let reset: Value = call(
        &webview,
        "reset_classifier_settings",
        json!({"args": {"profile": "personal"}}),
    )
    .expect("reset_classifier_settings must dispatch")
    .deserialize()
    .expect("valid JSON");
    assert_eq!(reset["strictness"], 50);
    assert_eq!(reset["uncertainty_band"], "medium");
    assert_eq!(reset["redaction_enabled"], true);
}

#[test]
fn set_classifier_settings_old_broken_shape_is_rejected() {
    let app = test_app();
    let webview = test_webview(&app);

    // Flat, camelCase, no `args` wrapper — the pre-fix bridge shape.
    let body = json!({"profile": "personal", "strictness": 80, "uncertaintyBand": "narrow"});
    let res = call(&webview, "set_classifier_settings", body);
    let err = res.expect_err("flat/unwrapped args must NOT dispatch");
    let msg = err.as_str().unwrap_or_default();
    assert!(
        is_ipc_arg_rejection(msg),
        "expected an IPC-level arg-deserialization rejection, got: {msg}"
    );
}

// ── M8 S2′/S3′ model IPC (A3) ──────────────────────────────────────────────

#[test]
fn calculate_model_fit_correct_shape_dispatches_and_returns_calc_output() {
    // The interactive calculator is PURE (no network) — the contract test runs
    // the full happy path against the cached (default) hardware profile.
    let app = test_app();
    let webview = test_webview(&app);
    let body = json!({
        "args": {
            "model_spec": {
                "architecture": "llama",
                "total_params_b": 8.0,
                "active_params_b": 8.0,
                "n_layers": 32,
                "n_kv_heads": 8,
                "head_dim": 128,
                "native_context_len": 8192,
                "kv_exact": true
            },
            "calc_input": {
                "weight_file_bytes": 5_000_000_000u64,
                "kv_quant": "f16",
                "context_len": 8192
            }
        }
    });
    let res = call(&webview, "calculate_model_fit", body);
    let ok = res.expect("correctly-wrapped args must dispatch and succeed");
    let value: Value = ok.deserialize().expect("response must be valid JSON");
    // A CalcOutput shape: the fit verdict + the byte breakdown must be present.
    assert!(value["fit"].is_string(), "expected a fit verdict: {value:?}");
    assert!(value["total_required_bytes"].is_number());
    assert!(value["kv_cache_bytes"].is_number());
    assert!(value.get("notes").is_some());
}

#[test]
fn calculate_model_fit_old_broken_shape_is_rejected() {
    let app = test_app();
    let webview = test_webview(&app);
    // Flat, no `args` wrapper — the pre-fix bridge shape.
    let body = json!({
        "model_spec": {"architecture": "llama", "total_params_b": 8.0, "active_params_b": 8.0,
            "n_layers": 32, "n_kv_heads": 8, "head_dim": 128, "native_context_len": 8192, "kv_exact": true},
        "calc_input": {"weight_file_bytes": 5_000_000_000u64, "kv_quant": "f16", "context_len": 8192}
    });
    let res = call(&webview, "calculate_model_fit", body);
    let err = res.expect_err("flat/unwrapped args must NOT dispatch");
    assert!(is_ipc_arg_rejection(err.as_str().unwrap_or_default()));
}

// The two networked commands (search_models / get_model_detail) are contract-
// tested for the ARG-ENVELOPE shape only — dispatching the happy path would hit
// HuggingFace, so their positive path is covered by the env-gated live tests +
// the pure parser unit tests in `models/hf_search.rs`. The wrong-shape rejection
// below still catches the real regression these tests exist for: the Tauri-v2
// `{args:{…}}` nesting.

#[test]
fn search_models_old_broken_shape_is_rejected() {
    let app = test_app();
    let webview = test_webview(&app);
    let body = json!({"query": "qwen", "sort": "downloads"}); // no `args` wrapper
    let res = call(&webview, "search_models", body);
    let err = res.expect_err("flat/unwrapped args must NOT dispatch");
    assert!(is_ipc_arg_rejection(err.as_str().unwrap_or_default()));
}

#[test]
fn get_model_detail_old_broken_shape_is_rejected() {
    let app = test_app();
    let webview = test_webview(&app);
    let body = json!({"model_id": "Qwen/Qwen3-0.6B-GGUF"}); // no `args` wrapper
    let res = call(&webview, "get_model_detail", body);
    let err = res.expect_err("flat/unwrapped args must NOT dispatch");
    assert!(is_ipc_arg_rejection(err.as_str().unwrap_or_default()));
}

#[test]
fn model_ipc_args_deserialize_from_the_wire_shape() {
    // Network-free positive coverage of the two networked commands' arg structs
    // (the happy-path dispatch would hit HF): the JSON the frontend sends must
    // deserialize into the args, defaults and all.
    let s: super::SearchModelsArgs =
        serde_json::from_value(json!({"query": "qwen3", "sort": "likes", "limit": 10})).unwrap();
    assert_eq!(s.query, "qwen3");
    assert_eq!(s.limit, Some(10));
    // Optional fields default when absent.
    let s2: super::SearchModelsArgs = serde_json::from_value(json!({"query": ""})).unwrap();
    assert!(s2.sort.is_none() && s2.limit.is_none());
    let d: super::GetModelDetailArgs =
        serde_json::from_value(json!({"model_id": "org/repo"})).unwrap();
    assert_eq!(d.model_id, "org/repo");
}

// ── sandbox_config (B2) ────────────────────────────────────────────────────

#[test]
fn set_then_get_sandbox_config_round_trips_the_ceiling() {
    let app = test_app();
    let webview = test_webview(&app);
    let cfg = json!({
        "enabled": true,
        "auto_allow_if_sandboxed": false,
        "excluded_commands": ["rm"],
        "network": {"allowed_domains": ["example.com"], "allow_localhost": false, "allow_unix_sockets": []}
    });
    let set = call(&webview, "set_sandbox_config", json!({"args": {"profile": "personal", "config": cfg}}));
    set.expect("a valid config must persist");
    let got = call(&webview, "get_sandbox_config", json!({"args": {"profile": "personal"}}))
        .expect("get must dispatch")
        .deserialize::<Value>()
        .unwrap();
    assert_eq!(got["network"]["allow_localhost"], false, "the locked-down ceiling round-trips");
    assert_eq!(got["network"]["allowed_domains"][0], "example.com");
    assert_eq!(got["enabled"], true);
}

#[test]
fn get_sandbox_config_returns_default_when_unset() {
    let app = test_app();
    let webview = test_webview(&app);
    let got = call(&webview, "get_sandbox_config", json!({"args": {"profile": "personal"}}))
        .expect("get must dispatch")
        .deserialize::<Value>()
        .unwrap();
    // The library default: disabled, localhost allowed, no domains.
    assert_eq!(got["enabled"], false);
    assert_eq!(got["network"]["allow_localhost"], true);
}

#[test]
fn set_sandbox_config_rejects_an_empty_allowlist_entry() {
    let app = test_app();
    let webview = test_webview(&app);
    let cfg = json!({
        "enabled": true, "auto_allow_if_sandboxed": false, "excluded_commands": [],
        "network": {"allowed_domains": ["  "], "allow_localhost": true, "allow_unix_sockets": []}
    });
    let res = call(&webview, "set_sandbox_config", json!({"args": {"profile": "personal", "config": cfg}}));
    let err = res.expect_err("an empty allowed_domain must be rejected");
    let msg = err.as_str().unwrap_or_default();
    assert!(msg.contains("must not be empty"), "expected a validation error, got: {msg}");
    assert!(!is_ipc_arg_rejection(msg), "it's a domain-level validation error, not an arg-shape rejection");
}

#[test]
fn sandbox_config_old_broken_shape_is_rejected() {
    let app = test_app();
    let webview = test_webview(&app);
    // No `args` wrapper.
    let res = call(&webview, "get_sandbox_config", json!({"profile": "personal"}));
    let err = res.expect_err("flat/unwrapped args must NOT dispatch");
    assert!(is_ipc_arg_rejection(err.as_str().unwrap_or_default()));
}

// ── budget_settings (C1) ───────────────────────────────────────────────────

#[test]
fn set_then_get_budget_settings_round_trips_and_reset_clears() {
    let app = test_app();
    let webview = test_webview(&app);
    call(&webview, "set_budget_settings", json!({"args": {"profile": "personal", "cap_usd": 12.5}}))
        .expect("set a cap");
    let got = call(&webview, "get_budget_settings", json!({"args": {"profile": "personal"}}))
        .expect("get")
        .deserialize::<Value>()
        .unwrap();
    assert_eq!(got["cap_usd"], 12.5, "the cap round-trips");
    call(&webview, "reset_budget_settings", json!({"args": {"profile": "personal"}})).expect("reset");
    let after = call(&webview, "get_budget_settings", json!({"args": {"profile": "personal"}}))
        .expect("get")
        .deserialize::<Value>()
        .unwrap();
    assert!(after["cap_usd"].is_null(), "reset → uncapped (null)");
}

#[test]
fn budget_settings_old_broken_shape_is_rejected() {
    let app = test_app();
    let webview = test_webview(&app);
    let res = call(&webview, "get_budget_settings", json!({"profile": "personal"})); // no `args`
    let err = res.expect_err("flat/unwrapped args must NOT dispatch");
    assert!(is_ipc_arg_rejection(err.as_str().unwrap_or_default()));
}

// ── B8: contract coverage across the whole registered surface ──────────────
// Two data-driven sweeps pin the Tauri-v2 `{args:{…}}` envelope for every
// args-taking command and prove every no-arg command actually dispatches.
// (send_message / download_model are excluded — they take a bare AppHandle and
// can't register under MockRuntime; they're covered by agent::loop_tests / the
// download path respectively.)

#[test]
fn every_args_taking_command_rejects_the_unwrapped_envelope() {
    let app = test_app();
    let webview = test_webview(&app);
    // A body WITHOUT the `{args:{…}}` wrapper — the exact regression these
    // contract tests guard. Tauri v2 looks for the `args` key and, finding
    // none, rejects at the IPC layer before the command body runs, regardless
    // of the flat fields present here.
    let flat = json!({
        "profile": "personal", "id": "x", "seat": "s", "provider_id": "p", "model": "m",
        "status": "approved", "enabled": true, "content": "c", "text": "t", "pinned": true,
        "decision": "approve", "json": "{}", "answer": "a"
    });
    let args_cmds = [
        "update_provider",
        "resolve_tool_approval",
        "resolve_ask_human",
        "get_usage_summary",
        "set_skill_approval",
        "delete_skill",
        "set_skill_reflect_enabled",
        "list_seat_bindings",
        "set_seat_binding",
        "delete_seat_binding",
        "set_agent_type_approval",
        "delete_agent_type",
        "install_pack",
        "list_tool_rules",
        "delete_tool_rule",
        "list_cron_jobs",
        "set_cron_job_enabled",
        "delete_cron_job",
        "explain_classification",
        "list_memory",
        "save_memory",
        "delete_memory",
        "set_memory_pinned",
        "get_memory_settings",
        "set_memory_settings",
        "remove_local_model",
        "get_budget_settings",
        "set_budget_settings",
        "reset_budget_settings",
        "register_mcp_server",
        "remove_mcp_server",
    ];
    for cmd in args_cmds {
        let res = call(&webview, cmd, flat.clone());
        let err = res.err().unwrap_or_else(|| panic!("{cmd}: an unwrapped body must be rejected, not accepted"));
        assert!(
            is_ipc_arg_rejection(err.as_str().unwrap_or_default()),
            "{cmd}: expected an IPC arg-envelope rejection, got: {err:?}"
        );
    }
}

#[test]
fn every_no_arg_command_dispatches() {
    let app = test_app();
    let webview = test_webview(&app);
    // A no-arg command takes only `State`, so an empty body dispatches into the
    // body (which may then return a domain result/error) — but NEVER an
    // arg-shape rejection, and never "command not found" (i.e. it's registered).
    let no_arg_cmds = [
        "list_mcp_servers",
        "list_skills",
        "get_skill_reflect_enabled",
        "list_agent_types",
        "probe_hardware",
        "list_model_catalog",
        "list_local_models",
    ];
    for cmd in no_arg_cmds {
        let res = call(&webview, cmd, json!({}));
        if let Err(e) = res {
            let msg = e.as_str().unwrap_or_default();
            assert!(
                !is_ipc_arg_rejection(msg),
                "{cmd}: a no-arg command must dispatch, got an arg rejection: {msg}"
            );
            assert!(
                !msg.contains("not found"),
                "{cmd}: must be registered, got: {msg}"
            );
        }
    }
}
