//! Interactive tool-approval — the Tauri side of the approval spine
//! (`crate::hooks::approval`). When `ToolDispatcher` hits an `Ask`, it calls
//! the [`TauriApprovalPrompter`], which:
//!   1. parks a one-shot channel in the [`ApprovalRegistry`], keyed by a
//!      request id, and
//!   2. emits `tool:approval_request` to the frontend, then awaits the
//!      one-shot with a deny-by-default timeout.
//!
//! The frontend answers via the `resolve_tool_approval` command
//! (`crate::ipc`), which looks the request up by id and sends the decision
//! back over the one-shot. That command touches ONLY this registry — never
//! the agent loop's stream lock — so answering can never deadlock against the
//! dispatch that is parked waiting for it.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::oneshot;

use crate::hooks::{ApprovalDecision, ApprovalPrompter, ApprovalRequest};

/// Event the frontend listens for to raise an approval dialog.
pub const APPROVAL_REQUEST_EVENT: &str = "tool:approval_request";

/// Emitted to the frontend for each pending approval. Mirrored by the TS
/// bridge (`src/lib/api/tauri.ts`).
#[derive(Debug, Clone, Serialize)]
pub struct ToolApprovalRequestPayload {
    pub id: String,
    pub conversation_id: String,
    pub tool_name: String,
    /// The canonical `name {args}` the user is being asked to approve.
    /// Untrusted — the UI must render it as display text, never execute it.
    pub command: String,
    pub prompt: String,
    /// Which hook raised it ("permission" | "first_use_confirm").
    pub by: String,
    /// The action fingerprint — informational for the UI; the grant target is
    /// resolved server-side from the parked request, not from this value.
    pub fingerprint: String,
    /// The tool's risk class, lowercase ("safe"|"write"|"external"|"dangerous").
    /// Server-derived from `Tool::risk()`. The dialog badges it and offers only
    /// the matrix-legal grant buttons; the server (`resolve_grant`) enforces, so
    /// this is legibility, not the gate.
    pub risk: String,
    /// For `External` tools, where the call goes — the consent to surface.
    /// `None` for non-egress tools (all current tools).
    pub destination: Option<String>,
}

/// A parked approval awaiting the user's answer.
struct Pending {
    sender: oneshot::Sender<ApprovalDecision>,
    fingerprint: String,
    tool_name: String,
}

/// In-flight approval prompts, keyed by request id. Shared (via `Arc`)
/// between the prompter and the resolve command.
#[derive(Default)]
pub struct ApprovalRegistry {
    pending: Mutex<HashMap<String, Pending>>,
}

impl ApprovalRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn park(
        &self,
        id: String,
        sender: oneshot::Sender<ApprovalDecision>,
        fingerprint: String,
        tool_name: String,
    ) {
        self.pending
            .lock()
            .expect("approval registry poisoned")
            .insert(
                id,
                Pending {
                    sender,
                    fingerprint,
                    tool_name,
                },
            );
    }

    fn take(&self, id: &str) -> Option<Pending> {
        self.pending
            .lock()
            .expect("approval registry poisoned")
            .remove(id)
    }

    /// Answer a parked request. `mk` builds the decision from the request's
    /// stored `(fingerprint, tool_name)` — so the frontend only has to say
    /// approve-vs-deny and action-vs-tool, never echo the fingerprint back.
    /// Returns false if the id is unknown (already answered, or timed out).
    pub fn answer(
        &self,
        id: &str,
        mk: impl FnOnce(&str, &str) -> ApprovalDecision,
    ) -> bool {
        match self.take(id) {
            Some(p) => {
                let decision = mk(&p.fingerprint, &p.tool_name);
                // `send` fails only if the awaiting dispatch already gave up
                // (timed out); treat that as "not delivered".
                p.sender.send(decision).is_ok()
            }
            None => false,
        }
    }
}

/// The app's [`ApprovalPrompter`]: emit an event to the frontend and await
/// the answer with a deny-by-default timeout.
pub struct TauriApprovalPrompter {
    app: AppHandle,
    registry: Arc<ApprovalRegistry>,
    timeout: Duration,
}

impl TauriApprovalPrompter {
    pub fn new(app: AppHandle, registry: Arc<ApprovalRegistry>, timeout: Duration) -> Self {
        Self {
            app,
            registry,
            timeout,
        }
    }
}

impl ApprovalPrompter for TauriApprovalPrompter {
    fn request<'a>(
        &'a self,
        req: ApprovalRequest,
    ) -> Pin<Box<dyn Future<Output = ApprovalDecision> + Send + 'a>> {
        let (tx, rx) = oneshot::channel();
        self.registry
            .park(req.id.clone(), tx, req.fingerprint.clone(), req.tool_name.clone());

        // Surface the prompt. If emit fails (e.g. no window yet), the request
        // will simply time out and deny by default — fail closed.
        if let Err(e) = self.app.emit(
            APPROVAL_REQUEST_EVENT,
            ToolApprovalRequestPayload {
                id: req.id.clone(),
                conversation_id: req.conversation_id,
                tool_name: req.tool_name,
                command: req.command,
                prompt: req.prompt,
                by: req.by,
                fingerprint: req.fingerprint,
                risk: req.risk.as_str().to_string(),
                destination: req.destination,
            },
        ) {
            tracing::warn!(error = %e, "failed to emit tool approval request");
        }

        let timeout = self.timeout;
        let registry = Arc::clone(&self.registry);
        let id = req.id;
        Box::pin(async move {
            let decision = match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(d)) => d,
                // Timed out, or the sender was dropped without answering.
                _ => ApprovalDecision::Timeout,
            };
            // Clean up the parked entry (no-op if `answer` already took it).
            //
            // Known, accepted race (fail-closed): in a nanosecond window
            // between `timeout` electing Timeout and this `take`, a concurrent
            // `answer` can still find the entry and `send` a decision this side
            // will never read — so `resolve_tool_approval` may briefly report
            // `true` for a decision the dispatch already denied. The security
            // outcome stays correct (the call is denied); only the frontend's
            // "delivered" signal can be momentarily wrong. With a 300s human
            // timeout this needs a sub-nanosecond coincidence, so it is
            // documented rather than closed with a heavier claim/ack protocol.
            registry.take(&id);
            decision
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::{GrantScope, GrantTarget};

    #[test]
    fn answer_unknown_id_returns_false() {
        let reg = ApprovalRegistry::new();
        let answered = reg.answer("nope", |_, _| ApprovalDecision::Deny);
        assert!(!answered);
    }

    #[tokio::test]
    async fn park_then_answer_delivers_the_decision() {
        let reg = Arc::new(ApprovalRegistry::new());
        let (tx, rx) = oneshot::channel();
        reg.park("req-1".into(), tx, "fp-abc".into(), "write_file".into());

        // Answer as "approve this action, once" — target built from the
        // stored fingerprint.
        let ok = reg.answer("req-1", |fp, _tool| {
            ApprovalDecision::Approve(GrantScope::Once, GrantTarget::Fingerprint(fp.to_string()))
        });
        assert!(ok);
        match rx.await {
            Ok(ApprovalDecision::Approve(GrantScope::Once, GrantTarget::Fingerprint(fp))) => {
                assert_eq!(fp, "fp-abc")
            }
            other => panic!("expected the delivered decision, got {other:?}"),
        }
        // Second answer for the same id is a no-op (already taken).
        assert!(!reg.answer("req-1", |_, _| ApprovalDecision::Deny));
    }
}
