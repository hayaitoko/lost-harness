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
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use uuid::Uuid;

use crate::agent::gate::Binding;
use crate::agent::loop_mod::AgentLoop;
use crate::agent::result_sink::{ResultSink, TauriResultSink};
use crate::email::api_error::GoogleApi;
use crate::email::gmail::GmailApi as _; // trait methods on GmailClient (email round)
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
    /// Email round: pending OAuth dances + the needs-reconnect flags.
    pub email: Arc<EmailRuntime>,
    /// In-flight `ask_human` prompts. The tool parks a question here and awaits
    /// it; `resolve_ask_human` delivers the user's answer by id.
    pub ask_human: Arc<AskHumanRegistry>,
    /// The active privacy classifier (trained ensemble or rules-only fallback),
    /// shared with the §7 gate. Backs `explain_classification` for the
    /// annotated-redaction "why" sidebar (PLAN §11).
    pub classifier: Arc<dyn crate::classifier::Classifier>,
    /// C-01 / H-12: the SAME `PrivacyGate` the agent loop enforces with (clone
    /// shares its `Arc`s). Held so the UI can actually observe gate state:
    /// `get_classifier_health` reads the degraded flag for the warning banner,
    /// and `confirm_public_send` records a one-send confirmation the gate will
    /// consume on the user's retry. Without this field the degraded flag had
    /// zero call sites — nothing could react to it.
    pub gate: crate::agent::gate::PrivacyGate,
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
    /// call), so `probe_hardware` / `calculate_model_fit`
    /// read this snapshot instead of re-probing.
    pub hardware: Arc<crate::models::hardware::HardwareProfile>,
    /// M8 S4: the bundled-sidecar context (supervisor + resolved binary).
    /// `None` ⇒ no sidecar binary resolved at boot — local models need an
    /// external runner. Used by `remove_local_model` (stop-before-delete) and
    /// the app-exit teardown.
    #[cfg(feature = "local-runner")]
    pub local_runner: Option<Arc<crate::models::runner::LocalRunnerContext>>,
    /// H-07: outstanding MCP install nonces (nonce → issue time). Issued by
    /// `generate_mcp_install_nonce` on the consent path, consumed once by
    /// `register_mcp_server`, expired after [`MCP_INSTALL_NONCE_TTL`].
    pub pending_mcp_nonces: Arc<Mutex<std::collections::HashMap<String, Instant>>>,
}

// ── Response types ───────────────────────────────────────────────────────

/// Returned by `send_message` once the agent finishes a turn. Streaming
/// tokens arrive separately via the `stream:token` event.
#[derive(Debug, Clone, Serialize)]
pub struct SendMessageResponse {
    /// Id of the persisted assistant row. `null` when there is no such row to
    /// point at (see `routing_unavailable`) — never a placeholder, so the
    /// frontend can't adopt an id that addresses nothing.
    pub message_id: Option<String>,
    pub content: String,
    pub conversation_id: String,
    /// "personal" | "work" | "school" | "developer" — the profile the
    /// message was handled under.
    pub profile: String,
    /// "allow" | "route_local" — which branch of the gate served this
    /// message. Frontend uses this to label the chip / banner.
    ///
    /// M-22: `null`, never a stand-in, when the persisted decision could not
    /// be read. A fabricated value here is a privacy-UI lie: the routing badge
    /// is how the user learns whether their text left the machine.
    pub routing_decision: Option<String>,
    /// Present *only* on the degraded path, naming why `message_id` /
    /// `routing_decision` are `null`. Absent from the JSON on the happy path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routing_unavailable: Option<RoutingUnavailable>,
    pub completed_at: i64,
}

/// Why a completed `send_message` has no persisted routing decision to
/// report. Serialized in snake_case on `SendMessageResponse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingUnavailable {
    /// The profile database could not be opened for the read-back.
    ProfileDbUnavailable,
    /// The conversation's messages could not be queried.
    MessageQueryFailed,
    /// The read succeeded but the turn persisted no assistant row — the
    /// normal shape of a gate *block*, where nothing was written.
    NoAssistantRow,
}

/// What the post-turn read-back yielded.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RoutingLookup {
    Resolved {
        message_id: String,
        routing_decision: String,
    },
    Unavailable(RoutingUnavailable),
}

impl RoutingLookup {
    /// The three `SendMessageResponse` fields this outcome authorizes:
    /// `(message_id, routing_decision, routing_unavailable)`. `send_message`
    /// builds its payload through here rather than matching inline, so the
    /// unit tests below exercise the mapping the command actually uses —
    /// there is no second copy of it to drift.
    fn into_parts(self) -> (Option<String>, Option<String>, Option<RoutingUnavailable>) {
        match self {
            RoutingLookup::Resolved {
                message_id,
                routing_decision,
            } => (Some(message_id), Some(routing_decision), None),
            RoutingLookup::Unavailable(why) => (None, None, Some(why)),
        }
    }
}

/// Map the outcome of the post-turn profile-db read onto what we are willing
/// to tell the frontend. Pure so it stays unit-tested: the bug this replaced
/// substituted `("", "unknown")` into an otherwise-successful payload, which
/// no test could see because the substitution happened inline in the command.
fn routing_lookup(rows: Result<Vec<Message>, RoutingUnavailable>) -> RoutingLookup {
    match rows {
        Err(why) => RoutingLookup::Unavailable(why),
        Ok(rows) => match latest_assistant_routing(&rows) {
            Some((message_id, routing_decision)) => RoutingLookup::Resolved {
                message_id,
                routing_decision,
            },
            None => RoutingLookup::Unavailable(RoutingUnavailable::NoAssistantRow),
        },
    }
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
        let trusted_by_name =
            crate::agent::egress::is_private_endpoint_trusted_by_name(&p.base_url);
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
pub struct SetConversationBindingArgs {
    pub profile: String,
    pub conversation_id: String,
    /// The per-conversation privacy intent: "auto" | "public" | "private".
    pub binding: String,
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

/// Returns the app version string from Cargo.toml.
#[tauri::command]
pub fn get_app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
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

/// Persists a chat's routing intent in its own profile database.  This is
/// deliberately narrower than the older storage `update_conversation` helper:
/// the UI must not be able to accidentally mutate a title, pin, or folder when
/// all the user asked to change was where the conversation may run.
#[tauri::command]
pub fn set_conversation_binding(
    state: State<'_, AppState>,
    args: SetConversationBindingArgs,
) -> Result<ConversationInfo, String> {
    if !matches!(args.binding.as_str(), "auto" | "public" | "private") {
        return Err("binding must be auto, public, or private".to_string());
    }
    let db = state
        .storage
        .open_profile(&args.profile)
        .map_err(|e| e.to_string())?;
    let existing = db
        .list_conversations()
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|row| row.id == args.conversation_id)
        .ok_or_else(|| "conversation not found in this profile".to_string())?;
    if !db
        .set_conversation_binding(&existing.id, &args.binding)
        .map_err(|e| e.to_string())?
    {
        return Err("conversation not found in this profile".to_string());
    }
    Ok(ConversationInfo {
        id: existing.id,
        name: existing.name,
        pinned: existing.pinned,
        binding: args.binding,
        folder_id: existing.folder_id,
        color: existing.color,
        created_at: existing.created_at,
        updated_at: chrono::Utc::now().timestamp(),
    })
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

/// Does `host` name a machine on this device or this LAN, such that talking
/// to it over cleartext `http` never puts a bearer key on the public
/// internet?
///
/// This is an *address* test, not a string test. The previous version asked
/// `host.starts_with("127.")`, which `http://127.evil.com` satisfies — a
/// public DNS name that would have been handed the provider's API key over
/// plaintext HTTP. Everything here goes through `url::Host`, so a name is
/// only ever accepted when it is the literal `localhost`; anything else must
/// be an IP literal that the standard library agrees is loopback or
/// private.
///
/// Accepted for cleartext:
/// - the name `localhost` (and nothing else — `localhost.evil.com`,
///   `127.evil.com`, `10.evil.com` are all `Domain`s and all rejected)
/// - IPv4 loopback `127.0.0.0/8`, RFC 1918 (`10/8`, `172.16/12`,
///   `192.168/16`), link-local `169.254/16`, CGNAT/Tailscale `100.64/10`
/// - IPv6 loopback `::1`, unique-local `fc00::/7`, link-local `fe80::/10`
///
/// The RFC 1918 / CGNAT ranges are here deliberately: the product's own
/// contract test and the documented deployment (a llama.cpp server on the
/// user's LAN, e.g. `http://10.0.0.100:8000/v1`) add providers over
/// cleartext HTTP on the local network. Requiring `is_loopback()` alone
/// would have made every LAN endpoint unaddable. Private *names*
/// (`.local`, `.lan`, `.ts.net`) stay rejected for cleartext even though
/// `agent::egress` treats them as private, because a name is resolved by
/// whatever DNS/mDNS answers first and cannot be trusted to hold a
/// cleartext bearer key.
fn host_allows_cleartext(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(name) => name.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(ip) => {
            let o = ip.octets();
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                // 100.64.0.0/10 — CGNAT, which Tailscale hands out.
                || (o[0] == 100 && (64..=127).contains(&o[1]))
        }
        url::Host::Ipv6(ip) => {
            let first = ip.segments()[0];
            ip.is_loopback()
                // fc00::/7 unique-local, fe80::/10 link-local. Both have
                // unstable std predicates, so they're spelled out here.
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
        }
    }
}

/// Validate a provider base URL.
///
/// Rejects:
/// - Unparsable URLs.
/// - Embedded credentials (`user:pass@host`).
/// - Fragments (`#`).
/// - Empty/ambiguous hosts.
/// - Non-HTTPS schemes for anything that isn't a loopback/private *address*
///   (see [`host_allows_cleartext`]) — so users can point at a local or LAN
///   LM Studio / Ollama / llama.cpp server without TLS, while a public host
///   can never be handed a bearer key in cleartext.
fn validate_base_url(raw: &str) -> Result<(), String> {
    let url = url::Url::parse(raw).map_err(|e| format!("invalid base URL: {e}"))?;

    // Reject embedded credentials — bearer keys should never be in the URL.
    if url.username() != "" || url.password().is_some() {
        return Err("base URL must not contain embedded credentials (user:password@)".into());
    }

    // Reject fragments — server has no use for them.
    if url.fragment().is_some() {
        return Err("base URL must not contain a fragment (#)".into());
    }

    // Reject empty / ambiguous hosts. `url::Url::host` gives the *parsed*
    // host (Domain / Ipv4 / Ipv6), which is what the cleartext decision below
    // needs — `host_str` would hand back a bare string and invite another
    // prefix test.
    let Some(host) = url.host() else {
        return Err("base URL has no host".into());
    };
    if matches!(host, url::Host::Domain(d) if d.is_empty()) {
        return Err("base URL has no host".into());
    }

    // Scheme check: require HTTPS unless the host is a loopback/private
    // address. See `host_allows_cleartext` — this is an address test, not a
    // string-prefix test.
    match url.scheme() {
        "https" => {}                                // always OK
        "http" if host_allows_cleartext(&host) => {} // OK for local/LAN servers
        "http" => {
            return Err(
                "cleartext http:// is only allowed for loopback or private-network addresses (localhost, 127.x, 10.x, 172.16–31.x, 192.168.x, 169.254.x, 100.64–127.x, ::1, fc00::/7, fe80::/10) — public endpoints must use HTTPS"
                    .into(),
            );
        }
        other => {
            return Err(format!(
                "unsupported URL scheme \"{other}\" (expected https or http)"
            ));
        }
    }

    Ok(())
}

#[tauri::command]
pub fn add_provider(
    state: State<'_, AppState>,
    args: AddProviderArgs,
) -> Result<ProviderInfo, String> {
    add_provider_inner(&state, args)
}

/// Body of [`add_provider`], taking `&AppState` instead of `State` so it is
/// reachable from unit tests (a `State` can only be minted by a live Tauri
/// app). The command above is a one-line forwarder.
fn add_provider_inner(state: &AppState, args: AddProviderArgs) -> Result<ProviderInfo, String> {
    let kind = parse_kind(&args.kind)?;
    validate_base_url(&args.base_url)?;
    let id = Uuid::new_v4().to_string();
    let provider = Provider::new(id.clone(), args.name, args.base_url, args.api_key, kind)
        .with_native_tools(args.supports_native_tools);
    let wrote_secret = if let Some(secret) = provider.api_key.as_deref() {
        state.provider_secrets.set(&id, secret)?;
        true
    } else {
        false
    };
    // Persist metadata BEFORE publishing in-memory success. A storage
    // failure is surfaced to the caller (not silently swallowed) so the
    // frontend can show an error instead of a provider that vanishes on
    // restart.
    let persisted = state
        .storage
        .global()
        .insert_endpoint(&crate::storage::Endpoint {
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
        });
    if let Err(e) = persisted {
        // M-05: the keychain write happened first, so a failed insert would
        // otherwise strand a secret under an id no row (and no UI) will ever
        // mention again — unreachable, undeletable, and still holding a live
        // API key. Compensate, exactly as `update_provider` does on its own
        // persist failure.
        if wrote_secret {
            let _ = state.provider_secrets.delete(&id);
        }
        return Err(format!("failed to persist endpoint: {e}"));
    }
    state.model_manager.add_provider(provider.clone());
    Ok(provider.into())
}

#[tauri::command]
pub fn update_provider(
    state: State<'_, AppState>,
    args: UpdateProviderArgs,
) -> Result<ProviderInfo, String> {
    let kind = parse_kind(&args.kind)?;
    validate_base_url(&args.base_url)?;
    let existing = state
        .model_manager
        .get_provider(&args.id)
        .ok_or_else(|| format!("unknown provider: {}", args.id))?;

    // Snapshot the old secret so we can restore it if persistence fails.
    let old_secret = state.provider_secrets.get(&args.id).ok().flatten();

    let api_key = args.api_key.or(existing.api_key);
    let provider = Provider::new(args.id.clone(), args.name, args.base_url, api_key, kind)
        .with_native_tools(args.supports_native_tools);

    // Persist metadata BEFORE publishing in-memory success.
    // Keychain and DB must both succeed before the provider is visible.
    if let Some(secret) = provider.api_key.as_deref() {
        state.provider_secrets.set(&args.id, secret)?;
    }
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
            state
                .storage
                .global()
                .insert_endpoint(&crate::storage::Endpoint {
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
        // Restore the old secret on failure so the keychain stays
        // consistent with the rolled-back DB / memory state.
        match old_secret {
            Some(ref s) => {
                let _ = state.provider_secrets.set(&args.id, s);
            }
            None => {
                let _ = state.provider_secrets.delete(&args.id);
            }
        }
        return Err(format!("failed to persist endpoint update: {e}"));
    }

    // Publish in-memory success only after durable storage is updated.
    state.model_manager.add_provider(provider.clone());
    Ok(provider.into())
}

#[tauri::command]
pub fn remove_provider(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    if state.model_manager.get_provider(&id).is_none() {
        return Err(format!("unknown provider: {id}"));
    }
    // Delete durable state first: DB before keychain, so a mid-failure
    // leaves the two consistent (keychain entry has no backing row).
    state
        .storage
        .global()
        .delete_endpoint(&id)
        .map_err(|e| e.to_string())?;
    state.provider_secrets.delete(&id)?;
    state.model_manager.remove_provider(&id);
    Ok(true)
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderIdArgs {
    pub provider_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetProviderApiKeyArgs {
    pub provider_id: String,
    pub api_key: String,
}

/// One-shot command: update a provider's API key in the OS credential store
/// without re-registering the provider. Used by packet P04 for key rotation.
#[tauri::command]
pub fn set_provider_api_key(
    state: State<'_, AppState>,
    args: SetProviderApiKeyArgs,
) -> Result<(), String> {
    set_provider_api_key_inner(&state, args)
}

/// Body of [`set_provider_api_key`], taking `&AppState` so it is unit-testable
/// without a live Tauri app.
fn set_provider_api_key_inner(state: &AppState, args: SetProviderApiKeyArgs) -> Result<(), String> {
    let existing = state
        .model_manager
        .get_provider(&args.provider_id)
        .ok_or_else(|| format!("unknown provider: {}", args.provider_id))?;
    state
        .provider_secrets
        .set(&args.provider_id, &args.api_key)?;
    // Persist the presence marker, or the secret we just stored is unreachable
    // after a restart. `hydrate_providers_from_storage` (lib.rs) reads the
    // keychain ONLY when `ep.has_keychain_secret()`, i.e. when this column is
    // set. The frontend always creates a provider with `api_key: null` and
    // delivers the key through this command, so `add_provider` never writes the
    // marker and this is the only place that can: without it every key is
    // silently dropped on the next launch while still sitting in the keychain.
    //
    // Compensating on failure, the same shape `add_provider`/`update_provider`
    // use: an orphaned keychain entry with no marker is exactly the state that
    // produced this bug, so undo the secret rather than leave it dangling.
    // `mark_endpoint_secret_in_keychain` returns Ok(false) when the UPDATE
    // matched zero rows — a provider live in `ModelManager` with no `endpoints`
    // row (the bundled local sidecar is registered exactly that way). Treating
    // that as success would recreate the very state this fix exists to prevent:
    // a secret in the keychain with no marker. `secrets.rs` already matches on
    // `Ok(true)` for the same reason; do the same here.
    let marked = state
        .storage
        .global()
        .mark_endpoint_secret_in_keychain(&args.provider_id);
    let failure = match marked {
        Ok(true) => None,
        Ok(false) => Some("it has no stored endpoint row".to_string()),
        Err(e) => Some(e.to_string()),
    };
    if let Some(why) = failure {
        let _ = state.provider_secrets.delete(&args.provider_id);
        return Err(format!(
            "stored the key but could not record it on the provider row ({why}), \
             so it would not survive a restart; the key was removed again"
        ));
    }
    // Rotation must take effect NOW, not after a restart. The keychain is only
    // read at boot when seeding `ModelManager`; the live `Provider` (and the
    // `ModelClient` cached against it, which copies the key into its bearer
    // header) would otherwise keep signing requests with the revoked key for
    // the rest of the session. `add_provider` replaces the record by id and
    // drops the cached client, so the next `get_client` rebuilds with the new
    // key.
    let rotated = Provider {
        api_key: Some(args.api_key),
        ..existing
    };
    state.model_manager.add_provider(rotated);
    Ok(())
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
        // Preserve the provider's nested HTTP error in the UI. `to_string()`
        // kept only the outer context (for example, a provider UUID), making
        // a recoverable configuration/compatibility failure impossible to
        // diagnose from the chat surface.
        .map_err(|e| format!("{e:#}"))?;

    // Look up the assistant message we just persisted. We re-query the
    // profile db (read-only) and pick the most recent assistant row —
    // this gives us both the message id AND the real routing decision
    // that process_message stamped on it (one of "allow" / "route_local"),
    // so the frontend's RoutingBadge is honest on a live send instead of
    // only after a reload.
    //
    // M-22: each way this read can come up empty is reported as itself. It
    // used to collapse into `("", "unknown")` inside an `Ok(..)` payload,
    // which told the frontend "the turn succeeded and here is its routing" —
    // with a message id addressing nothing and a decision nobody made.
    let rows = state
        .storage
        .open_profile(&profile)
        .map_err(|_| RoutingUnavailable::ProfileDbUnavailable)
        .and_then(|db| {
            db.list_messages_by_conversation(&conversation_id)
                .map_err(|_| RoutingUnavailable::MessageQueryFailed)
        });
    let (message_id, routing_decision, routing_unavailable) = routing_lookup(rows).into_parts();

    Ok(SendMessageResponse {
        message_id,
        content,
        conversation_id,
        profile,
        routing_decision,
        routing_unavailable,
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
                    if let Some(tool) = crate::tools::skills::SkillTool::for_skill(
                        &skill,
                        Arc::clone(&state.storage),
                    ) {
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
/// allowlisted-publisher Staff-picks default; a non-empty query searches live.
/// Every row's `provenance` is resolved against the signed model manifest under
/// the storage base (P09 / H-08) — fail-closed, so without a verified manifest
/// entry a row is `community` no matter who published it. Networked.
#[tauri::command]
pub async fn search_models(
    state: State<'_, AppState>,
    args: SearchModelsArgs,
) -> Result<Vec<crate::models::hf_search::HfModelSummary>, String> {
    use crate::models::hf_search::{search, staff_picks, SearchSort};
    let limit = args.limit.unwrap_or(25);
    let q = args.query.trim().to_string();
    let sort = match args.sort.as_deref() {
        Some("likes") => SearchSort::Likes,
        Some("trending") => SearchSort::Trending,
        Some("last_modified") => SearchSort::LastModified,
        _ => SearchSort::Downloads,
    };
    let storage_base = state.storage.base_path().to_path_buf();
    let result = if q.is_empty() {
        staff_picks(limit, &storage_base).await
    } else {
        search(&q, sort, limit, &storage_base).await
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

/// Fetch a model's quants + a representative [`ModelSpec`] (M8 S2′). The
/// provenance/pinning decision comes from the signed manifest under the storage
/// base (P09 / H-08); `detail.manifest` reports why. Networked.
#[tauri::command]
pub async fn get_model_detail(
    state: State<'_, AppState>,
    args: GetModelDetailArgs,
) -> Result<ModelDetailResponse, String> {
    let storage_base = state.storage.base_path().to_path_buf();
    let detail = crate::models::hf_search::model_detail(&args.model_id, &storage_base)
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
        None => (
            None,
            vec!["No downloadable quant found for this model.".to_string()],
        ),
    };
    Ok(ModelDetailResponse {
        detail,
        spec,
        spec_notes,
    })
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
    let db = state
        .storage
        .open_profile(&args.profile)
        .map_err(|e| e.to_string())?;
    // `?` propagates a corrupt-row Err; `unwrap_or_default` only fills the
    // UNSET (None) case with the library default.
    Ok(db
        .get_sandbox_config()
        .map_err(|e| e.to_string())?
        .unwrap_or_default())
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
    let db = state
        .storage
        .open_profile(&args.profile)
        .map_err(|e| e.to_string())?;
    db.set_sandbox_config(&args.config)
        .map_err(|e| e.to_string())?;
    Ok(args.config)
}

/// Reject shapes that would be silently mis-stored: empty allowlist / exclusion
/// entries (a blank domain/socket/command is never meaningful and would just be
/// dead weight the shell path has to skip). Fail closed on bad input.
fn validate_sandbox_config(cfg: &crate::hooks::SandboxConfig) -> Result<(), String> {
    if cfg
        .network
        .allowed_domains
        .iter()
        .any(|d| d.trim().is_empty())
    {
        return Err("sandbox_config: allowed_domains entries must not be empty".into());
    }
    if cfg
        .network
        .allow_unix_sockets
        .iter()
        .any(|s| s.trim().is_empty())
    {
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
    let db = state
        .storage
        .open_profile(&args.profile)
        .map_err(|e| e.to_string())?;
    Ok(BudgetSettings {
        cap_usd: db.budget_cap().map_err(|e| e.to_string())?,
    })
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
    let db = state
        .storage
        .open_profile(&args.profile)
        .map_err(|e| e.to_string())?;
    db.set_budget_cap(args.cap_usd).map_err(|e| e.to_string())?;
    Ok(BudgetSettings {
        cap_usd: db.budget_cap().map_err(|e| e.to_string())?,
    })
}

/// Clear the cap entirely (uncapped).
#[tauri::command]
pub fn reset_budget_settings(
    state: State<'_, AppState>,
    args: GetBudgetSettingsArgs,
) -> Result<BudgetSettings, String> {
    crate::storage::validate_profile_name(&args.profile).map_err(|e| e.to_string())?;
    let db = state
        .storage
        .open_profile(&args.profile)
        .map_err(|e| e.to_string())?;
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
    /// "local" | "remote" (anything else fails closed to remote). Streamable
    /// HTTP endpoints are always remote, regardless of this field.
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub trusted_read_only: bool,
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// H-07: a single-use install nonce from `generate_mcp_install_nonce`.
    /// Required — a `register_mcp_server` call that never went through the
    /// consent step has no nonce to present and is rejected before any spawn.
    pub nonce: String,
}

/// How long an issued MCP install nonce stays usable.
const MCP_INSTALL_NONCE_TTL: Duration = Duration::from_secs(300);

/// Issue a single-use, short-lived MCP install nonce and remember it.
/// Split out of the Tauri command so the gate is directly testable.
fn issue_mcp_install_nonce(nonces: &Mutex<std::collections::HashMap<String, Instant>>) -> String {
    let nonce = Uuid::new_v4().to_string();
    let now = Instant::now();
    if let Ok(mut map) = nonces.lock() {
        // Opportunistically drop anything already past its TTL so an abandoned
        // install flow can't grow the map without bound.
        map.retain(|_, issued| now.duration_since(*issued) <= MCP_INSTALL_NONCE_TTL);
        map.insert(nonce.clone(), now);
    }
    nonce
}

/// Consume a nonce. Fails closed on unknown, already-used, or expired values —
/// and removes it either way, so a nonce is never usable twice.
fn consume_mcp_install_nonce(
    nonces: &Mutex<std::collections::HashMap<String, Instant>>,
    nonce: &str,
) -> Result<(), String> {
    let mut map = nonces
        .lock()
        .map_err(|_| "the MCP install nonce store is poisoned".to_string())?;
    let issued = map.remove(nonce).ok_or_else(|| {
        "this MCP install was not confirmed — no valid install nonce was presented".to_string()
    })?;
    if issued.elapsed() > MCP_INSTALL_NONCE_TTL {
        return Err("the MCP install confirmation expired — start the install again".to_string());
    }
    Ok(())
}

/// H-07: mint the install nonce that `register_mcp_server` demands. The UI must
/// call this only from the confirmed-install path; it is the token that proves
/// registration came from that flow rather than a bare `invoke(...)`.
#[tauri::command]
pub fn generate_mcp_install_nonce(state: State<'_, AppState>) -> Result<String, String> {
    Ok(issue_mcp_install_nonce(&state.pending_mcp_nonces))
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
/// H-07 enforces the second half of that contract mechanically: the call needs
/// a single-use nonce from `generate_mcp_install_nonce`, and the executable it
/// resolves to is hashed and pinned so a later swap can't ride this consent.
#[tauri::command]
pub async fn register_mcp_server(
    state: State<'_, AppState>,
    args: RegisterMcpServerArgs,
) -> Result<McpServerInfo, String> {
    // H-07: consent gate FIRST — before any validation, resolution, or spawn.
    consume_mcp_install_nonce(&state.pending_mcp_nonces, &args.nonce)?;
    if args.name.trim().is_empty() || args.command.trim().is_empty() {
        return Err("an MCP server needs a non-empty name and command".to_string());
    }
    let is_http = args.command.trim_start().starts_with("https://")
        || args.command.trim_start().starts_with("http://");
    if is_http && !args.args.is_empty() {
        return Err("a Streamable HTTP MCP endpoint does not take process arguments".to_string());
    }
    // Review fix (#2): reject a sanitized-NAMESPACE collision with an existing
    // server. The `mcp__{server}__{tool}` separator is collision-free per
    // (server, tool) — so the only cross-server collision domain is the server
    // segment itself; letting two servers share it would let the EARLIER one
    // silently pre-claim (and answer for) the later one's tool names.
    let new_seg = crate::tools::mcp::sanitize_name_segment(args.name.trim());
    let existing = state
        .storage
        .global()
        .list_mcp_servers()
        .map_err(|e| e.to_string())?;
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
    // H-07: for a stdio server, resolve the command to a concrete file and pin
    // the whole invocation now — the executable's contents, the argv vector, and
    // any script file argv names. This registration IS the user approving that
    // exact invocation, and for the common `node …` / `npx …` / `python …` shape
    // the argv is where the actual server code lives, so pinning the interpreter
    // alone would pin nothing that matters. HTTP endpoints have no local
    // executable to pin.
    let (executable_path, executable_hash) = if is_http {
        (None, None)
    } else {
        let (path, hash) =
            crate::tools::mcp_stdio::resolve_and_hash_executable(args.command.trim(), &args.args)
                .map_err(|e| format!("couldn't pin the MCP server executable: {e}"))?;
        (Some(path), Some(hash))
    };
    let row = crate::storage::McpServerRow {
        id: uuid::Uuid::new_v4().to_string(),
        name: args.name.trim().to_string(),
        command: args.command.trim().to_string(),
        args: args.args,
        tier: if is_http {
            "remote".to_string()
        } else {
            match args.tier.as_deref() {
                Some("local") => "local".to_string(),
                _ => "remote".to_string(), // ambiguous ⇒ remote (the stricter tier)
            }
        },
        trusted_read_only: args.trusted_read_only,
        capabilities: args.capabilities,
        enabled: true,
        created_at: chrono::Utc::now().timestamp(),
        executable_path,
        executable_hash,
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
    let rows = state
        .storage
        .global()
        .list_mcp_servers()
        .map_err(|e| e.to_string())?;
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
    state
        .storage
        .global()
        .delete_mcp_server(&args.id)
        .map_err(|e| e.to_string())
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
        Self {
            id: m.id,
            name: m.name,
            path: m.path,
            size_bytes: m.size_bytes,
            status: m.status,
        }
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
        // Delete the file FIRST. If this fails, keep the DB row (so the user
        // can retry) and surface the error instead of orphaning gigabytes of
        // GGUF on disk with no catalog entry.
        std::fs::remove_file(&m.path).map_err(|e| format!("failed to delete model file: {e}"))?;
    }
    global.delete_model(&args.id).map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Deserialize)]
pub struct DownloadModelArgs {
    /// The exact Hugging Face repository selected in the live discovery UI.
    pub model_id: String,
    /// The first file of the selected logical quant. The backend re-fetches the
    /// repository manifest and resolves this filename itself; URL/hash values
    /// from the renderer are never trusted.
    pub first_filename: String,
    /// Community repositories are deliberately a two-step UI action. The
    /// backend re-derives provenance from the live manifest before accepting
    /// this acknowledgement.
    #[serde(default)]
    pub acknowledge_community: bool,
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

/// Download one single-file GGUF selected from the live Hugging Face discovery
/// result. The artifact URL and LFS hash are fetched again from the repository
/// tree on the backend, so the webview can neither redirect a download nor
/// substitute a checksum. Community repositories require an explicit UI
/// acknowledgement; trusted-publisher status is re-derived here, never taken
/// from the webview. A digest mismatch installs nothing.
///
/// Multi-file GGUFs are shown in discovery but intentionally refused here: the
/// current local-model database records one independently boot-verified file
/// per runnable model. Treating just the first part as verified would be a
/// security bug, so the app asks the user to choose a single-file quant instead
/// of pretending split artifacts are safe to run.
#[tauri::command]
pub async fn download_model(
    state: State<'_, AppState>,
    app: AppHandle,
    args: DownloadModelArgs,
) -> Result<DownloadedModelInfo, String> {
    use crate::models::hf_search::Provenance;

    let storage_base = state.storage.base_path().to_path_buf();
    let detail = crate::models::hf_search::model_detail(args.model_id.trim(), &storage_base)
        .await
        .map_err(|e| e.to_string())?;
    let quant = detail
        .quants
        .into_iter()
        .find(|q| {
            q.complete
                && q.files
                    .first()
                    .is_some_and(|f| f.filename == args.first_filename)
        })
        .ok_or_else(|| {
            "the selected GGUF is no longer present or is incomplete; refresh the model details"
                .to_string()
        })?;
    if quant.files.len() != 1 {
        return Err(
            "split GGUF files are not yet supported by the verified local runner; choose a single-file quant"
                .to_string(),
        );
    }
    if detail.provenance == Provenance::Community && !args.acknowledge_community {
        return Err(
            "this is a community model; review its publisher and explicitly acknowledge its provenance before downloading"
                .to_string(),
        );
    }
    let file = quant.files.into_iter().next().expect("len checked above");

    let dir = state.storage.base_path().join("models").join("downloaded");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // The LFS digest is a stable, filesystem-safe artifact id. Repository and
    // filename text never becomes a local path.
    let id = format!("hf-{}", &file.sha256[..16]);
    let final_path = dir.join(format!("{id}.gguf"));
    let partial = dir.join(format!("{id}.gguf.partial"));

    if let Some(existing) = state
        .storage
        .global()
        .get_model(&id)
        .map_err(|e| e.to_string())?
    {
        if existing.status == "ready" && std::path::Path::new(&existing.path).is_file() {
            return Ok(DownloadedModelInfo {
                id: existing.id,
                name: existing.name,
                path: existing.path,
                sha256: existing.sha256,
            });
        }
    }

    // Stream, emitting progress (throttling is the frontend's job).
    let id_for_progress = id.clone();
    let app_for_progress = app.clone();
    crate::models::download::download_to_partial(
        &file.url,
        &partial,
        file.size_bytes,
        move |downloaded, total| {
            let _ = app_for_progress.emit(
                "model:download-progress",
                DownloadProgress {
                    id: id_for_progress.clone(),
                    downloaded,
                    total,
                },
            );
        },
    )
    .await
    .map_err(|e| e.to_string())?;

    // Verify-or-nothing: a mismatch removes the partial + registers nothing.
    crate::models::download::verify_and_install(&partial, &final_path, &file.sha256)
        .map_err(|e| e.to_string())?;

    let model = crate::storage::ModelEntry {
        id,
        name: format!(
            "{} · {}",
            detail.id,
            quant.quant.as_deref().unwrap_or("GGUF")
        ),
        path: final_path.to_string_lossy().to_string(),
        size_bytes: file.size_bytes as i64,
        quantization: quant.quant,
        added_at: chrono::Utc::now().timestamp(),
        sha256: file.sha256,
        status: "ready".to_string(),
    };
    state
        .storage
        .global()
        .insert_model(&model)
        .map_err(|e| e.to_string())?;

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

/// Maximum bytes for a single pack-import JSON payload. Prevents OOM from
/// an oversized or malicious pack file (used by packet P13).
const PACK_IMPORT_MAX_BYTES: usize = 1_000_000;

/// Install a Capability Pack (Wave 4.5): register its skills + agent types
/// (GLOBAL) + cron jobs (this profile) at once. Everything lands INERT — skills
/// + agent types `Pending` (review in Settings → Skills / Agent types), cron
/// jobs disabled — so a pack adds capabilities to review, never arms one.
#[tauri::command]
pub fn install_pack(
    state: State<'_, AppState>,
    args: InstallPackArgs,
) -> Result<crate::packs::InstallReport, String> {
    if args.json.len() > PACK_IMPORT_MAX_BYTES {
        return Err(format!(
            "pack JSON exceeds maximum size ({} bytes)",
            PACK_IMPORT_MAX_BYTES
        ));
    }
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
    db.usage_summary()
        .map(Into::into)
        .map_err(|e| e.to_string())
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
    let cfg = crate::classifier::ClassifierConfig::from_ui(args.strictness, &args.uncertainty_band);
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

// ── C-01: observable classifier health (the degraded banner) ────────────────

/// The wire shape of [`crate::classifier::ClassifierHealth`].
#[derive(Debug, Clone, Serialize)]
pub struct ClassifierHealthInfo {
    /// `true` ⇒ the trained ONNX ensemble did not load; only the deterministic
    /// rules fallback is screening egress, and `Auto` binding refuses cloud
    /// endpoints outright. The UI MUST surface this — reduced screening that the
    /// user cannot see is the C-01 finding.
    pub degraded: bool,
    /// Why (the `EnsembleClassifier::load` error), when degraded.
    pub reason: Option<String>,
    /// How long a one-send confirmation stays usable, in seconds — the UI shows
    /// this on the confirmation affordance so "this expires" is visible.
    pub confirm_ttl_secs: u64,
}

/// Report whether egress screening is degraded, for the persistent warning
/// banner. C-01: the gate's fail-closed flag previously had zero readers; this
/// is the read side, so the user learns that the trained classifier is missing
/// instead of silently getting rules-only screening.
#[tauri::command]
pub fn get_classifier_health(state: State<'_, AppState>) -> ClassifierHealthInfo {
    ClassifierHealthInfo {
        degraded: state.gate.degraded(),
        reason: state.gate.degraded_reason(),
        confirm_ttl_secs: state.gate.confirmations().ttl().as_secs(),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConfirmPublicSendArgs {
    /// The EXACT message text the user is authorising. The grant is fingerprinted
    /// over this text, so editing the message invalidates the confirmation.
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfirmPublicSendResponse {
    /// The fingerprint the grant was filed under (debug/telemetry; the frontend
    /// does not need it — it simply re-sends the same text).
    pub fingerprint: String,
    /// Seconds until the confirmation expires.
    pub expires_in_secs: u64,
}

/// H-12: record the user's "send this once anyway" for a `Public`-bound message
/// the gate flagged with `GateDecision::ConfirmRequired`.
///
/// The authorisation is **one send** of **this exact text** and it **expires**:
/// the gate consumes it atomically on the next matching send
/// (`PublicSendConfirmations::take`), and an unused grant times out. It is
/// therefore never a persistent "Public means anything goes" allow — calling
/// this twice in a row does not authorise two sends of different content, and a
/// second send of the same content re-prompts.
#[tauri::command]
pub fn confirm_public_send(
    state: State<'_, AppState>,
    args: ConfirmPublicSendArgs,
) -> ConfirmPublicSendResponse {
    let fingerprint = state.gate.confirm_public_send(&args.text);
    ConfirmPublicSendResponse {
        fingerprint,
        expires_in_secs: state.gate.confirmations().ttl().as_secs(),
    }
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
        assert!(
            !category_display("PII_CONTACT").1,
            "contact info is not hard"
        );
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
            route_memory_sensitivity(
                &clf.classify("my api key is sk-ABCD1234efgh5678ijkl9012mnop3456")
            ),
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

fn to_memory_info(
    fact: crate::storage::MemoryFact,
    bucket: crate::storage::MemoryBucket,
) -> MemoryInfo {
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
        .map(|rows| {
            rows.into_iter()
                .map(|(f, b)| to_memory_info(f, b))
                .collect()
        })
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
    db.set_memory_settings(&settings)
        .map_err(|e| e.to_string())?;
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

// ── Gmail (the email round — per-USER OAuth client, per-PROFILE connection) ──
//
// M7-Q2 (Lukas, 2026-07-24): every user creates their OWN Google OAuth client
// through the in-app guided setup (stage 3's wizard) — no vendor client, no
// Lost Harness server in the loop. The pasted client id/secret are install-
// global; the Gmail connection (refresh token) is per-profile. All secrets
// live in the OS keychain via `AppState.provider_secrets`; SQLite never sees
// them. `NeedsReconnect` is a NORMAL state (Testing-status Google clients
// expire refresh tokens after ~7 days) — the UI renders a calm reconnect
// button, driven by `needs_reconnect` below.

/// In-flight OAuth dances + soft state for the email round.
pub struct EmailRuntime {
    /// profile → the pending loopback-listener auth (consumed by finish).
    pending:
        parking_lot::Mutex<std::collections::HashMap<String, crate::email::oauth::PendingAuth>>,
    /// The recoverable-failure state (dead grant / disabled API) for every
    /// profile. An `Arc` rather than an owned value so the SAME state can be
    /// handed to `tools::email::EmailToolDeps` (see
    /// [`EmailRuntime::with_shared_state`]) — the agent tool path and this
    /// screen IPC path must record into one place, or an agent-only failure
    /// never lights a banner.
    google: GoogleConnection,
}

/// The shared Google connection state. Aliased because it is threaded through
/// several signatures.
pub type GoogleConnection = std::sync::Arc<crate::email::connection_state::GoogleConnectionState>;

impl EmailRuntime {
    pub fn new() -> Self {
        Self::with_shared_state(std::sync::Arc::new(
            crate::email::connection_state::GoogleConnectionState::new(),
        ))
    }

    /// Build with connection state that's already shared with the agent tool
    /// path — pass the SAME `Arc` used to build `tools::email::EmailToolDeps`
    /// so a failure on either path lands in one place both paths observe.
    pub fn with_shared_state(google: GoogleConnection) -> Self {
        Self {
            pending: parking_lot::Mutex::new(std::collections::HashMap::new()),
            google,
        }
    }
}

impl Default for EmailRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// The production token endpoint, per call (cheap: one reqwest client).
fn email_endpoint() -> Result<std::sync::Arc<dyn crate::email::oauth::TokenEndpoint>, String> {
    crate::email::oauth::HttpTokenEndpoint::new()
        .map(|e| std::sync::Arc::new(e) as std::sync::Arc<dyn crate::email::oauth::TokenEndpoint>)
        .map_err(|e| e.to_string())
}

/// A per-profile Gmail client over the keychain token provider.
fn email_client(
    state: &AppState,
    profile: &str,
) -> Result<crate::email::gmail::GmailClient, String> {
    let endpoint = email_endpoint()?;
    let http = crate::email::gmail::ReqwestGmailHttp::new().map_err(|e| e.to_string())?;
    Ok(crate::email::gmail::GmailClient::new(
        Box::new(http),
        std::sync::Arc::new(crate::email::token_provider::KeychainTokenProvider::new(
            profile,
            std::sync::Arc::clone(&state.provider_secrets),
            endpoint,
        )),
    ))
}

/// A per-profile authenticated Google JSON client. Calendar and Tasks share
/// Gmail's keychain-backed OAuth session, but have their own narrow REST
/// clients and IPC surfaces.
fn google_client(
    state: &AppState,
    profile: &str,
) -> Result<crate::email::google::GoogleClient, String> {
    let endpoint = email_endpoint()?;
    crate::email::google::GoogleClient::new(Box::new(
        crate::email::token_provider::KeychainTokenProvider::new(
            profile,
            std::sync::Arc::clone(&state.provider_secrets),
            endpoint,
        ),
    ))
    .map_err(|e| e.to_string())
}

fn calendar_client(
    state: &AppState,
    profile: &str,
) -> Result<crate::email::calendar::CalendarClient, String> {
    Ok(crate::email::calendar::CalendarClient::new(google_client(
        state, profile,
    )?))
}

fn tasks_client(
    state: &AppState,
    profile: &str,
) -> Result<crate::email::tasks::TasksClient, String> {
    Ok(crate::email::tasks::TasksClient::new(google_client(
        state, profile,
    )?))
}

/// Record what a Google call PROVED about this profile's connection, then map
/// the outcome into the `Result<_, String>` the IPC layer returns.
///
/// Every Gmail/Calendar/Tasks IPC path runs through here, in both directions:
///
/// - a failure that carries a typed verdict lights the matching banner (a
///   connector call that fails with a connection-state error and leaves BOTH
///   banners dark is exactly the dead end this replaces);
/// - a SUCCESS is proof that `api` is switched on for this profile, so any
///   stale "this API is off" state for it is dropped. Without that half, the
///   banner keeps asserting something false after the user enables the API in
///   the console until they press the manual re-check.
///
/// The state is recorded from the TYPED error (`&anyhow::Error`, downcast in
/// `connection_state`), which is why this takes the error itself rather than
/// the string the command returns.
fn observe_google_call<T>(
    state: &AppState,
    profile: &str,
    api: crate::email::api_error::GoogleApi,
    outcome: anyhow::Result<T>,
) -> Result<T, String> {
    match outcome {
        Ok(value) => {
            state.email.google.observe_success(profile, api);
            Ok(value)
        }
        Err(err) => {
            state.email.google.observe_failure(profile, &err);
            Err(err.to_string())
        }
    }
}

/// The Gmail setup/connection state for one profile — everything the setup
/// wizard + Email screen need to render.
#[derive(Debug, Clone, Serialize)]
pub struct GmailSetupStatus {
    /// A GCP OAuth client id+secret are pasted (install-global).
    pub client_configured: bool,
    /// This profile holds a refresh token (is connected).
    pub connected: bool,
    /// The address this profile connected as, when known.
    pub account_email: Option<String>,
    /// The stored authorization died (expired/revoked) — show "Reconnect".
    pub needs_reconnect: bool,
    /// A Google API this profile needs is switched off in the user's own
    /// Cloud project. `None` means "not in that state"; `Some` lists them one
    /// by one, each with its own wire id and the console link Google gave for
    /// it — the screen rendering the banner can only re-test some of them, so
    /// it needs to tell them apart. A SEPARATE field from `needs_reconnect`,
    /// and it must drive a separate banner with NO Reconnect button — see
    /// `email::connection_state`.
    pub api_not_enabled: Option<crate::email::connection_state::GoogleApiDisabled>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GmailProfileArgs {
    pub profile: String,
}

#[tauri::command]
pub fn gmail_setup_status(
    state: State<'_, AppState>,
    args: GmailProfileArgs,
) -> Result<GmailSetupStatus, String> {
    crate::storage::validate_profile_name(&args.profile).map_err(|e| e.to_string())?;
    let s = &state.provider_secrets;
    let client_configured = s
        .get(crate::email::SECRET_GMAIL_CLIENT_ID)
        .map_err(|e| e.to_string())?
        .is_some()
        && s.get(crate::email::SECRET_GMAIL_CLIENT_SECRET)
            .map_err(|e| e.to_string())?
            .is_some();
    let connected = s
        .get(&crate::email::secret_gmail_refresh_token(&args.profile))
        .map_err(|e| e.to_string())?
        .is_some();
    let account_email = s
        .get(&crate::email::secret_gmail_account_email(&args.profile))
        .map_err(|e| e.to_string())?;
    let needs_reconnect = state.email.google.needs_reconnect(&args.profile);
    let api_not_enabled = state.email.google.disabled_apis(&args.profile);
    Ok(GmailSetupStatus {
        client_configured,
        connected,
        account_email,
        needs_reconnect,
        api_not_enabled,
    })
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClearApiNotEnabledArgs {
    pub profile: String,
    /// Which APIs the caller is about to re-test, by wire id ("gmail",
    /// "calendar", "tasks"). Empty means every API for this profile.
    #[serde(default)]
    pub apis: Vec<String>,
}

/// Forget the disabled-API state for the named APIs so the next call
/// re-decides.
///
/// The state clears itself when a call to that API SUCCEEDS (see
/// `observe_google_call`), which covers the ordinary "user enabled it, the app
/// tried again" path. This command exists for the other half: the remedy
/// happens OUTSIDE the app, so a profile can sit on the banner with nothing
/// retrying. The banner's "I've enabled it — check again" lands here — we
/// forget, the screen retries, and a still-disabled API re-records itself.
/// Nothing is ever assumed fixed.
///
/// Scoped to the APIs the caller names because a screen can only re-test its
/// own: Email clearing Tasks would blank a banner nothing is about to retry.
#[tauri::command]
pub fn google_clear_api_not_enabled(
    state: State<'_, AppState>,
    args: ClearApiNotEnabledArgs,
) -> Result<(), String> {
    crate::storage::validate_profile_name(&args.profile).map_err(|e| e.to_string())?;
    // An unknown name is refused rather than skipped: silently clearing
    // nothing while reporting success is how a banner survives a re-check
    // that looked like it worked.
    let apis = args
        .apis
        .iter()
        .map(|name| {
            crate::email::api_error::GoogleApi::from_wire(name).ok_or_else(|| {
                format!("unknown Google API \"{name}\" — expected gmail, calendar or tasks")
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    state.email.google.clear_disabled(&args.profile, &apis);
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetGmailClientArgs {
    pub client_id: String,
    pub client_secret: String,
}

/// Store the user's own GCP OAuth client (install-global). Minimal format
/// validation so an obviously mispasted value fails HERE with a pointer,
/// not later as an opaque Google error.
///
/// A refresh token is minted BY a specific client — swapping in a DIFFERENT
/// client orphans every profile's stored refresh token (it belongs to the
/// old client and can never be exchanged with the new one), while
/// `gmail_setup_status` would keep reporting `connected: true` on a
/// credential nothing can actually use. So a client CHANGE wipes every
/// profile's Gmail connection up front, honestly resetting status to
/// disconnected. Re-pasting the SAME client is a no-op here (nothing to
/// wipe).
#[tauri::command]
pub fn set_gmail_client(
    state: State<'_, AppState>,
    args: SetGmailClientArgs,
) -> Result<(), String> {
    let id = args.client_id.trim();
    let secret = args.client_secret.trim();
    if !id.ends_with(".apps.googleusercontent.com") || id.len() < 30 {
        return Err(
            "that doesn't look like a Google OAuth client ID (it should end with \
             .apps.googleusercontent.com) — copy it from Credentials in the Google Cloud console"
                .to_string(),
        );
    }
    if secret.is_empty() {
        return Err("the client secret is empty — copy it from the same Credentials page".into());
    }
    let s = &state.provider_secrets;
    let previous_id = s
        .get(crate::email::SECRET_GMAIL_CLIENT_ID)
        .map_err(|e| e.to_string())?;
    let client_changed = previous_id.as_deref().is_some_and(|prev| prev != id);
    if client_changed {
        if let Ok(names) = state.storage.list_profile_names() {
            for name in names {
                let _ = s.delete(&crate::email::secret_gmail_refresh_token(&name));
                let _ = s.delete(&crate::email::secret_gmail_account_email(&name));
                state.email.google.forget(&name);
                state.email.pending.lock().remove(&name);
            }
        }
    }
    s.set(crate::email::SECRET_GMAIL_CLIENT_ID, id)
        .map_err(|e| e.to_string())?;
    s.set(crate::email::SECRET_GMAIL_CLIENT_SECRET, secret)
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct GmailBeginConnect {
    pub auth_url: String,
}

/// Start the OAuth dance for a profile: bind the loopback listener, open the
/// consent URL in the system browser (macOS), and stash the pending auth for
/// `gmail_finish_connect`. Re-calling replaces any prior pending dance.
#[tauri::command]
pub async fn gmail_begin_connect(
    state: State<'_, AppState>,
    args: GmailProfileArgs,
) -> Result<GmailBeginConnect, String> {
    crate::storage::validate_profile_name(&args.profile).map_err(|e| e.to_string())?;
    let s = &state.provider_secrets;
    let (Some(client_id), Some(client_secret)) = (
        s.get(crate::email::SECRET_GMAIL_CLIENT_ID)
            .map_err(|e| e.to_string())?,
        s.get(crate::email::SECRET_GMAIL_CLIENT_SECRET)
            .map_err(|e| e.to_string())?,
    ) else {
        return Err("paste your Google OAuth client first (Settings → Email setup)".into());
    };
    let gcp = crate::email::oauth::GcpClient {
        client_id,
        client_secret,
    };
    let pending = crate::email::oauth::begin_auth(&gcp)
        .await
        .map_err(|e| e.to_string())?;
    let auth_url = pending.auth_url.clone();
    state
        .email
        .pending
        .lock()
        .insert(args.profile.clone(), pending);

    // Best-effort browser launch; the UI also shows the URL for copy/paste
    // (the honest fallback on other OSes or if `open` fails).
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("/usr/bin/open")
            .arg(&auth_url)
            .spawn();
    }

    Ok(GmailBeginConnect { auth_url })
}

#[derive(Debug, Clone, Serialize)]
pub struct GmailConnected {
    /// `None` when the post-connect profile lookup failed — never a
    /// fabricated placeholder address. The UI falls back to a generic
    /// "your Gmail account" label when this is absent.
    pub account_email: Option<String>,
}

/// Await the browser redirect, exchange the code, persist the credential, and
/// capture the connected address. Blocks until the user finishes consent in
/// the browser (bounded by the flow's 5-minute timeout).
#[tauri::command]
pub async fn gmail_finish_connect(
    state: State<'_, AppState>,
    args: GmailProfileArgs,
) -> Result<GmailConnected, String> {
    crate::storage::validate_profile_name(&args.profile).map_err(|e| e.to_string())?;
    let pending = state
        .email
        .pending
        .lock()
        .remove(&args.profile)
        .ok_or_else(|| "no connection attempt in progress — click Connect first".to_string())?;
    let s = &state.provider_secrets;
    let (Some(client_id), Some(client_secret)) = (
        s.get(crate::email::SECRET_GMAIL_CLIENT_ID)
            .map_err(|e| e.to_string())?,
        s.get(crate::email::SECRET_GMAIL_CLIENT_SECRET)
            .map_err(|e| e.to_string())?,
    ) else {
        return Err("the Google OAuth client was removed mid-connect — start over".into());
    };
    let gcp = crate::email::oauth::GcpClient {
        client_id,
        client_secret,
    };
    let endpoint = email_endpoint()?;
    let tokens = pending
        .finish(endpoint.as_ref(), &gcp)
        .await
        .map_err(|e| e.to_string())?;
    let refresh = tokens.refresh_token.clone().ok_or_else(|| {
        "Google didn't return a refresh token — remove the app's access at \
             myaccount.google.com/permissions and connect again"
            .to_string()
    })?;
    s.set(
        &crate::email::secret_gmail_refresh_token(&args.profile),
        &refresh,
    )
    .map_err(|e| e.to_string())?;

    // One profile call with the fresh access token to capture the address.
    struct OneShot(String);
    impl crate::email::gmail::TokenProvider for OneShot {
        fn access_token(
            &self,
            _force: bool,
        ) -> crate::email::BoxFuture<'_, anyhow::Result<String>> {
            let t = self.0.clone();
            Box::pin(async move { Ok(t) })
        }
    }
    let http = crate::email::gmail::ReqwestGmailHttp::new().map_err(|e| e.to_string())?;
    let client = crate::email::gmail::GmailClient::new(
        Box::new(http),
        Arc::new(OneShot(tokens.access_token.clone())),
    );
    // Best-effort address capture. On failure, store NOTHING — a fabricated
    // "connected" placeholder would be a fake identity persisted as if it
    // were real. A missing address is an honest, UI-tolerated state.
    let account_email = client.get_profile().await.ok();
    if let Some(email) = &account_email {
        let _ = s.set(
            &crate::email::secret_gmail_account_email(&args.profile),
            email,
        );
    }
    state.email.google.clear_needs_reconnect(&args.profile);
    // The disabled-API state is deliberately NOT cleared here. A reconnect
    // re-consents scopes against the SAME Cloud project, so a disabled API is
    // still disabled; clearing would blank the banner and imply the reconnect
    // fixed something it cannot fix. What DOES clear it is a call to that API
    // actually succeeding (`observe_google_call`) — evidence, not a hope.
    Ok(GmailConnected { account_email })
}

/// Disconnect a profile's Gmail: delete its keychain credentials. The
/// install-global client id/secret stay (other profiles may use them).
#[tauri::command]
pub fn gmail_disconnect(state: State<'_, AppState>, args: GmailProfileArgs) -> Result<(), String> {
    crate::storage::validate_profile_name(&args.profile).map_err(|e| e.to_string())?;
    let s = &state.provider_secrets;
    let _ = s.delete(&crate::email::secret_gmail_refresh_token(&args.profile));
    let _ = s.delete(&crate::email::secret_gmail_account_email(&args.profile));
    // The whole connection is gone, so every soft state about it is stale —
    // including the disabled-API one (a fresh connect may target a different
    // Cloud project entirely).
    state.email.google.forget(&args.profile);
    state.email.pending.lock().remove(&args.profile);
    Ok(())
}

/// One inbox row for the Email screen.
#[derive(Debug, Clone, Serialize)]
pub struct EmailSummary {
    pub id: String,
    pub from: String,
    pub subject: String,
    pub date: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListEmailArgs {
    pub profile: String,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub max: Option<u32>,
}

/// The Email screen's inbox read (human-initiated; the agent path is the
/// gated `email_search` tool).
#[tauri::command]
pub async fn list_email(
    state: State<'_, AppState>,
    args: ListEmailArgs,
) -> Result<Vec<EmailSummary>, String> {
    crate::storage::validate_profile_name(&args.profile).map_err(|e| e.to_string())?;
    let max = args.max.unwrap_or(15).clamp(1, 15);
    let client = email_client(&state, &args.profile)?;
    let metas = observe_google_call(
        &state,
        &args.profile,
        GoogleApi::Gmail,
        client.list_messages(args.query.as_deref(), max).await,
    )?;
    let mut rows = Vec::new();
    let mut last_err: Option<anyhow::Error> = None;
    for meta in metas.iter().take(max as usize) {
        match client.get_message(&meta.id).await {
            Ok(m) => rows.push(EmailSummary {
                id: m.id,
                from: m.from,
                subject: m.subject,
                date: m.date,
                snippet: m.snippet,
            }),
            // One bad message shouldn't sink the whole list — but if EVERY
            // one fails (below), that's not "an empty inbox", it's a dead
            // token mid-loop, and reporting it that way would be a lie. The
            // ERROR is kept, not its text: the verdict rides on the value.
            Err(e) => last_err = Some(e),
        }
    }
    if rows.is_empty() && !metas.is_empty() {
        let err = last_err.unwrap_or_else(|| anyhow::anyhow!("every message fetch failed"));
        // Same typed recorder, straight: there is no success half to observe
        // here (the listing above already cleared Gmail if it worked).
        state.email.google.observe_failure(&args.profile, &err);
        return Err(err.to_string());
    }
    Ok(rows)
}

/// One full message for the Email screen's reading pane.
#[derive(Debug, Clone, Serialize)]
pub struct EmailDetail {
    pub id: String,
    pub from: String,
    pub to: String,
    pub subject: String,
    pub date: String,
    pub body: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReadEmailArgs {
    pub profile: String,
    pub id: String,
}

#[tauri::command]
pub async fn read_email(
    state: State<'_, AppState>,
    args: ReadEmailArgs,
) -> Result<EmailDetail, String> {
    crate::storage::validate_profile_name(&args.profile).map_err(|e| e.to_string())?;
    let client = email_client(&state, &args.profile)?;
    let m = observe_google_call(
        &state,
        &args.profile,
        GoogleApi::Gmail,
        client.get_message(&args.id).await,
    )?;
    Ok(EmailDetail {
        id: m.id,
        from: m.from,
        to: m.to,
        subject: m.subject,
        date: m.date,
        body: m.body_text,
    })
}

#[derive(Debug, Clone, Deserialize)]
pub struct SendEmailArgs {
    pub profile: String,
    pub to: String,
    pub subject: String,
    pub body: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EmailSent {
    pub id: String,
}

/// The compose pane's send (human clicked Send — that click IS the consent;
/// the agent path is the Dangerous `email_send` tool with its own Ask).
#[tauri::command]
pub async fn send_email(
    state: State<'_, AppState>,
    args: SendEmailArgs,
) -> Result<EmailSent, String> {
    crate::storage::validate_profile_name(&args.profile).map_err(|e| e.to_string())?;
    let raw = crate::email::gmail::build_rfc822(&args.to, &args.subject, &args.body)
        .map_err(|e| e.to_string())?;
    let client = email_client(&state, &args.profile)?;
    let id = observe_google_call(
        &state,
        &args.profile,
        GoogleApi::Gmail,
        client.send(&raw).await,
    )?;
    Ok(EmailSent { id })
}

// ── Google Calendar + Tasks (same per-profile OAuth connection as Gmail) ───

#[derive(Debug, Clone, Serialize)]
pub struct CalendarEventInfo {
    pub id: String,
    pub summary: String,
    pub description: String,
    pub start: String,
    pub end: String,
    pub all_day: bool,
}

impl From<crate::email::calendar::CalendarEvent> for CalendarEventInfo {
    fn from(event: crate::email::calendar::CalendarEvent) -> Self {
        Self {
            id: event.id,
            summary: event.summary,
            description: event.description,
            start: event.start,
            end: event.end,
            all_day: event.all_day,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListCalendarEventsArgs {
    pub profile: String,
    /// RFC 3339 range start. Omitted means now.
    #[serde(default)]
    pub from: Option<String>,
    /// RFC 3339 range end. Omitted means seven days after `from`.
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub max: Option<u32>,
}

fn parse_calendar_range(
    args: &ListCalendarEventsArgs,
) -> Result<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>), String> {
    let from = args
        .from
        .as_deref()
        .map(|v| chrono::DateTime::parse_from_rfc3339(v).map(|d| d.with_timezone(&chrono::Utc)))
        .transpose()
        .map_err(|_| "calendar range start must be an RFC 3339 timestamp".to_string())?
        .unwrap_or_else(chrono::Utc::now);
    let to = args
        .to
        .as_deref()
        .map(|v| chrono::DateTime::parse_from_rfc3339(v).map(|d| d.with_timezone(&chrono::Utc)))
        .transpose()
        .map_err(|_| "calendar range end must be an RFC 3339 timestamp".to_string())?
        .unwrap_or_else(|| from + chrono::Duration::days(7));
    if to <= from {
        return Err("calendar range end must be after its start".to_string());
    }
    Ok((from, to))
}

#[tauri::command]
pub async fn list_calendar_events(
    state: State<'_, AppState>,
    args: ListCalendarEventsArgs,
) -> Result<Vec<CalendarEventInfo>, String> {
    crate::storage::validate_profile_name(&args.profile).map_err(|e| e.to_string())?;
    let (from, to) = parse_calendar_range(&args)?;
    let client = calendar_client(&state, &args.profile)?;
    observe_google_call(
        &state,
        &args.profile,
        GoogleApi::Calendar,
        client.list_upcoming(from, to, args.max.unwrap_or(30)).await,
    )
    .map(|events| events.into_iter().map(Into::into).collect())
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateCalendarEventArgs {
    pub profile: String,
    pub summary: String,
    #[serde(default)]
    pub description: String,
    pub start: String,
    pub end: String,
}

#[tauri::command]
pub async fn create_calendar_event(
    state: State<'_, AppState>,
    args: CreateCalendarEventArgs,
) -> Result<CalendarEventInfo, String> {
    crate::storage::validate_profile_name(&args.profile).map_err(|e| e.to_string())?;
    let client = calendar_client(&state, &args.profile)?;
    observe_google_call(
        &state,
        &args.profile,
        GoogleApi::Calendar,
        client
            .create(&args.summary, &args.description, &args.start, &args.end)
            .await,
    )
    .map(Into::into)
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeleteCalendarEventArgs {
    pub profile: String,
    pub id: String,
}

#[tauri::command]
pub async fn delete_calendar_event(
    state: State<'_, AppState>,
    args: DeleteCalendarEventArgs,
) -> Result<(), String> {
    crate::storage::validate_profile_name(&args.profile).map_err(|e| e.to_string())?;
    observe_google_call(
        &state,
        &args.profile,
        GoogleApi::Calendar,
        calendar_client(&state, &args.profile)?
            .delete(&args.id)
            .await,
    )
}

#[derive(Debug, Clone, Serialize)]
pub struct GoogleTaskInfo {
    pub id: String,
    pub title: String,
    pub notes: String,
    pub due: Option<String>,
    pub completed: bool,
}

impl From<crate::email::tasks::Task> for GoogleTaskInfo {
    fn from(task: crate::email::tasks::Task) -> Self {
        Self {
            id: task.id,
            title: task.title,
            notes: task.notes,
            due: task.due,
            completed: task.completed,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListGoogleTasksArgs {
    pub profile: String,
    #[serde(default)]
    pub max: Option<u32>,
}

#[tauri::command]
pub async fn list_google_tasks(
    state: State<'_, AppState>,
    args: ListGoogleTasksArgs,
) -> Result<Vec<GoogleTaskInfo>, String> {
    crate::storage::validate_profile_name(&args.profile).map_err(|e| e.to_string())?;
    observe_google_call(
        &state,
        &args.profile,
        GoogleApi::Tasks,
        tasks_client(&state, &args.profile)?
            .list(args.max.unwrap_or(50))
            .await,
    )
    .map(|tasks| tasks.into_iter().map(Into::into).collect())
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateGoogleTaskArgs {
    pub profile: String,
    pub title: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub due: Option<String>,
}

#[tauri::command]
pub async fn create_google_task(
    state: State<'_, AppState>,
    args: CreateGoogleTaskArgs,
) -> Result<GoogleTaskInfo, String> {
    crate::storage::validate_profile_name(&args.profile).map_err(|e| e.to_string())?;
    observe_google_call(
        &state,
        &args.profile,
        GoogleApi::Tasks,
        tasks_client(&state, &args.profile)?
            .create(&args.title, &args.notes, args.due.as_deref())
            .await,
    )
    .map(Into::into)
}

#[derive(Debug, Clone, Deserialize)]
pub struct SetGoogleTaskCompletedArgs {
    pub profile: String,
    pub id: String,
    pub completed: bool,
}

#[tauri::command]
pub async fn set_google_task_completed(
    state: State<'_, AppState>,
    args: SetGoogleTaskCompletedArgs,
) -> Result<GoogleTaskInfo, String> {
    crate::storage::validate_profile_name(&args.profile).map_err(|e| e.to_string())?;
    observe_google_call(
        &state,
        &args.profile,
        GoogleApi::Tasks,
        tasks_client(&state, &args.profile)?
            .set_completed(&args.id, args.completed)
            .await,
    )
    .map(Into::into)
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeleteGoogleTaskArgs {
    pub profile: String,
    pub id: String,
}

#[tauri::command]
pub async fn delete_google_task(
    state: State<'_, AppState>,
    args: DeleteGoogleTaskArgs,
) -> Result<(), String> {
    crate::storage::validate_profile_name(&args.profile).map_err(|e| e.to_string())?;
    observe_google_call(
        &state,
        &args.profile,
        GoogleApi::Tasks,
        tasks_client(&state, &args.profile)?.delete(&args.id).await,
    )
}

// ── workspace files (the Files screen's read-only browser) ─────────────────
//
// READ-ONLY listing of the profile's Tier-P workspace subtree — the same tree
// the agent's fs tools write into (`<base>/workspace/<profile>`). No content
// read, no mutation: the write path stays exclusively behind the gated fs
// tools. Confinement mirrors the fs tools: the validated profile name maps
// through `profile_workspace_path`, and the optional subpath is rejected on
// any traversal/absolute component, then canonicalize-checked to stay inside
// the workspace (a symlinked dir can't walk the listing out of the tree).

/// One workspace entry, for the Files screen.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceEntry {
    pub name: String,
    pub is_dir: bool,
    pub size_bytes: i64,
    /// Seconds since epoch; None when the metadata read failed.
    pub modified_at: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListWorkspaceFilesArgs {
    pub profile: String,
    /// Relative directory within the workspace ("" = the root).
    #[serde(default)]
    pub subpath: String,
}

/// List one directory of this profile's workspace (read-only).
#[tauri::command]
pub fn list_workspace_files(
    state: State<'_, AppState>,
    args: ListWorkspaceFilesArgs,
) -> Result<Vec<WorkspaceEntry>, String> {
    crate::storage::validate_profile_name(&args.profile).map_err(|e| e.to_string())?;
    let ws_root = state.storage.base_path().join("workspace");
    let ws = crate::tools::fs::profile_workspace_path(&ws_root, &args.profile);
    if ws == ws_root {
        // The resolver only falls back to the shared root on a hostile
        // profile string; validate_profile_name should have caught it.
        return Err("invalid profile for workspace listing".into());
    }
    std::fs::create_dir_all(&ws).map_err(|e| e.to_string())?;

    // Reject traversal in the subpath BEFORE touching the filesystem.
    let sub = args.subpath.trim_matches('/');
    if sub.split('/').any(|c| c == ".." || c.starts_with('\\')) || sub.starts_with('/') {
        return Err("invalid subpath".into());
    }
    let dir = if sub.is_empty() {
        ws.clone()
    } else {
        ws.join(sub)
    };

    // Canonicalize-confine: the listed dir must still live under the
    // workspace after symlink resolution.
    let canon_ws = ws.canonicalize().map_err(|e| e.to_string())?;
    let canon_dir = dir
        .canonicalize()
        .map_err(|_| "no such folder in this profile's workspace".to_string())?;
    if !canon_dir.starts_with(&canon_ws) {
        return Err("path escapes the profile workspace".into());
    }

    let mut entries: Vec<WorkspaceEntry> = Vec::new();
    for entry in std::fs::read_dir(&canon_dir).map_err(|e| e.to_string())? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        // The Tier-P migration marker is plumbing, not user data.
        if name == crate::tools::fs::LEGACY_MIGRATION_MARKER {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        entries.push(WorkspaceEntry {
            name,
            is_dir: meta.is_dir(),
            size_bytes: if meta.is_dir() { 0 } else { meta.len() as i64 },
            modified_at: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64),
        });
    }
    // Dirs first, then case-insensitive by name — stable browsing order.
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── H-07: the MCP install consent gate ──────────────────────────────────
    //
    // These drive `consume_mcp_install_nonce` / `issue_mcp_install_nonce`
    // directly — the gate `register_mcp_server` runs before anything else —
    // to cover the TTL/expiry edges cheaply. The command boundary itself
    // (missing / forged / replayed nonce through `generate_handler!` and a real
    // `AppState`) is covered in `ipc::contract_tests`.

    fn nonce_store() -> Mutex<std::collections::HashMap<String, Instant>> {
        Mutex::new(std::collections::HashMap::new())
    }

    /// H-07 gap (b): a renderer that calls `register_mcp_server` without going
    /// through the consent step has no nonce to present, and is refused.
    #[test]
    fn registering_without_a_nonce_fails() {
        let store = nonce_store();
        // The forged call: empty / made-up nonce, never issued by the backend.
        for forged in [
            "",
            "not-a-real-nonce",
            "00000000-0000-0000-0000-000000000000",
        ] {
            let err = consume_mcp_install_nonce(&store, forged)
                .expect_err("an unissued nonce must be rejected");
            assert!(err.contains("not confirmed"), "got: {err}");
        }
        // Control: a nonce the backend actually issued is accepted.
        let good = issue_mcp_install_nonce(&store);
        consume_mcp_install_nonce(&store, &good).expect("an issued nonce must pass");
    }

    /// Single use: replaying a captured nonce is refused.
    #[test]
    fn an_install_nonce_cannot_be_replayed() {
        let store = nonce_store();
        let n = issue_mcp_install_nonce(&store);
        consume_mcp_install_nonce(&store, &n).expect("first use passes");
        let err = consume_mcp_install_nonce(&store, &n).expect_err("second use must fail");
        assert!(err.contains("not confirmed"), "got: {err}");
    }

    /// A nonce older than the TTL is refused, and consuming it clears it.
    #[test]
    fn an_expired_install_nonce_fails() {
        let store = nonce_store();
        let stale = "stale-nonce".to_string();
        let issued_at = Instant::now() - (MCP_INSTALL_NONCE_TTL + Duration::from_secs(1));
        store.lock().unwrap().insert(stale.clone(), issued_at);
        let err = consume_mcp_install_nonce(&store, &stale).expect_err("expired must fail");
        assert!(err.contains("expired"), "got: {err}");
        assert!(
            store.lock().unwrap().is_empty(),
            "a rejected nonce must not linger in the store"
        );
    }

    /// One issued nonce does not authorize a second install.
    #[test]
    fn each_install_needs_its_own_nonce() {
        let store = nonce_store();
        let a = issue_mcp_install_nonce(&store);
        let b = issue_mcp_install_nonce(&store);
        assert_ne!(a, b, "nonces must be unique per issue");
        consume_mcp_install_nonce(&store, &a).expect("first install");
        consume_mcp_install_nonce(&store, &b).expect("second install");
        assert!(store.lock().unwrap().is_empty());
    }

    /// `RegisterMcpServerArgs` has no `#[serde(default)]` on `nonce`, so a
    /// renderer payload that simply omits it fails to deserialize at the IPC
    /// boundary — the call never reaches the command body at all.
    #[test]
    fn register_args_without_a_nonce_do_not_deserialize() {
        let no_nonce = serde_json::json!({ "name": "srv", "command": "echo" });
        assert!(
            serde_json::from_value::<RegisterMcpServerArgs>(no_nonce).is_err(),
            "an args payload with no nonce field must be rejected by serde"
        );
        let with_nonce = serde_json::json!({ "name": "srv", "command": "echo", "nonce": "n" });
        let parsed: RegisterMcpServerArgs =
            serde_json::from_value(with_nonce).expect("a payload carrying a nonce parses");
        assert_eq!(parsed.nonce, "n");
    }

    #[test]
    fn app_version_is_from_cargo() {
        assert_eq!(get_app_version(), env!("CARGO_PKG_VERSION"));
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
        let rows = vec![
            msg("u1", "user", None),
            msg("a1", "assistant", Some("route_local")),
        ];
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
        assert_eq!(
            id, "a2",
            "must pick the newest assistant row, not the first"
        );
        assert_eq!(decision, "route_local");
    }

    #[test]
    fn latest_assistant_routing_is_none_without_an_assistant_row() {
        let rows = vec![msg("u1", "user", None)];
        assert!(latest_assistant_routing(&rows).is_none());
    }

    // ── M-22: send_message reports the read-back honestly ──────────────

    #[test]
    fn routing_lookup_resolves_a_persisted_assistant_row() {
        let rows = vec![msg("a1", "assistant", Some("route_local"))];
        assert_eq!(
            routing_lookup(Ok(rows)),
            RoutingLookup::Resolved {
                message_id: "a1".to_string(),
                routing_decision: "route_local".to_string(),
            }
        );
    }

    #[test]
    fn routing_lookup_distinguishes_every_way_the_read_back_can_fail() {
        // The bug this replaced flattened all three of these into
        // `("", "unknown")` inside an Ok payload. Each must stay its own
        // reportable reason.
        assert_eq!(
            routing_lookup(Ok(vec![msg("u1", "user", None)])),
            RoutingLookup::Unavailable(RoutingUnavailable::NoAssistantRow)
        );
        assert_eq!(
            routing_lookup(Err(RoutingUnavailable::ProfileDbUnavailable)),
            RoutingLookup::Unavailable(RoutingUnavailable::ProfileDbUnavailable)
        );
        assert_eq!(
            routing_lookup(Err(RoutingUnavailable::MessageQueryFailed)),
            RoutingLookup::Unavailable(RoutingUnavailable::MessageQueryFailed)
        );
    }

    /// Build the response exactly the way `send_message` does — same
    /// `into_parts` call, not a re-implementation of it — so the serialized
    /// shapes asserted below are the shapes the frontend actually receives.
    fn response_for(lookup: RoutingLookup) -> SendMessageResponse {
        let (message_id, routing_decision, routing_unavailable) = lookup.into_parts();
        SendMessageResponse {
            message_id,
            content: "hi".to_string(),
            conversation_id: "c1".to_string(),
            profile: "personal".to_string(),
            routing_decision,
            routing_unavailable,
            completed_at: 0,
        }
    }

    #[test]
    fn a_degraded_send_response_never_fabricates_a_routing_decision() {
        let json = serde_json::to_value(response_for(RoutingLookup::Unavailable(
            RoutingUnavailable::ProfileDbUnavailable,
        )))
        .unwrap();
        // No placeholder id, no invented decision — both explicitly null.
        assert_eq!(json["message_id"], serde_json::Value::Null);
        assert_eq!(json["routing_decision"], serde_json::Value::Null);
        // And the failure is named, so the UI can suppress the routing badge
        // on purpose instead of rendering a decision nobody made.
        assert_eq!(json["routing_unavailable"], "profile_db_unavailable");
        let raw = serde_json::to_string(&response_for(RoutingLookup::Unavailable(
            RoutingUnavailable::NoAssistantRow,
        )))
        .unwrap();
        assert!(
            !raw.contains("unknown"),
            "the old fabricated \"unknown\" decision is back: {raw}"
        );
    }

    #[test]
    fn a_resolved_send_response_carries_the_decision_and_omits_the_failure_field() {
        let json = serde_json::to_value(response_for(RoutingLookup::Resolved {
            message_id: "a1".to_string(),
            routing_decision: "route_local".to_string(),
        }))
        .unwrap();
        assert_eq!(json["message_id"], "a1");
        assert_eq!(json["routing_decision"], "route_local");
        assert!(
            json.get("routing_unavailable").is_none(),
            "happy path must not carry a failure field: {json}"
        );
    }

    // ── H-06: cleartext is gated on the ADDRESS, not on a string prefix ──

    #[test]
    fn cleartext_http_is_refused_for_hosts_that_only_look_local() {
        // Every one of these passed the old `host.starts_with("127.")` /
        // `"localhost"` string tests or is the same class of trick, and every
        // one resolves through public DNS. Accepting any of them ships the
        // provider's bearer key over plaintext HTTP to a stranger.
        for hostile in [
            "http://127.evil.com/v1",
            "http://127.0.0.1.evil.com/v1",
            "http://localhost.evil.com/v1",
            "http://10.evil.com/v1",
            "http://192.168.1.1.evil.com/v1",
            "http://172.16.evil.com/v1",
            "http://100.64.evil.com/v1",
            "http://evil.com/127.0.0.1/v1",
        ] {
            let err = match validate_base_url(hostile) {
                Ok(()) => panic!("hostile lookalike host was ACCEPTED: {hostile}"),
                Err(e) => e,
            };
            assert!(
                err.contains("cleartext"),
                "expected the cleartext refusal for {hostile}, got: {err}"
            );
        }
    }

    #[test]
    fn cleartext_http_is_allowed_for_real_loopback_and_lan_addresses() {
        for ok in [
            "http://localhost:1234/v1",
            "http://LocalHost:1234/v1",
            "http://127.0.0.1:1234/v1",
            "http://127.9.9.9:1234/v1",
            // Decimal form of 127.0.0.1. No string test can see this; only
            // parsing the host as an address does.
            "http://2130706433:1234/v1",
            "http://[::1]:8080/v1",
            "http://10.0.0.100:8000/v1",
            "http://192.168.1.50:11434/v1",
            "http://172.20.4.4:8000/v1",
            "http://169.254.7.7:8000/v1",
            "http://100.85.52.127:8000/v1",
        ] {
            assert!(
                validate_base_url(ok).is_ok(),
                "local/LAN endpoint must stay addable over http: {ok} — {:?}",
                validate_base_url(ok)
            );
        }
    }

    #[test]
    fn public_endpoints_must_use_https_and_https_is_always_accepted() {
        for public_cleartext in [
            "http://api.openai.com/v1",
            // 8.8.8.8 and 172.32.x are outside every private range — the
            // boundary just past 172.16/12.
            "http://8.8.8.8/v1",
            "http://172.32.0.1/v1",
            "http://11.0.0.1/v1",
        ] {
            assert!(
                validate_base_url(public_cleartext).is_err(),
                "public cleartext endpoint must be refused: {public_cleartext}"
            );
        }
        for tls in [
            "https://api.openai.com/v1",
            "https://openrouter.ai/api/v1",
            // TLS to a lookalike name is fine — the key is encrypted and the
            // certificate is the server's problem, not ours.
            "https://127.evil.com/v1",
        ] {
            assert!(
                validate_base_url(tls).is_ok(),
                "https endpoint must be accepted: {tls}"
            );
        }
    }

    #[test]
    fn base_url_rejects_credentials_fragments_and_odd_schemes() {
        assert!(validate_base_url("https://user:pass@api.openai.com/v1").is_err());
        assert!(validate_base_url("https://api.openai.com/v1#frag").is_err());
        assert!(validate_base_url("ftp://api.openai.com/v1").is_err());
        assert!(validate_base_url("file:///etc/passwd").is_err());
        assert!(validate_base_url("not a url").is_err());
    }

    // ── H-06 (client side): a redirect must never carry the bearer key ──

    /// One-shot HTTP server on an ephemeral loopback port. Answers the first
    /// request with `raw` and closes. Returns the bound port.
    fn one_shot_server(raw: &'static str) -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf);
                let _ = sock.write_all(raw.as_bytes());
                let _ = sock.flush();
            }
        });
        port
    }

    #[tokio::test]
    async fn the_model_client_does_not_follow_redirects() {
        // A 302 is how a compromised or misconfigured endpoint moves an
        // authenticated request somewhere else; reqwest's default policy would
        // replay it (up to 10 hops). `ModelClient::new` disables redirects, so
        // the 302 must surface as the error instead of being followed to the
        // destination — which here would answer with a valid model list and
        // make `list_models` succeed.
        let destination = one_shot_server(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 30\r\nConnection: close\r\n\r\n{\"data\":[{\"id\":\"redirected\"}]}",
        );
        let hop = one_shot_server_redirecting_to(destination);
        let provider = Provider::new(
            "p1",
            "Hop",
            format!("http://127.0.0.1:{hop}"),
            Some("sk-secret".into()),
            ProviderKind::Local,
        );
        let client = crate::models::client::ModelClient::new(provider).expect("build client");
        let err = client
            .list_models()
            .await
            .expect_err("a 302 must not be followed");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("302"),
            "expected the redirect to surface as a 302 error, got: {msg}"
        );
        assert!(
            !msg.contains("redirected"),
            "the redirect was followed to its destination: {msg}"
        );
    }

    /// A server that 302s to `port`. Separate from `one_shot_server` because
    /// the Location header is built at runtime.
    fn one_shot_server_redirecting_to(port: u16) -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let hop = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/models\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = sock.write_all(resp.as_bytes());
                let _ = sock.flush();
            }
        });
        hop
    }

    // ── M-05 / rotation: provider mutations against a real AppState ─────

    use crate::secrets::ProviderSecretStore as _; // get/set/delete on the doubles

    /// Credential-store double that also records deletions, so a test can
    /// assert the *compensating* delete happened (and against which id) —
    /// `secrets::MemoryProviderSecretStore` only exposes the surviving values.
    #[derive(Default)]
    struct RecordingSecretStore {
        values: std::sync::Mutex<std::collections::HashMap<String, String>>,
        deleted: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingSecretStore {
        /// Ids that currently hold a secret.
        fn live_ids(&self) -> Vec<String> {
            let mut ids: Vec<String> = self.values.lock().unwrap().keys().cloned().collect();
            ids.sort();
            ids
        }

        fn deleted_ids(&self) -> Vec<String> {
            self.deleted.lock().unwrap().clone()
        }
    }

    impl crate::secrets::ProviderSecretStore for RecordingSecretStore {
        fn get(&self, endpoint_id: &str) -> Result<Option<String>, String> {
            Ok(self.values.lock().unwrap().get(endpoint_id).cloned())
        }
        fn set(&self, endpoint_id: &str, secret: &str) -> Result<(), String> {
            self.values
                .lock()
                .unwrap()
                .insert(endpoint_id.to_string(), secret.to_string());
            Ok(())
        }
        fn delete(&self, endpoint_id: &str) -> Result<(), String> {
            self.values.lock().unwrap().remove(endpoint_id);
            self.deleted.lock().unwrap().push(endpoint_id.to_string());
            Ok(())
        }
    }

    /// A minimal but real `AppState`: on-disk temp storage, empty
    /// `ModelManager`, recording secret store. No Tauri app — the command
    /// bodies live in `*_inner(&AppState, ..)` precisely so this works.
    fn test_state() -> (AppState, Arc<RecordingSecretStore>) {
        let mut dir = std::env::temp_dir();
        dir.push(format!("lhp-ipc-p05-{}", Uuid::new_v4()));
        let storage = Arc::new(Storage::open(&dir).expect("open temp storage"));
        let model_manager = Arc::new(ModelManager::new());
        let secrets = Arc::new(RecordingSecretStore::default());
        let tools = Arc::new(crate::tools::ToolDispatcher::empty());
        let gate = crate::agent::gate::PrivacyGate::new(Arc::new(
            crate::classifier::HeuristicClassifier::new(),
        ));
        let agent_loop = Arc::new(AgentLoop::new(
            // Clone, so `AppState.gate` below is the SAME gate the loop enforces
            // with — a PrivacyGate clone shares its `Arc`s (C-01/H-12). Handing
            // the loop a separate gate is what left the degraded flag with zero
            // observable call sites in the first place.
            gate.clone(),
            Arc::clone(&model_manager),
            Arc::clone(&storage),
            Arc::clone(&tools),
        ));
        let state = AppState {
            agent_loop,
            email: Arc::new(EmailRuntime::new()),
            model_manager,
            storage,
            provider_secrets: Arc::clone(&secrets) as Arc<dyn crate::secrets::ProviderSecretStore>,
            approvals: Arc::new(ApprovalRegistry::new()),
            ask_human: Arc::new(AskHumanRegistry::new()),
            classifier: Arc::new(crate::classifier::HeuristicClassifier::new()),
            gate,
            embedder: None,
            tools,
            mcp: Arc::new(crate::tools::mcp_stdio::McpRuntime::new()),
            hardware: Arc::new(Default::default()),
            #[cfg(feature = "local-runner")]
            local_runner: None,
            // H-07: MCP install nonces — empty, like a fresh boot.
            pending_mcp_nonces: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        };
        (state, secrets)
    }

    /// The banner-flip contract at the layer that owns the banners, driven
    /// through the SAME seam every Gmail/Calendar/Tasks command uses.
    ///
    /// Two 403s, two states, and neither may stand in for the other. If a
    /// disabled-API 403 flipped `needs_reconnect`, the user would be offered
    /// Reconnect, complete the whole OAuth dance, fail identically, and be
    /// offered Reconnect again — forever. Driven with REAL classifier output
    /// (`google_api_error`), so the wiring from response body to state is
    /// what's under test — and the classification travels as the typed value,
    /// which is why this hands `observe_google_call` an ERROR and not a string.
    #[test]
    fn the_two_403s_flip_two_different_states_and_never_each_others() {
        use crate::email::api_error::google_api_error;
        const CONSOLE: &str =
            "https://console.developers.google.com/apis/api/tasks.googleapis.com/overview?project=9";

        // 1. Scope-short grant → reconnect state ONLY.
        let (state, _secrets) = test_state();
        let scope: Result<(), String> = observe_google_call(
            &state,
            "personal",
            GoogleApi::Calendar,
            Err(google_api_error(
                GoogleApi::Calendar,
                403,
                r#"{"error":{"code":403,"status":"PERMISSION_DENIED","details":[
                {"@type":"type.googleapis.com/google.rpc.ErrorInfo",
                 "reason":"ACCESS_TOKEN_SCOPE_INSUFFICIENT"}]}}"#,
                "snip",
            )),
        );
        assert!(scope.is_err(), "the call still fails for the caller");
        assert!(state.email.google.needs_reconnect("personal"));
        assert_eq!(
            state.email.google.disabled_apis("personal"),
            None,
            "a scope-short grant is not a disabled API"
        );

        // 2. Disabled API → the disabled state, with Google's link, naming the
        //    API that failed, and the reconnect flag untouched.
        let (state, _secrets) = test_state();
        let _: Result<(), String> = observe_google_call(
            &state,
            "personal",
            GoogleApi::Tasks,
            Err(google_api_error(
                GoogleApi::Tasks,
                403,
                &format!(
                    r#"{{"error":{{"errors":[{{"reason":"accessNotConfigured",
                "message":"Access Not Configured.","extendedHelp":"{CONSOLE}"}}],"code":403}}}}"#
                ),
                "snip",
            )),
        );
        assert!(
            !state.email.google.needs_reconnect("personal"),
            "reconnecting can never enable a disabled API — this must NOT light \
             the reconnect banner, or the user loops forever"
        );
        assert_eq!(
            state.email.google.disabled_apis("personal"),
            Some(crate::email::connection_state::GoogleApiDisabled {
                apis: vec![crate::email::connection_state::DisabledApi {
                    id: "tasks",
                    label: "Google Tasks",
                    console_url: Some(CONSOLE.to_string()),
                }],
            })
        );
        // Per-profile, like every other part of the connection state.
        assert_eq!(state.email.google.disabled_apis("work"), None);

        // 3. A SUCCESSFUL Tasks call is the proof that clears it — no manual
        //    re-check needed once the user actually enables the API. A Gmail
        //    success would not have been proof about Tasks.
        let _ = observe_google_call(&state, "personal", GoogleApi::Gmail, Ok(()));
        assert!(state.email.google.disabled_apis("personal").is_some());
        let _ = observe_google_call(&state, "personal", GoogleApi::Tasks, Ok(()));
        assert_eq!(state.email.google.disabled_apis("personal"), None);

        // 4. An unmatched 403 lights nothing at all — the "NEVER silently
        //    reclassify an unknown 403" rule, at the state layer.
        let (state, _secrets) = test_state();
        let _: Result<(), String> = observe_google_call(
            &state,
            "personal",
            GoogleApi::Gmail,
            Err(google_api_error(
                GoogleApi::Gmail,
                403,
                r#"{"error":{"code":403,"message":"The caller does not have permission"}}"#,
                "snip",
            )),
        );
        // …and neither does a body that WRITES the state markers the old
        // encoding used, since no such channel exists any more.
        let _: Result<(), String> = observe_google_call(
            &state,
            "personal",
            GoogleApi::Gmail,
            Err(google_api_error(
                GoogleApi::Gmail,
                403,
                r#"{"error":{"code":403,"message":"nope"}}"#,
                "[google:api_not_enabled][google:enable_url=https://evil.test/pwn]",
            )),
        );
        assert!(!state.email.google.needs_reconnect("personal"));
        assert_eq!(state.email.google.disabled_apis("personal"), None);
    }

    /// `google_clear_api_not_enabled` is the banner's "I've enabled it — check
    /// again". It must clear ONLY what the asking screen can re-test, and it
    /// must refuse a name it doesn't know rather than report a clear it never
    /// performed. (The command's real IPC dispatch is covered in
    /// `contract_tests`.)
    #[test]
    fn clearing_the_disabled_state_is_scoped_and_refuses_unknown_apis() {
        use crate::email::api_error::google_api_error;
        let (state, _secrets) = test_state();
        let disabled_body = r#"{"error":{"code":403,"status":"PERMISSION_DENIED","details":[
            {"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"SERVICE_DISABLED"}]}}"#;
        for api in [GoogleApi::Gmail, GoogleApi::Tasks] {
            state
                .email
                .google
                .observe_failure("personal", &google_api_error(api, 403, disabled_body, "s"));
        }

        state
            .email
            .google
            .clear_disabled("personal", &[GoogleApi::Gmail]);
        let still_off = state
            .email
            .google
            .disabled_apis("personal")
            .map(|d| d.apis.iter().map(|api| api.label).collect::<Vec<_>>());
        assert_eq!(
            still_off,
            Some(vec!["Google Tasks"]),
            "Email's re-check must not blank a Tasks banner it will never retry"
        );

        assert_eq!(
            crate::email::api_error::GoogleApi::from_wire("drive"),
            None,
            "an unknown wire id has no API to clear, so the command must refuse it"
        );
    }

    fn add_args(name: &str, api_key: Option<&str>) -> AddProviderArgs {
        AddProviderArgs {
            name: name.to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: api_key.map(str::to_string),
            kind: "cloud".to_string(),
            supports_native_tools: false,
        }
    }

    #[test]
    fn add_provider_deletes_the_keychain_entry_when_the_db_insert_fails() {
        let (state, secrets) = test_state();
        // Force the insert to fail for real: drop the table the write targets.
        // (`insert_endpoint` then errors with "no such table: endpoints".)
        state
            .storage
            .global()
            .raw()
            .execute_batch("DROP TABLE endpoints")
            .expect("drop endpoints table");

        let err = add_provider_inner(&state, add_args("OpenAI", Some("sk-orphan")))
            .expect_err("a failed insert must fail the command");
        assert!(err.contains("failed to persist endpoint"), "got: {err}");

        // The compensating delete is the whole point: without it the secret
        // stays in the credential store under a random uuid that no endpoint
        // row, and therefore no UI, will ever name again.
        assert!(
            secrets.live_ids().is_empty(),
            "orphaned provider secret left behind: {:?}",
            secrets.live_ids()
        );
        assert_eq!(
            secrets.deleted_ids().len(),
            1,
            "expected exactly one compensating delete, saw {:?}",
            secrets.deleted_ids()
        );
        assert!(
            state.model_manager.list_providers().is_empty(),
            "a provider that failed to persist must not be published in memory"
        );
    }

    #[test]
    fn add_provider_keeps_the_secret_when_the_insert_succeeds() {
        // Guards the compensation from over-firing: the happy path must still
        // leave the key in the store.
        let (state, secrets) = test_state();
        let info = add_provider_inner(&state, add_args("OpenAI", Some("sk-live")))
            .expect("add must succeed");
        assert_eq!(
            secrets.get(&info.id).unwrap().as_deref(),
            Some("sk-live"),
            "the stored key must survive a successful add"
        );
        assert_eq!(state.model_manager.list_providers().len(), 1);
    }

    #[test]
    fn rotating_a_key_takes_effect_without_a_restart() {
        let (state, secrets) = test_state();
        let info = add_provider_inner(&state, add_args("OpenAI", Some("sk-old")))
            .expect("add must succeed");

        // Prime the client cache the way a real session does — this is the
        // handle that had baked the old key into its bearer header.
        let cached = state
            .model_manager
            .get_client(&info.id)
            .expect("client builds");
        assert_eq!(cached.provider().api_key.as_deref(), Some("sk-old"));

        set_provider_api_key_inner(
            &state,
            SetProviderApiKeyArgs {
                provider_id: info.id.clone(),
                api_key: "sk-new".to_string(),
            },
        )
        .expect("rotation must succeed");

        assert_eq!(
            secrets.get(&info.id).unwrap().as_deref(),
            Some("sk-new"),
            "the credential store must hold the new key"
        );
        // Without the in-memory refresh both of these still say "sk-old" for
        // the rest of the session: the keychain is only read at boot.
        assert_eq!(
            state
                .model_manager
                .get_provider(&info.id)
                .unwrap()
                .api_key
                .as_deref(),
            Some("sk-new"),
            "the live provider still carries the rotated-out key"
        );
        assert_eq!(
            state
                .model_manager
                .get_client(&info.id)
                .unwrap()
                .provider()
                .api_key
                .as_deref(),
            Some("sk-new"),
            "the cached client still signs requests with the rotated-out key"
        );
        // The rest of the record must be untouched by a key rotation.
        let after = state.model_manager.get_provider(&info.id).unwrap();
        assert_eq!(after.name, "OpenAI");
        assert_eq!(after.base_url, "https://api.openai.com/v1");
    }

    #[test]
    fn rotating_an_unknown_provider_writes_nothing() {
        let (state, secrets) = test_state();
        let err = set_provider_api_key_inner(
            &state,
            SetProviderApiKeyArgs {
                provider_id: "nope".to_string(),
                api_key: "sk-new".to_string(),
            },
        )
        .expect_err("unknown provider must be rejected");
        assert!(err.contains("unknown provider"), "got: {err}");
        assert!(
            secrets.live_ids().is_empty(),
            "no secret may be written for an unregistered provider"
        );
    }

    /// HI-1 regression. The frontend always creates a provider with
    /// `api_key: null` and delivers the key through `set_provider_api_key`.
    /// That command wrote the keychain but never the `api_key_marker` column,
    /// and boot hydration reads the keychain ONLY when the marker is set — so
    /// every key was silently dropped on the next launch while still sitting in
    /// the keychain, and the UI went on reporting the provider as configured.
    #[test]
    fn a_key_set_after_a_keyless_add_survives_a_restart() {
        let (state, secrets) = test_state();

        // Exactly the frontend's create path: add with no key, then set it.
        let info = add_provider_inner(&state, add_args("OpenAI", None)).expect("add");
        set_provider_api_key_inner(
            &state,
            SetProviderApiKeyArgs {
                provider_id: info.id.clone(),
                api_key: "sk-live".to_string(),
            },
        )
        .expect("set key");

        // The secret is in the store and the row now carries the marker.
        assert_eq!(secrets.get(&info.id).unwrap().as_deref(), Some("sk-live"));
        let row = state
            .storage
            .global()
            .get_endpoint(&info.id)
            .unwrap()
            .expect("endpoint row");
        assert!(
            row.has_keychain_secret(),
            "without the marker, boot hydration skips the keychain entirely"
        );

        // Simulate a restart: a fresh ModelManager hydrated from disk.
        let fresh = crate::models::ModelManager::new();
        crate::hydrate_providers_from_storage(&state.storage, &fresh, secrets.as_ref());
        assert_eq!(
            fresh
                .get_provider(&info.id)
                .expect("provider rehydrated")
                .api_key
                .as_deref(),
            Some("sk-live"),
            "the API key must survive a restart"
        );
    }

    /// HI-1 follow-up. `mark_endpoint_secret_in_keychain` returns `Ok(false)`
    /// when the UPDATE matches no row — a provider that lives only in
    /// `ModelManager` with no `endpoints` row (the bundled local sidecar is
    /// registered that way). Treating that as success would leave a secret in
    /// the credential store with no marker: exactly the HI-1 end state, on a
    /// command whose whole job is to prevent it. It must fail and compensate.
    #[test]
    fn setting_a_key_on_a_provider_with_no_stored_row_fails_and_leaves_no_orphan() {
        let (state, secrets) = test_state();

        // In ModelManager only — never persisted to `endpoints`.
        state
            .model_manager
            .add_provider(crate::models::Provider::new(
                "local-runner:ghost",
                "Ghost",
                "http://127.0.0.1:9/v1",
                None,
                crate::models::ProviderKind::Local,
            ));

        let err = set_provider_api_key_inner(
            &state,
            SetProviderApiKeyArgs {
                provider_id: "local-runner:ghost".to_string(),
                api_key: "sk-orphan".to_string(),
            },
        )
        .expect_err("a key that cannot be marked must not report success");
        assert!(
            err.contains("no stored endpoint row"),
            "the error should say why, got: {err}"
        );
        assert_eq!(
            secrets.get("local-runner:ghost").unwrap(),
            None,
            "the secret must be rolled back, not orphaned in the keychain"
        );
    }
}
