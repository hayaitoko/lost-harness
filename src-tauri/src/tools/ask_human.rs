//! `ask_human` — the single blocking "ask the user" tool (PLAN §8 M3 item 10).
//! The one tool that PAUSES the agent to get a real answer from the person:
//! the model calls it with a question, the app surfaces it, and the tool's
//! result is whatever the user types back (or a "no answer" note if they
//! decline / no one is there).
//!
//! `RiskClass::Safe` ⇒ pre-trusted (no approval prompt — asking a question has
//! no side effect and only *increases* user control; gating a clarifying
//! question would be absurd). It blocks the loop while it waits, the same
//! single-in-flight constraint the approval prompt has (the dispatcher holds
//! the stream lock across the wait).
//!
//! The prompter is abstracted behind [`HumanPrompter`] so the core is
//! unit-testable; the Tauri app plugs in `ipc::ask_human::TauriHumanPrompter`
//! (emit an event + await a `resolve_ask_human` command). A `None` prompter
//! (headless / tests without a UI) means "no interactive user" — the tool
//! returns a not-answered result rather than hanging.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::json;

use crate::tools::{Capability, ExecCtx, Tool, ToolInput, ToolResult};

/// A question to put to the user, with the ids needed to route the answer back.
#[derive(Debug, Clone)]
pub struct AskRequest {
    pub id: String,
    pub conversation_id: String,
    pub question: String,
}

/// Something that can put a question to the human and return their answer.
/// `Some(text)` = the user answered; `None` = declined, timed out, or no
/// interactive surface. The Tauri app implements this; a headless body could
/// plug a queue-backed variant later.
pub trait HumanPrompter: Send + Sync {
    fn ask<'a>(
        &'a self,
        req: AskRequest,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>;
}

/// The blocking ask-the-user tool. Holds an optional prompter — `None` in a
/// context with no interactive surface, where the tool reports "not answered"
/// instead of hanging forever.
pub struct AskHumanTool {
    prompter: Option<Arc<dyn HumanPrompter>>,
}

impl AskHumanTool {
    pub fn new(prompter: Option<Arc<dyn HumanPrompter>>) -> Self {
        Self { prompter }
    }
}

impl Tool for AskHumanTool {
    fn name(&self) -> &str {
        "ask_human"
    }

    fn description(&self) -> &str {
        "Pause and ask the user a question, then continue with their answer. \
         Use when you genuinely need a decision, clarification, or missing \
         detail only they can give. args: {\"question\": \"what to ask\"}."
    }

    fn requires(&self) -> &[Capability] {
        &[]
    }

    // risk() defaults to Safe — asking a question has no side effect and is
    // pre-trusted (no approval prompt to raise a prompt).

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "the question to put to the user" }
            },
            "required": ["question"],
            "additionalProperties": false
        })
    }

    fn run<'a>(
        &'a self,
        input: ToolInput,
        ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let question = match input.args.get("question").and_then(|v| v.as_str()) {
                Some(q) if !q.trim().is_empty() => q.trim().to_string(),
                _ => {
                    return ToolResult::Err(
                        "ask_human requires a non-empty string \"question\" arg".to_string(),
                    )
                }
            };
            let Some(prompter) = &self.prompter else {
                // No interactive surface — report it rather than hang, so the
                // agent can proceed (e.g. make a documented assumption).
                return ToolResult::Ok(json!({
                    "answered": false,
                    "note": "No interactive user is available to answer right now.",
                }));
            };
            let req = AskRequest {
                id: uuid::Uuid::new_v4().to_string(),
                conversation_id: ctx.conversation_id.clone(),
                question: question.clone(),
            };
            match prompter.ask(req).await {
                Some(answer) => ToolResult::Ok(json!({
                    "answered": true,
                    "question": question,
                    "answer": answer,
                })),
                None => ToolResult::Ok(json!({
                    "answered": false,
                    "question": question,
                    "note": "The user did not answer (declined or timed out).",
                })),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakePrompter(Option<String>);
    impl HumanPrompter for FakePrompter {
        fn ask<'a>(
            &'a self,
            _req: AskRequest,
        ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>> {
            let ans = self.0.clone();
            Box::pin(async move { ans })
        }
    }

    #[tokio::test]
    async fn returns_the_users_answer() {
        let tool = AskHumanTool::new(Some(Arc::new(FakePrompter(Some("blue".into())))));
        match tool
            .run(ToolInput::new(json!({ "question": "favorite color?" })), &ExecCtx::default())
            .await
        {
            ToolResult::Ok(v) => {
                assert_eq!(v["answered"], true);
                assert_eq!(v["answer"], "blue");
                assert_eq!(v["question"], "favorite color?");
            }
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        }
    }

    #[tokio::test]
    async fn declined_or_timed_out_is_a_not_answered_ok() {
        let tool = AskHumanTool::new(Some(Arc::new(FakePrompter(None))));
        match tool
            .run(ToolInput::new(json!({ "question": "proceed?" })), &ExecCtx::default())
            .await
        {
            ToolResult::Ok(v) => assert_eq!(v["answered"], false),
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        }
    }

    #[tokio::test]
    async fn no_prompter_reports_no_interactive_user() {
        let tool = AskHumanTool::new(None);
        match tool
            .run(ToolInput::new(json!({ "question": "there?" })), &ExecCtx::default())
            .await
        {
            ToolResult::Ok(v) => {
                assert_eq!(v["answered"], false);
                assert!(v["note"].as_str().unwrap().contains("No interactive user"));
            }
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        }
    }

    #[tokio::test]
    async fn empty_question_is_a_usage_error() {
        let tool = AskHumanTool::new(Some(Arc::new(FakePrompter(Some("x".into())))));
        assert!(matches!(
            tool.run(ToolInput::new(json!({ "question": "  " })), &ExecCtx::default()).await,
            ToolResult::Err(_)
        ));
    }
}
