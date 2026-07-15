//! `ProfileDb` — CRUD over a per-profile database (`profiles/<name>.db`).
//!
//! Schema source of truth: spec §5 (Storage Schema). All timestamps are
//! Unix seconds. The agent always knows which profile it's operating in
//! (see spec §1) and writes always go to the active profile; cross-profile
//! reads are handled at a higher level.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use super::migrations::migrate_profile;

// ─────────────────────────────────────────────────────────────────────────────
// Row types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Conversation {
    pub id: String,
    pub name: String,
    pub pinned: bool,
    pub binding: String,
    pub folder_id: Option<String>,
    pub color: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub id: String,
    pub conversation_id: String,
    /// "user" | "assistant" | "tool" | "system"
    pub role: String,
    pub content: String,
    pub model: Option<String>,
    pub provider_id: Option<String>,
    pub routing_decision: Option<String>,
    /// Reasoning/thinking output (spec §5). None for non-thinking models.
    pub thinking_content: Option<String>,
    pub error: Option<String>,
    pub aborted: bool,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct EmailAccount {
    pub id: String,
    pub label: String,
    pub address: String,
    pub imap_host: Option<String>,
    pub imap_port: Option<i64>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<i64>,
    pub username: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct EmailMessage {
    pub id: String,
    pub account_id: String,
    pub subject: Option<String>,
    pub from_addr: Option<String>,
    pub date: Option<i64>,
    pub body: Option<String>,
    pub read: bool,
}

#[derive(Debug, Clone)]
pub struct CalendarEvent {
    pub id: String,
    pub title: String,
    pub start_time: i64,
    pub end_time: Option<i64>,
    pub location: Option<String>,
    pub description: Option<String>,
    pub source: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub done: bool,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct CronJob {
    pub id: String,
    pub name: String,
    pub prompt: String,
    pub schedule: String,
    pub enabled: bool,
    pub last_run_at: Option<i64>,
    pub last_status: Option<String>,
    pub target_conversation_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TrmLog {
    pub id: String,
    pub conversation_id: String,
    pub message_hash: String,
    /// "private" | "public"
    pub decision: String,
    pub confidence: f64,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct Folder {
    pub id: String,
    pub name: String,
    pub color: Option<String>,
    pub created_at: i64,
}

/// One row in the append-only `tool_audit` table — a single tool dispatch's
/// post-hoc record (item 5, Fable Q9). Written AFTER the outcome exists
/// (observer lane, never gates). Denied/asked/unknown/unavailable/err calls
/// are rows too — refusals are the interesting audit entries.
///
/// `canonical_args` is size-capped on write (see `CAPTURED_ARGS_CAP` in
/// `hooks::audit`) so a long file body can't blow up a row.
/// All times are Unix seconds (UTC).
#[derive(Debug, Clone)]
pub struct ToolAuditRow {
    /// Set by the DB on insert (AUTOINCREMENT). `0` means "not yet inserted".
    pub id: i64,
    pub ts: i64,
    pub conversation_id: String,
    pub tool_name: String,
    pub canonical_args: String,
    pub fingerprint: String,
    /// `format!("{:?}", tool.risk())` — one of "Safe" / "Write" /
    /// "External" / "Dangerous". Storing as text so a future risk-class
    /// rename doesn't break the table.
    pub risk: String,
    /// One of "ok" / "err" / "denied" / "ask" / "unavailable" / "unknown".
    pub outcome: String,
    /// The hook name that denied/asked, if any — e.g. "sandbox",
    /// "permission", "protected_path", "privacy-filter", "user",
    /// "approval", "budget", "batch". `None` for `Ok`/`Err`/`Unknown`/
    /// `Unavailable`.
    pub gate_by: Option<String>,
    /// What authority allowed the call, when applicable: "pre-trusted" for
    /// whole-tool-allow Safe tools; "once-fp" for a Once+Fingerprint grant;
    /// "session-fp" / "session-tool" for a session-scoped grant. `None`
    /// when the audit row's grant source isn't determinable.
    pub grant_used: Option<String>,
    /// For an `Ask`-derived outcome: "approve-once" / "approve-session" /
    /// "approve-tool" / "approve-always" / "deny" / "timeout". `None` for
    /// other outcomes.
    pub decision: Option<String>,
    /// "local" or "cloud" — the endpoint kind the call was on at the
    /// `is_cloud` argument to `dispatch()`. Tells the Activity pane at a
    /// glance which calls would have left the device.
    pub endpoint_kind: Option<String>,
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct TagDefinition {
    pub id: String,
    pub label: String,
    pub color: Option<String>,
    pub created_at: i64,
}

// ─────────────────────────────────────────────────────────────────────────────
// ProfileDb
// ─────────────────────────────────────────────────────────────────────────────

pub struct ProfileDb {
    conn: Connection,
    /// The profile name this DB belongs to (e.g. "personal", "work"). Set
    /// on open and used for logging + cross-profile access checks.
    pub name: String,
}

// SAFETY (M1): see `Storage` for the full rationale. `ProfileDb` holds
// a bare `rusqlite::Connection` (RefCell-backed, `!Sync`). The agent
// loop's internal `stream_lock` and Tauri's command boundary together
// guarantee no two threads touch a `ProfileDb` concurrently in the M1
// wiring. If/when true concurrency is needed, wrap `conn` in a
// `parking_lot::Mutex` and remove these manual impls.
unsafe impl Send for ProfileDb {}
unsafe impl Sync for ProfileDb {}

impl ProfileDb {
    /// Open an existing profile DB (or create + migrate a fresh one).
    pub fn open(path: &std::path::Path, name: &str) -> Result<Self> {
        crate::storage::ensure_sqlite_vec_registered();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating profile dir {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening profile db at {}", path.display()))?;
        migrate_profile(&conn)?;
        Ok(Self {
            conn,
            name: name.to_string(),
        })
    }

    /// In-memory variant for tests. The `name` is still recorded so
    /// cross-profile log lines look correct.
    #[cfg(test)]
    pub fn open_in_memory(name: &str) -> Result<Self> {
        crate::storage::ensure_sqlite_vec_registered();
        let conn = Connection::open_in_memory()?;
        migrate_profile(&conn)?;
        Ok(Self {
            conn,
            name: name.to_string(),
        })
    }

    pub fn raw(&self) -> &Connection {
        &self.conn
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    // ── conversations ───────────────────────────────────────────────────────

    pub fn create_conversation(&self, c: &Conversation) -> Result<()> {
        self.conn.execute(
            "INSERT INTO conversations
             (id, name, pinned, binding, folder_id, color, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                c.id,
                c.name,
                c.pinned as i64,
                c.binding,
                c.folder_id,
                c.color,
                c.created_at,
                c.updated_at
            ],
        )?;
        Ok(())
    }

    pub fn get_conversation(&self, id: &str) -> Result<Option<Conversation>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, name, pinned, binding, folder_id, color, created_at, updated_at
                 FROM conversations WHERE id = ?1",
                params![id],
                row_to_conversation,
            )
            .optional()?)
    }

    pub fn list_conversations(&self) -> Result<Vec<Conversation>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, pinned, binding, folder_id, color, created_at, updated_at
             FROM conversations
             ORDER BY pinned DESC, updated_at DESC",
        )?;
        let rows = stmt
            .query_map([], row_to_conversation)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn list_conversations_in_folder(&self, folder_id: &str) -> Result<Vec<Conversation>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, pinned, binding, folder_id, color, created_at, updated_at
             FROM conversations WHERE folder_id = ?1
             ORDER BY pinned DESC, updated_at DESC",
        )?;
        let rows = stmt
            .query_map(params![folder_id], row_to_conversation)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Update mutable fields. `name`, `pinned`, `binding`, `folder_id`,
    /// `color`, and `updated_at` are settable; `id` and `created_at` are not.
    /// Pass `None` for `folder_id` to remove a conversation from its folder.
    pub fn update_conversation(
        &self,
        id: &str,
        name: &str,
        pinned: bool,
        binding: &str,
        folder_id: Option<&str>,
        color: Option<&str>,
    ) -> Result<bool> {
        let now = chrono::Utc::now().timestamp();
        let n = self.conn.execute(
            "UPDATE conversations
             SET name = ?1, pinned = ?2, binding = ?3, folder_id = ?4, color = ?5, updated_at = ?6
             WHERE id = ?7",
            params![name, pinned as i64, binding, folder_id, color, now, id],
        )?;
        Ok(n > 0)
    }

    pub fn delete_conversation(&self, id: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM conversations WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    // ── messages ────────────────────────────────────────────────────────────

    pub fn add_message(&self, m: &Message) -> Result<()> {
        self.conn.execute(
            "INSERT INTO messages
             (id, conversation_id, role, content, model, provider_id,
              routing_decision, thinking_content, error, aborted, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                m.id,
                m.conversation_id,
                m.role,
                m.content,
                m.model,
                m.provider_id,
                m.routing_decision,
                m.thinking_content,
                m.error,
                m.aborted as i64,
                m.created_at
            ],
        )?;
        Ok(())
    }

    pub fn get_message(&self, id: &str) -> Result<Option<Message>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, conversation_id, role, content, model, provider_id,
                        routing_decision, thinking_content, error, aborted, created_at
                 FROM messages WHERE id = ?1",
                params![id],
                row_to_message,
            )
            .optional()?)
    }

    pub fn list_messages_by_conversation(&self, conversation_id: &str) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            // Tiebreak on rowid (insertion order), NOT id: `created_at` is
            // second-granularity and `id` is a random UUID, so two messages
            // written in the same second would otherwise sort by a random
            // string. rowid preserves insertion order, which the transcript
            // and send_message's "last assistant row" lookup rely on.
            "SELECT id, conversation_id, role, content, model, provider_id,
                    routing_decision, thinking_content, error, aborted, created_at
             FROM messages WHERE conversation_id = ?1
             ORDER BY created_at ASC, rowid ASC",
        )?;
        let rows = stmt
            .query_map(params![conversation_id], row_to_message)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Update a message. Used for: setting `aborted = 1` when a stream is
    /// interrupted, writing `error` after a failed model call, or appending
    /// `thinking_content` once a thinking model returns.
    pub fn update_message(
        &self,
        id: &str,
        content: Option<&str>,
        thinking_content: Option<&str>,
        error: Option<&str>,
        aborted: Option<bool>,
    ) -> Result<bool> {
        // Build a dynamic SET clause so callers can patch any subset.
        // We coalesce NULLs in the SQL to the existing column value.
        let n = self.conn.execute(
            "UPDATE messages SET
                content          = COALESCE(?1, content),
                thinking_content = COALESCE(?2, thinking_content),
                error            = COALESCE(?3, error),
                aborted          = COALESCE(?4, aborted)
             WHERE id = ?5",
            params![
                content,
                thinking_content,
                error,
                aborted.map(|b| b as i64),
                id,
            ],
        )?;
        Ok(n > 0)
    }

    pub fn delete_message(&self, id: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM messages WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    // ── folders ─────────────────────────────────────────────────────────────

    pub fn create_folder(&self, f: &Folder) -> Result<()> {
        self.conn.execute(
            "INSERT INTO folders (id, name, color, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![f.id, f.name, f.color, f.created_at],
        )?;
        Ok(())
    }

    pub fn get_folder(&self, id: &str) -> Result<Option<Folder>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, name, color, created_at FROM folders WHERE id = ?1",
                params![id],
                |r| {
                    Ok(Folder {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        color: r.get(2)?,
                        created_at: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn list_folders(&self) -> Result<Vec<Folder>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, color, created_at FROM folders ORDER BY name")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Folder {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    color: r.get(2)?,
                    created_at: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn delete_folder(&self, id: &str) -> Result<bool> {
        // ON DELETE SET NULL on conversations.folder_id handles the cleanup.
        let n = self
            .conn
            .execute("DELETE FROM folders WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    // ── tag_definitions + session_tags ──────────────────────────────────────

    pub fn create_tag(&self, t: &TagDefinition) -> Result<()> {
        self.conn.execute(
            "INSERT INTO tag_definitions (id, label, color, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![t.id, t.label, t.color, t.created_at],
        )?;
        Ok(())
    }

    pub fn list_tags(&self) -> Result<Vec<TagDefinition>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, label, color, created_at FROM tag_definitions ORDER BY label")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(TagDefinition {
                    id: r.get(0)?,
                    label: r.get(1)?,
                    color: r.get(2)?,
                    created_at: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn delete_tag(&self, id: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM tag_definitions WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    /// Apply a tag to a conversation. Idempotent (INSERT OR IGNORE on the
    /// composite PK).
    pub fn tag_conversation(&self, conversation_id: &str, tag_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO session_tags (conversation_id, tag_id) VALUES (?1, ?2)",
            params![conversation_id, tag_id],
        )?;
        Ok(())
    }

    pub fn untag_conversation(&self, conversation_id: &str, tag_id: &str) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM session_tags WHERE conversation_id = ?1 AND tag_id = ?2",
            params![conversation_id, tag_id],
        )?;
        Ok(n > 0)
    }

    /// All conversations that carry a given tag.
    pub fn list_conversations_with_tag(&self, tag_id: &str) -> Result<Vec<Conversation>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.id, c.name, c.pinned, c.binding, c.folder_id, c.color, c.created_at, c.updated_at
             FROM conversations c
             JOIN session_tags st ON st.conversation_id = c.id
             WHERE st.tag_id = ?1
             ORDER BY c.pinned DESC, c.updated_at DESC",
        )?;
        let rows = stmt
            .query_map(params![tag_id], row_to_conversation)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// All tags applied to a given conversation.
    pub fn list_tags_for_conversation(&self, conversation_id: &str) -> Result<Vec<TagDefinition>> {
        let mut stmt = self.conn.prepare(
            "SELECT td.id, td.label, td.color, td.created_at
             FROM tag_definitions td
             JOIN session_tags st ON st.tag_id = td.id
             WHERE st.conversation_id = ?1
             ORDER BY td.label",
        )?;
        let rows = stmt
            .query_map(params![conversation_id], |r| {
                Ok(TagDefinition {
                    id: r.get(0)?,
                    label: r.get(1)?,
                    color: r.get(2)?,
                    created_at: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ── email_accounts + email_messages ─────────────────────────────────────

    pub fn insert_email_account(&self, a: &EmailAccount) -> Result<()> {
        self.conn.execute(
            "INSERT INTO email_accounts
             (id, label, address, imap_host, imap_port, smtp_host, smtp_port, username, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                a.id,
                a.label,
                a.address,
                a.imap_host,
                a.imap_port,
                a.smtp_host,
                a.smtp_port,
                a.username,
                a.created_at
            ],
        )?;
        Ok(())
    }

    pub fn list_email_accounts(&self) -> Result<Vec<EmailAccount>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, label, address, imap_host, imap_port, smtp_host, smtp_port,
                    username, created_at
             FROM email_accounts ORDER BY label",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(EmailAccount {
                    id: r.get(0)?,
                    label: r.get(1)?,
                    address: r.get(2)?,
                    imap_host: r.get(3)?,
                    imap_port: r.get(4)?,
                    smtp_host: r.get(5)?,
                    smtp_port: r.get(6)?,
                    username: r.get(7)?,
                    created_at: r.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn insert_email_message(&self, m: &EmailMessage) -> Result<()> {
        self.conn.execute(
            "INSERT INTO email_messages (id, account_id, subject, from_addr, date, body, read)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                m.id,
                m.account_id,
                m.subject,
                m.from_addr,
                m.date,
                m.body,
                m.read as i64
            ],
        )?;
        Ok(())
    }

    pub fn list_email_messages(&self, account_id: &str) -> Result<Vec<EmailMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, account_id, subject, from_addr, date, body, read
             FROM email_messages WHERE account_id = ?1 ORDER BY date DESC",
        )?;
        let rows = stmt
            .query_map(params![account_id], |r| {
                Ok(EmailMessage {
                    id: r.get(0)?,
                    account_id: r.get(1)?,
                    subject: r.get(2)?,
                    from_addr: r.get(3)?,
                    date: r.get(4)?,
                    body: r.get(5)?,
                    read: r.get::<_, i64>(6)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ── calendar_events ─────────────────────────────────────────────────────

    pub fn insert_calendar_event(&self, e: &CalendarEvent) -> Result<()> {
        self.conn.execute(
            "INSERT INTO calendar_events
             (id, title, start_time, end_time, location, description, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                e.id,
                e.title,
                e.start_time,
                e.end_time,
                e.location,
                e.description,
                e.source
            ],
        )?;
        Ok(())
    }

    pub fn list_calendar_events(&self, from: i64, to: i64) -> Result<Vec<CalendarEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, start_time, end_time, location, description, source
             FROM calendar_events
             WHERE start_time >= ?1 AND start_time < ?2
             ORDER BY start_time ASC",
        )?;
        let rows = stmt
            .query_map(params![from, to], |r| {
                Ok(CalendarEvent {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    start_time: r.get(2)?,
                    end_time: r.get(3)?,
                    location: r.get(4)?,
                    description: r.get(5)?,
                    source: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ── tasks ───────────────────────────────────────────────────────────────

    pub fn insert_task(&self, t: &Task) -> Result<()> {
        self.conn.execute(
            "INSERT INTO tasks (id, title, done, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![t.id, t.title, t.done as i64, t.created_at],
        )?;
        Ok(())
    }

    pub fn list_tasks(&self) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, done, created_at FROM tasks ORDER BY done, created_at DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Task {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    done: r.get::<_, i64>(2)? != 0,
                    created_at: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn set_task_done(&self, id: &str, done: bool) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE tasks SET done = ?1 WHERE id = ?2",
            params![done as i64, id],
        )?;
        Ok(n > 0)
    }

    pub fn delete_task(&self, id: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    // ── cron_jobs ───────────────────────────────────────────────────────────

    pub fn insert_cron_job(&self, j: &CronJob) -> Result<()> {
        self.conn.execute(
            "INSERT INTO cron_jobs
             (id, name, prompt, schedule, enabled, last_run_at, last_status, target_conversation_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                j.id,
                j.name,
                j.prompt,
                j.schedule,
                j.enabled as i64,
                j.last_run_at,
                j.last_status,
                j.target_conversation_id
            ],
        )?;
        Ok(())
    }

    pub fn list_cron_jobs(&self) -> Result<Vec<CronJob>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, prompt, schedule, enabled, last_run_at, last_status, target_conversation_id
             FROM cron_jobs ORDER BY name",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(CronJob {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    prompt: r.get(2)?,
                    schedule: r.get(3)?,
                    enabled: r.get::<_, i64>(4)? != 0,
                    last_run_at: r.get(5)?,
                    last_status: r.get(6)?,
                    target_conversation_id: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_cron_job(&self, id: &str) -> Result<Option<CronJob>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, name, prompt, schedule, enabled, last_run_at, last_status, target_conversation_id
                 FROM cron_jobs WHERE id = ?1",
                params![id],
                |r| {
                    Ok(CronJob {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        prompt: r.get(2)?,
                        schedule: r.get(3)?,
                        enabled: r.get::<_, i64>(4)? != 0,
                        last_run_at: r.get(5)?,
                        last_status: r.get(6)?,
                        target_conversation_id: r.get(7)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn set_cron_job_enabled(&self, id: &str, enabled: bool) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE cron_jobs SET enabled = ?1 WHERE id = ?2",
            params![enabled as i64, id],
        )?;
        Ok(n > 0)
    }

    pub fn record_cron_run(&self, id: &str, status: &str) -> Result<bool> {
        let now = chrono::Utc::now().timestamp();
        let n = self.conn.execute(
            "UPDATE cron_jobs SET last_run_at = ?1, last_status = ?2 WHERE id = ?3",
            params![now, status, id],
        )?;
        Ok(n > 0)
    }

    pub fn delete_cron_job(&self, id: &str) -> Result<bool> {
        let n = self
            .conn
            .execute("DELETE FROM cron_jobs WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    // ── trm_logs ────────────────────────────────────────────────────────────

    pub fn insert_trm_log(&self, l: &TrmLog) -> Result<()> {
        self.conn.execute(
            "INSERT INTO trm_logs
             (id, conversation_id, message_hash, decision, confidence, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                l.id,
                l.conversation_id,
                l.message_hash,
                l.decision,
                l.confidence,
                l.created_at
            ],
        )?;
        Ok(())
    }

    pub fn list_trm_logs(&self, conversation_id: &str) -> Result<Vec<TrmLog>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, conversation_id, message_hash, decision, confidence, created_at
             FROM trm_logs WHERE conversation_id = ?1 ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map(params![conversation_id], |r| {
                Ok(TrmLog {
                    id: r.get(0)?,
                    conversation_id: r.get(1)?,
                    message_hash: r.get(2)?,
                    decision: r.get(3)?,
                    confidence: r.get(4)?,
                    created_at: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Spec §3: TRM logs are auto-deleted after 7 days. Call this from a
    /// background task to enforce retention.
    pub fn purge_trm_logs_older_than(&self, cutoff: i64) -> Result<usize> {
        let n = self.conn.execute(
            "DELETE FROM trm_logs WHERE created_at < ?1",
            params![cutoff],
        )?;
        Ok(n)
    }

    // ── tool_audit (item 5, Fable Q9) ─────────────────────────────────────
    //
    // Append-only: only `add_tool_audit` (INSERT) and `list_tool_audit`
    // (SELECT) — no UPDATE, no DELETE. The table is the post-hoc record
    // lane; refusing to mutate rows keeps the audit chain defensible
    // (a future "purge after N days" can only be a separate retention
    // background task, not an in-band UPDATE).

    /// Insert one audit row. `id` is ignored (the column is AUTOINCREMENT)
    /// — the inserted `id` is reflected in the row after this call via
    /// `conn.last_insert_rowid()` but we don't return it; the caller
    /// that needs it back can re-`list_tool_audit` by `(ts, fingerprint)`.
    pub fn add_tool_audit(&self, row: &ToolAuditRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO tool_audit
             (ts, conversation_id, tool_name, canonical_args, fingerprint, risk, outcome,
              gate_by, grant_used, decision, endpoint_kind, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                row.ts,
                row.conversation_id,
                row.tool_name,
                row.canonical_args,
                row.fingerprint,
                row.risk,
                row.outcome,
                row.gate_by,
                row.grant_used,
                row.decision,
                row.endpoint_kind,
                row.duration_ms,
            ],
        )
        .context("insert tool_audit row")?;
        Ok(())
    }

    /// All audit rows for a conversation, oldest-first (insertion order).
    /// Empty Vec if there are no calls yet on this conversation — a fresh
    /// install has no rows, and a conversation that hasn't called any tools
    /// also has no rows.
    pub fn list_tool_audit(&self, conversation_id: &str) -> Result<Vec<ToolAuditRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, conversation_id, tool_name, canonical_args, fingerprint,
                    risk, outcome, gate_by, grant_used, decision, endpoint_kind, duration_ms
             FROM tool_audit WHERE conversation_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map(params![conversation_id], |row| {
                Ok(ToolAuditRow {
                    id: row.get(0)?,
                    ts: row.get(1)?,
                    conversation_id: row.get(2)?,
                    tool_name: row.get(3)?,
                    canonical_args: row.get(4)?,
                    fingerprint: row.get(5)?,
                    risk: row.get(6)?,
                    outcome: row.get(7)?,
                    gate_by: row.get(8)?,
                    grant_used: row.get(9)?,
                    decision: row.get(10)?,
                    endpoint_kind: row.get(11)?,
                    duration_ms: row.get(12)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("query tool_audit rows")?;
        Ok(rows)
    }
}

fn row_to_conversation(r: &rusqlite::Row<'_>) -> rusqlite::Result<Conversation> {
    Ok(Conversation {
        id: r.get(0)?,
        name: r.get(1)?,
        pinned: r.get::<_, i64>(2)? != 0,
        binding: r.get(3)?,
        folder_id: r.get(4)?,
        color: r.get(5)?,
        created_at: r.get(6)?,
        updated_at: r.get(7)?,
    })
}

fn row_to_message(r: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get(0)?,
        conversation_id: r.get(1)?,
        role: r.get(2)?,
        content: r.get(3)?,
        model: r.get(4)?,
        provider_id: r.get(5)?,
        routing_decision: r.get(6)?,
        thinking_content: r.get(7)?,
        error: r.get(8)?,
        aborted: r.get::<_, i64>(9)? != 0,
        created_at: r.get(10)?,
    })
}
