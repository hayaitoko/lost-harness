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
pub const GLOBAL_MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "initial_global_schema",
    sql: GLOBAL_SCHEMA_SQL,
}];

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
