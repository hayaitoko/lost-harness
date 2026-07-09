//! `HookChain` integration tests: ordering, deny-wins, short-circuit, and
//! the end-to-end privacy-filter → local-only-routing path. Per-hook unit
//! tests live alongside each hook (`privacy_filter.rs`, `permission.rs`,
//! `sandbox.rs`, `first_use.rs`, `routing.rs`); this file exercises the
//! chain as a whole, matching the M3 test list in `docs/PLAN.md` §8.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::*;
use crate::agent::gate::{Binding, PrivacyGate};
use crate::models::{Provider, ProviderKind};
use crate::classifier::HeuristicClassifier;

// ── test-only hooks for ordering/short-circuit assertions ───────────────

/// A gating hook that always returns a fixed result and records that it
/// ran, so tests can assert both *what* happened and *which hooks fired*.
struct RecordingHook {
    name: &'static str,
    result: HookResult,
    calls: Arc<AtomicUsize>,
}

impl GatingHook for RecordingHook {
    fn name(&self) -> &str {
        self.name
    }
    fn on_event(&self, _ctx: &mut EventContext) -> HookResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.result.clone()
    }
}

fn recording(name: &'static str, result: HookResult) -> (Box<dyn GatingHook>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    (
        Box::new(RecordingHook {
            name,
            result,
            calls: Arc::clone(&calls),
        }),
        calls,
    )
}

// ── ordering + short-circuit ─────────────────────────────────────────────

#[test]
fn all_continue_reaches_the_end_of_the_chain() {
    let (h1, c1) = recording("first", HookResult::Continue);
    let (h2, c2) = recording("second", HookResult::Continue);
    let (h3, c3) = recording("third", HookResult::Continue);

    let mut chain = HookChain::new();
    chain.register_gating(h1);
    chain.register_gating(h2);
    chain.register_gating(h3);

    let mut ctx = EventContext::pre_tool_use("some_tool");
    let (result, denied_by) = chain.run_gating(&mut ctx);

    assert_eq!(result, HookResult::Continue);
    assert_eq!(denied_by, None);
    assert_eq!(c1.load(Ordering::SeqCst), 1);
    assert_eq!(c2.load(Ordering::SeqCst), 1);
    assert_eq!(c3.load(Ordering::SeqCst), 1);
}

#[test]
fn deny_short_circuits_and_later_hooks_never_run() {
    let (h1, c1) = recording("first", HookResult::Continue);
    let (h2, c2) = recording("second", HookResult::Deny("nope".to_string()));
    let (h3, c3) = recording("third", HookResult::Continue);

    let mut chain = HookChain::new();
    chain.register_gating(h1);
    chain.register_gating(h2);
    chain.register_gating(h3);

    let mut ctx = EventContext::pre_tool_use("some_tool");
    let (result, denied_by) = chain.run_gating(&mut ctx);

    match result {
        HookResult::Deny(reason) => assert_eq!(reason, "nope"),
        other => panic!("expected Deny, got {other:?}"),
    }
    assert_eq!(denied_by, Some("second"));
    assert_eq!(c1.load(Ordering::SeqCst), 1, "hook before the deny must run");
    assert_eq!(c2.load(Ordering::SeqCst), 1);
    assert_eq!(
        c3.load(Ordering::SeqCst),
        0,
        "hook after the deny must NOT run — short-circuit"
    );
}

#[test]
fn ask_also_short_circuits() {
    let (h1, _c1) = recording("first", HookResult::Continue);
    let (h2, c2) = recording("second", HookResult::Ask("confirm?".to_string()));
    let (h3, c3) = recording("third", HookResult::Continue);

    let mut chain = HookChain::new();
    chain.register_gating(h1);
    chain.register_gating(h2);
    chain.register_gating(h3);

    let mut ctx = EventContext::pre_tool_use("some_tool");
    let (result, denied_by) = chain.run_gating(&mut ctx);

    match result {
        HookResult::Ask(_) => {}
        other => panic!("expected Ask, got {other:?}"),
    }
    assert_eq!(denied_by, Some("second"));
    assert_eq!(c2.load(Ordering::SeqCst), 1);
    assert_eq!(c3.load(Ordering::SeqCst), 0, "hook after Ask must NOT run");
}

#[test]
fn first_deny_wins_when_multiple_hooks_would_deny() {
    // Only the FIRST would-be-Deny hook should ever get the chance to
    // fire; a later hook that would also deny must never run at all.
    let (h1, c1) = recording("first_denier", HookResult::Deny("first reason".to_string()));
    let (h2, c2) = recording("second_denier", HookResult::Deny("second reason".to_string()));

    let mut chain = HookChain::new();
    chain.register_gating(h1);
    chain.register_gating(h2);

    let mut ctx = EventContext::pre_tool_use("some_tool");
    let (result, denied_by) = chain.run_gating(&mut ctx);

    match result {
        HookResult::Deny(reason) => assert_eq!(reason, "first reason"),
        other => panic!("expected Deny, got {other:?}"),
    }
    assert_eq!(denied_by, Some("first_denier"));
    assert_eq!(c1.load(Ordering::SeqCst), 1);
    assert_eq!(
        c2.load(Ordering::SeqCst),
        0,
        "the second denier must never even run"
    );
}

#[test]
fn modify_rewrites_input_and_the_chain_continues() {
    let modified = ToolInput::new(serde_json::json!({"rewritten": true}));
    let (h1, _c1) = recording("modifier", HookResult::Modify(modified.clone()));
    let (h2, c2) = recording("after", HookResult::Continue);

    let mut chain = HookChain::new();
    chain.register_gating(h1);
    chain.register_gating(h2);

    let mut ctx =
        EventContext::pre_tool_use("some_tool").with_input(ToolInput::new(serde_json::json!({})));
    let (result, _) = chain.run_gating(&mut ctx);

    assert_eq!(result, HookResult::Continue);
    assert_eq!(c2.load(Ordering::SeqCst), 1, "chain continues after Modify");
    assert_eq!(ctx.input, modified, "ctx.input must be rewritten by Modify");
}

#[test]
fn gating_names_reports_registration_order() {
    let (h1, _) = recording("a", HookResult::Continue);
    let (h2, _) = recording("b", HookResult::Continue);
    let mut chain = HookChain::new();
    chain.register_gating(h1);
    chain.register_gating(h2);
    assert_eq!(chain.gating_names(), vec!["a", "b"]);
}

// ── the real ordered PreToolUse chain ────────────────────────────────────

fn real_gate() -> PrivacyGate {
    PrivacyGate::new(Arc::new(HeuristicClassifier::new()))
}

#[test]
fn default_pretooluse_chain_is_in_spec_order() {
    let policy = Box::new(InMemoryPolicySource::new());
    let chain = build_pretooluse_chain(real_gate(), policy);
    assert_eq!(
        chain.gating_names(),
        vec!["privacy_filter", "sandbox", "permission", "first_use_confirm"]
    );
}

#[test]
fn sandbox_denies_even_when_permission_would_allow() {
    // Whole-tool Allow from PermissionHook must not save a call the
    // hardline SandboxHook denylist catches — sandbox runs before
    // permission in the chain and still short-circuits.
    let mut policy = InMemoryPolicySource::new();
    policy.set_mode("shell_exec", PermissionMode::Allow);
    let chain = build_pretooluse_chain(real_gate(), Box::new(policy));

    let mut ctx = EventContext::pre_tool_use("shell_exec")
        .with_binding(Binding::Public) // bypass the privacy filter cleanly
        .with_content("cleanup")
        .with_command_text("rm -rf /");

    let (result, denied_by) = chain.run_gating(&mut ctx);
    match result {
        HookResult::Deny(_) => {}
        other => panic!("expected Deny from the sandbox floor, got {other:?}"),
    }
    assert_eq!(denied_by, Some("sandbox"));
}

#[test]
fn sandbox_runs_before_any_hook_that_can_ask() {
    // A whole-tool "ask" permission mode must not let a hardline-denylist
    // call reach human confirmation (and thus, once Ask-resume is wired
    // up, Tool::run()) without the sandbox floor ever being consulted.
    // SandboxHook must be positioned ahead of PermissionHook so this
    // still denies rather than short-circuiting on Ask first.
    let mut policy = InMemoryPolicySource::new();
    policy.set_mode("shell_exec", PermissionMode::Ask);
    let chain = build_pretooluse_chain(real_gate(), Box::new(policy));

    let mut ctx = EventContext::pre_tool_use("shell_exec")
        .with_binding(Binding::Public)
        .with_content("cleanup")
        .with_command_text("rm -rf /");

    let (result, denied_by) = chain.run_gating(&mut ctx);
    match result {
        HookResult::Deny(_) => {}
        other => panic!(
            "expected Deny from the sandbox floor even under an Ask-mode tool, got {other:?}"
        ),
    }
    assert_eq!(denied_by, Some("sandbox"));
}

#[test]
fn privacy_filter_denies_before_permission_or_sandbox_ever_run() {
    let mut policy = InMemoryPolicySource::new();
    policy.set_mode("shell_exec", PermissionMode::Allow);
    let chain = build_pretooluse_chain(real_gate(), Box::new(policy));

    let mut ctx = EventContext::pre_tool_use("shell_exec")
        .with_binding(Binding::Private)
        .with_content("send this")
        .with_command_text("git status")
        .with_cloud(true);

    let (result, denied_by) = chain.run_gating(&mut ctx);
    match result {
        HookResult::Deny(reason) => assert!(reason.contains("Private binding")),
        other => panic!("expected Deny from the privacy filter, got {other:?}"),
    }
    assert_eq!(denied_by, Some("privacy_filter"));
}

#[test]
fn first_use_confirm_is_the_last_gate_reached_on_a_clean_call() {
    let policy = InMemoryPolicySource::new();
    let chain = build_pretooluse_chain(real_gate(), Box::new(policy));

    let mut ctx = EventContext::pre_tool_use("brand_new_tool")
        .with_binding(Binding::Public)
        .with_content("hello")
        .with_command_text("hello");

    let (result, denied_by) = chain.run_gating(&mut ctx);
    match result {
        HookResult::Ask(_) => {}
        other => panic!("expected Ask from first-use-confirm, got {other:?}"),
    }
    assert_eq!(denied_by, Some("first_use_confirm"));
}

// ── end-to-end: RouteLocal must be enforced, never silently allowed ─────

#[test]
fn local_required_annotation_survives_the_whole_chain_and_blocks_cloud_routing() {
    let mut policy = InMemoryPolicySource::new();
    policy.set_mode("shell_exec", PermissionMode::Allow);
    let chain = build_pretooluse_chain(real_gate(), Box::new(policy));

    // SSN + Auto binding + a cloud endpoint → the privacy filter's
    // RouteLocal must survive being passed through permission/sandbox
    // and first-use-confirm without ever being cleared back to
    // Unconstrained.
    let mut ctx = EventContext::pre_tool_use("shell_exec")
        .with_binding(Binding::Auto)
        .with_content("my SSN is 123-45-6789")
        .with_command_text("send message")
        .with_cloud(true);

    // Pre-mark first-use so the chain doesn't stop on the Ask before we
    // can observe the routing annotation end to end.
    // (We rebuild the chain fresh each test, so this is just documenting
    // intent — the assertion below reads ctx.routing regardless of what
    // run_gating returns.)
    let _ = chain.run_gating(&mut ctx);

    assert!(
        ctx.routing.is_local_required(),
        "RouteLocal must survive the full chain traversal, got {:?}",
        ctx.routing
    );

    // Now prove the annotation is actually enforced: with only a cloud
    // provider available, routing must fail loud, never silently pick
    // the cloud provider.
    let cloud_only = vec![Provider::new(
        "openai",
        "OpenAI",
        "https://api.openai.com/v1",
        Some("sk-test".to_string()),
        ProviderKind::Cloud,
    )];
    let routed = enforce_local_routing(&ctx.routing, &cloud_only);
    assert!(
        routed.is_err(),
        "a local_required request with only cloud endpoints must never succeed"
    );
}
