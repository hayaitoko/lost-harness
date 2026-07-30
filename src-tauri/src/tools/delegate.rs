//! `delegate` — dispatch a bounded, background "helper" sub-agent (Wave 4.3c).
//!
//! **Why this tool only enqueues instead of running anything.** `delegate`
//! lives inside the tool registry that `ToolDispatcher` (owned by
//! `AgentLoop`) dispatches through. If `delegate` held an `Arc<AgentLoop>` to
//! actually RUN a helper, the dependency graph would be circular:
//! `AgentLoop → ToolDispatcher → delegate → AgentLoop`. So this tool holds
//! only `Storage` + `Arc<ModelManager>` — enough to validate the request and
//! resolve a seat — and persists a `work_items` row (`WorkKind::AgentDispatch`)
//! instead. A separate background `WorkQueueRunner` (`agent::work_runner`,
//! which DOES hold `Arc<AgentLoop>`) drains that queue and calls
//! `AgentLoop::run_subagent` to actually do the work. This is Lukas's decision
//! #4 (async: `delegate` enqueues and returns "dispatched" immediately; the
//! helper's result arrives later, out of band).
//!
//! **Lukas's binding decisions, as implemented here:**
//! 1. `risk() == RiskClass::Dangerous` — an always-shown Once-only Ask (Q8
//!    matrix, invariant #8): no standing grant, and `accept_edits` (which
//!    blanket-approves `Write`) can never silently dispatch a helper.
//! 2. The helper's result streams into the PARENT conversation — this tool
//!    stamps `item.target_conversation_id = ctx.conversation_id` so the
//!    runner knows where to post it (see `agent::work_runner`).
//! 3. No floor-cap on the helper's toolbelt (`tools_allowlist` rides through
//!    unfiltered) — the full gate chain (already proven by
//!    `ToolDispatcher::restricted`'s tests) is what keeps it safe, not a
//!    tool-name ceiling here.
//! 4. Async dispatch — this `run()` never awaits a model call; it only
//!    validates + enqueues.
//!
//! **The trust gate**: only an APPROVED agent type (`AgentTypeApproval::Approved`,
//! `storage.global().list_approved_agent_types()`) is dispatchable by name —
//! a `Pending`/`Rejected` persona (including one an agent itself just drafted)
//! cannot be delegated to until a human approves it in Settings.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::json;

use crate::models::{resolve_seat, ModelManager};
use crate::queue::{WorkItem, WorkKind};
use crate::storage::Storage;
use crate::tools::{Capability, ExecCtx, RiskClass, Tool, ToolInput, ToolResult};

pub struct DelegateTool {
    storage: Storage,
    model_manager: Arc<ModelManager>,
}

impl DelegateTool {
    pub fn new(storage: Storage, model_manager: Arc<ModelManager>) -> Self {
        Self {
            storage,
            model_manager,
        }
    }
}

impl Tool for DelegateTool {
    fn name(&self) -> &str {
        "delegate"
    }

    fn description(&self) -> &str {
        "Dispatch a background helper sub-agent — a named, approved persona (see \
         Settings → Agent types) that works a bounded task on its own toolbelt and \
         reports back into THIS conversation when it's done. Returns immediately \
         (\"dispatched\"); the helper's result arrives later as a labeled message in \
         this same chat, not as this call's return value. \
         args: {\"agent_type\": \"<persona name>\", \"task\": \"<what it should do>\"}."
    }

    fn requires(&self) -> &[Capability] {
        &[]
    }

    fn risk(&self) -> RiskClass {
        // Dispatching an autonomous helper with its own toolbelt — which may
        // include External/Dangerous tools per Lukas decision #3 (no
        // floor-cap) — is a high-blast-radius act, the same tier as
        // manage_cron's standing automation. Dangerous forces an
        // always-shown, Once-only Ask (Q8 matrix, invariant #8): no standing
        // grant, ever, and `accept_edits` (which blanket-approves `Write`)
        // can never silently dispatch one.
        RiskClass::Dangerous
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "agent_type": {
                    "type": "string",
                    "description": "the approved persona's name, exactly as shown in Settings → Agent types"
                },
                "task": {
                    "type": "string",
                    "description": "what the helper should do"
                }
            },
            "required": ["agent_type", "task"],
            "additionalProperties": false
        })
    }

    fn run<'a>(
        &'a self,
        input: ToolInput,
        ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let agent_type =
                match input.args.get("agent_type").and_then(|v| v.as_str()) {
                    Some(s) if !s.trim().is_empty() => s.trim().to_string(),
                    _ => return ToolResult::Err(
                        "delegate requires a non-empty string \"agent_type\" (the persona name)"
                            .to_string(),
                    ),
                };
            let task = match input.args.get("task").and_then(|v| v.as_str()) {
                Some(s) if !s.trim().is_empty() => s.trim().to_string(),
                _ => {
                    return ToolResult::Err(
                        "delegate requires a non-empty string \"task\"".to_string(),
                    )
                }
            };

            // Only APPROVED agent types are dispatchable — the trust gate. A
            // case-insensitive match on the human-facing `name` (not `id`),
            // since that's what the model is given to choose from.
            let approved = match self.storage.global().list_approved_agent_types() {
                Ok(v) => v,
                Err(e) => return ToolResult::Err(format!("delegate failed: {e}")),
            };
            let Some(persona) = approved
                .into_iter()
                .find(|a| a.name.eq_ignore_ascii_case(&agent_type))
            else {
                return ToolResult::Err(format!(
                    "no approved agent type named \"{agent_type}\" — create/approve it in Settings"
                ));
            };

            // Resolve the persona's seat to a concrete (provider, model),
            // inheriting the CALLER's own model when the seat is unbound
            // (`resolve_seat`'s documented fallback).
            let (provider, model) = resolve_seat(
                &self.storage,
                &self.model_manager,
                &ctx.profile,
                &persona.seat,
                &ctx.caller_provider_id,
                &ctx.caller_model,
            );
            if provider.is_empty() {
                // Only reachable when the seat is unbound AND the caller's own
                // model is unknown (an empty `ExecCtx.caller_provider_id`) —
                // resolve_seat never returns an unusable pair otherwise.
                return ToolResult::Err(format!(
                    "the persona's seat \"{}\" isn't bound to a model — bind it in \
                     Settings→Models→Seats",
                    persona.seat
                ));
            }

            // Opaque payload for the WorkQueueRunner (agent::work_runner) to
            // parse when it later claims this item. The helper INHERITS this
            // turn's binding (Wave 4.3c review fix): a Private parent's helper
            // stays Private (local-only), never silently downgraded to cloud.
            let binding_str = match ctx.binding {
                crate::agent::gate::Binding::Auto => "auto",
                crate::agent::gate::Binding::Public => "public",
                crate::agent::gate::Binding::Private => "private",
            };
            let payload = json!({
                "agent_name": persona.name,
                "system_prompt": persona.system_prompt,
                "tools_allowlist": persona.tools_allowlist,
                "provider": provider,
                "model": model,
                "task": task,
                "profile": ctx.profile,
                "binding": binding_str,
            });

            let now = chrono::Utc::now().timestamp();
            let mut item = WorkItem::queued(WorkKind::AgentDispatch, payload.to_string(), now);
            // Lukas decision #2: the helper's result streams into the PARENT
            // conversation — this is what tells the runner where to post it.
            item.target_conversation_id = Some(ctx.conversation_id.clone());
            item.source_ref = Some(ctx.conversation_id.clone());

            let db = match self.storage.open_profile(&ctx.profile) {
                Ok(d) => d,
                Err(e) => return ToolResult::Err(format!("delegate failed: {e}")),
            };
            if let Err(e) = db.insert_work_item(&item) {
                return ToolResult::Err(format!("delegate failed: {e}"));
            }

            // Lukas decision #4: async — dispatched now, returns immediately.
            // The eventual model-generated helper result is NOT this call's
            // return value; it lands later as its own message (see
            // `agent::work_runner`), so there is nothing here for the parent
            // turn's guard-wrap to treat as untrusted model output.
            ToolResult::Ok(json!({
                "dispatched": true,
                "agent": agent_type,
                "note": "the helper runs in the background; its result will appear in this conversation.",
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ModelManager, Provider, ProviderKind};
    use crate::queue::WorkState;

    fn temp_storage() -> (Storage, std::path::PathBuf) {
        let mut root = std::env::temp_dir();
        root.push(format!("lhp-delegate-{}", uuid::Uuid::new_v4()));
        let storage = Storage::open(&root).unwrap();
        (storage, root)
    }

    fn ctx_for(profile: &str, conversation_id: &str) -> ExecCtx {
        ExecCtx {
            conversation_id: conversation_id.to_string(),
            profile: profile.to_string(),
            ..ExecCtx::default()
        }
    }

    /// Seed the built-in personas + bind the "Reviewer" seat so `delegate`
    /// succeeds, returning a wired tool + storage.
    fn wired() -> (DelegateTool, Storage, std::path::PathBuf) {
        let (storage, root) = temp_storage();
        storage.global().ensure_builtin_agent_types(1).unwrap();
        let mm = ModelManager::new();
        mm.add_provider(Provider::new(
            "lmstudio",
            "LM Studio",
            "http://localhost:1234/v1",
            None,
            ProviderKind::Local,
        ));
        storage
            .open_profile("personal")
            .unwrap()
            .set_seat_binding("Reviewer", "lmstudio", "qwen3-14b")
            .unwrap();
        let tool = DelegateTool::new(storage.clone(), Arc::new(mm));
        (tool, storage, root)
    }

    #[tokio::test]
    async fn helper_inherits_the_parents_binding_never_running_weaker() {
        // Wave 4.3c review fix (HIGH): a Private parent must NOT silently
        // downgrade its helper to Auto — that would let Private-designated
        // content egress via a cloud-seated helper. The enqueued binding must
        // match the dispatching turn's binding.
        for (turn_binding, expect) in [
            (crate::agent::gate::Binding::Private, "private"),
            (crate::agent::gate::Binding::Public, "public"),
            (crate::agent::gate::Binding::Auto, "auto"),
        ] {
            let (tool, storage, root) = wired();
            let ctx = ExecCtx {
                conversation_id: "conv-1".into(),
                profile: "personal".into(),
                binding: turn_binding,
                ..ExecCtx::default()
            };
            let out = tool
                .run(
                    ToolInput::new(json!({"agent_type": "Code reviewer", "task": "review"})),
                    &ctx,
                )
                .await;
            assert!(
                matches!(out, ToolResult::Ok(_)),
                "dispatch should succeed, got {out:?}"
            );
            let claimed = storage
                .open_profile("personal")
                .unwrap()
                .claim_next_due_work(chrono::Utc::now().timestamp())
                .unwrap()
                .expect("enqueued");
            let payload: serde_json::Value = serde_json::from_str(&claimed.input_json).unwrap();
            assert_eq!(
                payload["binding"], expect,
                "helper must inherit the {turn_binding:?} parent binding"
            );
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn delegate_is_dangerous() {
        let (storage, root) = temp_storage();
        let tool = DelegateTool::new(storage, Arc::new(ModelManager::new()));
        assert_eq!(tool.risk(), RiskClass::Dangerous);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn unknown_agent_type_is_an_error() {
        let (storage, root) = temp_storage();
        // No ensure_builtin_agent_types call — nothing is approved yet.
        let tool = DelegateTool::new(storage, Arc::new(ModelManager::new()));
        let ctx = ctx_for("personal", "conv-1");
        let out = tool
            .run(
                ToolInput::new(json!({"agent_type": "Nonexistent Persona", "task": "do it"})),
                &ctx,
            )
            .await;
        assert!(
            matches!(out, ToolResult::Err(_)),
            "unapproved agent type must error, got {out:?}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn missing_args_are_rejected() {
        let (storage, root) = temp_storage();
        let tool = DelegateTool::new(storage, Arc::new(ModelManager::new()));
        let ctx = ctx_for("personal", "conv-1");
        assert!(matches!(
            tool.run(ToolInput::new(json!({"task": "do it"})), &ctx)
                .await,
            ToolResult::Err(_)
        ));
        assert!(matches!(
            tool.run(ToolInput::new(json!({"agent_type": "Code reviewer"})), &ctx)
                .await,
            ToolResult::Err(_)
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn approved_agent_type_enqueues_a_work_item_targeting_the_conversation() {
        let (storage, root) = temp_storage();
        storage.global().ensure_builtin_agent_types(1).unwrap();

        // The builtin "Code reviewer" persona's seat is "Reviewer" — bind it
        // to a registered provider so `resolve_seat` succeeds.
        let mm = ModelManager::new();
        mm.add_provider(Provider::new(
            "lmstudio",
            "LM Studio",
            "http://localhost:1234/v1",
            None,
            ProviderKind::Local,
        ));
        storage
            .open_profile("personal")
            .unwrap()
            .set_seat_binding("Reviewer", "lmstudio", "qwen3-14b")
            .unwrap();

        let tool = DelegateTool::new(storage.clone(), Arc::new(mm));
        let ctx = ctx_for("personal", "conv-42");
        // Case-insensitive match on the persona name.
        let out = tool
            .run(
                ToolInput::new(json!({"agent_type": "code reviewer", "task": "look at foo.rs"})),
                &ctx,
            )
            .await;
        match out {
            ToolResult::Ok(v) => assert_eq!(v["dispatched"], true),
            ToolResult::Err(e) => panic!("delegate failed: {e}"),
        }

        let db = storage.open_profile("personal").unwrap();
        let claimed = db
            .claim_next_due_work(chrono::Utc::now().timestamp())
            .unwrap()
            .expect("a work item should have been enqueued");
        assert_eq!(claimed.kind, WorkKind::AgentDispatch);
        assert_eq!(
            claimed.state,
            WorkState::Running,
            "claiming flips it to running"
        );
        assert_eq!(
            claimed.target_conversation_id.as_deref(),
            Some("conv-42"),
            "the helper's result must be able to find its way back to the delegating conversation"
        );
        let payload: serde_json::Value = serde_json::from_str(&claimed.input_json).unwrap();
        assert_eq!(payload["agent_name"], "Code reviewer");
        assert_eq!(payload["provider"], "lmstudio");
        assert_eq!(payload["model"], "qwen3-14b");
        assert_eq!(payload["task"], "look at foo.rs");

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn unbound_seat_with_no_caller_model_is_an_error() {
        let (storage, root) = temp_storage();
        storage.global().ensure_builtin_agent_types(1).unwrap();
        // No seat binding AND ctx has no caller_provider_id/caller_model
        // (ExecCtx::default() leaves them empty) — resolve_seat's inherit
        // fallback yields an empty pair.
        let tool = DelegateTool::new(storage, Arc::new(ModelManager::new()));
        let ctx = ctx_for("personal", "conv-1");
        let out = tool
            .run(
                ToolInput::new(json!({"agent_type": "Research explorer", "task": "look into it"})),
                &ctx,
            )
            .await;
        assert!(
            matches!(out, ToolResult::Err(_)),
            "unbound seat + no caller model must error"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
