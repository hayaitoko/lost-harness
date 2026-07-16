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
}

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
    let tokens: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"", t.to_lowercase()))
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" OR "))
    }
}

#[derive(Debug, Clone)]
pub struct MemoryVector {
    pub id: i64,
    pub fact_id: String,
    pub embedding: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub content: String,
    pub created_at: i64,
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
    conn: Connection,
}

impl GlobalDb {
    /// Open an existing global.db (or create + migrate a fresh one) at `path`.
    pub fn open(path: &std::path::Path) -> Result<Self> {
        crate::storage::ensure_sqlite_vec_registered();
        let conn = Connection::open(path)
            .with_context(|| format!("opening global.db at {}", path.display()))?;
        migrate_global(&conn)?;
        Ok(Self { conn })
    }

    /// In-memory variant for tests.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        crate::storage::ensure_sqlite_vec_registered();
        let conn = Connection::open_in_memory()?;
        migrate_global(&conn)?;
        Ok(Self { conn })
    }

    /// Borrow the underlying connection. Use sparingly — most callers should
    /// go through the typed methods.
    pub fn raw(&self) -> &Connection {
        &self.conn
    }

    // ── user_facts ──────────────────────────────────────────────────────────

    pub fn set_user_fact(&self, key: &str, value: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
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
        let mut stmt = self
            .conn
            .prepare("SELECT key, value, updated_at FROM user_facts ORDER BY key")?;
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
            .execute("DELETE FROM user_facts WHERE key = ?1", params![key])?;
        Ok(n > 0)
    }

    // ── endpoints ───────────────────────────────────────────────────────────

    pub fn insert_endpoint(&self, ep: &Endpoint) -> Result<()> {
        self.conn.execute(
            "INSERT INTO endpoints (id, name, base_url, api_key_encrypted, kind, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                ep.id,
                ep.name,
                ep.base_url,
                ep.api_key_encrypted,
                ep.kind,
                ep.created_at
            ],
        )?;
        Ok(())
    }

    pub fn get_endpoint(&self, id: &str) -> Result<Option<Endpoint>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, name, base_url, api_key_encrypted, kind, created_at
                 FROM endpoints WHERE id = ?1",
                params![id],
                row_to_endpoint,
            )
            .optional()?)
    }

    pub fn list_endpoints(&self) -> Result<Vec<Endpoint>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, base_url, api_key_encrypted, kind, created_at
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
        let n = self.conn.execute(
            "UPDATE endpoints SET name = ?1, base_url = ?2, api_key_encrypted = ?3, kind = ?4
             WHERE id = ?5",
            params![name, base_url, api_key_encrypted, kind, id],
        )?;
        Ok(n > 0)
    }

    pub fn delete_endpoint(&self, id: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM endpoints WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    // ── model_catalog ───────────────────────────────────────────────────────

    pub fn insert_model(&self, m: &ModelEntry) -> Result<()> {
        self.conn.execute(
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
        let mut stmt = self.conn.prepare(
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
        self.conn.execute(
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
            .query_row(
                "SELECT id, content, origin_profile, tags, created_at, pinned
                 FROM memory_facts WHERE id = ?1",
                params![id],
                row_to_memory_fact,
            )
            .optional()?)
    }

    pub fn list_memory_facts(&self) -> Result<Vec<MemoryFact>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content, origin_profile, tags, created_at, pinned
             FROM memory_facts ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map([], row_to_memory_fact)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn list_memory_facts_by_profile(&self, profile: &str) -> Result<Vec<MemoryFact>> {
        let mut stmt = self.conn.prepare(
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
        let n = self
            .conn
            .execute("DELETE FROM memory_facts WHERE id = ?1", params![id])?
            + self.conn.execute(
                "DELETE FROM memory_facts_private WHERE id = ?1",
                params![id],
            )?;
        Ok(n > 0)
    }

    /// Pin/unpin a fact into the always-loaded curated summary. Checks both
    /// buckets; returns true if a row changed.
    pub fn set_memory_pinned(&self, id: &str, pinned: bool) -> Result<bool> {
        let p = pinned as i64;
        let n = self.conn.execute(
            "UPDATE memory_facts SET pinned = ?2 WHERE id = ?1",
            params![id, p],
        )? + self.conn.execute(
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

    fn summary_from(
        &self,
        bucket: MemoryBucket,
        profile: &str,
        limit: usize,
    ) -> Result<Vec<MemoryFact>> {
        let mut stmt = self.conn.prepare(&format!(
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
        let Some(match_expr) = fts_match_expr(query) else {
            return Ok(Vec::new());
        };
        let mut hits = self.search_bucket(MemoryBucket::Shared, &match_expr, limit)?;
        if allow_private {
            hits.extend(self.search_bucket(MemoryBucket::PrivateLocal, &match_expr, limit)?);
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
        limit: usize,
    ) -> Result<Vec<MemorySearchHit>> {
        // Reference the FTS table by its real name (not an alias) — FTS5's
        // bm25() aux function requires it.
        let sql = format!(
            "SELECT f.id, f.content, f.origin_profile, f.tags, f.created_at, f.pinned,
                    bm25({fts}) AS score
             FROM {fts}
             JOIN {tbl} f ON f.rowid = {fts}.rowid
             WHERE {fts} MATCH ?1
             ORDER BY score LIMIT ?2",
            fts = bucket.fts_table(),
            tbl = bucket.table(),
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![match_expr, limit as i64], |r| {
                Ok(MemorySearchHit {
                    fact: row_to_memory_fact(r)?,
                    bucket,
                    score: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ── memory_vectors ─────────────────────────────────────────────────────

    pub fn insert_memory_vector(&self, v: &MemoryVector) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO memory_vectors (fact_id, embedding) VALUES (?1, ?2)",
            params![v.fact_id, v.embedding],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn list_vectors_for_fact(&self, fact_id: &str) -> Result<Vec<MemoryVector>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, fact_id, embedding FROM memory_vectors WHERE fact_id = ?1")?;
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
        self.conn.execute(
            "INSERT INTO skills (id, name, content, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![s.id, s.name, s.content, s.created_at],
        )?;
        Ok(())
    }

    pub fn get_skill(&self, id: &str) -> Result<Option<Skill>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, name, content, created_at FROM skills WHERE id = ?1",
                params![id],
                |r| {
                    Ok(Skill {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        content: r.get(2)?,
                        created_at: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn list_skills(&self) -> Result<Vec<Skill>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, content, created_at FROM skills ORDER BY name")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Skill {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    content: r.get(2)?,
                    created_at: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn delete_skill(&self, id: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM skills WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    // ── app_settings ────────────────────────────────────────────────────────

    pub fn set_app_setting(&self, key: &str, value: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO app_settings (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![key, value, now],
        )?;
        Ok(())
    }

    pub fn get_app_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM app_settings WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn list_app_settings(&self) -> Result<Vec<AppSetting>> {
        let mut stmt = self
            .conn
            .prepare("SELECT key, value, updated_at FROM app_settings ORDER BY key")?;
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
    })
}
