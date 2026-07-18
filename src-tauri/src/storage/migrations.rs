//! Migration system for the Lost Harness storage layer.
//!
//! Each migration is a numbered SQL blob. On `open`, we read the current
//! `schema_version` row, then apply every migration > current in order.
//! A migration is applied in a single transaction so a failure rolls back
//! cleanly and the DB stays at its previous version.
//!
//! Schema source of truth: spec §1 (Profile Data Model) + §5 (Storage Schema).

use anyhow::{Context, Result};
use rusqlite::Connection;

use super::schema::{
    GLOBAL_SCHEMA_SQL, GLOBAL_SCHEMA_VERSION, PROFILE_SCHEMA_SQL, PROFILE_SCHEMA_VERSION,
};

/// A single named migration. Migrations are append-only: never edit a
/// shipped migration, add a new one instead.
pub struct Migration {
    pub version: i32,
    pub name: &'static str,
    /// SQL blob (may contain multiple statements, terminated by `;`).
    pub sql: &'static str,
}

/// All global.db migrations, in order.
pub const GLOBAL_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial_global_schema",
        sql: GLOBAL_SCHEMA_SQL,
    },
    Migration {
        version: 2,
        // Memory system foundation (PLAN §9): sensitivity buckets + FTS5
        // keyword search + curated-summary pinning.
        //
        // The `private-local` bucket lives in a PHYSICALLY SEPARATE table
        // (`memory_facts_private`), not a filtered view of `memory_facts` —
        // so a cloud-bound context assembly never even queries it, and a bug
        // can't leak it (PLAN §9, same physical-separation principle as the
        // §7 profile wall). `pinned` backs the always-loaded curated summary.
        // FTS5 external-content indexes give the keyword lane of hybrid search;
        // the meaning lane (sqlite-vec) layers on once an embedder is chosen.
        //
        // `pinned` is added ONLY here (not in GLOBAL_SCHEMA_SQL) because
        // `ALTER TABLE ADD COLUMN` has no IF-NOT-EXISTS: a fresh DB runs v1
        // (memory_facts without pinned) then v2 (adds it), matching an
        // existing DB's upgrade path exactly.
        name: "memory_buckets_fts_and_pinning",
        sql: "
        ALTER TABLE memory_facts ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;

        CREATE TABLE IF NOT EXISTS memory_facts_private (
            id              TEXT PRIMARY KEY,
            content         TEXT NOT NULL,
            origin_profile  TEXT NOT NULL,
            tags            TEXT,
            created_at      INTEGER NOT NULL,
            pinned          INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_memory_facts_private_origin
            ON memory_facts_private(origin_profile);

        CREATE VIRTUAL TABLE IF NOT EXISTS memory_facts_fts USING fts5(
            content, content='memory_facts', content_rowid='rowid'
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS memory_facts_private_fts USING fts5(
            content, content='memory_facts_private', content_rowid='rowid'
        );

        -- Backfill the index for any rows that predate it.
        INSERT INTO memory_facts_fts(rowid, content)
            SELECT rowid, content FROM memory_facts;

        CREATE TRIGGER IF NOT EXISTS memory_facts_ai AFTER INSERT ON memory_facts BEGIN
            INSERT INTO memory_facts_fts(rowid, content) VALUES (new.rowid, new.content);
        END;
        CREATE TRIGGER IF NOT EXISTS memory_facts_ad AFTER DELETE ON memory_facts BEGIN
            INSERT INTO memory_facts_fts(memory_facts_fts, rowid, content)
                VALUES('delete', old.rowid, old.content);
        END;
        CREATE TRIGGER IF NOT EXISTS memory_facts_au AFTER UPDATE ON memory_facts BEGIN
            INSERT INTO memory_facts_fts(memory_facts_fts, rowid, content)
                VALUES('delete', old.rowid, old.content);
            INSERT INTO memory_facts_fts(rowid, content) VALUES (new.rowid, new.content);
        END;

        CREATE TRIGGER IF NOT EXISTS memory_facts_private_ai
            AFTER INSERT ON memory_facts_private BEGIN
            INSERT INTO memory_facts_private_fts(rowid, content)
                VALUES (new.rowid, new.content);
        END;
        CREATE TRIGGER IF NOT EXISTS memory_facts_private_ad
            AFTER DELETE ON memory_facts_private BEGIN
            INSERT INTO memory_facts_private_fts(memory_facts_private_fts, rowid, content)
                VALUES('delete', old.rowid, old.content);
        END;
        CREATE TRIGGER IF NOT EXISTS memory_facts_private_au
            AFTER UPDATE ON memory_facts_private BEGIN
            INSERT INTO memory_facts_private_fts(memory_facts_private_fts, rowid, content)
                VALUES('delete', old.rowid, old.content);
            INSERT INTO memory_facts_private_fts(rowid, content)
                VALUES (new.rowid, new.content);
        END;",
    },
    Migration {
        version: 3,
        // Meaning lane of hybrid memory search (PLAN §9): embedding vectors
        // for the PRIVATE-LOCAL bucket live in their own PHYSICALLY SEPARATE
        // table, mirroring the memory_facts / memory_facts_private split — a
        // cloud-bound semantic search never even queries the private vector
        // index, so the wall holds structurally (same principle as v2).
        // The shared bucket keeps the original `memory_vectors` (v1) table.
        // ON DELETE CASCADE works because run_migrations turns
        // PRAGMA foreign_keys ON for every connection.
        name: "memory_vectors_private",
        sql: "
        CREATE TABLE IF NOT EXISTS memory_vectors_private (
            id          INTEGER PRIMARY KEY,
            fact_id     TEXT NOT NULL,
            embedding   BLOB NOT NULL,
            FOREIGN KEY(fact_id) REFERENCES memory_facts_private(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_memory_vectors_private_fact
            ON memory_vectors_private(fact_id);",
    },
    Migration {
        version: 4,
        // Q1 native tool-use: per-endpoint capability flag. An endpoint whose
        // API supports OpenAI-style structured tool calls gets the native
        // transport; everything else keeps the fenced dialect.
        name: "endpoints_native_tools_flag",
        sql: "ALTER TABLE endpoints ADD COLUMN supports_native_tools INTEGER NOT NULL DEFAULT 0;",
    },
];

/// All per-profile DB migrations, in order.
pub const PROFILE_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial_profile_schema",
        sql: PROFILE_SCHEMA_SQL,
    },
    Migration {
        version: 2,
        // Append-only per-dispatch audit trail (item 5, Fable Q9). Lives
        // in the per-profile DB (same isolation logic as the rest of
        // profile data); written from `tools::dispatch` AFTER the outcome
        // exists, so it can never gate a call. The CREATE is
        // IF NOT EXISTS because fresh DBs running the v1 migration in
        // the same pass also get the table from PROFILE_SCHEMA_SQL
        // (item 5 ships the table definition in both places so v2 is
        // a no-op on a fresh install and a real upgrade on an existing
        // v1 DB).
        name: "tool_audit_table",
        sql: "CREATE TABLE IF NOT EXISTS tool_audit (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            ts              INTEGER NOT NULL,
            conversation_id TEXT NOT NULL,
            tool_name       TEXT NOT NULL,
            canonical_args  TEXT NOT NULL,
            fingerprint     TEXT NOT NULL,
            risk            TEXT NOT NULL,
            outcome         TEXT NOT NULL,
            gate_by         TEXT,
            grant_used      TEXT,
            decision        TEXT,
            endpoint_kind   TEXT,
            duration_ms     INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_tool_audit_conversation ON tool_audit(conversation_id);
        CREATE INDEX IF NOT EXISTS idx_tool_audit_created ON tool_audit(ts);",
    },
    Migration {
        version: 3,
        // Persisted Always grants (Q8). Per-profile, live-read on the gating
        // path. Same dual-definition convention as v2: the CREATE is
        // IF NOT EXISTS and also lives in PROFILE_SCHEMA_SQL, so v3 is a no-op
        // on a fresh install (v1 already created it) and a real upgrade on an
        // existing v2 DB.
        name: "tool_rules_table",
        sql: "CREATE TABLE IF NOT EXISTS tool_rules (
            id          TEXT PRIMARY KEY,
            tool_name   TEXT NOT NULL,
            pattern     TEXT NOT NULL,
            action      TEXT NOT NULL,
            created_at  INTEGER NOT NULL,
            UNIQUE(tool_name, pattern)
        );
        CREATE INDEX IF NOT EXISTS idx_tool_rules_tool ON tool_rules(tool_name);",
    },
    Migration {
        version: 4,
        // Per-profile classifier thresholds (PLAN §11 settings page). Single
        // row (id=1); absence means "use defaults". Same dual-definition
        // convention as v2/v3: the CREATE is IF NOT EXISTS and also lives in
        // PROFILE_SCHEMA_SQL, so v4 is a no-op on a fresh install (v1 already
        // created it) and a real upgrade on an existing v3 DB.
        name: "classifier_settings_table",
        sql: "CREATE TABLE IF NOT EXISTS classifier_settings (
            id          INTEGER PRIMARY KEY CHECK (id = 1),
            tau_block   REAL NOT NULL,
            tau_band    REAL NOT NULL,
            updated_at  INTEGER NOT NULL
        );",
    },
    Migration {
        version: 5,
        // Per-profile redaction toggle (PLAN §11 partial delegation). Added as a
        // column on the existing classifier_settings row. Like the `pinned`
        // column in global v2, ADD COLUMN has no IF-NOT-EXISTS, so this lives
        // ONLY here (not in PROFILE_SCHEMA_SQL): a fresh DB runs v1..v4 (row
        // without the column) then v5 adds it, matching an existing DB's upgrade.
        // Default 1 (on) — redaction is the privacy-preserving default.
        name: "classifier_redaction_toggle",
        sql: "ALTER TABLE classifier_settings
              ADD COLUMN redaction_enabled INTEGER NOT NULL DEFAULT 1;",
    },
    Migration {
        version: 6,
        // Per-profile memory toggles (Wave 1). Single row (id=1): a missing row
        // means defaults (semantic search ON, not walled). `semantic_search_enabled`
        // is the meaning-lane off switch (PLAN §9); `walled` is the §7 memory
        // island. Same dual-definition convention as v3/v4: the CREATE is
        // IF NOT EXISTS and also lives in PROFILE_SCHEMA_SQL, so v6 is a no-op on
        // a fresh install (v1 already created it) and a real upgrade on a v5 DB.
        name: "memory_settings_table",
        sql: "CREATE TABLE IF NOT EXISTS memory_settings (
            id                      INTEGER PRIMARY KEY CHECK (id = 1),
            semantic_search_enabled INTEGER NOT NULL DEFAULT 1,
            walled                  INTEGER NOT NULL DEFAULT 0,
            updated_at              INTEGER NOT NULL
        );",
    },
    Migration {
        version: 7,
        // Per-profile model-call cost ledger (Wave 3.2, PLAN §3 usage ledger).
        // One row per model call; `cost_usd` NULL means UNKNOWN ("flying
        // blind") — never a silent guess — and 0.0 for a local/on-device call.
        // Same dual-definition convention as v2/v3/v4/v6: the CREATE is
        // IF NOT EXISTS and also lives in PROFILE_SCHEMA_SQL, so v7 is a no-op on
        // a fresh install (v1 already created it) and a real upgrade on a v6 DB.
        name: "usage_events_table",
        sql: "CREATE TABLE IF NOT EXISTS usage_events (
            id                TEXT PRIMARY KEY,
            conversation_id   TEXT,
            model             TEXT NOT NULL,
            provider_id       TEXT,
            provider_kind     TEXT NOT NULL,
            cost_usd          REAL,
            created_at        INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_usage_events_created ON usage_events(created_at);",
    },
    Migration {
        version: 8,
        // The one-queue-model substrate (Wave 4.4): deferred work — cron fires,
        // agent dispatch, server results — as one persisted lifecycle (also the
        // 2.5 durability journal via idempotency_key). Same dual-definition
        // convention: the CREATE is IF NOT EXISTS and also lives in
        // PROFILE_SCHEMA_SQL, so v8 is a no-op on a fresh install and a real
        // upgrade on a v7 DB.
        name: "work_items_table",
        sql: "CREATE TABLE IF NOT EXISTS work_items (
            id                      TEXT PRIMARY KEY,
            kind                    TEXT NOT NULL,
            state                   TEXT NOT NULL,
            source_ref              TEXT,
            input_json              TEXT NOT NULL,
            result_json             TEXT,
            error                   TEXT,
            scheduled_at            INTEGER,
            claim_key               TEXT,
            idempotency_key         TEXT,
            attempts                INTEGER NOT NULL DEFAULT 0,
            target_conversation_id  TEXT,
            created_at              INTEGER NOT NULL,
            started_at              INTEGER,
            finished_at             INTEGER
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_work_items_claim ON work_items(claim_key) WHERE claim_key IS NOT NULL;
        CREATE INDEX IF NOT EXISTS idx_work_items_state_sched ON work_items(state, scheduled_at);",
    },
];

/// Apply all pending global migrations to a freshly opened connection.
///
/// Safe to call on a brand-new DB (creates `schema_version` and the initial
/// migration row) and on an existing DB (skips already-applied versions).
pub fn migrate_global(conn: &Connection) -> Result<()> {
    run_migrations(conn, GLOBAL_MIGRATIONS, "global")?;
    // Sanity: the latest migration must equal the global schema version.
    debug_assert_eq!(
        GLOBAL_MIGRATIONS.last().map(|m| m.version).unwrap_or(0),
        GLOBAL_SCHEMA_VERSION,
        "GLOBAL_MIGRATIONS must end at GLOBAL_SCHEMA_VERSION"
    );
    Ok(())
}

/// Apply all pending per-profile migrations to a freshly opened connection.
pub fn migrate_profile(conn: &Connection) -> Result<()> {
    run_migrations(conn, PROFILE_MIGRATIONS, "profile")?;
    // Sanity: the latest migration must equal the profile schema version.
    debug_assert_eq!(
        PROFILE_MIGRATIONS.last().map(|m| m.version).unwrap_or(0),
        PROFILE_SCHEMA_VERSION,
        "PROFILE_MIGRATIONS must end at PROFILE_SCHEMA_VERSION"
    );
    Ok(())
}

fn run_migrations(conn: &Connection, migrations: &[Migration], label: &str) -> Result<()> {
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .with_context(|| format!("[{label}] enabling foreign_keys"))?;

    // Bootstrap schema_version on a fresh DB so the SELECT below works.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version     INTEGER PRIMARY KEY,
            applied_at  INTEGER NOT NULL
        );",
    )
    .with_context(|| format!("[{label}] creating schema_version table"))?;

    let current: i32 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    for migration in migrations {
        if migration.version <= current {
            continue;
        }
        // Every migration runs in its own transaction so a failure in v3
        // doesn't poison the v1 + v2 state.
        let tx = conn.unchecked_transaction().with_context(|| {
            format!("[{label}] starting transaction for v{}", migration.version)
        })?;
        tx.execute_batch(migration.sql).with_context(|| {
            format!(
                "[{label}] applying migration v{} ({})",
                migration.version, migration.name
            )
        })?;
        let now = chrono::Utc::now().timestamp();
        tx.execute(
            "INSERT OR REPLACE INTO schema_version (version, applied_at) VALUES (?1, ?2)",
            rusqlite::params![migration.version, now],
        )
        .with_context(|| format!("[{label}] recording schema_version v{}", migration.version))?;
        tx.commit()
            .with_context(|| format!("[{label}] committing v{}", migration.version))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn fresh_global_db_lands_at_schema_version() {
        let conn = Connection::open_in_memory().unwrap();
        migrate_global(&conn).unwrap();
        let v: i32 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, GLOBAL_SCHEMA_VERSION);
    }

    #[test]
    fn fresh_profile_db_lands_at_schema_version() {
        let conn = Connection::open_in_memory().unwrap();
        migrate_profile(&conn).unwrap();
        let v: i32 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, PROFILE_SCHEMA_VERSION);
    }

    #[test]
    fn reapplying_migrations_is_a_noop() {
        let conn = Connection::open_in_memory().unwrap();
        migrate_global(&conn).unwrap();
        migrate_global(&conn).unwrap();
        let v: i32 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, GLOBAL_SCHEMA_VERSION);
    }
}
