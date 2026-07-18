//! The Tauri side of `ask_human` (`crate::tools::ask_human`). Mirrors the
//! approval spine (`ipc::approval`): when the tool needs an answer it calls
//! [`TauriHumanPrompter`], which parks a one-shot in the [`AskHumanRegistry`]
//! keyed by request id and emits `tool:ask_human_request` to the frontend,
//! then awaits the answer with a "no answer" timeout. The frontend replies via
//! the `resolve_ask_human` command (`crate::ipc`), which touches ONLY this
//! registry — never the agent loop's stream lock — so answering can't deadlock
//! against the dispatch parked waiting for it.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::oneshot;

use crate::tools::ask_human::{AskRequest, HumanPrompter};

/// Event the frontend listens for to raise the ask-human prompt.
pub const ASK_HUMAN_REQUEST_EVENT: &str = "tool:ask_human_request";

/// Emitted to the frontend for each pending question. Mirrored by the TS
/// bridge (`src/lib/api/tauri.ts`).
#[derive(Debug, Clone, Serialize)]
pub struct AskHumanRequestPayload {
    pub id: String,
    pub conversation_id: String,
    /// The question. Untrusted (model-authored) — the UI renders it as display
    /// text, never as markup/HTML it would execute.
    pub question: String,
}

/// In-flight ask-human prompts, keyed by request id. Shared (via `Arc`)
/// between the prompter and the resolve command.
#[derive(Default)]
pub struct AskHumanRegistry {
    pending: Mutex<HashMap<String, oneshot::Sender<Option<String>>>>,
}

impl AskHumanRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn park(&self, id: String, sender: oneshot::Sender<Option<String>>) {
        self.pending
            .lock()
            .expect("ask-human registry poisoned")
            .insert(id, sender);
    }

    fn take(&self, id: &str) -> Option<oneshot::Sender<Option<String>>> {
        self.pending
            .lock()
            .expect("ask-human registry poisoned")
            .remove(id)
    }

    /// Deliver the user's answer (`Some(text)`) or their decline (`None`) to
    /// the parked request. Returns false if the id is unknown (already
    /// answered, or timed out).
    pub fn answer(&self, id: &str, answer: Option<String>) -> bool {
        match self.take(id) {
            // `send` fails only if the awaiting dispatch already gave up
            // (timed out); treat that as "not delivered".
            Some(tx) => tx.send(answer).is_ok(),
            None => false,
        }
    }
}

/// The app's [`HumanPrompter`]: emit an event to the frontend and await the
/// answer with a decline-by-default timeout.
pub struct TauriHumanPrompter {
    app: AppHandle,
    registry: Arc<AskHumanRegistry>,
    timeout: Duration,
}

impl TauriHumanPrompter {
    pub fn new(app: AppHandle, registry: Arc<AskHumanRegistry>, timeout: Duration) -> Self {
        Self {
            app,
            registry,
            timeout,
        }
    }
}

impl HumanPrompter for TauriHumanPrompter {
    fn ask<'a>(
        &'a self,
        req: AskRequest,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>> {
        let (tx, rx) = oneshot::channel();
        self.registry.park(req.id.clone(), tx);

        // Surface the question. If emit fails (e.g. no window yet), the request
        // simply times out and returns `None` — the agent proceeds without an
        // answer rather than hanging.
        if let Err(e) = self.app.emit(
            ASK_HUMAN_REQUEST_EVENT,
            AskHumanRequestPayload {
                id: req.id.clone(),
                conversation_id: req.conversation_id,
                question: req.question,
            },
        ) {
            tracing::warn!(error = %e, "failed to emit ask_human request");
        }

        let timeout = self.timeout;
        let registry = Arc::clone(&self.registry);
        let id = req.id;
        Box::pin(async move {
            let answer = match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(a)) => a,
                // Timed out, or the sender was dropped without answering.
                _ => None,
            };
            // Clean up the parked entry (no-op if `answer` already took it).
            registry.take(&id);
            answer
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_unknown_id_returns_false() {
        let reg = AskHumanRegistry::new();
        assert!(!reg.answer("nope", Some("x".into())));
    }

    #[tokio::test]
    async fn park_then_answer_delivers_the_text() {
        let reg = Arc::new(AskHumanRegistry::new());
        let (tx, rx) = oneshot::channel();
        reg.park("q-1".into(), tx);
        assert!(reg.answer("q-1", Some("green".into())));
        assert_eq!(rx.await.unwrap(), Some("green".to_string()));
        // A second answer for the same id is a no-op (already taken).
        assert!(!reg.answer("q-1", Some("blue".into())));
    }

    #[tokio::test]
    async fn a_decline_delivers_none() {
        let reg = Arc::new(AskHumanRegistry::new());
        let (tx, rx) = oneshot::channel();
        reg.park("q-2".into(), tx);
        assert!(reg.answer("q-2", None));
        assert_eq!(rx.await.unwrap(), None);
    }
}
