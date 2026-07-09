//! `FirstUseConfirmHook` — the last link in the `PreToolUse` chain: confirm
//! the first use of a tool that isn't pre-trusted, remember the answer. Spec
//! `docs/tooling-and-skills.md` §3.4, `docs/PLAN.md` §8 M3 item 3/9.
//!
//! Two independent "already OK" sources:
//!  - `seen` — tools explicitly pre-confirmed via `mark_confirmed` (how a body
//!    pre-trusts its safe-by-default tools, e.g. the read-only fs tools). Set
//!    at construction; never written from `on_event`.
//!  - the shared [`ApprovalLedger`] — runtime grants recorded by
//!    `ToolDispatcher` when the user approves an interactive prompt.
//!
//! `on_event` returns `Continue` if either source covers the call, else
//! `Ask`. Crucially it does NOT mark a tool seen merely for having asked (the
//! old placeholder behavior) — "asked" is not "approved". Only an actual user
//! "yes", recorded in the ledger by the dispatcher, flips a later call to
//! `Continue`. That's what makes an unattended agent unable to grant itself a
//! state-changing tool just by attempting it once.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::hooks::approval::{ActionFingerprint, ApprovalLedger};
use crate::hooks::{EventContext, GatingHook, HookEvent, HookResult};

pub struct FirstUseConfirmHook {
    seen: Mutex<HashSet<String>>,
    ledger: Arc<ApprovalLedger>,
}

impl Default for FirstUseConfirmHook {
    fn default() -> Self {
        Self::new()
    }
}

impl FirstUseConfirmHook {
    pub fn new() -> Self {
        Self {
            seen: Mutex::new(HashSet::new()),
            ledger: Arc::new(ApprovalLedger::new()),
        }
    }

    /// Share the dispatcher's approval ledger so a recorded grant flips a
    /// later call to `Continue` (see `crate::hooks::build_pretooluse_chain_full`).
    pub fn with_ledger(mut self, ledger: Arc<ApprovalLedger>) -> Self {
        self.ledger = ledger;
        self
    }

    /// Has this tool already been confirmed in this session?
    pub fn is_confirmed(&self, tool_name: &str) -> bool {
        self.seen.lock().expect("first_use lock poisoned").contains(tool_name)
    }

    /// Explicitly mark a tool confirmed without going through `on_event` —
    /// useful for tests and for a future "pre-approve this tool" setting.
    pub fn mark_confirmed(&self, tool_name: &str) {
        self.seen
            .lock()
            .expect("first_use lock poisoned")
            .insert(tool_name.to_string());
    }

    /// Forget every confirmation. Test-only escape hatch; production code
    /// has no legitimate reason to reset this within a running session.
    #[cfg(test)]
    pub fn reset(&self) {
        self.seen.lock().expect("first_use lock poisoned").clear();
    }
}

impl GatingHook for FirstUseConfirmHook {
    fn name(&self) -> &str {
        "first_use_confirm"
    }

    fn on_event(&self, ctx: &mut EventContext) -> HookResult {
        if ctx.event != HookEvent::PreToolUse {
            return HookResult::Continue;
        }

        // Pre-trusted, or already granted at runtime? Continue. Otherwise ask
        // — WITHOUT marking anything: only an actual approval (recorded in the
        // ledger by the dispatcher) may flip a future call to Continue.
        if self
            .seen
            .lock()
            .expect("first_use lock poisoned")
            .contains(&ctx.tool_name)
        {
            return HookResult::Continue;
        }
        let fp = ActionFingerprint::from_ctx(ctx);
        if self.ledger.covers(&ctx.tool_name, &fp) {
            return HookResult::Continue;
        }
        HookResult::Ask(format!(
            "first use of tool '{}' this session — confirm to proceed",
            ctx.tool_name
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_call_asks() {
        let hook = FirstUseConfirmHook::new();
        let mut ctx = EventContext::pre_tool_use("shell_exec");
        match hook.on_event(&mut ctx) {
            HookResult::Ask(_) => {}
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[test]
    fn asking_does_not_mark_the_tool_seen() {
        // "asked" is not "approved": a bare on_event must not flip a later
        // call to Continue on its own — that would let an unattended agent
        // self-grant a tool just by attempting it once.
        let hook = FirstUseConfirmHook::new();
        let mut c1 = EventContext::pre_tool_use("shell_exec");
        hook.on_event(&mut c1);
        assert!(!hook.is_confirmed("shell_exec"));
        let mut c2 = EventContext::pre_tool_use("shell_exec");
        match hook.on_event(&mut c2) {
            HookResult::Ask(_) => {}
            other => panic!("asking must not self-approve; expected Ask again, got {other:?}"),
        }
    }

    #[test]
    fn different_tools_are_tracked_independently() {
        let hook = FirstUseConfirmHook::new();
        let mut a1 = EventContext::pre_tool_use("shell_exec");
        hook.on_event(&mut a1);
        // A different tool name hasn't been seen yet — still asks.
        let mut b1 = EventContext::pre_tool_use("write_file");
        match hook.on_event(&mut b1) {
            HookResult::Ask(_) => {}
            other => panic!("expected Ask for a distinct tool, got {other:?}"),
        }
    }

    #[test]
    fn mark_confirmed_skips_the_ask() {
        let hook = FirstUseConfirmHook::new();
        hook.mark_confirmed("shell_exec");
        let mut ctx = EventContext::pre_tool_use("shell_exec");
        assert_eq!(hook.on_event(&mut ctx), HookResult::Continue);
    }

    #[test]
    fn a_ledger_grant_flips_the_ask_to_continue() {
        use crate::hooks::approval::{GrantScope, GrantTarget};
        let ledger = Arc::new(ApprovalLedger::new());
        let hook = FirstUseConfirmHook::new().with_ledger(Arc::clone(&ledger));

        let mut ctx = EventContext::pre_tool_use("write_file");
        match hook.on_event(&mut ctx) {
            HookResult::Ask(_) => {}
            other => panic!("expected Ask before a grant, got {other:?}"),
        }
        // A whole-tool session grant now covers it.
        ledger.grant(GrantTarget::Tool("write_file".into()), GrantScope::Session);
        assert_eq!(hook.on_event(&mut ctx), HookResult::Continue);
    }
}
