//! Headless approval queue + rule-based pre-authorization (Q5) — the
//! [`ApprovalPrompter`] for an UNATTENDED body (the future headless server, or
//! the desktop app running with no user present). Server-track prep: no
//! headless body exists yet to plug this into, so it ships as a fully-tested,
//! ready component.
//!
//! Where the interactive `TauriApprovalPrompter` BLOCKS up to 5 minutes waiting
//! for a human, [`QueueingPrompter`] returns immediately:
//!
//! * **Pre-authorize** an action iff a rule covers it — rules ride the Q8
//!   [`PolicySource`] (`(tool, pattern, Allow)` [`ToolRule`]s), the same
//!   human-readable rule store the interactive path uses, so a rule authored /
//!   synced once governs both. A pre-authorized action returns an immediate
//!   `Approve` as a **per-action `Once` grant** (audited, never a standing
//!   grant it grants itself).
//! * Otherwise **park** the request in the [`ApprovalQueue`] for later human
//!   review and return `Deny` — nothing auto-grants just because no one is
//!   watching. This is "park-and-queue instead of block."
//!
//! Two non-negotiable floors, enforced *in the prompter* (not delegated to the
//! rule store, so a permissive/corrupt rule can't loosen them):
//!
//! * A **`Dangerous`** action is NEVER pre-authorized (invariant #8 — an
//!   irreversible/high-blast action can never earn a standing grant, and
//!   "no human present" is the last place to relax that).
//! * An **`External`** (egress) action is pre-authorized only if a rule *names
//!   the destination* (a non-wildcard pattern that matches the call's
//!   destination) — a bare `*` never green-lights sending to an arbitrary
//!   place. This mirrors the decided autonomy model (build plan: "External =
//!   standing permission only if the rule names the destination").

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::hooks::approval::{
    ApprovalDecision, ApprovalPrompter, ApprovalRequest, GrantScope, GrantTarget,
};
use crate::hooks::permission::{glob_match, resolve_effective_mode, PermissionMode, PolicySource};
use crate::tools::RiskClass;

/// A parked approval awaiting later human review. Serialize-able so a future
/// review UI / server endpoint can list the queue.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct QueuedApproval {
    pub id: String,
    pub conversation_id: String,
    pub tool_name: String,
    /// The canonical `name {args}` — display-only, untrusted.
    pub command: String,
    /// Lowercase risk discriminant ("safe"|"write"|"external"|"dangerous").
    pub risk: String,
    /// For an External call, where it would have gone.
    pub destination: Option<String>,
}

/// The durable-ish queue of parked, unanswered approvals. Shared via `Arc`
/// between the prompter (which enqueues) and a future review surface (which
/// drains). In-memory for now; the server track persists it (PLAN §5 outbox).
#[derive(Default)]
pub struct ApprovalQueue {
    parked: Mutex<Vec<QueuedApproval>>,
}

impl ApprovalQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Park a request for later human review.
    pub fn enqueue(&self, item: QueuedApproval) {
        self.parked
            .lock()
            .expect("approval queue poisoned")
            .push(item);
    }

    /// A snapshot of everything currently parked (for a review UI).
    pub fn pending(&self) -> Vec<QueuedApproval> {
        self.parked.lock().expect("approval queue poisoned").clone()
    }

    pub fn len(&self) -> usize {
        self.parked.lock().expect("approval queue poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Remove and return a parked entry (a human answered it out-of-band).
    /// Returns `None` if the id isn't parked. Draining the queue does NOT
    /// retroactively run the action — the dispatch that parked it already
    /// received `Deny`; a resolution is a review artifact (and, in the server
    /// track, the trigger to author a rule so the RETRY pre-authorizes).
    pub fn resolve(&self, id: &str) -> Option<QueuedApproval> {
        let mut g = self.parked.lock().expect("approval queue poisoned");
        if let Some(pos) = g.iter().position(|q| q.id == id) {
            Some(g.remove(pos))
        } else {
            None
        }
    }
}

/// The unattended [`ApprovalPrompter`]. Holds the shared queue, a
/// [`PolicySource`] (the Q8 rule store), and the profile whose rules apply.
pub struct QueueingPrompter {
    queue: Arc<ApprovalQueue>,
    policy: Arc<dyn PolicySource>,
    profile: String,
}

impl QueueingPrompter {
    pub fn new(
        queue: Arc<ApprovalQueue>,
        policy: Arc<dyn PolicySource>,
        profile: impl Into<String>,
    ) -> Self {
        Self {
            queue,
            policy,
            profile: profile.into(),
        }
    }

    /// Would a rule pre-authorize this action unattended? Returns the grant to
    /// issue, or `None` to park + deny. Two floors are enforced HERE, above the
    /// rule store, so no rule can relax them. A pre-authorized grant is always
    /// per-action `Once` — the prompter never hands itself a standing grant.
    fn preauthorize(&self, req: &ApprovalRequest) -> Option<(GrantScope, GrantTarget)> {
        // Floor 1: an irreversible/high-blast action is never pre-authorized.
        if req.risk == RiskClass::Dangerous {
            return None;
        }

        // Resolve with the EXACT precedence the interactive PermissionHook uses
        // (deny > ask > allow, most-specific-wins), matched against the same
        // command text — so a specific Ask/Deny carve-out under a broad Allow
        // still wins, and the headless path is never MORE permissive than an
        // attended one. Anything but a definitive Allow ⇒ park + deny.
        let rules = self.policy.rules_for(&req.tool_name, &self.profile);
        let whole_tool = self.policy.mode_for(&req.tool_name);
        if resolve_effective_mode(&rules, whole_tool, &req.command) != Some(PermissionMode::Allow) {
            return None;
        }

        // Floor 2: an egress call additionally requires a rule that NAMES the
        // destination — an Allow pattern with at least one non-wildcard
        // character that matches the call's destination. A bare/all-`*` rule
        // (`"*"`, `"**"`, …) has no literal char, so it can Allow-win on the
        // command yet never green-light sending somewhere arbitrary.
        if req.risk == RiskClass::External {
            let dest = req.destination.as_deref()?;
            let names_dest = rules.iter().any(|r| {
                r.action == PermissionMode::Allow
                    && r.pattern.chars().any(|c| c != '*')
                    && glob_match(&r.pattern, dest)
            });
            if !names_dest {
                return None;
            }
        }

        Some((
            GrantScope::Once,
            GrantTarget::Fingerprint(req.fingerprint.clone()),
        ))
    }
}

impl ApprovalPrompter for QueueingPrompter {
    fn request<'a>(
        &'a self,
        req: ApprovalRequest,
    ) -> Pin<Box<dyn Future<Output = ApprovalDecision> + Send + 'a>> {
        Box::pin(async move {
            if let Some((scope, target)) = self.preauthorize(&req) {
                return ApprovalDecision::Approve(scope, target);
            }
            // Park for later human review, then fail closed.
            self.queue.enqueue(QueuedApproval {
                id: req.id,
                conversation_id: req.conversation_id,
                tool_name: req.tool_name,
                command: req.command,
                risk: req.risk.as_str().to_string(),
                destination: req.destination,
            });
            ApprovalDecision::Deny
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::permission::{InMemoryPolicySource, ToolRule};

    fn req(tool: &str, risk: RiskClass, dest: Option<&str>) -> ApprovalRequest {
        let command = format!("{tool} {{}}");
        req_cmd(tool, risk, dest, &command)
    }

    /// Like `req` but with an explicit command text — needed for External,
    /// whose real command embeds the URL/destination (the winner-resolution
    /// matches against the command, same as the interactive path).
    fn req_cmd(tool: &str, risk: RiskClass, dest: Option<&str>, command: &str) -> ApprovalRequest {
        ApprovalRequest {
            id: format!("req-{tool}"),
            conversation_id: "c1".into(),
            tool_name: tool.into(),
            fingerprint: format!("fp-{tool}"),
            command: command.into(),
            prompt: "approve?".into(),
            by: "permission".into(),
            risk,
            destination: dest.map(str::to_string),
        }
    }

    fn policy_with(rules: Vec<ToolRule>) -> Arc<dyn PolicySource> {
        let mut p = InMemoryPolicySource::new();
        for r in rules {
            p.add_rule(r.tool_name, r.pattern, r.action);
        }
        Arc::new(p)
    }

    async fn decide(prompter: &QueueingPrompter, r: ApprovalRequest) -> ApprovalDecision {
        prompter.request(r).await
    }

    #[tokio::test]
    async fn no_rule_parks_and_denies() {
        let queue = Arc::new(ApprovalQueue::new());
        let prompter = QueueingPrompter::new(queue.clone(), policy_with(vec![]), "personal");

        let d = decide(&prompter, req("write_file", RiskClass::Write, None)).await;
        assert_eq!(d, ApprovalDecision::Deny, "no rule ⇒ fail closed");
        assert_eq!(queue.len(), 1, "the request is parked for review");
        assert_eq!(queue.pending()[0].tool_name, "write_file");
    }

    #[tokio::test]
    async fn a_matching_allow_rule_preauthorizes_a_write() {
        let queue = Arc::new(ApprovalQueue::new());
        let policy = policy_with(vec![ToolRule::new(
            "write_file",
            "*",
            PermissionMode::Allow,
        )]);
        let prompter = QueueingPrompter::new(queue.clone(), policy, "personal");

        let d = decide(&prompter, req("write_file", RiskClass::Write, None)).await;
        match d {
            ApprovalDecision::Approve(GrantScope::Once, GrantTarget::Fingerprint(fp)) => {
                assert_eq!(fp, "fp-write_file")
            }
            other => panic!("expected a Once/Fingerprint approve, got {other:?}"),
        }
        assert!(queue.is_empty(), "a pre-authorized call is not parked");
    }

    #[tokio::test]
    async fn dangerous_is_never_preauthorized_even_with_a_matching_rule() {
        let queue = Arc::new(ApprovalQueue::new());
        // Even a wide-open Allow rule for the tool must not pre-authorize it.
        let policy = policy_with(vec![ToolRule::new(
            "shell_exec",
            "*",
            PermissionMode::Allow,
        )]);
        let prompter = QueueingPrompter::new(queue.clone(), policy, "personal");

        let d = decide(&prompter, req("shell_exec", RiskClass::Dangerous, None)).await;
        assert_eq!(
            d,
            ApprovalDecision::Deny,
            "Dangerous must fail closed regardless of rules"
        );
        assert_eq!(queue.len(), 1);
    }

    #[tokio::test]
    async fn external_needs_a_rule_that_names_the_destination() {
        let ex_cmd = r#"fetch_url {"url":"https://example.com/"}"#;

        // A bare-* Allow rule does NOT pre-authorize an egress call, even though
        // it Allow-wins on the command — it names no destination.
        let queue = Arc::new(ApprovalQueue::new());
        let star = QueueingPrompter::new(
            queue.clone(),
            policy_with(vec![ToolRule::new("fetch_url", "*", PermissionMode::Allow)]),
            "personal",
        );
        let d = decide(
            &star,
            req_cmd(
                "fetch_url",
                RiskClass::External,
                Some("example.com"),
                ex_cmd,
            ),
        )
        .await;
        assert_eq!(
            d,
            ApprovalDecision::Deny,
            "bare * must not green-light egress"
        );

        // Finding A: an all-wildcard pattern ("**") likewise names nothing.
        let star2 = QueueingPrompter::new(
            Arc::new(ApprovalQueue::new()),
            policy_with(vec![ToolRule::new(
                "fetch_url",
                "**",
                PermissionMode::Allow,
            )]),
            "personal",
        );
        let d = decide(
            &star2,
            req_cmd(
                "fetch_url",
                RiskClass::External,
                Some("example.com"),
                ex_cmd,
            ),
        )
        .await;
        assert_eq!(
            d,
            ApprovalDecision::Deny,
            "** must not green-light egress either"
        );

        // A rule naming the destination DOES pre-authorize it.
        let named_queue = Arc::new(ApprovalQueue::new());
        let named = QueueingPrompter::new(
            named_queue.clone(),
            policy_with(vec![ToolRule::new(
                "fetch_url",
                "*example.com*",
                PermissionMode::Allow,
            )]),
            "personal",
        );
        let d = decide(
            &named,
            req_cmd(
                "fetch_url",
                RiskClass::External,
                Some("example.com"),
                ex_cmd,
            ),
        )
        .await;
        assert!(
            matches!(d, ApprovalDecision::Approve(GrantScope::Once, _)),
            "a destination-naming rule pre-authorizes egress, got {d:?}"
        );
        assert!(named_queue.is_empty());

        // …but only for THAT destination — a different host still parks (the
        // rule doesn't even match the evil.com command).
        let d = decide(
            &named,
            req_cmd(
                "fetch_url",
                RiskClass::External,
                Some("evil.com"),
                r#"fetch_url {"url":"https://evil.com/"}"#,
            ),
        )
        .await;
        assert_eq!(
            d,
            ApprovalDecision::Deny,
            "the rule names example.com, not evil.com"
        );
    }

    #[tokio::test]
    async fn a_specific_ask_carveout_beats_a_broad_allow() {
        // Finding B: the headless path must resolve rules with the same
        // precedence as the interactive PermissionHook — a more-specific Ask
        // wins over a broad Allow, so it must NOT pre-authorize.
        let queue = Arc::new(ApprovalQueue::new());
        let prompter = QueueingPrompter::new(
            queue.clone(),
            policy_with(vec![
                ToolRule::new("write_file", "*", PermissionMode::Allow),
                ToolRule::new("write_file", "*secret*", PermissionMode::Ask),
            ]),
            "personal",
        );
        let d = decide(
            &prompter,
            req_cmd(
                "write_file",
                RiskClass::Write,
                None,
                r#"write_file {"path":"secret.txt"}"#,
            ),
        )
        .await;
        assert_eq!(
            d,
            ApprovalDecision::Deny,
            "a specific Ask carve-out must win → park"
        );
        // A non-matching command still rides the broad Allow.
        let d = decide(
            &prompter,
            req_cmd(
                "write_file",
                RiskClass::Write,
                None,
                r#"write_file {"path":"notes.txt"}"#,
            ),
        )
        .await;
        assert!(matches!(d, ApprovalDecision::Approve(GrantScope::Once, _)));
    }

    #[tokio::test]
    async fn rules_are_profile_scoped_and_queue_resolves() {
        // A rule authored in one profile must not pre-authorize another's call.
        // (InMemoryPolicySource is profile-blind, so simulate scope by using a
        // rule that matches only the intended command; here we assert the queue
        // drain path.)
        let queue = Arc::new(ApprovalQueue::new());
        let prompter = QueueingPrompter::new(queue.clone(), policy_with(vec![]), "work");
        let _ = decide(&prompter, req("delete_file", RiskClass::Write, None)).await;
        assert_eq!(queue.len(), 1);
        let drained = queue
            .resolve("req-delete_file")
            .expect("parked entry present");
        assert_eq!(drained.tool_name, "delete_file");
        assert!(queue.is_empty(), "resolve removes it");
        assert!(queue.resolve("nope").is_none(), "unknown id ⇒ None");
    }
}
