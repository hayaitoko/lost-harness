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
        let conn = Connection::open(path)
            .with_context(|| format!("opening global.db at {}", path.display()))?;
        migrate_global(&conn)?;
        Ok(Self { conn })
    }

    /// In-memory variant for tests.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
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

    pub fn insert_memory_fact(&self, fact: &MemoryFact) -> Result<()> {
        self.conn.execute(
            "INSERT INTO memory_facts (id, content, origin_profile, tags, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                fact.id,
                fact.content,
                fact.origin_profile,
                fact.tags,
                fact.created_at
            ],
        )?;
        Ok(())
    }

    pub fn get_memory_fact(&self, id: &str) -> Result<Option<MemoryFact>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, content, origin_profile, tags, created_at
                 FROM memory_facts WHERE id = ?1",
                params![id],
                |r| {
                    Ok(MemoryFact {
                        id: r.get(0)?,
                        content: r.get(1)?,
                        origin_profile: r.get(2)?,
                        tags: r.get(3)?,
                        created_at: r.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn list_memory_facts(&self) -> Result<Vec<MemoryFact>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content, origin_profile, tags, created_at
             FROM memory_facts ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(MemoryFact {
                    id: r.get(0)?,
                    content: r.get(1)?,
                    origin_profile: r.get(2)?,
                    tags: r.get(3)?,
                    created_at: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn list_memory_facts_by_profile(&self, profile: &str) -> Result<Vec<MemoryFact>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content, origin_profile, tags, created_at
             FROM memory_facts WHERE origin_profile = ?1 ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map(params![profile], |r| {
                Ok(MemoryFact {
                    id: r.get(0)?,
                    content: r.get(1)?,
                    origin_profile: r.get(2)?,
                    tags: r.get(3)?,
                    created_at: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn delete_memory_fact(&self, id: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM memory_facts WHERE id = ?1", params![id])?;
        Ok(n > 0)
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
