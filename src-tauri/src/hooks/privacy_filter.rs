//! `PrivacyFilterHook` — wraps the existing §7 `PrivacyGate::check()`
//! verbatim (no reimplementation, no logic changes) so it slots into the
//! unified `PreToolUse` chain as the first gate. Spec
//! `docs/tooling-and-skills.md` §3.4, `docs/PLAN.md` §8 M3 item 3.
//!
//! Mapping from `GateDecision` to `HookResult`:
//!   - `Allow`             → `HookResult::Continue` (chain proceeds)
//!   - `Block(reason)`     → `HookResult::Deny(reason)` (chain stops)
//!   - `ConfirmRequired`   → `HookResult::Ask(reason)`, satisfiable by a grant
//!     for THIS exact tool+args in the shared [`ApprovalLedger`] (see below).
//!   - `RouteLocal`        → `HookResult::Continue`, but `ctx.routing` is
//!     set to `RoutingRequirement::LocalRequired`. This is the load-bearing
//!     bit: `RouteLocal` must never quietly become "allow whatever endpoint
//!     was already picked" — annotating the context and requiring every
//!     caller to run `hooks::routing::enforce_local_routing` before
//!     actually dispatching to an endpoint is what makes that hard rather
//!     than a convention someone can forget.
//!
//! ## H-12 on the tool path: `Ask`, not a terminal `Deny`
//!
//! A `Public`-bound tool call whose canonical `name {args}` text hits the
//! un-tunable structured-secret floor yields `GateDecision::ConfirmRequired`.
//! The message path resolves that with the gate's own text-keyed one-send store
//! (banner → `ipc::confirm_public_send` → re-send). A tool call has no banner,
//! and the floor fires on *ordinary* args more often than one might expect —
//! `rules::detect` flags any IPv4 literal, e-mail address, or long digit run, so
//! `fetch_url {"url":"http://10.0.0.26:8000/x"}` hits it. Mapping that to
//! `Deny` made such a call unrunnable with no way for the user to proceed.
//!
//! So this hook resolves the same verdict through the **approval spine**, the
//! recovery route every other ask-capable hook already uses: it returns
//! `Ask(reason)`, `ToolDispatcher` prompts the human, records the answer in the
//! shared [`ApprovalLedger`] keyed on the call's [`ActionFingerprint`], and
//! re-runs the whole chain — on which this hook sees the grant and continues.
//! Coverage is checked with `covers_for`, so B5's rule still holds: a
//! Session-scope grant for an `External` tool cannot satisfy an UNATTENDED
//! (cron / delegate / headless) replay of the same call.
//!
//! Two consequences worth stating plainly rather than hiding:
//!  - A standing grant the user deliberately recorded for this tool (e.g.
//!    "allow `write_file` for this session") satisfies the privacy confirm too.
//!    That is weaker than the message path's strictly-one-send store, and
//!    weaker than `ProtectedPathHook`'s `covers_once` floor — but `covers_once`
//!    alone is not usable here: the dispatcher only force-pins an extra `Once`
//!    grant for `by == "protected_path"`, so a user answering "allow for this
//!    session" would loop the chain to the round cap and get denied anyway. It
//!    is still strictly stronger than the pre-H-12 behaviour, which allowed
//!    every `Public`-bound tool call silently.
//!  - This hook is FIRST in the chain, ahead of the non-overridable
//!    `SandboxHook` denylist, and it can now `Ask`. Nothing is bypassed — an
//!    approval makes the dispatcher re-run the FULL chain, where `SandboxHook`
//!    denies as before — but a prompt can now be raised for a call the sandbox
//!    floor would refuse anyway. See `hooks::mod`'s ordering note.

use std::sync::Arc;

use crate::agent::gate::{GateDecision, PrivacyGate};
use crate::hooks::approval::{ActionFingerprint, ApprovalLedger};
use crate::hooks::{EventContext, GatingHook, HookEvent, HookResult, RoutingRequirement};

pub struct PrivacyFilterHook {
    gate: PrivacyGate,
    /// Shared with `ToolDispatcher` so an approval recorded there turns this
    /// hook's `Ask` into `Continue` on the re-run. A hook built without
    /// [`Self::with_ledger`] owns a private, always-empty ledger — its `Ask` has
    /// no in-process recovery route, which is correct for the chain builders
    /// that have no dispatcher to prompt with.
    ledger: Arc<ApprovalLedger>,
}

impl PrivacyFilterHook {
    pub fn new(gate: PrivacyGate) -> Self {
        Self {
            gate,
            ledger: Arc::new(ApprovalLedger::new()),
        }
    }

    /// Share the dispatcher's approval ledger (see
    /// `crate::hooks::build_pretooluse_chain_full`).
    pub fn with_ledger(mut self, ledger: Arc<ApprovalLedger>) -> Self {
        self.ledger = ledger;
        self
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

        // B4 (gap CLOSED): tool-action content is gated at the PROFILE's
        // classifier config — the dispatcher loads it for the turn and threads it
        // through `EventContext::classifier_cfg` (mirroring
        // `AgentLoop::process_message`'s per-profile load), so a profile's tool
        // calls and its chat messages now share identical thresholds. (The
        // rules-layer floor for structured PII stays un-tunable and fires
        // regardless of thresholds — unchanged.)
        match self.gate.check(
            &ctx.binding,
            &ctx.content,
            ctx.is_cloud_endpoint,
            &ctx.classifier_cfg,
        ) {
            GateDecision::Allow => HookResult::Continue,
            GateDecision::Block(reason) => HookResult::Deny(reason),
            // H-12: a `Public`-bound TOOL action whose content hits the
            // structured-secret floor. Resolved through the approval spine, NOT
            // the gate's text-keyed store — the dispatcher records the human's
            // answer against the TOOL's fingerprint, so that is what we consult.
            // See the module docs for why this is `Ask` and not a terminal
            // `Deny`, and for what a standing grant can satisfy.
            GateDecision::ConfirmRequired { reason, .. } => {
                let action_fp = ActionFingerprint::from_ctx(ctx);
                if self
                    .ledger
                    .covers_for(&ctx.tool_name, &action_fp, ctx.risk, ctx.attended)
                {
                    HookResult::Continue
                } else {
                    HookResult::Ask(reason)
                }
            }
            GateDecision::RouteLocal => {
                ctx.routing = RoutingRequirement::LocalRequired {
                    reason: "privacy filter: content must not leave this device".to_string(),
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
    use crate::hooks::approval::{GrantScope, GrantTarget};

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
        assert_eq!(
            result,
            HookResult::Continue,
            "RouteLocal must not become Deny"
        );
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

    // ── H-12 on the tool path: Ask + a real recovery route ──────────────────

    /// The exact content the dispatcher builds (`dispatch_inner`'s
    /// `format!("{} {}", call.name, call.args)`) for a call whose ORDINARY args
    /// trip the un-tunable rules floor: `rules::detect` flags any IPv4 literal
    /// as `PiiContact`. Nothing about this call is a "secret the user typed" —
    /// which is exactly why hard-denying it was a regression.
    const FETCH_ARGS: &str = r#"{"url":"http://10.0.0.26:8000/status"}"#;

    fn fetch_ctx() -> EventContext {
        EventContext::pre_tool_use("fetch_url")
            .with_input(crate::tools::ToolInput::new(
                serde_json::from_str(FETCH_ARGS).expect("valid json"),
            ))
            .with_binding(Binding::Public)
            .with_content(format!("fetch_url {FETCH_ARGS}"))
            .with_cloud(true)
    }

    #[test]
    fn a_public_floor_hit_on_a_tool_action_asks_it_does_not_hard_deny() {
        // Precondition: this really is a floor hit (otherwise the test proves
        // nothing about the ConfirmRequired arm).
        assert!(
            !crate::classifier::rules::detect(&format!("fetch_url {FETCH_ARGS}")).is_empty(),
            "fixture must trip the rules floor"
        );
        let h = hook();
        let mut ctx = fetch_ctx();
        match h.on_event(&mut ctx) {
            HookResult::Ask(reason) => assert!(
                !reason.is_empty(),
                "the approval dialog needs something to show"
            ),
            other => panic!(
                "a Public-bound floor hit must be recoverable (Ask), not terminal, got {other:?}"
            ),
        }
    }

    #[test]
    fn an_approval_for_this_exact_action_settles_the_privacy_confirm() {
        let ledger = Arc::new(ApprovalLedger::new());
        let h = PrivacyFilterHook::new(PrivacyGate::new(Arc::new(HeuristicClassifier::new())))
            .with_ledger(Arc::clone(&ledger));

        // Pass 1: no grant → Ask.
        let mut ctx = fetch_ctx();
        assert!(
            matches!(h.on_event(&mut ctx), HookResult::Ask(_)),
            "control: without an approval the hook must ask"
        );

        // The human approves; `ToolDispatcher` records it against the call's
        // ActionFingerprint (Once + fingerprint, per `resolve_grant`).
        let fp = ActionFingerprint::from_ctx(&ctx);
        ledger.grant(GrantTarget::Fingerprint(fp), GrantScope::Once);

        // Pass 2 (the dispatcher's re-run of the full chain): it proceeds.
        let mut ctx = fetch_ctx();
        assert_eq!(
            h.on_event(&mut ctx),
            HookResult::Continue,
            "the approval must settle the confirm — otherwise the chain loops to \
             the round cap and denies"
        );
    }

    #[test]
    fn an_approval_for_a_different_action_does_not_settle_it() {
        // The pin: a grant is per-action. Approving one fetch must not clear the
        // privacy confirm for a fetch of somewhere else.
        let ledger = Arc::new(ApprovalLedger::new());
        let h = PrivacyFilterHook::new(PrivacyGate::new(Arc::new(HeuristicClassifier::new())))
            .with_ledger(Arc::clone(&ledger));

        let other = EventContext::pre_tool_use("fetch_url")
            .with_input(crate::tools::ToolInput::new(
                serde_json::json!({"url": "http://10.0.0.99:8000/status"}),
            ))
            .with_binding(Binding::Public);
        ledger.grant(
            GrantTarget::Fingerprint(ActionFingerprint::from_ctx(&other)),
            GrantScope::Once,
        );

        let mut ctx = fetch_ctx();
        assert!(
            matches!(h.on_event(&mut ctx), HookResult::Ask(_)),
            "a grant pinned to a different action must not clear this one"
        );
    }

    #[test]
    fn an_unattended_external_replay_is_not_settled_by_a_standing_grant() {
        // B5 still holds through this new path: `covers_for`, not `covers`. An
        // interactively-granted SESSION approval for an External (egress) tool
        // must not clear the privacy confirm for a byte-identical cron/headless
        // dispatch of the same call.
        let ledger = Arc::new(ApprovalLedger::new());
        let h = PrivacyFilterHook::new(PrivacyGate::new(Arc::new(HeuristicClassifier::new())))
            .with_ledger(Arc::clone(&ledger));
        let fp = ActionFingerprint::from_ctx(&fetch_ctx());
        ledger.grant(GrantTarget::Fingerprint(fp), GrantScope::Session);

        // ATTENDED (a human is present): the session grant covers.
        let mut attended = fetch_ctx()
            .with_risk(crate::tools::RiskClass::External)
            .with_attended(true);
        assert_eq!(
            h.on_event(&mut attended),
            HookResult::Continue,
            "control: attended + a session grant proceeds"
        );

        // UNATTENDED + External: the same grant must not satisfy the confirm.
        let mut unattended = fetch_ctx()
            .with_risk(crate::tools::RiskClass::External)
            .with_attended(false);
        assert!(
            matches!(h.on_event(&mut unattended), HookResult::Ask(_)),
            "a session grant must not clear an unattended External replay"
        );
    }

    #[test]
    fn a_public_bound_tool_call_to_a_local_endpoint_is_not_confirmed_at_all() {
        // F1: the gate's Public arm now consults `is_cloud_endpoint` like Auto
        // and Private do. Nothing egresses on a local endpoint, so there is
        // nothing to confirm — and nothing for the tool path to deny.
        let h = hook();
        let mut ctx = fetch_ctx().with_cloud(false);
        assert_eq!(
            h.on_event(&mut ctx),
            HookResult::Continue,
            "an on-device tool call must not need a cloud-egress confirmation"
        );
        assert_eq!(ctx.routing, RoutingRequirement::Unconstrained);
    }

    #[test]
    fn tool_action_gates_at_the_profiles_classifier_config_not_the_default() {
        // B4: the hook must consult `ctx.classifier_cfg`. The rules/heuristic
        // classifiers ignore the config (only the trained ONNX ensemble grades
        // on tau), so we use a spy whose `classify_with` DOES respond — a
        // behavioral flip proves the profile's config threaded from the
        // EventContext actually reaches the gate.
        use crate::classifier::{Classification, Classifier, ClassifierConfig, Label};

        struct ConfigSpyClassifier;
        impl Classifier for ConfigSpyClassifier {
            fn classify(&self, _t: &str) -> Classification {
                Classification {
                    label: Label::Public,
                    confidence: 1.0,
                    raw_output: vec![],
                    spans: vec![],
                }
            }
            fn classify_with(&self, _t: &str, cfg: &ClassifierConfig) -> Classification {
                // A "strict" profile (tiny tau_band) → Private; otherwise Public.
                let label = if cfg.tau_band < 0.01 {
                    Label::Private
                } else {
                    Label::Public
                };
                Classification {
                    label,
                    confidence: 1.0,
                    raw_output: vec![],
                    spans: vec![],
                }
            }
        }

        let h = PrivacyFilterHook::new(PrivacyGate::new(Arc::new(ConfigSpyClassifier)));

        // DEFAULT config (tau_band 0.05) → Public → Continue, unconstrained.
        let mut lax = EventContext::pre_tool_use("shell_exec")
            .with_binding(Binding::Auto)
            .with_content("anything")
            .with_cloud(true);
        assert_eq!(h.on_event(&mut lax), HookResult::Continue);
        assert_eq!(
            lax.routing,
            RoutingRequirement::Unconstrained,
            "default config allows"
        );

        // A STRICTER profile config → Private → RouteLocal (routing annotated).
        let strict = ClassifierConfig {
            tau_band: 0.005,
            ..Default::default()
        };
        let mut ctx = EventContext::pre_tool_use("shell_exec")
            .with_binding(Binding::Auto)
            .with_content("anything")
            .with_cloud(true)
            .with_classifier_config(strict);
        assert_eq!(
            h.on_event(&mut ctx),
            HookResult::Continue,
            "RouteLocal maps to Continue"
        );
        assert!(
            ctx.routing.is_local_required(),
            "the profile's stricter config must route the tool action local, got {:?}",
            ctx.routing
        );
    }
}
