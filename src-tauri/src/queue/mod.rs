//! Wave 4.4 — the one-queue-model substrate. A single persisted work-item
//! abstraction that (as its consumers land) backs cron fires, sub-agent
//! dispatch, and server results — replacing three overlapping deferred-work
//! mechanisms with ONE lifecycle + ONE claim/dedup discipline (PLAN §8 M4;
//! `docs/plans/2026-07-18-wave4-skills-agents.md`).
//!
//! This module is the FOUNDATION only: the [`WorkItem`] shape, the checked
//! [`WorkState`] lifecycle, and (in `storage`) atomic enqueue/claim/finish. The
//! scheduler + the `WorkExecutor`/`ResultSink` traits that actually RUN items
//! arrive with the first consumer (a cron runner / the `delegate` tool) and the
//! `AppHandle`-decoupling refactor — see the plan. Kept consumer-agnostic on
//! purpose: `input_json`/`result_json` are opaque strings each consumer owns.
//!
//! The `work_items` row also settles the deferred durability journal (2.5):
//! `idempotency_key` + "write the row before the external effect, reconcile on
//! boot" means intent-without-effect is re-confirmed, never silently re-run.

/// What kind of deferred work an item represents. Extensible; serialized as a
/// stable lowercase string in the `kind` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkKind {
    /// A scheduled cron job's fire.
    Cron,
    /// A dispatched sub-agent run (4.3 `delegate`).
    AgentDispatch,
    /// An inbound server-companion result to apply locally (Wave 6).
    ServerResult,
    /// C2 (2.5): a durability-journal row for one MUTATING tool execution —
    /// written BEFORE the effect, finished after, idempotency-keyed by the
    /// call's `ActionFingerprint` so a double-fired action executes once and a
    /// crash mid-action leaves a reconcilable `running` row (terminalized by
    /// the boot pass), never silent half-state. Not claimable by the work
    /// runner — the dispatcher drives these rows synchronously.
    MutatingAction,
}

impl WorkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkKind::Cron => "cron",
            WorkKind::AgentDispatch => "agent_dispatch",
            WorkKind::ServerResult => "server_result",
            WorkKind::MutatingAction => "mutating_action",
        }
    }

    /// Parse from the stored string. Unknown values are an error (fail closed —
    /// never silently coerce an unrecognized kind).
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "cron" => Some(WorkKind::Cron),
            "agent_dispatch" => Some(WorkKind::AgentDispatch),
            "server_result" => Some(WorkKind::ServerResult),
            "mutating_action" => Some(WorkKind::MutatingAction),
            _ => None,
        }
    }
}

/// The lifecycle state of a work item. Transitions are checked
/// ([`WorkState::can_transition_to`]) so a runner can't move an item into an
/// illegal state (e.g. re-run a `Done` item).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkState {
    /// Enqueued, not yet claimed. If `scheduled_at` is set, not due until then.
    Queued,
    /// Claimed by a runner and executing.
    Running,
    /// Completed successfully (terminal).
    Done,
    /// Failed (terminal) — including a crash reconciled at boot.
    Failed,
    /// Paused awaiting something (e.g. an unattended approval parked in the
    /// headless queue); re-queued or cancelled later.
    Parked,
    /// Cancelled by the user/system (terminal).
    Cancelled,
}

impl WorkState {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkState::Queued => "queued",
            WorkState::Running => "running",
            WorkState::Done => "done",
            WorkState::Failed => "failed",
            WorkState::Parked => "parked",
            WorkState::Cancelled => "cancelled",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(WorkState::Queued),
            "running" => Some(WorkState::Running),
            "done" => Some(WorkState::Done),
            "failed" => Some(WorkState::Failed),
            "parked" => Some(WorkState::Parked),
            "cancelled" => Some(WorkState::Cancelled),
            _ => None,
        }
    }

    /// Is this a terminal state (no further transitions)?
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            WorkState::Done | WorkState::Failed | WorkState::Cancelled
        )
    }

    /// May an item move from `self` to `to`? The single source of truth for the
    /// lifecycle — a runner/scheduler consults this before any state write.
    pub fn can_transition_to(self, to: WorkState) -> bool {
        use WorkState::*;
        match self {
            Queued => matches!(to, Running | Cancelled),
            // A run can complete, fail, or pause awaiting approval.
            Running => matches!(to, Done | Failed | Parked),
            // A parked run resumes (back into the queue) or is cancelled.
            Parked => matches!(to, Queued | Cancelled),
            // Terminal states never transition.
            Done | Failed | Cancelled => false,
        }
    }
}

/// One persisted unit of deferred work. Mirrors the `work_items` row. Consumers
/// serialize their own payload into `input_json` and their outcome into
/// `result_json` — the substrate treats both as opaque.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkItem {
    pub id: String,
    pub kind: WorkKind,
    pub state: WorkState,
    /// Origin: a `cron_jobs.id`, a parent conversation id, a server event id.
    pub source_ref: Option<String>,
    /// Opaque, consumer-defined input payload (JSON).
    pub input_json: String,
    /// Opaque, consumer-defined result payload (JSON), set on completion.
    pub result_json: Option<String>,
    /// A human-readable failure reason when `state == Failed`.
    pub error: Option<String>,
    /// Fire-time (unix seconds); `None` ⇒ run as soon as claimed.
    pub scheduled_at: Option<i64>,
    /// Exactly-once dedup key (e.g. `"cron:<id>@<scheduled_at>"`), enforced by a
    /// partial UNIQUE index. `None` ⇒ no dedup.
    pub claim_key: Option<String>,
    /// Durability (2.5): pins a mutating side-effect so a replay executes once.
    pub idempotency_key: Option<String>,
    pub attempts: i64,
    pub target_conversation_id: Option<String>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

impl WorkItem {
    /// A fresh queued item of `kind` with an opaque input payload. `id` is a new
    /// uuid; `created_at` is stamped by the caller.
    pub fn queued(kind: WorkKind, input_json: impl Into<String>, created_at: i64) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            kind,
            state: WorkState::Queued,
            source_ref: None,
            input_json: input_json.into(),
            result_json: None,
            error: None,
            scheduled_at: None,
            claim_key: None,
            idempotency_key: None,
            attempts: 0,
            target_conversation_id: None,
            created_at,
            started_at: None,
            finished_at: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_and_state_round_trip_through_strings() {
        for k in [
            WorkKind::Cron,
            WorkKind::AgentDispatch,
            WorkKind::ServerResult,
            WorkKind::MutatingAction,
        ] {
            assert_eq!(WorkKind::from_str(k.as_str()), Some(k));
        }
        assert_eq!(WorkKind::from_str("nope"), None);
        for s in [
            WorkState::Queued,
            WorkState::Running,
            WorkState::Done,
            WorkState::Failed,
            WorkState::Parked,
            WorkState::Cancelled,
        ] {
            assert_eq!(WorkState::from_str(s.as_str()), Some(s));
        }
        assert_eq!(WorkState::from_str("nope"), None);
    }

    #[test]
    fn lifecycle_allows_only_legal_transitions() {
        use WorkState::*;
        // Queued.
        assert!(Queued.can_transition_to(Running));
        assert!(Queued.can_transition_to(Cancelled));
        assert!(
            !Queued.can_transition_to(Done),
            "a queued item can't jump to done"
        );
        assert!(!Queued.can_transition_to(Parked));
        // Running.
        assert!(Running.can_transition_to(Done));
        assert!(Running.can_transition_to(Failed));
        assert!(Running.can_transition_to(Parked));
        assert!(
            !Running.can_transition_to(Queued),
            "a running item can't silently un-claim"
        );
        // Parked resumes or cancels.
        assert!(Parked.can_transition_to(Queued));
        assert!(Parked.can_transition_to(Cancelled));
        assert!(
            !Parked.can_transition_to(Running),
            "parked must re-queue before running"
        );
        // Terminals are frozen.
        for t in [Done, Failed, Cancelled] {
            assert!(t.is_terminal());
            for to in [Queued, Running, Done, Failed, Parked, Cancelled] {
                assert!(
                    !t.can_transition_to(to),
                    "{t:?} is terminal, can't → {to:?}"
                );
            }
        }
    }

    #[test]
    fn queued_constructor_defaults() {
        let w = WorkItem::queued(WorkKind::Cron, "{}", 100);
        assert_eq!(w.state, WorkState::Queued);
        assert_eq!(w.kind, WorkKind::Cron);
        assert_eq!(w.attempts, 0);
        assert!(w.scheduled_at.is_none() && w.claim_key.is_none());
        assert!(!w.id.is_empty());
    }
}
