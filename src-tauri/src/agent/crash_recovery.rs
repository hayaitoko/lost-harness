//! Crash-recovery boot pass (Q3, do-now item 4).
//!
//! On every core init, before any conversation can be touched, reconcile the
//! one kind of state this codebase can actually leave dangling after an
//! unclean shutdown — an `assistant` turn that opened a tool call whose
//! result was never persisted — by writing a durable, transcript-visible
//! "tool interrupted" message row.
//!
//! **Why this is a boot pass, not a per-turn check.** Detection requires
//! comparing the last message of every conversation on disk to the "did
//! the tool reply arrive" invariant. That scan is cheap, runs once at
//! startup, and gets ahead of the agent loop so a user opening the app
//! after a crash sees the explanation in the transcript immediately
//! instead of only after sending a new message.
//!
//! **Why a `tool` role for the repair row, not a fresh `assistant`
//! apology.** A `tool` row closes the dangling tool call — the
//! conversation's last message is no longer an unanswered `assistant`
//! turn asking for a tool to run. The next assistant reply (when the user
//! sends a new prompt) sees a clean state: `user → assistant(tool
//! call) → tool(interrupted) → user(new) → assistant(new)`. The agent
//! loop treats a `role="tool"` row as a tool result it can summarize
//! just like any other tool result.
//!
//! **Idempotent by construction, no extra flag needed.** The repair row
//! has `role: "tool"`, so on a second boot pass the conversation's last
//! message is `tool`, not `assistant` — it's skipped automatically. No
//! "already_reconciled" marker to drift.
//!
//! **Three explicit non-goals** (per the build-plan Invariants): we do NOT
//! touch (a) a conversation whose last message is `role: "user"` with
//! no assistant reply, (b) one whose last message is `role: "tool"`
//! with no follow-up assistant reply, or (c) an assistant turn the agent
//! loop itself marked `aborted: true` — today that means a deliberate
//! stop at the tool-round budget (`MAX_TOOL_ROUNDS`), which leaves an open
//! fence with no completing tool row that is otherwise byte-identical to a
//! crash. All three are normal states, not crash damage — the user is
//! waiting on a reply / the model hasn't answered / the loop stopped on
//! purpose. The detection rule is narrowly: "assistant + open `tool`
//! fence + NOT already marked `aborted`, nothing after." A genuine crash
//! can never carry `aborted: true` (the process dies before that row is
//! written), so this cleanly separates the deliberate stop from the crash.

use anyhow::{Context, Result};
use rusqlite::params;
use uuid::Uuid;

use crate::storage::{Conversation, Message, ProfileDb, Storage};
use crate::tools::calling::contains_open_tool_fence;

/// String tag persisted in `messages.error` for the repair row. Stable —
/// UI or downstream tooling can branch on it without parsing content.
pub const INTERRUPTED_ERROR_TAG: &str = "interrupted_by_crash";

/// Routing-decision label written on the repair row. Distinguishes the
/// row from a real tool result and from the model's own error rows.
const REPAIR_ROUTING_DECISION: &str = "crash_recovery";

/// Verbatim content the repair row puts in the transcript. Loud, plain
/// English, no model-isms — the user has to read this without an LLM in
/// the loop. The bracketed `[tool interrupted]` prefix lets the UI
/// surface it as a distinct event without parsing free-form text.
const REPAIR_CONTENT: &str = "[tool interrupted] The app closed or crashed before this tool call \
                              could run or return a result. No tool ran and nothing changed. \
                              Ask again if you still need this action.";

/// Summary of one boot-pass run. Returned (and logged) so a future
/// caller (CLI, IPC) can surface "reconciled N interrupted tool calls"
/// without re-walking the DBs. `interrupted` is `(profile_name,
/// conversation_id)`; `profile_errors` is `(profile_name, error)` for
/// profiles that failed to open OR failed to reconcile — `run_boot_pass`
/// never aborts the rest of the pass on a per-profile failure.
#[derive(Debug, Default, Clone)]
pub struct CrashRecoveryReport {
    pub profiles_scanned: usize,
    pub interrupted: Vec<(String, String)>,
    pub profile_errors: Vec<(String, String)>,
}

/// Reconcile ONE already-open profile DB in a single transaction. Returns
/// the ids of conversations that were terminalized. Exposed at this
/// granularity so tests can drive it directly against
/// `ProfileDb::open_in_memory` with no `Storage`/tempdir needed.
///
/// **Why this inlines SQL instead of calling `db.list_conversations()` /
/// `db.list_messages_by_conversation()` / `db.add_message()`.**
/// `ProfileDb`'s connection is a `parking_lot::Mutex<Connection>`, which is
/// NOT reentrant. `db.raw()` below locks it for this whole function so the
/// transaction is a genuine critical section; re-entering any of
/// `ProfileDb`'s own methods while that guard is held would try to lock the
/// same mutex again on this thread and deadlock. So this function locks
/// once, opens the transaction on that single guard, and runs the exact
/// same queries those methods use directly against the transaction.
pub(crate) fn reconcile_profile_db(db: &ProfileDb) -> Result<Vec<String>> {
    let conn = db.raw();
    let tx = conn
        .unchecked_transaction()
        .context("crash-recovery: starting transaction")?;
    let mut terminalized = Vec::new();

    // Same query as `ProfileDb::list_conversations`. Bind the mapped rows to
    // a name (`rows`) rather than tail-returning `.collect()` directly —
    // otherwise the borrow-checker sees `stmt` as dropped before a temporary
    // that (structurally) still refers to it, even though the temporary is
    // fully consumed by `.collect()` (a well-known false-positive shape for
    // a block whose tail expression borrows an earlier local).
    let conversations: Vec<Conversation> = {
        let mut stmt = tx
            .prepare(
                "SELECT id, name, pinned, binding, folder_id, color, created_at, updated_at
                 FROM conversations
                 ORDER BY pinned DESC, updated_at DESC",
            )
            .context("crash-recovery: preparing conversations query")?;
        let rows = stmt
            .query_map([], |r| {
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
            })
            .context("crash-recovery: listing conversations")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("crash-recovery: listing conversations")?;
        rows
    };

    for conv in conversations {
        // Same query as `ProfileDb::list_messages_by_conversation`.
        let msgs: Vec<Message> = {
            let mut stmt = tx
                .prepare(
                    "SELECT id, conversation_id, role, content, model, provider_id,
                            routing_decision, thinking_content, error, aborted, created_at
                     FROM messages WHERE conversation_id = ?1
                     ORDER BY created_at ASC, rowid ASC",
                )
                .context("crash-recovery: preparing messages query")?;
            let rows = stmt
                .query_map(params![conv.id], |r| {
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
                })
                .context("crash-recovery: loading messages")?
                .collect::<rusqlite::Result<Vec<_>>>()
                .context("crash-recovery: loading messages")?;
            rows
        };
        let Some(last) = msgs.last() else {
            continue;
        };
        // Only an assistant message that opened a tool call and got no
        // reply is "non-terminal" in this codebase. Plain-text final
        // answers (no fence), already-completed tool rounds (last is
        // role="tool"), dangling user messages (last is role="user"), and
        // turns the agent loop DELIBERATELY stopped at the tool-round budget
        // (marked `aborted: true` on write — see `loop_mod.rs`) are all
        // normal states, not crash damage — see module docs. A genuine
        // crash can never carry `aborted: true`, because the process dies
        // before the row that would set it is ever written.
        if last.role != "assistant" || last.aborted || !contains_open_tool_fence(&last.content) {
            continue;
        }
        let repair = Message {
            id: Uuid::new_v4().to_string(),
            conversation_id: conv.id.clone(),
            role: "tool".to_string(),
            content: REPAIR_CONTENT.to_string(),
            model: None,
            provider_id: None,
            routing_decision: Some(REPAIR_ROUTING_DECISION.to_string()),
            thinking_content: None,
            error: Some(INTERRUPTED_ERROR_TAG.to_string()),
            aborted: true,
            created_at: chrono::Utc::now().timestamp(),
        };
        // Same query as `ProfileDb::add_message`.
        tx.execute(
            "INSERT INTO messages
             (id, conversation_id, role, content, model, provider_id,
              routing_decision, thinking_content, error, aborted, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                repair.id,
                repair.conversation_id,
                repair.role,
                repair.content,
                repair.model,
                repair.provider_id,
                repair.routing_decision,
                repair.thinking_content,
                repair.error,
                repair.aborted as i64,
                repair.created_at
            ],
        )
        .context("crash-recovery: persisting interrupted-tool event")?;
        terminalized.push(conv.id.clone());

        // TODO(item 5, once tool_audit exists): also insert an audit row
        // here with outcome = "interrupted". Not required for this
        // item's acceptance criteria — the message row above is already
        // a durable, visibly-reported event on its own.
    }

    // Expire persisted pending-approval artifacts. No-op today:
    // ApprovalLedger (hooks/approval.rs) and ApprovalRegistry
    // (ipc/approval.rs) are in-memory only — see the "No half-durability"
    // note in hooks/approval.rs's module doc — so there is nothing
    // persisted to expire yet. Kept as an explicit, named step so this
    // pass already has the right shape once a persisted artifact exists.

    tx.commit().context("crash-recovery: committing transaction")?;
    Ok(terminalized)
}

/// Run once at core init, across every profile on disk, before anything
/// else touches storage. Never `?`-propagates from the caller — a boot
/// pass failure must not brick app boot (see build-plan Invariants).
pub fn run_boot_pass(storage: &Storage) -> Result<CrashRecoveryReport> {
    let mut report = CrashRecoveryReport::default();
    let names = storage
        .list_profile_names()
        .context("crash-recovery: listing profiles")?;
    for name in names {
        report.profiles_scanned += 1;
        let db = match storage.open_profile(&name) {
            Ok(db) => db,
            Err(e) => {
                tracing::error!(
                    profile = %name,
                    error = %e,
                    "crash-recovery: could not open profile; skipping"
                );
                report.profile_errors.push((name, e.to_string()));
                continue;
            }
        };
        match reconcile_profile_db(&db) {
            Ok(ids) => {
                report
                    .interrupted
                    .extend(ids.into_iter().map(|id| (name.clone(), id)));
            }
            Err(e) => {
                tracing::error!(
                    profile = %name,
                    error = %e,
                    "crash-recovery: reconciliation failed; skipping profile"
                );
                report.profile_errors.push((name.clone(), e.to_string()));
            }
        }

        // Wave 4.4: also reconcile any `work_items` (e.g. a `delegate`
        // dispatch) a crash left `running` — never silently re-run a
        // mutating/dispatched action (2.5 durability). Independent of the
        // message-transcript repair above; best-effort, log-and-continue like
        // every other step in this pass.
        if let Err(e) = db.terminalize_orphaned_work(chrono::Utc::now().timestamp()) {
            tracing::error!(
                profile = %name,
                error = %e,
                "crash-recovery: terminalizing orphaned work_items failed"
            );
            report.profile_errors.push((name, e.to_string()));
        }
    }
    Ok(report)
}
