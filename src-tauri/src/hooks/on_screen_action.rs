//! C6 / M5 (logic half) — [`OnScreenActionHook`]: the computer-use gate that
//! enforces the two things the generic chain can't know about an on-screen
//! action (m5 design Revision v2):
//!
//! 1. **The fresh-snapshot re-resolve** — the action's semantic target must
//!    still exist RIGHT NOW; a moved/vanished control is a `Deny`, never a
//!    mis-click. (The `ui_*` tool's own `run()` re-resolves a second time just
//!    before synthesis — the double-re-snapshot gate.)
//! 2. **The `covers_once` floor for IRREVERSIBLE targets** (Send/Delete/Buy/…):
//!    like `ProtectedPathHook`, an irreversible actuation is satisfiable ONLY
//!    by a fresh `Once` grant — a Session/Always grant (legal for the tool's
//!    `External` risk on CONSEQUENTIAL targets) can never cover it.
//!
//! What this hook deliberately does NOT do (Fix 2): decide the consequential
//! tier's Ask — that's `PermissionHook`'s job, driven by the tool's static
//! `RiskClass::External`. And it never uses `HookResult::Modify` — the
//! fingerprint was computed from the args BEFORE the chain ran, and must never
//! diverge from what the ledger pinned.
//!
//! Chain placement: appended AFTER the generic gates (see
//! `lib.rs::build_tool_dispatcher`). Correct even though `PermissionHook` runs
//! first: a Session grant lets `PermissionHook` continue, but this hook still
//! demands `covers_once` for an irreversible target — and a `Once` answer is
//! consumed only at the execution arm (after the whole chain), so it is still
//! armed when this hook checks it.

use std::sync::Arc;

use crate::hooks::{
    ActionFingerprint, ApprovalLedger, EventContext, GatingHook, HookEvent, HookResult,
};
use crate::tools::computer_backend::ComputerBackend;
use crate::tools::computer_use::{reversibility, ComputerAction, Reversibility};

pub struct OnScreenActionHook {
    backend: Arc<dyn ComputerBackend>,
    ledger: Arc<ApprovalLedger>,
}

impl OnScreenActionHook {
    pub fn new(backend: Arc<dyn ComputerBackend>) -> Self {
        Self { backend, ledger: Arc::new(ApprovalLedger::new()) }
    }

    /// Share the SAME ledger `Arc` the dispatcher/chain use, so a `Once` grant
    /// recorded by the approval spine is visible here.
    pub fn with_ledger(mut self, ledger: Arc<ApprovalLedger>) -> Self {
        self.ledger = ledger;
        self
    }
}

impl GatingHook for OnScreenActionHook {
    fn name(&self) -> &str {
        "on_screen_action"
    }

    fn on_event(&self, ctx: &mut EventContext) -> HookResult {
        if ctx.event != HookEvent::PreToolUse {
            return HookResult::Continue;
        }
        // Only `ui_*` actions are mine; everything else passes untouched.
        let Some(action) = crate::tools::computer_tools::parse_action(&ctx.tool_name, &ctx.input.args)
        else {
            return HookResult::Continue;
        };
        // Reversible (scroll) — nothing to gate here; the tool is Safe.
        if reversibility(&action) == Reversibility::Reversible {
            return HookResult::Continue;
        }
        // The fresh-snapshot re-resolve: every endpoint must exist RIGHT NOW.
        let targets = match &action {
            ComputerAction::Click { target }
            | ComputerAction::Type { target, .. }
            | ComputerAction::Key { target, .. } => vec![target],
            ComputerAction::Drag { from, to } => vec![from, to],
            _ => vec![],
        };
        let mut resolved_label = None;
        for t in targets {
            match self.backend.resolve(t) {
                Some(r) => resolved_label = Some(r),
                None => {
                    return HookResult::Deny(format!(
                        "on-screen target not found on a fresh snapshot (\"{}\" {} in {}) — it moved or vanished",
                        t.label, t.role, t.app
                    ))
                }
            }
        }
        match reversibility(&action) {
            Reversibility::Irreversible => {
                // The Once-only floor: satisfiable ONLY by a fresh Once grant
                // (covers_once ignores Session/Always — ProtectedPathHook's
                // exact discipline).
                let fp = ActionFingerprint::from_ctx(ctx);
                if self.ledger.covers_once(&fp) {
                    HookResult::Continue
                } else {
                    let what = resolved_label
                        .map(|r| format!("{} \"{}\" in {}", r.role, r.label, r.app))
                        .unwrap_or_else(|| "this control".to_string());
                    HookResult::Ask(format!(
                        "This would actuate the {what} — an IRREVERSIBLE action (its label is in the \
                         fail-safe set). Confirm this exact action once."
                    ))
                }
            }
            // Consequential: defer to PermissionHook (the tool's External risk
            // already forced an Ask / a fingerprint-pinned session grant there).
            Reversibility::Consequential | Reversibility::Reversible => HookResult::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::{GrantScope, GrantTarget};
    use crate::tools::computer_backend::MockComputerBackend;
    use crate::tools::ToolInput;
    use serde_json::json;

    fn hook_with(
        elements: Vec<(&'static str, &'static str, &'static str)>,
    ) -> (OnScreenActionHook, Arc<ApprovalLedger>, Arc<MockComputerBackend>) {
        let mock = Arc::new(MockComputerBackend::with_elements(elements));
        let ledger = Arc::new(ApprovalLedger::new());
        let hook = OnScreenActionHook::new(mock.clone() as Arc<dyn ComputerBackend>)
            .with_ledger(Arc::clone(&ledger));
        (hook, ledger, mock)
    }

    fn click_ctx(label: &str) -> EventContext {
        EventContext::pre_tool_use("ui_click")
            .with_input(ToolInput::new(json!({"app": "Mail", "role": "button", "label": label})))
    }

    #[test]
    fn non_ui_tools_pass_untouched() {
        let (hook, _, _) = hook_with(vec![]);
        let mut ctx = EventContext::pre_tool_use("read_file")
            .with_input(ToolInput::new(json!({"path": "x"})));
        assert_eq!(hook.on_event(&mut ctx), HookResult::Continue);
    }

    #[test]
    fn scroll_is_never_gated_here() {
        let (hook, _, _) = hook_with(vec![]); // even with NO resolvable elements
        let mut ctx = EventContext::pre_tool_use("ui_scroll")
            .with_input(ToolInput::new(json!({"app": "Mail", "role": "scrollArea", "label": "list"})));
        assert_eq!(hook.on_event(&mut ctx), HookResult::Continue);
    }

    #[test]
    fn a_vanished_target_is_denied_on_the_fresh_snapshot() {
        let (hook, _, mock) = hook_with(vec![("Mail", "button", "Reply")]);
        mock.vanish_all();
        let mut ctx = click_ctx("Reply");
        assert!(matches!(hook.on_event(&mut ctx), HookResult::Deny(ref r) if r.contains("moved or vanished")));
    }

    #[test]
    fn consequential_click_defers_to_the_permission_hook() {
        let (hook, _, _) = hook_with(vec![("Mail", "button", "Reply")]);
        let mut ctx = click_ctx("Reply");
        assert_eq!(
            hook.on_event(&mut ctx),
            HookResult::Continue,
            "consequential actuation is PermissionHook's Ask, not mine"
        );
    }

    #[test]
    fn irreversible_click_needs_a_fresh_once_grant_session_never_covers() {
        let (hook, ledger, _) = hook_with(vec![("Mail", "button", "Send")]);
        let mut ctx = click_ctx("Send");
        // No grant → Ask.
        assert!(matches!(hook.on_event(&mut ctx), HookResult::Ask(_)));
        // A SESSION grant for this exact fingerprint must NOT satisfy the floor.
        let fp = ActionFingerprint::from_ctx(&ctx);
        ledger.grant(GrantTarget::Fingerprint(fp.clone()), GrantScope::Session);
        assert!(
            matches!(hook.on_event(&mut ctx), HookResult::Ask(_)),
            "a standing grant can never cover an irreversible actuation"
        );
        // A fresh ONCE grant does.
        ledger.grant(GrantTarget::Fingerprint(fp), GrantScope::Once);
        assert_eq!(hook.on_event(&mut ctx), HookResult::Continue);
    }
}
