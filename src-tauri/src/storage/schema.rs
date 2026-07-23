//! SQL schema constants for the Lost Harness storage layer.
//!
//! Two databases:
//!   - `global.db` — shared across all profiles (user_facts, endpoints, etc.)
//!   - `profiles/<name>.db` — per-profile data (conversations, messages, etc.)
//!
//! Schema source of truth: spec §1 (Profile Data Model) + §5 (Storage Schema).
//! All tables use INTEGER for timestamps (Unix seconds, UTC) — chrono::Utc::now().timestamp().

/// Returns the current schema version for the GLOBAL database.
/// Bump when adding a new global migration.
pub const GLOBAL_SCHEMA_VERSION: i32 = 8;

/// Returns the current schema version for each PROFILE database.
/// Bump when adding a new per-profile migration. Profile and global
/// track versions independently: an item that only adds a per-profile
/// table (e.g. `tool_audit` in item 5, `tool_rules` in Q8,
/// `classifier_settings` in the classifier settings round,
/// `memory_settings` in Wave 1 memory) bumps `PROFILE_SCHEMA_VERSION`
/// without touching `GLOBAL_SCHEMA_VERSION`.
pub const PROFILE_SCHEMA_VERSION: i32 = 11;

// ─────────────────────────────────────────────────────────────────────────────
// Global tables (global.db)
// ─────────────────────────────────────────────────────────────────────────────

pub const GLOBAL_TABLES: &[&str] = &[
    // Schema bookkeeping
    "schema_version",
    // 1. user_facts — key/value facts about the user (name, preferences, hardware)
    //    See spec §1: Global facts — name, preferences, hardware profile, app config
    "user_facts",
    // 2. endpoints — API key + base URL definitions (global; any profile can use)
    //    See spec §1: Endpoint definitions — API keys, URLs (not bound to a profile)
    "endpoints",
    // 3. model_catalog — registry of downloaded local models (GGUF)
    //    See spec §1: Local models — global
    "model_catalog",
    // 4. memory_facts — structured memory facts, tagged with origin profile
    //    See spec §1: Memory — tagged with origin profile
    "memory_facts",
    // 5. memory_vectors — embeddings for memory_facts (sqlite-vec in M1+; raw BLOB for now)
    "memory_vectors",
    // 6. skills — saved skills, globally shared
    "skills",
    // 7. app_settings — theme, update channel, etc.
    "app_settings",
    // 8. agent_types — declarative agent-type personas (Wave 4.3), globally shared
    "agent_types",
    // 9. mcp_servers — persisted MCP server configs (C3), globally shared
    "mcp_servers",
];

/// CREATE TABLE statements for global.db (in dependency order).
pub const GLOBAL_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (
    version     INTEGER PRIMARY KEY,
    applied_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS user_facts (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS endpoints (
    id                  TEXT PRIMARY KEY,
    name                TEXT NOT NULL,
    base_url            TEXT NOT NULL,
    api_key_encrypted   BLOB,
    kind                TEXT NOT NULL,
    created_at          INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS model_catalog (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    path            TEXT NOT NULL,
    size_bytes      INTEGER NOT NULL,
    quantization    TEXT,
    added_at        INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS memory_facts (
    id              TEXT PRIMARY KEY,
    content         TEXT NOT NULL,
    origin_profile  TEXT NOT NULL,
    tags            TEXT,                       -- JSON array of tag strings
    created_at      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS memory_vectors (
    id          INTEGER PRIMARY KEY,
    fact_id     TEXT NOT NULL,
    embedding   BLOB NOT NULL,
    FOREIGN KEY(fact_id) REFERENCES memory_facts(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS skills (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    content     TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS app_settings (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL,
    updated_at  INTEGER NOT NULL
);

-- Wave 4.3: declarative agent-type personas (a code reviewer, a research
-- explorer). `tools_allowlist` is a JSON array of tool-name strings; the
-- effective belt is the INTERSECTION with the running body's tools, never a
-- widening. `seat` is a Wave-3.1 seat NAME resolved per-profile at dispatch.
-- `approval_status` mirrors skills' trust gate; `source` distinguishes
-- 'builtin' seeds from 'user'-authored / pack-installed types.
CREATE TABLE IF NOT EXISTS agent_types (
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
);

-- C3: persisted MCP server configs (the registration store). `args` and
-- `capabilities` are JSON arrays of strings; `tier` is "local" | "remote"
-- (ambiguous ⇒ remote, matching McpTrustTier::default). The runtime transport
-- (a spawned child) is derived session state — never persisted here.
CREATE TABLE IF NOT EXISTS mcp_servers (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL,
    command           TEXT NOT NULL,
    args              TEXT NOT NULL DEFAULT '[]',
    tier              TEXT NOT NULL DEFAULT 'remote',
    trusted_read_only INTEGER NOT NULL DEFAULT 0,
    capabilities      TEXT NOT NULL DEFAULT '[]',
    enabled           INTEGER NOT NULL DEFAULT 1,
    created_at        INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_memory_facts_origin ON memory_facts(origin_profile);
CREATE INDEX IF NOT EXISTS idx_memory_vectors_fact ON memory_vectors(fact_id);
"#;

// ─────────────────────────────────────────────────────────────────────────────
// Per-profile tables (profiles/<name>.db)
// ─────────────────────────────────────────────────────────────────────────────

pub const PROFILE_TABLES: &[&str] = &[
    "schema_version",
    // 1. conversations — chat thread metadata
    "conversations",
    // 2. messages — chat message history
    "messages",
    // 3. email_accounts — IMAP/SMTP config for this profile
    "email_accounts",
    // 4. email_messages — cached email body
    "email_messages",
    // 5. calendar_events — CalDAV events cached locally
    "calendar_events",
    // 6. tasks — per-profile task list
    "tasks",
    // 7. cron_jobs — scheduled jobs (jobs that target this profile's conversations)
    "cron_jobs",
    // 8. trm_logs — privacy classifier audit trail (spec §3)
    "trm_logs",
    // 9. folders — conversation organization (left pane)
    "folders",
    // 10. tag_definitions — tag palette
    "tag_definitions",
    // 11. session_tags — many-to-many join: conversation <-> tag
    "session_tags",
    // 12. tool_audit — append-only per-dispatch record (item 5, Q9)
    "tool_audit",
    // 13. tool_rules — persisted Always grants (Q8); live-read on gating path
    "tool_rules",
    // 14. classifier_settings — per-profile classifier thresholds (PLAN §11)
    "classifier_settings",
    // 15. memory_settings — per-profile memory toggles (Wave 1: semantic-search
    //     on/off, walled-memory island) — PLAN §9 + §7
    "memory_settings",
    // 16. usage_events — per-profile model-call cost ledger (Wave 3.2, PLAN §3):
    //     one row per model call; cost NULL = unknown ("flying blind"), never a guess.
    "usage_events",
    // 17. work_items — the one-queue-model substrate (Wave 4.4): deferred work
    //     (cron fires / agent dispatch / server results) as one lifecycle.
    "work_items",
    // 18. sandbox_config — per-profile OS-sandbox config (M7 Tier-K, PLAN §8 M7):
    //     one row (id=1) serializing `SandboxConfig`; a missing row = the legacy
    //     unconstrained default. Consumed as a per-profile CEILING (network) on
    //     the shell_exec path; the Seatbelt confinement itself is always-on.
    "sandbox_config",
];

/// CREATE TABLE statements for per-profile DBs (in dependency order).
pub const PROFILE_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (
    version     INTEGER PRIMARY KEY,
    applied_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS conversations (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    pinned          INTEGER NOT NULL DEFAULT 0,
    binding         TEXT NOT NULL DEFAULT 'auto',
    folder_id       TEXT,
    color           TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    FOREIGN KEY(folder_id) REFERENCES folders(id) ON DELETE SET NULL
);

CREATE TABLE IF NOT EXISTS messages (
    id                  TEXT PRIMARY KEY,
    conversation_id     TEXT NOT NULL,
    role                TEXT NOT NULL,           -- user | assistant | tool | system
    content             TEXT NOT NULL,
    model               TEXT,
    provider_id         TEXT,
    routing_decision    TEXT,
    thinking_content    TEXT,                    -- spec §5: reasoning output for thinking models
    error               TEXT,
    aborted             INTEGER NOT NULL DEFAULT 0,
    created_at          INTEGER NOT NULL,
    FOREIGN KEY(conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS email_accounts (
    id          TEXT PRIMARY KEY,
    label       TEXT NOT NULL,
    address     TEXT NOT NULL,
    imap_host   TEXT,
    imap_port   INTEGER,
    smtp_host   TEXT,
    smtp_port   INTEGER,
    username    TEXT,
    created_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS email_messages (
    id          TEXT PRIMARY KEY,
    account_id  TEXT NOT NULL,
    subject     TEXT,
    from_addr   TEXT,
    date        INTEGER,
    body        TEXT,
    read        INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY(account_id) REFERENCES email_accounts(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS calendar_events (
    id          TEXT PRIMARY KEY,
    title       TEXT NOT NULL,
    start_time  INTEGER NOT NULL,
    end_time    INTEGER,
    location    TEXT,
    description TEXT,
    source      TEXT
);

CREATE TABLE IF NOT EXISTS tasks (
    id          TEXT PRIMARY KEY,
    title       TEXT NOT NULL,
    done        INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS cron_jobs (
    id                          TEXT PRIMARY KEY,
    name                        TEXT NOT NULL,
    prompt                      TEXT NOT NULL,
    schedule                    TEXT NOT NULL,
    enabled                     INTEGER NOT NULL DEFAULT 1,
    last_run_at                 INTEGER,
    last_status                 TEXT,
    target_conversation_id      TEXT
);

CREATE TABLE IF NOT EXISTS trm_logs (
    id              TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL,
    message_hash    TEXT NOT NULL,
    decision        TEXT NOT NULL,           -- private | public
    confidence      REAL NOT NULL,          -- 0.0 - 1.0
    created_at      INTEGER NOT NULL,
    FOREIGN KEY(conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS folders (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    color       TEXT,
    created_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS tag_definitions (
    id          TEXT PRIMARY KEY,
    label       TEXT NOT NULL,
    color       TEXT,
    created_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS session_tags (
    conversation_id TEXT NOT NULL,
    tag_id          TEXT NOT NULL,
    PRIMARY KEY(conversation_id, tag_id),
    FOREIGN KEY(conversation_id) REFERENCES conversations(id) ON DELETE CASCADE,
    FOREIGN KEY(tag_id) REFERENCES tag_definitions(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS tool_audit (
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
CREATE INDEX IF NOT EXISTS idx_tool_audit_created ON tool_audit(ts);

CREATE TABLE IF NOT EXISTS tool_rules (
    id          TEXT PRIMARY KEY,
    tool_name   TEXT NOT NULL,
    pattern     TEXT NOT NULL,
    action      TEXT NOT NULL,        -- 'allow' | 'ask' | 'deny'
    created_at  INTEGER NOT NULL,
    UNIQUE(tool_name, pattern)
);

CREATE INDEX IF NOT EXISTS idx_tool_rules_tool ON tool_rules(tool_name);

-- Per-profile classifier thresholds (PLAN §11 settings page). Single row
-- (id=1); a missing row means "use defaults". Raw fusion thresholds are
-- stored (the UI strictness/band mapping lives in Rust, ClassifierConfig).
CREATE TABLE IF NOT EXISTS classifier_settings (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    tau_block   REAL NOT NULL,
    tau_band    REAL NOT NULL,
    updated_at  INTEGER NOT NULL
);

-- Per-profile memory toggles (Wave 1). Single row (id=1); a missing row means
-- "use defaults" (semantic search ON, not walled). `semantic_search_enabled`
-- gates the meaning-lane embedder (PLAN §9 — the user's hard off switch for
-- computing a meaning fingerprint of everything they save). `walled` is the §7
-- "keep this profile's memory private" island: when set, this profile's memory
-- lives in its own physically-separate DB, never global.db.
CREATE TABLE IF NOT EXISTS memory_settings (
    id                      INTEGER PRIMARY KEY CHECK (id = 1),
    semantic_search_enabled INTEGER NOT NULL DEFAULT 1,
    walled                  INTEGER NOT NULL DEFAULT 0,
    updated_at              INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS usage_events (
    id                TEXT PRIMARY KEY,
    conversation_id   TEXT,
    model             TEXT NOT NULL,
    provider_id       TEXT,
    provider_kind     TEXT NOT NULL,          -- 'local' | 'cloud' | 'custom'
    cost_usd          REAL,                    -- NULL = unknown ("flying blind"); local = 0.0
    created_at        INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS work_items (
    id                      TEXT PRIMARY KEY,
    kind                    TEXT NOT NULL,      -- 'cron' | 'agent_dispatch' | 'server_result'
    state                   TEXT NOT NULL,      -- 'queued'|'running'|'done'|'failed'|'parked'|'cancelled'
    source_ref              TEXT,
    input_json              TEXT NOT NULL,
    result_json             TEXT,
    error                   TEXT,
    scheduled_at            INTEGER,            -- fire-time; NULL = run ASAP
    claim_key               TEXT,               -- exactly-once dedup (partial-unique below)
    idempotency_key         TEXT,               -- 2.5 durability guard
    attempts                INTEGER NOT NULL DEFAULT 0,
    target_conversation_id  TEXT,
    created_at              INTEGER NOT NULL,
    started_at              INTEGER,
    finished_at             INTEGER
);

-- Wave 3.1: per-profile model-seat bindings. `seat` is a user-defined name (any
-- string), resolved to (provider_id, model) at run time; an unbound seat
-- inherits the caller's model. No FK to endpoints — a binding may outlive a
-- deleted provider, and resolve_seat falls back when it does.
CREATE TABLE IF NOT EXISTS seat_bindings (
    seat         TEXT PRIMARY KEY,
    provider_id  TEXT NOT NULL,
    model        TEXT NOT NULL,
    updated_at   INTEGER NOT NULL
);

-- Per-profile OS-sandbox config (M7 Tier-K). Single row (id=1) serializing
-- `SandboxConfig` as JSON; a missing row means the legacy unconstrained default
-- (shell network follows the call's own request). Consumed on the shell_exec
-- path as a per-profile CEILING (e.g. a locked-down profile denies shell
-- network even when the call asks for it). The Seatbelt confinement itself is
-- always-on and independent of this row — this never yields an unsandboxed run.
CREATE TABLE IF NOT EXISTS sandbox_config (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    config_json TEXT NOT NULL,
    updated_at  INTEGER NOT NULL
);

-- C1: per-profile budget cap (spend governor). NULL/no row = uncapped.
CREATE TABLE IF NOT EXISTS budget_settings (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    cap_usd    REAL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_messages_conversation ON messages(conversation_id);
CREATE INDEX IF NOT EXISTS idx_messages_created ON messages(created_at);
CREATE INDEX IF NOT EXISTS idx_conversations_folder ON conversations(folder_id);
CREATE INDEX IF NOT EXISTS idx_conversations_updated ON conversations(updated_at);
CREATE INDEX IF NOT EXISTS idx_trm_logs_conversation ON trm_logs(conversation_id);
CREATE INDEX IF NOT EXISTS idx_trm_logs_created ON trm_logs(created_at);
CREATE INDEX IF NOT EXISTS idx_email_messages_account ON email_messages(account_id);
CREATE INDEX IF NOT EXISTS idx_calendar_events_start ON calendar_events(start_time);
CREATE INDEX IF NOT EXISTS idx_tasks_done ON tasks(done);
CREATE INDEX IF NOT EXISTS idx_usage_events_created ON usage_events(created_at);
CREATE UNIQUE INDEX IF NOT EXISTS idx_work_items_claim ON work_items(claim_key) WHERE claim_key IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_work_items_state_sched ON work_items(state, scheduled_at);
"#;
