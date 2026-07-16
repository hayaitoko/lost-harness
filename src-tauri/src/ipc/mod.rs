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
//! App state: `Storage` has a manual `unsafe impl Send + Sync` (see
//! `storage::Storage`) — the underlying `rusqlite::Connection` is
//! `!Sync` due to its `RefCell` internals, but every code path that
//! touches `Storage` goes through Tauri's command boundary which is
//! single-threaded per command, and the agent loop uses a private
//! `tokio::sync::Mutex` to serialize streams. So the manual impl is
//! sound for the M1 wiring.
//!
//! Each top-level handle is held in an `Arc` directly (no outer Mutex
//! at the AppState level) so the IPC commands can access them
//! synchronously. The agent loop carries its own internal `Mutex` for
//! stream serialization.

#[cfg(test)]
mod contract_tests;

pub mod approval;

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use uuid::Uuid;

use crate::agent::gate::Binding;
use crate::agent::loop_mod::AgentLoop;
use crate::hooks::{ApprovalDecision, GrantScope, GrantTarget};
use crate::ipc::approval::ApprovalRegistry;
use crate::models::{ModelManager, Provider, ProviderKind};
use crate::storage::{Conversation, Message, Storage};

// ── App state ────────────────────────────────────────────────────────────

/// Shared application state. Tauri stores this via `.manage(state)` and
/// injects it into commands with `state: State<'_, AppState>`. Each
/// field is an `Arc<T>` where `T: Send + Sync`. See the module docs
/// for why the `unsafe impl Send + Sync for Storage` is sound.
pub struct AppState {
    pub agent_loop: Arc<AgentLoop>,
    pub model_manager: Arc<ModelManager>,
    pub storage: Arc<Storage>,
    /// In-flight tool-approval prompts (§3.5). The dispatcher parks a request
    /// here and awaits it; `resolve_tool_approval` answers by id.
    pub approvals: Arc<ApprovalRegistry>,
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
}

impl From<Provider> for ProviderInfo {
    fn from(p: Provider) -> Self {
        // Compute `is_private` first — it takes `&self` and we want to
        // move `id`/`name`/`base_url`/`kind` afterwards.
        let is_private = p.is_private();
        Self {
            id: p.id,
            name: p.name,
            base_url: p.base_url,
            kind: p.kind,
            is_private,
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

/// Returns the id of the currently active profile. M1 just returns the
/// default; per-profile routing lands when the UI ships the cycle chip.
#[tauri::command]
pub fn get_active_profile() -> String {
    "personal".to_string()
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
    );
    state.model_manager.add_provider(provider.clone());
    Ok(provider.into())
}

#[tauri::command]
pub fn remove_provider(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    if state.model_manager.get_provider(&id).is_none() {
        return Err(format!("unknown provider: {id}"));
    }
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
    let binding = parse_binding(&args.binding)
        .map_err(|e| format!("invalid binding {:?}: {e}", args.binding))?;

    let profile = args.profile.clone();
    let started = chrono::Utc::now().timestamp_millis();
    let conversation_id = args.conversation_id.clone();

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
            app.clone(),
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
    let scope = match args.scope.as_deref().map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("session") => GrantScope::Session,
        Some("always") => GrantScope::Always,
        _ => GrantScope::Once,
    };
    // A one-time grant is per-action, never whole-tool: force `action` for
    // `Once` so a "just this once" answer can't widen to every call of the
    // tool (defense in depth with `ApprovalLedger::grant`).
    let want_tool = !matches!(scope, GrantScope::Once)
        && matches!(args.target.as_deref(), Some(t) if t.eq_ignore_ascii_case("tool"));

    let answered = state.approvals.answer(&args.id, |fingerprint, tool_name| {
        if !approve {
            return ApprovalDecision::Deny;
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

    #[test]
    fn active_profile_defaults_to_personal() {
        assert_eq!(get_active_profile(), "personal");
    }

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
