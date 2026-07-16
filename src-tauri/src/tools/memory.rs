//! Memory tools — the agent's read access to its saved memory (PLAN §9).
//!
//! `recall_memory` is the always-available "pinned search tool": keyword-search
//! the saved archive for facts relevant to a query. It searches the SHARED
//! store only (`allow_private = false`), so a private-local fact can never be
//! surfaced back into any model's context — cloud or local. Endpoint-aware
//! private access (a local turn may safely read private facts) waits for
//! `ExecCtx` to carry the turn's endpoint kind; until then the cloud-proof
//! default holds regardless of endpoint. Read-only ⇒ `RiskClass::Safe` ⇒
//! pre-trusted (no approval prompt), matching the design's "always-available,
//! pinned search tool the agent can reach for."

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::json;

use crate::classifier::rules::RuleCategory;
use crate::classifier::{Classification, Classifier, Label};
use crate::storage::{MemoryBucket, MemoryFact, Storage};
use crate::tools::{Capability, ExecCtx, RiskClass, Tool, ToolInput, ToolResult};

/// How many matches `recall_memory` returns (PLAN §9: "only the top handful").
const RECALL_LIMIT: usize = 5;

/// Where a memory write should go — PLAN §9's 3-bucket sensitivity model.
/// Canonical here; the `save_memory` IPC uses it too, so a manual add and an
/// agent `remember` route identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryRoute {
    Shared,
    PrivateLocal,
    /// A credential / one-time secret — dropped, never written anywhere.
    NeverPersist,
}

/// Route a fact by the classifier's read of it: a credential span → never
/// persist; otherwise Private/Uncertain → private-local, Public → shared.
pub fn route_memory_sensitivity(c: &Classification) -> MemoryRoute {
    if c
        .spans
        .iter()
        .any(|s| s.category == RuleCategory::Credential)
    {
        return MemoryRoute::NeverPersist;
    }
    match c.label {
        Label::Public => MemoryRoute::Shared,
        Label::Private | Label::Uncertain => MemoryRoute::PrivateLocal,
    }
}

/// The agent's memory-search tool. Holds a `Storage` handle (cheap Arc clone).
pub struct RecallMemoryTool {
    storage: Storage,
}

impl RecallMemoryTool {
    pub fn new(storage: Storage) -> Self {
        Self { storage }
    }
}

impl Tool for RecallMemoryTool {
    fn name(&self) -> &str {
        "recall_memory"
    }

    fn description(&self) -> &str {
        "Search your saved memory for facts relevant to a query. \
         args: {\"query\": \"what to look up\"}. Returns the top matches; \
         private on-device facts are never included."
    }

    fn requires(&self) -> &[Capability] {
        // Local, read-only — needs nothing special from the body.
        &[]
    }

    // risk() defaults to Safe (read-only, on-device) → pre-trusted.

    fn run<'a>(
        &'a self,
        input: ToolInput,
        _ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let query = match input.args.get("query").and_then(|v| v.as_str()) {
                Some(q) if !q.trim().is_empty() => q.to_string(),
                _ => {
                    return ToolResult::Err(
                        "recall_memory requires a non-empty string \"query\" arg".to_string(),
                    )
                }
            };
            // SHARED store only — never surface a private-local fact into model
            // context (the cloud-proof default; see the module docs).
            match self
                .storage
                .global()
                .search_memory(&query, false, RECALL_LIMIT)
            {
                Ok(hits) => {
                    let matches: Vec<_> = hits
                        .into_iter()
                        .map(|h| {
                            json!({
                                "content": h.fact.content,
                                "saved_at": h.fact.created_at,
                            })
                        })
                        .collect();
                    ToolResult::Ok(json!({ "query": query, "matches": matches }))
                }
                Err(e) => ToolResult::Err(format!("recall_memory failed: {e}")),
            }
        })
    }
}

/// `remember` — save a durable fact to memory (PLAN §9's save-as-you-go).
/// Write-risk, so it routes through the approval spine (non-silent, gated). The
/// fact is classified and routed by sensitivity: a credential is dropped, a
/// private detail goes to the local-only store, a benign fact to the shared
/// store — identical routing to the manual `save_memory` IPC.
pub struct RememberMemoryTool {
    storage: Storage,
    classifier: Arc<dyn Classifier>,
}

impl RememberMemoryTool {
    pub fn new(storage: Storage, classifier: Arc<dyn Classifier>) -> Self {
        Self {
            storage,
            classifier,
        }
    }
}

impl Tool for RememberMemoryTool {
    fn name(&self) -> &str {
        "remember"
    }

    fn description(&self) -> &str {
        "Save a durable fact about the user to memory. \
         args: {\"content\": \"the fact\"}. Routed by sensitivity — secrets are \
         never saved, private details stay on this device."
    }

    fn requires(&self) -> &[Capability] {
        &[]
    }

    fn risk(&self) -> RiskClass {
        // Mutates the user's memory → routes through the approval spine.
        RiskClass::Write
    }

    fn run<'a>(
        &'a self,
        input: ToolInput,
        ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let content = match input.args.get("content").and_then(|v| v.as_str()) {
                Some(c) if !c.trim().is_empty() => c.trim().to_string(),
                _ => {
                    return ToolResult::Err(
                        "remember requires a non-empty string \"content\" arg".to_string(),
                    )
                }
            };
            let classification = self.classifier.classify(&content);
            let bucket = match route_memory_sensitivity(&classification) {
                MemoryRoute::NeverPersist => {
                    return ToolResult::Ok(json!({
                        "saved": false,
                        "sensitivity": "never_persist",
                        "note": "That looked like a secret, so it was not saved anywhere — even locally.",
                    }));
                }
                MemoryRoute::Shared => MemoryBucket::Shared,
                MemoryRoute::PrivateLocal => MemoryBucket::PrivateLocal,
            };
            let fact = MemoryFact {
                id: uuid::Uuid::new_v4().to_string(),
                content: content.clone(),
                origin_profile: ctx.profile.clone(),
                tags: None,
                created_at: chrono::Utc::now().timestamp(),
                pinned: false,
            };
            match self.storage.global().insert_memory_fact_in(bucket, &fact) {
                Ok(()) => ToolResult::Ok(json!({
                    "saved": true,
                    "sensitivity": if bucket == MemoryBucket::PrivateLocal {
                        "private_local"
                    } else {
                        "shared"
                    },
                    "id": fact.id,
                })),
                Err(e) => ToolResult::Err(format!("remember failed: {e}")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classifier::RulesClassifier;
    use crate::storage::{MemoryBucket, MemoryFact};

    fn temp_storage() -> (Storage, std::path::PathBuf) {
        let mut root = std::env::temp_dir();
        root.push(format!("lhp-mem-tool-{}", uuid::Uuid::new_v4()));
        let storage = Storage::open(&root).unwrap();
        (storage, root)
    }

    fn fact(id: &str, content: &str) -> MemoryFact {
        MemoryFact {
            id: id.into(),
            content: content.into(),
            origin_profile: "personal".into(),
            tags: None,
            created_at: 1,
            pinned: false,
        }
    }

    #[tokio::test]
    async fn recall_returns_shared_hits_but_never_private() {
        let (storage, root) = temp_storage();
        storage
            .global()
            .insert_memory_fact_in(
                MemoryBucket::Shared,
                &fact("s", "the deploy key lives in the vault"),
            )
            .unwrap();
        storage
            .global()
            .insert_memory_fact_in(
                MemoryBucket::PrivateLocal,
                &fact("p", "home address is 123 Oak Street"),
            )
            .unwrap();

        let tool = RecallMemoryTool::new(storage);
        let ctx = ExecCtx::default();

        // A shared hit is found.
        match tool
            .run(ToolInput::new(json!({ "query": "deploy key vault" })), &ctx)
            .await
        {
            ToolResult::Ok(v) => {
                let matches = v["matches"].as_array().unwrap();
                assert_eq!(matches.len(), 1);
                assert!(matches[0]["content"]
                    .as_str()
                    .unwrap()
                    .contains("deploy key"));
            }
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        }

        // A private-only term returns NOTHING — recall never touches the private
        // store, so a private-local fact can't leak back into model context.
        match tool
            .run(
                ToolInput::new(json!({ "query": "Oak Street home address" })),
                &ctx,
            )
            .await
        {
            ToolResult::Ok(v) => {
                assert!(
                    v["matches"].as_array().unwrap().is_empty(),
                    "recall must never surface a private-local fact"
                );
            }
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        }

        // Empty query is a usage error.
        assert!(matches!(
            tool.run(ToolInput::new(json!({ "query": "  " })), &ctx).await,
            ToolResult::Err(_)
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn remember_routes_by_sensitivity() {
        let (storage, root) = temp_storage();
        let tool = RememberMemoryTool::new(storage.clone(), Arc::new(RulesClassifier::new()));
        let ctx = ExecCtx {
            conversation_id: "c".into(),
            profile: "personal".into(),
            reads: None,
        };

        // Benign → shared, saved.
        match tool
            .run(ToolInput::new(json!({ "content": "the standup is at 10am" })), &ctx)
            .await
        {
            ToolResult::Ok(v) => {
                assert_eq!(v["saved"], true);
                assert_eq!(v["sensitivity"], "shared");
            }
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        }

        // A credential → dropped (never persisted).
        match tool
            .run(
                ToolInput::new(
                    json!({ "content": "my api key is sk-ABCD1234efgh5678ijkl9012mnop3456" }),
                ),
                &ctx,
            )
            .await
        {
            ToolResult::Ok(v) => {
                assert_eq!(v["saved"], false);
                assert_eq!(v["sensitivity"], "never_persist");
            }
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        }

        // A private detail (SSN) → the local-only store.
        match tool
            .run(ToolInput::new(json!({ "content": "my SSN is 123-45-6789" })), &ctx)
            .await
        {
            ToolResult::Ok(v) => assert_eq!(v["sensitivity"], "private_local"),
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        }

        // The credential is nowhere (even a private-inclusive search finds nothing).
        assert!(
            storage.global().search_memory("api key", true, 10).unwrap().is_empty(),
            "a credential must never be persisted"
        );
        // The SSN is in the private store only — a cloud/shared search can't see it.
        assert!(storage
            .global()
            .search_memory("123 45 6789", false, 10)
            .unwrap()
            .is_empty());
        assert_eq!(
            storage.global().search_memory("123 45 6789", true, 10).unwrap().len(),
            1
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
