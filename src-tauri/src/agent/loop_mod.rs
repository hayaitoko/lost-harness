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
use crate::agent::result_sink::ResultSink;
use crate::hooks::{enforce_local_routing, RoutingRequirement};
use crate::models::{ChatMessage, ModelClient, ModelManager, OwnOutput, Provider, TrustZone};
use crate::storage::{MemoryBucket, MemoryFact, Message, ProfileDb, Storage, TrmLog};
use crate::tools::{ExecCtx, ToolDispatcher, TurnOutcome};

// ── Endpoint-selection errors ─────────────────────────────────────────────
//
// A turn carries the endpoint the user picked. When that selection arrives
// empty the cause is upstream state (the composer's picker lost its pair),
// not a bad provider id — so it gets its own user-actionable wording instead
// of the internal-sounding `unknown provider id: `. These live here, next to
// the invariant they describe, and the IPC boundary reuses them so the user
// sees one sentence no matter which layer catches it.

/// No provider id at all was supplied for the turn.
pub(crate) const NO_ENDPOINT_SELECTED: &str =
    "no model endpoint is selected — pick a model in the composer";

/// A provider was supplied but no model name with it.
pub(crate) const NO_MODEL_SELECTED: &str =
    "no model is selected for this endpoint — pick a model in the composer";

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
    /// C5: true when `to_provider` is the app's bundled sidecar (M8 S4 lazy
    /// spawn) rather than a user-added local endpoint — lets the toast read
    /// "started your local model" vs "switched to <name>".
    pub to_is_bundled_runner: bool,
}

/// Payload of `stream:budget_warning` (C1) — a non-blocking banner when an
/// attended chat turn is over its spend cap. The turn proceeds regardless.
#[derive(Debug, Clone, Serialize)]
pub struct BudgetWarningPayload {
    pub conversation_id: String,
    pub message: String,
}

/// Payload of the `memory:event` event — the non-silent memory signal (PLAN
/// §9). Emitted when the agent recalls saved notes for an answer. Carries only
/// a kind + count, never the recalled content itself.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryEventPayload {
    pub conversation_id: String,
    /// "recalled" (relevance-gated notes injected for this turn) or
    /// "remembered" (a durable fact was just saved).
    pub kind: &'static str,
    /// How many notes were recalled / remembered.
    pub count: usize,
}

// ── Model streamer abstraction ───────────────────────────────────────────

/// The slice of `ModelClient` the agent loop needs. Trait-ified so tests
/// can inject canned responses without an HTTP server. Production code
/// uses the blanket impl below; no runtime overhead (monomorphization).
pub trait ModelStreamer: Send + Sync {
    fn provider(&self) -> &Provider;
    /// Open a streaming chat-completion. Mirrors the inherent
    /// `ModelClient::stream_chat` but with a different name so the trait
    /// method doesn't collide with the inherent method (Rust forbids that).
    /// Returns a boxed future so the trait is **dyn-compatible** — B7 injects a
    /// `Arc<dyn ModelStreamer>` fake into the real loop (same shape as the
    /// `DurableFactExtractor`/`SkillDrafter` injectable traits).
    fn stream<'a>(
        &'a self,
        model: &'a str,
        messages: Vec<ChatMessage>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<crate::models::sse::SseStream>> + Send + 'a>,
    >;
}

impl ModelStreamer for ModelClient {
    fn provider(&self) -> &Provider {
        self.provider()
    }
    fn stream<'a>(
        &'a self,
        model: &'a str,
        messages: Vec<ChatMessage>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<crate::models::sse::SseStream>> + Send + 'a>,
    > {
        Box::pin(async move { ModelClient::stream_chat(self, model, messages).await })
    }
}

// ── AgentLoop ────────────────────────────────────────────────────────────

/// The §9 agent loop. Cheap to clone (`Arc` fields). `process_message` is
/// the only entry point the IPC layer needs.
///
/// Stream serialization: locking is PER-CONVERSATION — a
/// `HashMap<String, Arc<tokio::sync::Mutex<()>>>` guards the in-flight
/// stream per `conversation_id`, so a stalled model/provider in one
/// conversation does not block others. The lock is held for the duration
/// of one message — fine for a chat UX where one in-flight stream per
/// conversation is the natural shape.
pub struct AgentLoop {
    gate: PrivacyGate,
    model_manager: Arc<ModelManager>,
    storage: Arc<Storage>,
    /// The tool spine: registry + gating chain + body capabilities. Every
    /// tool call the agent makes goes through this (§3.3 `tools::dispatch`).
    tools: Arc<ToolDispatcher>,
    /// Memory's meaning-lane embedder handle (PLAN §9), when its model is
    /// installed. `None` ⇒ the automatic memory injection runs keyword-only.
    /// Loading is lazy AND gated per-profile — the model is only pulled in when
    /// a profile with semantic memory search enabled actually needs it (Wave 1.2).
    embedder: Option<Arc<crate::embedder::EmbedderHandle>>,
    /// Per-conversation snapshot of the curated summary's candidate fact set
    /// (Wave 1.3). Frozen at a conversation's first turn and reused for the rest
    /// of it, so a mid-conversation `remember` doesn't churn the loaded summary
    /// and the prompt prefix stays cache-stable (PLAN §9 "Timing and trust").
    /// Keyed by conversation id; holds both buckets — the per-turn renderer
    /// applies the endpoint privacy filter, so a private fact never rides a
    /// cloud turn even though the frozen set includes it.
    summary_cache:
        parking_lot::Mutex<std::collections::HashMap<String, Vec<(MemoryFact, MemoryBucket)>>>,
    /// Per-conversation cloud-safety cache (privacy). Re-classifying every prior
    /// turn on each cloud send is expensive AND, capped, would wrongly force a
    /// long-but-benign cloud chat local. This caches `(is_safe, verified_count,
    /// cfg)` per conversation: a private turn is permanent (append-only history),
    /// so once unsafe it stays unsafe; when still safe, only turns added since
    /// the last check are re-classified. A classifier-config change invalidates
    /// the entry (full re-scan) so tightening strictness can never ride a stale
    /// "safe". Cleared implicitly by process restart (cold scan re-populates).
    cloud_safe_cache: parking_lot::Mutex<std::collections::HashMap<String, CloudSafeEntry>>,
    /// Wave 3.5: durable-fact extractor for the pre-compaction flush (a LOCAL
    /// model in production; a fake in tests). Default `LocalModelExtractor`.
    fact_extractor: Arc<dyn crate::agent::memory_flush::DurableFactExtractor>,
    /// Wave 3.5: the classifier the flush re-classifies extracted facts with
    /// (same one the gate uses). `None` ⇒ the flush is disabled (it can't route
    /// safely without a classifier), so `on_pre_compaction` stays a no-op — the
    /// pre-3.5 behavior. `lib.rs` sets it in production.
    flush_classifier: Option<Arc<dyn crate::classifier::Classifier>>,
    /// Wave 3.5: per-conversation content-hash high-water set — a turn is swept
    /// for durable facts at most once. `Arc` so the new-chat nudge's detached
    /// task can share it with the on-stream flush. Same bounded-cache discipline
    /// as `summary_cache`/`cloud_safe_cache`.
    flush_marks: Arc<
        parking_lot::Mutex<std::collections::HashMap<String, std::collections::HashSet<String>>>,
    >,
    /// Wave 4.2: the autonomous skill drafter (a LOCAL model in production; a fake
    /// in tests). `None` ⇒ reflection is disabled. Even when wired, it only runs
    /// if the global `skill_reflect_enabled` toggle is on, and its drafts are
    /// always saved `Pending` (inert until a human approves them). `lib.rs` sets it.
    skill_drafter: Option<Arc<dyn crate::agent::skill_reflect::SkillDrafter>>,
    /// Wave 4.2: per-conversation high-water — a prior conversation is reflected
    /// at most once per process run. `Arc` so the detached reflect task shares it.
    reflect_marks: Arc<parking_lot::Mutex<std::collections::HashSet<String>>>,
    /// M8 S4: the bundled-sidecar context (supervisor + resolved binary), when
    /// the feature is on AND the vendored binary resolved at boot. `None` ⇒
    /// `find_or_start_local_provider` degrades to the plain snapshot lookup —
    /// exactly the pre-S4 behavior. `lib.rs` sets it.
    #[cfg(feature = "local-runner")]
    local_runner: Option<Arc<crate::models::runner::LocalRunnerContext>>,
    stream_locks:
        Arc<parking_lot::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// B7: a test-only injectable model streamer. `None` in production (one
    /// `Option` check on the per-round stream path, always None there); set by
    /// the `#[cfg(test)] with_model_streamer_override` builder so the REAL
    /// `process_message` can be driven end-to-end against a canned transport
    /// (the Allow→cloud history guard, redact-and-send, and usage booking are
    /// tested through the real loop, not a reimplementation).
    streamer_override: Option<Arc<dyn ModelStreamer>>,
    /// C7 (M6 Slice 4a): in-flight cancellation tokens keyed by conversation_id.
    /// `cancel_message` flips the token; `stream_to_provider`'s SSE drain loop
    /// observes it cooperatively and breaks. Same `parking_lot::Mutex<HashMap>`
    /// idiom as `summary_cache`; touched only via the two tiny methods below,
    /// never `stream_locks`, so a cancel can never deadlock the turn it interrupts.
    cancellations:
        parking_lot::Mutex<std::collections::HashMap<String, tokio_util::sync::CancellationToken>>,
}

/// A borrowed lazy-runner reference for the free-fn reroute path
/// ([`resolve_turn_outcome`]): the sidecar context + the storage it reads the
/// model catalog from. A cfg-stable alias so callers (incl. tests) can always
/// pass `None` regardless of feature flags.
#[cfg(feature = "local-runner")]
pub(crate) type LocalRunnerRef<'a> =
    Option<(&'a crate::models::runner::LocalRunnerContext, &'a Storage)>;
#[cfg(not(feature = "local-runner"))]
pub(crate) type LocalRunnerRef<'a> = Option<std::convert::Infallible>;

/// A cached cloud-safety verdict for a conversation's replayable history.
#[derive(Debug, Clone)]
struct CloudSafeEntry {
    safe: bool,
    /// How many leading messages have been verified under `cfg`.
    verified_count: usize,
    cfg: crate::classifier::ClassifierConfig,
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
        let model_manager_for_flush = Arc::clone(&model_manager);
        let storage_for_flush = Arc::clone(&storage);
        Self {
            gate,
            model_manager,
            storage,
            tools,
            embedder: None,
            summary_cache: parking_lot::Mutex::new(std::collections::HashMap::new()),
            cloud_safe_cache: parking_lot::Mutex::new(std::collections::HashMap::new()),
            fact_extractor: Arc::new(crate::agent::memory_flush::LocalModelExtractor::new(
                Arc::clone(&model_manager_for_flush),
                storage_for_flush,
            )),
            flush_classifier: None,
            flush_marks: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
            skill_drafter: None,
            reflect_marks: Arc::new(parking_lot::Mutex::new(std::collections::HashSet::new())),
            #[cfg(feature = "local-runner")]
            local_runner: None,
            stream_locks: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
            streamer_override: None,
            cancellations: parking_lot::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Wire the bundled-sidecar context (M8 S4). Without it the loop never
    /// lazy-spawns — `RouteLocal` requires an already-registered local provider,
    /// the pre-S4 behavior. `lib.rs` sets it when the vendored binary resolves.
    #[cfg(feature = "local-runner")]
    pub fn with_local_runner(
        mut self,
        ctx: Arc<crate::models::runner::LocalRunnerContext>,
    ) -> Self {
        self.local_runner = Some(ctx);
        self
    }

    /// Wire the flush's fact classifier (Wave 3.5). Without it the pre-compaction
    /// flush stays disabled (a no-op `on_pre_compaction`). `lib.rs` sets it.
    pub fn with_flush_classifier(
        mut self,
        classifier: Arc<dyn crate::classifier::Classifier>,
    ) -> Self {
        self.flush_classifier = Some(classifier);
        self
    }

    /// Wire the Wave 4.2 autonomous skill drafter. Without it, new-chat reflection
    /// stays disabled. Even wired, it fires only when the global
    /// `skill_reflect_enabled` toggle is on. `lib.rs` sets it.
    pub fn with_skill_drafter(
        mut self,
        drafter: Arc<dyn crate::agent::skill_reflect::SkillDrafter>,
    ) -> Self {
        self.skill_drafter = Some(drafter);
        self
    }

    /// Override the durable-fact extractor (tests inject a fake). Production uses
    /// the default `LocalModelExtractor`.
    #[cfg(test)]
    pub fn with_fact_extractor(
        mut self,
        extractor: Arc<dyn crate::agent::memory_flush::DurableFactExtractor>,
    ) -> Self {
        self.fact_extractor = extractor;
        self
    }

    /// B7: inject a fake [`ModelStreamer`] so the REAL `process_message` streams
    /// against a canned transport (no HTTP). Tests only.
    #[cfg(test)]
    pub fn with_model_streamer_override(mut self, streamer: Arc<dyn ModelStreamer>) -> Self {
        self.streamer_override = Some(streamer);
        self
    }

    /// C7: register a fresh cancellation token for a conversation's turn and
    /// return a clone for the loop to observe. Overwrites any stale leftover
    /// (a prior turn's cleanup should have removed it; defensive).
    fn begin_cancellable(&self, conversation_id: &str) -> tokio_util::sync::CancellationToken {
        let token = tokio_util::sync::CancellationToken::new();
        self.cancellations
            .lock()
            .insert(conversation_id.to_string(), token.clone());
        token
    }

    /// C7: cancel the in-flight streaming turn for `conversation_id`, if one
    /// exists. Returns whether there was something to cancel. Takes ONLY the
    /// internal registry lock (never `stream_locks`), so it can't deadlock
    /// against the `process_message` it interrupts.
    pub fn cancel_conversation(&self, conversation_id: &str) -> bool {
        match self.cancellations.lock().get(conversation_id) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// Attach the memory embedder handle (meaning-lane hybrid search).
    /// Builder-style so existing constructions stay valid; `None` keeps
    /// keyword-only.
    pub fn with_embedder(mut self, embedder: Option<Arc<crate::embedder::EmbedderHandle>>) -> Self {
        self.embedder = embedder;
        self
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
    #[allow(clippy::too_many_arguments)]
    pub async fn process_message(
        &self,
        content: String,
        conversation_id: String,
        binding: Binding,
        provider_id: String,
        model: String,
        profile: String,
        session_mode: crate::hooks::SessionMode,
        sink: &Arc<dyn ResultSink>,
    ) -> Result<String> {
        // Serialize per-conversation — one in-flight message per
        // conversation_id, so a stalled turn in one conversation does not
        // block others. Held for the whole turn so the storage /
        // model_manager accesses for THIS conversation don't race with
        // another concurrent process_message call against the same
        // conversation.
        let conv_lock = {
            let mut locks = self.stream_locks.lock();
            locks
                .entry(conversation_id.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _stream_guard = conv_lock.lock().await;
        // C7: register a cancellation token for this turn. The thin wrapper
        // removes the registry entry on every NORMAL exit path (Ok, Err, or any
        // early `return` inside the inner body) without a manual guard type. A
        // panic-unwind would skip the remove — but there are no reachable panics
        // on this path, and `begin_cancellable` overwrites on the next turn, so a
        // hypothetical leaked entry is self-healing.
        let cancel_token = self.begin_cancellable(&conversation_id);
        let result = self
            .process_message_inner(
                content,
                conversation_id.clone(),
                binding,
                provider_id,
                model,
                profile,
                session_mode,
                cancel_token,
                sink,
            )
            .await;
        self.cancellations.lock().remove(&conversation_id);
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_message_inner(
        &self,
        content: String,
        conversation_id: String,
        binding: Binding,
        provider_id: String,
        model: String,
        profile: String,
        session_mode: crate::hooks::SessionMode,
        cancel_token: tokio_util::sync::CancellationToken,
        sink: &Arc<dyn ResultSink>,
    ) -> Result<String> {
        // C1: the budget governor (attended path). A human is NEVER hard-blocked
        // — an over-cap turn only surfaces a non-blocking banner and proceeds.
        // The unattended HALT lives in `work_runner` (before the model call ever
        // fires). `is_attended()` is false in a `run_subagent` sub-loop (headless
        // dispatcher), so this fires only for real interactive turns. Best-effort:
        // a settings/ledger read error defaults open — a governor check must never
        // block the send path.
        if self.tools.is_attended() {
            let since = crate::hooks::budget::month_start_ts(chrono::Utc::now());
            let cap = self
                .storage
                .open_profile(&profile)
                .and_then(|db| db.budget_cap());
            let sum = self
                .storage
                .open_profile(&profile)
                .and_then(|db| db.usage_summary_since(since));
            if let (Ok(cap), Ok(sum)) = (cap, sum) {
                if let crate::hooks::budget::BudgetVerdict::Warn(reason) =
                    crate::hooks::budget::evaluate(cap, &sum, true)
                {
                    sink.budget_warning(&conversation_id, &reason);
                }
            }
        }

        // ── 1. Resolve provider + classify endpoint ──────────────────────
        //
        // Two distinct failures, deliberately worded differently. A blank id
        // means the caller never had a selection to send (the composer's
        // provider/model pair came apart upstream) — a state the USER can
        // fix, and one that used to render as the dangling, internal-looking
        // `unknown provider id: `. A non-blank id that isn't registered stays
        // loud and unchanged: it is never a cue to substitute some other
        // provider (see `hooks::routing::enforce_local_routing`).
        let provider = self
            .model_manager
            .get_provider(&provider_id)
            .ok_or_else(|| {
                if provider_id.trim().is_empty() {
                    anyhow!("{NO_ENDPOINT_SELECTED}")
                } else {
                    anyhow!("unknown provider id: {provider_id}")
                }
            })?;
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
        let (decision, classification) =
            self.gate
                .check_detailed(&binding, &content, is_cloud, &classifier_cfg);

        // Log + stream every decision. We always log (the spec says
        // "always log the decision"), even when Allow — that gives the
        // user a full audit trail.
        let message_hash = sha256_hex(content.as_bytes());
        self.log_trm_decision(&profile, &conversation_id, &decision, &message_hash)?;

        match decision {
            GateDecision::Block(reason) => {
                sink.error(&conversation_id, &reason, "gate");
                return Ok(reason);
            }
            // H-12: a `Public`-bound message that hit the structured-secret
            // floor. The turn stops here WITHOUT egress; the `"gate_confirm"`
            // source tells the UI to render the one-send confirmation affordance
            // instead of a plain error. On "send anyway" the frontend calls
            // `confirm_public_send(text)` (which grants a single-use, expiring
            // authorisation for this exact text) and re-sends the message; the
            // gate consumes the grant and the retry proceeds.
            GateDecision::ConfirmRequired { reason, .. } => {
                sink.error(&conversation_id, &reason, "gate_confirm");
                return Ok(reason);
            }
            GateDecision::RouteLocal => {
                // ── Partial delegation (PLAN §11): before forcing the whole
                // turn local, try to black out the sensitive VALUE spans and
                // send the safe remainder to the ORIGINAL cloud provider. This
                // only happens when the profile allows redaction, the redaction
                // fully cleans the current message (re-classified Public), AND
                // the rest of the outgoing payload (prior turns) is already
                // cloud-safe — otherwise a prior private turn replayed in the
                // history would leak. Any failure falls through to local.
                if is_cloud {
                    if let Some(redaction) = self.plan_redaction(
                        &profile,
                        &content,
                        classification.as_ref(),
                        &classifier_cfg,
                    ) {
                        if self.conversation_is_cloud_safe(
                            &profile,
                            &conversation_id,
                            &classifier_cfg,
                        ) {
                            // The non-silent signal is the persisted
                            // `routing_decision = "redact_send"`, which
                            // MainScreen renders as an inline event bar (survives
                            // reload — unlike a transient event).
                            return self
                                .stream_to_provider(
                                    provider,
                                    model,
                                    content,
                                    conversation_id,
                                    profile,
                                    "redact_send",
                                    binding,
                                    is_cloud,
                                    Some(redaction),
                                    session_mode,
                                    cancel_token.clone(),
                                    sink,
                                )
                                .await;
                        }
                    }
                }

                // Find a local provider. Prefer a provider whose `kind` is
                // `Local` AND whose `base_url` is private; this catches the
                // case where someone marked a Cloud provider pointing at
                // localhost (rare but possible) and gives the user a way
                // out. The empty-snapshot case lazily starts the bundled
                // sidecar for a downloaded model first (M8 S4).
                let local = self.find_or_start_local_provider().await.ok_or_else(|| {
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
                        None,
                        session_mode,
                        cancel_token.clone(),
                        sink,
                    )
                    .await;
            }
            GateDecision::Allow => {
                // A cloud send replays the FULL conversation history. Even when
                // THIS message is Public, an earlier turn may hold private
                // content whose ORIGINAL is persisted — a turn that routed local
                // or redact-sent, OR a private message that was merely "allow"ed
                // because the endpoint was local at the time. Replaying any of
                // those to cloud would leak, and the per-message gate never
                // re-vets the history. So before a cloud send, require the whole
                // prior history to be cloud-safe; if it isn't, keep THIS turn
                // local (so the private history never leaves the device), or fail
                // closed if no local model is configured. (Mirrors the guard the
                // redact-and-send path already applies.)
                if is_cloud
                    && !self.conversation_is_cloud_safe(&profile, &conversation_id, &classifier_cfg)
                {
                    let Some(local) = self.find_or_start_local_provider().await else {
                        let reason = "This conversation can't be safely continued on a cloud \
                                      model (it contains earlier private content, or is too long \
                                      to verify), and no local model is configured to continue it \
                                      privately."
                            .to_string();
                        sink.error(&conversation_id, &reason, "gate");
                        return Ok(reason);
                    };
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
                            None,
                            session_mode,
                            cancel_token.clone(),
                            sink,
                        )
                        .await;
                }
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
                        None,
                        session_mode,
                        cancel_token.clone(),
                        sink,
                    )
                    .await;
            }
        }
    }

    /// This loop's provider registry. Exposed so the work runner can stamp the
    /// trust zone of a helper's endpoint onto the note it posts back into the
    /// parent conversation — a zone must come from the endpoint that actually
    /// ran, at the time it ran, never from whatever the registry holds when the
    /// transcript is later rendered.
    pub(crate) fn model_manager(&self) -> &ModelManager {
        &self.model_manager
    }

    /// Wave 4.3c — run one bounded, one-shot "helper" sub-agent. This is the
    /// `delegate` tool's actual EXECUTION, performed here by the background
    /// `WorkQueueRunner` (`agent::work_runner`), never by `delegate` itself:
    /// `delegate` can only hold `Storage` + `ModelManager`, not an
    /// `Arc<AgentLoop>`, because `AgentLoop` owns the `ToolDispatcher` that
    /// owns `delegate` — holding an `AgentLoop` back in `delegate` would be a
    /// circular `Arc` dependency. So `delegate` only enqueues a `work_items`
    /// row; this method is what the runner calls once it claims that row.
    ///
    /// Builds a FRESH `AgentLoop` sharing this loop's `gate`/`model_manager`/
    /// `storage` (same classifier, same providers, same on-disk storage), but
    /// with `tools` RESTRICTED to `tools_allowlist` via
    /// `ToolDispatcher::restricted` — same full gate chain as the parent
    /// (Lukas's decision #3: **no floor-cap**; a helper's belt may include
    /// External/Dangerous tools, each call is still individually gated by the
    /// identical chain). Deliberately does NOT wire `with_embedder` /
    /// `with_flush_classifier` / `with_skill_drafter` — a helper run is
    /// one-shot and its ephemeral sub-conversation is abandoned right after,
    /// so background memory-flush/skill-reflection setup would be wasted work.
    ///
    /// v1 persona framing (noted as a deliberate simplification): the
    /// persona's `system_prompt` LEADS the first (only) user turn rather than
    /// overriding `process_message`'s own system prompt — `process_message`
    /// always builds its own system messages (tool catalog + curated memory
    /// summary). A proper system-prompt override is later refinement.
    ///
    /// The sub-conversation created here is just the ephemeral scratch
    /// transcript `process_message` needs to operate against — Lukas's
    /// decision #2 (the helper's result streams into the PARENT conversation)
    /// is honored by the CALLER (`WorkQueueRunner`), which posts the returned
    /// text into `target_conversation_id`, not by anything in this method.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_subagent(
        &self,
        system_prompt: &str,
        tools_allowlist: &[String],
        provider_id: &str,
        model: &str,
        profile: &str,
        binding: Binding,
        task: &str,
    ) -> Result<String> {
        let belt: std::collections::HashSet<String> = tools_allowlist.iter().cloned().collect();
        let restricted = Arc::new(self.tools.restricted(&belt));
        let mut sub = AgentLoop::new(
            self.gate.clone(),
            Arc::clone(&self.model_manager),
            Arc::clone(&self.storage),
            restricted,
        );
        // The helper's sub-loop uses the same model transport as the parent —
        // in production this is always `None` (real `ModelClient`, unchanged);
        // in tests it carries the injected fake so B6/B7 can drive the helper
        // path end-to-end.
        sub.streamer_override.clone_from(&self.streamer_override);

        let db = self
            .storage
            .open_profile(profile)
            .context("run_subagent: opening profile for the ephemeral sub-conversation")?;
        let now = chrono::Utc::now().timestamp();
        let sub_conv_id = Uuid::new_v4().to_string();
        // Keep the auto-generated name short and simple — this conversation is
        // scratch space, never surfaced as a first-class chat in the sidebar.
        // `.chars().take(30)` (not a byte-index slice) so a persona whose
        // prompt starts with a multi-byte character can never panic here.
        let title: String = system_prompt.chars().take(30).collect();
        let binding_str = match binding {
            Binding::Auto => "auto",
            Binding::Public => "public",
            Binding::Private => "private",
        };
        db.create_conversation(&crate::storage::Conversation {
            id: sub_conv_id.clone(),
            name: format!("⟳ {title}"),
            pinned: false,
            binding: binding_str.to_string(),
            folder_id: None,
            color: None,
            created_at: now,
            updated_at: now,
        })
        .context("run_subagent: creating the ephemeral sub-conversation")?;

        // Frame the persona: v1 leads the first user turn with the system
        // prompt (see doc comment above for why this isn't a real system-role
        // override yet).
        let content = format!("{system_prompt}\n\n---\nTask:\n{task}");
        let sink: Arc<dyn ResultSink> = Arc::new(crate::agent::result_sink::HeadlessSink);

        let result = sub
            .process_message(
                content,
                sub_conv_id.clone(),
                binding,
                provider_id.to_string(),
                model.to_string(),
                profile.to_string(),
                crate::hooks::SessionMode::Normal,
                &sink,
            )
            .await;

        // Wave 4.3c review fix: the sub-conversation is scratch space, not a
        // first-class chat — delete it (its messages cascade via the FK) so
        // delegated helper runs don't pile up junk conversations in the sidebar
        // or grow storage unboundedly. Best-effort (the result is already
        // captured), and runs whether the helper succeeded or errored.
        let _ = db.delete_conversation(&sub_conv_id);
        result
    }

    /// Run one cron job's prompt as an unattended, HEADLESS, LOCAL-only turn
    /// (Wave 4.4). Privacy-safe by default: an unattended scheduled job must
    /// never egress, so it always uses a `local && private` model and a
    /// `Private` binding, and its tools run headless (a Dangerous/External call
    /// needing an `Ask` is denied this round, never a background prompt). The
    /// prompt + result persist in `target_conv` (a fresh dedicated conversation
    /// is created when the job names none), so the user can see what ran.
    pub async fn run_cron(
        &self,
        prompt: &str,
        profile: &str,
        target_conv: Option<String>,
    ) -> Result<String> {
        let local = self
            .model_manager
            .list_providers()
            .into_iter()
            .find(|p| p.is_local() && p.is_private())
            .ok_or_else(|| anyhow!("no local model available for an unattended cron job"))?;
        let client = self
            .model_manager
            .get_client(&local.id)
            .ok_or_else(|| anyhow!("cron: local provider has no client"))?;
        let model = match client.list_models().await {
            Ok(mut ms) if !ms.is_empty() => ms.remove(0),
            _ => anyhow::bail!("cron: local provider lists no models"),
        };

        let headless = Arc::new(self.tools.headless());
        let sub = AgentLoop::new(
            self.gate.clone(),
            Arc::clone(&self.model_manager),
            Arc::clone(&self.storage),
            headless,
        );

        let db = self.storage.open_profile(profile)?;
        let now = chrono::Utc::now().timestamp();
        // Deliver into the job's target conversation, or a fresh dedicated one.
        let conv_id = match target_conv {
            Some(id) if db.get_conversation(&id)?.is_some() => id,
            _ => {
                let id = Uuid::new_v4().to_string();
                let title: String = prompt.chars().take(30).collect();
                db.create_conversation(&crate::storage::Conversation {
                    id: id.clone(),
                    name: format!("⏰ {title}"),
                    pinned: false,
                    binding: "private".to_string(),
                    folder_id: None,
                    color: None,
                    created_at: now,
                    updated_at: now,
                })?;
                id
            }
        };
        let sink: Arc<dyn ResultSink> = Arc::new(crate::agent::result_sink::HeadlessSink);
        sub.process_message(
            prompt.to_string(),
            conv_id,
            Binding::Private,
            local.id,
            model,
            profile.to_string(),
            crate::hooks::SessionMode::Normal,
            &sink,
        )
        .await
    }

    /// Plan a redact-and-send for the current message, or `None` if it can't be
    /// done safely. Returns `Some(redaction)` only when: (1) the profile has
    /// redaction enabled, (2) the classification carries redactable VALUE spans,
    /// (3) blacking them out and **re-classifying the result** yields a clean
    /// (Public) verdict under the same thresholds. The re-classify is the
    /// load-bearing safety check — redaction alone is never trusted; if the
    /// redacted text is still non-Public (a proprietary cue, a model-detected
    /// disclosure, or a value the redaction missed), this returns `None` and the
    /// caller keeps the turn local.
    pub(crate) fn plan_redaction(
        &self,
        profile: &str,
        content: &str,
        classification: Option<&crate::classifier::Classification>,
        cfg: &crate::classifier::ClassifierConfig,
    ) -> Option<crate::classifier::Redaction> {
        // Redaction toggle (default on). A read error → treat as OFF (safe:
        // keep the turn local rather than risk a redact-and-send).
        let redaction_on = self
            .storage
            .open_profile(profile)
            .and_then(|db| db.redaction_enabled())
            .unwrap_or(false);
        if !redaction_on {
            return None;
        }

        // A per-turn random nonce makes the placeholders unforgeable, so
        // untrusted tool/web content can't inject a `[REDACTED:…]` string that
        // rehydration would blindly expand into the user's real value.
        let nonce = Uuid::new_v4().simple().to_string();
        let redaction = crate::classifier::redact(content, &classification?.spans, &nonce);
        if !redaction.is_redacted() {
            return None; // nothing redactable (proprietary/model-only) → local
        }
        // Re-classify the redacted REMAINDER — the load-bearing safety check:
        // only a clean (Public) verdict means the safe part may go to cloud.
        // The placeholders themselves must not be scored (the nonce can look
        // ID-ish to the rules layer), so they're normalized to a benign token
        // first; this isolates the actual remaining text. The text SENT still
        // carries the real (nonce'd) placeholders.
        let mut probe = redaction.redacted_text.clone();
        for r in &redaction.replacements {
            probe = probe.replace(&r.placeholder, " redacted ");
        }
        let recheck = self.gate.check(&Binding::Auto, &probe, true, cfg);
        if matches!(recheck, GateDecision::Allow) {
            Some(redaction)
        } else {
            None
        }
    }

    /// True when every already-persisted turn in the conversation is safe to
    /// replay to a cloud model (each classifies Public under `cfg`). Redact-and-
    /// send builds the outgoing prompt from the full history, so a single prior
    /// private turn would leak if we sent to cloud — this is the guard that
    /// keeps the WHOLE payload safe, not just the freshly-redacted message.
    /// Conservative by design: any non-Public prior turn (or a read error)
    /// returns `false`, and the turn stays local.
    pub(crate) fn conversation_is_cloud_safe(
        &self,
        profile: &str,
        conversation_id: &str,
        cfg: &crate::classifier::ClassifierConfig,
    ) -> bool {
        // A cold full scan re-classifies every prior turn (each a potential ONNX
        // pass) while holding the per-conversation lock, so bound the FIRST (uncached)
        // scan of a very long conversation — beyond this, fail closed (stay
        // local) rather than stall the send. This only bites a cold scan of a
        // huge conversation (e.g. right after restart); once cached, growth is
        // verified incrementally with no cap, so an ordinary long-but-benign
        // cloud chat is NOT forced local.
        const COLD_SCAN_CAP: usize = 200;

        let Ok(db) = self.storage.open_profile(profile) else {
            return false;
        };
        let Ok(messages) = db.list_messages_by_conversation(conversation_id) else {
            return false;
        };
        let n = messages.len();

        // Classify one message for cloud replay: empty content is trivially safe;
        // otherwise it must classify Allow under `cfg` on a cloud endpoint. Note
        // this re-checks CONTENT, not the persisted routing_decision (a private
        // message allowed on a LOCAL endpoint is still unsafe for cloud).
        let msg_safe = |m: &Message| {
            m.content.trim().is_empty()
                || matches!(
                    self.gate.check(&Binding::Auto, &m.content, true, cfg),
                    GateDecision::Allow
                )
        };

        let mut cache = self.cloud_safe_cache.lock();
        if let Some(entry) = cache.get(conversation_id) {
            if entry.cfg == *cfg && entry.verified_count <= n {
                if !entry.safe {
                    // A prior private turn is permanent (history is append-only),
                    // so the conversation stays unsafe without re-scanning.
                    return false;
                }
                if entry.verified_count == n {
                    return true; // nothing new to check
                }
                // Still safe so far; re-check ONLY the turns added since last time.
                let safe = messages[entry.verified_count..].iter().all(msg_safe);
                cache.insert(
                    conversation_id.to_string(),
                    CloudSafeEntry {
                        safe,
                        verified_count: n,
                        cfg: *cfg,
                    },
                );
                return safe;
            }
            // cfg changed (or an impossible shrink) ⇒ discard and re-scan below.
        }

        // Cold scan. Bound its cost; a too-long cold scan fails closed (local).
        if n > COLD_SCAN_CAP {
            return false;
        }
        let safe = messages.iter().all(msg_safe);
        cache.insert(
            conversation_id.to_string(),
            CloudSafeEntry {
                safe,
                verified_count: n,
                cfg: *cfg,
            },
        );
        safe
    }

    /// Assemble this turn's memory context (PLAN §9): the always-loaded curated
    /// summary plus up to a few relevance-gated snippets for the current
    /// message, **guard-wrapped as untrusted content** (a poisoned memory can't
    /// forge an instruction). Endpoint-aware — private-local facts are included
    /// only on a non-cloud turn (`allow_private = !is_cloud`); a cloud turn
    /// never queries the private store. Profile-scoped for the relevance
    /// snippets. Returns the wrapped block + the number of relevance snippets
    /// (for the non-silent "recalled" event), or `None` when there's nothing to
    /// inject. Reads are best-effort — a storage error yields no injection, it
    /// never blocks the send.
    ///
    /// The relevance gate is the HYBRID search (PLAN §9): the keyword (FTS)
    /// lane — a snippet must share search tokens with the message — fused with
    /// the meaning (sqlite-vec) lane, which is distance-gated at
    /// [`crate::storage::SEMANTIC_MAX_DIST_INJECT`] so only genuinely-near
    /// facts inject, capped at `AUTO_RECALL_LIMIT`. Without an installed
    /// embedder the meaning lane is skipped (keyword-only, as before).
    /// Combined memory context — the always-loaded curated summary plus the
    /// per-message relevance snippets, as one guard-wrapped block. Retained as a
    /// thin composition of the two split assemblers ([`Self::assemble_curated_summary`]
    /// + [`Self::assemble_relevance_snippets`]); the live loop uses the split
    /// pieces directly (Wave 3.3 cache-shaped assembly: summary → stable prefix,
    /// snippets → volatile tail), while this keeps the pre-split callers/tests
    /// intact. `recalled` counts ONLY the relevance snippets.
    pub(crate) fn assemble_memory_context(
        &self,
        conversation_id: &str,
        profile: &str,
        content: &str,
        is_cloud: bool,
    ) -> Option<(String, usize)> {
        let summary = self.assemble_curated_summary(conversation_id, profile, is_cloud);
        let snippets =
            self.assemble_relevance_snippets(conversation_id, profile, content, is_cloud);
        match (summary, snippets) {
            (None, None) => None,
            (Some(s), None) => Some((s, 0)),
            (None, Some((sn, n))) => Some((sn, n)),
            (Some(s), Some((sn, n))) => Some((format!("{s}\n\n{sn}"), n)),
        }
    }

    /// The frozen curated-summary candidate set for a conversation (Wave 1.3
    /// snapshot) — endpoint-INdependent (the per-turn cloud/local filter is the
    /// caller's job). Shared by [`Self::assemble_curated_summary`] and
    /// [`Self::assemble_relevance_snippets`] (the latter needs the summary's
    /// fact ids to dedup). Best-effort: a storage error yields an empty set,
    /// never blocks the send. Bounded cache (clear-and-re-freeze on overflow).
    fn frozen_summary_facts(
        &self,
        conversation_id: &str,
        profile: &str,
    ) -> Vec<(MemoryFact, MemoryBucket)> {
        const SUMMARY_LIMIT: usize = 8;
        const SUMMARY_CACHE_CAP: usize = 512;
        let Ok(mem) = self.storage.memory_db_for_profile(profile) else {
            return Vec::new();
        };
        let mut cache = self.summary_cache.lock();
        if let Some(c) = cache.get(conversation_id) {
            return c.clone();
        }
        if cache.len() >= SUMMARY_CACHE_CAP {
            cache.clear();
        }
        let c = mem
            .curated_summary_with_buckets(profile, SUMMARY_LIMIT)
            .unwrap_or_default();
        cache.insert(conversation_id.to_string(), c.clone());
        c
    }

    /// The always-loaded curated summary as a guard-wrapped system block, or
    /// `None` when the profile has no summary facts to show this turn. The
    /// frozen snapshot (Wave 1.3) is filtered per turn by endpoint — a cloud
    /// turn drops private-local facts (the wall), a local turn keeps them. Wrapped
    /// with the DETERMINISTIC [`guard_wrap_stable`](crate::tools::calling::guard_wrap_stable)
    /// (seed = conversation id) so the block is byte-identical across a
    /// conversation's turns — the stable prompt PREFIX for KV/prompt-cache reuse.
    pub(crate) fn assemble_curated_summary(
        &self,
        conversation_id: &str,
        profile: &str,
        is_cloud: bool,
    ) -> Option<String> {
        const SUMMARY_LIMIT: usize = 8;
        let allow_private = !is_cloud;
        let candidates = self.frozen_summary_facts(conversation_id, profile);
        // Per-turn privacy filter over the frozen set: this only ever REMOVES
        // facts, so the wall holds even though the snapshot froze the full set.
        let summary: Vec<&MemoryFact> = candidates
            .iter()
            .filter(|(_, b)| allow_private || *b == MemoryBucket::Shared)
            .map(|(f, _)| f)
            .take(SUMMARY_LIMIT)
            .collect();
        if summary.is_empty() {
            return None;
        }
        let mut block = String::from("What you remember about the user:\n");
        for f in &summary {
            block.push_str("- ");
            block.push_str(f.content.trim());
            block.push('\n');
        }
        // Deterministic wrap so the same conversation's summary is byte-stable
        // turn-over-turn (cache-shaped prefix). Seed = conversation id (an
        // unguessable uuid a fact author never saw; neutralize still strips
        // forged markers regardless of the nonce).
        Some(crate::tools::calling::guard_wrap_stable(
            "your saved memory",
            block.trim(),
            conversation_id,
        ))
    }

    /// The per-message relevance snippets as a guard-wrapped block plus their
    /// count, or `None` when nothing clears the relevance gate. Volatile (differs
    /// per message) → lives in the TAIL, next to the current turn, NOT in the
    /// cache-stable prefix. Deduped against the always-loaded summary so a fact
    /// isn't shown twice. Ordinary (random-nonce) `guard_wrap` is fine here.
    pub(crate) fn assemble_relevance_snippets(
        &self,
        conversation_id: &str,
        profile: &str,
        content: &str,
        is_cloud: bool,
    ) -> Option<(String, usize)> {
        const SUMMARY_LIMIT: usize = 8;
        const AUTO_RECALL_LIMIT: usize = 3;
        let allow_private = !is_cloud;
        let mem = self.storage.memory_db_for_profile(profile).ok()?;

        // The summary fact ids (post endpoint-filter) to dedup against.
        let candidates = self.frozen_summary_facts(conversation_id, profile);
        let summary_ids: Vec<String> = candidates
            .iter()
            .filter(|(_, b)| allow_private || *b == MemoryBucket::Shared)
            .map(|(f, _)| f.id.clone())
            .take(SUMMARY_LIMIT)
            .collect();

        // Meaning-lane query vector — only when semantic search is enabled for
        // this profile (Wave 1.2) AND the embedder loads. Either off ⇒ keyword
        // only. Any failure degrades to keyword-only; never blocks the send.
        let embedder = if self.semantic_search_enabled(profile) {
            self.embedder.as_ref().and_then(|h| h.get())
        } else {
            None
        };
        let query_vec = embedder.as_ref().and_then(|e| e.embed_query(content).ok());
        let hits = mem
            .search_memory_scoped_hybrid(
                content,
                query_vec.as_deref(),
                profile,
                allow_private,
                crate::storage::SEMANTIC_MAX_DIST_INJECT,
                AUTO_RECALL_LIMIT,
            )
            .unwrap_or_default();

        let fresh: Vec<_> = hits
            .into_iter()
            .filter(|h| !summary_ids.iter().any(|id| id == &h.fact.id))
            .collect();
        if fresh.is_empty() {
            return None;
        }
        let mut block = String::from("Possibly relevant saved notes for this message:\n");
        for h in &fresh {
            block.push_str("- ");
            block.push_str(h.fact.content.trim());
            block.push('\n');
        }
        let wrapped = crate::tools::calling::guard_wrap("your saved memory", block.trim());
        Some((wrapped, fresh.len()))
    }

    /// Wave 3.5 — the pre-compaction flush (PLAN §9 trigger #2). Called right
    /// before a compacted send drops the `trimmed` older turns; sweeps them for
    /// durable facts and saves them BEFORE they leave the model-facing history.
    ///
    /// Runs UNDER the per-conversation lock, so it does ONLY cheap synchronous work here —
    /// pick the not-yet-swept turns, mark them, and `spawn` a detached task for
    /// the (local-model) extraction + saves. A failure or slow local model can
    /// never delay or fail the send. Disabled (no-op) until a flush classifier is
    /// wired (pre-3.5 behavior) or when no local model is available — in the
    /// latter case nothing is marked, so a later round/restart still catches it.
    fn on_pre_compaction(
        &self,
        conversation_id: &str,
        profile: &str,
        trimmed: &[ChatMessage],
        sink: &Arc<dyn ResultSink>,
    ) {
        // The flush needs a classifier (to route safely) AND a local model.
        let Some(classifier) = &self.flush_classifier else {
            return;
        };
        if !self.fact_extractor.available() {
            return; // no local model → skip WITHOUT marking (catch it later)
        }
        // Pick the turns not yet swept, and mark them swept synchronously (before
        // the next round runs under the still-held stream lock) — at-most-once.
        let unswept = self.take_unswept_for_flush(conversation_id, trimmed);
        if unswept.is_empty() {
            return;
        }
        // Detached, best-effort. Nothing here is awaited by the send.
        let extractor = Arc::clone(&self.fact_extractor);
        let classifier = Arc::clone(classifier);
        let storage = Arc::clone(&self.storage);
        let embedder = self.embedder.clone();
        let profile = profile.to_string();
        let conversation_id = conversation_id.to_string();
        // Clone the `Arc`, not the trait object — this task runs detached
        // (`tauri::async_runtime::spawn` needs a `'static` future), so the
        // sink must outlive `on_pre_compaction`'s own stack frame.
        let sink = Arc::clone(sink);
        let now = chrono::Utc::now().timestamp();
        tauri::async_runtime::spawn(async move {
            match crate::agent::memory_flush::run_flush(
                extractor,
                classifier,
                storage,
                embedder,
                profile,
                conversation_id.clone(),
                unswept,
                now,
            )
            .await
            {
                Ok(n) if n > 0 => sink.memory_event(&conversation_id, "remembered", n),
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!(target: "lhp::compaction", error = %e, "pre-compaction flush failed")
                }
            }
        });
    }

    /// Wave 3.5 trigger #3 — the new-chat consolidation nudge. Fire-and-forget:
    /// when a new conversation is created, sweep the most-recent PRIOR
    /// conversation for durable facts the on-stream flush missed (a short chat
    /// that never compacted). Cheap sync guard here, then a detached task does
    /// the DB reads + local-model extraction + saves. Disabled (no-op) until a
    /// flush classifier is wired or when no local model is available.
    pub(crate) fn consolidate_on_new_chat(&self, profile: &str, new_conversation_id: &str) {
        // Two write-triggers mine the prior conversation on a new chat: the Wave
        // 3.5 memory nudge and the Wave 4.2 skill reflection. They run in ONE
        // detached task, SEQUENTIALLY — never as two concurrent tasks — so they
        // can't both touch the shared, `!Sync` `global.db` Connection at once.
        // (Each is still independently gated; either or both may be enabled.)
        let flush = match &self.flush_classifier {
            Some(classifier) if self.fact_extractor.available() => Some((
                Arc::clone(&self.fact_extractor),
                Arc::clone(classifier),
                self.embedder.clone(),
                Arc::clone(&self.flush_marks),
            )),
            _ => None,
        };
        // Cheap in-memory gate here (a local model exists); the DB-backed toggle
        // is read INSIDE the task, so no global.db read happens off-task.
        let reflect = match &self.skill_drafter {
            Some(drafter) if drafter.available() => {
                Some((Arc::clone(drafter), Arc::clone(&self.reflect_marks)))
            }
            _ => None,
        };
        if flush.is_none() && reflect.is_none() {
            return;
        }
        let storage = Arc::clone(&self.storage);
        let profile = profile.to_string();
        let new_conversation_id = new_conversation_id.to_string();
        let now = chrono::Utc::now().timestamp();
        tauri::async_runtime::spawn(async move {
            if let Some((extractor, classifier, embedder, flush_marks)) = flush {
                if let Err(e) = crate::agent::memory_flush::run_new_chat_nudge(
                    extractor,
                    classifier,
                    Arc::clone(&storage),
                    embedder,
                    flush_marks,
                    profile.clone(),
                    new_conversation_id.clone(),
                    now,
                )
                .await
                {
                    tracing::debug!(target: "lhp::compaction", error = %e, "new-chat consolidation nudge failed");
                }
            }
            if let Some((drafter, reflect_marks)) = reflect {
                // Read the (global.db) toggle here, in-task and after the flush —
                // never concurrently with the flush's own global access.
                if storage.global().skill_reflect_enabled() {
                    if let Err(e) = crate::agent::skill_reflect::run_new_chat_reflect(
                        drafter,
                        storage,
                        reflect_marks,
                        profile,
                        new_conversation_id,
                        now,
                    )
                    .await
                    {
                        tracing::debug!(target: "lhp::skills", error = %e, "new-chat skill reflection failed");
                    }
                }
            }
        });
    }

    /// The synchronous, at-most-once core of the pre-compaction flush: the turns
    /// in `trimmed` not yet swept for this conversation, marking them swept in
    /// the same locked critical section (so a concurrent next round can't
    /// re-sweep). Extracted from `on_pre_compaction` so the dedup is testable
    /// without a Tauri `AppHandle`.
    pub(crate) fn take_unswept_for_flush(
        &self,
        conversation_id: &str,
        trimmed: &[ChatMessage],
    ) -> Vec<ChatMessage> {
        const FLUSH_MARKS_CAP: usize = 512;
        let mut marks = self.flush_marks.lock();
        if marks.len() >= FLUSH_MARKS_CAP {
            marks.clear();
        }
        let swept = marks.entry(conversation_id.to_string()).or_default();
        let unswept = crate::agent::memory_flush::select_unswept(trimmed, swept);
        for m in &unswept {
            swept.insert(crate::agent::memory_flush::identity(m));
        }
        unswept
    }

    /// Whether this profile has the meaning-lane (semantic memory search)
    /// enabled (Wave 1.2). Defaults to `true` (on) when settings can't be read,
    /// matching the pre-Wave-1 behavior.
    fn semantic_search_enabled(&self, profile: &str) -> bool {
        self.storage
            .open_profile(profile)
            .and_then(|db| db.memory_settings())
            .map(|s| s.semantic_search_enabled)
            .unwrap_or(true)
    }

    // ── helpers ─────────────────────────────────────────────────────────

    /// The endpoint a privacy reroute lands on: a provider that is both
    /// `ProviderKind::Local` *and* private by base URL. `None` if no local
    /// model is set up (the caller then fails loudly — never onto cloud).
    ///
    /// # Selection rule, stated
    ///
    /// **The first `is_local() && is_private()` provider in
    /// `ModelManager::list_providers()` order — which the storage layer emits
    /// `ORDER BY name` (`storage/global.rs`). That is: alphabetically first
    /// among the local private endpoints.**
    ///
    /// Say it out loud because it *is* arbitrary. With two local endpoints
    /// registered, which one serves a rerouted turn is decided by their names,
    /// not by anything the user asked for. What it is NOT arbitrary about is
    /// the trust zone: every candidate the predicate admits is private by base
    /// URL, so a reroute can only ever move a turn *toward* the private zone.
    /// That is why this is a legitimate endpoint source and the user's
    /// explicit picker choice being silently replaced by
    /// `providers.first()` was not.
    ///
    /// Routed through [`enforce_local_routing`] rather than repeating the
    /// predicate here. It was a hand-rolled `find(..)` over the same list —
    /// same rule, but a second copy of it, which is how two "local" definitions
    /// quietly drift apart. `hooks::routing` is the one place allowed to turn a
    /// local-only requirement into an endpoint, and this is now genuinely that
    /// one place; see the invariant on `enforce_local_routing`.
    fn find_local_provider(&self) -> Option<Provider> {
        let candidates = self.model_manager.list_providers();
        enforce_local_routing(
            &RoutingRequirement::LocalRequired {
                reason: "privacy reroute: this turn must stay on a local endpoint".to_string(),
            },
            &candidates,
        )
        .ok()
        .cloned()
    }

    /// The borrowed lazy-runner reference for the free-fn reroute path.
    #[cfg(feature = "local-runner")]
    fn local_runner_ref(&self) -> LocalRunnerRef<'_> {
        self.local_runner
            .as_deref()
            .map(|ctx| (ctx, self.storage.as_ref()))
    }
    #[cfg(not(feature = "local-runner"))]
    fn local_runner_ref(&self) -> LocalRunnerRef<'_> {
        None
    }

    /// [`Self::find_local_provider`], with the M8 S4 lazy-spawn seam behind it:
    /// when the snapshot has no local provider but a downloaded `ready` model
    /// exists and the bundled sidecar is wired, bring the sidecar up and
    /// register it — so `RouteLocal` stops failing on a machine that has a
    /// model but no external runner. Failure to start degrades to `None`
    /// (the caller's existing refuse-loudly path), never a panic or a hang.
    async fn find_or_start_local_provider(&self) -> Option<Provider> {
        if let Some(p) = self.find_local_provider() {
            return Some(p);
        }
        #[cfg(feature = "local-runner")]
        if let Some(ctx) = &self.local_runner {
            match crate::models::runner::ensure_running(
                &ctx.supervisor,
                &self.model_manager,
                &self.storage,
                &ctx.paths,
                None,
                None,
                None,
            )
            .await
            {
                Ok(p) => return Some(p),
                Err(e) => {
                    tracing::info!(
                        target: "lhp::runner",
                        error = %e,
                        "lazy sidecar start unavailable — falling through to refuse-loudly"
                    );
                }
            }
        }
        None
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
        // Partial-delegation redaction (PLAN §11). `Some` ⇒ the current user
        // message is sent to the model as `redaction.redacted_text` (sensitive
        // value spans blacked out) while the ORIGINAL `content` is persisted to
        // the transcript, and the model's reply is rehydrated back to the real
        // values before it's persisted/returned. `None` ⇒ ordinary send.
        redaction: Option<crate::classifier::Redaction>,
        // Q11: the conversation's permission mode, threaded into `ExecCtx` for
        // this turn's tool calls.
        session_mode: crate::hooks::SessionMode,
        // C7: cooperative cancellation — the SSE drain loop breaks when this
        // fires (a `cancel_message` IPC flipped it), and the persisted turn is
        // marked `aborted`.
        cancel_token: tokio_util::sync::CancellationToken,
        sink: &Arc<dyn ResultSink>,
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
            // Stamp the zone this turn ran in, from the SAME `is_cloud` the
            // gate was given — never recomputed later from the live registry.
            endpoint_zone: Some(TrustZone::from_is_cloud(is_cloud).as_str().to_string()),
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
        // Cache-shaped assembly (Wave 3.3, PLAN §3): a byte-stable PREFIX
        // (catalog + curated summary) for KV/prompt-cache reuse, then the
        // trimmable prior turns, then the VOLATILE tail (relevance snippets +
        // the current turn). Tool-result rows are persisted with role "tool" but
        // replayed to the model as "user" (the fenced dialect carries tool
        // results as plain text; avoids the native "tool" role OpenAI-compatible
        // servers reject without a matching tool_call_id).
        let mut history: Vec<ChatMessage> = Vec::new();
        // Tier 0 (stable prefix): the tool catalog.
        let catalog = self.tools.catalog();
        if !catalog.is_empty() {
            history.push(ChatMessage::system(catalog));
        }
        // Tier 1 (stable prefix): the always-loaded curated summary. Byte-stable
        // across the conversation's turns (deterministic wrap, frozen snapshot),
        // so the prompt prefix is reused turn-over-turn. Endpoint-aware
        // (private-local facts dropped on a cloud turn) + profile-scoped.
        if let Some(summary) = self.assemble_curated_summary(&conversation_id, &profile, is_cloud) {
            history.push(ChatMessage::system(summary));
        }
        // The trimmable middle: prior turns — every persisted message EXCEPT the
        // current user message (it rides the tail below with its sent_content, so
        // the model sees the redacted remainder, not the stored original).
        for m in profile_db
            .list_messages_by_conversation(&conversation_id)
            .context("load conversation history")?
        {
            if m.id == user_message.id {
                continue;
            }
            let (role, content) = if m.role == "tool" {
                ("user".to_string(), m.content)
            } else if m.routing_decision.as_deref() == Some("delegated") {
                // Wave 4.3c review fix: a delegated helper's result is
                // model-generated from possibly-untrusted sources (a helper can
                // fetch the web), so when it re-enters the MAIN agent's context
                // it must be neutralized like tool output — never replayed as a
                // trusted assistant turn it could be steered by. The stored
                // message stays clean (the user sees the plain answer); only the
                // model-facing copy is guard-wrapped, as untrusted `user` input.
                (
                    "user".to_string(),
                    crate::tools::calling::guard_wrap("delegated helper result", &m.content),
                )
            } else {
                (m.role, m.content)
            };
            history.push(ChatMessage { role, content });
        }
        // Tail (volatile): the current user turn. When redacting, the model
        // sees the redacted remainder — never the original sensitive spans —
        // even though the transcript kept the original. The per-message
        // relevance snippets (if any) are PREPENDED into this same user message
        // as a guard-wrapped block, so (a) there are never two consecutive
        // user-role messages on the wire, and (b) the snippets + question are
        // one pinned unit. A recall fires a non-silent `memory:event`.
        let sent_content = redaction
            .as_ref()
            .map(|r| r.redacted_text.clone())
            .unwrap_or_else(|| content.clone());
        let current_turn = match self.assemble_relevance_snippets(
            &conversation_id,
            &profile,
            &content,
            is_cloud,
        ) {
            Some((snippets, recalled)) => {
                if recalled > 0 {
                    sink.memory_event(&conversation_id, "recalled", recalled);
                }
                format!("{snippets}\n\n{sent_content}")
            }
            None => sent_content,
        };
        history.push(ChatMessage::user(current_turn));
        // Wave 3.3: the index of the current user turn. Compaction pins
        // everything from here forward (the question + whatever the tool loop
        // appends), so the user's actual request can never be trimmed no matter
        // how deep the tool loop goes.
        let pinned_from = history.len() - 1;

        // `reads` is injected by the dispatcher (which owns the shared handle),
        // so the loop leaves it `None` here. `allow_private_memory` mirrors the
        // endpoint: private-local facts are readable only on a non-cloud turn
        // (the dispatcher re-stamps this per call with the CURRENT is_cloud, so
        // a mid-turn reroute-to-local is honored — this is the base value).
        let exec_ctx = ExecCtx {
            conversation_id: conversation_id.clone(),
            profile: profile.clone(),
            reads: None,
            allow_private_memory: !is_cloud,
            // Q11: the conversation's permission mode, applied to every tool
            // call this turn makes (SessionModeHook). The dispatcher inherits it
            // via `..ctx.clone()`, so a mid-turn reroute preserves it.
            session_mode,
            // Wave 4.3c: this turn's own provider/model, so `delegate`'s
            // `resolve_seat` inherit-fallback has something to inherit when a
            // persona's seat is unbound. Stamped once here (not re-stamped per
            // round like `is_cloud`/`allow_private_memory` below) — a mid-turn
            // reroute-to-local doesn't change what "the caller's own model"
            // meant when the turn started.
            caller_provider_id: provider.id.clone(),
            caller_model: model.clone(),
            // Wave 4.3c: this turn's privacy binding, so `delegate` makes a
            // helper inherit it (never run weaker than a Private parent). The
            // dispatcher re-stamps it from the `binding` arg at the tool.run
            // boundary too; both are the same value.
            binding,
            // B4: the profile's classifier config, so the dispatcher gates tool
            // actions at the SAME strictness as this profile's chat messages
            // (loaded once per turn, best-effort → defaults; matches the
            // message-egress load above).
            classifier_cfg: self
                .storage
                .open_profile(&profile)
                .and_then(|db| db.classifier_config())
                .unwrap_or_default(),
        };

        // Bound the tool loop so a model that keeps calling tools can't run
        // away. MAX_TOOL_ROUNDS tool rounds + one final answer turn.
        const MAX_TOOL_ROUNDS: usize = 6;
        let mut final_text = String::new();

        // Q4 do-now item 2: reset the dispatcher's per-run budget + repeat
        // detection ring at the start of every user message. The dispatcher
        // then enforces ceilings and cascades inside `run_turn`.
        //
        // Scoped to THIS conversation: since M-08 (P14) replaced the global
        // stream lock with a per-conversation one, another conversation may be
        // mid-run against the same shared `Arc<ToolDispatcher>`, and starting
        // this run must not wipe its budget, repeat ring or journal nonce.
        self.tools.begin_run(&conversation_id);

        // Q1: the native tools spec is rendered once; whether a given round
        // USES it depends on the round's current provider (a mid-turn local
        // reroute may land on a fenced-dialect endpoint).
        let native_spec = self.tools.native_tools_spec();

        for round in 0..=MAX_TOOL_ROUNDS {
            // M-09 (mid-run ceiling): an UNATTENDED run re-checks the cap at the
            // TOP OF EVERY ROUND, not just once before dispatch.
            //
            // `work_runner`'s pre-dispatch check-and-reserve only bounds how many
            // helpers START; it says nothing about how much ONE helper spends
            // once it is running. Cost is booked per round further down this same
            // loop, so re-reading the ledger here is what actually stops a single
            // background helper from running the profile past its cap for the
            // whole `HELPER_DEADLINE` window.
            //
            // The residual overrun is therefore ONE round's cost (the round that
            // crosses the cap is already paid for by the time we can see it) —
            // bounded, not zero. A true zero-overrun cap needs pre-call cost
            // RESERVATION, which the provider APIs don't offer.
            //
            // Attended turns are untouched: a human is only WARNED (above),
            // never hard-blocked mid-thought. Fail-closed on a ledger read error,
            // matching `work_runner::budget_check_and_reserve`'s direction — an
            // unattended run whose spend can't be verified stops.
            if !self.tools.is_attended() {
                let since = crate::hooks::budget::month_start_ts(chrono::Utc::now());
                let verdict = match (
                    profile_db.budget_cap(),
                    profile_db.usage_summary_since(since),
                ) {
                    (Ok(cap), Ok(sum)) => crate::hooks::budget::evaluate(cap, &sum, false),
                    _ => crate::hooks::budget::BudgetVerdict::Halt(
                        "budget check unavailable (couldn't read the spend ledger) — \
                             halting to fail closed"
                            .to_string(),
                    ),
                };
                if let crate::hooks::budget::BudgetVerdict::Halt(reason) = verdict {
                    tracing::warn!(
                        target: "lhp::budget",
                        profile = %profile,
                        conversation = %conversation_id,
                        round,
                        reason = %reason,
                        "unattended run halted mid-loop by the budget governor"
                    );
                    anyhow::bail!("budget: {reason}");
                }
            }
            let assistant_id = Uuid::new_v4().to_string();
            // Per-round transport: native structured tool calls when this
            // round's endpoint supports them (and any tools exist), the
            // fenced dialect otherwise.
            let native_mode = provider.supports_native_tools && native_spec.is_some();
            // Wave 3.3: compact the model-facing history to a char budget before
            // each send. Deterministic + prefix-stable (catalog + summary kept
            // byte-identical); the stored transcript is untouched. Runs EVERY
            // round, so the AGGREGATE history growth across a tool loop is
            // bounded (older rounds trim as it grows). `history` itself is left
            // intact for the loop's own appends. NOTE: the budget is a target,
            // not a hard cap — the pinned tail (the current turn onward) and any
            // single oversized recent message are kept WHOLE (never sliced, to
            // avoid bisecting a guard-wrap/redaction frame), so one huge tool
            // result can still exceed the budget on its own round.
            //
            // `keep_recent` is raised to cover the current user turn forward
            // (`pinned_from`), so the actual question is never trimmed however
            // deep the tool loop goes.
            let keep_recent = crate::agent::compaction::KEEP_RECENT_MESSAGES
                .max(history.len().saturating_sub(pinned_from));
            let compaction = crate::agent::compaction::compact_history(
                &history,
                crate::agent::compaction::COMPACT_BUDGET_CHARS,
                keep_recent,
            );
            if !compaction.trimmed.is_empty() {
                // Wave 3.5: about-to-be-trimmed turns are swept for durable facts
                // (async, local-model, best-effort) BEFORE they leave the wire.
                self.on_pre_compaction(&conversation_id, &profile, &compaction.trimmed, sink);
            }
            // B7: a test-injected streamer (None in production) drives the REAL
            // loop against a canned transport; the production arm below is
            // byte-identical to before.
            let mut sse = match &self.streamer_override {
                Some(fake) => fake
                    .stream(&model, compaction.sent)
                    .await
                    .with_context(|| format!("stream to provider {}", provider.id))?,
                None => client
                    .stream_chat_with_tools(
                        &model,
                        compaction.sent,
                        if native_mode {
                            native_spec.as_ref()
                        } else {
                            None
                        },
                    )
                    .await
                    .with_context(|| format!("stream_chat to provider {}", provider.id))?,
            };

            let mut assembled = String::new();
            let mut native_frags: Vec<crate::models::sse::ToolCallFragment> = Vec::new();
            // Wave 3.2: this round's reported token usage (if the endpoint sent
            // it), used to book a real cost to the ledger below.
            let mut round_usage: Option<(u32, u32)> = None;
            // C7: cooperative cancellation — pull the next SSE event OR observe a
            // cancel, whichever resolves first. `biased` makes the cancel branch
            // win a tie (favor responsiveness — cooperative check-and-yield at
            // the next natural await point, never preemptive).
            let mut was_cancelled = false;
            loop {
                let event = tokio::select! {
                    biased;
                    _ = cancel_token.cancelled() => {
                        was_cancelled = true;
                        break;
                    }
                    maybe = sse.next_event() => match maybe {
                        Some(e) => e,
                        None => break,
                    },
                };
                match event {
                    crate::models::sse::SseEvent::Delta(delta) => {
                        assembled.push_str(&delta);
                        sink.token(&conversation_id, &assistant_id, &delta);
                    }
                    crate::models::sse::SseEvent::ToolCalls(frags) => {
                        if native_mode {
                            native_frags.extend(frags);
                        } else {
                            // A server we didn't flag as native-capable sent
                            // structured calls anyway. We don't run them —
                            // the flag is the user-set capability contract —
                            // but say so loudly instead of silently dropping.
                            tracing::warn!(
                                target: "lhp::tools",
                                provider = %provider.id,
                                "endpoint streamed native tool_calls but supports_native_tools is off — ignored"
                            );
                        }
                    }
                    crate::models::sse::SseEvent::Error(msg) => {
                        sink.error(&conversation_id, &msg, "model");
                        anyhow::bail!("model stream error: {msg}");
                    }
                    crate::models::sse::SseEvent::Usage {
                        prompt_tokens,
                        completion_tokens,
                    } => {
                        round_usage = Some((prompt_tokens, completion_tokens));
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

            // If this turn was a redact-and-send, un-mask the placeholders in the
            // reply for what the USER sees and what we persist — but NOT in
            // `assembled`, which is what feeds back into the model-facing history
            // below (rehydrated originals must never travel back to the cloud).
            let persisted_content = match &redaction {
                Some(r) => crate::classifier::rehydrate(&assembled, &r.replacements),
                None => assembled.clone(),
            };

            // Persist this assistant turn.
            let assistant_message = Message {
                id: assistant_id,
                conversation_id: conversation_id.clone(),
                role: "assistant".to_string(),
                content: persisted_content.clone(),
                model: Some(model.clone()),
                provider_id: Some(provider.id.clone()),
                routing_decision: Some(routing_decision.to_string()),
                // The trust zone THIS round ran in, taken from the live
                // `is_cloud` that governed it — so a mid-turn reroute to a
                // local endpoint is recorded as local, and a cloud turn stays
                // cloud forever regardless of what happens to the provider
                // afterwards. This is the value the route badge renders; it is
                // never re-derived frontend-side.
                endpoint_zone: Some(TrustZone::from_is_cloud(is_cloud).as_str().to_string()),
                thinking_content: None,
                error: None,
                // C7: a cancelled turn is ALSO marked aborted (distinguishable
                // from a crash for the same reason as round-cap: a crash kills
                // the process before this row is ever written).
                aborted: is_round_cap_stop || was_cancelled,
                created_at: chrono::Utc::now().timestamp(),
            };
            profile_db
                .add_message(&assistant_message)
                .context("persist assistant message")?;

            // Wave 3.2: book this model call to the per-profile usage ledger
            // (PLAN §3). A local/on-device (or private) endpoint costs $0; a
            // cloud call we can't price is recorded as UNKNOWN (`None`) —
            // flagged, never guessed. Cost is driven by `is_cloud` (the real
            // routing, honoring a mid-turn reroute-to-local), while
            // `provider_kind`/`provider_id` record the endpoint that served this
            // round. Best-effort: a ledger write must never fail the turn.
            // Caveat (pre-existing): `model` is the turn's originally-selected
            // model id and is not re-stamped when a round reroutes to a local
            // endpoint — cost stays correct, but the per-row model label is
            // endpoint-approximate after a reroute.
            {
                let kind = match provider.kind {
                    crate::models::ProviderKind::Local => "local",
                    crate::models::ProviderKind::Cloud => "cloud",
                    crate::models::ProviderKind::Custom => "custom",
                };
                // Cost (PLAN §3, never a guess): a local/private endpoint is $0;
                // a cloud call is priced ONLY when the endpoint reported token
                // usage AND the model is in the pricing table — otherwise it's
                // recorded as unknown (`None`, "flying blind").
                let cost_usd = if !is_cloud {
                    Some(0.0)
                } else {
                    round_usage
                        .and_then(|(pt, ct)| crate::models::pricing::cost_usd(&model, pt, ct))
                };
                if let Err(e) = profile_db.record_usage(&crate::storage::UsageEvent {
                    id: Uuid::new_v4().to_string(),
                    conversation_id: Some(conversation_id.clone()),
                    model: model.clone(),
                    provider_id: Some(provider.id.clone()),
                    provider_kind: kind.to_string(),
                    cost_usd,
                    created_at: chrono::Utc::now().timestamp(),
                }) {
                    tracing::warn!(error = %e, "failed to book usage event to the ledger");
                }
            }
            final_text = persisted_content;

            // On the last permitted round, or once cancelled, stop without
            // dispatching more tools (C7: a cancelled turn never proceeds into
            // the next round's tool dispatch).
            if is_round_cap_stop || was_cancelled {
                break;
            }

            // Run the tool calls the model made THIS TURN. Two transports,
            // one downstream pipeline (Q1):
            //   - native: the provider attributed structured `tool_calls` to
            //     the assistant; we assemble the streamed fragments and NEVER
            //     run the fenced parser — a typed call block is something read
            //     content can't mint, so on a native turn there's no second
            //     listener for a forged fence.
            //   - fenced: `parse_tool_calls` reads ONLY `own_output` (typed
            //     `&OwnOutput`, never a tool result or prior turn) — the
            //     "parse only your own current output" rule, enforced at the
            //     type level.
            // A call that must stay on-device on this (cloud) endpoint is
            // resolved by `resolve_turn_outcome`, which may switch
            // provider/client/is_cloud/routing_decision to a local endpoint
            // for the rest of this turn.
            let turn_outcome = if native_mode {
                let calls = crate::tools::calling::assemble_native_calls(native_frags);
                if !calls.is_empty() {
                    tracing::debug!(target: "lhp::tools", n = calls.len(), "native tool-use turn");
                }
                self.tools
                    .run_turn_native(calls, &exec_ctx, binding, is_cloud)
                    .await
            } else {
                self.tools
                    .run_turn(&own_output, &exec_ctx, binding, is_cloud)
                    .await
            };
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
                    &|from, to, reason, to_is_bundled| {
                        sink.local_reroute(&conv_id, reason, from, to, to_is_bundled);
                    },
                    self.local_runner_ref(),
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
                        // Post-reroute `is_cloud`, matching the provider on
                        // this same row.
                        endpoint_zone: Some(
                            TrustZone::from_is_cloud(is_cloud).as_str().to_string(),
                        ),
                        thinking_content: None,
                        error: None,
                        aborted: false,
                        created_at: chrono::Utc::now().timestamp(),
                    };
                    profile_db
                        .add_message(&tool_message)
                        .context("persist tool message")?;

                    // Extend the working history and loop for another round.
                    // In native mode a tool-only turn can stream no text — a
                    // blank assistant message trips some strict servers, so
                    // substitute a short placeholder that keeps the turn
                    // coherent without inventing model content.
                    let assistant_turn = if assembled.trim().is_empty() {
                        "[called tools]".to_string()
                    } else {
                        assembled
                    };
                    history.push(ChatMessage::assistant(assistant_turn));
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
                // H-12 `ConfirmRequired` did NOT egress — it is a refusal
                // pending the user's one-send confirmation, so it logs
                // "private" like the other refusals.
                GateDecision::Block(_)
                | GateDecision::RouteLocal
                | GateDecision::ConfirmRequired { .. } => "private".to_string(),
            },
            // Confidence is the gate's confidence in its decision. The
            // gate doesn't surface a number directly, so we report 1.0
            // for hard allow/block and let `RouteLocal` carry the
            // classifier's underlying confidence via the tracing layer
            // (see `gate.log_decision`).
            confidence: match decision {
                GateDecision::Allow
                | GateDecision::Block(_)
                | GateDecision::ConfirmRequired { .. } => 1.0,
                GateDecision::RouteLocal => 0.8,
            },
            created_at: chrono::Utc::now().timestamp(),
        };
        profile_db.insert_trm_log(&entry)?;
        // Also keep the tracing layer happy (for operators tailing logs
        // without a DB connection).
        self.gate
            .log_decision(decision, message_hash, conversation_id);
        Ok(())
    }
}

// ── free fns ─────────────────────────────────────────────────────────────

/// Emit the non-silent memory signal (`memory:event`). Content-free: only a
/// kind + count, so a recalled/remembered fact's text never rides the event.
/// `pub(crate)` so the `remember` tool and the manual `save_memory` IPC can
/// fire the "remembered" variant through the same channel (Wave 1.4).
pub(crate) fn emit_memory_event(
    app: &AppHandle,
    conversation_id: &str,
    kind: &'static str,
    count: usize,
) {
    let payload = MemoryEventPayload {
        conversation_id: conversation_id.to_string(),
        kind,
        count,
    };
    if let Err(e) = app.emit("memory:event", payload) {
        tracing::warn!(error = %e, "failed to emit memory:event");
    }
}

pub(crate) fn emit_error(
    app: &AppHandle,
    conversation_id: &str,
    error: String,
    source: &'static str,
) {
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
/// `on_reroute(from_name, to_name, reason, to_is_bundled_runner)` fires exactly
/// once per successful switch (C5: the bool distinguishes the bundled sidecar
/// from a user-added local endpoint, for the UI toast). This is the ONLY place
/// `reason` — a privacy signal — is allowed to
/// travel; it must never end up in the returned `ChatMessage` (which gets
/// persisted and replayed into a future turn that may be on cloud). The
/// returned message carries only the reason-free `reroute_banner`.
///
/// Local-model-down fails loud, never falls back to cloud: this function never
/// calls `stream_chat`, so an unreachable local endpoint surfaces on the NEXT
/// round's `client.stream_chat(...)?` as a propagated error — there is no
/// catch-and-retry-on-cloud path here, and none must be added.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
/// M8 S4: bring the bundled sidecar up (when wired) and retry
/// `enforce_local_routing` exactly once over the refreshed snapshot. The
/// structural guarantee is preserved — the retry goes through the SAME
/// `enforce_local_routing`, never a hand-rolled predicate. `None` on any
/// failure (no runner wired, no ready model, spawn failed) — the caller's
/// hard-deny stands.
#[cfg_attr(not(feature = "local-runner"), allow(unused_variables))]
async fn lazy_start_then_retry(
    model_manager: &ModelManager,
    routing: &RoutingRequirement,
    local_runner: LocalRunnerRef<'_>,
) -> Option<(Provider, ModelClient)> {
    #[cfg(feature = "local-runner")]
    if let Some((ctx, storage)) = local_runner {
        match crate::models::runner::ensure_running(
            &ctx.supervisor,
            model_manager,
            storage,
            &ctx.paths,
            None,
            None,
            None,
        )
        .await
        {
            Ok(_) => {
                let candidates = model_manager.list_providers();
                if let Ok(local) = enforce_local_routing(routing, &candidates) {
                    return model_manager
                        .get_client(&local.id)
                        .map(|c| (local.clone(), c));
                }
            }
            Err(e) => {
                tracing::info!(
                    target: "lhp::runner",
                    error = %e,
                    "lazy sidecar start for tool reroute unavailable"
                );
            }
        }
    }
    None
}

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
    on_reroute: &(dyn Fn(&str, &str, &str, bool) + Send + Sync),
    // M8 S4: when the reroute finds NO local provider in the snapshot, this
    // (if wired) lazily starts the bundled sidecar and the lookup retries
    // once. `None` (tests, feature-off) = the pre-S4 hard-deny behavior.
    local_runner: LocalRunnerRef<'_>,
) -> Result<(
    Option<ChatMessage>,
    Provider,
    ModelClient,
    bool,
    &'static str,
)> {
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
                    Err(_) => {
                        // M8 S4: empty snapshot — lazily start the bundled
                        // sidecar, then retry the SAME structural check once
                        // (never hand-rolled). Failure stays the hard-deny.
                        lazy_start_then_retry(model_manager, &routing, local_runner).await
                    }
                };
                match found {
                    Some((local, local_client)) => {
                        on_reroute(
                            &provider.name,
                            &local.name,
                            &reason,
                            local.is_bundled_runner(),
                        );
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

// ── P14 concurrent-conversation test ─────────────────────────────────────

#[cfg(test)]
mod concurrent_tests {
    use super::*;
    use crate::agent::gate::Binding;
    use crate::agent::result_sink::ResultSink;
    use crate::models::sse::SseStream;
    use crate::models::{ModelManager, Provider, ProviderKind};
    use crate::storage::Storage;
    use std::sync::Arc;

    /// A `ModelStreamer` that returns a PENDING stream when the request
    /// messages contain the marker "slow-conversation", and a fast canned
    /// SSE response otherwise. This lets us test per-conversation locking:
    /// a stalled turn in one conversation must not block another.
    struct SelectorStreamer(Provider);

    impl ModelStreamer for SelectorStreamer {
        fn provider(&self) -> &Provider {
            &self.0
        }
        fn stream<'a>(
            &'a self,
            _model: &'a str,
            messages: Vec<ChatMessage>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<SseStream>> + Send + 'a>>
        {
            let is_slow = messages
                .iter()
                .any(|m| m.content.contains("slow-conversation"));
            Box::pin(async move {
                if is_slow {
                    // A stream that never produces events — the SSE drain loop
                    // hangs forever on `sse.next_event().await`. This simulates
                    // a stalled provider or long network timeout.
                    Ok(SseStream::from_byte_stream(tokio_stream::pending::<
                        Result<Vec<u8>, reqwest::Error>,
                    >()))
                } else {
                    // A fast canned response that completes immediately.
                    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"fast reply\"}}]}\n\ndata: [DONE]\n";
                    Ok(SseStream::from_byte_stream(tokio_stream::iter(vec![Ok::<
                        _,
                        reqwest::Error,
                    >(
                        body.as_bytes().to_vec(),
                    )])))
                }
            })
        }
    }

    /// P14: demonstrate that a stalled turn in one conversation does not
    /// block another conversation from progressing. Uses a single
    /// `AgentLoop` with a `SelectorStreamer` that returns a pending stream
    /// for "slow-conversation" and a fast stream for any other conversation.
    #[tokio::test]
    async fn two_conversations_progress_independently() {
        let dir = std::env::temp_dir().join(format!("lhp-ptest-{}", Uuid::new_v4()));
        let storage = Arc::new(Storage::open(&dir).expect("open temp storage"));

        // Seed the profile and two conversations.
        let _ = storage.open_profile("personal").expect("profile");
        for conv_id in &["slow-conv", "fast-conv"] {
            storage
                .open_profile("personal")
                .unwrap()
                .create_conversation(&crate::storage::Conversation {
                    id: (*conv_id).to_string(),
                    name: "t".to_string(),
                    pinned: false,
                    binding: "public".to_string(),
                    folder_id: None,
                    color: None,
                    created_at: 1,
                    updated_at: 1,
                })
                .unwrap();
        }

        let mm = Arc::new(ModelManager::new());
        let cloud = Provider::new(
            "cloudco",
            "CloudCo",
            "https://api.openai.com/v1",
            Some("sk-test".into()),
            ProviderKind::Cloud,
        );
        mm.add_provider(cloud.clone());

        let gate = PrivacyGate::new(Arc::new(crate::classifier::RulesClassifier::new()));
        let agent = Arc::new(
            AgentLoop::new(
                gate,
                mm,
                Arc::clone(&storage),
                Arc::new(ToolDispatcher::empty()),
            )
            .with_model_streamer_override(
                Arc::new(SelectorStreamer(cloud.clone())) as Arc<dyn ModelStreamer>
            ),
        );

        let sink: Arc<dyn ResultSink> = Arc::new(crate::agent::result_sink::HeadlessSink);

        // Spawn conversation A — it uses a pending streamer and hangs
        // indefinitely in the SSE drain loop.
        let agent_a = Arc::clone(&agent);
        let sink_a = Arc::clone(&sink);
        let handle_a = tokio::spawn(async move {
            agent_a
                .process_message(
                    "slow-conversation marker, hangs forever".into(),
                    "slow-conv".into(),
                    Binding::Public,
                    "cloudco".into(),
                    "m".into(),
                    "personal".into(),
                    crate::hooks::SessionMode::Normal,
                    &sink_a,
                )
                .await
        });

        // Conversation B must complete within the timeout — it uses a fast
        // streamer. With the OLD global stream_lock, conv B would block
        // waiting for conv A's lock to release. With per-conversation
        // locking, conv B acquires its OWN lock and proceeds immediately.
        let fast_result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            agent.process_message(
                "hello from the fast conversation".into(),
                "fast-conv".into(),
                Binding::Public,
                "cloudco".into(),
                "m".into(),
                "personal".into(),
                crate::hooks::SessionMode::Normal,
                &sink,
            ),
        )
        .await
        .expect("timeout: fast conv B was blocked by slow conv A")
        .expect("process_message should succeed for conv B");

        assert!(
            fast_result.contains("fast reply"),
            "conv B should get the fast reply, got: {fast_result}"
        );

        // Clean up: abort conv A's handle (best-effort).
        handle_a.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
