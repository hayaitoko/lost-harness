//! §3.5 The approval spine — body-agnostic primitives for interactive
//! tool-call confirmation. Spec `docs/PLAN.md` §4 (approval spine: layered
//! policy + pinned/locked approvals) / §8 (M3 build order item 9).
//!
//! The gating hooks (`PermissionHook`, `FirstUseConfirmHook`) are
//! *synchronous* (`GatingHook::on_event` returns immediately), so the
//! human-in-the-loop wait can't live inside a hook. It lives in the async
//! `ToolDispatcher::dispatch`: when the chain returns `Ask`, dispatch asks an
//! [`ApprovalPrompter`], records the answer in the shared [`ApprovalLedger`],
//! then RE-RUNS the whole chain (so the non-overridable Sandbox floor is
//! always re-consulted) and proceeds. The hooks are made "ledger-aware" so a
//! recorded grant turns their `Ask` into `Continue` on the re-run.
//!
//! The **pin** against approval-drift is [`ActionFingerprint`] — a hash of
//! the tool name + canonicalized args. A "just this action" grant binds to
//! that exact fingerprint, so a later call with different args is a different
//! fingerprint and re-prompts. "Always allow this tool" is the deliberate
//! broadening ([`GrantTarget::Tool`]).
//!
//! Scope note: `Once` and `Session` grants live entirely in this in-memory
//! ledger and ship now. `Always` is meant to persist across restarts via a
//! persistent `PolicySource` (SQLite `tool_rules`, PLAN M4) — until that
//! lands, `Always` is treated as `Session` here (works for the session, does
//! not survive a restart); see [`ApprovalLedger::grant`].
//!
//! **No half-durability (Q3).** Never persist an approval/intent without
//! also persisting the execution state machine it authorizes (a journal
//! row written *before* the side effect, with an idempotency key; boot
//! then reconciles "intent without effect" by re-confirming, never by
//! re-running). A persisted grant plus volatile run state is exactly the
//! double-execution bug — all-volatile, today's state, is safe. This is
//! *why* `Once`/`Session` living only in this in-memory ledger is
//! correct, not a gap: force-quit between "user clicked Allow" and
//! `tool.run` executing loses the grant and the tool never ran — nothing
//! to reconcile, the user re-asks and re-approves. `agent::crash_recovery`
//! terminalizes the *turn* left hanging by that scenario, but has nothing
//! to do for approvals specifically until a real persisted artifact
//! exists. Keep it that way until the action journal lands (deferred to
//! the first non-idempotent external-effect tool), and route
//! `GrantScope::Always` through a rule table (Q8) rather than a "pending
//! armed action" when that work starts.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use sha2::{Digest, Sha256};

use crate::hooks::permission::ToolRule;
use crate::hooks::EventContext;
use crate::tools::RiskClass;

// ── ActionFingerprint ──────────────────────────────────────────────────────

/// A stable hash over `(tool_name, canonicalized args)`. Two calls hash
/// equal iff they invoke the same tool with the same arguments — this is
/// what pins a "just this action" grant so it can't drift to a different call.
pub struct ActionFingerprint;

impl ActionFingerprint {
    pub fn of(tool_name: &str, args: &serde_json::Value) -> String {
        let mut hasher = Sha256::new();
        hasher.update(tool_name.as_bytes());
        hasher.update([0u8]); // domain separator between name and args
        hasher.update(canonical(args).as_bytes());
        let digest = hasher.finalize();
        let mut out = String::with_capacity(64);
        for b in digest {
            use std::fmt::Write;
            let _ = write!(&mut out, "{:02x}", b);
        }
        out
    }

    pub fn from_ctx(ctx: &EventContext) -> String {
        Self::of(&ctx.tool_name, &ctx.input.args)
    }
}

/// Canonical, order-stable string form of a JSON value: object keys sorted,
/// no incidental whitespace. Deterministic regardless of the `serde_json`
/// `preserve_order` feature, so a fingerprint is reproducible.
fn canonical(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut s = String::from("{");
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                // {:?} quotes+escapes the key; recurse for the value.
                s.push_str(&format!("{:?}:{}", k, canonical(&map[*k])));
            }
            s.push('}');
            s
        }
        serde_json::Value::Array(arr) => {
            let mut s = String::from("[");
            for (i, e) in arr.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&canonical(e));
            }
            s.push(']');
            s
        }
        other => other.to_string(),
    }
}

// ── Grants ───────────────────────────────────────────────────────────────

/// How long an approval lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantScope {
    /// This exact action, one time — consumed at execution.
    Once,
    /// Remembered until the app restarts.
    Session,
    /// Meant to persist across restarts (needs a persistent PolicySource —
    /// PLAN M4). Until that exists it behaves like `Session`.
    Always,
}

/// What an approval applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantTarget {
    /// The PIN: exactly this action (tool + args), by fingerprint.
    Fingerprint(String),
    /// The deliberate broadening: any call to this tool.
    Tool(String),
}

// ── ApprovalLedger ─────────────────────────────────────────────────────────

/// In-memory record of what the user has approved this session. Cheap to
/// share via `Arc`; consulted (read-only) by the ledger-aware hooks and
/// written by `ToolDispatcher` on an approval.
#[derive(Debug, Default)]
pub struct ApprovalLedger {
    /// Fingerprints granted for one single execution (consumed on use).
    once_fps: Mutex<HashSet<String>>,
    /// Fingerprints granted for the rest of the session.
    session_fps: Mutex<HashSet<String>>,
    /// Whole tools granted for the rest of the session.
    session_tools: Mutex<HashSet<String>>,
}

impl ApprovalLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Is this `(tool, fingerprint)` already covered by a grant? Read-only.
    pub fn covers(&self, tool_name: &str, fingerprint: &str) -> bool {
        self.once_fps.lock().unwrap().contains(fingerprint)
            || self.session_fps.lock().unwrap().contains(fingerprint)
            || self.session_tools.lock().unwrap().contains(tool_name)
    }

    /// Like [`Self::covers`], but risk-and-attendance aware (B5). An UNATTENDED
    /// dispatch (cron / delegate helper / headless — `approver: None`) of an
    /// `External`-risk call is NOT satisfied by a `Session`/`Always` grant:
    /// only a fresh `Once` fingerprint grant counts. This closes the cron
    /// replay loophole — `ActionFingerprint` is tool+args only (no session
    /// discriminator), so without this an interactively-granted Session-scope
    /// External approval would silently satisfy a byte-identical headless
    /// dispatch later in the same app session. Attended calls, and every
    /// non-`External` risk, resolve exactly as [`Self::covers`] (so
    /// `accept_edits`'s documented auto-approve-Write-on-cron still holds).
    pub fn covers_for(
        &self,
        tool_name: &str,
        fingerprint: &str,
        risk: RiskClass,
        attended: bool,
    ) -> bool {
        if !attended && risk == RiskClass::External {
            return self.once_fps.lock().unwrap().contains(fingerprint);
        }
        self.covers(tool_name, fingerprint)
    }

    /// Is this fingerprint covered by a `Once` grant specifically — ignores
    /// session/tool-wide coverage. Used by floor-style hooks
    /// (`ProtectedPathHook`) that must never be satisfiable by a standing
    /// grant: a `Session`/`Tool` or `Session`/`Fingerprint` grant is
    /// invisible here, so the only way the floor flips to `Continue` is a
    /// fresh `Once`+`Fingerprint` grant recorded for this exact action.
    pub fn covers_once(&self, fingerprint: &str) -> bool {
        self.once_fps.lock().unwrap().contains(fingerprint)
    }

    /// Record a grant. `Always` currently maps to session storage (no
    /// persistence yet — see the module docs); when a persistent PolicySource
    /// lands, the dispatcher will route `Always` there instead of here.
    pub fn grant(&self, target: GrantTarget, scope: GrantScope) {
        match (scope, target) {
            (GrantScope::Once, GrantTarget::Fingerprint(fp)) => {
                self.once_fps.lock().unwrap().insert(fp);
            }
            // A one-time grant is inherently per-ACTION. It must never widen
            // into a session-length, whole-tool grant — a `Once` + `Tool`
            // request has no fingerprint to pin, so it grants NOTHING (the
            // next call re-prompts). `resolve_tool_approval` also forces
            // `Once => action`, and `resolve_grant` (Q8) never emits this
            // combo, so this arm shouldn't be reached in practice; it's the
            // defensive floor. Reaching it means a caller constructed the
            // illegal pair directly — record nothing AND flag it, since a
            // silently-lost approval click is a real UX bug worth surfacing.
            (GrantScope::Once, GrantTarget::Tool(t)) => {
                tracing::warn!(
                    tool = %t,
                    "ignored an illegal (Once, Tool) grant — a one-time grant must be per-action; nothing recorded"
                );
            }
            (GrantScope::Session | GrantScope::Always, GrantTarget::Fingerprint(fp)) => {
                self.session_fps.lock().unwrap().insert(fp);
            }
            (GrantScope::Session | GrantScope::Always, GrantTarget::Tool(t)) => {
                self.session_tools.lock().unwrap().insert(t);
            }
        }
    }

    /// Consume a one-time fingerprint grant (call right before executing, so
    /// a `Once` approval covers exactly one execution and no more).
    pub fn consume_once(&self, fingerprint: &str) {
        self.once_fps.lock().unwrap().remove(fingerprint);
    }
}

// ── Grant × risk matrix (Q8) ────────────────────────────────────────────────

/// The single server-side enforcement point of Q8's grant-scope × risk
/// matrix. Given a tool's [`RiskClass`] and the scope/target the human's
/// answer requested, return the grant that may actually be RECORDED in the
/// ledger — **never wider than requested** on either axis (duration
/// `Once` < `Session` < `Always`; target `Fingerprint` < `Tool`).
///
/// The call still RUNS when this is reached (the human approved it in person);
/// only the *standing* coverage is narrowed. This is what makes invariant #8
/// ("a `Dangerous` action can never be silently covered by a Session/Always
/// grant") a structural, tested property of the grant path rather than a UI
/// behavior or a one-off dispatch special-case.
///
/// | Risk | Once | Session | Always |
/// |---|---|---|---|
/// | Safe | (Once, fp) | as-asked | as-asked | *(defensive; Safe is pre-trusted and never Asks)* |
/// | Write | (Once, fp) | as-asked | as-asked |
/// | External | (Once, fp) | (Session, fp) | (Session, fp) | *fingerprint-pinned only — no whole-tool standing for egress; a bare whole-tool "always" is refused, narrowed to a session fingerprint. Destination-scoped standing rules are the External path and arrive via [`ApprovalDecision::Persist`] with the first External tool.* |
/// | Dangerous | (Once, fp) | (Once, fp) | (Once, fp) | *Once-only: any standing answer collapses; runs once, records nothing.* |
///
/// `Once` is always forced to `Fingerprint` regardless of risk (a one-time
/// grant is inherently per-action — the "Things you didn't ask" hardening).
pub fn resolve_grant(
    risk: RiskClass,
    scope: GrantScope,
    target: GrantTarget,
    fingerprint: &str,
) -> (GrantScope, GrantTarget) {
    let fp = || GrantTarget::Fingerprint(fingerprint.to_string());
    match (risk, scope) {
        // Dangerous: never a standing grant (invariant #8). Runs once.
        (RiskClass::Dangerous, _) => (GrantScope::Once, fp()),

        // External: fingerprint-pinned only — a whole-tool standing grant for
        // an egress tool is refused. `Always` (bare whole-tool) narrows to a
        // session fingerprint, strictly narrower than the requested Always/Tool.
        (RiskClass::External, GrantScope::Once) => (GrantScope::Once, fp()),
        (RiskClass::External, GrantScope::Session | GrantScope::Always) => {
            (GrantScope::Session, fp())
        }

        // Safe / Write: honor the request, but a one-time grant is per-action.
        (_, GrantScope::Once) => (GrantScope::Once, fp()),
        (_, scope) => (scope, target),
    }
}

/// Which risk classes may persist a durable `Always` `tool_rules` row (Q8).
/// **Only `Write`** — reversible, on-machine mutations get standing policy.
///   * `External` — egress; a bare whole-tool standing grant is refused
///     (destination-scoped authoring lands with the first External tool),
///   * `Dangerous` — invariant #8, Once-only (never a standing grant),
///   * `Safe` — pre-trusted, never reaches an Ask.
/// A refused persist doesn't block the call: the human approved it, so it runs
/// once (`(Once, Fingerprint)`), it just records nothing durable — the same
/// narrowing `resolve_grant` applies to a standing `Approve` answer.
pub fn persist_rule_allowed(risk: RiskClass) -> bool {
    matches!(risk, RiskClass::Write)
}

// ── Prompter ───────────────────────────────────────────────────────────────

/// A pending approval, handed to the prompter to surface to the human.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub id: String,
    pub conversation_id: String,
    pub tool_name: String,
    pub fingerprint: String,
    /// The canonical `name {args}` form of the call, so the human can vet
    /// exactly what they're approving (not just which tool). Untrusted —
    /// display-only.
    pub command: String,
    /// The hook-supplied prompt (e.g. "first use of tool 'write_file'…").
    pub prompt: String,
    /// Which hook raised the Ask ("permission" | "first_use_confirm").
    pub by: String,
    /// The tool's risk class — server-derived from `Tool::risk()`, never
    /// client-supplied. Drives the dialog's risk badge and which grant
    /// buttons are offered (`Dangerous` hides Session/Always; `External`
    /// hides whole-tool standing). The dialog is convenience only — the
    /// server (`resolve_grant`) is the enforcement, so a bypassed button
    /// still can't widen the grant.
    pub risk: RiskClass,
    /// For `External` tools, where the call goes (domain/recipient) — the
    /// consent the dialog must surface. Server-derived from the call, never
    /// from client input. `None` until the first real `External` tool ships.
    pub destination: Option<String>,
}

/// The user's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// An ephemeral grant (Once/Session). Narrowed per risk by `resolve_grant`
    /// before it reaches the ledger.
    Approve(GrantScope, GrantTarget),
    /// A durable `Always` grant — persist a per-profile `tool_rules` row. The
    /// dialog produces this for "Always allow"; the dispatcher enforces the
    /// matrix on it via `persist_rule_allowed` (only `Write` persists; the
    /// rest degrade to run-once) and keys the write off the call's profile.
    Persist(ToolRule),
    Deny,
    /// No answer within the timeout, or the channel dropped — deny by default.
    Timeout,
}

/// Something that can ask the human and return their decision. The Tauri app
/// implements this (emit an event + await a resolve command); the headless
/// server can plug a different implementation. `ToolDispatcher` holds an
/// `Option`, so `None` = the round-1 fallback (surface `Ask` to the model as
/// "not granted this round") with no interactive wait.
pub trait ApprovalPrompter: Send + Sync {
    fn request<'a>(
        &'a self,
        req: ApprovalRequest,
    ) -> Pin<Box<dyn Future<Output = ApprovalDecision> + Send + 'a>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fingerprint_is_stable_and_arg_order_independent() {
        let a = ActionFingerprint::of("write_file", &json!({"path": "a.txt", "content": "x"}));
        let b = ActionFingerprint::of("write_file", &json!({"content": "x", "path": "a.txt"}));
        assert_eq!(a, b, "key order must not change the fingerprint");
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn fingerprint_differs_on_tool_or_args() {
        let base = ActionFingerprint::of("write_file", &json!({"path": "a.txt"}));
        assert_ne!(base, ActionFingerprint::of("write_file", &json!({"path": "b.txt"})));
        assert_ne!(base, ActionFingerprint::of("delete_file", &json!({"path": "a.txt"})));
    }

    #[test]
    fn once_grant_covers_then_is_consumed() {
        let led = ApprovalLedger::new();
        let fp = "abc".to_string();
        led.grant(GrantTarget::Fingerprint(fp.clone()), GrantScope::Once);
        assert!(led.covers("write_file", &fp));
        led.consume_once(&fp);
        assert!(!led.covers("write_file", &fp), "a once-grant must not cover twice");
    }

    #[test]
    fn session_fingerprint_grant_persists_and_pins() {
        let led = ApprovalLedger::new();
        led.grant(GrantTarget::Fingerprint("fp1".into()), GrantScope::Session);
        assert!(led.covers("write_file", "fp1"));
        // A different fingerprint (different args) is NOT covered — no drift.
        assert!(!led.covers("write_file", "fp2"));
    }

    #[test]
    fn session_tool_grant_covers_any_fingerprint_of_that_tool() {
        let led = ApprovalLedger::new();
        led.grant(GrantTarget::Tool("write_file".into()), GrantScope::Session);
        assert!(led.covers("write_file", "any-fp"));
        assert!(!led.covers("delete_file", "any-fp"), "tool grant must not cover a different tool");
    }

    #[test]
    fn a_once_grant_for_a_whole_tool_grants_nothing() {
        // "Once" is per-action; it must never silently widen into a
        // session-length, whole-tool grant.
        let led = ApprovalLedger::new();
        led.grant(GrantTarget::Tool("write_file".into()), GrantScope::Once);
        assert!(
            !led.covers("write_file", "any-fp"),
            "a Once+Tool grant must record nothing (re-prompt next time)"
        );
    }

    #[test]
    fn covers_once_only_sees_once_fps_not_session_grants() {
        // The protected-path floor relies on covers_once to be Once-only
        // — a Session+Tool grant that PermissionHook would happily
        // consume must be invisible here, otherwise a future
        // "Allow for this session" click on a protected-path prompt
        // would silently widen the floor to standing coverage.
        let led = ApprovalLedger::new();
        let fp = "fp-1".to_string();

        // No grant at all: not covered.
        assert!(!led.covers_once(&fp));

        // Session/Tool grant: a broad standing grant for the whole tool
        // must not satisfy the floor.
        led.grant(GrantTarget::Tool("write_file".into()), GrantScope::Session);
        assert!(
            !led.covers_once(&fp),
            "a Session+Tool grant must not satisfy covers_once"
        );

        // Session/Fingerprint grant: even a pinned Session grant must
        // not satisfy the floor.
        led.grant(GrantTarget::Fingerprint(fp.clone()), GrantScope::Session);
        assert!(
            !led.covers_once(&fp),
            "a Session+Fingerprint grant must not satisfy covers_once"
        );

        // Once/Fingerprint grant: THIS is what flips the floor to Continue.
        led.grant(GrantTarget::Fingerprint(fp.clone()), GrantScope::Once);
        assert!(
            led.covers_once(&fp),
            "a Once+Fingerprint grant must satisfy covers_once"
        );
    }

    // ── resolve_grant: the grant × risk matrix ──────────────────────────────

    const FP: &str = "deadbeef";

    /// A one-time grant is per-action for EVERY risk — `Once` never widens to
    /// a whole tool.
    #[test]
    fn resolve_grant_forces_once_to_fingerprint_for_every_risk() {
        for risk in [
            RiskClass::Safe,
            RiskClass::Write,
            RiskClass::External,
            RiskClass::Dangerous,
        ] {
            let (scope, target) =
                resolve_grant(risk, GrantScope::Once, GrantTarget::Tool("t".into()), FP);
            assert_eq!(scope, GrantScope::Once);
            assert_eq!(
                target,
                GrantTarget::Fingerprint(FP.into()),
                "{risk:?}: Once must pin the fingerprint, never a whole tool"
            );
        }
    }

    /// Write honors a Session/Always answer as-asked (whole-tool standing is
    /// legal for a reversible, on-machine mutation).
    #[test]
    fn resolve_grant_write_honors_standing_grants() {
        let (s, t) = resolve_grant(
            RiskClass::Write,
            GrantScope::Session,
            GrantTarget::Tool("write_file".into()),
            FP,
        );
        assert_eq!(s, GrantScope::Session);
        assert_eq!(t, GrantTarget::Tool("write_file".into()));
    }

    /// External is fingerprint-pinned only — a whole-tool Session answer is
    /// narrowed to this exact fingerprint (no standing coverage for egress).
    #[test]
    fn resolve_grant_external_session_is_fingerprint_only() {
        let (s, t) = resolve_grant(
            RiskClass::External,
            GrantScope::Session,
            GrantTarget::Tool("send_email".into()),
            FP,
        );
        assert_eq!(s, GrantScope::Session);
        assert_eq!(
            t,
            GrantTarget::Fingerprint(FP.into()),
            "External must never grant a whole-tool standing grant"
        );
    }

    /// External `Always` (bare whole-tool) is refused — narrowed to a session
    /// fingerprint, strictly narrower than the requested Always/Tool.
    #[test]
    fn resolve_grant_external_always_whole_tool_is_refused() {
        let (s, t) = resolve_grant(
            RiskClass::External,
            GrantScope::Always,
            GrantTarget::Tool("send_email".into()),
            FP,
        );
        assert_eq!(s, GrantScope::Session);
        assert_eq!(t, GrantTarget::Fingerprint(FP.into()));
    }

    /// Dangerous collapses ANY standing answer to `(Once, Fingerprint)` — the
    /// structural form of invariant #8. Runs once, records nothing standing.
    #[test]
    fn resolve_grant_dangerous_collapses_all_standing_to_once() {
        for (scope, target) in [
            (GrantScope::Session, GrantTarget::Tool("shell_exec".into())),
            (GrantScope::Always, GrantTarget::Tool("shell_exec".into())),
            (GrantScope::Session, GrantTarget::Fingerprint("other".into())),
            (GrantScope::Always, GrantTarget::Fingerprint("other".into())),
        ] {
            let (s, t) = resolve_grant(RiskClass::Dangerous, scope, target, FP);
            assert_eq!(s, GrantScope::Once, "Dangerous must never grant a standing scope");
            assert_eq!(t, GrantTarget::Fingerprint(FP.into()));
        }
    }

    /// The matrix never WIDENS: for every risk × scope × target, the output is
    /// no broader than the input on either axis.
    #[test]
    fn resolve_grant_never_widens() {
        let dur = |s: GrantScope| match s {
            GrantScope::Once => 0u8,
            GrantScope::Session => 1,
            GrantScope::Always => 2,
        };
        let breadth = |t: &GrantTarget| match t {
            GrantTarget::Fingerprint(_) => 0u8,
            GrantTarget::Tool(_) => 1,
        };
        for risk in [
            RiskClass::Safe,
            RiskClass::Write,
            RiskClass::External,
            RiskClass::Dangerous,
        ] {
            for scope in [GrantScope::Once, GrantScope::Session, GrantScope::Always] {
                for target in [
                    GrantTarget::Fingerprint(FP.into()),
                    GrantTarget::Tool("t".into()),
                ] {
                    let (os, ot) = resolve_grant(risk, scope, target.clone(), FP);
                    assert!(
                        dur(os) <= dur(scope),
                        "{risk:?}/{scope:?}: duration widened"
                    );
                    assert!(
                        breadth(&ot) <= breadth(&target),
                        "{risk:?}/{target:?}: target breadth widened"
                    );
                }
            }
        }
    }

    #[test]
    fn covers_for_excludes_external_session_grants_from_unattended_replay() {
        // B5 (cron replay loophole): ActionFingerprint is tool+args only (no
        // session discriminator), so a Session-scope External grant made
        // interactively would otherwise satisfy a byte-identical headless/cron
        // dispatch later in the same app session. covers_for closes that.
        let ledger = ApprovalLedger::new();
        let fp = "fetch_url|https://example.com";
        ledger.grant(GrantTarget::Fingerprint(fp.into()), GrantScope::Session);

        // Attended (a human is present): the session grant covers — unchanged.
        assert!(ledger.covers_for("fetch_url", fp, RiskClass::External, true));
        // UNATTENDED + External: the session grant must NOT satisfy a headless
        // replay — only a fresh Once grant would.
        assert!(
            !ledger.covers_for("fetch_url", fp, RiskClass::External, false),
            "a Session External grant must not cover an unattended (cron/headless) dispatch"
        );
        // Unattended + a non-External risk: session grant still covers, so
        // accept_edits' documented auto-approve-Write-on-cron is unaffected.
        assert!(ledger.covers_for("write_file", fp, RiskClass::Write, false));

        // A fresh Once grant DOES cover even an unattended External call.
        let fp2 = "fetch_url|https://other.com";
        ledger.grant(GrantTarget::Fingerprint(fp2.into()), GrantScope::Once);
        assert!(ledger.covers_for("fetch_url", fp2, RiskClass::External, false));
    }
}
