//! Lost Harness — IPC layer (frontend ↔ Rust core)
//!
//! M1 surface: real commands backed by the §9 agent loop, §4 model
//! manager, and §5 storage. Streaming tokens arrive via the
//! `stream:token` event; `stream:error` carries gate / routing / model
//! failures.
//!
//! Conventions:
//! - Commands are sync or `async` functions marked with `#[tauri::command]`.
//! - Every command that may fail returns `Result<T, String>` (frontend sees
//!   the rejection as a JS error).
//! - Events emitted to the frontend follow the naming scheme
//!   `<domain>:<action>` (e.g. `stream:token`). Payloads are serde-derived
//!   structs that the TS bridge mirrors.
//! - The frontend bridge is `src/lib/api/tauri.ts`. Any new command or event
//!   here must be reflected there.
//!
//! App state: `Storage` is genuinely `Send + Sync` (see `storage::Storage`)
//! — `GlobalDb` and `ProfileDb` each hold their `rusqlite::Connection`
//! behind a `parking_lot::Mutex`, so concurrent IPC commands and the agent
//! loop can all safely hold a `Storage` handle at once; no manual/unsafe
//! impl is needed or present.
//!
//! Each top-level handle is held in an `Arc` directly (no outer Mutex
//! at the AppState level) so the IPC commands can access them
//! synchronously. The agent loop carries its own internal `Mutex` for
//! stream serialization.

#[cfg(test)]
mod contract_tests;

pub mod approval;
pub mod ask_human;

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use uuid::Uuid;

use crate::agent::gate::Binding;
use crate::agent::loop_mod::AgentLoop;
use crate::agent::result_sink::{ResultSink, TauriResultSink};
use crate::hooks::{ApprovalDecision, GrantScope, GrantTarget, PermissionMode, ToolRule};
use crate::ipc::approval::ApprovalRegistry;
use crate::ipc::ask_human::AskHumanRegistry;
use crate::models::{ModelManager, Provider, ProviderKind};
use crate::storage::{Conversation, Message, Storage};

// ── App state ────────────────────────────────────────────────────────────

/// Shared application state. Tauri stores this via `.manage(state)` and
/// injects it into commands with `state: State<'_, AppState>`. Each
/// field is an `Arc<T>` where `T: Send + Sync`. See the module docs
/// for why `Storage` is genuinely `Send + Sync`.
pub struct AppState {
    pub agent_loop: Arc<AgentLoop>,
    pub model_manager: Arc<ModelManager>,
    pub storage: Arc<Storage>,
    /// OS-backed provider credential store. SQLite contains only an opaque
    /// presence marker; tests inject the in-memory implementation.
    pub provider_secrets: Arc<dyn crate::secrets::ProviderSecretStore>,
    /// In-flight tool-approval prompts (§3.5). The dispatcher parks a request
    /// here and awaits it; `resolve_tool_approval` answers by id.
    pub approvals: Arc<ApprovalRegistry>,
    /// In-flight `ask_human` prompts. The tool parks a question here and awaits
    /// it; `resolve_ask_human` delivers the user's answer by id.
    pub ask_human: Arc<AskHumanRegistry>,
    /// The active privacy classifier (trained ensemble or rules-only fallback),
    /// shared with the §7 gate. Backs `explain_classification` for the
    /// annotated-redaction "why" sidebar (PLAN §11).
    pub classifier: Arc<dyn crate::classifier::Classifier>,
    /// Memory's meaning-lane embedder handle (PLAN §9). Loads lazily + only
    /// when a profile has semantic search enabled (Wave 1.2); `None` ⇒ no model
    /// dir configured, saves stay keyword-indexed.
    pub embedder: Option<Arc<crate::embedder::EmbedderHandle>>,
    /// C4: the live tool dispatcher (same `Arc` the agent loop drives). Held so
    /// `set_skill_approval`/`delete_skill` can hot-register an approved skill
    /// as a callable Tool and unregister it on rejection/delete.
    pub tools: Arc<crate::tools::ToolDispatcher>,
    /// C3: the live MCP server registry (spawned stdio children + their
    /// registered tool names). The persisted config lives in `mcp_servers`;
    /// this is derived session state.
    pub mcp: Arc<crate::tools::mcp_stdio::McpRuntime>,
    /// A4: the machine's hardware profile, probed ONCE at boot and cached here.
    /// `hardware::probe()` shells out to `system_profiler` (hundreds of ms per
    /// call), so `probe_hardware` / `list_model_catalog` / `calculate_model_fit`
    /// read this snapshot instead of re-probing.
    pub hardware: Arc<crate::models::hardware::HardwareProfile>,
    /// M8 S4: the bundled-sidecar context (supervisor + resolved binary).
    /// `None` ⇒ no sidecar binary resolved at boot — local models need an
    /// external runner. Used by `remove_local_model` (stop-before-delete) and
    /// the app-exit teardown.
    #[cfg(feature = "local-runner")]
    pub local_runner: Option<Arc<crate::models::runner::LocalRunnerContext>>,
}

// ── Response types ───────────────────────────────────────────────────────

/// Returned by `send_message` once the agent finishes a turn. Streaming
/// tokens arrive separately via the `stream:token` event.
#[derive(Debug, Clone, Serialize)]
pub struct SendMessageResponse {
    pub message_id: String,
    pub content: String,
    pub conversation_id: String,
    /// "personal" | "work" | "school" | "developer" — the profile the
    /// message was handled under.
    pub profile: String,
    /// "allow" | "route_local" — which branch of the gate served this
    /// message. Frontend uses this to label the chip / banner.
    pub routing_decision: String,
    pub completed_at: i64,
}

/// Mirrors a `Provider` row for the model picker (§4). API key is
/// omitted; the picker only needs id / name / base_url / kind.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub kind: ProviderKind,
    pub is_private: bool,
    /// True when locality is trusted only from a DNS/mDNS/tailnet suffix
    /// (`.local`, `.lan`, `.internal`, `.ts.net`) rather than a loopback or
    /// private IP literal. The provider UI must surface a one-time warning:
    /// only use these endpoints on a network the user controls.
    pub trusted_by_name: bool,
    pub supports_native_tools: bool,
}

impl From<Provider> for ProviderInfo {
    fn from(p: Provider) -> Self {
        // Compute `is_private` first — it takes `&self` and we want to
        // move `id`/`name`/`base_url`/`kind` afterwards.
        let is_private = p.is_private();
        let trusted_by_name = crate::agent::egress::is_private_endpoint_trusted_by_name(&p.base_url);
        Self {
            id: p.id,
            name: p.name,
            base_url: p.base_url,
            kind: p.kind,
            is_private,
            trusted_by_name,
            supports_native_tools: p.supports_native_tools,
        }
    }
}

/// Mirrors a `Conversation` row for the sidebar / session list.
#[derive(Debug, Clone, Serialize)]
pub struct ConversationInfo {
    pub id: String,
    pub name: String,
    pub pinned: bool,
    pub binding: String,
    pub folder_id: Option<String>,
    pub color: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<Conversation> for ConversationInfo {
    fn from(c: Conversation) -> Self {
        Self {
            id: c.id,
            name: c.name,
            pinned: c.pinned,
            binding: c.binding,
            folder_id: c.folder_id,
            color: c.color,
            created_at: c.created_at,
            updated_at: c.updated_at,
        }
    }
}

/// Mirrors a `Message` row for the transcript.
#[derive(Debug, Clone, Serialize)]
pub struct MessageInfo {
    pub id: String,
    pub conversation_id: String,
    pub role: String,
    pub content: String,
    pub model: Option<String>,
    pub provider_id: Option<String>,
    pub routing_decision: Option<String>,
    pub thinking_content: Option<String>,
    pub error: Option<String>,
    pub aborted: bool,
    pub created_at: i64,
}

impl From<Message> for MessageInfo {
    fn from(m: Message) -> Self {
        Self {
            id: m.id,
            conversation_id: m.conversation_id,
            role: m.role,
            content: m.content,
            model: m.model,
            provider_id: m.provider_id,
            routing_decision: m.routing_decision,
            thinking_content: m.thinking_content,
            error: m.error,
            aborted: m.aborted,
            created_at: m.created_at,
        }
    }
}

// ── Request payloads ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct SendMessageArgs {
    pub content: String,
    pub conversation_id: String,
    pub binding: String,
    pub provider_id: String,
    pub model: String,
    /// Profile name. Frontend should always send this; we keep it
    /// required (not `Option`) to make the wiring explicit and to avoid
    /// accidentally writing a personal-profile message to a work db.
    pub profile: String,
    /// Q11 permission mode for this turn: `"normal"` (default), `"plan"`
    /// (read-only), or `"accept_edits"` (auto-approve local edits). Optional +
    /// lenient so an older frontend that omits it gets `Normal`.
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AddProviderArgs {
    pub name: String,
    pub base_url: String,
    /// `None` for local endpoints that don't require auth.
    #[serde(default)]
    pub api_key: Option<String>,
    /// "local" | "cloud" | "custom"
    pub kind: String,
    /// Q1: endpoint supports OpenAI-style native structured tool calls.
    #[serde(default)]
    pub supports_native_tools: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateProviderArgs {
    pub id: String,
    pub name: String,
    pub base_url: String,
    /// `None` keeps the stored key — the Settings edit form never echoes a
    /// stored key back, so absence is not a request to clear it.
    #[serde(default)]
    pub api_key: Option<String>,
    /// "local" | "cloud" | "custom"
    pub kind: String,
    /// Q1: endpoint supports OpenAI-style native structured tool calls.
    #[serde(default)]
    pub supports_native_tools: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateConversationArgs {
    pub name: String,
    /// Defaults to "auto" if omitted.
    #[serde(default = "default_binding")]
    pub binding: String,
    /// Profile name. Frontend always supplies it.
    pub profile: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProfileScopedArgs {
    pub profile: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GetMessagesArgs {
    pub profile: String,
    pub conversation_id: String,
}

fn default_binding() -> String {
    "auto".to_string()
}

// ── Commands ─────────────────────────────────────────────────────────────

/// Returns the app version string.
#[tauri::command]
pub fn get_app_version() -> String {
    "0.1.0-m1".to_string()
}

/// Returns the id of the currently active profile — the one the user last
/// switched to, persisted in `global.db`'s `app_settings` so it survives a
/// restart. Falls back to `"personal"` when nothing is stored yet (fresh
/// install) or the stored value is somehow unreadable/invalid.
/// `set_active_profile` is the writer; the frontend's `switchProfile` calls it.
#[tauri::command]
pub fn get_active_profile(state: State<'_, AppState>) -> String {
    state
        .storage
        .global()
        .active_profile()
        // Defense-in-depth: only trust a stored value that still passes the
        // allowlist (it was validated on write; this guards a loosened schema
        // or a hand-edited db). Anything else falls back to the default.
        .filter(|id| crate::storage::validate_profile_name(id).is_ok())
        .unwrap_or_else(|| "personal".to_string())
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetActiveProfileArgs {
    pub id: String,
}

/// Persist the active-profile choice so it survives an app restart (writes the
/// `active_profile` row in `global.db`'s `app_settings`). Validates the id
/// against the same allowlist every profile-scoped command uses, so a
/// padded/confusable name can't be stored and later routed as a distinct db.
#[tauri::command]
pub fn set_active_profile(
    state: State<'_, AppState>,
    args: SetActiveProfileArgs,
) -> Result<(), String> {
    crate::storage::validate_profile_name(&args.id).map_err(|e| e.to_string())?;
    state
        .storage
        .global()
        .set_active_profile(&args.id)
        .map_err(|e| e.to_string())
}

/// Lists the profile ids known to the app. Matches the four-profile design
/// from the spec: personal / work / school / developer.
#[tauri::command]
pub fn list_profiles() -> Vec<String> {
    vec![
        "personal".to_string(),
        "work".to_string(),
        "school".to_string(),
        "developer".to_string(),
    ]
}

// ── Conversations ────────────────────────────────────────────────────────

#[tauri::command]
pub fn list_conversations(
    state: State<'_, AppState>,
    args: ProfileScopedArgs,
) -> Result<Vec<ConversationInfo>, String> {
    let db = state
        .storage
        .open_profile(&args.profile)
        .map_err(|e| e.to_string())?;
    db.list_conversations()
        .map(|rows| rows.into_iter().map(Into::into).collect())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn create_conversation(
    state: State<'_, AppState>,
    args: CreateConversationArgs,
) -> Result<ConversationInfo, String> {
    let db = state
        .storage
        .open_profile(&args.profile)
        .map_err(|e| e.to_string())?;
    let now = chrono::Utc::now().timestamp();
    let conv = Conversation {
        id: Uuid::new_v4().to_string(),
        name: args.name,
        pinned: false,
        binding: args.binding,
        folder_id: None,
        color: None,
        created_at: now,
        updated_at: now,
    };
    db.create_conversation(&conv).map_err(|e| e.to_string())?;
    // Wave 3.5 trigger #3: a new chat nudges a background consolidation pass over
    // the most-recent prior conversation (catches durable facts a short,
    // never-compacted chat missed). Fire-and-forget; never blocks this command.
    state
        .agent_loop
        .consolidate_on_new_chat(&args.profile, &conv.id);
    Ok(conv.into())
}

#[tauri::command]
pub fn get_messages(
    state: State<'_, AppState>,
    args: GetMessagesArgs,
) -> Result<Vec<MessageInfo>, String> {
    let db = state
        .storage
        .open_profile(&args.profile)
        .map_err(|e| e.to_string())?;
    db.list_messages_by_conversation(&args.conversation_id)
        .map(|rows| rows.into_iter().map(Into::into).collect())
        .map_err(|e| e.to_string())
}

// ── Providers + models ───────────────────────────────────────────────────

#[tauri::command]
pub fn list_providers(state: State<'_, AppState>) -> Result<Vec<ProviderInfo>, String> {
    Ok(state
        .model_manager
        .list_providers()
        .into_iter()
        .map(Into::into)
        .collect())
}

#[tauri::command]
pub fn add_provider(
    state: State<'_, AppState>,
    args: AddProviderArgs,
) -> Result<ProviderInfo, String> {
    let kind = parse_kind(&args.kind)?;
    let id = Uuid::new_v4().to_string();
    let provider = Provider::new(
        id.clone(),
        args.name,
        args.base_url,
        args.api_key,
        kind,
    )
    .with_native_tools(args.supports_native_tools);
    if let Some(secret) = provider.api_key.as_deref() {
        state.provider_secrets.set(&id, secret)?;
    }
    state.model_manager.add_provider(provider.clone());
    // Persist so the flag (and the endpoint) survive a restart and hydrate
    // back on next boot. Best-effort: a storage failure logs but the
    // in-memory provider still works for this session.
    if let Err(e) = state.storage.global().insert_endpoint(&crate::storage::Endpoint {
        id: id.clone(),
        name: provider.name.clone(),
        base_url: provider.base_url.clone(),
        api_key_marker: provider
            .api_key
            .as_ref()
            .map(|_| crate::secrets::KEYCHAIN_MARKER.to_vec()),
        kind: args.kind.clone(),
        created_at: chrono::Utc::now().timestamp(),
        supports_native_tools: provider.supports_native_tools,
    }) {
        tracing::warn!(error = %e, "failed to persist endpoint (in-memory only this session)");
    }
    Ok(provider.into())
}

#[tauri::command]
pub fn update_provider(
    state: State<'_, AppState>,
    args: UpdateProviderArgs,
) -> Result<ProviderInfo, String> {
    let kind = parse_kind(&args.kind)?;
    let existing = state
        .model_manager
        .get_provider(&args.id)
        .ok_or_else(|| format!("unknown provider: {}", args.id))?;
    let api_key = args.api_key.or(existing.api_key);
    let provider = Provider::new(
        args.id.clone(),
        args.name,
        args.base_url,
        api_key,
        kind,
    )
    .with_native_tools(args.supports_native_tools);
    if let Some(secret) = provider.api_key.as_deref() {
        state.provider_secrets.set(&args.id, secret)?;
    }
    // `ModelManager::add_provider` replaces by id and drops the cached
    // client, so the next request is built from the new base URL/key.
    state.model_manager.add_provider(provider.clone());
    // Persist with the same best-effort discipline as add_provider. An
    // UPDATE matching no row means the endpoint was never persisted (e.g.
    // the insert failed at add time) — insert it so the edit still
    // survives a restart.
    let api_key_marker = provider
        .api_key
        .as_ref()
        .map(|_| crate::secrets::KEYCHAIN_MARKER.to_vec());
    let persisted = state
        .storage
        .global()
        .update_endpoint(
            &args.id,
            &provider.name,
            &provider.base_url,
            api_key_marker.as_deref(),
            &args.kind,
            provider.supports_native_tools,
        )
        .and_then(|updated| {
            if updated {
                return Ok(());
            }
            state.storage.global().insert_endpoint(&crate::storage::Endpoint {
                id: args.id.clone(),
                name: provider.name.clone(),
                base_url: provider.base_url.clone(),
                api_key_marker,
                kind: args.kind.clone(),
                created_at: chrono::Utc::now().timestamp(),
                supports_native_tools: provider.supports_native_tools,
            })
        });
    if let Err(e) = persisted {
        tracing::warn!(error = %e, "failed to persist endpoint update (in-memory only this session)");
    }
    Ok(provider.into())
}

#[tauri::command]
pub fn remove_provider(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    if state.model_manager.get_provider(&id).is_none() {
        return Err(format!("unknown provider: {id}"));
    }
    state.provider_secrets.delete(&id)?;
    state
        .storage
        .global()
        .delete_endpoint(&id)
        .map_err(|e| e.to_string())?;
    state.model_manager.remove_provider(&id);
    Ok(true)
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderIdArgs {
    pub provider_id: String,
}

#[tauri::command]
pub async fn list_models(
    state: State<'_, AppState>,
    args: ProviderIdArgs,
) -> Result<Vec<String>, String> {
    state
        .model_manager
        .list_models_for(&args.provider_id)
        .await
        .map_err(|e| e.to_string())
}

// ── send_message ─────────────────────────────────────────────────────────

/// From a conversation's persisted rows, pick the `(message_id,
/// routing_decision)` to report to the frontend: the most recent
/// `assistant` row's real gate decision (what `process_message` stamped —
/// "allow" / "route_local"), defaulting to `"allow"` only when that row
/// carries no decision. Returns `None` when there is no assistant row yet,
/// leaving the caller to mint a fallback id.
///
/// Extracted as a pure function so this stays covered by a unit test — a
/// refactor that silently re-hardcoded the decision back to `"allow"` (the
/// bug this replaced) would fail the test instead of shipping green.
fn latest_assistant_routing(rows: &[Message]) -> Option<(String, String)> {
    rows.iter().rev().find(|m| m.role == "assistant").map(|m| {
        (
            m.id.clone(),
            m.routing_decision
                .clone()
                .unwrap_or_else(|| "allow".to_string()),
        )
    })
}

/// Process a user message end-to-end: gate → model → stream tokens →
/// persist transcript. The frontend already renders `stream:token` events
/// for the in-flight response; this command returns the assembled text
/// once the stream finishes (the frontend can ignore the return value
/// if it has been live-appending).
#[tauri::command]
pub async fn send_message(
    app: AppHandle,
    state: State<'_, AppState>,
    args: SendMessageArgs,
) -> Result<SendMessageResponse, String> {
    // B3: enforce the profile-name allowlist at the IPC boundary too — this is
    // the one command that uses `args.profile` (for routing/labels) before it
    // reaches `open_profile`, so a padded/confusable name must be rejected here.
    crate::storage::validate_profile_name(&args.profile).map_err(|e| e.to_string())?;
    let binding = parse_binding(&args.binding)
        .map_err(|e| format!("invalid binding {:?}: {e}", args.binding))?;
    let session_mode = args
        .mode
        .as_deref()
        .map(crate::hooks::SessionMode::from_str_lenient)
        .unwrap_or_default();

    let profile = args.profile.clone();
    let started = chrono::Utc::now().timestamp_millis();
    let conversation_id = args.conversation_id.clone();

    // Wave 4.3c: the loop streams through the `ResultSink` trait rather than
    // a bare `AppHandle` — this is the one place that builds the production
    // (Tauri-backed) sink from the handle Tauri injected into this command.
    let sink: Arc<dyn ResultSink> = Arc::new(TauriResultSink::new(app));

    // The agent loop's internal stream_lock serializes concurrent
    // process_message calls — see `AgentLoop::new` / `process_message`.
    let content = state
        .agent_loop
        .process_message(
            args.content,
            args.conversation_id,
            binding,
            args.provider_id,
            args.model,
            args.profile,
            session_mode,
            &sink,
        )
        .await
        .map_err(|e| e.to_string())?;

    // Look up the assistant message we just persisted. We re-query the
    // profile db (read-only) and pick the most recent assistant row —
    // this gives us both the message id AND the real routing decision
    // that process_message stamped on it (one of "allow" / "route_local"),
    // so the frontend's RoutingBadge is honest on a live send instead of
    // only after a reload.
    let rows = state
        .storage
        .open_profile(&profile)
        .ok()
        .and_then(|db| db.list_messages_by_conversation(&conversation_id).ok())
        .unwrap_or_default();
    let (message_id, routing_decision) = latest_assistant_routing(&rows)
        .unwrap_or_else(|| (Uuid::new_v4().to_string(), "allow".to_string()));

    Ok(SendMessageResponse {
        message_id,
        content,
        conversation_id,
        profile,
        routing_decision,
        completed_at: chrono::Utc::now().timestamp_millis().max(started),
    })
}

// ── tool approval ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct ResolveApprovalArgs {
    pub id: String,
    /// "approve" | "deny". Anything that isn't "approve" is treated as a
    /// denial (fail closed).
    pub decision: String,
    /// "once" | "session" | "always" — how long the approval lasts. Defaults
    /// to "once". Ignored for a denial.
    #[serde(default)]
    pub scope: Option<String>,
    /// "action" (pin to this exact call) | "tool" (any call to this tool).
    /// Defaults to "action", the safer/narrower grant.
    #[serde(default)]
    pub target: Option<String>,
    /// For scope="always": the glob pattern of the persisted `tool_rules` row
    /// (`"*"` = whole tool). Defaults to `"*"`. Ignored for once/session/deny.
    /// Untrusted client input — the dispatcher still enforces the grant×risk
    /// matrix on the resulting rule (only `Write` persists), so a crafted
    /// pattern for a `Dangerous`/`External` tool cannot buy standing coverage.
    #[serde(default)]
    pub pattern: Option<String>,
}

/// Answer a pending tool-approval prompt raised via `tool:approval_request`.
/// Returns `false` if the id is unknown — already answered, or it timed out
/// and denied by default. Touches only the approval registry (never the agent
/// loop's stream lock), so it can safely resolve a dispatch that is parked
/// waiting on it.
#[tauri::command]
pub fn resolve_tool_approval(
    state: State<'_, AppState>,
    args: ResolveApprovalArgs,
) -> Result<bool, String> {
    let approve = args.decision.eq_ignore_ascii_case("approve");
    let scope_str = args.scope.as_deref().map(|s| s.to_ascii_lowercase());
    // "always" is a DURABLE grant — it becomes a persisted `tool_rules` row
    // (ApprovalDecision::Persist), not a ledger scope. Only "once"/"session"
    // reach the ledger here.
    let is_always = scope_str.as_deref() == Some("always");
    let scope = match scope_str.as_deref() {
        Some("session") => GrantScope::Session,
        _ => GrantScope::Once,
    };
    // A one-time grant is per-action, never whole-tool: force `action` for
    // `Once` so a "just this once" answer can't widen to every call of the
    // tool (defense in depth with `ApprovalLedger::grant`).
    let want_tool = !matches!(scope, GrantScope::Once)
        && matches!(args.target.as_deref(), Some(t) if t.eq_ignore_ascii_case("tool"));
    let pattern = args.pattern.clone().unwrap_or_else(|| "*".to_string());

    let answered = state.approvals.answer(&args.id, |fingerprint, tool_name| {
        if !approve {
            return ApprovalDecision::Deny;
        }
        if is_always {
            // Durable "Always allow" → a rule the dispatcher persists (and
            // enforces the matrix on: only Write persists, others run once).
            return ApprovalDecision::Persist(ToolRule::new(
                tool_name,
                pattern,
                PermissionMode::Allow,
            ));
        }
        let target = if want_tool {
            GrantTarget::Tool(tool_name.to_string())
        } else {
            GrantTarget::Fingerprint(fingerprint.to_string())
        };
        ApprovalDecision::Approve(scope, target)
    });
    Ok(answered)
}

/// Args for `resolve_ask_human`: the request id and the user's answer. A
/// `None`/absent `answer` = declined (the tool reports "not answered").
#[derive(Debug, Clone, Deserialize)]
pub struct ResolveAskHumanArgs {
    pub id: String,
    #[serde(default)]
    pub answer: Option<String>,
}

/// A profile's model-usage roll-up (Wave 3.2 ledger), for the Settings "Usage"
/// view. Mirrors `storage::UsageSummary`. `known_cost_usd` sums only priced
/// calls (local $0 + priced cloud); `unknown_cost_calls` is the honest count of
/// cloud calls we couldn't price ("flying blind").
#[derive(Debug, Clone, Serialize)]
pub struct UsageSummaryInfo {
    pub total_calls: usize,
    pub known_cost_usd: f64,
    pub unknown_cost_calls: usize,
}

impl From<crate::storage::UsageSummary> for UsageSummaryInfo {
    fn from(s: crate::storage::UsageSummary) -> Self {
        Self {
            total_calls: s.total_calls,
            known_cost_usd: s.known_cost_usd,
            unknown_cost_calls: s.unknown_cost_calls,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GetUsageSummaryArgs {
    pub profile: String,
}

/// A saved skill for the Settings "Skills" view. Mirrors `storage::Skill` (the
/// body is included so the user can review what a skill actually does).
#[derive(Debug, Clone, Serialize)]
pub struct SkillInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub content: String,
    pub capabilities_required: Vec<String>,
    /// "pending" | "approved" | "rejected".
    pub approval_status: String,
    pub version: String,
    pub created_at: i64,
}

impl From<crate::storage::Skill> for SkillInfo {
    fn from(s: crate::storage::Skill) -> Self {
        Self {
            id: s.id,
            name: s.name,
            description: s.description,
            content: s.content,
            capabilities_required: s.capabilities_required,
            approval_status: s.approval_status.as_str().to_string(),
            version: s.version,
            created_at: s.created_at,
        }
    }
}

/// List every saved skill (all statuses) for the Settings "Skills" management +
/// review view. Skills are global (not profile-scoped), so no profile arg.
#[tauri::command]
pub fn list_skills(state: State<'_, AppState>) -> Result<Vec<SkillInfo>, String> {
    state
        .storage
        .global()
        .list_skills()
        .map(|v| v.into_iter().map(Into::into).collect())
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillApprovalArgs {
    pub id: String,
    /// "approved" | "rejected" | "pending".
    pub status: String,
}

/// Set a skill's trust state (the review gate) — the Settings "Skills" pane's
/// approve/reject. An unknown status string fails closed to `pending`.
#[tauri::command]
pub fn set_skill_approval(
    state: State<'_, AppState>,
    args: SkillApprovalArgs,
) -> Result<bool, String> {
    let status = crate::storage::SkillApproval::from_str(&args.status);
    let changed = state
        .storage
        .global()
        .set_skill_approval(&args.id, status)
        .map_err(|e| e.to_string())?;
    // C4: keep the live tool registry in lock-step with the approval state.
    // Approved → hot-register the skill as a callable Tool; anything else →
    // unregister it. Best-effort AFTER the storage write (the DB is the source
    // of truth; SkillTool::run re-checks it at call time anyway).
    if changed {
        if let Ok(Some(skill)) = state.storage.global().get_skill(&args.id) {
            let tool_name = crate::tools::skills::skill_tool_name(&skill.name);
            match skill.approval_status {
                crate::storage::SkillApproval::Approved => {
                    if let Some(tool) =
                        crate::tools::skills::SkillTool::for_skill(&skill, Arc::clone(&state.storage))
                    {
                        state.tools.hot_register(Box::new(tool));
                    }
                }
                _ => {
                    state.tools.hot_unregister(&tool_name);
                }
            }
        }
    }
    Ok(changed)
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeleteSkillArgs {
    pub id: String,
}

/// Delete a saved skill (two-click confirm in the UI). Returns whether a row was removed.
#[tauri::command]
pub fn delete_skill(state: State<'_, AppState>, args: DeleteSkillArgs) -> Result<bool, String> {
    // C4: unregister the live Tool BEFORE the row goes away ("never a live Tool
    // serving a deleted skill" — the remove_local_model stop-before-delete
    // precedent). Needs the name to compute the tool id, so read first.
    if let Ok(Some(skill)) = state.storage.global().get_skill(&args.id) {
        state
            .tools
            .hot_unregister(&crate::tools::skills::skill_tool_name(&skill.name));
    }
    state
        .storage
        .global()
        .delete_skill(&args.id)
        .map_err(|e| e.to_string())
}

/// Probe this machine's hardware for M8 (RAM, cores, OS/arch, + Probe-v2:
/// bandwidth/GPU/unified-memory). Serves the boot-time cached snapshot (A4) —
/// `probe()` shells out to `system_profiler`, so we never re-probe per call.
#[tauri::command]
pub fn probe_hardware(state: State<'_, AppState>) -> crate::models::hardware::HardwareProfile {
    (*state.hardware).clone()
}

/// The curated model catalog, each entry annotated with its fit against the
/// cached hardware profile (Wave 5.3 / M8). Works offline (bundled catalog).
#[tauri::command]
pub fn list_model_catalog(
    state: State<'_, AppState>,
) -> Vec<crate::models::catalog::CatalogEntryView> {
    crate::models::catalog::catalog_for(&state.hardware)
}

// ── M8 S2′/S3′ — HF search + interactive calculator IPC (A3) ──────────────

/// Args for [`search_models`]. `sort` ∈ {downloads,likes,trending,last_modified}
/// (default downloads); an empty `query` returns the trusted-publisher
/// Staff-picks default.
#[derive(Debug, Deserialize)]
pub struct SearchModelsArgs {
    pub query: String,
    #[serde(default)]
    pub sort: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Search HuggingFace for GGUF models (M8 S2′). An empty query returns the
/// trusted-publisher Staff-picks default; a non-empty query searches live and
/// surfaces community results, each carrying its provenance label. Networked.
#[tauri::command]
pub async fn search_models(
    args: SearchModelsArgs,
) -> Result<Vec<crate::models::hf_search::HfModelSummary>, String> {
    use crate::models::hf_search::{search, staff_picks, SearchSort};
    let limit = args.limit.unwrap_or(25);
    let q = args.query.trim();
    let result = if q.is_empty() {
        staff_picks(limit).await
    } else {
        let sort = match args.sort.as_deref() {
            Some("likes") => SearchSort::Likes,
            Some("trending") => SearchSort::Trending,
            Some("last_modified") => SearchSort::LastModified,
            _ => SearchSort::Downloads,
        };
        search(q, sort, limit).await
    };
    result.map_err(|e| e.to_string())
}

/// The detail view for one model (M8 S2′): its quants (grouped, multi-part
/// aware, with sizes + provenance) PLUS a one-time [`ModelSpec`] read. All quants
/// of a model share the same architecture geometry, so a single header read
/// serves the interactive calculator for every quant. `spec: None` when the
/// architecture couldn't be read — the UI shows the discovery view but can't run
/// the calculator (honest, never a fabricated spec).
#[derive(Debug, Clone, Serialize)]
pub struct ModelDetailResponse {
    #[serde(flatten)]
    pub detail: crate::models::hf_search::HfModelDetail,
    pub spec: Option<crate::models::calculator::ModelSpec>,
    pub spec_notes: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct GetModelDetailArgs {
    pub model_id: String,
}

/// Fetch a model's quants + a representative [`ModelSpec`] (M8 S2′). Networked.
#[tauri::command]
pub async fn get_model_detail(args: GetModelDetailArgs) -> Result<ModelDetailResponse, String> {
    let detail = crate::models::hf_search::model_detail(&args.model_id)
        .await
        .map_err(|e| e.to_string())?;
    // All quants share the architecture geometry — read the spec once from a
    // representative complete quant's first file.
    let repr_url = detail
        .quants
        .iter()
        .find(|q| q.complete)
        .and_then(|q| q.files.first())
        .map(|f| f.url.clone());
    let (spec, spec_notes) = match repr_url {
        Some(url) => match crate::models::gguf_meta::read_model_spec(&args.model_id, &url).await {
            Ok((s, notes)) => (Some(s), notes),
            Err(e) => (None, vec![format!("Couldn't read model architecture: {e}")]),
        },
        None => (None, vec!["No downloadable quant found for this model.".to_string()]),
    };
    Ok(ModelDetailResponse { detail, spec, spec_notes })
}

/// Args for [`calculate_model_fit`] — the model's architecture spec (from
/// [`get_model_detail`]) and the user's chosen knobs (weight-file size, KV-cache
/// quant, context length).
#[derive(Debug, Deserialize)]
pub struct CalculateModelFitArgs {
    pub model_spec: crate::models::calculator::ModelSpec,
    pub calc_input: crate::models::calculator::CalcInput,
}

/// The interactive calculator (M8 S3′): cached-probe × ModelSpec × CalcInput →
/// CalcOutput. PURE + instant (no I/O) — reads the cached hardware snapshot so
/// the UI recomputes on every slider drag without re-probing or re-fetching.
#[tauri::command]
pub fn calculate_model_fit(
    state: State<'_, AppState>,
    args: CalculateModelFitArgs,
) -> crate::models::calculator::CalcOutput {
    crate::models::calculator::calculate(&state.hardware, &args.model_spec, &args.calc_input)
}

// ── sandbox_config (B2 — M7 Tier-K network ceiling reachable) ──────────────
// Nothing wrote a `sandbox_config` row before this, so `ShellExecTool`'s
// per-profile network ceiling took the "unconfigured → unconstrained" branch
// for every real profile. These commands are the writer surface.

#[derive(Debug, Clone, Deserialize)]
pub struct GetSandboxConfigArgs {
    pub profile: String,
}

/// This profile's stored sandbox config, or the default when UNSET. A CORRUPT
/// stored row propagates as an `Err` (never coerced to a default) — the
/// storage layer fails closed and so must this command, so a garbled row can't
/// silently loosen the shell's network ceiling.
#[tauri::command]
pub fn get_sandbox_config(
    state: State<'_, AppState>,
    args: GetSandboxConfigArgs,
) -> Result<crate::hooks::SandboxConfig, String> {
    crate::storage::validate_profile_name(&args.profile).map_err(|e| e.to_string())?;
    let db = state.storage.open_profile(&args.profile).map_err(|e| e.to_string())?;
    // `?` propagates a corrupt-row Err; `unwrap_or_default` only fills the
    // UNSET (None) case with the library default.
    Ok(db.get_sandbox_config().map_err(|e| e.to_string())?.unwrap_or_default())
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetSandboxConfigArgs {
    pub profile: String,
    pub config: crate::hooks::SandboxConfig,
}

/// Persist this profile's sandbox config. Validates before writing so a
/// self-contradictory / empty-entry shape never reaches the JSON blob the shell
/// path trusts. Echoes the validated config now in effect.
#[tauri::command]
pub fn set_sandbox_config(
    state: State<'_, AppState>,
    args: SetSandboxConfigArgs,
) -> Result<crate::hooks::SandboxConfig, String> {
    crate::storage::validate_profile_name(&args.profile).map_err(|e| e.to_string())?;
    validate_sandbox_config(&args.config)?;
    let db = state.storage.open_profile(&args.profile).map_err(|e| e.to_string())?;
    db.set_sandbox_config(&args.config).map_err(|e| e.to_string())?;
    Ok(args.config)
}

/// Reject shapes that would be silently mis-stored: empty allowlist / exclusion
/// entries (a blank domain/socket/command is never meaningful and would just be
/// dead weight the shell path has to skip). Fail closed on bad input.
fn validate_sandbox_config(cfg: &crate::hooks::SandboxConfig) -> Result<(), String> {
    if cfg.network.allowed_domains.iter().any(|d| d.trim().is_empty()) {
        return Err("sandbox_config: allowed_domains entries must not be empty".into());
    }
    if cfg.network.allow_unix_sockets.iter().any(|s| s.trim().is_empty()) {
        return Err("sandbox_config: allow_unix_sockets entries must not be empty".into());
    }
    if cfg.excluded_commands.iter().any(|c| c.trim().is_empty()) {
        return Err("sandbox_config: excluded_commands entries must not be empty".into());
    }
    Ok(())
}

// ── budget_settings (C1 — the spend governor's per-profile cap) ────────────

/// This profile's spend cap. `cap_usd = null` ⇒ uncapped.
#[derive(Debug, Clone, Serialize)]
pub struct BudgetSettings {
    pub cap_usd: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GetBudgetSettingsArgs {
    pub profile: String,
}

#[tauri::command]
pub fn get_budget_settings(
    state: State<'_, AppState>,
    args: GetBudgetSettingsArgs,
) -> Result<BudgetSettings, String> {
    crate::storage::validate_profile_name(&args.profile).map_err(|e| e.to_string())?;
    let db = state.storage.open_profile(&args.profile).map_err(|e| e.to_string())?;
    Ok(BudgetSettings { cap_usd: db.budget_cap().map_err(|e| e.to_string())? })
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetBudgetSettingsArgs {
    pub profile: String,
    /// The cap in USD; `null` clears it (uncapped).
    #[serde(default)]
    pub cap_usd: Option<f64>,
}

/// Set (or clear, with `cap_usd: null`) this profile's spend cap. Echoes the
/// value now in effect.
#[tauri::command]
pub fn set_budget_settings(
    state: State<'_, AppState>,
    args: SetBudgetSettingsArgs,
) -> Result<BudgetSettings, String> {
    crate::storage::validate_profile_name(&args.profile).map_err(|e| e.to_string())?;
    let db = state.storage.open_profile(&args.profile).map_err(|e| e.to_string())?;
    db.set_budget_cap(args.cap_usd).map_err(|e| e.to_string())?;
    Ok(BudgetSettings { cap_usd: db.budget_cap().map_err(|e| e.to_string())? })
}

/// Clear the cap entirely (uncapped).
#[tauri::command]
pub fn reset_budget_settings(
    state: State<'_, AppState>,
    args: GetBudgetSettingsArgs,
) -> Result<BudgetSettings, String> {
    crate::storage::validate_profile_name(&args.profile).map_err(|e| e.to_string())?;
    let db = state.storage.open_profile(&args.profile).map_err(|e| e.to_string())?;
    db.reset_budget_cap().map_err(|e| e.to_string())?;
    Ok(BudgetSettings { cap_usd: None })
}

// ── MCP servers (C3 — registration/list/remove over the stdio transport) ────

#[derive(Debug, Clone, Deserialize)]
pub struct RegisterMcpServerArgs {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// "local" | "remote" (anything else fails closed to remote).
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub trusted_read_only: bool,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// What `register_mcp_server` / `list_mcp_servers` return per server.
#[derive(Debug, Clone, Serialize)]
pub struct McpServerInfo {
    pub id: String,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub tier: String,
    pub trusted_read_only: bool,
    pub enabled: bool,
    /// Whether a live child is currently running for this server.
    pub running: bool,
    /// The namespaced tool names currently registered (empty when not running).
    pub tools: Vec<String>,
}

/// Register an MCP server: SPAWN + handshake + `tools/list` FIRST (fail-closed
/// — a server that can't come up is never persisted), then persist the config
/// and hot-register its tools through the untouched trust spine. Returns the
/// server + its namespaced tools.
///
/// SECURITY / UI CONTRACT: registration installs an unsandboxed executable
/// that runs with the user's OS privileges and automatically restarts at app
/// boot while enabled. Any UI exposing this command must present it as software
/// installation and require explicit native confirmation before invoking it.
#[tauri::command]
pub async fn register_mcp_server(
    state: State<'_, AppState>,
    args: RegisterMcpServerArgs,
) -> Result<McpServerInfo, String> {
    if args.name.trim().is_empty() || args.command.trim().is_empty() {
        return Err("an MCP server needs a non-empty name and command".to_string());
    }
    // Review fix (#2): reject a sanitized-NAMESPACE collision with an existing
    // server. The `mcp__{server}__{tool}` separator is collision-free per
    // (server, tool) — so the only cross-server collision domain is the server
    // segment itself; letting two servers share it would let the EARLIER one
    // silently pre-claim (and answer for) the later one's tool names.
    let new_seg = crate::tools::mcp::sanitize_name_segment(args.name.trim());
    let existing = state.storage.global().list_mcp_servers().map_err(|e| e.to_string())?;
    if let Some(clash) = existing
        .iter()
        .find(|r| crate::tools::mcp::sanitize_name_segment(&r.name) == new_seg)
    {
        return Err(format!(
            "an MCP server named \"{}\" already occupies the \"{new_seg}\" tool namespace — \
             remove it first or pick a distinct name",
            clash.name
        ));
    }
    // Review fix (#7): unknown capability strings fail CLOSED — a typo must not
    // silently shrink a tool's requirements (widening its availability).
    for c in &args.capabilities {
        if crate::tools::Capability::from_capability_str(c).is_none() {
            return Err(format!(
                "unknown capability \"{c}\" — valid names match the skill capability list"
            ));
        }
    }
    let row = crate::storage::McpServerRow {
        id: uuid::Uuid::new_v4().to_string(),
        name: args.name.trim().to_string(),
        command: args.command.trim().to_string(),
        args: args.args,
        tier: match args.tier.as_deref() {
            Some("local") => "local".to_string(),
            _ => "remote".to_string(), // ambiguous ⇒ remote (the stricter tier)
        },
        trusted_read_only: args.trusted_read_only,
        capabilities: args.capabilities,
        enabled: true,
        created_at: chrono::Utc::now().timestamp(),
    };
    // Fail-closed ordering: bring the server up BEFORE persisting anything.
    let tools = crate::tools::mcp_stdio::bring_up_server(&row, &state.tools, &state.mcp).await?;
    if let Err(e) = state.storage.global().insert_mcp_server(&row) {
        // Persist failed → tear the live half back down; never a half-state.
        crate::tools::mcp_stdio::tear_down_server(&row.id, &state.tools, &state.mcp).await;
        return Err(format!("couldn't persist the MCP server config: {e}"));
    }
    Ok(McpServerInfo {
        id: row.id,
        name: row.name,
        command: row.command,
        args: row.args,
        tier: row.tier,
        trusted_read_only: row.trusted_read_only,
        enabled: row.enabled,
        running: true,
        tools,
    })
}

/// The persisted MCP servers, annotated with live status.
#[tauri::command]
pub fn list_mcp_servers(state: State<'_, AppState>) -> Result<Vec<McpServerInfo>, String> {
    let rows = state.storage.global().list_mcp_servers().map_err(|e| e.to_string())?;
    let live = state.mcp.servers.lock();
    Ok(rows
        .into_iter()
        .map(|r| {
            let entry = live.get(&r.id);
            McpServerInfo {
                running: entry.is_some(),
                tools: entry.map(|e| e.tool_names.clone()).unwrap_or_default(),
                id: r.id,
                name: r.name,
                command: r.command,
                args: r.args,
                tier: r.tier,
                trusted_read_only: r.trusted_read_only,
                enabled: r.enabled,
            }
        })
        .collect())
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoveMcpServerArgs {
    pub id: String,
}

/// Remove an MCP server: unregister its tools + kill its child BEFORE deleting
/// the row ("never a live Tool serving a deleted config" — the delete_skill /
/// remove_local_model precedent). Returns whether a row was removed.
#[tauri::command]
pub async fn remove_mcp_server(
    state: State<'_, AppState>,
    args: RemoveMcpServerArgs,
) -> Result<bool, String> {
    crate::tools::mcp_stdio::tear_down_server(&args.id, &state.tools, &state.mcp).await;
    state.storage.global().delete_mcp_server(&args.id).map_err(|e| e.to_string())
}

// ── cancel_message (C7 — M6 Slice 4a: cooperative turn cancellation) ────────

#[derive(Debug, Clone, Deserialize)]
pub struct CancelMessageArgs {
    pub conversation_id: String,
}

/// Flip the in-flight cancellation token for a conversation's current streaming
/// turn, if one exists (the SSE drain loop then breaks cooperatively and the
/// turn persists `aborted: true`). Touches ONLY the agent loop's cancellation
/// registry — never `stream_lock` — so it can't block behind or deadlock the
/// in-flight `process_message` it interrupts. Returns `false` when nothing was
/// in flight (already finished / never started) — not an error, a client racing
/// a fast reply is expected.
#[tauri::command]
pub fn cancel_message(state: State<'_, AppState>, args: CancelMessageArgs) -> Result<bool, String> {
    Ok(state.agent_loop.cancel_conversation(&args.conversation_id))
}

/// A downloaded/registered local model for the Settings model-manager (M8 S6).
#[derive(Debug, Clone, Serialize)]
pub struct LocalModelInfo {
    pub id: String,
    pub name: String,
    pub path: String,
    pub size_bytes: i64,
    /// "ready" | "quarantined".
    pub status: String,
}

impl From<crate::storage::ModelEntry> for LocalModelInfo {
    fn from(m: crate::storage::ModelEntry) -> Self {
        Self { id: m.id, name: m.name, path: m.path, size_bytes: m.size_bytes, status: m.status }
    }
}

/// List downloaded local models (M8 S6). Global.
#[tauri::command]
pub fn list_local_models(state: State<'_, AppState>) -> Result<Vec<LocalModelInfo>, String> {
    state
        .storage
        .global()
        .list_models()
        .map(|v| v.into_iter().map(Into::into).collect())
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoveModelArgs {
    pub id: String,
}

/// Delete a downloaded model — its file AND its `model_catalog` row (M8 S6). A
/// seat that pointed at it falls back to `inherit` automatically at resolve time
/// (resolve_seat inherits when the provider is gone), so no dangling reference.
/// M8 S4: the model's sidecar (if running) is stopped BEFORE the file is
/// deleted, and its derived provider is unregistered — never a runner serving
/// a deleted file.
#[tauri::command]
pub async fn remove_local_model(
    state: State<'_, AppState>,
    args: RemoveModelArgs,
) -> Result<bool, String> {
    #[cfg(feature = "local-runner")]
    {
        if let Some(ctx) = &state.local_runner {
            ctx.supervisor.stop(&args.id).await;
        }
        state
            .model_manager
            .remove_provider(&crate::models::runner::provider_id_for(&args.id));
    }
    let global = state.storage.global();
    if let Ok(Some(m)) = global.get_model(&args.id) {
        // Best-effort file delete (the row removal is the source of truth).
        let _ = std::fs::remove_file(&m.path);
    }
    global.delete_model(&args.id).map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Deserialize)]
pub struct DownloadModelArgs {
    /// The catalog entry id to download.
    pub id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DownloadedModelInfo {
    pub id: String,
    pub name: String,
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize)]
struct DownloadProgress {
    id: String,
    downloaded: u64,
    total: u64,
}

/// Download + verify a curated catalog model (Wave 5.3 / M8). Streams progress
/// via the `model:download-progress` event; on success the VERIFIED file is
/// registered in `model_catalog` (status `ready`). A digest mismatch or an
/// off-allowlist / uncurated entry installs NOTHING and errors (the
/// verified-before-runnable invariant). The model becomes a runnable provider
/// once a runner points at it (external runner today; the bundled sidecar is S4).
#[tauri::command]
pub async fn download_model(
    state: State<'_, AppState>,
    app: AppHandle,
    args: DownloadModelArgs,
) -> Result<DownloadedModelInfo, String> {
    let entry = crate::models::catalog::bundled_catalog()
        .into_iter()
        .find(|e| e.id == args.id)
        .ok_or_else(|| format!("no catalog model \"{}\"", args.id))?;
    if !entry.is_curated() {
        return Err(
            "this model isn't release-curated yet (no verified hash) — can't download safely"
                .to_string(),
        );
    }

    let dir = state.storage.base_path().join("models").join("downloaded");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let final_path = dir.join(format!("{}.gguf", entry.id));
    let partial = dir.join(format!("{}.gguf.partial", entry.id));

    // Stream, emitting progress (throttling is the frontend's job).
    let id_for_progress = entry.id.clone();
    let app_for_progress = app.clone();
    crate::models::download::download_to_partial(&entry.url, &partial, move |downloaded, total| {
        let _ = app_for_progress.emit(
            "model:download-progress",
            DownloadProgress { id: id_for_progress.clone(), downloaded, total },
        );
    })
    .await
    .map_err(|e| e.to_string())?;

    // Verify-or-nothing: a mismatch removes the partial + registers nothing.
    crate::models::download::verify_and_install(&partial, &final_path, &entry.sha256)
        .map_err(|e| e.to_string())?;

    let model = crate::storage::ModelEntry {
        id: entry.id.clone(),
        name: entry.name.clone(),
        path: final_path.to_string_lossy().to_string(),
        size_bytes: entry.size_bytes as i64,
        quantization: Some(entry.quantization.clone()),
        added_at: chrono::Utc::now().timestamp(),
        sha256: entry.sha256.clone(),
        status: "ready".to_string(),
    };
    state.storage.global().insert_model(&model).map_err(|e| e.to_string())?;

    Ok(DownloadedModelInfo {
        id: model.id,
        name: model.name,
        path: model.path,
        sha256: model.sha256,
    })
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstallPackArgs {
    pub profile: String,
    /// The pack, as JSON (pasted or loaded from a file by the UI).
    pub json: String,
}

/// Install a Capability Pack (Wave 4.5): register its skills + agent types
/// (GLOBAL) + cron jobs (this profile) at once. Everything lands INERT — skills
/// + agent types `Pending` (review in Settings → Skills / Agent types), cron
/// jobs disabled — so a pack adds capabilities to review, never arms one.
#[tauri::command]
pub fn install_pack(
    state: State<'_, AppState>,
    args: InstallPackArgs,
) -> Result<crate::packs::InstallReport, String> {
    let pack = crate::packs::parse_pack(&args.json).map_err(|e| e.to_string())?;
    crate::packs::install_pack(
        &state.storage,
        &args.profile,
        &pack,
        chrono::Utc::now().timestamp(),
    )
    .map_err(|e| e.to_string())
}

/// A declarative agent-type persona for the Settings view (Wave 4.3).
#[derive(Debug, Clone, Serialize)]
pub struct AgentTypeInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub tools_allowlist: Vec<String>,
    pub seat: String,
    /// "pending" | "approved" | "rejected".
    pub approval_status: String,
    /// "builtin" | "user" | pack id.
    pub source: String,
    pub created_at: i64,
}

impl From<crate::storage::AgentType> for AgentTypeInfo {
    fn from(a: crate::storage::AgentType) -> Self {
        Self {
            id: a.id,
            name: a.name,
            description: a.description,
            tools_allowlist: a.tools_allowlist,
            seat: a.seat,
            approval_status: a.approval_status.as_str().to_string(),
            source: a.source,
            created_at: a.created_at,
        }
    }
}

/// List every agent-type persona (all statuses). Agent types are global.
#[tauri::command]
pub fn list_agent_types(state: State<'_, AppState>) -> Result<Vec<AgentTypeInfo>, String> {
    state
        .storage
        .global()
        .list_agent_types()
        .map(|v| v.into_iter().map(Into::into).collect())
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentTypeApprovalArgs {
    pub id: String,
    /// "approved" | "rejected" | "pending".
    pub status: String,
}

/// Set an agent type's trust state — the review gate. Only `Approved` types are
/// dispatchable by `delegate`. An unknown status fails closed to `pending`.
#[tauri::command]
pub fn set_agent_type_approval(
    state: State<'_, AppState>,
    args: AgentTypeApprovalArgs,
) -> Result<bool, String> {
    let status = crate::storage::AgentTypeApproval::from_str(&args.status);
    state
        .storage
        .global()
        .set_agent_type_approval(&args.id, status)
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeleteAgentTypeArgs {
    pub id: String,
}

/// Delete an agent-type persona (two-click confirm in the UI).
#[tauri::command]
pub fn delete_agent_type(
    state: State<'_, AppState>,
    args: DeleteAgentTypeArgs,
) -> Result<bool, String> {
    state
        .storage
        .global()
        .delete_agent_type(&args.id)
        .map_err(|e| e.to_string())
}

/// A per-profile model-seat binding for the Settings → Models "Seats" view.
#[derive(Debug, Clone, Serialize)]
pub struct SeatBindingInfo {
    pub seat: String,
    pub provider_id: String,
    pub model: String,
    pub updated_at: i64,
}

impl From<crate::storage::SeatBinding> for SeatBindingInfo {
    fn from(b: crate::storage::SeatBinding) -> Self {
        Self {
            seat: b.seat,
            provider_id: b.provider_id,
            model: b.model,
            updated_at: b.updated_at,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListSeatsArgs {
    pub profile: String,
}

/// List a profile's model-seat bindings (Wave 3.1). Seats are per-profile.
#[tauri::command]
pub fn list_seat_bindings(
    state: State<'_, AppState>,
    args: ListSeatsArgs,
) -> Result<Vec<SeatBindingInfo>, String> {
    state
        .storage
        .open_profile(&args.profile)
        .and_then(|db| db.list_seat_bindings())
        .map(|v| v.into_iter().map(Into::into).collect())
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetSeatArgs {
    pub profile: String,
    pub seat: String,
    pub provider_id: String,
    pub model: String,
}

/// Bind a (user-defined) seat name to a provider+model for this profile.
#[tauri::command]
pub fn set_seat_binding(state: State<'_, AppState>, args: SetSeatArgs) -> Result<(), String> {
    let seat = args.seat.trim();
    if seat.is_empty() {
        return Err("seat name must not be empty".to_string());
    }
    if seat.eq_ignore_ascii_case("inherit") {
        return Err("\"inherit\" is reserved (it means \"use the caller's model\")".to_string());
    }
    if args.provider_id.trim().is_empty() || args.model.trim().is_empty() {
        return Err("a seat binding needs both a provider and a model".to_string());
    }
    state
        .storage
        .open_profile(&args.profile)
        .and_then(|db| db.set_seat_binding(seat, args.provider_id.trim(), args.model.trim()))
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeleteSeatArgs {
    pub profile: String,
    pub seat: String,
}

/// Unbind a seat (it then resolves to the caller's model). Returns whether a row went.
#[tauri::command]
pub fn delete_seat_binding(
    state: State<'_, AppState>,
    args: DeleteSeatArgs,
) -> Result<bool, String> {
    state
        .storage
        .open_profile(&args.profile)
        .and_then(|db| db.delete_seat_binding(&args.seat))
        .map_err(|e| e.to_string())
}

/// Whether autonomous skill drafting (Wave 4.2) is on. Global (skills are global).
#[tauri::command]
pub fn get_skill_reflect_enabled(state: State<'_, AppState>) -> Result<bool, String> {
    Ok(state.storage.global().skill_reflect_enabled())
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillReflectArgs {
    pub enabled: bool,
}

/// Turn autonomous skill drafting on/off. When on, a LOCAL model reviews a
/// finished conversation for a reusable procedure and drafts a **Pending** skill
/// (inert until the user approves it in the Skills pane).
#[tauri::command]
pub fn set_skill_reflect_enabled(
    state: State<'_, AppState>,
    args: SkillReflectArgs,
) -> Result<(), String> {
    state
        .storage
        .global()
        .set_skill_reflect_enabled(args.enabled)
        .map_err(|e| e.to_string())
}

/// Roll up a profile's model-call cost ledger for the Settings "Usage" view.
#[tauri::command]
pub fn get_usage_summary(
    state: State<'_, AppState>,
    args: GetUsageSummaryArgs,
) -> Result<UsageSummaryInfo, String> {
    let db = state
        .storage
        .open_profile(&args.profile)
        .map_err(|e| e.to_string())?;
    db.usage_summary().map(Into::into).map_err(|e| e.to_string())
}

/// Deliver the user's answer to a parked `ask_human` question. Touches only the
/// ask-human registry (never the stream lock), so it can't deadlock the
/// dispatch waiting on it. Returns whether the id was still awaiting an answer.
#[tauri::command]
pub fn resolve_ask_human(
    state: State<'_, AppState>,
    args: ResolveAskHumanArgs,
) -> Result<bool, String> {
    // Normalize an all-whitespace answer to a decline so an empty submit isn't
    // fed back to the model as a meaningful reply.
    let answer = args
        .answer
        .filter(|a| !a.trim().is_empty())
        .map(|a| a.trim().to_string());
    Ok(state.ask_human.answer(&args.id, answer))
}

// ── persisted tool rules (Q8) — list + revoke ─────────────────────────────

/// A persisted `tool_rules` row, for the Settings "Permissions" pane. Mirrors
/// `storage::ToolRuleRow`.
#[derive(Debug, Clone, Serialize)]
pub struct ToolRuleInfo {
    pub id: String,
    pub tool_name: String,
    pub pattern: String,
    pub action: String,
    pub created_at: i64,
}

impl From<crate::storage::ToolRuleRow> for ToolRuleInfo {
    fn from(r: crate::storage::ToolRuleRow) -> Self {
        Self {
            id: r.id,
            tool_name: r.tool_name,
            pattern: r.pattern,
            action: r.action,
            created_at: r.created_at,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListToolRulesArgs {
    pub profile: String,
}

/// List the persisted "Always allow" rules for a profile (newest first) so the
/// user can review/revoke them. Per-profile — a rule in one profile is
/// invisible to another.
#[tauri::command]
pub fn list_tool_rules(
    state: State<'_, AppState>,
    args: ListToolRulesArgs,
) -> Result<Vec<ToolRuleInfo>, String> {
    let db = state
        .storage
        .open_profile(&args.profile)
        .map_err(|e| e.to_string())?;
    db.list_tool_rules()
        .map(|rows| rows.into_iter().map(Into::into).collect())
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeleteToolRuleArgs {
    pub profile: String,
    pub id: String,
}

/// Revoke one persisted rule by id (takes effect immediately — `SqlitePolicySource`
/// reads live, so the next call re-prompts). Returns `true` if a row was removed.
#[tauri::command]
pub fn delete_tool_rule(
    state: State<'_, AppState>,
    args: DeleteToolRuleArgs,
) -> Result<bool, String> {
    let db = state
        .storage
        .open_profile(&args.profile)
        .map_err(|e| e.to_string())?;
    db.delete_tool_rule(&args.id).map_err(|e| e.to_string())
}

// ── classifier settings (PLAN §11 — per-profile strictness) ───────────────

/// The classifier tuning for one profile, in the UI's own vocabulary
/// (strictness 0–100, an uncertainty-band label) plus the raw thresholds those
/// map to (for display/debugging).
#[derive(Debug, Clone, Serialize)]
pub struct ClassifierSettingsInfo {
    /// Detection strictness, 0 (permissive) – 100 (paranoid).
    pub strictness: u8,
    /// "narrow" | "medium" | "wide".
    pub uncertainty_band: String,
    /// Raw fusion thresholds (the source of truth in storage).
    pub tau_block: f32,
    pub tau_band: f32,
    /// Whether partial-delegation redaction is enabled (PLAN §11).
    pub redaction_enabled: bool,
}

impl ClassifierSettingsInfo {
    fn from_parts(cfg: crate::classifier::ClassifierConfig, redaction_enabled: bool) -> Self {
        let (strictness, band) = cfg.to_ui();
        Self {
            strictness,
            uncertainty_band: band.to_string(),
            tau_block: cfg.tau_block,
            tau_band: cfg.tau_band,
            redaction_enabled,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GetClassifierSettingsArgs {
    pub profile: String,
}

/// The active classifier settings for a profile (defaults when unset).
#[tauri::command]
pub fn get_classifier_settings(
    state: State<'_, AppState>,
    args: GetClassifierSettingsArgs,
) -> Result<ClassifierSettingsInfo, String> {
    let db = state
        .storage
        .open_profile(&args.profile)
        .map_err(|e| e.to_string())?;
    let cfg = db.classifier_config().map_err(|e| e.to_string())?;
    let redaction = db.redaction_enabled().map_err(|e| e.to_string())?;
    Ok(ClassifierSettingsInfo::from_parts(cfg, redaction))
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetClassifierSettingsArgs {
    pub profile: String,
    /// 0–100 (clamped). Higher = more content kept local.
    pub strictness: u8,
    /// "narrow" | "medium" | "wide" (unknown → "medium").
    pub uncertainty_band: String,
}

/// Persist a profile's classifier tuning (thresholds only — the redaction
/// toggle is preserved). Takes effect on the next message (the gate loads the
/// config live per send). Returns the full stored settings.
#[tauri::command]
pub fn set_classifier_settings(
    state: State<'_, AppState>,
    args: SetClassifierSettingsArgs,
) -> Result<ClassifierSettingsInfo, String> {
    let db = state
        .storage
        .open_profile(&args.profile)
        .map_err(|e| e.to_string())?;
    let cfg =
        crate::classifier::ClassifierConfig::from_ui(args.strictness, &args.uncertainty_band);
    db.set_classifier_config(&cfg).map_err(|e| e.to_string())?;
    let redaction = db.redaction_enabled().map_err(|e| e.to_string())?;
    Ok(ClassifierSettingsInfo::from_parts(cfg, redaction))
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetRedactionEnabledArgs {
    pub profile: String,
    pub enabled: bool,
}

/// Toggle a profile's partial-delegation redaction (PLAN §11). Preserves the
/// profile's thresholds. Returns the full stored settings.
#[tauri::command]
pub fn set_redaction_enabled(
    state: State<'_, AppState>,
    args: SetRedactionEnabledArgs,
) -> Result<ClassifierSettingsInfo, String> {
    let db = state
        .storage
        .open_profile(&args.profile)
        .map_err(|e| e.to_string())?;
    db.set_redaction_enabled(args.enabled)
        .map_err(|e| e.to_string())?;
    let cfg = db.classifier_config().map_err(|e| e.to_string())?;
    Ok(ClassifierSettingsInfo::from_parts(cfg, args.enabled))
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResetClassifierSettingsArgs {
    pub profile: String,
}

/// Revert a profile's classifier settings to defaults (thresholds AND the
/// redaction toggle). Returns the (default) settings now in effect.
#[tauri::command]
pub fn reset_classifier_settings(
    state: State<'_, AppState>,
    args: ResetClassifierSettingsArgs,
) -> Result<ClassifierSettingsInfo, String> {
    let db = state
        .storage
        .open_profile(&args.profile)
        .map_err(|e| e.to_string())?;
    db.reset_classifier_config().map_err(|e| e.to_string())?;
    Ok(ClassifierSettingsInfo::from_parts(
        crate::classifier::ClassifierConfig::default(),
        true,
    ))
}

// ── classification explainability (PLAN §11 — the "why" sidebar) ──────────

/// One detected sensitive span, for the annotated-review sidebar. Char offsets
/// (not byte) so the frontend can slice the JS string directly.
#[derive(Debug, Clone, Serialize)]
pub struct ClassificationSpan {
    /// Char offset of the span start in the original text (inclusive).
    pub start: usize,
    /// Char offset of the span end (exclusive).
    pub end: usize,
    /// The exact matched text.
    pub text: String,
    /// Machine category, shared vocabulary with the classifier/audit log
    /// (e.g. "PII_CONTACT", "PROPRIETARY").
    pub category: String,
    /// Human-friendly category label for the legend (e.g. "contact info").
    pub label: String,
    /// The specific rule that fired (e.g. "email", "luhn_card").
    pub rule: String,
    /// Which layer caught it — "rule" (deterministic) or "model" (the ensemble).
    /// Every span currently comes from the rules layer; the ensemble contributes
    /// the overall label but no offsets.
    pub layer: String,
    /// Hard-block category (PROPRIETARY / HEALTH) — can never leave, no override.
    pub hard: bool,
}

/// The classifier's explanation of a piece of text, for the "why" sidebar.
#[derive(Debug, Clone, Serialize)]
pub struct ClassificationExplanation {
    /// "private" | "public" | "uncertain".
    pub label: String,
    /// 0.0..=1.0 confidence in the decision.
    pub confidence: f32,
    /// The detected sensitive spans (empty when nothing tripped the filter).
    pub spans: Vec<ClassificationSpan>,
}

/// Human-friendly label + hard-block flag for a rules category string.
fn category_display(category: &str) -> (&'static str, bool) {
    match category {
        "PII_CONTACT" => ("contact info", false),
        "PII_ID" => ("ID number", false),
        "FINANCIAL" => ("financial detail", false),
        "CREDENTIAL" => ("credential / secret", false),
        "PROPRIETARY" => ("confidential / proprietary", true),
        "HEALTH" => ("health information", true),
        _ => ("sensitive", false),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExplainClassificationArgs {
    pub text: String,
    /// Optional active profile — when present, the explanation uses that
    /// profile's classifier thresholds so the "why" sidebar matches the real
    /// routing decision. Absent (or unknown) → default thresholds.
    #[serde(default)]
    pub profile: Option<String>,
}

/// Classify `text` and return the label + annotated spans, so the UI can show
/// *why* a message was held/redacted (PLAN §11: "censorship is surfaced, never
/// silent" + the annotated review view). Uses the same shared classifier the
/// §7 gate does, under the same per-profile thresholds, so the explanation
/// matches the routing decision exactly.
#[tauri::command]
pub fn explain_classification(
    state: State<'_, AppState>,
    args: ExplainClassificationArgs,
) -> Result<ClassificationExplanation, String> {
    let cfg = profile_classifier_config(&state, args.profile.as_deref());
    Ok(build_explanation(
        state.classifier.classify_with(&args.text, &cfg),
    ))
}

/// Load a profile's classifier thresholds from storage, defaulting on any
/// error or when `profile` is `None`. Central helper for every command that
/// classifies on the user's behalf.
fn profile_classifier_config(
    state: &AppState,
    profile: Option<&str>,
) -> crate::classifier::ClassifierConfig {
    match profile {
        Some(p) => state
            .storage
            .open_profile(p)
            .and_then(|db| db.classifier_config())
            .unwrap_or_default(),
        None => crate::classifier::ClassifierConfig::default(),
    }
}

/// Pure mapping from a `Classification` to the wire shape (testable without a
/// Tauri `State`).
fn build_explanation(c: crate::classifier::Classification) -> ClassificationExplanation {
    let label = match c.label {
        crate::classifier::Label::Private => "private",
        crate::classifier::Label::Public => "public",
        crate::classifier::Label::Uncertain => "uncertain",
    }
    .to_string();

    let spans = c
        .spans
        .into_iter()
        .map(|s| {
            let category = s.category.as_str().to_string();
            let (display, hard) = category_display(&category);
            ClassificationSpan {
                start: s.start_char,
                end: s.end_char,
                text: s.text,
                category,
                label: display.to_string(),
                rule: s.rule.to_string(),
                layer: "rule".to_string(),
                hard,
            }
        })
        .collect();

    ClassificationExplanation {
        label,
        confidence: c.confidence,
        spans,
    }
}

#[cfg(test)]
mod explain_tests {
    use super::*;
    use crate::classifier::{Classifier, RulesClassifier};

    #[test]
    fn explains_rule_spans_with_labels_and_hard_flags() {
        let c = RulesClassifier::new().classify("email me at jane@example.com; SSN 123-45-6789");
        let exp = build_explanation(c);
        assert_eq!(exp.label, "private", "PII present → private");
        assert!(!exp.spans.is_empty(), "should surface the detected spans");

        // Every span carries valid char offsets, a friendly label, and layer.
        for s in &exp.spans {
            assert!(s.end > s.start, "char range must be non-empty");
            assert_eq!(s.layer, "rule");
            assert!(!s.label.is_empty());
        }
        // The email is a (non-hard) contact span.
        assert!(
            exp.spans
                .iter()
                .any(|s| s.category == "PII_CONTACT" && !s.hard),
            "email should be a PII_CONTACT span"
        );
    }

    #[test]
    fn proprietary_is_hard_blocked() {
        let (label, hard) = category_display("PROPRIETARY");
        assert!(hard, "proprietary is a hard-block category");
        assert_eq!(label, "confidential / proprietary");
        assert!(!category_display("PII_CONTACT").1, "contact info is not hard");
    }

    #[test]
    fn benign_text_has_no_spans() {
        let exp = build_explanation(RulesClassifier::new().classify("what time is the meeting?"));
        assert!(exp.spans.is_empty());
    }

    #[test]
    fn memory_sensitivity_routing() {
        let clf = RulesClassifier::new();
        // Benign → shared.
        assert_eq!(
            route_memory_sensitivity(&clf.classify("the standup is at 10am")),
            MemoryRoute::Shared
        );
        // A credential span → never-persist (dropped).
        assert_eq!(
            route_memory_sensitivity(&clf.classify("my api key is sk-ABCD1234efgh5678ijkl9012mnop3456")),
            MemoryRoute::NeverPersist
        );
        // Sensitive-but-durable PII (an SSN — PII_ID, not a credential) → local.
        assert_eq!(
            route_memory_sensitivity(&clf.classify("my SSN is 123-45-6789")),
            MemoryRoute::PrivateLocal
        );
    }
}

// ── memory (PLAN §9 — the curated summary / archive, viewable + editable) ──

/// A memory fact for the UI. `sensitivity` is the bucket it lives in.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryInfo {
    pub id: String,
    pub content: String,
    /// JSON-encoded tag array, or null.
    pub tags: Option<String>,
    pub created_at: i64,
    pub pinned: bool,
    /// "shared" (may inform cloud turns) | "private_local" (local-only).
    pub sensitivity: String,
}

fn bucket_str(b: crate::storage::MemoryBucket) -> &'static str {
    match b {
        crate::storage::MemoryBucket::Shared => "shared",
        crate::storage::MemoryBucket::PrivateLocal => "private_local",
    }
}

fn to_memory_info(fact: crate::storage::MemoryFact, bucket: crate::storage::MemoryBucket) -> MemoryInfo {
    MemoryInfo {
        id: fact.id,
        content: fact.content,
        tags: fact.tags,
        created_at: fact.created_at,
        pinned: fact.pinned,
        sensitivity: bucket_str(bucket).to_string(),
    }
}

// Sensitivity routing lives in `tools::memory` (canonical), so a manual add
// here and an agent `remember` call route identically. Re-exported so the
// in-module tests reach it via `use super::*`.
pub use crate::tools::memory::{route_memory_sensitivity, MemoryRoute};

#[derive(Debug, Clone, Deserialize)]
pub struct ListMemoryArgs {
    pub profile: String,
}

/// List a profile's memory facts (both buckets — this is the user's own local
/// view), pinned-first then newest.
#[tauri::command]
pub fn list_memory(
    state: State<'_, AppState>,
    args: ListMemoryArgs,
) -> Result<Vec<MemoryInfo>, String> {
    state
        .storage
        .memory_db_for_profile(&args.profile)
        .map_err(|e| e.to_string())?
        .list_memory_by_profile(&args.profile, true)
        .map(|rows| rows.into_iter().map(|(f, b)| to_memory_info(f, b)).collect())
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Deserialize)]
pub struct SaveMemoryArgs {
    pub profile: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SaveMemoryResult {
    /// The sensitivity route the classifier chose.
    pub sensitivity: String,
    /// The saved fact, or null when the route was never-persist (dropped).
    pub fact: Option<MemoryInfo>,
}

/// Save a memory fact, routing it by sensitivity (PLAN §9). A credential is
/// dropped (never-persist); a private fact goes to the local-only store; a
/// benign fact to the shared store. Writes go to the profile's memory store —
/// shared global.db, or a walled profile's own DB (Wave 1.5). Returns the saved
/// fact (or null if dropped) — the Settings pane surfaces that outcome (the
/// non-silent trace for a manual save; the agent's `remember` tool fires the
/// in-chat "remembered" event, Wave 1.4).
#[tauri::command]
pub fn save_memory(
    state: State<'_, AppState>,
    args: SaveMemoryArgs,
) -> Result<SaveMemoryResult, String> {
    let content = args.content.trim();
    if content.is_empty() {
        return Err("cannot save an empty memory".to_string());
    }
    // Classify under the profile's own thresholds so a manual save routes to the
    // same sensitivity bucket the profile's gate would pick for the same text.
    let cfg = profile_classifier_config(&state, Some(&args.profile));
    let classification = state.classifier.classify_with(content, &cfg);
    let route = route_memory_sensitivity(&classification);
    let bucket = match route {
        MemoryRoute::NeverPersist => {
            return Ok(SaveMemoryResult {
                sensitivity: "never_persist".to_string(),
                fact: None,
            });
        }
        MemoryRoute::Shared => crate::storage::MemoryBucket::Shared,
        MemoryRoute::PrivateLocal => crate::storage::MemoryBucket::PrivateLocal,
    };
    let fact = crate::storage::MemoryFact {
        id: Uuid::new_v4().to_string(),
        content: content.to_string(),
        origin_profile: args.profile.clone(),
        tags: None,
        created_at: chrono::Utc::now().timestamp(),
        pinned: false,
    };
    let mem = state
        .storage
        .memory_db_for_profile(&args.profile)
        .map_err(|e| e.to_string())?;
    mem.insert_memory_fact_in(bucket, &fact)
        .map_err(|e| e.to_string())?;
    // Meaning-lane index (best-effort; identical to the agent's `remember`) —
    // gated by the profile's semantic setting (Wave 1.2), written to the same store.
    let embedder = if crate::tools::memory::semantic_search_enabled(&state.storage, &args.profile) {
        state.embedder.as_ref().and_then(|h| h.get())
    } else {
        None
    };
    crate::tools::memory::embed_fact_best_effort(&mem, embedder.as_ref(), bucket, &fact);
    Ok(SaveMemoryResult {
        sensitivity: bucket_str(bucket).to_string(),
        fact: Some(to_memory_info(fact, bucket)),
    })
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemoryIdArgs {
    /// The active profile, so a walled profile's fact is deleted from its own
    /// memory DB rather than looked for in the shared store (Wave 1.5).
    pub profile: String,
    pub id: String,
}

/// Forget a memory fact by id (checks both buckets), in the profile's memory
/// store (shared global.db or a walled profile's own DB).
#[tauri::command]
pub fn delete_memory(state: State<'_, AppState>, args: MemoryIdArgs) -> Result<bool, String> {
    state
        .storage
        .memory_db_for_profile(&args.profile)
        .map_err(|e| e.to_string())?
        .delete_memory_fact(&args.id)
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetMemoryPinnedArgs {
    /// The active profile (walled routing — see [`MemoryIdArgs`]).
    pub profile: String,
    pub id: String,
    pub pinned: bool,
}

/// Pin/unpin a fact into the always-loaded curated summary.
#[tauri::command]
pub fn set_memory_pinned(
    state: State<'_, AppState>,
    args: SetMemoryPinnedArgs,
) -> Result<bool, String> {
    state
        .storage
        .memory_db_for_profile(&args.profile)
        .map_err(|e| e.to_string())?
        .set_memory_pinned(&args.id, args.pinned)
        .map_err(|e| e.to_string())
}

// ── memory settings (Wave 1 — per-profile memory toggles) ─────────────────

/// A profile's memory toggles for the UI (Wave 1.2 + 1.5).
#[derive(Debug, Clone, Serialize)]
pub struct MemorySettingsInfo {
    /// Meaning-lane (semantic) memory search on/off (PLAN §9).
    pub semantic_search_enabled: bool,
    /// The §7 memory island — this profile's memory lives in its own DB.
    pub walled: bool,
}

impl From<crate::storage::MemorySettings> for MemorySettingsInfo {
    fn from(s: crate::storage::MemorySettings) -> Self {
        Self {
            semantic_search_enabled: s.semantic_search_enabled,
            walled: s.walled,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GetMemorySettingsArgs {
    pub profile: String,
}

/// The active memory settings for a profile (defaults when unset).
#[tauri::command]
pub fn get_memory_settings(
    state: State<'_, AppState>,
    args: GetMemorySettingsArgs,
) -> Result<MemorySettingsInfo, String> {
    state
        .storage
        .open_profile(&args.profile)
        .and_then(|db| db.memory_settings())
        .map(MemorySettingsInfo::from)
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetMemorySettingsArgs {
    pub profile: String,
    pub semantic_search_enabled: bool,
    pub walled: bool,
}

/// Persist a profile's memory settings. Returns the stored settings. Note the
/// wall is physical (§7): flipping `walled` on routes future reads/writes to
/// this profile's own memory DB; flipping it back off does NOT merge that DB's
/// facts into the shared store — the wall survives the toggle by construction.
#[tauri::command]
pub fn set_memory_settings(
    state: State<'_, AppState>,
    args: SetMemorySettingsArgs,
) -> Result<MemorySettingsInfo, String> {
    let db = state
        .storage
        .open_profile(&args.profile)
        .map_err(|e| e.to_string())?;
    let settings = crate::storage::MemorySettings {
        semantic_search_enabled: args.semantic_search_enabled,
        walled: args.walled,
    };
    db.set_memory_settings(&settings).map_err(|e| e.to_string())?;
    Ok(MemorySettingsInfo::from(settings))
}

// ── helpers ─────────────────────────────────────────────────────────────

fn parse_kind(s: &str) -> Result<ProviderKind, String> {
    match s.to_ascii_lowercase().as_str() {
        "local" => Ok(ProviderKind::Local),
        "cloud" => Ok(ProviderKind::Cloud),
        "custom" => Ok(ProviderKind::Custom),
        other => Err(format!("unknown provider kind: {other}")),
    }
}

fn parse_binding(s: &str) -> Result<Binding, String> {
    match s.to_ascii_lowercase().as_str() {
        "auto" => Ok(Binding::Auto),
        "public" => Ok(Binding::Public),
        "private" => Ok(Binding::Private),
        other => Err(format!("unknown binding: {other}")),
    }
}

// ── scheduled jobs (the ScheduledJobs screen's read/toggle/delete surface) ──
//
// Thin, deliberately read-mostly IPC over the existing per-profile `cron_jobs`
// store. CREATION stays agent-driven (the `manage_cron` tool, Dangerous —
// standing automation is minted through the gate, never silently from a
// settings form); the screen lists what exists and lets the human pause,
// resume, or delete it. Enable/disable/delete are the human's kill switches,
// so they need no approval gate of their own.

/// One scheduled job, for the ScheduledJobs screen. Mirrors `CronJob`.
#[derive(Debug, Clone, Serialize)]
pub struct CronJobInfo {
    pub id: String,
    pub name: String,
    pub prompt: String,
    pub schedule: String,
    pub enabled: bool,
    pub last_run_at: Option<i64>,
    pub last_status: Option<String>,
    pub target_conversation_id: Option<String>,
}

impl From<crate::storage::CronJob> for CronJobInfo {
    fn from(j: crate::storage::CronJob) -> Self {
        Self {
            id: j.id,
            name: j.name,
            prompt: j.prompt,
            schedule: j.schedule,
            enabled: j.enabled,
            last_run_at: j.last_run_at,
            last_status: j.last_status,
            target_conversation_id: j.target_conversation_id,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListCronJobsArgs {
    pub profile: String,
}

/// This profile's scheduled jobs.
#[tauri::command]
pub fn list_cron_jobs(
    state: State<'_, AppState>,
    args: ListCronJobsArgs,
) -> Result<Vec<CronJobInfo>, String> {
    let db = state
        .storage
        .open_profile(&args.profile)
        .map_err(|e| e.to_string())?;
    db.list_cron_jobs()
        .map(|rows| rows.into_iter().map(Into::into).collect())
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetCronJobEnabledArgs {
    pub profile: String,
    pub id: String,
    pub enabled: bool,
}

/// Pause/resume one scheduled job. Returns false if the id doesn't exist.
#[tauri::command]
pub fn set_cron_job_enabled(
    state: State<'_, AppState>,
    args: SetCronJobEnabledArgs,
) -> Result<bool, String> {
    let db = state
        .storage
        .open_profile(&args.profile)
        .map_err(|e| e.to_string())?;
    db.set_cron_job_enabled(&args.id, args.enabled)
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeleteCronJobArgs {
    pub profile: String,
    pub id: String,
}

/// Delete one scheduled job. Returns false if the id doesn't exist.
#[tauri::command]
pub fn delete_cron_job(
    state: State<'_, AppState>,
    args: DeleteCronJobArgs,
) -> Result<bool, String> {
    let db = state
        .storage
        .open_profile(&args.profile)
        .map_err(|e| e.to_string())?;
    db.delete_cron_job(&args.id).map_err(|e| e.to_string())
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_version_is_m1_tag() {
        assert_eq!(get_app_version(), "0.1.0-m1");
    }

    #[test]
    fn list_profiles_has_four_entries() {
        let p = list_profiles();
        assert_eq!(p.len(), 4);
        assert!(p.contains(&"personal".to_string()));
        assert!(p.contains(&"work".to_string()));
        assert!(p.contains(&"school".to_string()));
        assert!(p.contains(&"developer".to_string()));
    }

    // `get_active_profile` now takes `State<AppState>` (reads the persisted
    // `app_settings` row), so its default + round-trip is covered through the
    // real IPC boundary in `contract_tests` (`active_profile_round_trips_*`)
    // and at the storage layer in `storage::tests`
    // (`active_profile_defaults_to_none_and_round_trips`), not as a bare-fn
    // unit test here.

    #[test]
    fn parse_binding_accepts_known_values() {
        assert!(matches!(parse_binding("auto"), Ok(Binding::Auto)));
        assert!(matches!(parse_binding("PUBLIC"), Ok(Binding::Public)));
        assert!(matches!(parse_binding("Private"), Ok(Binding::Private)));
    }

    #[test]
    fn parse_binding_rejects_unknown_values() {
        assert!(parse_binding("maybe").is_err());
    }

    #[test]
    fn parse_kind_accepts_known_values() {
        assert!(matches!(parse_kind("local"), Ok(ProviderKind::Local)));
        assert!(matches!(parse_kind("cloud"), Ok(ProviderKind::Cloud)));
        assert!(matches!(parse_kind("custom"), Ok(ProviderKind::Custom)));
    }

    #[test]
    fn parse_kind_rejects_unknown_values() {
        assert!(parse_kind("hybrid").is_err());
    }

    #[test]
    fn provider_info_omits_api_key() {
        let p = Provider::new(
            "openai",
            "OpenAI",
            "https://api.openai.com/v1",
            Some("sk-secret".into()),
            ProviderKind::Cloud,
        );
        let info: ProviderInfo = p.into();
        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains("sk-secret"), "api key leaked: {json}");
        assert!(json.contains("\"id\":\"openai\""));
    }

    #[test]
    fn default_binding_is_auto() {
        assert_eq!(default_binding(), "auto");
    }

    // ── latest_assistant_routing (send_message's routing-decision source) ──

    fn msg(id: &str, role: &str, routing: Option<&str>) -> Message {
        Message {
            id: id.to_string(),
            conversation_id: "c1".to_string(),
            role: role.to_string(),
            content: String::new(),
            model: None,
            provider_id: None,
            routing_decision: routing.map(str::to_string),
            thinking_content: None,
            error: None,
            aborted: false,
            created_at: 0,
        }
    }

    #[test]
    fn latest_assistant_routing_reads_the_real_decision() {
        // The regression this guards: a live send must surface the real
        // persisted decision (e.g. "route_local"), not a hardcoded "allow".
        let rows = vec![msg("u1", "user", None), msg("a1", "assistant", Some("route_local"))];
        let (id, decision) = latest_assistant_routing(&rows).expect("assistant present");
        assert_eq!(id, "a1");
        assert_eq!(decision, "route_local");
    }

    #[test]
    fn latest_assistant_routing_defaults_to_allow_when_unset() {
        let rows = vec![msg("a1", "assistant", None)];
        let (_, decision) = latest_assistant_routing(&rows).unwrap();
        assert_eq!(decision, "allow");
    }

    #[test]
    fn latest_assistant_routing_picks_the_most_recent_assistant() {
        let rows = vec![
            msg("a1", "assistant", Some("allow")),
            msg("t1", "tool", None),
            msg("a2", "assistant", Some("route_local")),
        ];
        let (id, decision) = latest_assistant_routing(&rows).unwrap();
        assert_eq!(id, "a2", "must pick the newest assistant row, not the first");
        assert_eq!(decision, "route_local");
    }

    #[test]
    fn latest_assistant_routing_is_none_without_an_assistant_row() {
        let rows = vec![msg("u1", "user", None)];
        assert!(latest_assistant_routing(&rows).is_none());
    }
}
