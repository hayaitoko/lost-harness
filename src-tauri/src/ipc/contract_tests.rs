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
use tauri::test::{get_ipc_response, mock_builder, mock_context, noop_assets, MockRuntime};
use tauri::webview::InvokeRequest;
use tauri::{App, Manager, WebviewWindow, WebviewWindowBuilder};

use crate::agent::gate::PrivacyGate;
use crate::agent::loop_mod::AgentLoop;
use crate::classifier::HeuristicClassifier;
use crate::ipc::{self, AppState};
use crate::models::ModelManager;
use crate::storage::Storage;

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
        gate.clone(),
        Arc::clone(&model_manager),
        Arc::clone(&storage),
        Arc::clone(&tools),
    ));

    let state = AppState {
        agent_loop,
        email: Arc::new(crate::ipc::EmailRuntime::new()),
        model_manager,
        storage,
        provider_secrets: Arc::new(crate::secrets::MemoryProviderSecretStore::default()),
        approvals: Arc::new(crate::ipc::approval::ApprovalRegistry::new()),
        ask_human: Arc::new(crate::ipc::ask_human::AskHumanRegistry::new()),
        classifier: Arc::new(HeuristicClassifier::new()),
        gate,
        embedder: None,
        tools,
        mcp: Arc::new(crate::tools::mcp_stdio::McpRuntime::new(
            std::env::temp_dir().join(format!("lhp-ct-mcp-sandbox-{}", uuid::Uuid::new_v4())),
        )),
        // Default profile (total_ram 0) — the calculator contract test only
        // checks the command dispatches + returns a CalcOutput shape, not fit.
        hardware: Arc::new(Default::default()),
        #[cfg(feature = "local-runner")]
        local_runner: None,
        // H-07: MCP install nonces — empty, like a fresh boot.
        pending_mcp_nonces: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
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
            ipc::set_conversation_binding,
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
            ipc::generate_mcp_install_nonce,
            ipc::register_mcp_server,
            ipc::list_mcp_servers,
            ipc::remove_mcp_server,
            ipc::reapprove_mcp_server,
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
            // Round-2 item 3. `check_for_update`/`install_update` take a
            // bare `AppHandle` and so can't register under MockRuntime
            // (same structural reason as send_message/download_model).
            ipc::get_update_check_enabled,
            ipc::set_update_check_enabled,
            ipc::list_seat_bindings,
            ipc::set_seat_binding,
            ipc::delete_seat_binding,
            ipc::list_agent_types,
            ipc::set_agent_type_approval,
            ipc::delete_agent_type,
            ipc::install_pack,
            ipc::probe_hardware,
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
            ipc::google_clear_api_not_enabled,
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
            ipc::explain_classification,
            ipc::get_classifier_health,
            ipc::confirm_public_send,
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
    assert!(
        value["id"].is_string(),
        "expected a generated id: {value:?}"
    );
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

#[test]
fn conversation_binding_update_is_profile_scoped_and_persisted() {
    let app = test_app();
    let webview = test_webview(&app);
    let created: Value = call(
        &webview,
        "create_conversation",
        json!({"args": {"name": "Bound", "binding": "auto", "profile": "personal"}}),
    )
    .expect("seed conversation")
    .deserialize()
    .expect("valid create JSON");
    let id = created["id"].as_str().expect("conversation id");

    let updated: Value = call(
        &webview,
        "set_conversation_binding",
        json!({"args": {"conversation_id": id, "binding": "private", "profile": "personal"}}),
    )
    .expect("binding update")
    .deserialize()
    .expect("valid binding JSON");
    assert_eq!(updated["binding"], "private");

    let listed: Value = call(
        &webview,
        "list_conversations",
        json!({"args": {"profile": "personal"}}),
    )
    .expect("re-list")
    .deserialize()
    .expect("valid list JSON");
    assert_eq!(listed.as_array().unwrap()[0]["binding"], "private");

    let bad = call(
        &webview,
        "set_conversation_binding",
        json!({"args": {"conversation_id": id, "binding": "cloud", "profile": "personal"}}),
    )
    .expect_err("invalid binding must be rejected");
    assert!(bad.as_str().unwrap_or_default().contains("binding must be"));

    let foreign = call(
        &webview,
        "set_conversation_binding",
        json!({"args": {"conversation_id": id, "binding": "public", "profile": "work"}}),
    )
    .expect_err("another profile must not mutate this conversation");
    assert!(foreign.as_str().unwrap_or_default().contains("not found"));
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
    // provider UI). Guards against a regression to PascalCase "Cloud".
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
    let added: Value = added
        .deserialize()
        .expect("seed response must be valid JSON");
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
    let err = res
        .expect_err("unknown provider id should be a domain-level error, not a dispatch failure");
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
    assert!(
        !msg.contains("unknown provider"),
        "leaked into command body: {msg}"
    );
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
    let got =
        call(&webview, "get_active_profile", json!({})).expect("get_active_profile must dispatch");
    let got: Value = got.deserialize().expect("valid JSON");
    assert_eq!(got, "personal", "a fresh install defaults to personal");

    // Switch to "work" and persist it.
    call(
        &webview,
        "set_active_profile",
        json!({ "args": { "id": "work" } }),
    )
    .expect("set_active_profile must dispatch and succeed");

    // Read it back through IPC — this is exactly what boot-time `hydrate()`
    // sees. Before the fix this stayed "personal"; now it's the stored choice.
    let got =
        call(&webview, "get_active_profile", json!({})).expect("get_active_profile must dispatch");
    let got: Value = got.deserialize().expect("valid JSON");
    assert_eq!(
        got, "work",
        "the persisted profile must survive a (simulated) restart"
    );
}

#[test]
fn set_active_profile_rejects_a_confusable_name() {
    let app = test_app();
    let webview = test_webview(&app);

    // A whitespace-padded name maps to a distinct `.db` file — the allowlist
    // rejects it as a DOMAIN error (the command ran and validated), not an
    // arg-shape rejection. Proves `validate_profile_name` guards this writer.
    let res = call(
        &webview,
        "set_active_profile",
        json!({ "args": { "id": "work " } }),
    );
    let err = res.expect_err("a padded/confusable name must be rejected");
    let msg = err.as_str().unwrap_or_default();
    assert!(
        !is_ipc_arg_rejection(msg),
        "expected a domain-level validation error, got an arg rejection: {msg}"
    );
    assert!(
        msg.contains("invalid profile name"),
        "expected the validator's message, got: {msg}"
    );

    // A rejected set persists nothing — the read still returns the default.
    let got =
        call(&webview, "get_active_profile", json!({})).expect("get_active_profile must dispatch");
    let got: Value = got.deserialize().expect("valid JSON");
    assert_eq!(
        got, "personal",
        "a rejected set must not have written a row"
    );
}

// ── scheduled jobs (the ScheduledJobs screen surface) ──────────────────

#[test]
fn cron_jobs_list_toggle_delete_round_trip_through_real_ipc() {
    let app = test_app();
    let webview = test_webview(&app);

    // Empty profile → empty list (correct nested-args shape dispatches).
    let got = call(
        &webview,
        "list_cron_jobs",
        json!({ "args": { "profile": "personal" } }),
    )
    .expect("list_cron_jobs must dispatch");
    let got: Value = got.deserialize().expect("valid JSON");
    assert_eq!(got, json!([]), "a fresh profile has no scheduled jobs");

    // Seed one job directly at the storage layer (creation is agent-driven
    // via the Dangerous manage_cron tool, not this IPC surface).
    {
        let state: tauri::State<'_, crate::AppState> = app.state();
        let db = state
            .storage
            .open_profile("personal")
            .expect("open profile");
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
    let got = call(
        &webview,
        "list_cron_jobs",
        json!({ "args": { "profile": "personal" } }),
    )
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
    let got = call(
        &webview,
        "list_cron_jobs",
        json!({ "args": { "profile": "personal" } }),
    )
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
    let got = call(
        &webview,
        "list_cron_jobs",
        json!({ "args": { "profile": "personal" } }),
    )
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

// ── workspace files (the Files screen surface) ─────────────────────────

#[test]
fn list_workspace_files_lists_and_confines_to_the_profile_tree() {
    let app = test_app();
    let webview = test_webview(&app);

    // Fresh profile → empty listing (and the workspace dir is created).
    let got = call(
        &webview,
        "list_workspace_files",
        json!({ "args": { "profile": "personal", "subpath": "" } }),
    )
    .expect("list_workspace_files must dispatch");
    let got: Value = got.deserialize().expect("valid JSON");
    assert_eq!(got, json!([]));

    // Seed a file + a subdir directly on disk in the profile's Tier-P tree.
    {
        let state: tauri::State<'_, crate::AppState> = app.state();
        let ws = crate::tools::fs::profile_workspace_path(
            &state.storage.base_path().join("workspace"),
            "personal",
        );
        std::fs::create_dir_all(ws.join("notes")).expect("mkdir notes");
        std::fs::write(ws.join("draft.md"), b"hello").expect("write file");
    }

    let got = call(
        &webview,
        "list_workspace_files",
        json!({ "args": { "profile": "personal", "subpath": "" } }),
    )
    .expect("list_workspace_files must dispatch");
    let got: Value = got.deserialize().expect("valid JSON");
    let rows = got.as_array().expect("array");
    assert_eq!(rows.len(), 2);
    // Dirs sort first.
    assert_eq!(rows[0]["name"], "notes");
    assert_eq!(rows[0]["is_dir"], true);
    assert_eq!(rows[1]["name"], "draft.md");
    assert_eq!(rows[1]["size_bytes"], 5);

    // Subpath browsing works…
    let got = call(
        &webview,
        "list_workspace_files",
        json!({ "args": { "profile": "personal", "subpath": "notes" } }),
    )
    .expect("subpath listing must dispatch");
    let got: Value = got.deserialize().expect("valid JSON");
    assert_eq!(got, json!([]));

    // …but traversal is a DOMAIN error (the command ran and refused), and a
    // sibling profile's tree is unreachable via subpath from this profile.
    let err = call(
        &webview,
        "list_workspace_files",
        json!({ "args": { "profile": "personal", "subpath": "../work" } }),
    )
    .expect_err("traversal must be refused");
    let msg = err.as_str().unwrap_or_default();
    assert!(
        msg.contains("invalid subpath"),
        "expected the traversal refusal, got: {msg}"
    );
}

// ── Gmail (the email round) ────────────────────────────────────────────

#[test]
fn gmail_setup_status_and_client_paste_round_trip_through_real_ipc() {
    let app = test_app();
    let webview = test_webview(&app);

    // Fresh install: nothing configured, nothing connected.
    let got = call(
        &webview,
        "gmail_setup_status",
        json!({ "args": { "profile": "personal" } }),
    )
    .expect("gmail_setup_status must dispatch");
    let got: Value = got.deserialize().expect("valid JSON");
    assert_eq!(got["client_configured"], false);
    assert_eq!(got["connected"], false);
    assert_eq!(got["account_email"], Value::Null);
    assert_eq!(got["needs_reconnect"], false);
    // The disabled-API state is a DISTINCT field, absent on a fresh install.
    // Its own field (not a flavour of `needs_reconnect`) is the wire contract
    // that lets the UI render a banner with no Reconnect button.
    assert_eq!(got["api_not_enabled"], Value::Null);

    // A mispasted client id is a DOMAIN error pointing at the console page.
    let err = call(
        &webview,
        "set_gmail_client",
        json!({ "args": { "client_id": "not-a-client-id", "client_secret": "s" } }),
    )
    .expect_err("a mispasted id must be rejected");
    let msg = err.as_str().unwrap_or_default();
    assert!(msg.contains("apps.googleusercontent.com"), "got: {msg}");

    // A plausible client persists; status flips.
    call(
        &webview,
        "set_gmail_client",
        json!({ "args": {
            "client_id": "1234567890-abcdef.apps.googleusercontent.com",
            "client_secret": "GOCSPX-something"
        } }),
    )
    .expect("a plausible client must persist");
    let got = call(
        &webview,
        "gmail_setup_status",
        json!({ "args": { "profile": "personal" } }),
    )
    .expect("gmail_setup_status must dispatch");
    let got: Value = got.deserialize().expect("valid JSON");
    assert_eq!(got["client_configured"], true);
    assert_eq!(
        got["connected"], false,
        "a pasted client alone is not a connection"
    );
}

#[test]
fn gmail_flows_fail_closed_with_setup_pointing_errors() {
    let app = test_app();
    let webview = test_webview(&app);

    // finish without begin: a clear domain error.
    let err = call(
        &webview,
        "gmail_finish_connect",
        json!({ "args": { "profile": "personal" } }),
    )
    .expect_err("finish without begin must be refused");
    assert!(
        err.as_str().unwrap_or_default().contains("Connect"),
        "got: {err:?}"
    );

    // Reading mail without any client/connection: a setup-pointing error,
    // and no network is touched (the failure happens at the keychain).
    let err = call(
        &webview,
        "list_email",
        json!({ "args": { "profile": "personal" } }),
    )
    .expect_err("list_email without setup must be refused");
    assert!(
        err.as_str()
            .unwrap_or_default()
            .contains("Settings → Email"),
        "got: {err:?}"
    );

    // Disconnect is idempotent — never an error on an unconnected profile.
    call(
        &webview,
        "gmail_disconnect",
        json!({ "args": { "profile": "personal" } }),
    )
    .expect("disconnect must be idempotent");
}

/// `google_clear_api_not_enabled` — the banner's "I've enabled it — check
/// again" — end to end through real IPC.
///
/// It had NO coverage at all: not a dispatch test, not a behaviour test, and
/// it was missing from the contract-test command list, so nothing would have
/// noticed if it stopped being registered. It is also the only way out of a
/// state the app cannot observe being fixed (the user switches the API on in
/// the Google console), which makes "it dispatches and does the right thing"
/// load-bearing rather than incidental.
#[test]
fn google_clear_api_not_enabled_round_trips_through_real_ipc() {
    use crate::email::api_error::{google_api_error, GoogleApi};

    let app = test_app();
    let webview = test_webview(&app);
    let state = app.state::<AppState>();

    // Two APIs off for this profile, recorded the way a real failure records
    // them: from the classifier's TYPED verdict.
    let disabled_body = r#"{"error":{"code":403,"status":"PERMISSION_DENIED","details":[
        {"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"SERVICE_DISABLED"}]}}"#;
    for api in [GoogleApi::Gmail, GoogleApi::Tasks] {
        state
            .email
            .google
            .observe_failure("personal", &google_api_error(api, 403, disabled_body, "s"));
    }

    let status: Value = call(
        &webview,
        "gmail_setup_status",
        json!({ "args": { "profile": "personal" } }),
    )
    .expect("gmail_setup_status must dispatch")
    .deserialize()
    .expect("valid JSON");
    assert_eq!(
        status["api_not_enabled"]["apis"],
        json!([
            { "id": "gmail", "label": "Gmail", "console_url": null },
            { "id": "tasks", "label": "Google Tasks", "console_url": null },
        ]),
        "the wire contract names the APIs the banner reports, ONE BY ONE: the \
         screen rendering it can only re-test some of them, and matches on the \
         wire id"
    );
    assert_eq!(
        status["needs_reconnect"], false,
        "a disabled API is not a dead grant"
    );

    // A clear scoped to what the asking screen can re-test leaves the rest
    // standing — Email's re-check must not blank a Tasks banner that nothing
    // is about to retry.
    call(
        &webview,
        "google_clear_api_not_enabled",
        json!({ "args": { "profile": "personal", "apis": ["gmail"] } }),
    )
    .expect("a scoped clear must dispatch");
    let status: Value = call(
        &webview,
        "gmail_setup_status",
        json!({ "args": { "profile": "personal" } }),
    )
    .expect("gmail_setup_status must dispatch")
    .deserialize()
    .expect("valid JSON");
    assert_eq!(
        status["api_not_enabled"]["apis"],
        json!([{ "id": "tasks", "label": "Google Tasks", "console_url": null }])
    );

    // An unknown API name is a DOMAIN error naming what it expected — not a
    // silent no-op that reports success while the banner stays lit.
    let err = call(
        &webview,
        "google_clear_api_not_enabled",
        json!({ "args": { "profile": "personal", "apis": ["drive"] } }),
    )
    .expect_err("an unknown API name must be refused");
    let msg = err.as_str().unwrap_or_default();
    assert!(msg.contains("drive") && msg.contains("gmail"), "got: {msg}");
    let status: Value = call(
        &webview,
        "gmail_setup_status",
        json!({ "args": { "profile": "personal" } }),
    )
    .expect("gmail_setup_status must dispatch")
    .deserialize()
    .expect("valid JSON");
    assert_eq!(
        status["api_not_enabled"]["apis"],
        json!([{ "id": "tasks", "label": "Google Tasks", "console_url": null }]),
        "a refused clear must not have cleared anything"
    );

    // No `apis` at all = the whole profile, and the field is optional on the
    // wire (an older caller sending just a profile still works).
    call(
        &webview,
        "google_clear_api_not_enabled",
        json!({ "args": { "profile": "personal" } }),
    )
    .expect("a bare clear must dispatch");
    let status: Value = call(
        &webview,
        "gmail_setup_status",
        json!({ "args": { "profile": "personal" } }),
    )
    .expect("gmail_setup_status must dispatch")
    .deserialize()
    .expect("valid JSON");
    assert_eq!(status["api_not_enabled"], Value::Null);

    // A bad profile name is refused here like everywhere else.
    let err = call(
        &webview,
        "google_clear_api_not_enabled",
        json!({ "args": { "profile": "../escape" } }),
    )
    .expect_err("an invalid profile must be refused");
    assert!(
        !err.as_str().unwrap_or_default().is_empty(),
        "the refusal must say something"
    );
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
    assert_eq!(
        reget["strictness"], 100,
        "persisted strictness must survive a re-read"
    );
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
    assert_eq!(
        red["strictness"], 100,
        "redaction toggle preserved thresholds"
    );
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
    assert!(
        value["fit"].is_string(),
        "expected a fit verdict: {value:?}"
    );
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
    let set = call(
        &webview,
        "set_sandbox_config",
        json!({"args": {"profile": "personal", "config": cfg}}),
    );
    set.expect("a valid config must persist");
    let got = call(
        &webview,
        "get_sandbox_config",
        json!({"args": {"profile": "personal"}}),
    )
    .expect("get must dispatch")
    .deserialize::<Value>()
    .unwrap();
    assert_eq!(
        got["network"]["allow_localhost"], false,
        "the locked-down ceiling round-trips"
    );
    assert_eq!(got["network"]["allowed_domains"][0], "example.com");
    assert_eq!(got["enabled"], true);
}

#[test]
fn get_sandbox_config_returns_default_when_unset() {
    let app = test_app();
    let webview = test_webview(&app);
    let got = call(
        &webview,
        "get_sandbox_config",
        json!({"args": {"profile": "personal"}}),
    )
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
    let res = call(
        &webview,
        "set_sandbox_config",
        json!({"args": {"profile": "personal", "config": cfg}}),
    );
    let err = res.expect_err("an empty allowed_domain must be rejected");
    let msg = err.as_str().unwrap_or_default();
    assert!(
        msg.contains("must not be empty"),
        "expected a validation error, got: {msg}"
    );
    assert!(
        !is_ipc_arg_rejection(msg),
        "it's a domain-level validation error, not an arg-shape rejection"
    );
}

#[test]
fn sandbox_config_old_broken_shape_is_rejected() {
    let app = test_app();
    let webview = test_webview(&app);
    // No `args` wrapper.
    let res = call(
        &webview,
        "get_sandbox_config",
        json!({"profile": "personal"}),
    );
    let err = res.expect_err("flat/unwrapped args must NOT dispatch");
    assert!(is_ipc_arg_rejection(err.as_str().unwrap_or_default()));
}

// ── budget_settings (C1) ───────────────────────────────────────────────────

#[test]
fn set_then_get_budget_settings_round_trips_and_reset_clears() {
    let app = test_app();
    let webview = test_webview(&app);
    call(
        &webview,
        "set_budget_settings",
        json!({"args": {"profile": "personal", "cap_usd": 12.5}}),
    )
    .expect("set a cap");
    let got = call(
        &webview,
        "get_budget_settings",
        json!({"args": {"profile": "personal"}}),
    )
    .expect("get")
    .deserialize::<Value>()
    .unwrap();
    assert_eq!(got["cap_usd"], 12.5, "the cap round-trips");
    call(
        &webview,
        "reset_budget_settings",
        json!({"args": {"profile": "personal"}}),
    )
    .expect("reset");
    let after = call(
        &webview,
        "get_budget_settings",
        json!({"args": {"profile": "personal"}}),
    )
    .expect("get")
    .deserialize::<Value>()
    .unwrap();
    assert!(after["cap_usd"].is_null(), "reset → uncapped (null)");
}

#[test]
fn budget_settings_old_broken_shape_is_rejected() {
    let app = test_app();
    let webview = test_webview(&app);
    let res = call(
        &webview,
        "get_budget_settings",
        json!({"profile": "personal"}),
    ); // no `args`
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
        "set_update_check_enabled",
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
        "list_workspace_files",
        "gmail_setup_status",
        "set_gmail_client",
        "gmail_begin_connect",
        "gmail_finish_connect",
        "gmail_disconnect",
        "google_clear_api_not_enabled",
        "list_email",
        "read_email",
        "send_email",
        "list_calendar_events",
        "create_calendar_event",
        "delete_calendar_event",
        "list_google_tasks",
        "create_google_task",
        "set_google_task_completed",
        "delete_google_task",
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
        "reapprove_mcp_server",
    ];
    for cmd in args_cmds {
        let res = call(&webview, cmd, flat.clone());
        let err = res
            .err()
            .unwrap_or_else(|| panic!("{cmd}: an unwrapped body must be rejected, not accepted"));
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
        "get_update_check_enabled",
        "list_agent_types",
        "probe_hardware",
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

// ── C-01 / H-12: the gate's state is reachable from the real IPC table ──────

/// C-01's finding was that the degraded flag had **zero call sites**. This test
/// is the structural answer: it drives `get_classifier_health` through the real
/// `generate_handler!` table against the real `AppState`, and proves the value it
/// reports tracks the shared `ClassifierHealth` arc the GATE enforces on — i.e.
/// that flipping the gate's flag actually changes what the UI would be told.
#[test]
fn get_classifier_health_reports_the_gates_shared_degraded_flag() {
    let app = test_app();
    let webview = test_webview(&app);

    let before: Value = call(&webview, "get_classifier_health", json!({}))
        .expect("get_classifier_health must dispatch")
        .deserialize()
        .expect("valid JSON");
    assert_eq!(before["degraded"], false, "the harness gate starts healthy");
    assert!(
        before["confirm_ttl_secs"].as_u64().unwrap_or(0) > 0,
        "the UI needs a real TTL to display: {before:?}"
    );

    // Flip the flag on the gate the agent loop enforces with.
    app.state::<AppState>()
        .gate
        .health()
        .mark_degraded("models dir missing");

    let after: Value = call(&webview, "get_classifier_health", json!({}))
        .expect("get_classifier_health must dispatch")
        .deserialize()
        .expect("valid JSON");
    assert_eq!(
        after["degraded"], true,
        "the IPC read must observe the gate's flag, not a stale copy"
    );
    assert_eq!(after["reason"], "models dir missing");
}

/// H-12: the confirmation round trip, end to end through the command table.
/// One `confirm_public_send` authorises exactly ONE subsequent send of that
/// exact text, then the gate asks again.
#[test]
fn confirm_public_send_authorises_exactly_one_send_through_the_real_gate() {
    use crate::agent::gate::{Binding, GateDecision};
    use crate::classifier::ClassifierConfig;

    let app = test_app();
    let webview = test_webview(&app);
    let gate = app.state::<AppState>().gate.clone();
    let cfg = ClassifierConfig::default();
    let text = "my SSN is 123-45-6789";

    // Before any confirmation the gate holds the message.
    assert!(matches!(
        gate.check(&Binding::Public, text, true, &cfg),
        GateDecision::ConfirmRequired { .. }
    ));

    let res: Value = call(
        &webview,
        "confirm_public_send",
        json!({ "args": { "text": text } }),
    )
    .expect("confirm_public_send must dispatch with wrapped args")
    .deserialize()
    .expect("valid JSON");
    assert_eq!(
        res["fingerprint"].as_str().unwrap_or_default().len(),
        64,
        "a sha256 fingerprint: {res:?}"
    );

    // The grant landed on the SAME gate the loop uses → the send goes through...
    assert_eq!(
        gate.check(&Binding::Public, text, true, &cfg),
        GateDecision::Allow
    );
    // ...exactly once.
    assert!(
        matches!(
            gate.check(&Binding::Public, text, true, &cfg),
            GateDecision::ConfirmRequired { .. }
        ),
        "one confirmation must not authorise a second send"
    );
}

#[test]
fn confirm_public_send_rejects_the_old_unwrapped_arg_shape() {
    let app = test_app();
    let webview = test_webview(&app);
    let err = call(&webview, "confirm_public_send", json!({ "text": "x" }))
        .expect_err("flat/unwrapped args must NOT dispatch");
    let msg = err.as_str().unwrap_or_default();
    assert!(
        is_ipc_arg_rejection(msg),
        "expected an IPC-level arg rejection, got: {msg}"
    );
}
// ── H-07: the MCP install consent gate, through the real command ────────────
//
// These drive the actual `#[tauri::command] register_mcp_server` over IPC —
// real `App<MockRuntime>`, real `AppState`, real `generate_handler!`
// deserialization — so what is proven is the deployed boundary, not just the
// helper the gate is factored into (`ipc::tests` covers that separately).

/// The command's own error string, for a call that is expected to fail.
fn call_err(webview: &WebviewWindow<MockRuntime>, cmd: &str, body: Value) -> String {
    match call(webview, cmd, body) {
        Ok(ok) => panic!("{cmd}: expected a refusal, got success: {ok:?}"),
        Err(e) => e.as_str().unwrap_or_default().to_string(),
    }
}

/// A nonce minted by the backend, over IPC.
fn issued_nonce(webview: &WebviewWindow<MockRuntime>) -> String {
    let v: Value = call(webview, "generate_mcp_install_nonce", json!({}))
        .expect("generate_mcp_install_nonce must dispatch")
        .deserialize()
        .expect("the nonce response must be JSON");
    v.as_str().expect("the nonce must be a string").to_string()
}

/// A registration payload for a command that cannot possibly resolve, so the
/// only thing under test is *where* the call dies.
fn register_body(nonce: Option<&str>) -> Value {
    let mut args = json!({"name": "srv", "command": "/nonexistent/lhp-mcp-server"});
    if let Some(n) = nonce {
        args["nonce"] = json!(n);
    }
    json!({"args": args})
}

/// H-07 gap (b) at the real boundary: a renderer that calls
/// `register_mcp_server` without going through the consent step is refused.
///
/// The third leg is what makes the second meaningful: a *valid* nonce gets past
/// the gate and the call then dies further in (at pinning), so "not confirmed"
/// is specifically the gate talking and not an incidental failure.
#[test]
fn register_mcp_server_demands_a_backend_nonce() {
    let app = test_app();
    let webview = test_webview(&app);

    // (1) No `nonce` field at all — serde rejects the args before the body runs.
    let msg = call_err(&webview, "register_mcp_server", register_body(None));
    assert!(
        msg.contains("invalid args") && msg.contains("nonce"),
        "a payload omitting nonce must be rejected at the IPC boundary, got: {msg}"
    );

    // (2) A forged nonce the backend never issued — the gate refuses.
    let msg = call_err(
        &webview,
        "register_mcp_server",
        register_body(Some("00000000-0000-0000-0000-000000000000")),
    );
    assert!(
        msg.contains("was not confirmed"),
        "a forged nonce must hit the consent gate, got: {msg}"
    );

    // (3) A backend-issued nonce passes the gate and the call proceeds — it then
    // fails on the unresolvable command, which is a *different* error.
    let msg = call_err(
        &webview,
        "register_mcp_server",
        register_body(Some(&issued_nonce(&webview))),
    );
    assert!(
        !msg.contains("was not confirmed"),
        "an issued nonce must get past the gate, got: {msg}"
    );
    assert!(
        msg.contains("couldn't pin the MCP server executable"),
        "expected the call to proceed as far as pinning, got: {msg}"
    );
}

/// Round-4, at the real boundary: sandbox grants are validated BEFORE anything
/// is spawned or pinned, and they are refused outright for an HTTP endpoint
/// (which has no local child to confine). Both legs use a backend-issued nonce
/// so the consent gate is not what is being observed.
#[test]
fn register_mcp_server_validates_sandbox_grants_before_spawning() {
    let app = test_app();
    let webview = test_webview(&app);

    // A relative grant path has no fixed meaning — refused, and refused BEFORE
    // the unresolvable command would have failed pinning.
    let msg = call_err(
        &webview,
        "register_mcp_server",
        json!({"args": {
            "name": "srv",
            "command": "/nonexistent/lhp-mcp-server",
            "read_paths": ["relative/path"],
            "nonce": issued_nonce(&webview),
        }}),
    );
    assert!(
        msg.contains("must be absolute"),
        "a relative grant must be refused, got: {msg}"
    );

    // An absolute path that does not exist cannot be granted either.
    let msg = call_err(
        &webview,
        "register_mcp_server",
        json!({"args": {
            "name": "srv",
            "command": "/nonexistent/lhp-mcp-server",
            "write_paths": ["/nonexistent/lhp-grant-target"],
            "nonce": issued_nonce(&webview),
        }}),
    );
    assert!(
        msg.contains("cannot be granted"),
        "a missing grant target must be refused, got: {msg}"
    );

    // An HTTP endpoint has no child — grants there would be a UI lie.
    let msg = call_err(
        &webview,
        "register_mcp_server",
        json!({"args": {
            "name": "srv",
            "command": "https://example.com/mcp",
            "network_access": true,
            "nonce": issued_nonce(&webview),
        }}),
    );
    assert!(
        msg.contains("no local child to sandbox"),
        "grants on an HTTP endpoint must be refused, got: {msg}"
    );
}

/// Re-approval re-trusts an executable exactly like first registration did, so
/// it sits behind the SAME consent gate — proven at the real boundary. Without
/// this, a compromised renderer could silently re-pin a swapped binary and
/// defeat the whole fail-closed property the pin gate exists for.
#[test]
fn reapprove_mcp_server_demands_a_backend_nonce() {
    let app = test_app();
    let webview = test_webview(&app);

    // (1) No `nonce` field at all — serde rejects the args before the body runs.
    let msg = call_err(
        &webview,
        "reapprove_mcp_server",
        json!({"args": {"id": "srv"}}),
    );
    assert!(
        msg.contains("invalid args") && msg.contains("nonce"),
        "a payload omitting nonce must be rejected at the IPC boundary, got: {msg}"
    );

    // (2) A forged nonce the backend never issued — the gate refuses.
    let msg = call_err(
        &webview,
        "reapprove_mcp_server",
        json!({"args": {"id": "srv", "nonce": "00000000-0000-0000-0000-000000000000"}}),
    );
    assert!(
        msg.contains("was not confirmed"),
        "a forged nonce must hit the consent gate, got: {msg}"
    );

    // (3) An issued nonce passes the gate and the call proceeds — it then dies
    // on the unknown id, a *different* error, so "not confirmed" above is
    // specifically the gate talking.
    let msg = call_err(
        &webview,
        "reapprove_mcp_server",
        json!({"args": {"id": "srv", "nonce": issued_nonce(&webview)}}),
    );
    assert!(
        !msg.contains("was not confirmed"),
        "an issued nonce must get past the gate, got: {msg}"
    );
    assert!(
        msg.contains("no MCP server"),
        "expected the call to proceed as far as the row lookup, got: {msg}"
    );
}

/// Single-use, proven through the command: the same issued nonce cannot be
/// replayed even though the first call ultimately failed downstream of the gate.
#[test]
fn a_replayed_install_nonce_is_refused_by_the_command() {
    let app = test_app();
    let webview = test_webview(&app);
    let nonce = issued_nonce(&webview);

    let first = call_err(&webview, "register_mcp_server", register_body(Some(&nonce)));
    assert!(
        first.contains("couldn't pin the MCP server executable"),
        "the first call must consume the nonce and die later, got: {first}"
    );

    let second = call_err(&webview, "register_mcp_server", register_body(Some(&nonce)));
    assert!(
        second.contains("was not confirmed"),
        "a replayed nonce must be refused, got: {second}"
    );
}
