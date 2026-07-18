//! `GlobalDb` — CRUD over `global.db`.
//!
//! Holds data shared across all profiles: user facts, endpoints, model
//! catalog, memory facts + vectors, skills, app settings. See spec §1.
//!
//! All write methods use prepared statements. None of the methods require
//! a `&mut self` since rusqlite::Connection is internally synchronized via
//! `parking_lot::Mutex` when the higher-level `Storage` wraps it — but for
//! now the bare `Connection` works fine on its own.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use super::migrations::migrate_global;

// ─────────────────────────────────────────────────────────────────────────────
// Row types (one per table). Public so callers can move them around without
// reaching for untyped tuples.
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct UserFact {
    pub key: String,
    pub value: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub api_key_encrypted: Option<Vec<u8>>,
    pub kind: String,
    pub created_at: i64,
    /// Q1: this endpoint's API supports OpenAI-style structured tool calls.
    pub supports_native_tools: bool,
}

#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub id: String,
    pub name: String,
    pub path: String,
    pub size_bytes: i64,
    pub quantization: Option<String>,
    pub added_at: i64,
}

#[derive(Debug, Clone)]
pub struct MemoryFact {
    pub id: String,
    pub content: String,
    pub origin_profile: String,
    /// JSON-encoded array of tag strings. `None` when no tags.
    pub tags: Option<String>,
    pub created_at: i64,
    /// Pinned into the always-loaded curated summary (PLAN §9). Defaults false.
    pub pinned: bool,
}

/// Which physical store a memory fact lives in — the sensitivity bucket
/// (PLAN §9). `Shared` facts (in `memory_facts`) may inform any turn including
/// cloud; `PrivateLocal` facts (in the physically-separate
/// `memory_facts_private`) are only ever read by a local turn — a cloud-bound
/// context assembly never queries that table. (`never-persist` isn't a stored
/// bucket: those facts are dropped, never written.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryBucket {
    Shared,
    PrivateLocal,
}

impl MemoryBucket {
    fn table(self) -> &'static str {
        match self {
            MemoryBucket::Shared => "memory_facts",
            MemoryBucket::PrivateLocal => "memory_facts_private",
        }
    }
    fn fts_table(self) -> &'static str {
        match self {
            MemoryBucket::Shared => "memory_facts_fts",
            MemoryBucket::PrivateLocal => "memory_facts_private_fts",
        }
    }
    /// The bucket's embedding table — physically separate per bucket for the
    /// same reason the fact tables are: a cloud-bound semantic search never
    /// even queries the private vector index.
    fn vectors_table(self) -> &'static str {
        match self {
            MemoryBucket::Shared => "memory_vectors",
            MemoryBucket::PrivateLocal => "memory_vectors_private",
        }
    }
}

/// Semantic-lane relevance gates (cosine *distance*, i.e. `1 − similarity`,
/// on L2-normalized bge-small vectors — lower is nearer). Calibrated against
/// the live INT8 model (`embedder::live_gate_calibration`, 2026-07-16):
/// clearly-related pairs measure ≈0.33, adjacent-topic ≈0.43, unrelated
/// ≈0.54+. Tune with real usage (PLAN §9 "the genuinely hard part").
///
/// Auto-injection must stay quiet on most turns: admit clearly-related only.
pub const SEMANTIC_MAX_DIST_INJECT: f64 = 0.38;
/// An explicit `recall_memory` search casts wider — the user/agent asked —
/// admitting adjacent-topic matches but still excluding unrelated.
pub const SEMANTIC_MAX_DIST_RECALL: f64 = 0.48;

/// A memory search hit: the fact, which bucket it came from, and its keyword
/// relevance (lower bm25 = better; kept as the raw score for the caller to
/// merge with the future semantic lane).
#[derive(Debug, Clone)]
pub struct MemorySearchHit {
    pub fact: MemoryFact,
    pub bucket: MemoryBucket,
    pub score: f64,
}

/// Map a row of `(id, content, origin_profile, tags, created_at, pinned)` to a
/// `MemoryFact`. Shared by every memory read path (both buckets, same columns).
fn row_to_memory_fact(r: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryFact> {
    Ok(MemoryFact {
        id: r.get(0)?,
        content: r.get(1)?,
        origin_profile: r.get(2)?,
        tags: r.get(3)?,
        created_at: r.get(4)?,
        pinned: r.get::<_, i64>(5)? != 0,
    })
}

/// Turn an arbitrary user query into a safe FTS5 MATCH expression: extract
/// alphanumeric tokens, quote each (so FTS5 operators or punctuation in the raw
/// text can't break the query or inject syntax), and OR them for recall.
/// Returns `None` when the query has no usable tokens.
fn fts_match_expr(query: &str) -> Option<String> {
    /// English function words that would otherwise make the OR-recall keyword
    /// lane match nearly every fact ("the", "is", …) — turning the automatic
    /// injection's "shares search tokens" relevance gate into a fire-always.
    /// Content words only; a query that is ALL stopwords gets no keyword lane
    /// (the meaning lane, when installed, can still match it).
    const STOPWORDS: &[&str] = &[
        "a", "an", "and", "are", "as", "at", "be", "but", "by", "did", "do", "does", "for",
        "from", "had", "has", "have", "how", "i", "if", "in", "is", "it", "its", "me", "my",
        "no", "not", "of", "on", "or", "our", "so", "that", "the", "their", "them", "then",
        "there", "these", "they", "this", "to", "was", "we", "were", "what", "when", "where",
        "which", "who", "why", "will", "with", "would", "you", "your",
    ];
    let tokens: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        .map(|t| format!("\"{t}\""))
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" OR "))
    }
}

/// Reciprocal Rank Fusion over the keyword and semantic result lists: each
/// list contributes `1 / (60 + rank)` per fact (rank 1-based; 60 is the
/// standard RRF constant), scores summed across lists, deduped by fact id,
/// sorted best-first (HIGHER fused score = better), capped at `limit`. Rank
/// fusion sidesteps ever comparing bm25 against cosine distance directly —
/// the two scales share nothing.
fn rrf_fuse(
    keyword: Vec<MemorySearchHit>,
    semantic: Vec<MemorySearchHit>,
    limit: usize,
) -> Vec<MemorySearchHit> {
    const K: f64 = 60.0;
    let mut fused: Vec<MemorySearchHit> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for list in [keyword, semantic] {
        for (rank, mut hit) in list.into_iter().enumerate() {
            let contribution = 1.0 / (K + rank as f64 + 1.0);
            match index.get(&hit.fact.id) {
                Some(&i) => fused[i].score += contribution,
                None => {
                    index.insert(hit.fact.id.clone(), fused.len());
                    hit.score = contribution;
                    fused.push(hit);
                }
            }
        }
    }
    fused.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    fused.truncate(limit);
    fused
}

#[derive(Debug, Clone)]
pub struct MemoryVector {
    pub id: i64,
    pub fact_id: String,
    pub embedding: Vec<u8>,
}

/// A reusable playbook (Wave 4.1). Rides the same tool spine as everything else
/// (a skill becomes a `Tool`); the `capabilities_required` gate which bodies can
/// even be offered, and `approval_status` is the trust boundary — only
/// `Approved` skills are searchable / loadable. `capabilities_required` is stored
/// as capability NAME strings (JSON) so the storage layer stays independent of
/// `tools::Capability`; the tools layer parses them.
#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub content: String,
    pub capabilities_required: Vec<String>,
    pub approval_status: SkillApproval,
    pub path: String,
    pub version: String,
    pub created_at: i64,
}

/// The trust state of a skill. `Approved` is the boundary (CC's install-time
/// trust, re-expressed as our review gate) — only approved skills are offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillApproval {
    Pending,
    Approved,
    Rejected,
}

impl SkillApproval {
    pub fn as_str(self) -> &'static str {
        match self {
            SkillApproval::Pending => "pending",
            SkillApproval::Approved => "approved",
            SkillApproval::Rejected => "rejected",
        }
    }
    pub fn from_str(s: &str) -> SkillApproval {
        // Unknown/legacy values fail CLOSED to `Pending` (never auto-trusted).
        match s {
            "approved" => SkillApproval::Approved,
            "rejected" => SkillApproval::Rejected,
            _ => SkillApproval::Pending,
        }
    }
}

/// Parse a `skills` row (9 columns) into a [`Skill`]. `capabilities_required` is
/// a JSON array of capability-name strings; a corrupt value degrades to empty
/// (the skill then requires nothing — but `approval_status` still gates it).
fn row_to_skill(r: &rusqlite::Row<'_>) -> rusqlite::Result<Skill> {
    let caps_json: String = r.get(4)?;
    let capabilities_required: Vec<String> = serde_json::from_str(&caps_json).unwrap_or_default();
    let status: String = r.get(5)?;
    Ok(Skill {
        id: r.get(0)?,
        name: r.get(1)?,
        description: r.get(2)?,
        content: r.get(3)?,
        capabilities_required,
        approval_status: SkillApproval::from_str(&status),
        path: r.get(6)?,
        version: r.get(7)?,
        created_at: r.get(8)?,
    })
}

#[derive(Debug, Clone)]
pub struct AppSetting {
    pub key: String,
    pub value: String,
    pub updated_at: i64,
}

// ─────────────────────────────────────────────────────────────────────────────
// GlobalDb
// ─────────────────────────────────────────────────────────────────────────────

pub struct GlobalDb {
    conn: parking_lot::Mutex<Connection>,
}

impl GlobalDb {
    /// Open an existing global.db (or create + migrate a fresh one) at `path`.
    pub fn open(path: &std::path::Path) -> Result<Self> {
        crate::storage::ensure_sqlite_vec_registered();
        let conn = Connection::open(path)
            .with_context(|| format!("opening global.db at {}", path.display()))?;
        migrate_global(&conn)?;
        Ok(Self { conn: parking_lot::Mutex::new(conn) })
    }

    /// In-memory variant for tests.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        crate::storage::ensure_sqlite_vec_registered();
        let conn = Connection::open_in_memory()?;
        migrate_global(&conn)?;
        Ok(Self { conn: parking_lot::Mutex::new(conn) })
    }

    /// Lock and borrow the underlying connection. Use sparingly — most
    /// callers should go through the typed methods. The returned guard holds
    /// the connection's mutex for as long as it lives; a caller must not
    /// invoke another locking method on this same `GlobalDb` while holding
    /// it — `parking_lot::Mutex` is not reentrant, so that would deadlock.
    pub fn raw(&self) -> parking_lot::MutexGuard<'_, Connection> {
        self.conn.lock()
    }

    // ── user_facts ──────────────────────────────────────────────────────────

    pub fn set_user_fact(&self, key: &str, value: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .lock()
            .execute(
                "INSERT INTO user_facts (key, value, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                params![key, value, now],
            )
            .context("set_user_fact")?;
        Ok(())
    }

    pub fn get_user_fact(&self, key: &str) -> Result<Option<UserFact>> {
        let row = self
            .conn
            .lock()
            .query_row(
                "SELECT key, value, updated_at FROM user_facts WHERE key = ?1",
                params![key],
                |r| {
                    Ok(UserFact {
                        key: r.get(0)?,
                        value: r.get(1)?,
                        updated_at: r.get(2)?,
                    })
                },
            )
            .optional()
            .context("get_user_fact")?;
        Ok(row)
    }

    pub fn list_user_facts(&self) -> Result<Vec<UserFact>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT key, value, updated_at FROM user_facts ORDER BY key")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(UserFact {
                    key: r.get(0)?,
                    value: r.get(1)?,
                    updated_at: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn delete_user_fact(&self, key: &str) -> Result<bool> {
        let n = self
            .conn
            .lock()
            .execute("DELETE FROM user_facts WHERE key = ?1", params![key])?;
        Ok(n > 0)
    }

    // ── endpoints ───────────────────────────────────────────────────────────

    pub fn insert_endpoint(&self, ep: &Endpoint) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO endpoints (id, name, base_url, api_key_encrypted, kind, created_at, supports_native_tools)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                ep.id,
                ep.name,
                ep.base_url,
                ep.api_key_encrypted,
                ep.kind,
                ep.created_at,
                ep.supports_native_tools as i64
            ],
        )?;
        Ok(())
    }

    pub fn get_endpoint(&self, id: &str) -> Result<Option<Endpoint>> {
        Ok(self
            .conn
            .lock()
            .query_row(
                "SELECT id, name, base_url, api_key_encrypted, kind, created_at, supports_native_tools
                 FROM endpoints WHERE id = ?1",
                params![id],
                row_to_endpoint,
            )
            .optional()?)
    }

    pub fn list_endpoints(&self) -> Result<Vec<Endpoint>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, base_url, api_key_encrypted, kind, created_at, supports_native_tools
             FROM endpoints ORDER BY name",
        )?;
        let rows = stmt
            .query_map([], row_to_endpoint)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn update_endpoint(
        &self,
        id: &str,
        name: &str,
        base_url: &str,
        api_key_encrypted: Option<&[u8]>,
        kind: &str,
    ) -> Result<bool> {
        let n = self.conn.lock().execute(
            "UPDATE endpoints SET name = ?1, base_url = ?2, api_key_encrypted = ?3, kind = ?4
             WHERE id = ?5",
            params![name, base_url, api_key_encrypted, kind, id],
        )?;
        Ok(n > 0)
    }

    pub fn delete_endpoint(&self, id: &str) -> Result<bool> {
        let n = self
            .conn
            .lock()
            .execute("DELETE FROM endpoints WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    // ── model_catalog ───────────────────────────────────────────────────────

    pub fn insert_model(&self, m: &ModelEntry) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO model_catalog (id, name, path, size_bytes, quantization, added_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                m.id,
                m.name,
                m.path,
                m.size_bytes,
                m.quantization,
                m.added_at
            ],
        )?;
        Ok(())
    }

    pub fn list_models(&self) -> Result<Vec<ModelEntry>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, path, size_bytes, quantization, added_at
             FROM model_catalog ORDER BY added_at DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ModelEntry {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    path: r.get(2)?,
                    size_bytes: r.get(3)?,
                    quantization: r.get(4)?,
                    added_at: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_model(&self, id: &str) -> Result<Option<ModelEntry>> {
        Ok(self
            .conn
            .lock()
            .query_row(
                "SELECT id, name, path, size_bytes, quantization, added_at
                 FROM model_catalog WHERE id = ?1",
                params![id],
                |r| {
                    Ok(ModelEntry {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        path: r.get(2)?,
                        size_bytes: r.get(3)?,
                        quantization: r.get(4)?,
                        added_at: r.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn delete_model(&self, id: &str) -> Result<bool> {
        let n = self
            .conn
            .lock()
            .execute("DELETE FROM model_catalog WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    // ── memory_facts ────────────────────────────────────────────────────────

    /// Insert into the default (Shared) bucket. See `insert_memory_fact_in`
    /// for sensitivity routing.
    pub fn insert_memory_fact(&self, fact: &MemoryFact) -> Result<()> {
        self.insert_memory_fact_in(MemoryBucket::Shared, fact)
    }

    /// Insert a fact into the given sensitivity bucket (PLAN §9). `Shared` goes
    /// to `memory_facts`; `PrivateLocal` to the physically-separate
    /// `memory_facts_private` — so a cloud turn's search never touches it.
    pub fn insert_memory_fact_in(&self, bucket: MemoryBucket, fact: &MemoryFact) -> Result<()> {
        self.conn.lock().execute(
            &format!(
                "INSERT INTO {} (id, content, origin_profile, tags, created_at, pinned)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                bucket.table()
            ),
            params![
                fact.id,
                fact.content,
                fact.origin_profile,
                fact.tags,
                fact.created_at,
                fact.pinned as i64,
            ],
        )?;
        Ok(())
    }

    pub fn get_memory_fact(&self, id: &str) -> Result<Option<MemoryFact>> {
        Ok(self
            .conn
            .lock()
            .query_row(
                "SELECT id, content, origin_profile, tags, created_at, pinned
                 FROM memory_facts WHERE id = ?1",
                params![id],
                row_to_memory_fact,
            )
            .optional()?)
    }

    pub fn list_memory_facts(&self) -> Result<Vec<MemoryFact>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, content, origin_profile, tags, created_at, pinned
             FROM memory_facts ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map([], row_to_memory_fact)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn list_memory_facts_by_profile(&self, profile: &str) -> Result<Vec<MemoryFact>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, content, origin_profile, tags, created_at, pinned
             FROM memory_facts WHERE origin_profile = ?1 ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map(params![profile], row_to_memory_fact)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Delete a fact by id from whichever bucket holds it. Returns true if a
    /// row was removed.
    pub fn delete_memory_fact(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute("DELETE FROM memory_facts WHERE id = ?1", params![id])?
            + conn.execute(
                "DELETE FROM memory_facts_private WHERE id = ?1",
                params![id],
            )?;
        Ok(n > 0)
    }

    /// Pin/unpin a fact into the always-loaded curated summary. Checks both
    /// buckets; returns true if a row changed.
    pub fn set_memory_pinned(&self, id: &str, pinned: bool) -> Result<bool> {
        let p = pinned as i64;
        let conn = self.conn.lock();
        let n = conn.execute(
            "UPDATE memory_facts SET pinned = ?2 WHERE id = ?1",
            params![id, p],
        )? + conn.execute(
            "UPDATE memory_facts_private SET pinned = ?2 WHERE id = ?1",
            params![id, p],
        )?;
        Ok(n > 0)
    }

    /// The always-loaded curated summary for a profile (PLAN §9): pinned facts
    /// first, then most-recent, capped at `limit`. `allow_private` gates the
    /// private-local store — pass `false` for any cloud-bound assembly so the
    /// private table is never even read.
    pub fn curated_summary(
        &self,
        profile: &str,
        allow_private: bool,
        limit: usize,
    ) -> Result<Vec<MemoryFact>> {
        let mut facts = self.summary_from(MemoryBucket::Shared, profile, limit)?;
        if allow_private {
            facts.extend(self.summary_from(MemoryBucket::PrivateLocal, profile, limit)?);
        }
        // pinned first, then newest; cap.
        facts.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then(b.created_at.cmp(&a.created_at))
        });
        facts.truncate(limit);
        Ok(facts)
    }

    /// The curated-summary CANDIDATE SET for a profile, each fact tagged with
    /// its bucket and ALWAYS including the private-local store (Wave 1.3). This
    /// is the snapshot form: the conversation freezes this candidate set once at
    /// turn 1, then the per-turn renderer drops the private-local facts on a
    /// cloud turn (never queries them into a cloud prompt) and takes the top
    /// `limit` of what survives. Freezing the SET — but filtering privacy and
    /// truncating per turn — keeps the summary stable for prompt caching (PLAN
    /// §9 "Timing and trust") without ever leaking a private fact onto a cloud
    /// turn.
    ///
    /// Up to `limit` facts are kept **per bucket** (not `limit` across the
    /// union), so a cloud turn — which drops the private-local ones — can still
    /// render a full `limit` of shared facts, matching the pre-snapshot cloud
    /// behavior. The caller applies the final per-turn `limit`.
    pub fn curated_summary_with_buckets(
        &self,
        profile: &str,
        limit: usize,
    ) -> Result<Vec<(MemoryFact, MemoryBucket)>> {
        let mut facts: Vec<(MemoryFact, MemoryBucket)> = self
            .summary_from(MemoryBucket::Shared, profile, limit)?
            .into_iter()
            .map(|f| (f, MemoryBucket::Shared))
            .collect();
        facts.extend(
            self.summary_from(MemoryBucket::PrivateLocal, profile, limit)?
                .into_iter()
                .map(|f| (f, MemoryBucket::PrivateLocal)),
        );
        facts.sort_by(|a, b| {
            b.0.pinned
                .cmp(&a.0.pinned)
                .then(b.0.created_at.cmp(&a.0.created_at))
        });
        // NB: intentionally NOT truncated to `limit` here — the caller filters
        // by endpoint privacy first, then takes the top `limit`, so a cloud turn
        // isn't shorted by private facts that occupied union slots.
        Ok(facts)
    }

    /// All of a profile's memory facts, newest first, each tagged with its
    /// bucket. `include_private` gates the private-local store (pass `false`
    /// for any cloud-bound reader; `true` for the user's own local memory view).
    pub fn list_memory_by_profile(
        &self,
        profile: &str,
        include_private: bool,
    ) -> Result<Vec<(MemoryFact, MemoryBucket)>> {
        let mut out: Vec<(MemoryFact, MemoryBucket)> = self
            .list_bucket_by_profile(MemoryBucket::Shared, profile)?
            .into_iter()
            .map(|f| (f, MemoryBucket::Shared))
            .collect();
        if include_private {
            out.extend(
                self.list_bucket_by_profile(MemoryBucket::PrivateLocal, profile)?
                    .into_iter()
                    .map(|f| (f, MemoryBucket::PrivateLocal)),
            );
        }
        out.sort_by(|a, b| {
            b.0.pinned
                .cmp(&a.0.pinned)
                .then(b.0.created_at.cmp(&a.0.created_at))
        });
        Ok(out)
    }

    fn list_bucket_by_profile(
        &self,
        bucket: MemoryBucket,
        profile: &str,
    ) -> Result<Vec<MemoryFact>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT id, content, origin_profile, tags, created_at, pinned
             FROM {} WHERE origin_profile = ?1 ORDER BY created_at DESC",
            bucket.table()
        ))?;
        let rows = stmt
            .query_map(params![profile], row_to_memory_fact)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn summary_from(
        &self,
        bucket: MemoryBucket,
        profile: &str,
        limit: usize,
    ) -> Result<Vec<MemoryFact>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT id, content, origin_profile, tags, created_at, pinned
             FROM {} WHERE origin_profile = ?1
             ORDER BY pinned DESC, created_at DESC LIMIT ?2",
            bucket.table()
        ))?;
        let rows = stmt
            .query_map(params![profile, limit as i64], row_to_memory_fact)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Keyword (FTS5) memory search — the keyword lane of PLAN §9's hybrid
    /// search (the sqlite-vec meaning lane layers on once an embedder lands).
    /// Searches the Shared store, plus PrivateLocal only when `allow_private`
    /// is true — a cloud turn passes `false` and the private index is never
    /// queried. Results are ranked by bm25 (best first) and capped at `limit`.
    pub fn search_memory(
        &self,
        query: &str,
        allow_private: bool,
        limit: usize,
    ) -> Result<Vec<MemorySearchHit>> {
        self.search_memory_impl(query, None, allow_private, limit)
    }

    /// Like [`search_memory`], but restricted to facts whose `origin_profile`
    /// matches `profile`. Used by the automatic relevance-gated injection
    /// (PLAN §9), which must not surface one profile's facts into another
    /// profile's turn. (The `recall_memory` tool stays cross-profile — shared
    /// facts are one coherent memory of the user across profiles, §7.)
    pub fn search_memory_scoped(
        &self,
        query: &str,
        profile: &str,
        allow_private: bool,
        limit: usize,
    ) -> Result<Vec<MemorySearchHit>> {
        self.search_memory_impl(query, Some(profile), allow_private, limit)
    }

    /// Search for the `recall_memory` tool: **shared** facts across ALL profiles
    /// (one coherent memory of the user, §7) plus the ACTIVE profile's
    /// **private-local** facts, and only when `allow_private` (a non-cloud turn).
    /// A private-local fact from a *different* profile is never surfaced — that
    /// would cross the profile boundary for the most sensitive bucket.
    pub fn search_memory_for_recall(
        &self,
        query: &str,
        profile: &str,
        allow_private: bool,
        limit: usize,
    ) -> Result<Vec<MemorySearchHit>> {
        let Some(match_expr) = fts_match_expr(query) else {
            return Ok(Vec::new());
        };
        // Shared: cross-profile. Private-local: scoped to the active profile.
        let mut hits = self.search_bucket(MemoryBucket::Shared, &match_expr, None, limit)?;
        if allow_private {
            hits.extend(self.search_bucket(
                MemoryBucket::PrivateLocal,
                &match_expr,
                Some(profile),
                limit,
            )?);
        }
        hits.sort_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(limit);
        Ok(hits)
    }

    fn search_memory_impl(
        &self,
        query: &str,
        profile: Option<&str>,
        allow_private: bool,
        limit: usize,
    ) -> Result<Vec<MemorySearchHit>> {
        let Some(match_expr) = fts_match_expr(query) else {
            return Ok(Vec::new());
        };
        let mut hits = self.search_bucket(MemoryBucket::Shared, &match_expr, profile, limit)?;
        if allow_private {
            hits.extend(self.search_bucket(
                MemoryBucket::PrivateLocal,
                &match_expr,
                profile,
                limit,
            )?);
        }
        // Lower bm25 = more relevant. Stable sort across the merged buckets.
        hits.sort_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal));
        hits.truncate(limit);
        Ok(hits)
    }

    fn search_bucket(
        &self,
        bucket: MemoryBucket,
        match_expr: &str,
        profile: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MemorySearchHit>> {
        // Reference the FTS table by its real name (not an alias) — FTS5's
        // bm25() aux function requires it. When `profile` is set, restrict to
        // that profile's facts (the injection path); otherwise all profiles.
        let profile_clause = if profile.is_some() {
            "AND f.origin_profile = ?3"
        } else {
            ""
        };
        let sql = format!(
            "SELECT f.id, f.content, f.origin_profile, f.tags, f.created_at, f.pinned,
                    bm25({fts}) AS score
             FROM {fts}
             JOIN {tbl} f ON f.rowid = {fts}.rowid
             WHERE {fts} MATCH ?1 {profile_clause}
             ORDER BY score LIMIT ?2",
            fts = bucket.fts_table(),
            tbl = bucket.table(),
        );
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&sql)?;
        let map_hit = |r: &rusqlite::Row<'_>| {
            Ok(MemorySearchHit {
                fact: row_to_memory_fact(r)?,
                bucket,
                score: r.get(6)?,
            })
        };
        let rows = match profile {
            Some(p) => stmt
                .query_map(params![match_expr, limit as i64, p], map_hit)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => stmt
                .query_map(params![match_expr, limit as i64], map_hit)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        Ok(rows)
    }

    // ── hybrid search (keyword + meaning, PLAN §9) ──────────────────────────

    /// Hybrid variant of [`search_memory_scoped`] — the automatic-injection
    /// path. When `query_vec` is `Some` (an embedder is installed), the
    /// keyword (FTS) lane and the meaning (sqlite-vec) lane run together and
    /// are fused by Reciprocal Rank Fusion; `None` degrades to keyword-only.
    /// The semantic lane is gated at `max_dist` so injection stays quiet
    /// unless a fact is genuinely near. Hit `score` is the RRF score
    /// (HIGHER = better — unlike the raw-bm25 keyword functions).
    pub fn search_memory_scoped_hybrid(
        &self,
        query: &str,
        query_vec: Option<&[f32]>,
        profile: &str,
        allow_private: bool,
        max_dist: f64,
        limit: usize,
    ) -> Result<Vec<MemorySearchHit>> {
        let keyword = self.search_memory_scoped(query, profile, allow_private, limit)?;
        let semantic = match query_vec {
            Some(qv) => {
                let mut s =
                    self.semantic_search_bucket(MemoryBucket::Shared, qv, Some(profile), max_dist, limit)?;
                if allow_private {
                    s.extend(self.semantic_search_bucket(
                        MemoryBucket::PrivateLocal,
                        qv,
                        Some(profile),
                        max_dist,
                        limit,
                    )?);
                }
                s.sort_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal));
                s.truncate(limit);
                s
            }
            None => Vec::new(),
        };
        Ok(rrf_fuse(keyword, semantic, limit))
    }

    /// Hybrid variant of [`search_memory_for_recall`] — the `recall_memory`
    /// tool. Same bucket/profile scoping as the keyword version (shared =
    /// cross-profile; private-local = active profile only, and only when
    /// `allow_private`); same RRF fusion + `max_dist` gate as
    /// [`search_memory_scoped_hybrid`].
    pub fn search_memory_for_recall_hybrid(
        &self,
        query: &str,
        query_vec: Option<&[f32]>,
        profile: &str,
        allow_private: bool,
        max_dist: f64,
        limit: usize,
    ) -> Result<Vec<MemorySearchHit>> {
        let keyword = self.search_memory_for_recall(query, profile, allow_private, limit)?;
        let semantic = match query_vec {
            Some(qv) => {
                let mut s =
                    self.semantic_search_bucket(MemoryBucket::Shared, qv, None, max_dist, limit)?;
                if allow_private {
                    s.extend(self.semantic_search_bucket(
                        MemoryBucket::PrivateLocal,
                        qv,
                        Some(profile),
                        max_dist,
                        limit,
                    )?);
                }
                s.sort_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal));
                s.truncate(limit);
                s
            }
            None => Vec::new(),
        };
        Ok(rrf_fuse(keyword, semantic, limit))
    }

    /// Nearest facts in one bucket by cosine distance, best first, gated at
    /// `max_dist`. `profile` restricts to that profile's facts. The
    /// `length(...)` guard skips any row whose blob isn't this embedder's
    /// dimension (a stale/corrupt row must not error the whole query). Hit
    /// `score` here is the raw cosine DISTANCE (lower = nearer).
    fn semantic_search_bucket(
        &self,
        bucket: MemoryBucket,
        query_vec: &[f32],
        profile: Option<&str>,
        max_dist: f64,
        limit: usize,
    ) -> Result<Vec<MemorySearchHit>> {
        let blob = crate::embedder::vec_to_blob(query_vec);
        let profile_clause = if profile.is_some() {
            "AND f.origin_profile = ?4"
        } else {
            ""
        };
        let sql = format!(
            "SELECT f.id, f.content, f.origin_profile, f.tags, f.created_at, f.pinned,
                    vec_distance_cosine(v.embedding, ?1) AS dist
             FROM {vec} v
             JOIN {tbl} f ON f.id = v.fact_id
             WHERE length(v.embedding) = ?2 {profile_clause}
             ORDER BY dist LIMIT ?3",
            vec = bucket.vectors_table(),
            tbl = bucket.table(),
        );
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&sql)?;
        let map_hit = |r: &rusqlite::Row<'_>| {
            Ok(MemorySearchHit {
                fact: row_to_memory_fact(r)?,
                bucket,
                score: r.get(6)?,
            })
        };
        let blob_len = blob.len() as i64;
        let rows = match profile {
            Some(p) => stmt
                .query_map(params![blob, blob_len, limit as i64, p], map_hit)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => stmt
                .query_map(params![blob, blob_len, limit as i64], map_hit)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        Ok(rows.into_iter().filter(|h| h.score <= max_dist).collect())
    }

    // ── memory_vectors ─────────────────────────────────────────────────────

    /// Store `fact_id`'s embedding in its bucket's vector table, replacing any
    /// previous embedding for that fact (re-saving/re-embedding is idempotent).
    pub fn upsert_memory_embedding(
        &self,
        bucket: MemoryBucket,
        fact_id: &str,
        embedding: &[f32],
    ) -> Result<()> {
        let blob = crate::embedder::vec_to_blob(embedding);
        let conn = self.conn.lock();
        conn.execute(
            &format!("DELETE FROM {} WHERE fact_id = ?1", bucket.vectors_table()),
            params![fact_id],
        )?;
        conn.execute(
            &format!(
                "INSERT INTO {} (fact_id, embedding) VALUES (?1, ?2)",
                bucket.vectors_table()
            ),
            params![fact_id, blob],
        )?;
        Ok(())
    }

    /// Facts in `bucket` that have no embedding yet — the boot-time backfill
    /// worklist (facts saved before the embedder was installed, or whose
    /// embed-on-save failed).
    pub fn facts_missing_embedding(
        &self,
        bucket: MemoryBucket,
        limit: usize,
    ) -> Result<Vec<MemoryFact>> {
        let sql = format!(
            "SELECT f.id, f.content, f.origin_profile, f.tags, f.created_at, f.pinned
             FROM {tbl} f
             LEFT JOIN {vec} v ON v.fact_id = f.id
             WHERE v.id IS NULL
             ORDER BY f.created_at DESC LIMIT ?1",
            tbl = bucket.table(),
            vec = bucket.vectors_table(),
        );
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![limit as i64], row_to_memory_fact)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn insert_memory_vector(&self, v: &MemoryVector) -> Result<i64> {
        // Hoisted: `execute` + `last_insert_rowid` must run on the same
        // locked connection without another thread's statement landing in
        // between, or the rowid we read back could belong to someone else's
        // insert.
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO memory_vectors (fact_id, embedding) VALUES (?1, ?2)",
            params![v.fact_id, v.embedding],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn list_vectors_for_fact(&self, fact_id: &str) -> Result<Vec<MemoryVector>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT id, fact_id, embedding FROM memory_vectors WHERE fact_id = ?1")?;
        let rows = stmt
            .query_map(params![fact_id], |r| {
                Ok(MemoryVector {
                    id: r.get(0)?,
                    fact_id: r.get(1)?,
                    embedding: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ── skills ─────────────────────────────────────────────────────────────

    pub fn insert_skill(&self, s: &Skill) -> Result<()> {
        let caps = serde_json::to_string(&s.capabilities_required).unwrap_or_else(|_| "[]".into());
        self.conn.lock().execute(
            "INSERT INTO skills
             (id, name, description, content, capabilities_required, approval_status, path, version, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                s.id,
                s.name,
                s.description,
                s.content,
                caps,
                s.approval_status.as_str(),
                s.path,
                s.version,
                s.created_at
            ],
        )?;
        Ok(())
    }

    pub fn get_skill(&self, id: &str) -> Result<Option<Skill>> {
        Ok(self
            .conn
            .lock()
            .query_row(
                "SELECT id, name, description, content, capabilities_required, approval_status, path, version, created_at
                 FROM skills WHERE id = ?1",
                params![id],
                row_to_skill,
            )
            .optional()?)
    }

    pub fn list_skills(&self) -> Result<Vec<Skill>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, description, content, capabilities_required, approval_status, path, version, created_at
             FROM skills ORDER BY name",
        )?;
        let rows = stmt
            .query_map([], row_to_skill)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// APPROVED skills only — the set safe to offer/search (the trust boundary).
    pub fn list_approved_skills(&self) -> Result<Vec<Skill>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, description, content, capabilities_required, approval_status, path, version, created_at
             FROM skills WHERE approval_status = 'approved' ORDER BY name",
        )?;
        let rows = stmt
            .query_map([], row_to_skill)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Keyword search over APPROVED skills (name/description/content). `LIKE`
    /// with the wildcard-escaped query — good enough for the handful of skills a
    /// profile has; a meaning lane (sqlite-vec, like memory) is a later refinement.
    pub fn search_skills(&self, query: &str, limit: usize) -> Result<Vec<Skill>> {
        let esc = query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let pat = format!("%{esc}%");
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, description, content, capabilities_required, approval_status, path, version, created_at
             FROM skills
             WHERE approval_status = 'approved'
               AND (name LIKE ?1 ESCAPE '\\' OR description LIKE ?1 ESCAPE '\\' OR content LIKE ?1 ESCAPE '\\')
             ORDER BY name LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![pat, limit as i64], row_to_skill)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Move a skill to a trust state (the review gate). Returns whether a row moved.
    pub fn set_skill_approval(&self, id: &str, status: SkillApproval) -> Result<bool> {
        let n = self.conn.lock().execute(
            "UPDATE skills SET approval_status = ?1 WHERE id = ?2",
            params![status.as_str(), id],
        )?;
        Ok(n > 0)
    }

    pub fn delete_skill(&self, id: &str) -> Result<bool> {
        let n = self
            .conn
            .lock()
            .execute("DELETE FROM skills WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    // ── app_settings ────────────────────────────────────────────────────────

    pub fn set_app_setting(&self, key: &str, value: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.lock().execute(
            "INSERT INTO app_settings (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![key, value, now],
        )?;
        Ok(())
    }

    pub fn get_app_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .lock()
            .query_row(
                "SELECT value FROM app_settings WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The app_settings key for the Wave 4.2 autonomous-drafting toggle. Global
    /// (not per-profile) because skills themselves are global.
    const SKILL_REFLECT_KEY: &'static str = "skill_reflect_enabled";

    /// Is autonomous skill drafting (Wave 4.2) enabled? Defaults to `false`
    /// (approve-first / no auto-drafting) — the safe, opt-in default. A stored
    /// value only counts as `true` for exactly "1".
    pub fn skill_reflect_enabled(&self) -> bool {
        self.get_app_setting(Self::SKILL_REFLECT_KEY)
            .ok()
            .flatten()
            .map(|v| v == "1")
            .unwrap_or(false)
    }

    /// Turn autonomous skill drafting on/off.
    pub fn set_skill_reflect_enabled(&self, enabled: bool) -> Result<()> {
        self.set_app_setting(Self::SKILL_REFLECT_KEY, if enabled { "1" } else { "0" })
    }

    pub fn list_app_settings(&self) -> Result<Vec<AppSetting>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT key, value, updated_at FROM app_settings ORDER BY key")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(AppSetting {
                    key: r.get(0)?,
                    value: r.get(1)?,
                    updated_at: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

fn row_to_endpoint(r: &rusqlite::Row<'_>) -> rusqlite::Result<Endpoint> {
    Ok(Endpoint {
        id: r.get(0)?,
        name: r.get(1)?,
        base_url: r.get(2)?,
        api_key_encrypted: r.get(3)?,
        kind: r.get(4)?,
        created_at: r.get(5)?,
        supports_native_tools: r.get::<_, i64>(6)? != 0,
    })
}
