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

use serde_json::json;

use crate::storage::Storage;
use crate::tools::{Capability, ExecCtx, Tool, ToolInput, ToolResult};

/// How many matches `recall_memory` returns (PLAN §9: "only the top handful").
const RECALL_LIMIT: usize = 5;

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

#[cfg(test)]
mod tests {
    use super::*;
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
}
