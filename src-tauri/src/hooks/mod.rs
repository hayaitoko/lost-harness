//! §3.4 Hooks — the native, in-process checkpoint chain that unifies the
//! privacy filter (§7), tool permissions (§10), the sandbox floor (§11),
//! and first-use confirmation into ONE ordered decision. Spec
//! `docs/tooling-and-skills.md` §3.4 / `docs/PLAN.md` §8 (M3 build order
//! items 2–4).
//!
//! Not shell scripts — a native Rust trait, compiled identically into both
//! the Tauri app and (later) the headless server companion, so a future
//! rule is one new `GatingHook` impl instead of a scattered four-file edit.
//!
//! ```text
//! PreToolUse
//!   │
//!   ▼
//! [PrivacyFilterHook] ── wraps agent::gate::PrivacyGate::check()
//!   │  Deny(reason)  ─────────────────────────────▶ short-circuit, stop
//!   │  Continue (Allow, or RouteLocal annotated onto ctx.routing)
//!   ▼
//! [SandboxHook] ── non-overridable hardline denylist
//!   │  Deny ───────────────────────────────────────▶ short-circuit, stop
//!   │  Continue
//!   ▼
//! [ProtectedPathHook] ── non-overridable always-Ask floor for .git/,
//!   │                    config/secrets, .env, .ssh/ paths
//!   │  Ask (Once-only) ───────────────────────────▶ short-circuit, stop
//!   │  Continue
//!   ▼
//! [PermissionHook] ── tri-state mode + pattern rules
//!   │  Deny/Ask ───────────────────────────────────▶ short-circuit, stop
//!   │  Continue
//!   ▼
//! [FirstUseConfirmHook] ── ask once per tool, remember after
//!   │  Ask (first time) ───────────────────────────▶ short-circuit, stop
//!   ▼
//! Continue ⇒ Tool::run() may proceed
//! ```
//!
//! `SandboxHook` is deliberately positioned immediately after the (Deny-only,
//! never-Ask) `PrivacyFilterHook` and ahead of every hook capable of
//! returning `Ask` (`ProtectedPathHook`, `PermissionHook`,
//! `FirstUseConfirmHook`). `run_gating` short-circuits on the first
//! `Deny`/`Ask`, so if an Ask-capable hook ran before the hardline floor, a
//! whole-tool "ask" permission mode (or an "ask" pattern rule) could reach
//! human confirmation — and eventually `Tool::run()` — without the
//! non-overridable denylist ever being consulted. Putting `SandboxHook`
//! first among the fallible hooks means it always runs on every
//! `PreToolUse` event, regardless of what any later hook decides. See
//! `default_pretooluse_chain_is_in_spec_order` and
//! `sandbox_runs_before_any_hook_that_can_ask` in `hooks::tests`.
//!
//! `ProtectedPathHook` follows the same non-overridable invariant — its
//! path list is hardcoded, no config can narrow or broaden it, and its
//! `Ask` is satisfiable only by a fresh `Once` grant (it consults
//! `ApprovalLedger::covers_once`, which ignores `Session`/`Always`
//! grants), so a future `Allow` rule or `shell_exec` can never reach
//! these paths silently.
//!
//! Live wiring of real tool calls through this chain lands later in M3
//! once actual tool implementations exist (`docs/PLAN.md` §8, M3 item 6+);
//! this module ships the trait + chain + the four gates and is exercised
//! by unit tests today.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::agent::gate::Binding;
use crate::tools::{RiskClass, ToolInput};

pub mod approval;
pub mod audit;
pub mod first_use;
pub mod headless;
pub mod permission;
pub mod privacy_filter;
pub mod protected_path;
pub mod routing;
pub mod sandbox;
pub mod session_mode;

pub use approval::{
    persist_rule_allowed, resolve_grant, ActionFingerprint, ApprovalDecision, ApprovalLedger,
    ApprovalPrompter, ApprovalRequest, GrantScope, GrantTarget,
};
pub use audit::{
    outcome_gate_by, outcome_label, truncate_args, AuditEntry, AuditObserverHook, AuditWriter,
    CAPTURED_ARGS_CAP, StorageAuditWriter,
};
pub use first_use::FirstUseConfirmHook;
pub use headless::{ApprovalQueue, QueuedApproval, QueueingPrompter};
pub use permission::{
    InMemoryPolicySource, LayeredPolicySource, PermissionHook, PermissionMode, PolicySource,
    SqlitePolicySource, StorageToolRuleWriter, ToolRule, ToolRuleWriter,
};
pub use privacy_filter::PrivacyFilterHook;
pub use protected_path::ProtectedPathHook;
pub use routing::{enforce_local_routing, LocalRoutingViolation};
pub use sandbox::{SandboxConfig, SandboxHook, SandboxNetworkConfig};
pub use session_mode::{SessionMode, SessionModeHook};

// ── HookEvent ────────────────────────────────────────────────────────────

/// Lifecycle moments a hook can react to. Only `PreToolUse` is exercised by
/// this milestone's gating chain; the rest are reserved per spec §3.4 so
/// later milestones (cron ledger, app-launch defaults) don't need a new
/// enum variant threaded through every call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEvent {
    /// Immediately before a tool call executes. The only event the M3
    /// gating chain runs against.
    PreToolUse,
    /// Immediately after a tool call completes (observer lane candidate:
    /// TRM logging, telemetry). Reserved — not fired yet.
    PostToolUse,
    /// A scheduled cron job has fired and needs to claim-or-skip against
    /// the run ledger (spec §3.4 / PLAN.md §8 server track). Reserved.
    CronFired,
    /// A cron job's run completed and needs to enqueue its result or
    /// requeue on failure. Reserved.
    CronCompleted,
    /// The app (or server) has finished booting. Reserved for per-profile
    /// default seeding (memory tags, seats, permissions).
    AppLaunch,
}

// ── RoutingRequirement ───────────────────────────────────────────────────

/// Whether the current request is free to go to any available endpoint, or
/// has been flagged (by `PrivacyFilterHook`, or any future hook) as
/// "must not leave this host." This is the explicit, typed carrier that
/// keeps `GateDecision::RouteLocal` from silently degrading into
/// "allow on cloud" — see `enforce_local_routing` in `hooks::routing`,
/// which is the only thing allowed to resolve this into an actual
/// endpoint choice, and which fails loudly instead of falling through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutingRequirement {
    /// No routing constraint from the hook chain — any endpoint may be
    /// selected by the caller's normal picking logic.
    Unconstrained,
    /// This request must be served by a local/private endpoint. `reason`
    /// is a user-safe explanation (surfaced in the loud failure if no
    /// local endpoint turns out to be available).
    LocalRequired { reason: String },
}

impl Default for RoutingRequirement {
    fn default() -> Self {
        RoutingRequirement::Unconstrained
    }
}

impl RoutingRequirement {
    pub fn is_local_required(&self) -> bool {
        matches!(self, RoutingRequirement::LocalRequired { .. })
    }
}

// ── EventContext ─────────────────────────────────────────────────────────

/// Everything a hook needs to decide. One struct threaded through the
/// whole chain; hooks may read (all fields) and — via `&mut` — annotate it
/// (`routing`) or rewrite the tool input (`Modify`). Kept flat rather than
/// per-event-variant enums: every field is meaningful for `PreToolUse`
/// today, and the reserved events (§ above) can grow their own fields
/// later without breaking this shape.
#[derive(Debug, Clone)]
pub struct EventContext {
    pub event: HookEvent,
    /// Stable tool identifier, e.g. `"shell_exec"` — matches `Tool::name()`.
    pub tool_name: String,
    /// The tool's structured input.
    pub input: ToolInput,
    /// A canonical string form of the call used for pattern matching
    /// (`PermissionHook` rules, `SandboxHook`'s denylist) — e.g. the raw
    /// shell command for a `shell_exec` call, or a target path for a
    /// write. Defaults to a copy of `content` when built via
    /// `with_content`, but can be set independently.
    pub command_text: String,
    /// The conversation-scoped privacy binding (§7/§12).
    pub binding: Binding,
    /// The plaintext content being evaluated by the privacy filter (the
    /// user message, or whatever text a tool call would expose to a
    /// model/network boundary).
    pub content: String,
    /// Whether the endpoint this call would otherwise reach is a cloud
    /// endpoint (`agent::egress::is_private_endpoint` is the source of
    /// truth upstream of this).
    pub is_cloud_endpoint: bool,
    pub conversation_id: String,
    /// The active profile this call runs under. Drives per-profile persisted
    /// `tool_rules` resolution (`SqlitePolicySource`). Empty string = no
    /// profile (tests, or any path that didn't set one) → no persisted rules,
    /// pre-Q8 behavior. Set by the dispatcher from `ExecCtx.profile`.
    pub profile: String,
    /// Set by `PermissionHook` when it resolves the call to an EXPLICIT
    /// `Allow` (a whole-tool `Allow` mode or a matching allow-rule — including
    /// a persisted Q8 `tool_rules` "always allow"). A downstream
    /// confirmation hook (`FirstUseConfirmHook`) honors this and skips its ask:
    /// an explicit allow-policy is a definitive "yes", not the "no opinion"
    /// fall-through, so first-use confirmation shouldn't second-guess it.
    /// Communicated via the shared `ctx` (NOT a chain short-circuit), so the
    /// non-overridable Sandbox/ProtectedPath floors — which run BEFORE
    /// `PermissionHook` — are unaffected.
    pub policy_allowed: bool,
    /// The resolved tool's [`RiskClass`], stamped by the dispatcher. Drives the
    /// session-mode gate (`plan` denies risk > `Safe`; `accept_edits`
    /// auto-approves only `Write`). Defaults to `Safe` for contexts that don't
    /// set it — the safe direction (a mode never mistakes an unknown call for a
    /// mutation to auto-approve).
    pub risk: RiskClass,
    /// The conversation's [`SessionMode`] (Q11), stamped by the dispatcher from
    /// `ExecCtx.session_mode`. Defaults to `Normal` (no-op).
    pub session_mode: SessionMode,
    /// Set by `PrivacyFilterHook` (or any future hook) when this request
    /// must not leave the device. See `RoutingRequirement`.
    pub routing: RoutingRequirement,
    /// The resolved per-profile classifier config (PLAN §11), loaded by the
    /// dispatcher and consulted by `PrivacyFilterHook` (B4). Defaults to
    /// `ClassifierConfig::default()` so contexts/tests that don't set it gate at
    /// the default thresholds exactly as before — the load-bearing back-compat.
    pub classifier_cfg: crate::classifier::ClassifierConfig,
    /// Whether a HUMAN is present for this dispatch (B5). `true` for the
    /// interactive dispatcher (an approver is wired); `false` for cron/delegate/
    /// headless sub-dispatchers (`approver: None`). Used with `risk` to stop an
    /// interactively-granted Session-scope `External` approval from silently
    /// satisfying a later byte-identical headless/cron dispatch. Defaults `true`
    /// (the safe/interactive assumption).
    pub attended: bool,
}

impl EventContext {
    /// Build a minimal `PreToolUse` context. Chain via the `with_*`
    /// helpers to fill in only what a given test/call site needs.
    pub fn pre_tool_use(tool_name: impl Into<String>) -> Self {
        Self {
            event: HookEvent::PreToolUse,
            tool_name: tool_name.into(),
            input: ToolInput::empty(),
            command_text: String::new(),
            binding: Binding::Auto,
            content: String::new(),
            is_cloud_endpoint: false,
            conversation_id: String::new(),
            profile: String::new(),
            policy_allowed: false,
            risk: RiskClass::Safe,
            session_mode: SessionMode::Normal,
            routing: RoutingRequirement::Unconstrained,
            classifier_cfg: crate::classifier::ClassifierConfig::default(),
            attended: true,
        }
    }

    /// Build a minimal `PostToolUse` context. Reserved for the
    /// `ObserverHook` migration (item 5 + the future
    /// `HookChain::notify_observers` swap): the audit observer is
    /// wired today via the dispatcher's direct `write_audit` call, but
    /// the same struct shape will be the on-the-wire EventContext
    /// when notify_observers eventually carries the outcome.
    pub fn post_tool_use(tool_name: impl Into<String>) -> Self {
        Self {
            event: HookEvent::PostToolUse,
            tool_name: tool_name.into(),
            input: ToolInput::empty(),
            command_text: String::new(),
            binding: Binding::Auto,
            content: String::new(),
            is_cloud_endpoint: false,
            conversation_id: String::new(),
            profile: String::new(),
            policy_allowed: false,
            risk: RiskClass::Safe,
            session_mode: SessionMode::Normal,
            routing: RoutingRequirement::Unconstrained,
            classifier_cfg: crate::classifier::ClassifierConfig::default(),
            attended: true,
        }
    }

    /// Stamp the resolved tool's risk (dispatcher sets this from `Tool::risk()`).
    pub fn with_risk(mut self, risk: RiskClass) -> Self {
        self.risk = risk;
        self
    }

    /// Stamp the resolved per-profile classifier config (B4). The dispatcher
    /// loads it once per dispatch; `PrivacyFilterHook` gates tool-action content
    /// at these thresholds instead of the defaults.
    pub fn with_classifier_config(mut self, cfg: crate::classifier::ClassifierConfig) -> Self {
        self.classifier_cfg = cfg;
        self
    }

    /// Stamp whether a human is attending this dispatch (B5). The dispatcher
    /// sets this from `approver.is_some()`.
    pub fn with_attended(mut self, attended: bool) -> Self {
        self.attended = attended;
        self
    }

    /// Stamp the conversation's session mode (Q11).
    pub fn with_session_mode(mut self, mode: SessionMode) -> Self {
        self.session_mode = mode;
        self
    }

    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = profile.into();
        self
    }

    pub fn with_content(mut self, content: impl Into<String>) -> Self {
        let content = content.into();
        // Default the pattern-matching text to the same string; callers
        // that need a different canonical form (e.g. a shell command
        // distinct from a natural-language message) can override with
        // `with_command_text` afterwards.
        self.command_text = content.clone();
        self.content = content;
        self
    }

    pub fn with_command_text(mut self, text: impl Into<String>) -> Self {
        self.command_text = text.into();
        self
    }

    pub fn with_binding(mut self, binding: Binding) -> Self {
        self.binding = binding;
        self
    }

    pub fn with_cloud(mut self, is_cloud_endpoint: bool) -> Self {
        self.is_cloud_endpoint = is_cloud_endpoint;
        self
    }

    pub fn with_conversation_id(mut self, id: impl Into<String>) -> Self {
        self.conversation_id = id.into();
        self
    }

    pub fn with_input(mut self, input: ToolInput) -> Self {
        self.input = input;
        self
    }
}

// ── HookResult ───────────────────────────────────────────────────────────

/// What a single hook decided. Mirrors `docs/tooling-and-skills.md` §3.4's
/// sketch (`Allow, Deny(String), Ask(String), Modify(ToolInput), Continue`)
/// exactly, with the semantics spelled out here since the sketch didn't
/// disambiguate `Allow` from `Continue`:
///
///   - `Continue` — this hook has no opinion; proceed to the next hook.
///   - `Allow`    — this hook explicitly approves; also proceeds to the
///                  next hook (same chain effect as `Continue` — kept as a
///                  distinct variant so a hook can be explicit in logs/UI
///                  about *actively* allowing vs. simply not objecting).
///   - `Deny(reason)` — hard stop. The chain short-circuits here.
///   - `Ask(prompt)`  — needs human confirmation. The chain short-circuits
///                      here too (deny/ask both "win" over later hooks).
///   - `Modify(input)` — rewrite the tool input for subsequent hooks/the
///                       eventual `Tool::run()` call, then proceed.
#[derive(Debug, Clone, PartialEq)]
pub enum HookResult {
    Continue,
    Allow,
    Deny(String),
    Ask(String),
    Modify(ToolInput),
}

// ── GatingHook / ObserverHook ────────────────────────────────────────────

/// A hook in the sequential, blocking lane. Runs in registration order;
/// the chain stops at the first `Deny`/`Ask`.
pub trait GatingHook: Send + Sync {
    /// Stable name used in "denied by: X" UI surfaces and test assertions.
    fn name(&self) -> &str;

    fn on_event(&self, ctx: &mut EventContext) -> HookResult;
}

/// A hook in the fire-and-forget lane (telemetry, TRM logging, notification
/// emission). Never blocks a tool call and cannot deny/ask — it's not
/// consulted for the gating decision at all. Async execution against a
/// real event loop is deferred to when a concrete observer exists (e.g.
/// durable server-side logging, PLAN.md §8 M3 item 8); the trait is kept
/// synchronous-callable for now with a boxed-future escape hatch available
/// via `on_event_async` for implementors that need it.
pub trait ObserverHook: Send + Sync {
    fn name(&self) -> &str;

    fn on_event(&self, ctx: &EventContext);

    /// Optional async variant for observers that need to await durable
    /// writes (spec §3.4: "server observer handlers must write durably
    /// before returning"). Default delegates to the sync `on_event`.
    fn on_event_async<'a>(
        &'a self,
        ctx: &'a EventContext,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move { self.on_event(ctx) })
    }
}

// ── HookChain ────────────────────────────────────────────────────────────

/// Runs the registered gating hooks in order (short-circuiting on the
/// first `Deny`/`Ask`) and fans out to observer hooks. One chain instance
/// is meant to be built once per body (app / server) and reused.
/// Hooks are held behind `Arc` so a bounded sub-agent dispatcher (Wave 4.3) can
/// REUSE the exact same gate chain via `Clone` — a delegated helper's tool calls
/// pass the identical PrivacyFilter→…→Permission gate as the main agent's, never
/// a fresh/weaker one. `register_*` still takes a `Box` (call sites unchanged).
#[derive(Default, Clone)]
pub struct HookChain {
    gating: Vec<Arc<dyn GatingHook>>,
    observers: Vec<Arc<dyn ObserverHook>>,
}

impl HookChain {
    pub fn new() -> Self {
        Self {
            gating: Vec::new(),
            observers: Vec::new(),
        }
    }

    pub fn register_gating(&mut self, hook: Box<dyn GatingHook>) {
        self.gating.push(Arc::from(hook));
    }

    pub fn register_observer(&mut self, hook: Box<dyn ObserverHook>) {
        self.observers.push(Arc::from(hook));
    }

    /// Names of registered gating hooks, in run order. Useful for tests
    /// and for rendering "the chain is: privacy → permission → sandbox →
    /// first-use" in Settings/debug UI.
    pub fn gating_names(&self) -> Vec<&str> {
        self.gating.iter().map(|h| h.name()).collect()
    }

    /// Run every gating hook in order against `ctx`.
    ///
    /// - Any `Deny`/`Ask` short-circuits immediately; the returned tuple's
    ///   second element names the hook that produced it (for "denied by: X"
    ///   surfaces).
    /// - `Modify` rewrites `ctx.input` and continues.
    /// - `Continue`/`Allow` proceed to the next hook.
    /// - If every hook passes, returns `(HookResult::Continue, None)`.
    pub fn run_gating(&self, ctx: &mut EventContext) -> (HookResult, Option<&str>) {
        for hook in &self.gating {
            match hook.on_event(ctx) {
                HookResult::Continue | HookResult::Allow => continue,
                HookResult::Modify(new_input) => {
                    ctx.input = new_input;
                    continue;
                }
                denied_or_asked @ (HookResult::Deny(_) | HookResult::Ask(_)) => {
                    return (denied_or_asked, Some(hook.name()));
                }
            }
        }
        (HookResult::Continue, None)
    }

    /// Fan out to every observer hook. Fire-and-forget: return value (if
    /// any) is ignored, panics inside an observer are the observer's
    /// problem, not the chain's — callers that need durability should use
    /// `on_event_async` directly against a specific observer.
    pub fn notify_observers(&self, ctx: &EventContext) {
        for hook in &self.observers {
            hook.on_event(ctx);
        }
    }
}

// ── Default PreToolUse chain ────────────────────────────────────────────

/// Build the ordered chain:
/// `[PrivacyFilterHook, SandboxHook, ProtectedPathHook, PermissionHook, FirstUseConfirmHook]`.
///
/// `SandboxHook` runs immediately after the Deny-only `PrivacyFilterHook`
/// and ahead of every hook capable of returning `Ask`
/// (`ProtectedPathHook`, `PermissionHook`, `FirstUseConfirmHook`), so the
/// non-overridable hardline floor is always reached on every `PreToolUse`
/// event — see the module docs above for why putting an Ask-capable hook
/// before `SandboxHook` would let the floor be skipped once Ask-resume is
/// wired up.
///
/// Both bodies (app + future server) are meant to construct their own
/// instance of this against their own profile config — same chain shape,
/// different config seeded into `policy`.
pub fn build_pretooluse_chain(
    gate: crate::agent::gate::PrivacyGate,
    policy: Box<dyn PolicySource>,
) -> HookChain {
    build_pretooluse_chain_with_confirmed(gate, policy, &[])
}

/// Same ordered chain as [`build_pretooluse_chain`], but with `confirmed`
/// tools pre-marked in the `FirstUseConfirmHook` so they don't trigger a
/// first-use prompt.
///
/// This is how a body pre-trusts tools it ships as safe-by-default (e.g. the
/// app's read-only, workspace-confined filesystem tools): an explicit
/// whole-tool `Allow` in the policy expresses "always permit," so a
/// first-use confirmation on top of it would be redundant. Tools that can
/// change state are deliberately *not* pre-confirmed — they route through
/// the real approval flow (a later M3 round) instead.
pub fn build_pretooluse_chain_with_confirmed(
    gate: crate::agent::gate::PrivacyGate,
    policy: Box<dyn PolicySource>,
    confirmed: &[&str],
) -> HookChain {
    let mut chain = HookChain::new();
    chain.register_gating(Box::new(PrivacyFilterHook::new(gate)));
    chain.register_gating(Box::new(SandboxHook));
    chain.register_gating(Box::new(ProtectedPathHook::new()));
    chain.register_gating(Box::new(PermissionHook::new(policy)));
    let first_use = FirstUseConfirmHook::new();
    for tool in confirmed {
        first_use.mark_confirmed(tool);
    }
    chain.register_gating(Box::new(first_use));
    chain
}

/// Same ordered chain as [`build_pretooluse_chain_with_confirmed`], but with a
/// shared [`ApprovalLedger`] threaded into the ask-capable hooks
/// (`ProtectedPathHook`, `PermissionHook`, `FirstUseConfirmHook`). An
/// interactive approval recorded by `ToolDispatcher` (see
/// `ToolDispatcher::with_approval`) turns their `Ask` into `Continue` on
/// the re-run — a single grant satisfies every ask-capable hook because
/// they all consult the same ledger by fingerprint/tool.
///
/// `ProtectedPathHook` is wired here with a shared ledger but consults
/// only `ApprovalLedger::covers_once` (the Once-only path), so a
/// `Session`/`Tool` grant from a different ask never satisfies it — the
/// floor stays Once-only by construction. The dispatcher also pins an
/// extra `Once`+`Fingerprint` grant when a protected-path prompt is
/// answered with anything broader than `Once`, so the re-run settles
/// without upgrading the floor itself. See `dispatch.rs` `Approve` arm
/// and the `protected_path_runs_before_permission_even_under_an_allow_policy`
/// + `session_grant_does_not_bypass_the_floor_on_a_different_protected_path`
/// tests.
///
/// The dispatcher MUST hold the same `Arc<ApprovalLedger>` for grants to be
/// visible here — pass one `Arc`, clone it into both.
///
/// `workspace_root` is the fs tools' workspace directory (or `None` in
/// bodies/tests with no fs tools). When set, `ProtectedPathHook` uses it to
/// resolve a call's `path` arg through the same symlink-following logic the
/// fs tools use, so an in-workspace symlink aliasing a protected dir (e.g.
/// `alias -> .git`) can't slip a write/read/edit/delete past the raw-text
/// floor. See `ProtectedPathHook::with_workspace_root`.
pub fn build_pretooluse_chain_full(
    gate: crate::agent::gate::PrivacyGate,
    policy: Box<dyn PolicySource>,
    confirmed: &[&str],
    ledger: Arc<ApprovalLedger>,
    workspace_root: Option<std::path::PathBuf>,
) -> HookChain {
    let mut chain = HookChain::new();
    chain.register_gating(Box::new(PrivacyFilterHook::new(gate)));
    chain.register_gating(Box::new(SandboxHook));
    let mut protected = ProtectedPathHook::new().with_ledger(Arc::clone(&ledger));
    if let Some(root) = workspace_root {
        protected = protected.with_workspace_root(root);
    }
    chain.register_gating(Box::new(protected));
    // Session-mode gate (Q11): placed AFTER the non-overridable floors
    // (Sandbox danger denylist, ProtectedPath) and BEFORE PermissionHook, so a
    // mode can neither bypass a floor nor widen the Q8 matrix. `Normal` (the
    // default) is a no-op, so bodies/turns that don't set a mode are unaffected.
    chain.register_gating(Box::new(SessionModeHook));
    chain.register_gating(Box::new(
        PermissionHook::new(policy).with_ledger(Arc::clone(&ledger)),
    ));
    let first_use = FirstUseConfirmHook::new().with_ledger(Arc::clone(&ledger));
    for tool in confirmed {
        first_use.mark_confirmed(tool);
    }
    chain.register_gating(Box::new(first_use));
    chain
}

#[cfg(test)]
mod tests;
