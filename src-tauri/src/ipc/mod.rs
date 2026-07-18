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
pub mod ask_human;

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use uuid::Uuid;

use crate::agent::gate::Binding;
use crate::agent::loop_mod::AgentLoop;
use crate::hooks::{ApprovalDecision, GrantScope, GrantTarget, PermissionMode, ToolRule};
use crate::ipc::approval::ApprovalRegistry;
use crate::ipc::ask_human::AskHumanRegistry;
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
    pub supports_native_tools: bool,
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
    state.model_manager.add_provider(provider.clone());
    // Persist so the flag (and the endpoint) survive a restart and hydrate
    // back on next boot. Best-effort: a storage failure logs but the
    // in-memory provider still works for this session.
    if let Err(e) = state.storage.global().insert_endpoint(&crate::storage::Endpoint {
        id: id.clone(),
        name: provider.name.clone(),
        base_url: provider.base_url.clone(),
        api_key_encrypted: provider.api_key.as_ref().map(|k| k.as_bytes().to_vec()),
        kind: args.kind.clone(),
        created_at: chrono::Utc::now().timestamp(),
        supports_native_tools: provider.supports_native_tools,
    }) {
        tracing::warn!(error = %e, "failed to persist endpoint (in-memory only this session)");
    }
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
    let session_mode = args
        .mode
        .as_deref()
        .map(crate::hooks::SessionMode::from_str_lenient)
        .unwrap_or_default();

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
            session_mode,
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
    state
        .storage
        .global()
        .set_skill_approval(&args.id, status)
        .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeleteSkillArgs {
    pub id: String,
}

/// Delete a saved skill (two-click confirm in the UI). Returns whether a row was removed.
#[tauri::command]
pub fn delete_skill(state: State<'_, AppState>, args: DeleteSkillArgs) -> Result<bool, String> {
    state
        .storage
        .global()
        .delete_skill(&args.id)
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
