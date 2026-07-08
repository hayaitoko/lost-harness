//! SQL schema constants for the Lost Harness storage layer.
//!
//! Two databases:
//!   - `global.db` — shared across all profiles (user_facts, endpoints, etc.)
//!   - `profiles/<name>.db` — per-profile data (conversations, messages, etc.)
//!
//! Schema source of truth: spec §1 (Profile Data Model) + §5 (Storage Schema).
//! All tables use INTEGER for timestamps (Unix seconds, UTC) — chrono::Utc::now().timestamp().

/// Returns the current schema version. Bump when adding a new migration.
pub const SCHEMA_VERSION: i32 = 1;

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

CREATE INDEX IF NOT EXISTS idx_messages_conversation ON messages(conversation_id);
CREATE INDEX IF NOT EXISTS idx_messages_created ON messages(created_at);
CREATE INDEX IF NOT EXISTS idx_conversations_folder ON conversations(folder_id);
CREATE INDEX IF NOT EXISTS idx_conversations_updated ON conversations(updated_at);
CREATE INDEX IF NOT EXISTS idx_trm_logs_conversation ON trm_logs(conversation_id);
CREATE INDEX IF NOT EXISTS idx_trm_logs_created ON trm_logs(created_at);
CREATE INDEX IF NOT EXISTS idx_email_messages_account ON email_messages(account_id);
CREATE INDEX IF NOT EXISTS idx_calendar_events_start ON calendar_events(start_time);
CREATE INDEX IF NOT EXISTS idx_tasks_done ON tasks(done);
"#;
