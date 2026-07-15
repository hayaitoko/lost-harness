//! `ProtectedPathHook` — the always-`Ask` floor for a hardcoded list of
//! workspace paths (`.git/`, `config/secrets`, `.env`, `.ssh/`). Sits
//! between `SandboxHook` and `PermissionHook` in the `PreToolUse` chain.
//! Spec `docs/tool-system-build-plan.md` "## 3. Protected-paths
//! always-Ask floor hook" (lines 408–520).
//!
//! Like `SandboxHook` this is deliberately NOT config-driven — the path
//! list is hardcoded in `PROTECTED` below, no `PolicySource` or
//! per-profile setting can ever narrow or broaden it. The point is to
//! stop a future `Allow`-rule (Q8) or `shell_exec` (Q2) from reaching
//! these paths silently: anything matching a protected substring is
//! forced to `Ask`, and that `Ask` is satisfiable ONLY by a fresh
//! `Once` grant for the exact action — never by a `Session`/`Always`
//! grant — so the floor can't be silently widened to standing coverage.
//!
//! `ApprovalLedger::covers_once` (the only ledger method this hook
//! consults) is the single mechanism that enforces "Once-only" — a
//! `Session`/`Tool` grant lives in `session_fps`/`session_tools` and is
//! invisible to `covers_once`, so a user clicking "Allow for this
//! session" on a protected-path prompt gets an independent `Once`
//! piggyback in the dispatcher that lets THIS EXACT call through, but
//! never the next call to a different protected path. See
//! `dispatch.rs`'s `Approve` arm.

use std::sync::Arc;

use crate::hooks::approval::{ActionFingerprint, ApprovalLedger};
use crate::hooks::{EventContext, GatingHook, HookEvent, HookResult};

/// One entry in the protected path list: a human-readable label plus a
/// matcher over `EventContext::command_text`. Same shape as
/// `SandboxHook`'s `DenylistEntry` — just Ask-capable and ledger-aware
/// instead of Deny-only.
struct ProtectedPathEntry {
    label: &'static str,
    matches: fn(&str) -> bool,
}

/// The non-configurable floor. Every pattern here is spelled out
/// explicitly in the M3 spec. Substring matching on lowercased
/// `command_text` is recall-biased by design — a benign `write_file`
/// whose *content* (not path) happens to mention `.git/` or `.env` will
/// also trigger an `Ask`. This is the same accepted tradeoff as
/// `SandboxHook`'s denylist; we don't parse JSON to scope matching to
/// only the `path` key.
const PROTECTED: &[ProtectedPathEntry] = &[
    ProtectedPathEntry {
        label: "the .git directory",
        matches: |s| normalize(s).contains(".git/"),
    },
    ProtectedPathEntry {
        label: "config/secrets",
        matches: |s| normalize(s).contains("config/secrets"),
    },
    ProtectedPathEntry {
        label: "a .env file",
        matches: |s| normalize(s).contains(".env"),
    },
    ProtectedPathEntry {
        label: "an .ssh directory",
        matches: |s| normalize(s).contains(".ssh/"),
    },
];

fn normalize(s: &str) -> String {
    s.to_ascii_lowercase()
}

/// The non-overridable always-`Ask` floor. Like `SandboxHook` it takes
/// no config — that IS the enforced invariant. The optional
/// `with_ledger` builder (mirroring `FirstUseConfirmHook`) lets the
/// app share the dispatcher's `ApprovalLedger` so a recorded
/// `Once`-on-this-fingerprint grant flips a later call to `Continue`.
/// Without `.with_ledger`, the hook falls back to its own empty ledger
/// and asks every time.
pub struct ProtectedPathHook {
    ledger: Arc<ApprovalLedger>,
}

impl Default for ProtectedPathHook {
    fn default() -> Self {
        Self::new()
    }
}

impl ProtectedPathHook {
    pub fn new() -> Self {
        Self {
            ledger: Arc::new(ApprovalLedger::new()),
        }
    }

    /// Share the dispatcher's approval ledger so a `Once`+`Fingerprint`
    /// grant recorded by the dispatcher turns a later call into
    /// `Continue`. See `crate::hooks::build_pretooluse_chain_full`.
    pub fn with_ledger(mut self, ledger: Arc<ApprovalLedger>) -> Self {
        self.ledger = ledger;
        self
    }
}

impl GatingHook for ProtectedPathHook {
    fn name(&self) -> &str {
        "protected_path"
    }

    fn on_event(&self, ctx: &mut EventContext) -> HookResult {
        if ctx.event != HookEvent::PreToolUse {
            return HookResult::Continue;
        }
        for entry in PROTECTED {
            if (entry.matches)(&ctx.command_text) {
                // The whole mechanism: `covers_once` ignores `Session`/
                // `Always` grants, so the floor is satisfiable only by
                // a fresh `Once`+`Fingerprint` grant for this exact
                // action. See `dispatch.rs` for the forced-Once
                // piggyback that pins a Once grant when the user
                // answers a protected-path prompt with anything broader
                // than Once.
                let fp = ActionFingerprint::from_ctx(ctx);
                if self.ledger.covers_once(&fp) {
                    return HookResult::Continue;
                }
                return HookResult::Ask(format!(
                    "'{}' touches a protected path ({}) — requires a fresh one-time confirmation, \
                     even if this tool is otherwise allowed",
                    ctx.tool_name, entry.label
                ));
            }
        }
        HookResult::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::approval::{GrantScope, GrantTarget};

    fn ctx(cmd: &str) -> EventContext {
        EventContext::pre_tool_use("write_file").with_command_text(cmd)
    }

    #[test]
    fn asks_on_git_path() {
        let hook = ProtectedPathHook::new();
        let mut c = ctx("write_file {\"path\":\".git/config\"}");
        match hook.on_event(&mut c) {
            HookResult::Ask(_) => {}
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[test]
    fn asks_on_config_secrets() {
        let hook = ProtectedPathHook::new();
        let mut c = ctx("write_file {\"path\":\"config/secrets/api_key\"}");
        match hook.on_event(&mut c) {
            HookResult::Ask(_) => {}
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[test]
    fn asks_on_dotenv() {
        let hook = ProtectedPathHook::new();
        let mut c = ctx("write_file {\"path\":\".env\"}");
        match hook.on_event(&mut c) {
            HookResult::Ask(_) => {}
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[test]
    fn asks_on_ssh_dir() {
        let hook = ProtectedPathHook::new();
        let mut c = ctx("write_file {\"path\":\".ssh/authorized_keys\"}");
        match hook.on_event(&mut c) {
            HookResult::Ask(_) => {}
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[test]
    fn allows_benign_path() {
        let hook = ProtectedPathHook::new();
        let mut c = ctx("write_file {\"path\":\"note.txt\"}");
        assert_eq!(hook.on_event(&mut c), HookResult::Continue);
    }

    #[test]
    fn a_once_grant_for_the_exact_fingerprint_covers_it() {
        // Share a ledger with the hook, grant Once+Fingerprint for the
        // call we're about to make, assert the floor now returns
        // Continue instead of Ask.
        let ledger = Arc::new(ApprovalLedger::new());
        let hook = ProtectedPathHook::new().with_ledger(Arc::clone(&ledger));

        // First call: bare hook, no grant — must Ask.
        let mut c1 = ctx("write_file {\"path\":\".git/config\"}");
        let fp = ActionFingerprint::from_ctx(&c1);
        match hook.on_event(&mut c1) {
            HookResult::Ask(_) => {}
            other => panic!("expected Ask before any grant, got {other:?}"),
        }

        // Pin a Once+Fingerprint grant for that exact call.
        ledger.grant(GrantTarget::Fingerprint(fp), GrantScope::Once);

        // Second call (same canonical text, same fingerprint): now
        // covered.
        let mut c2 = ctx("write_file {\"path\":\".git/config\"}");
        assert_eq!(hook.on_event(&mut c2), HookResult::Continue);
    }

    #[test]
    fn a_session_tool_grant_does_not_cover_it() {
        // The whole point of the floor: a Session/Tool grant that
        // PermissionHook would happily consume must NOT satisfy
        // ProtectedPathHook. The hook only consults `covers_once`,
        // which only looks at `once_fps`.
        let ledger = Arc::new(ApprovalLedger::new());
        let hook = ProtectedPathHook::new().with_ledger(Arc::clone(&ledger));

        // Grant a Session/Tool(write_file) — broad standing coverage
        // for the whole tool.
        ledger.grant(GrantTarget::Tool("write_file".into()), GrantScope::Session);

        let mut c = ctx("write_file {\"path\":\".git/config\"}");
        match hook.on_event(&mut c) {
            HookResult::Ask(_) => {}
            other => panic!(
                "a Session/Tool grant must not satisfy the protected-path floor, got {other:?}"
            ),
        }

        // And a Session/Fingerprint grant also doesn't satisfy it —
        // covers_once only inspects once_fps.
        let mut c2 = ctx("write_file {\"path\":\".git/config\"}");
        let fp = ActionFingerprint::from_ctx(&c2);
        ledger.grant(GrantTarget::Fingerprint(fp), GrantScope::Session);
        match hook.on_event(&mut c2) {
            HookResult::Ask(_) => {}
            other => panic!(
                "a Session/Fingerprint grant must not satisfy the protected-path floor, got {other:?}"
            ),
        }
    }
}
