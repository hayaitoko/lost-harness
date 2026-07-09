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
use crate::trm::HeuristicClassifier;

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
    let agent_loop = Arc::new(AgentLoop::new(
        gate,
        Arc::clone(&model_manager),
        Arc::clone(&storage),
        // `send_message` (the only command that uses the dispatcher) isn't
        // registered in this harness, so an inert dispatcher is enough.
        Arc::new(crate::tools::ToolDispatcher::empty()),
    ));

    let state = AppState {
        agent_loop,
        model_manager,
        storage,
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
            ipc::list_profiles,
            ipc::list_conversations,
            ipc::create_conversation,
            ipc::get_messages,
            ipc::list_providers,
            ipc::add_provider,
            ipc::remove_provider,
            ipc::list_models,
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
