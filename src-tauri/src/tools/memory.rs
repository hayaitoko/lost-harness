//! Memory tools — the agent's read access to its saved memory (PLAN §9).
//!
//! `recall_memory` is the always-available "pinned search tool": keyword-search
//! the saved archive for facts relevant to a query. It's **endpoint-aware AND
//! profile-aware** (PLAN §9/§7): a cloud turn searches the SHARED store only, so
//! a private-local fact can never leak into a cloud model's context; a
//! local/private turn (`ExecCtx::allow_private_memory`, set by the dispatcher to
//! `!is_cloud`) may also read the ACTIVE profile's private-local facts — never
//! another profile's (shared facts stay cross-profile — one coherent memory of
//! the user, §7 — but the private-local bucket never crosses the profile
//! boundary). The safe default is shared-only — an unset context never surfaces
//! private facts. Read-only ⇒ `RiskClass::Safe` ⇒ pre-trusted (no approval
//! prompt), matching the design's "always-available, pinned search tool."

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::json;

use tauri::AppHandle;

use crate::classifier::rules::RuleCategory;
use crate::classifier::{Classification, Classifier, Label};
use crate::embedder::{EmbedderHandle, TextEmbedder};
use crate::storage::{GlobalDb, MemoryBucket, MemoryFact, Storage, SEMANTIC_MAX_DIST_RECALL};
use crate::tools::{Capability, ExecCtx, RiskClass, Tool, ToolInput, ToolResult};

/// How many matches `recall_memory` returns (PLAN §9: "only the top handful").
const RECALL_LIMIT: usize = 5;

/// Whether a profile has the meaning-lane (semantic memory search) enabled
/// (Wave 1.2). Defaults to `true` when settings can't be read, matching the
/// pre-Wave-1 behavior. Central so the recall/remember tools and the loop agree.
pub fn semantic_search_enabled(storage: &Storage, profile: &str) -> bool {
    storage
        .open_profile(profile)
        .and_then(|db| db.memory_settings())
        .map(|s| s.semantic_search_enabled)
        .unwrap_or(true)
}

/// Best-effort embed-and-index of a just-saved fact into its bucket's vector
/// table — the meaning lane of hybrid search. Canonical here so the agent's
/// `remember` and the manual `save_memory` IPC index identically. `mem` is the
/// resolved memory DB the fact was written to (shared global, or a walled
/// profile's own DB) — so the embedding lands in the SAME store as the fact
/// (Wave 1.5). Failure is logged, never propagated: the fact is already saved,
/// and the boot-time backfill (`facts_missing_embedding`) re-tries any fact left
/// unindexed.
pub fn embed_fact_best_effort(
    mem: &GlobalDb,
    embedder: Option<&Arc<dyn TextEmbedder>>,
    bucket: MemoryBucket,
    fact: &MemoryFact,
) {
    let Some(emb) = embedder else { return };
    match emb.embed_passage(&fact.content) {
        Ok(v) => {
            if let Err(e) = mem.upsert_memory_embedding(bucket, &fact.id, &v) {
                tracing::warn!(target: "lhp::memory", error = %e, fact = %fact.id,
                    "failed to store memory embedding (backfill will retry)");
            }
        }
        Err(e) => tracing::warn!(target: "lhp::memory", error = %e, fact = %fact.id,
            "failed to embed memory fact (backfill will retry)"),
    }
}

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

/// The agent's memory-search tool. Holds a `Storage` handle (cheap Arc clone)
/// and, when the embedding model is installed, the embedder that powers the
/// meaning lane of the hybrid search (keyword-only otherwise).
pub struct RecallMemoryTool {
    storage: Storage,
    embedder: Option<Arc<EmbedderHandle>>,
}

impl RecallMemoryTool {
    pub fn new(storage: Storage) -> Self {
        Self {
            storage,
            embedder: None,
        }
    }

    pub fn with_embedder(mut self, embedder: Option<Arc<EmbedderHandle>>) -> Self {
        self.embedder = embedder;
        self
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
        ctx: &'a ExecCtx,
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
            // Route to the profile's memory store — shared global.db, or a
            // walled profile's own physically-separate DB (Wave 1.5). An error
            // here fails the recall (rather than silently searching the wrong
            // store).
            let mem = match self.storage.memory_db_for_profile(&ctx.profile) {
                Ok(m) => m,
                Err(e) => return ToolResult::Err(format!("recall_memory failed: {e}")),
            };
            // Hybrid search: keyword (FTS) + meaning (sqlite-vec) fused by
            // rank, when semantic search is enabled for this profile (Wave 1.2)
            // AND the embedder loads; keyword-only otherwise. An embed failure
            // degrades to keyword-only rather than failing the recall.
            let embedder = if semantic_search_enabled(&self.storage, &ctx.profile) {
                self.embedder.as_ref().and_then(|h| h.get())
            } else {
                None
            };
            let query_vec = embedder.as_ref().and_then(|e| {
                e.embed_query(&query)
                    .map_err(|err| {
                        tracing::warn!(target: "lhp::memory", error = %err,
                            "query embedding failed — keyword-only recall");
                    })
                    .ok()
            });
            // Private-local facts are readable ONLY on a non-cloud turn
            // (`allow_private_memory`, set by the dispatcher to `!is_cloud`) AND
            // only from the ACTIVE profile — shared facts are cross-profile (one
            // coherent memory, §7), but a private-local fact never crosses the
            // profile boundary. A cloud turn stays shared-only.
            match mem.search_memory_for_recall_hybrid(
                &query,
                query_vec.as_deref(),
                &ctx.profile,
                ctx.allow_private_memory,
                SEMANTIC_MAX_DIST_RECALL,
                RECALL_LIMIT,
            ) {
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
    embedder: Option<Arc<EmbedderHandle>>,
    /// The app handle, when running inside the real Tauri app, so a save can
    /// fire the non-silent "remembered" `memory:event` (Wave 1.4). `None` in
    /// tests — the save still happens, just without the UI signal.
    app: Option<AppHandle>,
}

impl RememberMemoryTool {
    pub fn new(storage: Storage, classifier: Arc<dyn Classifier>) -> Self {
        Self {
            storage,
            classifier,
            embedder: None,
            app: None,
        }
    }

    pub fn with_embedder(mut self, embedder: Option<Arc<EmbedderHandle>>) -> Self {
        self.embedder = embedder;
        self
    }

    /// Attach the Tauri app handle so a successful save emits the non-silent
    /// "remembered" event (Wave 1.4).
    pub fn with_app_handle(mut self, app: Option<AppHandle>) -> Self {
        self.app = app;
        self
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
            // Classify under the active profile's own thresholds so the agent's
            // save routes to the same sensitivity bucket the profile's gate (and
            // the manual `save_memory` IPC) would pick for identical content. A
            // settings read error falls back to defaults (never blocks a save).
            let cfg = self
                .storage
                .open_profile(&ctx.profile)
                .and_then(|db| db.classifier_config())
                .unwrap_or_default();
            let classification = self.classifier.classify_with(&content, &cfg);
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
            // Route to the profile's memory store — shared global.db, or a
            // walled profile's own physically-separate DB (Wave 1.5).
            let mem = match self.storage.memory_db_for_profile(&ctx.profile) {
                Ok(m) => m,
                Err(e) => return ToolResult::Err(format!("remember failed: {e}")),
            };
            match mem.insert_memory_fact_in(bucket, &fact) {
                Ok(()) => {
                    // Meaning-lane index — gated by the profile's semantic
                    // setting (Wave 1.2) and written to the SAME store as the fact.
                    let embedder = if semantic_search_enabled(&self.storage, &ctx.profile) {
                        self.embedder.as_ref().and_then(|h| h.get())
                    } else {
                        None
                    };
                    embed_fact_best_effort(&mem, embedder.as_ref(), bucket, &fact);
                    // Non-silent "remembered" signal (Wave 1.4) — content-free.
                    if let Some(app) = &self.app {
                        crate::agent::loop_mod::emit_memory_event(
                            app,
                            &ctx.conversation_id,
                            "remembered",
                            1,
                        );
                    }
                    ToolResult::Ok(json!({
                    "saved": true,
                    "sensitivity": if bucket == MemoryBucket::PrivateLocal {
                        "private_local"
                    } else {
                        "shared"
                    },
                    "id": fact.id,
                    }))
                }
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

        // On a CLOUD turn (`ctx` default: allow_private_memory=false) a
        // private-only term returns NOTHING — the private store is never queried,
        // so a private-local fact can't leak into a cloud model's context.
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
                    "a cloud turn must never surface a private-local fact"
                );
            }
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        }

        // On a LOCAL turn (allow_private_memory=true) in the SAME profile the
        // query DOES surface the private fact — a local model may read the active
        // profile's private-local memory.
        let local_ctx = ExecCtx {
            profile: "personal".into(),
            allow_private_memory: true,
            ..ExecCtx::default()
        };
        match tool
            .run(
                ToolInput::new(json!({ "query": "Oak Street home address" })),
                &local_ctx,
            )
            .await
        {
            ToolResult::Ok(v) => {
                let matches = v["matches"].as_array().unwrap();
                assert_eq!(matches.len(), 1, "a local turn may read private-local memory");
                assert!(matches[0]["content"].as_str().unwrap().contains("Oak Street"));
            }
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        }

        // Cross-profile isolation (regression): a DIFFERENT profile's local turn
        // must NOT recall "personal"'s private-local fact — but shared facts are
        // cross-profile, so it still sees the shared one.
        let other_ctx = ExecCtx {
            profile: "work".into(),
            allow_private_memory: true,
            ..ExecCtx::default()
        };
        match tool
            .run(ToolInput::new(json!({ "query": "Oak Street home address" })), &other_ctx)
            .await
        {
            ToolResult::Ok(v) => assert!(
                v["matches"].as_array().unwrap().is_empty(),
                "a private-local fact must never cross the profile boundary"
            ),
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        }
        match tool
            .run(ToolInput::new(json!({ "query": "deploy key vault" })), &other_ctx)
            .await
        {
            ToolResult::Ok(v) => assert_eq!(
                v["matches"].as_array().unwrap().len(),
                1,
                "shared facts stay cross-profile"
            ),
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
            allow_private_memory: false,
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

    /// A model-scored classifier used to prove `remember` applies the active
    /// profile's thresholds (not the hardcoded defaults). `classify` uses the
    /// default config; `classify_with` grades the fixed score against `cfg`.
    #[derive(Debug)]
    struct ScoreFake(f32);
    impl crate::classifier::Classifier for ScoreFake {
        fn classify(&self, text: &str) -> Classification {
            self.classify_with(text, &crate::classifier::ClassifierConfig::default())
        }
        fn classify_with(
            &self,
            _text: &str,
            cfg: &crate::classifier::ClassifierConfig,
        ) -> Classification {
            let label = if self.0 >= cfg.tau_block {
                Label::Private
            } else if self.0 >= cfg.tau_band {
                Label::Uncertain
            } else {
                Label::Public
            };
            Classification {
                label,
                confidence: self.0,
                raw_output: vec![self.0],
                spans: Vec::new(),
            }
        }
    }

    #[tokio::test]
    async fn remember_applies_the_profiles_classifier_thresholds() {
        // A borderline score of 0.03: under the DEFAULT config (tau_band 0.05)
        // it's Public → Shared; under a STRICT profile config (strictness 100,
        // tau_band ≈ 0.005) it's Uncertain → PrivateLocal. Proves the tool reads
        // and applies the per-profile thresholds, not a hardcoded default.
        let (storage, root) = temp_storage();
        let tool = RememberMemoryTool::new(storage.clone(), Arc::new(ScoreFake(0.03)));
        let ctx = ExecCtx {
            conversation_id: "c".into(),
            profile: "personal".into(),
            reads: None,
            allow_private_memory: false,
        };

        // Default profile (no settings row) → Shared.
        match tool
            .run(ToolInput::new(json!({ "content": "a borderline note" })), &ctx)
            .await
        {
            ToolResult::Ok(v) => assert_eq!(v["sensitivity"], "shared", "default cfg ⇒ shared"),
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        }

        // Now store a strict config for the profile and remember again.
        storage
            .open_profile("personal")
            .unwrap()
            .set_classifier_config(&crate::classifier::ClassifierConfig::from_ui(100, "medium"))
            .unwrap();
        match tool
            .run(ToolInput::new(json!({ "content": "another borderline note" })), &ctx)
            .await
        {
            ToolResult::Ok(v) => assert_eq!(
                v["sensitivity"], "private_local",
                "strict profile cfg must route the same borderline content local"
            ),
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        }

        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod hybrid_tests {
    use super::*;
    use crate::embedder::FakeEmbedder;
    use crate::storage::MemoryBucket;
    use serde_json::json;

    fn temp_storage() -> (Storage, std::path::PathBuf) {
        let mut root = std::env::temp_dir();
        root.push(format!("lhp-mem-hybrid-{}", uuid::Uuid::new_v4()));
        let storage = Storage::open(&root).unwrap();
        (storage, root)
    }

    /// The headline PLAN §9 behavior: "a search for 'that thing about the
    /// deploy key' finds a fact that was phrased completely differently."
    #[tokio::test]
    async fn recall_finds_a_differently_phrased_fact_via_the_meaning_lane() {
        let (storage, root) = temp_storage();
        // Fake semantics: both phrasings land on axis 0.
        let fake: Arc<dyn TextEmbedder> =
            Arc::new(FakeEmbedder(vec![("vault", 0), ("sign-in secret", 0)]));

        let fact = MemoryFact {
            id: "f".into(),
            content: "the vault on artoo holds the door code".into(),
            origin_profile: "personal".into(),
            tags: None,
            created_at: 1,
            pinned: false,
        };
        storage.global().insert_memory_fact_in(MemoryBucket::Shared, &fact).unwrap();
        embed_fact_best_effort(storage.global(), Some(&fake), MemoryBucket::Shared, &fact);

        let tool =
            RecallMemoryTool::new(storage.clone()).with_embedder(Some(EmbedderHandle::ready(fake)));
        // Zero keyword overlap with the fact (and stopwords don't count).
        match tool
            .run(ToolInput::new(json!({ "query": "sign-in secret" })), &ExecCtx::default())
            .await
        {
            ToolResult::Ok(v) => {
                let matches = v["matches"].as_array().unwrap();
                assert_eq!(matches.len(), 1, "meaning lane must surface the fact");
                assert!(matches[0]["content"].as_str().unwrap().contains("door code"));
            }
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn remember_with_embedder_indexes_the_saved_fact() {
        let (storage, root) = temp_storage();
        let fake: Arc<dyn TextEmbedder> = Arc::new(FakeEmbedder(vec![("standup", 1)]));
        let tool = RememberMemoryTool::new(
            storage.clone(),
            Arc::new(crate::classifier::RulesClassifier::new()),
        )
        .with_embedder(Some(EmbedderHandle::ready(fake)));

        let saved_id = match tool
            .run(
                ToolInput::new(json!({ "content": "the standup is at 10am" })),
                &ExecCtx::default(),
            )
            .await
        {
            ToolResult::Ok(v) => v["id"].as_str().unwrap().to_string(),
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        };
        let vecs = storage.global().list_vectors_for_fact(&saved_id).unwrap();
        assert_eq!(vecs.len(), 1, "remember must index the fact for the meaning lane");
        assert_eq!(vecs[0].embedding.len(), 8 * 4, "8-dim f32 blob");
        let _ = std::fs::remove_dir_all(root);
    }

    /// Wave 1.2: with semantic search OFF for the profile, `remember` must NOT
    /// compute or store an embedding (the "hard off switch for computing a
    /// meaning fingerprint of everything I save"); turning it on restores it.
    #[tokio::test]
    async fn remember_skips_embedding_when_semantic_search_off() {
        use crate::storage::MemorySettings;
        let (storage, root) = temp_storage();
        let fake: Arc<dyn TextEmbedder> = Arc::new(FakeEmbedder(vec![("standup", 1)]));
        let tool = RememberMemoryTool::new(
            storage.clone(),
            Arc::new(crate::classifier::RulesClassifier::new()),
        )
        .with_embedder(Some(EmbedderHandle::ready(fake)));
        let ctx = ExecCtx {
            conversation_id: "c".into(),
            profile: "personal".into(),
            reads: None,
            allow_private_memory: false,
        };

        // Semantic OFF → no vector stored.
        storage
            .open_profile("personal")
            .unwrap()
            .set_memory_settings(&MemorySettings { semantic_search_enabled: false, walled: false })
            .unwrap();
        let off_id = match tool
            .run(ToolInput::new(json!({ "content": "the standup is at 10am" })), &ctx)
            .await
        {
            ToolResult::Ok(v) => v["id"].as_str().unwrap().to_string(),
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        };
        assert!(
            storage.global().list_vectors_for_fact(&off_id).unwrap().is_empty(),
            "semantic off ⇒ no meaning fingerprint computed"
        );

        // Semantic ON → vector stored.
        storage
            .open_profile("personal")
            .unwrap()
            .set_memory_settings(&MemorySettings { semantic_search_enabled: true, walled: false })
            .unwrap();
        let on_id = match tool
            .run(ToolInput::new(json!({ "content": "the standup moved to 9am" })), &ctx)
            .await
        {
            ToolResult::Ok(v) => v["id"].as_str().unwrap().to_string(),
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        };
        assert_eq!(
            storage.global().list_vectors_for_fact(&on_id).unwrap().len(),
            1,
            "semantic on ⇒ the meaning lane indexes the fact"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
