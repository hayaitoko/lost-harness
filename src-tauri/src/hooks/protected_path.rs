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

use std::path::PathBuf;
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
    /// The fs tools' workspace root, shared so this hook can canonicalize a
    /// call's `path` arg the SAME way `tools::fs::resolve_within` /
    /// `resolve_within_new` do (symlinks followed) before deciding whether
    /// the REAL on-disk target is protected. `None` (the default) means the
    /// signal is unavailable; the raw-text match below still runs
    /// unconditionally, so a future non-fs tool (shell_exec, an Allow-rule
    /// target) is caught exactly as before.
    workspace_root: Option<PathBuf>,
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
            workspace_root: None,
        }
    }

    /// Share the dispatcher's approval ledger so a `Once`+`Fingerprint`
    /// grant recorded by the dispatcher turns a later call into
    /// `Continue`. See `crate::hooks::build_pretooluse_chain_full`.
    pub fn with_ledger(mut self, ledger: Arc<ApprovalLedger>) -> Self {
        self.ledger = ledger;
        self
    }

    /// Give the hook the workspace root shared with the fs tools. Closes the
    /// symlink / non-canonical-path bypass: a workspace symlink like
    /// `alias -> .git` never mentions `.git/` in the raw command text, but
    /// `resolve_within` / `resolve_within_new` (which the fs tools call
    /// before ever touching disk) follow it to the real directory — this
    /// makes the hook see the same resolved target the tool will act on, so
    /// the floor fires on the real path, not the alias.
    pub fn with_workspace_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.workspace_root = Some(root.into());
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

        // Second signal: the REAL, symlink-resolved on-disk target of the
        // call's "path" arg. The raw-text match below catches a literal
        // ".git/"/".env"/etc. in the command; this catches the case the raw
        // text can't see — a workspace symlink `alias -> .git` whose name
        // never contains the protected substring but whose resolved target
        // does. Only computed for tools that carry a workspace-relative
        // "path" arg and only when a workspace root is wired; everything
        // else yields `None` and relies solely on the raw-text match,
        // unchanged from before.
        let resolved_text: Option<String> = self.workspace_root.as_deref().and_then(|root| {
            let rel = ctx.input.args.get("path")?.as_str()?;
            let resolved = crate::tools::fs::canonicalize_best_effort(root, rel)?;
            let mut s = resolved.to_string_lossy().into_owned();
            // Normalize a bare-directory target (e.g. `alias` itself, which
            // resolves straight to `.../.git`) so it still matches the
            // trailing-slash patterns (".git/", ".ssh/").
            if resolved.is_dir() {
                s.push('/');
            }
            Some(s)
        });

        for entry in PROTECTED {
            let raw_hit = (entry.matches)(&ctx.command_text);
            let resolved_hit = resolved_text
                .as_deref()
                .map(|s| (entry.matches)(s))
                .unwrap_or(false);
            if raw_hit || resolved_hit {
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

    #[cfg(unix)]
    #[test]
    fn symlink_to_git_is_caught_via_canonical_resolution_even_though_raw_text_never_mentions_git() {
        // The isolated, load-bearing regression for the symlink bypass: the
        // hook must flip to Ask purely because of the new resolved-path
        // signal, with an explicit sanity check that the raw text alone
        // would NOT have triggered it. Pinpoints which code path (raw vs.
        // resolved) is doing the work — the dispatch.rs integration test is
        // an end-to-end smoke test on top of this.
        let root =
            std::env::temp_dir().join(format!("lhp-protected-symlink-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::os::unix::fs::symlink(root.join(".git"), root.join("alias")).unwrap();

        let hook = ProtectedPathHook::new().with_workspace_root(&root);

        let raw_text = "write_file {\"path\":\"alias/pwned\"}";
        assert!(
            !raw_text.to_ascii_lowercase().contains(".git/"),
            "sanity: the symlink name itself must not contain the protected substring"
        );

        let mut c = EventContext::pre_tool_use("write_file")
            .with_command_text(raw_text)
            .with_input(crate::tools::ToolInput::new(
                serde_json::json!({"path": "alias/pwned"}),
            ));

        match hook.on_event(&mut c) {
            HookResult::Ask(_) => {}
            other => panic!("expected Ask via canonical-path resolution, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
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
