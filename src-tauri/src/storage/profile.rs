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

/// One booked model call in the per-profile usage ledger (Wave 3.2, PLAN §3).
/// `cost_usd` is `None` when the cost is UNKNOWN ("flying blind" — we don't
/// have the tokens/pricing to compute it) and `Some(0.0)` for a local /
/// on-device call. It is NEVER a silent guess for a cloud call.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageEvent {
    pub id: String,
    pub conversation_id: Option<String>,
    pub model: String,
    pub provider_id: Option<String>,
    /// "local" | "cloud" | "custom".
    pub provider_kind: String,
    pub cost_usd: Option<f64>,
    pub created_at: i64,
}

/// A roll-up of a profile's usage ledger — the shape a budget governor / a
/// "spend so far" UI reads. `unknown_cost_calls` is the honest "flying blind"
/// count: cloud calls we couldn't price. `known_cost_usd` sums only the calls
/// whose cost we actually know (local $0 + any priced cloud call).
#[derive(Debug, Clone, PartialEq)]
pub struct UsageSummary {
    pub total_calls: usize,
    pub known_cost_usd: f64,
    pub unknown_cost_calls: usize,
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

/// A persisted tool-permission rule (Q8 `Always` grant) — the durable half of
/// the approval spine. Lives in the PER-PROFILE DB, so a rule written in one
/// profile physically never applies in another (the walled-profile principle,
/// applied conservatively to standing tool authorizations). Read live by
/// `hooks::SqlitePolicySource` on the gating path and resolved through the same
/// `PermissionHook` deny>ask>allow / most-specific-wins path as an in-memory
/// `ToolRule`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRuleRow {
    pub id: String,
    pub tool_name: String,
    /// Glob pattern matched against the call's `command_text` (`"*"` = whole
    /// tool). Same vocabulary as `hooks::ToolRule`.
    pub pattern: String,
    /// "allow" | "ask" | "deny". The dialog only ever writes "allow"; "ask"/
    /// "deny" are reachable via Settings-authored rules.
    pub action: String,
    pub created_at: i64,
}

// ─────────────────────────────────────────────────────────────────────────────
// ProfileDb
// ─────────────────────────────────────────────────────────────────────────────

pub struct ProfileDb {
    conn: parking_lot::Mutex<Connection>,
    /// The profile name this DB belongs to (e.g. "personal", "work"). Set
    /// on open and used for logging + cross-profile access checks.
    pub name: String,
}

// `ProfileDb` is genuinely `Send + Sync` now: `conn` is a
// `parking_lot::Mutex<Connection>` (`Mutex<T>: Sync` when `T: Send`, and
// `rusqlite::Connection: Send`), and `name: String` is `Send + Sync` on its
// own. No manual/unsafe impl is needed or present.

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
            conn: parking_lot::Mutex::new(conn),
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
            conn: parking_lot::Mutex::new(conn),
            name: name.to_string(),
        })
    }

    /// Lock and borrow the underlying connection. Use sparingly — most
    /// callers should go through the typed methods. The returned guard holds
    /// the connection's mutex for as long as it lives; a caller must not
    /// invoke another locking method on this same `ProfileDb` while holding
    /// it — `parking_lot::Mutex` is not reentrant, so that would deadlock.
    pub fn raw(&self) -> parking_lot::MutexGuard<'_, Connection> {
        self.conn.lock()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    // ── conversations ───────────────────────────────────────────────────────

    pub fn create_conversation(&self, c: &Conversation) -> Result<()> {
        self.conn.lock().execute(
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
            .lock()
            .query_row(
                "SELECT id, name, pinned, binding, folder_id, color, created_at, updated_at
                 FROM conversations WHERE id = ?1",
                params![id],
                row_to_conversation,
            )
            .optional()?)
    }

    pub fn list_conversations(&self) -> Result<Vec<Conversation>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
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
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
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
        let n = self.conn.lock().execute(
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
            .lock()
            .execute("DELETE FROM conversations WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    // ── messages ────────────────────────────────────────────────────────────

    pub fn add_message(&self, m: &Message) -> Result<()> {
        self.conn.lock().execute(
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
            .lock()
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
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
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

    /// Search this profile's past conversation transcript for messages whose
    /// content contains `query` (case-insensitive substring). Powers the
    /// `session_search` tool — the agent's recall over past chats, distinct from
    /// the memory archive. Only `user`/`assistant` turns are searched (tool/
    /// system rows are noise). Most-recent first, capped at `limit`. LIKE
    /// wildcards in `query` are escaped so a literal `%`/`_` matches literally.
    pub fn search_messages(&self, query: &str, limit: usize) -> Result<Vec<SessionSearchHit>> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        // Escape LIKE metacharacters (order matters: backslash first).
        let escaped = trimmed
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT m.conversation_id, c.name, m.role, m.content, m.created_at
             FROM messages m
             LEFT JOIN conversations c ON c.id = m.conversation_id
             WHERE m.role IN ('user', 'assistant')
               AND m.content LIKE ?1 ESCAPE '\\'
             ORDER BY m.created_at DESC, m.rowid DESC
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![pattern, limit as i64], |r| {
                Ok(SessionSearchHit {
                    conversation_id: r.get(0)?,
                    conversation_name: r.get(1)?,
                    role: r.get(2)?,
                    content: r.get(3)?,
                    created_at: r.get(4)?,
                })
            })?
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
        let n = self.conn.lock().execute(
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
            .lock()
            .execute("DELETE FROM messages WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    // ── folders ─────────────────────────────────────────────────────────────

    pub fn create_folder(&self, f: &Folder) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO folders (id, name, color, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![f.id, f.name, f.color, f.created_at],
        )?;
        Ok(())
    }

    pub fn get_folder(&self, id: &str) -> Result<Option<Folder>> {
        Ok(self
            .conn
            .lock()
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
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT id, name, color, created_at FROM folders ORDER BY name")?;
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
            .lock()
            .execute("DELETE FROM folders WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    // ── tag_definitions + session_tags ──────────────────────────────────────

    pub fn create_tag(&self, t: &TagDefinition) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO tag_definitions (id, label, color, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![t.id, t.label, t.color, t.created_at],
        )?;
        Ok(())
    }

    pub fn list_tags(&self) -> Result<Vec<TagDefinition>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT id, label, color, created_at FROM tag_definitions ORDER BY label")?;
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
            .lock()
            .execute("DELETE FROM tag_definitions WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    /// Apply a tag to a conversation. Idempotent (INSERT OR IGNORE on the
    /// composite PK).
    pub fn tag_conversation(&self, conversation_id: &str, tag_id: &str) -> Result<()> {
        self.conn.lock().execute(
            "INSERT OR IGNORE INTO session_tags (conversation_id, tag_id) VALUES (?1, ?2)",
            params![conversation_id, tag_id],
        )?;
        Ok(())
    }

    pub fn untag_conversation(&self, conversation_id: &str, tag_id: &str) -> Result<bool> {
        let n = self.conn.lock().execute(
            "DELETE FROM session_tags WHERE conversation_id = ?1 AND tag_id = ?2",
            params![conversation_id, tag_id],
        )?;
        Ok(n > 0)
    }

    /// All conversations that carry a given tag.
    pub fn list_conversations_with_tag(&self, tag_id: &str) -> Result<Vec<Conversation>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
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
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
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
        self.conn.lock().execute(
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
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
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
        self.conn.lock().execute(
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
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
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
        self.conn.lock().execute(
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
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
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
        self.conn.lock().execute(
            "INSERT INTO tasks (id, title, done, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![t.id, t.title, t.done as i64, t.created_at],
        )?;
        Ok(())
    }

    pub fn list_tasks(&self) -> Result<Vec<Task>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
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
        let n = self.conn.lock().execute(
            "UPDATE tasks SET done = ?1 WHERE id = ?2",
            params![done as i64, id],
        )?;
        Ok(n > 0)
    }

    pub fn delete_task(&self, id: &str) -> Result<bool> {
        let n = self
            .conn
            .lock()
            .execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    // ── cron_jobs ───────────────────────────────────────────────────────────

    pub fn insert_cron_job(&self, j: &CronJob) -> Result<()> {
        self.conn.lock().execute(
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
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
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
            .lock()
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
        let n = self.conn.lock().execute(
            "UPDATE cron_jobs SET enabled = ?1 WHERE id = ?2",
            params![enabled as i64, id],
        )?;
        Ok(n > 0)
    }

    pub fn record_cron_run(&self, id: &str, status: &str) -> Result<bool> {
        let now = chrono::Utc::now().timestamp();
        let n = self.conn.lock().execute(
            "UPDATE cron_jobs SET last_run_at = ?1, last_status = ?2 WHERE id = ?3",
            params![now, status, id],
        )?;
        Ok(n > 0)
    }

    pub fn delete_cron_job(&self, id: &str) -> Result<bool> {
        let n = self
            .conn
            .lock()
            .execute("DELETE FROM cron_jobs WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    // ── usage_events (Wave 3.2 — the model-call cost ledger, PLAN §3) ─────────

    /// Book one model call to the ledger. `cost_usd` is `None` for an
    /// unknown/"flying blind" cloud cost (never a guess), `Some(0.0)` for local.
    pub fn record_usage(&self, e: &UsageEvent) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO usage_events
             (id, conversation_id, model, provider_id, provider_kind, cost_usd, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                e.id,
                e.conversation_id,
                e.model,
                e.provider_id,
                e.provider_kind,
                e.cost_usd,
                e.created_at
            ],
        )?;
        Ok(())
    }

    /// Roll up the whole profile's ledger: total calls, summed KNOWN cost, and
    /// the count of unknown-cost ("flying blind") calls. `SUM(cost_usd)` skips
    /// NULLs in SQLite, so the known-cost total is honest; the unknown count is
    /// surfaced separately rather than folded into the total as a guess.
    pub fn usage_summary(&self) -> Result<UsageSummary> {
        Ok(self.conn.lock().query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(cost_usd), 0.0),
                    SUM(CASE WHEN cost_usd IS NULL THEN 1 ELSE 0 END)
             FROM usage_events",
            [],
            |r| {
                Ok(UsageSummary {
                    total_calls: r.get::<_, i64>(0)? as usize,
                    known_cost_usd: r.get::<_, f64>(1)?,
                    unknown_cost_calls: r.get::<_, Option<i64>>(2)?.unwrap_or(0) as usize,
                })
            },
        )?)
    }

    /// Usage rolled up over events at/after `since_ts` (Unix seconds) — the
    /// windowed variant the budget governor (C1) reads, e.g. from the start of
    /// the current month. `usage_summary()` (all-time) still backs the Settings
    /// "Usage" view.
    pub fn usage_summary_since(&self, since_ts: i64) -> Result<UsageSummary> {
        Ok(self.conn.lock().query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(cost_usd), 0.0),
                    SUM(CASE WHEN cost_usd IS NULL THEN 1 ELSE 0 END)
             FROM usage_events WHERE created_at >= ?1",
            params![since_ts],
            |r| {
                Ok(UsageSummary {
                    total_calls: r.get::<_, i64>(0)? as usize,
                    known_cost_usd: r.get::<_, f64>(1)?,
                    unknown_cost_calls: r.get::<_, Option<i64>>(2)?.unwrap_or(0) as usize,
                })
            },
        )?)
    }

    // ── budget_settings (C1 — the spend governor's per-profile cap) ───────────

    /// This profile's spend cap in USD, or `None` when uncapped (no row / NULL).
    pub fn budget_cap(&self) -> Result<Option<f64>> {
        Ok(self
            .conn
            .lock()
            .query_row("SELECT cap_usd FROM budget_settings WHERE id = 1", [], |r| {
                r.get::<_, Option<f64>>(0)
            })
            .optional()
            .context("read budget_settings row")?
            .flatten())
    }

    /// Set (or clear, with `None`) this profile's spend cap. A negative cap is
    /// clamped to 0 (a cap can't be below zero).
    pub fn set_budget_cap(&self, cap_usd: Option<f64>) -> Result<()> {
        let cap = cap_usd.map(|c| if c < 0.0 { 0.0 } else { c });
        let now = chrono::Utc::now().timestamp();
        self.conn
            .lock()
            .execute(
                "INSERT INTO budget_settings (id, cap_usd, updated_at)
                 VALUES (1, ?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET cap_usd = excluded.cap_usd, updated_at = excluded.updated_at",
                params![cap, now],
            )
            .context("upsert budget_settings row")?;
        Ok(())
    }

    /// Clear the cap entirely (uncapped). Returns whether a row was removed.
    pub fn reset_budget_cap(&self) -> Result<bool> {
        let n = self
            .conn
            .lock()
            .execute("DELETE FROM budget_settings WHERE id = 1", [])
            .context("delete budget_settings row")?;
        Ok(n > 0)
    }

    // ── work_items (Wave 4.4 — the one-queue-model substrate) ─────────────────

    /// Enqueue a work item. Uses `INSERT OR IGNORE`, so a duplicate `claim_key`
    /// (the exactly-once dedup, via the partial-unique index) is silently
    /// skipped — returns `true` if a row was inserted, `false` if it was a
    /// dedup no-op (already queued/claimed for that key).
    pub fn insert_work_item(&self, w: &crate::queue::WorkItem) -> Result<bool> {
        let n = self.conn.lock().execute(
            "INSERT OR IGNORE INTO work_items
             (id, kind, state, source_ref, input_json, result_json, error, scheduled_at,
              claim_key, idempotency_key, attempts, target_conversation_id, created_at,
              started_at, finished_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            params![
                w.id,
                w.kind.as_str(),
                w.state.as_str(),
                w.source_ref,
                w.input_json,
                w.result_json,
                w.error,
                w.scheduled_at,
                w.claim_key,
                w.idempotency_key,
                w.attempts,
                w.target_conversation_id,
                w.created_at,
                w.started_at,
                w.finished_at,
            ],
        )?;
        Ok(n > 0)
    }

    /// Atomically claim the oldest DUE queued item (`scheduled_at` null or past),
    /// flipping it to `running` and stamping `started_at`/`attempts` in the same
    /// statement — so two runners can never claim the same item. Returns the
    /// claimed item, or `None` when nothing is due.
    pub fn claim_next_due_work(&self, now: i64) -> Result<Option<crate::queue::WorkItem>> {
        Ok(self
            .conn
            .lock()
            .query_row(
                "UPDATE work_items SET state='running', started_at=?1, attempts=attempts+1
                 WHERE id = (
                    SELECT id FROM work_items
                    WHERE state='queued' AND (scheduled_at IS NULL OR scheduled_at <= ?1)
                      -- C2: journal rows are dispatcher-driven, never runner-claimable
                      AND kind != 'mutating_action'
                    ORDER BY COALESCE(scheduled_at, created_at) ASC, created_at ASC
                    LIMIT 1
                 )
                 RETURNING id, kind, state, source_ref, input_json, result_json, error,
                           scheduled_at, claim_key, idempotency_key, attempts,
                           target_conversation_id, created_at, started_at, finished_at",
                params![now],
                row_to_work_item,
            )
            .optional()?)
    }

    /// Finish a currently-`running` item into a terminal-or-parked `to` state
    /// (`Done`/`Failed`/`Parked`). Guarded both by the checked lifecycle
    /// (`Running.can_transition_to`) and the SQL `state='running'` predicate, so
    /// a terminal item can never be re-finished. Returns whether a row moved.
    pub fn finish_work_item(
        &self,
        id: &str,
        to: crate::queue::WorkState,
        result_json: Option<&str>,
        error: Option<&str>,
        finished_at: i64,
    ) -> Result<bool> {
        if !crate::queue::WorkState::Running.can_transition_to(to) {
            anyhow::bail!("illegal work-item transition: running -> {}", to.as_str());
        }
        let n = self.conn.lock().execute(
            "UPDATE work_items SET state=?1, result_json=?2, error=?3, finished_at=?4
             WHERE id=?5 AND state='running'",
            params![to.as_str(), result_json, error, finished_at, id],
        )?;
        Ok(n > 0)
    }

    /// On boot, fail any item left `running` by a crash — never silently re-run
    /// a mutating action (2.5 durability). Queued items are untouched (they just
    /// run when a scheduler next claims them). Returns how many were reconciled.
    pub fn terminalize_orphaned_work(&self, now: i64) -> Result<usize> {
        let n = self.conn.lock().execute(
            "UPDATE work_items SET state='failed', error='interrupted_by_crash', finished_at=?1
             WHERE state='running'",
            params![now],
        )?;
        Ok(n)
    }

    pub fn get_work_item(&self, id: &str) -> Result<Option<crate::queue::WorkItem>> {
        Ok(self
            .conn
            .lock()
            .query_row(
                "SELECT id, kind, state, source_ref, input_json, result_json, error,
                        scheduled_at, claim_key, idempotency_key, attempts,
                        target_conversation_id, created_at, started_at, finished_at
                 FROM work_items WHERE id = ?1",
                params![id],
                row_to_work_item,
            )
            .optional()?)
    }

    /// C2: look up the (at-most-one, thanks to the partial UNIQUE index) work
    /// item carrying this `claim_key` — the durability journal's
    /// find-prior-attempt primitive.
    pub fn find_work_item_by_claim_key(&self, claim_key: &str) -> Result<Option<crate::queue::WorkItem>> {
        Ok(self
            .conn
            .lock()
            .query_row(
                "SELECT id, kind, state, source_ref, input_json, result_json, error,
                        scheduled_at, claim_key, idempotency_key, attempts,
                        target_conversation_id, created_at, started_at, finished_at
                 FROM work_items WHERE claim_key = ?1",
                params![claim_key],
                row_to_work_item,
            )
            .optional()?)
    }

    /// C2: begin (or retry) a journal attempt for THIS row id: `queued → running`
    /// (fresh attempt) or `failed → running` (a legitimate retry after a failed /
    /// crash-interrupted attempt — the effect didn't complete, re-running IS the
    /// recovery). Bumps `attempts`, clears the stale error/finish stamps.
    /// Refuses (`Ok(false)`) for `done` (a recorded success is immutable — the
    /// caller replays it instead) and `running` (an in-flight double-fire).
    pub fn start_work_attempt(&self, id: &str, now: i64) -> Result<bool> {
        let n = self.conn.lock().execute(
            "UPDATE work_items
             SET state='running', attempts=attempts+1, started_at=?2,
                 error=NULL, finished_at=NULL
             WHERE id=?1 AND state IN ('queued','failed')",
            params![id, now],
        )?;
        Ok(n > 0)
    }

    // ── trm_logs ────────────────────────────────────────────────────────────

    pub fn insert_trm_log(&self, l: &TrmLog) -> Result<()> {
        self.conn.lock().execute(
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
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
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
        let n = self.conn.lock().execute(
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
        self.conn.lock().execute(
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
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
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

    // ── tool_rules (Q8 — persisted Always grants) ─────────────────────────
    //
    // Unlike tool_audit this is NOT append-only: a rule is a live policy the
    // user can revoke. `UNIQUE(tool_name, pattern)` + `INSERT OR REPLACE`
    // makes re-adding the same (tool, pattern) idempotent (updates the action
    // + timestamp instead of piling duplicate rows).

    /// Upsert a tool rule (keyed on `(tool_name, pattern)`). Returns `Err` on
    /// a real DB failure — the caller MUST surface it (a rule is an
    /// authorization the user relies on, never best-effort telemetry).
    pub fn add_tool_rule(&self, row: &ToolRuleRow) -> Result<()> {
        self.conn
            .lock()
            .execute(
                "INSERT OR REPLACE INTO tool_rules
                 (id, tool_name, pattern, action, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![row.id, row.tool_name, row.pattern, row.action, row.created_at],
            )
            .context("insert tool_rules row")?;
        Ok(())
    }

    /// All rules for one tool, newest-first. The hot read path
    /// (`SqlitePolicySource::rules_for`) — one indexed lookup per gating pass.
    pub fn list_tool_rules_for(&self, tool_name: &str) -> Result<Vec<ToolRuleRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, tool_name, pattern, action, created_at
             FROM tool_rules WHERE tool_name = ?1 ORDER BY created_at DESC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![tool_name], Self::map_tool_rule)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("query tool_rules rows for tool")?;
        Ok(rows)
    }

    /// Every rule in this profile, newest-first — for the Settings pane.
    pub fn list_tool_rules(&self) -> Result<Vec<ToolRuleRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, tool_name, pattern, action, created_at
             FROM tool_rules ORDER BY created_at DESC, id ASC",
        )?;
        let rows = stmt
            .query_map([], Self::map_tool_rule)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("query all tool_rules rows")?;
        Ok(rows)
    }

    /// Revoke a rule by id. Returns `true` if a row was actually removed.
    pub fn delete_tool_rule(&self, id: &str) -> Result<bool> {
        let n = self
            .conn
            .lock()
            .execute("DELETE FROM tool_rules WHERE id = ?1", params![id])
            .context("delete tool_rules row")?;
        Ok(n > 0)
    }

    fn map_tool_rule(row: &rusqlite::Row<'_>) -> rusqlite::Result<ToolRuleRow> {
        Ok(ToolRuleRow {
            id: row.get(0)?,
            tool_name: row.get(1)?,
            pattern: row.get(2)?,
            action: row.get(3)?,
            created_at: row.get(4)?,
        })
    }

    // ── classifier_settings (PLAN §11 — per-profile thresholds) ───────────────
    //
    // A single row (id=1). Absence = defaults. Reads always `sanitize` so a
    // corrupt/hand-edited row can only ever make the filter STRICTER, never
    // leakier (see `ClassifierConfig::sanitized`).

    /// The active classifier thresholds for this profile. Returns
    /// [`ClassifierConfig::default`] when no row exists (never errors on
    /// "unset"); a real DB error propagates.
    pub fn classifier_config(&self) -> Result<crate::classifier::ClassifierConfig> {
        use crate::classifier::ClassifierConfig;
        let row: Option<(f64, f64)> = self
            .conn
            .lock()
            .query_row(
                "SELECT tau_block, tau_band FROM classifier_settings WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .context("read classifier_settings row")?;
        Ok(match row {
            Some((tau_block, tau_band)) => ClassifierConfig {
                tau_block: tau_block as f32,
                tau_band: tau_band as f32,
            }
            .sanitized(),
            None => ClassifierConfig::default(),
        })
    }

    /// Persist this profile's classifier thresholds (upsert the single row).
    /// The value is `sanitized` before storage so the table never holds an
    /// out-of-range/leakier-than-valid threshold. Column-scoped `ON CONFLICT`
    /// updates ONLY the thresholds — the `redaction_enabled` flag on the same
    /// row is preserved (a threshold change must not silently flip redaction).
    pub fn set_classifier_config(&self, cfg: &crate::classifier::ClassifierConfig) -> Result<()> {
        let cfg = cfg.sanitized();
        let now = chrono::Utc::now().timestamp();
        self.conn
            .lock()
            .execute(
                "INSERT INTO classifier_settings (id, tau_block, tau_band, updated_at)
                 VALUES (1, ?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET
                     tau_block = excluded.tau_block,
                     tau_band = excluded.tau_band,
                     updated_at = excluded.updated_at",
                params![cfg.tau_block as f64, cfg.tau_band as f64, now],
            )
            .context("upsert classifier_settings thresholds")?;
        Ok(())
    }

    /// Clear this profile's classifier settings (revert thresholds AND the
    /// redaction toggle to defaults). Returns `true` if a row was removed.
    pub fn reset_classifier_config(&self) -> Result<bool> {
        let n = self
            .conn
            .lock()
            .execute("DELETE FROM classifier_settings WHERE id = 1", [])
            .context("delete classifier_settings row")?;
        Ok(n > 0)
    }

    // ── sandbox_config (M7 Tier-K Slice 2 — per-profile OS-sandbox config) ─────
    //
    // A single row (id=1) holding `SandboxConfig` as JSON. `None` = no row = the
    // legacy unconstrained default (the caller keeps today's behavior). A row
    // that exists but fails to parse is a hard Err — the shell path treats that
    // as fail-safe (deny), never as "unconstrained".

    /// This profile's stored sandbox config, or `None` if unset. A corrupt row
    /// is an `Err` (the caller must fail safe, not silently drop the ceiling).
    pub fn get_sandbox_config(&self) -> Result<Option<crate::hooks::SandboxConfig>> {
        let json: Option<String> = self
            .conn
            .lock()
            .query_row(
                "SELECT config_json FROM sandbox_config WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .optional()
            .context("read sandbox_config row")?;
        match json {
            None => Ok(None),
            Some(json) => {
                let cfg: crate::hooks::SandboxConfig = serde_json::from_str(&json)
                    .context("parse sandbox_config JSON (corrupt row — shell fails safe)")?;
                Ok(Some(cfg))
            }
        }
    }

    /// Persist this profile's sandbox config (upsert the single JSON row).
    pub fn set_sandbox_config(&self, cfg: &crate::hooks::SandboxConfig) -> Result<()> {
        let json = serde_json::to_string(cfg).context("serialize sandbox_config")?;
        let now = chrono::Utc::now().timestamp();
        self.conn
            .lock()
            .execute(
                "INSERT INTO sandbox_config (id, config_json, updated_at)
                 VALUES (1, ?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET
                     config_json = excluded.config_json,
                     updated_at = excluded.updated_at",
                params![json, now],
            )
            .context("upsert sandbox_config row")?;
        Ok(())
    }

    /// Whether partial-delegation redaction is enabled for this profile (PLAN
    /// §11). Defaults to `true` (redaction is the privacy-preserving default)
    /// when no row exists.
    pub fn redaction_enabled(&self) -> Result<bool> {
        let v: Option<i64> = self
            .conn
            .lock()
            .query_row(
                "SELECT redaction_enabled FROM classifier_settings WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .optional()
            .context("read redaction_enabled")?;
        Ok(v.map(|n| n != 0).unwrap_or(true))
    }

    /// Set this profile's redaction toggle. Column-scoped `ON CONFLICT` updates
    /// ONLY the flag — the thresholds on the same row are preserved. On a fresh
    /// insert (no row yet) the thresholds take their column defaults.
    pub fn set_redaction_enabled(&self, enabled: bool) -> Result<()> {
        use crate::classifier::ClassifierConfig;
        let d = ClassifierConfig::default();
        let now = chrono::Utc::now().timestamp();
        self.conn
            .lock()
            .execute(
                "INSERT INTO classifier_settings (id, tau_block, tau_band, redaction_enabled, updated_at)
                 VALUES (1, ?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET
                     redaction_enabled = excluded.redaction_enabled,
                     updated_at = excluded.updated_at",
                params![d.tau_block as f64, d.tau_band as f64, enabled as i64, now],
            )
            .context("upsert classifier_settings redaction toggle")?;
        Ok(())
    }

    // ── memory_settings (Wave 1 — per-profile memory toggles) ─────────────────
    //
    // A single row (id=1). Absence = defaults: semantic search ON (hybrid
    // memory as before), NOT walled (memory lives in the shared global.db).

    /// This profile's memory settings, defaulting when no row exists.
    pub fn memory_settings(&self) -> Result<MemorySettings> {
        let row: Option<(i64, i64)> = self
            .conn
            .lock()
            .query_row(
                "SELECT semantic_search_enabled, walled FROM memory_settings WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .context("read memory_settings row")?;
        Ok(match row {
            Some((semantic, walled)) => MemorySettings {
                semantic_search_enabled: semantic != 0,
                walled: walled != 0,
            },
            None => MemorySettings::default(),
        })
    }

    /// Persist this profile's memory settings (upsert the single row).
    pub fn set_memory_settings(&self, s: &MemorySettings) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .lock()
            .execute(
                "INSERT INTO memory_settings
                     (id, semantic_search_enabled, walled, updated_at)
                 VALUES (1, ?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET
                     semantic_search_enabled = excluded.semantic_search_enabled,
                     walled = excluded.walled,
                     updated_at = excluded.updated_at",
                params![s.semantic_search_enabled as i64, s.walled as i64, now],
            )
            .context("upsert memory_settings")?;
        Ok(())
    }

    // ── seat_bindings (Wave 3.1 — per-profile model seats) ───────────────────

    /// Bind a (user-defined) seat name to a concrete provider+model for THIS
    /// profile. Upsert — re-binding a seat just changes what it resolves to.
    pub fn set_seat_binding(&self, seat: &str, provider_id: &str, model: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        self.conn
            .lock()
            .execute(
                "INSERT INTO seat_bindings (seat, provider_id, model, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(seat) DO UPDATE SET
                     provider_id = excluded.provider_id,
                     model = excluded.model,
                     updated_at = excluded.updated_at",
                params![seat.trim(), provider_id, model, now],
            )
            .context("upsert seat_binding")?;
        Ok(())
    }

    /// The binding for `seat` in this profile, if any. A missing row is normal
    /// (an unbound seat inherits the caller's model — see `resolve_seat`).
    pub fn get_seat_binding(&self, seat: &str) -> Result<Option<SeatBinding>> {
        Ok(self
            .conn
            .lock()
            .query_row(
                "SELECT seat, provider_id, model, updated_at FROM seat_bindings WHERE seat = ?1",
                params![seat.trim()],
                row_to_seat_binding,
            )
            .optional()
            .context("get_seat_binding")?)
    }

    /// Every seat binding for this profile (for the Settings → Seats view).
    pub fn list_seat_bindings(&self) -> Result<Vec<SeatBinding>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT seat, provider_id, model, updated_at FROM seat_bindings ORDER BY seat",
        )?;
        let rows = stmt
            .query_map([], row_to_seat_binding)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Unbind a seat (it then resolves to `inherit`). Returns whether a row went.
    pub fn delete_seat_binding(&self, seat: &str) -> Result<bool> {
        let n = self
            .conn
            .lock()
            .execute("DELETE FROM seat_bindings WHERE seat = ?1", params![seat.trim()])?;
        Ok(n > 0)
    }
}

fn row_to_seat_binding(r: &rusqlite::Row<'_>) -> rusqlite::Result<SeatBinding> {
    Ok(SeatBinding {
        seat: r.get(0)?,
        provider_id: r.get(1)?,
        model: r.get(2)?,
        updated_at: r.get(3)?,
    })
}

/// A per-profile model-seat binding (Wave 3.1). `seat` is a user-defined name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatBinding {
    pub seat: String,
    pub provider_id: String,
    pub model: String,
    pub updated_at: i64,
}

/// Per-profile memory toggles (Wave 1). Defaults preserve the pre-Wave-1
/// behavior exactly: semantic (meaning-lane) search on, memory shared in
/// `global.db`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemorySettings {
    /// Whether the meaning-lane embedder is loaded + used (PLAN §9). Off ⇒
    /// memory search runs keyword-only and no embeddings are computed.
    pub semantic_search_enabled: bool,
    /// The §7 "keep this profile's memory private" island. When set, this
    /// profile's memory reads/writes route to its own physically-separate
    /// memory DB instead of the shared `global.db`.
    pub walled: bool,
}

impl Default for MemorySettings {
    fn default() -> Self {
        Self {
            semantic_search_enabled: true,
            walled: false,
        }
    }
}

/// One match from [`ProfileDb::search_messages`] — a past transcript turn whose
/// content contained the query.
#[derive(Debug, Clone)]
pub struct SessionSearchHit {
    pub conversation_id: String,
    /// The conversation's display name, if it still exists.
    pub conversation_name: Option<String>,
    /// `"user"` or `"assistant"`.
    pub role: String,
    pub content: String,
    pub created_at: i64,
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

fn row_to_work_item(r: &rusqlite::Row<'_>) -> rusqlite::Result<crate::queue::WorkItem> {
    let kind_s: String = r.get(1)?;
    let state_s: String = r.get(2)?;
    let kind = crate::queue::WorkKind::from_str(&kind_s)
        .ok_or_else(|| rusqlite::Error::InvalidColumnName(format!("bad work_items.kind: {kind_s}")))?;
    let state = crate::queue::WorkState::from_str(&state_s)
        .ok_or_else(|| rusqlite::Error::InvalidColumnName(format!("bad work_items.state: {state_s}")))?;
    Ok(crate::queue::WorkItem {
        id: r.get(0)?,
        kind,
        state,
        source_ref: r.get(3)?,
        input_json: r.get(4)?,
        result_json: r.get(5)?,
        error: r.get(6)?,
        scheduled_at: r.get(7)?,
        claim_key: r.get(8)?,
        idempotency_key: r.get(9)?,
        attempts: r.get(10)?,
        target_conversation_id: r.get(11)?,
        created_at: r.get(12)?,
        started_at: r.get(13)?,
        finished_at: r.get(14)?,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Storage;

    fn temp_profile() -> (Storage, std::sync::Arc<ProfileDb>, std::path::PathBuf) {
        let mut root = std::env::temp_dir();
        root.push(format!("lhp-usage-{}", uuid::Uuid::new_v4()));
        let storage = Storage::open(&root).unwrap();
        let db = storage.open_profile("personal").unwrap();
        (storage, db, root)
    }

    fn ev(kind: &str, cost: Option<f64>) -> UsageEvent {
        UsageEvent {
            id: uuid::Uuid::new_v4().to_string(),
            conversation_id: Some("c1".into()),
            model: "some-model".into(),
            provider_id: Some("p1".into()),
            provider_kind: kind.into(),
            cost_usd: cost,
            created_at: 100,
        }
    }

    #[test]
    fn usage_ledger_sums_known_cost_and_counts_flying_blind() {
        let (_storage, db, root) = temp_profile();

        // Fresh ledger is empty.
        let s = db.usage_summary().unwrap();
        assert_eq!(s, UsageSummary { total_calls: 0, known_cost_usd: 0.0, unknown_cost_calls: 0 });

        // Two local ($0), one priced cloud, two unknown-cost cloud calls.
        db.record_usage(&ev("local", Some(0.0))).unwrap();
        db.record_usage(&ev("local", Some(0.0))).unwrap();
        db.record_usage(&ev("cloud", Some(0.42))).unwrap();
        db.record_usage(&ev("cloud", None)).unwrap();
        db.record_usage(&ev("cloud", None)).unwrap();

        let s = db.usage_summary().unwrap();
        assert_eq!(s.total_calls, 5);
        // Known cost = 0 + 0 + 0.42; the two None calls are NOT guessed into it.
        assert!((s.known_cost_usd - 0.42).abs() < 1e-9, "known cost = {}", s.known_cost_usd);
        assert_eq!(s.unknown_cost_calls, 2, "the two None cloud calls are flagged, not summed");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn work_item_enqueue_claim_finish_lifecycle() {
        use crate::queue::{WorkItem, WorkKind, WorkState};
        let (_storage, db, root) = temp_profile();

        // Enqueue two items; a claim_key dedups the second identical enqueue.
        let mut a = WorkItem::queued(WorkKind::Cron, r#"{"prompt":"a"}"#, 100);
        a.claim_key = Some("cron:job1@1000".into());
        a.scheduled_at = Some(1000);
        assert!(db.insert_work_item(&a).unwrap(), "first enqueue inserts");
        let mut dup = WorkItem::queued(WorkKind::Cron, r#"{"prompt":"a"}"#, 101);
        dup.claim_key = Some("cron:job1@1000".into()); // same key → deduped
        assert!(!db.insert_work_item(&dup).unwrap(), "same claim_key is a dedup no-op");

        let b = WorkItem::queued(WorkKind::AgentDispatch, r#"{"prompt":"b"}"#, 50);
        assert!(db.insert_work_item(&b).unwrap());

        // Nothing due before scheduled_at for `a`, but `b` (no schedule) is due.
        let claimed = db.claim_next_due_work(999).unwrap().expect("b is due");
        assert_eq!(claimed.id, b.id, "the unscheduled item claims first");
        assert_eq!(claimed.state, WorkState::Running);
        assert_eq!(claimed.attempts, 1, "claim stamps an attempt");

        // A second claim now gets nothing (a is not due until 1000).
        assert!(db.claim_next_due_work(999).unwrap().is_none());
        // At/after 1000, `a` becomes claimable.
        let claimed_a = db.claim_next_due_work(1000).unwrap().expect("a now due");
        assert_eq!(claimed_a.id, a.id);

        // Finish `b` (running) → done; a re-finish is a no-op (already terminal).
        assert!(db.finish_work_item(&b.id, WorkState::Done, Some(r#"{"ok":true}"#), None, 200).unwrap());
        assert!(!db.finish_work_item(&b.id, WorkState::Failed, None, Some("x"), 300).unwrap(),
            "a terminal item can't be re-finished");
        let got = db.get_work_item(&b.id).unwrap().unwrap();
        assert_eq!(got.state, WorkState::Done);
        assert_eq!(got.result_json.as_deref(), Some(r#"{"ok":true}"#));

        // An illegal transition (running -> queued) is rejected.
        assert!(db.finish_work_item(&a.id, WorkState::Queued, None, None, 400).is_err());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn terminalize_orphaned_work_fails_only_running_items() {
        use crate::queue::{WorkItem, WorkKind, WorkState};
        let (_storage, db, root) = temp_profile();

        let q = WorkItem::queued(WorkKind::Cron, "{}", 1);
        db.insert_work_item(&q).unwrap();
        let r = WorkItem::queued(WorkKind::AgentDispatch, "{}", 1);
        db.insert_work_item(&r).unwrap();
        // Claim ONE (both are due; which one is a tie) so it's mid-run, then
        // simulate a crash + boot reconcile.
        let claimed_id = db.claim_next_due_work(10).unwrap().unwrap().id;
        let other_id = if claimed_id == r.id { q.id.clone() } else { r.id.clone() };
        let n = db.terminalize_orphaned_work(500).unwrap();
        assert_eq!(n, 1, "only the running item is reconciled");
        assert_eq!(db.get_work_item(&claimed_id).unwrap().unwrap().state, WorkState::Failed);
        // The still-queued item is untouched (it will run later).
        assert_eq!(db.get_work_item(&other_id).unwrap().unwrap().state, WorkState::Queued);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn usage_events_are_profile_isolated() {
        let (storage, personal, root) = temp_profile();
        personal.record_usage(&ev("cloud", None)).unwrap();

        // A different profile's ledger is independent.
        let work = storage.open_profile("work").unwrap();
        assert_eq!(work.usage_summary().unwrap().total_calls, 0);
        assert_eq!(personal.usage_summary().unwrap().total_calls, 1);

        let _ = std::fs::remove_dir_all(root);
    }
}
