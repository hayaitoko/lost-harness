//! `session_search` — the agent's recall over PAST conversations (PLAN §8 M3
//! item 10). A read-only substring search over *this profile's* transcript,
//! distinct from the memory archive (`recall_memory`, which stores curated
//! facts): this answers "did we talk about X before, and where?".
//!
//! `RiskClass::Safe` (read-only, on-device) ⇒ pre-trusted (no approval prompt).
//! **Profile-scoped**: only the active profile's own conversations are searched —
//! a profile boundary is never crossed (unlike shared memory, past chats are
//! inherently per-profile). The returned snippets are guard-wrapped as untrusted
//! content by the dispatcher's `run_turn`, same as any tool output.

use std::future::Future;
use std::pin::Pin;

use serde_json::json;

use crate::storage::Storage;
use crate::tools::{Capability, ExecCtx, Tool, ToolInput, ToolResult};

/// How many past-message matches to return (PLAN §9's "only the top handful").
const RESULT_LIMIT: usize = 8;
/// Cap each returned snippet so a long message can't blow the tool result up.
const SNIPPET_CAP: usize = 240;

/// The agent's past-conversation search tool. Holds a `Storage` handle (cheap
/// `Arc` clone); it opens the ACTIVE profile's DB per call from `ExecCtx`.
pub struct SessionSearchTool {
    storage: Storage,
}

impl SessionSearchTool {
    pub fn new(storage: Storage) -> Self {
        Self { storage }
    }
}

impl Tool for SessionSearchTool {
    fn name(&self) -> &str {
        "session_search"
    }

    fn description(&self) -> &str {
        "Search your past conversations with this user for messages matching a \
         query. args: {\"query\": \"what to look for\"}. Returns recent matching \
         snippets from this profile's chats only."
    }

    fn requires(&self) -> &[Capability] {
        // Local, read-only — needs nothing special from the body.
        &[]
    }

    // risk() defaults to Safe (read-only, on-device) → pre-trusted.

    fn run<'a>(
        &'a self,
        input: ToolInput,
        ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let query = match input.args.get("query").and_then(|v| v.as_str()) {
                Some(q) if !q.trim().is_empty() => q.trim().to_string(),
                _ => {
                    return ToolResult::Err(
                        "session_search requires a non-empty string \"query\" arg".to_string(),
                    )
                }
            };
            // Profile-scoped: open THIS profile's transcript DB. An empty/invalid
            // profile (a degenerate ExecCtx) fails the call rather than searching
            // the wrong store.
            let db = match self.storage.open_profile(&ctx.profile) {
                Ok(d) => d,
                Err(e) => return ToolResult::Err(format!("session_search failed: {e}")),
            };
            match db.search_messages(&query, RESULT_LIMIT) {
                Ok(hits) => {
                    let matches: Vec<_> = hits
                        .into_iter()
                        .map(|h| {
                            // Truncate over a code-point boundary so a multi-byte
                            // char never splits.
                            let snippet: String = h.content.chars().take(SNIPPET_CAP).collect();
                            json!({
                                "conversation": h
                                    .conversation_name
                                    .unwrap_or_else(|| "(untitled)".to_string()),
                                "conversation_id": h.conversation_id,
                                "role": h.role,
                                "when": h.created_at,
                                "snippet": snippet,
                            })
                        })
                        .collect();
                    ToolResult::Ok(json!({ "query": query, "matches": matches }))
                }
                Err(e) => ToolResult::Err(format!("session_search failed: {e}")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{Conversation, Message};

    fn temp_storage() -> (Storage, std::path::PathBuf) {
        let mut root = std::env::temp_dir();
        root.push(format!("lhp-session-search-{}", uuid::Uuid::new_v4()));
        let storage = Storage::open(&root).unwrap();
        (storage, root)
    }

    fn seed(
        storage: &Storage,
        profile: &str,
        conv_id: &str,
        conv_name: &str,
        turns: &[(&str, &str)],
    ) {
        let db = storage.open_profile(profile).unwrap();
        db.create_conversation(&Conversation {
            id: conv_id.into(),
            name: conv_name.into(),
            pinned: false,
            binding: "auto".into(),
            folder_id: None,
            color: None,
            created_at: 1,
            updated_at: 1,
        })
        .unwrap();
        for (i, (role, content)) in turns.iter().enumerate() {
            db.add_message(&Message {
                id: format!("{conv_id}-{i}"),
                conversation_id: conv_id.into(),
                role: (*role).into(),
                content: (*content).into(),
                model: None,
                provider_id: None,
                routing_decision: None,
                thinking_content: None,
                error: None,
                aborted: false,
                created_at: 100 + i as i64,
            })
            .unwrap();
        }
    }

    #[tokio::test]
    async fn finds_a_past_message_scoped_to_the_profile() {
        let (storage, root) = temp_storage();
        seed(
            &storage,
            "personal",
            "c1",
            "Furnace repair",
            &[
                ("user", "when did we fix the furnace?"),
                ("assistant", "The heater was repaired in March."),
            ],
        );
        // A DIFFERENT profile's chat that also mentions the furnace.
        seed(
            &storage,
            "work",
            "c2",
            "Office HVAC",
            &[("user", "the office furnace is on the maintenance plan")],
        );

        let tool = SessionSearchTool::new(storage.clone());
        let ctx = ExecCtx {
            profile: "personal".into(),
            ..ExecCtx::default()
        };

        match tool
            .run(ToolInput::new(json!({ "query": "furnace" })), &ctx)
            .await
        {
            ToolResult::Ok(v) => {
                let matches = v["matches"].as_array().unwrap();
                // Only the personal profile's one matching message — the work
                // profile's furnace chat is never searched.
                assert_eq!(matches.len(), 1, "profile-scoped: only personal's match");
                assert_eq!(matches[0]["conversation"], "Furnace repair");
                assert!(matches[0]["snippet"].as_str().unwrap().contains("furnace"));
            }
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        }

        // A term only in the OTHER profile returns nothing here.
        match tool
            .run(ToolInput::new(json!({ "query": "maintenance plan" })), &ctx)
            .await
        {
            ToolResult::Ok(v) => assert!(
                v["matches"].as_array().unwrap().is_empty(),
                "a term from another profile's chat must not surface"
            ),
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        }

        // Empty query is a usage error.
        assert!(matches!(
            tool.run(ToolInput::new(json!({ "query": "   " })), &ctx)
                .await,
            ToolResult::Err(_)
        ));

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn like_wildcards_in_the_query_match_literally() {
        let (storage, root) = temp_storage();
        seed(
            &storage,
            "personal",
            "c1",
            "Discounts",
            &[
                ("user", "the coupon gives 50% off"),
                ("assistant", "Noted — 50 percent is a good deal."),
            ],
        );
        let tool = SessionSearchTool::new(storage.clone());
        let ctx = ExecCtx {
            profile: "personal".into(),
            ..ExecCtx::default()
        };

        // "50%" must match the literal "50%" turn, NOT act as a LIKE wildcard
        // (which would match everything). Exactly one turn contains "50%".
        match tool
            .run(ToolInput::new(json!({ "query": "50%" })), &ctx)
            .await
        {
            ToolResult::Ok(v) => {
                let matches = v["matches"].as_array().unwrap();
                assert_eq!(
                    matches.len(),
                    1,
                    "the % must be escaped, matching literally"
                );
                assert!(matches[0]["snippet"].as_str().unwrap().contains("50%"));
            }
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
