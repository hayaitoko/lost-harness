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
use crate::hooks::{enforce_local_routing, RoutingRequirement};
use crate::models::{ChatMessage, ModelClient, ModelManager, OwnOutput, Provider};
use crate::storage::{Message, ProfileDb, Storage, TrmLog};
use crate::tools::{ExecCtx, ToolDispatcher, TurnOutcome};

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

/// Payload of the `stream:local_reroute` event — emitted once when a tool
/// call forced the rest of a turn onto a local endpoint (Q6). Ephemeral UI
/// signal ONLY: `reason` is the detailed privacy signal and must never be
/// persisted or replayed into a model (see `resolve_turn_outcome`).
#[derive(Debug, Clone, Serialize)]
pub struct LocalReroutePayload {
    pub conversation_id: String,
    pub reason: String,
    pub from_provider: String,
    pub to_provider: String,
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
    /// The tool spine: registry + gating chain + body capabilities. Every
    /// tool call the agent makes goes through this (§3.3 `tools::dispatch`).
    tools: Arc<ToolDispatcher>,
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
        tools: Arc<ToolDispatcher>,
    ) -> Self {
        Self {
            gate,
            model_manager,
            storage,
            tools,
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
        // Load the active profile's classifier thresholds (PLAN §11). A read
        // error or missing settings falls back to defaults — a settings read
        // must never block the send path, and defaults match pre-tunable
        // behavior. `classifier_config` already sanitizes.
        let classifier_cfg = self
            .storage
            .open_profile(&profile)
            .and_then(|db| db.classifier_config())
            .unwrap_or_default();
        let decision = self
            .gate
            .check(&binding, &content, is_cloud, &classifier_cfg);

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
                let local_is_cloud = !is_private_endpoint(&local.base_url);
                return self
                    .stream_to_provider(
                        local,
                        model,
                        content,
                        conversation_id,
                        profile,
                        "route_local",
                        binding,
                        local_is_cloud,
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
                        binding,
                        is_cloud,
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

    /// Persist the user message, then run the agentic loop: stream a turn,
    /// execute any tool calls the model made **in its own output**, feed the
    /// (guard-wrapped) results back, and repeat until the model answers
    /// without calling a tool — bounded by `MAX_TOOL_ROUNDS`. `routing_decision`
    /// is stamped on every persisted message for the audit log.
    ///
    /// `binding` / `is_cloud` describe the endpoint this turn talks to; they
    /// flow into the tool gating chain so a tool call is evaluated against
    /// the same privacy posture as the conversation.
    #[allow(clippy::too_many_arguments)]
    async fn stream_to_provider(
        &self,
        mut provider: Provider,
        model: String,
        content: String,
        conversation_id: String,
        profile: String,
        mut routing_decision: &'static str,
        binding: Binding,
        mut is_cloud: bool,
        app: AppHandle,
    ) -> Result<String> {
        // `provider`/`client`/`is_cloud`/`routing_decision` are mutable because
        // a must-stay-local tool call mid-turn can switch the rest of THIS
        // turn to a local endpoint (Q6, `resolve_turn_outcome`). The switch is
        // turn-scoped: the next user message starts a fresh `stream_to_provider`
        // with a fresh gate check.
        let mut client = self
            .model_manager
            .get_client(&provider.id)
            .ok_or_else(|| anyhow!("provider {} has no client", provider.id))?;

        let profile_db = self.storage.open_profile(&profile)?;

        // Persist the user message first so the transcript is consistent
        // even if the model call fails.
        let now = chrono::Utc::now().timestamp();
        let user_message = Message {
            id: Uuid::new_v4().to_string(),
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
        profile_db
            .add_message(&user_message)
            .context("persist user message")?;

        // Build the working chat history: an optional system message that
        // teaches the fenced tool dialect and lists the tools available in
        // this environment, then the prior turns. Tool-result rows are
        // persisted with role "tool" for transcript fidelity but replayed to
        // the model as "user" — the fenced dialect carries tool results as
        // plain text, so we avoid the native "tool" role that OpenAI-compatible
        // servers reject without a matching tool_call_id.
        let mut history: Vec<ChatMessage> = Vec::new();
        let catalog = self.tools.catalog();
        if !catalog.is_empty() {
            history.push(ChatMessage::system(catalog));
        }
        for m in profile_db
            .list_messages_by_conversation(&conversation_id)
            .context("load conversation history")?
        {
            let role = if m.role == "tool" {
                "user".to_string()
            } else {
                m.role
            };
            history.push(ChatMessage { role, content: m.content });
        }
        // The persisted copy of the current user message may differ from the
        // live `content` by whitespace; make the last user turn authoritative.
        if let Some(last_user) = history.iter_mut().rev().find(|m| m.role == "user") {
            last_user.content = content.clone();
        } else {
            history.push(ChatMessage::user(content.clone()));
        }

        // `reads` is injected by the dispatcher (which owns the shared handle),
        // so the loop leaves it `None` here.
        let exec_ctx = ExecCtx {
            conversation_id: conversation_id.clone(),
            profile: profile.clone(),
            reads: None,
        };

        // Bound the tool loop so a model that keeps calling tools can't run
        // away. MAX_TOOL_ROUNDS tool rounds + one final answer turn.
        const MAX_TOOL_ROUNDS: usize = 6;
        let mut final_text = String::new();

        // Q4 do-now item 2: reset the dispatcher's per-run budget + repeat
        // detection ring at the start of every user message. The dispatcher
        // then enforces ceilings and cascades inside `run_turn`.
        self.tools.begin_run();

        for round in 0..=MAX_TOOL_ROUNDS {
            let assistant_id = Uuid::new_v4().to_string();
            let mut sse = client
                .stream_chat(&model, history.clone())
                .await
                .with_context(|| format!("stream_chat to provider {}", provider.id))?;

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
                    crate::models::sse::SseEvent::Done
                    | crate::models::sse::SseEvent::KeepAlive => {}
                }
            }

            // Mint the type that proves this text came from nowhere but this
            // model's own current-turn SSE stream — see models::client::OwnOutput.
            let own_output = OwnOutput::from_stream_assembly(assembled.clone());

            // A deliberate round-cap stop persists this last assistant turn
            // with `aborted: true`. The turn may still carry an open
            // ```tool fence (the model tried to call another tool, but we
            // stop it at the budget) — on disk that is byte-identical to a
            // genuine crash mid-tool-call, which crash-recovery repairs with
            // a "[tool interrupted]" row. The marker is what lets
            // crash-recovery tell the two apart: a real crash kills the
            // process before this row is ever written, so a genuine
            // interrupted turn can never carry `aborted: true`. (Here
            // `aborted` means "cut off by the round budget," not "content
            // truncated mid-fence" — reconcile only repairs when an open
            // fence is ALSO present, so a clean final answer on the cap
            // round is still left alone.)
            let is_round_cap_stop = round == MAX_TOOL_ROUNDS;

            // Persist this assistant turn.
            let assistant_message = Message {
                id: assistant_id,
                conversation_id: conversation_id.clone(),
                role: "assistant".to_string(),
                content: assembled.clone(),
                model: Some(model.clone()),
                provider_id: Some(provider.id.clone()),
                routing_decision: Some(routing_decision.to_string()),
                thinking_content: None,
                error: None,
                aborted: is_round_cap_stop,
                created_at: chrono::Utc::now().timestamp(),
            };
            profile_db
                .add_message(&assistant_message)
                .context("persist assistant message")?;
            final_text = assembled.clone();

            // On the last permitted round, stop without dispatching more tools.
            if is_round_cap_stop {
                break;
            }

            // Run any tool calls the model made in ITS OWN output. Passing
            // only `own_output` here (typed `&OwnOutput`, never a tool result
            // or prior turn) is the "parse only your own current output"
            // safety rule — enforced at the type level. A call that must stay
            // on-device on this (cloud) endpoint is resolved by
            // `resolve_turn_outcome`, which may switch
            // provider/client/is_cloud/routing_decision to a local endpoint
            // for the rest of this turn.
            let turn_outcome = self
                .tools
                .run_turn(&own_output, &exec_ctx, binding, is_cloud)
                .await;
            let conv_id = conversation_id.clone();
            let (tool_feedback, new_provider, new_client, new_is_cloud, new_routing_decision) =
                resolve_turn_outcome(
                    &self.tools,
                    &self.model_manager,
                    turn_outcome,
                    &exec_ctx,
                    binding,
                    provider.clone(),
                    client,
                    is_cloud,
                    routing_decision,
                    &|from, to, reason| {
                        let payload = LocalReroutePayload {
                            conversation_id: conv_id.clone(),
                            reason: reason.to_string(),
                            from_provider: from.to_string(),
                            to_provider: to.to_string(),
                        };
                        if let Err(e) = app.emit("stream:local_reroute", payload) {
                            tracing::warn!(error = %e, "failed to emit stream:local_reroute");
                        }
                    },
                )
                .await?;
            provider = new_provider;
            client = new_client;
            is_cloud = new_is_cloud;
            routing_decision = new_routing_decision;

            match tool_feedback {
                Some(tool_feedback) => {
                    // Persist the tool feedback (role "tool" for the transcript).
                    // Uses the now-current provider/routing_decision, so a
                    // reroute's feedback row is tagged with the local endpoint.
                    let tool_message = Message {
                        id: Uuid::new_v4().to_string(),
                        conversation_id: conversation_id.clone(),
                        role: "tool".to_string(),
                        content: tool_feedback.content.clone(),
                        model: Some(model.clone()),
                        provider_id: Some(provider.id.clone()),
                        routing_decision: Some(routing_decision.to_string()),
                        thinking_content: None,
                        error: None,
                        aborted: false,
                        created_at: chrono::Utc::now().timestamp(),
                    };
                    profile_db
                        .add_message(&tool_message)
                        .context("persist tool message")?;

                    // Extend the working history and loop for another round.
                    history.push(ChatMessage::assistant(assembled));
                    history.push(tool_feedback);
                }
                // No tool calls — this turn is the final answer.
                None => break,
            }
        }

        Ok(final_text)
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

/// The transcript banner shown when a tool call forced the rest of a turn
/// onto a local endpoint. Deliberately **reason-free**: it is persisted and
/// replayed into future turns of the conversation (which may be cloud-bound),
/// so it must never carry the detailed privacy `reason` — that flows only
/// through the ephemeral `stream:local_reroute` event. See
/// `resolve_turn_outcome`.
fn reroute_banner(local_provider_name: &str) -> String {
    format!(
        "[routing] switched to the local model \"{local_provider_name}\" for the rest of this \
         turn — a tool call needed to stay on-device."
    )
}

/// Drive a [`TurnOutcome`] to completion, resolving any `NeedsLocalReroute`
/// via `enforce_local_routing` over `model_manager`'s current providers.
/// Returns the feedback message (`None` if there were no tool calls) plus the
/// provider/client/is_cloud/routing_decision to use for the REST of this turn
/// (unchanged unless a reroute actually happened).
///
/// Pulled out of `stream_to_provider` as a free function so it's unit-testable
/// without a live HTTP model endpoint — it never calls `stream_chat`.
///
/// `on_reroute(from_name, to_name, reason)` fires exactly once per successful
/// switch. This is the ONLY place `reason` — a privacy signal — is allowed to
/// travel; it must never end up in the returned `ChatMessage` (which gets
/// persisted and replayed into a future turn that may be on cloud). The
/// returned message carries only the reason-free `reroute_banner`.
///
/// Local-model-down fails loud, never falls back to cloud: this function never
/// calls `stream_chat`, so an unreachable local endpoint surfaces on the NEXT
/// round's `client.stream_chat(...)?` as a propagated error — there is no
/// catch-and-retry-on-cloud path here, and none must be added.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub(crate) async fn resolve_turn_outcome(
    tools: &ToolDispatcher,
    model_manager: &ModelManager,
    mut turn_outcome: TurnOutcome,
    exec_ctx: &ExecCtx,
    binding: Binding,
    mut provider: Provider,
    mut client: ModelClient,
    mut is_cloud: bool,
    mut routing_decision: &'static str,
    // `Send + Sync` because this reference is held across `.await` points
    // inside the reroute loop, and `stream_to_provider`'s future must stay
    // `Send` for the Tauri command boundary.
    on_reroute: &(dyn Fn(&str, &str, &str) + Send + Sync),
) -> Result<(Option<ChatMessage>, Provider, ModelClient, bool, &'static str)> {
    // Backstop, not a designed retry count — `remaining` strictly shrinks
    // each pass, so a reroute chain terminates naturally; hitting the cap
    // means a logic bug, and it fails closed. Same philosophy as
    // MAX_APPROVAL_ROUNDS / MAX_TOOL_ROUNDS.
    const MAX_REROUTE_STEPS: usize = 8;
    let mut steps = 0;
    loop {
        match turn_outcome {
            TurnOutcome::NoToolCalls => {
                return Ok((None, provider, client, is_cloud, routing_decision))
            }
            TurnOutcome::Feedback(msg) => {
                return Ok((Some(msg), provider, client, is_cloud, routing_decision))
            }
            TurnOutcome::NeedsLocalReroute {
                reason,
                call,
                prior_sections,
                remaining,
                turn_call_count,
                cascade_active,
            } => {
                steps += 1;
                if steps > MAX_REROUTE_STEPS {
                    anyhow::bail!("too many local-reroute steps in one tool round");
                }
                let candidates = model_manager.list_providers();
                let routing = RoutingRequirement::LocalRequired {
                    reason: reason.clone(),
                };
                // `enforce_local_routing` is the ONLY thing structurally
                // guaranteed to never return a cloud provider on the
                // LocalRequired branch — never hand-roll is_local()&&is_private().
                let found = match enforce_local_routing(&routing, &candidates) {
                    Ok(local) => model_manager
                        .get_client(&local.id)
                        .map(|c| (local.clone(), c)),
                    Err(_) => None,
                };
                match found {
                    Some((local, local_client)) => {
                        on_reroute(&provider.name, &local.name, &reason);
                        let resumed = tools
                            .resume_after_local_switch(
                                call,
                                remaining,
                                prior_sections,
                                exec_ctx,
                                binding,
                                turn_call_count,
                                cascade_active,
                            )
                            .await;
                        // Reason-free banner only — see the doc comment.
                        let combined = ChatMessage::user(format!(
                            "{}\n\n{}",
                            reroute_banner(&local.name),
                            resumed.content
                        ));
                        provider = local;
                        client = local_client;
                        is_cloud = false; // enforce_local_routing proved is_local() && is_private()
                        routing_decision = "tool_reroute_local";
                        return Ok((Some(combined), provider, client, is_cloud, routing_decision));
                    }
                    None => {
                        // No local candidate — format the call as exactly
                        // today's hard-deny text (no re-dispatch) and keep
                        // driving the rest of the batch on the same endpoint.
                        turn_outcome = tools
                            .deny_and_continue_turn(
                                call,
                                remaining,
                                prior_sections,
                                reason,
                                exec_ctx,
                                binding,
                                is_cloud,
                                turn_call_count,
                                cascade_active,
                            )
                            .await;
                    }
                }
            }
        }
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
