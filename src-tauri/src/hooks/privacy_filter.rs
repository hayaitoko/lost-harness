//! `PrivacyFilterHook` — wraps the existing §7 `PrivacyGate::check()`
//! verbatim (no reimplementation, no logic changes) so it slots into the
//! unified `PreToolUse` chain as the first gate. Spec
//! `docs/tooling-and-skills.md` §3.4, `docs/PLAN.md` §8 M3 item 3.
//!
//! Mapping from `GateDecision` to `HookResult`:
//!   - `Allow`             → `HookResult::Continue` (chain proceeds)
//!   - `Block(reason)`     → `HookResult::Deny(reason)` (chain stops)
//!   - `RouteLocal`        → `HookResult::Continue`, but `ctx.routing` is
//!     set to `RoutingRequirement::LocalRequired`. This is the load-bearing
//!     bit: `RouteLocal` must never quietly become "allow whatever endpoint
//!     was already picked" — annotating the context and requiring every
//!     caller to run `hooks::routing::enforce_local_routing` before
//!     actually dispatching to an endpoint is what makes that hard rather
//!     than a convention someone can forget.

use crate::agent::gate::{GateDecision, PrivacyGate};
use crate::hooks::{EventContext, GatingHook, HookEvent, HookResult, RoutingRequirement};

pub struct PrivacyFilterHook {
    gate: PrivacyGate,
}

impl PrivacyFilterHook {
    pub fn new(gate: PrivacyGate) -> Self {
        Self { gate }
    }
}

impl GatingHook for PrivacyFilterHook {
    fn name(&self) -> &str {
        "privacy_filter"
    }

    fn on_event(&self, ctx: &mut EventContext) -> HookResult {
        if ctx.event != HookEvent::PreToolUse {
            return HookResult::Continue;
        }

        match self.gate.check(&ctx.binding, &ctx.content, ctx.is_cloud_endpoint) {
            GateDecision::Allow => HookResult::Continue,
            GateDecision::Block(reason) => HookResult::Deny(reason),
            GateDecision::RouteLocal => {
                ctx.routing = RoutingRequirement::LocalRequired {
                    reason: "privacy filter: content must not leave this device"
                        .to_string(),
                };
                HookResult::Continue
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::agent::gate::Binding;
    use crate::classifier::HeuristicClassifier;

    fn hook() -> PrivacyFilterHook {
        PrivacyFilterHook::new(PrivacyGate::new(Arc::new(HeuristicClassifier::new())))
    }

    #[test]
    fn allow_maps_to_continue() {
        let h = hook();
        let mut ctx = EventContext::pre_tool_use("shell_exec")
            .with_binding(Binding::Auto)
            .with_content("what's the weather like")
            .with_cloud(true);
        assert_eq!(h.on_event(&mut ctx), HookResult::Continue);
        assert_eq!(ctx.routing, RoutingRequirement::Unconstrained);
    }

    #[test]
    fn block_maps_to_deny_with_reason() {
        let h = hook();
        let mut ctx = EventContext::pre_tool_use("shell_exec")
            .with_binding(Binding::Private)
            .with_content("anything")
            .with_cloud(true);
        match h.on_event(&mut ctx) {
            HookResult::Deny(reason) => assert!(reason.contains("Private binding")),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn route_local_maps_to_continue_with_local_required_annotation() {
        let h = hook();
        // SSN + cloud + Auto binding → RouteLocal in the underlying gate.
        let mut ctx = EventContext::pre_tool_use("shell_exec")
            .with_binding(Binding::Auto)
            .with_content("my SSN is 123-45-6789")
            .with_cloud(true);
        let result = h.on_event(&mut ctx);
        assert_eq!(result, HookResult::Continue, "RouteLocal must not become Deny");
        assert!(
            ctx.routing.is_local_required(),
            "RouteLocal must annotate ctx.routing, not silently allow, got {:?}",
            ctx.routing
        );
    }

    #[test]
    fn route_local_never_silently_becomes_allow_on_cloud() {
        // Same as above, but explicit about the failure mode we're
        // guarding against: a naive implementation could map RouteLocal
        // to Continue without ever setting ctx.routing, which downstream
        // would be indistinguishable from Allow. Assert the annotation
        // is actually present, not just that the chain didn't Deny.
        let h = hook();
        let mut ctx = EventContext::pre_tool_use("shell_exec")
            .with_binding(Binding::Auto)
            .with_content("my email is a@b.com and my password is hunter2")
            .with_cloud(true);
        h.on_event(&mut ctx);
        assert_ne!(ctx.routing, RoutingRequirement::Unconstrained);
    }

    #[test]
    fn route_local_on_non_cloud_endpoint_stays_unconstrained() {
        // Private text but the endpoint is already local — no need to
        // force routing, the gate itself returns Allow in this case.
        let h = hook();
        let mut ctx = EventContext::pre_tool_use("shell_exec")
            .with_binding(Binding::Auto)
            .with_content("my SSN is 123-45-6789")
            .with_cloud(false);
        assert_eq!(h.on_event(&mut ctx), HookResult::Continue);
        assert_eq!(ctx.routing, RoutingRequirement::Unconstrained);
    }

    #[test]
    fn non_pretooluse_event_is_ignored() {
        let h = hook();
        let mut ctx = EventContext::pre_tool_use("shell_exec")
            .with_binding(Binding::Private)
            .with_content("anything")
            .with_cloud(true);
        ctx.event = HookEvent::PostToolUse;
        assert_eq!(h.on_event(&mut ctx), HookResult::Continue);
    }
}
