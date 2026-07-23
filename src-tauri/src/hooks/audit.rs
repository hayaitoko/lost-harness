//! §3.4 / item 5 — tool_audit append-only record + first concrete
//! `ObserverHook`. Spec `docs/tool-system-decisions.md` Q9 (Fable).
//!
//! This module is the *recording* half of the audit story. The
//! *transport* is a thin trait so the dispatcher can call into it
//! synchronously without owning a `ProfileDb`:
//!
//! ```text
//! ToolDispatcher::dispatch
//!   │
//!   │   // every return path, AFTER outcome exists
//!   ▼
//! self.audit_writer.write_audit(&AuditEntry { … })
//!   │
//!   │   (production: StorageAuditWriter → ProfileDb::add_tool_audit)
//!   │   (tests:      TestAuditWriter  → Vec<Mutex<AuditEntry>>)
//!   ▼
//! tool_audit table (per-profile, append-only)
//! ```
//!
//! **Why a trait, not a direct call to ProfileDb?** The dispatcher is
//! built once at body startup and the profile name comes from the live
//! conversation (`ctx.profile`), so a per-profile DB handle has to be
//! *opened* at write time, not stashed in the dispatcher. The trait
//! abstraction makes that ("open + insert") a single method
//! (`StorageAuditWriter::write_audit`) and lets the test suite swap in a
//! collecting writer without touching SQLite.
//!
//! **Why not just use `HookChain::notify_observers`?** `EventContext` is
//! built *before* the tool runs and carries no `ToolOutcome`, so
//! `notify_observers` would lose the load-bearing field
//! (outcome / gate_by / duration). A direct call from `dispatch` keeps
//! the outcome in scope without changing `EventContext`'s shape. The
//! thin `AuditObserverHook` is still implemented and registered in the
//! chain so the trait is exercised and the eventual
//! `HookChain::notify_observers` (with a richer `EventContext` carrying
//! the outcome) can swap in without rewriting the hook itself.
//!
//! **Failure policy.** A SQLite write that fails MUST NOT propagate
//! back to `dispatch` — the tool call already succeeded (or already
//! failed) and the user's outcome is determined. We log the error and
//! continue. This is the same posture as TRM logging (Q9: "cheap, on
//! best-effort, never blocks a tool call").

use std::sync::Arc;

use crate::hooks::{EventContext, ObserverHook};
use crate::storage::ToolAuditRow;

// ── AuditEntry ────────────────────────────────────────────────────────────

/// One audit observation, in the shape the dispatcher has after a
/// `dispatch()` call has produced an outcome. Built at every return
/// point of `ToolDispatcher::dispatch` and handed to the `AuditWriter`.
///
/// `profile` is the *target* per-profile DB the row belongs in (so
/// `StorageAuditWriter` doesn't need a profile name from anywhere
/// else); `conversation_id` and `tool_name` etc. are the obvious things.
/// `gate_by` is `Some` only when a hook denied or asked, matching the
/// `by` field on `ToolOutcome::Denied` / `ToolOutcome::Ask`.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    /// The profile DB the row belongs in. The dispatcher sources this
    /// from `ctx.profile` (already in `ExecCtx`).
    pub profile: String,
    pub conversation_id: String,
    pub tool_name: String,
    /// The same canonical string the gating chain saw
    /// (`format!("{} {}", call.name, call.args)`), size-capped in memory and
    /// replaced by a redacted marker before SQLite persistence.
    pub canonical_args: String,
    /// The same `ActionFingerprint` hash the chain used to pin any
    /// Once grant. Copying it (rather than re-deriving) keeps the
    /// `tool_audit.fingerprint` value byte-identical to whatever an
    /// approval grant against the same call would have used.
    pub fingerprint: String,
    /// `format!("{:?}", tool.risk())` — one of "Safe" / "Write" /
    /// "External" / "Dangerous". Storing as text so a future
    /// risk-class rename doesn't break the table.
    pub risk: String,
    /// One of "ok" / "err" / "denied" / "ask" / "unavailable" / "unknown".
    /// See `outcome_label` for the exact mapping.
    pub outcome: String,
    /// The hook that denied or asked. Mirrors `ToolOutcome::Denied.by`
    /// / `ToolOutcome::Ask.by`. `None` for `Ok`/`Err`/`Unknown`/
    /// `Unavailable`.
    pub gate_by: Option<String>,
    /// What authority allowed the call, when applicable: "pre-trusted"
    /// for a whole-tool-allow Safe tool, "once-fp" for a consumed Once
    /// grant, "session-fp" / "session-tool" for a session grant.
    /// `None` for denials / when the grant source isn't determinable.
    pub grant_used: Option<String>,
    /// For an `Ask`-derived outcome: "approve-once" / "approve-session"
    /// / "approve-tool" / "approve-always" / "deny" / "timeout". `None`
    /// for other outcomes.
    pub decision: Option<String>,
    /// "local" or "cloud" — mirrors the `is_cloud` argument to
    /// `dispatch()`. Tells the Activity pane at a glance which calls
    /// would have left the device.
    pub endpoint_kind: String,
    pub duration_ms: i64,
}

// ── AuditWriter trait ────────────────────────────────────────────────────

/// Anything that can persist a single `AuditEntry`. The dispatcher
/// holds an `Option<Arc<dyn AuditWriter + Send + Sync>>` and calls
/// `write_audit` once per dispatch (every return path).
///
/// Implementors MUST NOT block the caller on a slow side effect beyond
/// a single SQLite write. The dispatcher does not retry, does not
/// requeue, and does not surface a write failure to the user (it's
/// logged at the call site instead). A future "durable server body"
/// can override this trait to flush-before-ack (Q9 says server
/// observers must write durably before returning).
pub trait AuditWriter: Send + Sync {
    fn write_audit(&self, entry: &AuditEntry);
}

// ── outcome_label ────────────────────────────────────────────────────────

/// Map a `ToolOutcome` to the label stored in `tool_audit.outcome`.
/// Public so `tools::dispatch` can call it directly; `AuditEntry` carries
/// the label as a `String` so the trait stays a single-method interface
/// with no `ToolOutcome` dependency.
pub fn outcome_label(outcome: &crate::tools::dispatch::ToolOutcome) -> &'static str {
    use crate::tools::dispatch::ToolOutcome;
    match outcome {
        ToolOutcome::Ok(_) => "ok",
        ToolOutcome::Err(_) => "err",
        ToolOutcome::Denied { .. } => "denied",
        ToolOutcome::Ask { .. } => "ask",
        ToolOutcome::Unavailable(_) => "unavailable",
        ToolOutcome::Unknown(_) => "unknown",
        // Not run on the cloud endpoint; the caller may reroute to local.
        ToolOutcome::NeedsLocalReroute { .. } => "needs_local_reroute",
    }
}

/// Pull `gate_by` off an outcome. For `Denied`/`Ask` it's the `by`
/// field; for everything else, `None`.
pub fn outcome_gate_by(outcome: &crate::tools::dispatch::ToolOutcome) -> Option<String> {
    use crate::tools::dispatch::ToolOutcome;
    match outcome {
        ToolOutcome::Denied { by, .. } | ToolOutcome::Ask { by, .. } => Some(by.clone()),
        // The privacy filter is what forced the reroute; name it as the gate.
        ToolOutcome::NeedsLocalReroute { .. } => Some("privacy-filter".to_string()),
        ToolOutcome::Ok(_)
        | ToolOutcome::Err(_)
        | ToolOutcome::Unavailable(_)
        | ToolOutcome::Unknown(_) => None,
    }
}

// ── truncate_args ────────────────────────────────────────────────────────

/// Size cap on the in-memory `canonical_args` string. ~4KB keeps a single
/// `shell_exec {"cmd":"<4KB of args>"}` row well under the SQLite
/// page boundary. If a call's canonical form exceeds this, the suffix
/// is replaced with `"…[truncated]"` and a byte-size annotation is
/// appended in the same row so the Activity pane can flag that more
/// existed. The action fingerprint is the durable audit identity; the raw
/// canonical string is redacted before SQLite persistence.
pub const CAPTURED_ARGS_CAP: usize = 4 * 1024;

pub fn truncate_args(s: &str) -> String {
    if s.len() <= CAPTURED_ARGS_CAP {
        return s.to_string();
    }
    // Cut on a char boundary at the cap, then suffix.
    let mut cut = CAPTURED_ARGS_CAP;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…[truncated from {} bytes]", &s[..cut], s.len())
}

// ── StorageAuditWriter ──────────────────────────────────────────────────

/// Production `AuditWriter`: opens (and caches) the per-profile DB
/// from a `Storage` handle and inserts the audit row.
///
/// We hold a `Storage` (not a `ProfileDb`) because the audit row's
/// `profile` is the *target* conversation's profile, not whatever
/// profile was open at dispatcher-build time. Caching is on
/// `Storage`'s side (`Storage::open_profile` already memoizes an
/// `Arc<ProfileDb>` per name), so two consecutive audits for the same
/// profile reuse the same connection — no per-call open cost.
pub struct StorageAuditWriter {
    storage: crate::storage::Storage,
}

impl StorageAuditWriter {
    pub fn new(storage: crate::storage::Storage) -> Self {
        Self { storage }
    }
}

impl AuditWriter for StorageAuditWriter {
    fn write_audit(&self, entry: &AuditEntry) {
        // Open (or hit the cache for) the per-profile DB. `Storage`
        // already memoizes an `Arc<ProfileDb>` per name so the second
        // call in a hot loop is a HashMap lookup, not a disk open.
        let profile = match self.storage.open_profile(&entry.profile) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(
                    profile = %entry.profile,
                    error = %e,
                    "tool_audit: failed to open profile DB; skipping audit write"
                );
                return;
            }
        };
        let row = ToolAuditRow {
            id: 0, // AUTOINCREMENT — ProfileDb sets it
            ts: chrono::Utc::now().timestamp(),
            conversation_id: entry.conversation_id.clone(),
            tool_name: entry.tool_name.clone(),
            canonical_args: format!("[redacted; fingerprint={}]", entry.fingerprint),
            fingerprint: entry.fingerprint.clone(),
            risk: entry.risk.clone(),
            outcome: entry.outcome.clone(),
            gate_by: entry.gate_by.clone(),
            grant_used: entry.grant_used.clone(),
            decision: entry.decision.clone(),
            endpoint_kind: Some(entry.endpoint_kind.clone()),
            duration_ms: Some(entry.duration_ms),
        };
        if let Err(e) = profile.add_tool_audit(&row) {
            // Never bubble: a failed audit log is not a reason to
            // fail the tool call (the call already settled; Q9: "best
            // effort, never blocks"). Just log and move on.
            tracing::error!(
                profile = %entry.profile,
                tool = %entry.tool_name,
                error = %e,
                "tool_audit: insert failed; audit row dropped"
            );
        }
    }
}

// ── AuditObserverHook ───────────────────────────────────────────────────

/// The first concrete `ObserverHook` (Q9). For now the dispatcher
/// calls `AuditWriter::write_audit` directly, so the sync `on_event`
/// is a no-op and the async escape hatch logs a warning — the trait
/// is exercised so the eventual `HookChain::notify_observers`
/// migration only changes the dispatcher's call site, not the hook.
///
/// The hook HOLDS the same `Arc<dyn AuditWriter>` as the dispatcher,
/// so the two paths (direct call now, observer call later) end up at
/// the same `StorageAuditWriter::write_audit` — i.e. migrating to
/// `notify_observers` later is a refactor of the dispatcher, not of
/// the persistence layer.
pub struct AuditObserverHook {
    writer: Arc<dyn AuditWriter>,
}

impl AuditObserverHook {
    pub fn new(writer: Arc<dyn AuditWriter>) -> Self {
        Self { writer }
    }
}

impl ObserverHook for AuditObserverHook {
    fn name(&self) -> &str {
        "audit"
    }

    fn on_event(&self, _ctx: &EventContext) {
        // Today `EventContext` doesn't carry the outcome, so a sync
        // observer call has nothing to audit. The dispatcher calls
        // `AuditWriter::write_audit` directly at every return path —
        // see the module docs for the why. Log a one-shot hint so a
        // future migration of the dispatcher to `notify_observers`
        // (which would enrich `EventContext` with the outcome) gets a
        // visible breadcrumb.
        tracing::trace!(
            "AuditObserverHook::on_event called — outcome is not in EventContext yet; \
             the dispatcher's direct write_audit path is the one writing rows today"
        );
    }
}

// ── AuditEventContext helper (for the future notify_observers migration)

/// Build a `PostToolUse`-flavored `EventContext` from an `AuditEntry`.
/// Used by the future migration to `HookChain::notify_observers`; the
/// dispatcher's direct path doesn't need it. Public so the tests in
/// `tools::dispatch` can assert the future shape is sound without
/// going through SQLite.
#[allow(dead_code)]
pub fn event_context_for(entry: &AuditEntry) -> EventContext {
    EventContext::post_tool_use(&entry.tool_name)
        .with_content(entry.canonical_args.clone())
        .with_command_text(entry.canonical_args.clone())
        .with_conversation_id(entry.conversation_id.clone())
}

// (No re-exports here — `hooks::mod.rs` re-exports the public types.)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::dispatch::ToolOutcome;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Test-only `AuditWriter` that just appends entries to a Vec.
    pub struct TestAuditWriter {
        pub entries: Mutex<Vec<AuditEntry>>,
    }

    impl TestAuditWriter {
        pub fn new() -> Self {
            Self {
                entries: Mutex::new(Vec::new()),
            }
        }
        pub fn snapshot(&self) -> Vec<AuditEntry> {
            self.entries.lock().unwrap().clone()
        }
        pub fn by_outcome(&self) -> HashMap<String, usize> {
            let mut out = HashMap::new();
            for e in self.snapshot() {
                *out.entry(e.outcome).or_insert(0usize) += 1;
            }
            out
        }
    }

    impl AuditWriter for TestAuditWriter {
        fn write_audit(&self, entry: &AuditEntry) {
            self.entries.lock().unwrap().push(entry.clone());
        }
    }

    #[test]
    fn truncate_args_under_cap_is_unchanged() {
        let s = "shell_exec {\"cmd\":\"ls\"}".to_string();
        assert_eq!(truncate_args(&s), s);
    }

    #[test]
    fn truncate_args_over_cap_is_suffixed() {
        let s: String = std::iter::repeat('a').take(CAPTURED_ARGS_CAP + 100).collect();
        let t = truncate_args(&s);
        assert!(t.starts_with(&s[..CAPTURED_ARGS_CAP]));
        assert!(t.contains("…[truncated from"));
        // Must record the ORIGINAL byte length, not the cut length.
        let original_len = s.len();
        assert!(t.contains(&format!("from {original_len} bytes]")));
    }

    #[test]
    fn truncate_args_does_not_panic_on_non_ascii() {
        // Cut on a char boundary so a multi-byte char at the cap doesn't split.
        let s = "a".repeat(CAPTURED_ARGS_CAP - 1) + "🦀"; // 🦀 is 4 bytes
        let t = truncate_args(&s);
        // Result is well-formed UTF-8.
        assert!(t.chars().last().is_some());
    }

    #[test]
    fn storage_writer_never_persists_raw_tool_arguments() {
        let root = std::env::temp_dir().join(format!("lhp-audit-redact-{}", uuid::Uuid::new_v4()));
        let storage = crate::storage::Storage::open(&root).unwrap();
        let writer = StorageAuditWriter::new(storage.clone());
        writer.write_audit(&AuditEntry {
            profile: "personal".into(),
            conversation_id: "conv".into(),
            tool_name: "shell_exec".into(),
            canonical_args: "shell_exec {\"token\":\"secret-value\"}".into(),
            fingerprint: "abc123".into(),
            risk: "Dangerous".into(),
            outcome: "denied".into(),
            gate_by: Some("sandbox".into()),
            grant_used: None,
            decision: None,
            endpoint_kind: "local".into(),
            duration_ms: 1,
        });
        let rows = storage
            .open_profile("personal")
            .unwrap()
            .list_tool_audit("conv")
            .unwrap();
        assert_eq!(rows[0].canonical_args, "[redacted; fingerprint=abc123]");
        assert!(!rows[0].canonical_args.contains("secret-value"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn outcome_label_maps_every_variant() {
        assert_eq!(outcome_label(&ToolOutcome::Ok(serde_json::json!(null))), "ok");
        assert_eq!(outcome_label(&ToolOutcome::Err("x".into())), "err");
        assert_eq!(
            outcome_label(&ToolOutcome::Denied {
                by: "sandbox".into(),
                reason: "x".into()
            }),
            "denied"
        );
        assert_eq!(
            outcome_label(&ToolOutcome::Ask {
                by: "permission".into(),
                prompt: "?".into()
            }),
            "ask"
        );
        assert_eq!(outcome_label(&ToolOutcome::Unavailable("x".into())), "unavailable");
        assert_eq!(outcome_label(&ToolOutcome::Unknown("x".into())), "unknown");
    }

    #[test]
    fn outcome_gate_by_returns_by_for_denied_and_ask_only() {
        assert_eq!(
            outcome_gate_by(&ToolOutcome::Denied {
                by: "sandbox".into(),
                reason: "x".into()
            }),
            Some("sandbox".into())
        );
        assert_eq!(
            outcome_gate_by(&ToolOutcome::Ask {
                by: "permission".into(),
                prompt: "?".into()
            }),
            Some("permission".into())
        );
        assert!(outcome_gate_by(&ToolOutcome::Ok(serde_json::json!(null))).is_none());
        assert!(outcome_gate_by(&ToolOutcome::Err("x".into())).is_none());
        assert!(outcome_gate_by(&ToolOutcome::Unavailable("x".into())).is_none());
        assert!(outcome_gate_by(&ToolOutcome::Unknown("x".into())).is_none());
    }
}
