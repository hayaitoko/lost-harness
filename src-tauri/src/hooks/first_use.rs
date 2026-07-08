//! `FirstUseConfirmHook` — the last link in the `PreToolUse` chain. Asks
//! once per tool, remembers the approval after that. Spec
//! `docs/tooling-and-skills.md` §3.4, `docs/PLAN.md` §8 M3 item 3.
//!
//! Simplification for this milestone: there is no real approval callback
//! wired up yet (that's the in-chat confirmation dialog, a later UI
//! concern), so "remembers approval" is modeled as "remembers having
//! asked" — the first `on_event` for a given tool name returns `Ask` and
//! marks that tool as seen; every subsequent call for the same tool name
//! returns `Continue`. When the real confirmation UI lands, swap
//! `mark_seen` (called eagerly here) for a `mark_approved` called only on
//! an actual user "yes" — the trait/chain shape doesn't need to change.

use std::collections::HashSet;
use std::sync::Mutex;

use crate::hooks::{EventContext, GatingHook, HookEvent, HookResult};

pub struct FirstUseConfirmHook {
    seen: Mutex<HashSet<String>>,
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
        }
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

        let mut seen = self.seen.lock().expect("first_use lock poisoned");
        if seen.contains(&ctx.tool_name) {
            HookResult::Continue
        } else {
            seen.insert(ctx.tool_name.clone());
            HookResult::Ask(format!(
                "first use of tool '{}' this session — confirm to proceed",
                ctx.tool_name
            ))
        }
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
    fn second_call_for_same_tool_continues() {
        let hook = FirstUseConfirmHook::new();
        let mut ctx1 = EventContext::pre_tool_use("shell_exec");
        hook.on_event(&mut ctx1);
        let mut ctx2 = EventContext::pre_tool_use("shell_exec");
        assert_eq!(hook.on_event(&mut ctx2), HookResult::Continue);
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
    fn is_confirmed_reflects_state() {
        let hook = FirstUseConfirmHook::new();
        assert!(!hook.is_confirmed("shell_exec"));
        let mut ctx = EventContext::pre_tool_use("shell_exec");
        hook.on_event(&mut ctx);
        assert!(hook.is_confirmed("shell_exec"));
    }
}
