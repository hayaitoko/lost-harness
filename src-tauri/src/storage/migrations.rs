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
    Migration {
        version: 5,
        // Wave 4.1 — flesh out the `skills` stub into a real skill row: a
        // description + capability requirements (which bodies can be offered),
        // an approval_status trust boundary (only 'approved' is searchable/
        // loadable), a resource path (Tier-3 progressive disclosure), a version,
        // and an embedding slot (meaning-lane search, later). ADD COLUMN has no
        // IF-NOT-EXISTS, so these live ONLY here (not in GLOBAL_SCHEMA_SQL's
        // skills CREATE): a fresh DB creates the 4-column stub then v5 widens it,
        // matching an existing v4 DB's upgrade. Default approval 'pending' fails
        // CLOSED — a pre-existing skill is not auto-trusted until reviewed.
        name: "skills_metadata",
        sql: "ALTER TABLE skills ADD COLUMN description TEXT NOT NULL DEFAULT '';
              ALTER TABLE skills ADD COLUMN capabilities_required TEXT NOT NULL DEFAULT '[]';
              ALTER TABLE skills ADD COLUMN approval_status TEXT NOT NULL DEFAULT 'pending';
              ALTER TABLE skills ADD COLUMN path TEXT NOT NULL DEFAULT '';
              ALTER TABLE skills ADD COLUMN version TEXT NOT NULL DEFAULT '0.1.0';
              ALTER TABLE skills ADD COLUMN embedding BLOB;",
    },
    Migration {
        version: 6,
        // Wave 4.3 — declarative agent-type personas (a code reviewer, a research
        // explorer): a bounded `tools_allowlist` (intersected with the running
        // body's tools at dispatch, never widening), a Wave-3.1 `seat` name
        // resolved per-profile, an `approval_status` trust gate mirroring skills,
        // and a `source` marking 'builtin' seeds vs 'user'/pack types. New table,
        // so dual-defined: the CREATE is IF NOT EXISTS and ALSO lives in
        // GLOBAL_SCHEMA_SQL, so v6 is a no-op on a fresh install and a real
        // upgrade on a v5 DB.
        name: "agent_types_table",
        sql: "CREATE TABLE IF NOT EXISTS agent_types (
            id                TEXT PRIMARY KEY,
            name              TEXT NOT NULL,
            description       TEXT NOT NULL DEFAULT '',
            system_prompt     TEXT NOT NULL DEFAULT '',
            tools_allowlist   TEXT NOT NULL DEFAULT '[]',
            seat              TEXT NOT NULL DEFAULT 'inherit',
            trigger_examples  TEXT NOT NULL DEFAULT '[]',
            approval_status   TEXT NOT NULL DEFAULT 'pending',
            source            TEXT NOT NULL DEFAULT 'user',
            created_at        INTEGER NOT NULL
        );",
    },
    Migration {
        version: 7,
        // Wave 5.3 / M8 — the verified-before-runnable invariant needs two more
        // model_catalog columns: `sha256` (the trust anchor — a row only exists
        // after the bytes hashed to the catalog value) and `status` (ready vs
        // quarantined — an integrity re-check that fails at boot never silently
        // serves the model). ADD COLUMN has no IF-NOT-EXISTS, so these live ONLY
        // here (not in GLOBAL_SCHEMA_SQL's model_catalog CREATE): a fresh DB
        // creates the 6-column table then v7 widens it, matching a v6 DB's upgrade.
        name: "model_catalog_integrity",
        sql: "ALTER TABLE model_catalog ADD COLUMN sha256 TEXT NOT NULL DEFAULT '';
              ALTER TABLE model_catalog ADD COLUMN status TEXT NOT NULL DEFAULT 'ready';",
    },
    Migration {
        version: 8,
        // C3: the persisted MCP server-config store. Same dual-definition
        // convention (also in GLOBAL_SCHEMA_SQL) — a no-op on a fresh install,
        // a real upgrade on a v7 DB.
        name: "mcp_servers_table",
        sql: "CREATE TABLE IF NOT EXISTS mcp_servers (
            id                TEXT PRIMARY KEY,
            name              TEXT NOT NULL,
            command           TEXT NOT NULL,
            args              TEXT NOT NULL DEFAULT '[]',
            tier              TEXT NOT NULL DEFAULT 'remote',
            trusted_read_only INTEGER NOT NULL DEFAULT 0,
            capabilities      TEXT NOT NULL DEFAULT '[]',
            enabled           INTEGER NOT NULL DEFAULT 1,
            created_at        INTEGER NOT NULL
        );",
    },
    Migration {
        version: 9,
        // H-07: pin the approved MCP server invocation. Registration records the
        // canonical resolved path plus a digest over the executable's contents,
        // the argv vector, and any absolute script file argv names; every later
        // bring-up (including the unattended auto-start at boot) re-measures and
        // refuses to spawn on a mismatch, so neither a swapped binary NOR
        // swapped args can ride the old consent. `executable_hash` is therefore
        // an invocation pin, not a bare file hash — see
        // `tools::mcp_stdio::invocation_pin_digest`.
        //
        // Rows written before v9 get NULL in both columns. That is NOT treated
        // as "trusted": `verify_pinned_executable` fails closed on a missing
        // pin and asks the user to re-register — we never measured that binary,
        // so we cannot attest to it.
        //
        // These columns live ONLY here, not in GLOBAL_SCHEMA_SQL's mcp_servers
        // CREATE, because `ALTER TABLE ADD COLUMN` has no IF-NOT-EXISTS: a fresh
        // DB runs v1 (CREATE without them) then v9 (ALTER adds them), matching
        // an existing DB's upgrade path exactly. Same convention as v2's
        // `memory_facts.pinned`.
        name: "mcp_servers_executable_pinning",
        sql: "ALTER TABLE mcp_servers ADD COLUMN executable_path TEXT;
              ALTER TABLE mcp_servers ADD COLUMN executable_hash TEXT;",
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
    Migration {
        version: 9,
        // Per-profile model-seat bindings (Wave 3.1). A seat is a USER-DEFINED
        // name (any string — not an enumerated set) that resolves to a concrete
        // (provider, model) at run time; kept PER-PROFILE so a profile can point
        // a seat at a different (e.g. forced-local) model than another profile.
        // An unbound seat resolves to the caller's own model (`inherit`), so a
        // missing row is normal. Same dual-definition convention: the CREATE is
        // IF NOT EXISTS and also lives in PROFILE_SCHEMA_SQL, so v9 is a no-op on
        // a fresh install and a real upgrade on a v8 DB. No FK to the (global)
        // endpoints table — a binding may outlive a deleted provider, and
        // `resolve_seat` detects the dangling id at run time and falls back.
        name: "seat_bindings_table",
        sql: "CREATE TABLE IF NOT EXISTS seat_bindings (
            seat         TEXT PRIMARY KEY,
            provider_id  TEXT NOT NULL,
            model        TEXT NOT NULL,
            updated_at   INTEGER NOT NULL
        );",
    },
    Migration {
        version: 10,
        // Per-profile OS-sandbox config (M7 Tier-K, Slice 2). One row (id=1)
        // serializing `SandboxConfig` as JSON. Same dual-definition convention:
        // the CREATE is IF NOT EXISTS and also lives in PROFILE_SCHEMA_SQL, so v10
        // is a no-op on a fresh install and a real upgrade on a v9 DB. A missing
        // row = the legacy unconstrained default; the row is consumed as a
        // per-profile CEILING (network) on shell_exec — the Seatbelt confinement
        // itself is always-on regardless, so this can never yield an unsandboxed run.
        name: "sandbox_config_table",
        sql: "CREATE TABLE IF NOT EXISTS sandbox_config (
            id          INTEGER PRIMARY KEY CHECK (id = 1),
            config_json TEXT NOT NULL,
            updated_at  INTEGER NOT NULL
        );",
    },
    Migration {
        version: 11,
        // C1: per-profile budget cap (the spend governor). One row (id=1); a
        // NULL `cap_usd` (or no row) = uncapped, the safe default. The governor
        // reads this against the usage ledger to WARN attended chat / HALT
        // unattended work. Same dual-definition convention (also in
        // PROFILE_SCHEMA_SQL).
        name: "budget_settings_table",
        sql: "CREATE TABLE IF NOT EXISTS budget_settings (
            id         INTEGER PRIMARY KEY CHECK (id = 1),
            cap_usd    REAL,
            updated_at INTEGER NOT NULL
        );",
    },
    Migration {
        version: 12,
        // The per-turn TRUST ZONE, stamped when the turn ran: 'local' | 'cloud'
        // (models::TrustZone). Persisted history, like `provider_id` beside it.
        //
        // Without it, the route badge had to re-derive the zone from the LIVE
        // provider registry at render time — so a turn genuinely served by a
        // public cloud endpoint rendered as a green "Local" badge once that
        // provider was removed or its kind edited. The zone of a past turn is a
        // fact about the past; it does not get to change.
        //
        // NULL on every row written before this migration, and the UI must
        // render that as UNKNOWN. Backfilling it from today's registry would
        // manufacture exactly the lie this column removes.
        //
        // Like v5 (and global v2's `pinned`), ADD COLUMN has no IF-NOT-EXISTS,
        // so this lives ONLY here and NOT in PROFILE_SCHEMA_SQL: a fresh DB
        // runs v1 (column absent) then v12 adds it, matching the upgrade path.
        name: "messages_endpoint_zone",
        sql: "ALTER TABLE messages ADD COLUMN endpoint_zone TEXT;",
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
    fn upgrading_an_existing_profile_db_adds_the_trust_zone_column_without_backfilling() {
        // v12 is an ALTER TABLE, so it must work on a DB that already holds
        // messages — and it must leave those pre-existing rows with a NULL
        // zone. Backfilling them from today's provider registry would
        // manufacture exactly the lie the column exists to remove: a turn
        // served months ago by a cloud endpoint would be re-labelled from
        // whatever that provider looks like now.
        let conn = Connection::open_in_memory().unwrap();
        let v11 = &PROFILE_MIGRATIONS[..11];
        assert_eq!(v11.last().unwrap().version, 11, "slice must stop at v11");
        run_migrations(&conn, v11, "profile-v11").unwrap();

        conn.execute(
            "INSERT INTO conversations (id, name, pinned, binding, created_at, updated_at)
             VALUES ('c1', 'old chat', 0, 'auto', 0, 0)",
            [],
        )
        .unwrap();
        // Written by the pre-v12 code path: no zone column existed at all.
        conn.execute(
            "INSERT INTO messages
             (id, conversation_id, role, content, model, provider_id,
              routing_decision, thinking_content, error, aborted, created_at)
             VALUES ('m1', 'c1', 'assistant', 'hi', 'gpt-x', 'cloudco',
                     'allow', NULL, NULL, 0, 0)",
            [],
        )
        .unwrap();

        migrate_profile(&conn).unwrap();

        let v: i32 = conn
            .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, PROFILE_SCHEMA_VERSION);

        let zone: Option<String> = conn
            .query_row(
                "SELECT endpoint_zone FROM messages WHERE id = 'm1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            zone, None,
            "a pre-v12 row must read back as UNKNOWN — never backfilled, and \
             never defaulted to 'local'"
        );
        // The row it belongs to is otherwise untouched.
        let provider: String = conn
            .query_row(
                "SELECT provider_id FROM messages WHERE id = 'm1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(provider, "cloudco");
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
