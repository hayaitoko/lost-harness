//! §9 Agent Loop — the orchestrator that ties together the §7 Privacy Gate,
//! the model picker (§4), and the storage layer (spec §5).
//!
//! ```text
//!  user message
//!       │
//!       ▼
//!  ┌─────────────┐    ┌──────────────┐    ┌───────────────┐
//!  │ PrivacyGate │──▶ │ ModelManager │──▶ │ ModelClient   │──▶ SSE stream
//!  └─────────────┘    └──────────────┘    └───────────────┘
//!       │ log                                │
//!       ▼                                    ▼
//!   trm_logs (storage)                 stream:token events
//!                                            │
//!                                            ▼
//!                                       messages (storage)
//! ```
//!
//! The loop is intentionally small. The M1 contract is:
//!   1. Look up the provider by `provider_id`; classify the endpoint as
//!      cloud or private via `is_private_endpoint`.
//!   2. Run the privacy gate on the user text. `Allow` proceeds, `Block`
//!      emits a `stream:error` and returns, `RouteLocal` finds a local
//!      provider or errors out.
//!   3. Log the TRM decision to `trm_logs` (spec §3 — hash only, never
//!      plaintext).
//!   4. Stream the model response, emitting one `stream:token` per delta.
//!   5. Persist the final assistant message and return its id + content.
//!
//! The streaming HTTP call is abstracted behind a private `ModelStreamer`
//! trait so tests can inject canned SSE without standing up a real server.
//! In production the trait is implemented for `crate::models::ModelClient`.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter};
use uuid::Uuid;

use crate::agent::egress::is_private_endpoint;
use crate::agent::gate::{Binding, GateDecision, PrivacyGate};
use crate::models::{ChatMessage, ModelClient, ModelManager, Provider};
use crate::storage::{Message, ProfileDb, Storage, TrmLog};

// ── Event payloads ────────────────────────────────────────────────────────

/// Payload of the `stream:token` event. Mirrors what the Svelte frontend
/// already consumes from the M0 stub. See `src/lib/api/tauri.ts`.
#[derive(Debug, Clone, Serialize)]
pub struct StreamTokenPayload {
    pub token: String,
    pub conversation_id: String,
    pub message_id: String,
}

/// Payload of the `stream:error` event. Emitted when the gate blocks a
/// message, a routing decision fails, or the model stream itself errors.
#[derive(Debug, Clone, Serialize)]
pub struct StreamErrorPayload {
    pub error: String,
    pub conversation_id: String,
    /// "gate" | "routing" | "model" — helps the UI render a useful toast.
    pub source: &'static str,
}

// ── Model streamer abstraction ───────────────────────────────────────────

/// The slice of `ModelClient` the agent loop needs. Trait-ified so tests
/// can inject canned responses without an HTTP server. Production code
/// uses the blanket impl below; no runtime overhead (monomorphization).
#[allow(async_fn_in_trait)]
pub trait ModelStreamer: Send + Sync {
    fn provider(&self) -> &Provider;
    /// Open a streaming chat-completion. Mirrors the inherent
    /// `ModelClient::stream_chat` but with a different name so the trait
    /// method doesn't collide with the inherent method (Rust forbids
    /// that). The blanket impl below delegates to the inherent method.
    async fn stream(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
    ) -> Result<crate::models::sse::SseStream>;
}

impl ModelStreamer for ModelClient {
    fn provider(&self) -> &Provider {
        self.provider()
    }
    async fn stream(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
    ) -> Result<crate::models::sse::SseStream> {
        ModelClient::stream_chat(self, model, messages).await
    }
}

// ── AgentLoop ────────────────────────────────────────────────────────────

/// The §9 agent loop. Cheap to clone (`Arc` fields). `process_message` is
/// the only entry point the IPC layer needs.
///
/// Stream serialization: a `tokio::sync::Mutex<()>` guards the in-flight
/// stream so two concurrent `process_message` calls (from different
/// Tauri commands on the thread pool) don't both open connections and
/// race on the shared storage. The lock is held for the duration of one
/// message — fine for a chat UX where one in-flight stream per app is
/// the natural shape.
pub struct AgentLoop {
    gate: PrivacyGate,
    model_manager: Arc<ModelManager>,
    storage: Arc<Storage>,
    stream_lock: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for AgentLoop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLoop").finish_non_exhaustive()
    }
}

impl AgentLoop {
    pub fn new(
        gate: PrivacyGate,
        model_manager: Arc<ModelManager>,
        storage: Arc<Storage>,
    ) -> Self {
        Self {
            gate,
            model_manager,
            storage,
            stream_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Process one user message and return the final assistant text.
    ///
    /// The flow is:
    ///  1. Resolve the provider; classify the endpoint as cloud via
    ///     `is_private_endpoint(&base_url)`.
    ///  2. Run the §7 gate. `Block` emits a `stream:error` and returns
    ///     `Ok` with an explanatory string; `RouteLocal` finds the first
    ///     local provider (or errors if none is registered).
    ///  3. Log the TRM decision to `trm_logs` (hash only).
    ///  4. Persist the user message, build the request, stream the
    ///     response, emit one `stream:token` per delta, then persist the
    ///     final assistant message.
    pub async fn process_message(
        &self,
        content: String,
        conversation_id: String,
        binding: Binding,
        provider_id: String,
        model: String,
        profile: String,
        app: AppHandle,
    ) -> Result<String> {
        // Serialize streams — one in-flight message per agent loop.
        // We hold the guard for the entire method so the storage /
        // model_manager accesses below don't race with another concurrent
        // process_message call.
        let _stream_guard = self.stream_lock.lock().await;

        // ── 1. Resolve provider + classify endpoint ──────────────────────
        let provider = self
            .model_manager
            .get_provider(&provider_id)
            .ok_or_else(|| anyhow!("unknown provider id: {provider_id}"))?;
        let is_cloud = !is_private_endpoint(&provider.base_url);

        // ── 2. Privacy gate ──────────────────────────────────────────────
        let decision = self.gate.check(&binding, &content, is_cloud);

        // Log + stream every decision. We always log (the spec says
        // "always log the decision"), even when Allow — that gives the
        // user a full audit trail.
        let message_hash = sha256_hex(content.as_bytes());
        self.log_trm_decision(&profile, &conversation_id, &decision, &message_hash)?;

        match decision {
            GateDecision::Block(reason) => {
                emit_error(
                    &app,
                    &conversation_id,
                    reason.clone(),
                    "gate",
                );
                return Ok(reason);
            }
            GateDecision::RouteLocal => {
                // Find a local provider. Prefer a provider whose `kind` is
                // `Local` AND whose `base_url` is private; this catches the
                // case where someone marked a Cloud provider pointing at
                // localhost (rare but possible) and gives the user a way
                // out.
                let local = self.find_local_provider().ok_or_else(|| {
                    anyhow!("gate routed to local model, but no local provider is registered")
                })?;
                return self
                    .stream_to_provider(
                        local,
                        model,
                        content,
                        conversation_id,
                        profile,
                        "route_local",
                        app,
                    )
                    .await;
            }
            GateDecision::Allow => {
                return self
                    .stream_to_provider(
                        provider,
                        model,
                        content,
                        conversation_id,
                        profile,
                        "allow",
                        app,
                    )
                    .await;
            }
        }
    }

    // ── helpers ─────────────────────────────────────────────────────────

    /// Find the first registered provider that is both `Local` *and*
    /// private by base URL. Returns `None` if no local model is set up.
    fn find_local_provider(&self) -> Option<Provider> {
        self.model_manager
            .list_providers()
            .into_iter()
            .find(|p| p.is_local() && p.is_private())
    }

    /// Persist the user message, run the stream, emit `stream:token` per
    /// delta, persist the final assistant message, and return the
    /// assembled text. `routing_decision` is stamped on the assistant
    /// message for the audit log.
    async fn stream_to_provider(
        &self,
        provider: Provider,
        model: String,
        content: String,
        conversation_id: String,
        profile: String,
        routing_decision: &'static str,
        app: AppHandle,
    ) -> Result<String> {
        let client = self
            .model_manager
            .get_client(&provider.id)
            .ok_or_else(|| anyhow!("provider {} has no client", provider.id))?;

        // Persist the user message first so the transcript is consistent
        // even if the model call fails.
        let user_msg_id = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().timestamp();
        let user_message = Message {
            id: user_msg_id,
            conversation_id: conversation_id.clone(),
            role: "user".to_string(),
            content: content.clone(),
            model: Some(model.clone()),
            provider_id: Some(provider.id.clone()),
            routing_decision: Some(routing_decision.to_string()),
            thinking_content: None,
            error: None,
            aborted: false,
            created_at: now,
        };
        let profile_db = self.storage.open_profile(&profile)?;
        profile_db
            .add_message(&user_message)
            .context("persist user message")?;

        // Build the chat request: prior history + the new user message.
        let mut history = profile_db
            .list_messages_by_conversation(&conversation_id)
            .context("load conversation history")?
            .into_iter()
            .map(|m| ChatMessage {
                role: m.role,
                content: m.content,
            })
            .collect::<Vec<_>>();
        // Replace the just-persisted user message's role/content from the
        // live `content` argument (the historical copy may have been
        // written by the frontend with whitespace differences).
        if let Some(last) = history.last_mut() {
            if last.role == "user" {
                last.content = content.clone();
            }
        }
        if history.is_empty() {
            history.push(ChatMessage::user(content.clone()));
        }

        let mut sse = client
            .stream_chat(&model, history)
            .await
            .with_context(|| format!("stream_chat to provider {}", provider.id))?;

        // Stream + accumulate.
        let assistant_id = Uuid::new_v4().to_string();
        let mut assembled = String::new();
        while let Some(event) = sse.next_event().await {
            match event {
                crate::models::sse::SseEvent::Delta(delta) => {
                    assembled.push_str(&delta);
                    let payload = StreamTokenPayload {
                        token: delta,
                        conversation_id: conversation_id.clone(),
                        message_id: assistant_id.clone(),
                    };
                    if let Err(e) = app.emit("stream:token", payload) {
                        tracing::warn!(error = %e, "failed to emit stream:token");
                    }
                }
                crate::models::sse::SseEvent::Error(msg) => {
                    emit_error(&app, &conversation_id, msg.clone(), "model");
                    anyhow::bail!("model stream error: {msg}");
                }
                crate::models::sse::SseEvent::Done | crate::models::sse::SseEvent::KeepAlive => {
                    // Done = end of stream. KeepAlive = no-op.
                }
            }
        }

        // Persist the assistant message.
        let assistant_message = Message {
            id: assistant_id.clone(),
            conversation_id: conversation_id.clone(),
            role: "assistant".to_string(),
            content: assembled.clone(),
            model: Some(model.clone()),
            provider_id: Some(provider.id.clone()),
            routing_decision: Some(routing_decision.to_string()),
            thinking_content: None,
            error: None,
            aborted: false,
            created_at: chrono::Utc::now().timestamp(),
        };
        profile_db
            .add_message(&assistant_message)
            .context("persist assistant message")?;

        Ok(assembled)
    }

    /// Insert a row into `trm_logs` for the given gate decision. The
    /// `message_hash` is the hex sha256 of the plaintext — we never
    /// store the plaintext itself (spec §3).
    fn log_trm_decision(
        &self,
        profile: &str,
        conversation_id: &str,
        decision: &GateDecision,
        message_hash: &str,
    ) -> Result<()> {
        let profile_db: Arc<ProfileDb> = self.storage.open_profile(profile)?;
        let entry = TrmLog {
            id: Uuid::new_v4().to_string(),
            conversation_id: conversation_id.to_string(),
            message_hash: message_hash.to_string(),
            // Spec §3 schema uses "private" | "public". Map our three-way
            // decision onto that: Block and RouteLocal both become
            // "private" (the gate refused egress); Allow → "public".
            decision: match decision {
                GateDecision::Allow => "public".to_string(),
                GateDecision::Block(_) | GateDecision::RouteLocal => "private".to_string(),
            },
            // Confidence is the gate's confidence in its decision. The
            // gate doesn't surface a number directly, so we report 1.0
            // for hard allow/block and let `RouteLocal` carry the
            // classifier's underlying confidence via the tracing layer
            // (see `gate.log_decision`).
            confidence: match decision {
                GateDecision::Allow | GateDecision::Block(_) => 1.0,
                GateDecision::RouteLocal => 0.8,
            },
            created_at: chrono::Utc::now().timestamp(),
        };
        profile_db.insert_trm_log(&entry)?;
        // Also keep the tracing layer happy (for operators tailing logs
        // without a DB connection).
        self.gate.log_decision(decision, message_hash, conversation_id);
        Ok(())
    }
}

// ── free fns ─────────────────────────────────────────────────────────────

fn emit_error(app: &AppHandle, conversation_id: &str, error: String, source: &'static str) {
    let payload = StreamErrorPayload {
        error,
        conversation_id: conversation_id.to_string(),
        source,
    };
    if let Err(e) = app.emit("stream:error", payload) {
        tracing::warn!(error = %e, "failed to emit stream:error");
    }
}

/// Lowercase hex sha256 of a byte slice. Stable, collision-resistant
/// enough for an audit-log key (spec §3: "record the *hash* of the text,
/// never the text itself").
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(&mut out, "{:02x}", b);
    }
    out
}
