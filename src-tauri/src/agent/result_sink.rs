//! `ResultSink` — the streaming/notification surface `AgentLoop::process_message`
//! (and its helpers) write through, decoupled from a live Tauri `AppHandle`
//! (Wave 4.3c step 2).
//!
//! Before this, the agent loop held a bare `tauri::AppHandle` and called
//! `.emit(...)` inline at four sites (`stream:token`, `stream:local_reroute`,
//! `memory:event`, `stream:error`). That made `process_message` impossible to
//! drive without a live Tauri app — a blocker for the Wave 4.3 headless
//! sub-agent, which needs to run the same loop with no window/event-loop at
//! all. This trait is the seam: production wires [`TauriResultSink`], which
//! reconstructs the exact payload each site built before and emits it over the
//! same event name; a future headless caller implements `ResultSink` some
//! other way (e.g. capturing into a channel) with no Tauri dependency.
//!
//! `Send + Sync` because the `memory:event` site fires twice — once inline in
//! `stream_to_provider`, and once from inside a detached
//! `tauri::async_runtime::spawn`ed task (the pre-compaction flush,
//! `AgentLoop::on_pre_compaction`). A spawned task needs a `'static` future, so
//! callers hold the sink as `Arc<dyn ResultSink>` and clone the `Arc` (not the
//! trait object) into the task.

use tauri::AppHandle;

use crate::agent::loop_mod::{emit_error, emit_memory_event, LocalReroutePayload, StreamTokenPayload};

/// One method per `.emit(...)` call site the agent loop had. Each method's
/// parameters carry exactly the fields the corresponding event payload struct
/// has (see `loop_mod`'s `StreamTokenPayload` / `LocalReroutePayload` /
/// `MemoryEventPayload` / `StreamErrorPayload`) — a `ResultSink` impl has
/// everything it needs to reconstruct the identical payload.
pub trait ResultSink: Send + Sync {
    /// `stream:token` — one per streamed delta.
    fn token(&self, conversation_id: &str, message_id: &str, token: &str);

    /// `stream:local_reroute` — fired once when a tool call forces the rest
    /// of a turn onto a local endpoint (Q6). `reason` is the detailed privacy
    /// signal; ephemeral UI-only, must never be persisted or replayed into a
    /// model (see `resolve_turn_outcome`).
    fn local_reroute(
        &self,
        conversation_id: &str,
        reason: &str,
        from_provider: &str,
        to_provider: &str,
    );

    /// `memory:event` — the non-silent memory signal (PLAN §9). `kind` is
    /// "recalled" or "remembered"; `count` is content-free (never the
    /// recalled/remembered text itself).
    fn memory_event(&self, conversation_id: &str, kind: &'static str, count: usize);

    /// `stream:error` — emitted when the gate blocks a message, a routing
    /// decision fails, or the model stream itself errors. `source` is
    /// "gate" | "routing" | "model".
    fn error(&self, conversation_id: &str, error: &str, source: &'static str);
}

/// Production [`ResultSink`]: wraps a live Tauri `AppHandle` and reconstructs
/// the identical payload each site built before this refactor, emitting it
/// over the same event name with the same best-effort (log-and-continue)
/// error handling.
pub struct TauriResultSink {
    app: AppHandle,
}

impl TauriResultSink {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

impl ResultSink for TauriResultSink {
    fn token(&self, conversation_id: &str, message_id: &str, token: &str) {
        use tauri::Emitter;
        let payload = StreamTokenPayload {
            token: token.to_string(),
            conversation_id: conversation_id.to_string(),
            message_id: message_id.to_string(),
        };
        if let Err(e) = self.app.emit("stream:token", payload) {
            tracing::warn!(error = %e, "failed to emit stream:token");
        }
    }

    fn local_reroute(
        &self,
        conversation_id: &str,
        reason: &str,
        from_provider: &str,
        to_provider: &str,
    ) {
        use tauri::Emitter;
        let payload = LocalReroutePayload {
            conversation_id: conversation_id.to_string(),
            reason: reason.to_string(),
            from_provider: from_provider.to_string(),
            to_provider: to_provider.to_string(),
        };
        if let Err(e) = self.app.emit("stream:local_reroute", payload) {
            tracing::warn!(error = %e, "failed to emit stream:local_reroute");
        }
    }

    fn memory_event(&self, conversation_id: &str, kind: &'static str, count: usize) {
        // Reuses the exact same free fn `tools::memory::RememberMemoryTool`
        // calls directly (it isn't part of the sub-agent decoupling target),
        // so both paths stay byte-identical by construction.
        emit_memory_event(&self.app, conversation_id, kind, count);
    }

    fn error(&self, conversation_id: &str, error: &str, source: &'static str) {
        emit_error(&self.app, conversation_id, error.to_string(), source);
    }
}
