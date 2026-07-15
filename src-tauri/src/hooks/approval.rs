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

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use sha2::{Digest, Sha256};

use crate::hooks::EventContext;

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
            // `Once => action`, so this arm shouldn't be reached in practice;
            // it's the defensive floor.
            (GrantScope::Once, GrantTarget::Tool(_)) => {}
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
}

/// The user's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve(GrantScope, GrantTarget),
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
}
