# Hooks — gating chain and approval spine

- **Purpose** — The single, ordered, in-process checkpoint every tool call must
  pass through before it executes: privacy egress rules, a non-overridable
  hardline sandbox denylist, a non-overridable always-`Ask` floor for a
  hardcoded set of protected workspace paths (now re-rooted per active
  profile), a session-mode gate (`plan`/`accept_edits`, structurally bounded by
  the grant×risk matrix so a mode can never widen `External`/`Dangerous` or
  bypass a floor), per-tool/pattern permission policy (now layered with a
  persisted per-profile "Always allow" rule store), and first-use confirmation
  — plus the async human-approval machinery (ledger + prompter + the Q8
  grant×risk matrix) that lets an `Ask` turn into a `Continue` without ever
  skipping the earlier gates, and a fully-built-but-unwired headless
  pre-authorization path for an unattended body.

## Files

- `src-tauri/src/hooks/mod.rs` — `HookEvent`, `EventContext`, `HookResult`,
  `GatingHook`/`ObserverHook` traits, `HookChain` (runs the gating hooks in
  order), and the three `build_pretooluse_chain*` constructors that assemble the
  real chain. `RoutingRequirement` also lives here.
- `src-tauri/src/hooks/privacy_filter.rs` — `PrivacyFilterHook`, a thin adapter
  over `agent::gate::PrivacyGate::check()`. First hook in the chain. Carries a
  documented, unclosed gap — see Gotchas.
- `src-tauri/src/hooks/sandbox.rs` — `SandboxHook`, the hardline denylist (unit
  struct, no config). Second hook in the chain, deliberately positioned before
  anything that can `Ask`. Also defines the `SandboxConfig`/`SandboxNetworkConfig`
  shape — read by `tools::exec`, **not** by this hook (see Gotchas).
- `src-tauri/src/hooks/protected_path.rs` — `ProtectedPathHook`, the always-`Ask`
  floor for a hardcoded list of workspace paths (`.git/`, `config/secrets`,
  `.env`, `.ssh/`). Third hook in the chain, between `SandboxHook` and
  `SessionModeHook`/`PermissionHook`. Non-configurable. The `Ask` is satisfiable
  only by a fresh `Once`+`Fingerprint` grant — it consults
  `ApprovalLedger::covers_once` (which only inspects `once_fps`), so a
  `Session`/`Tool` grant from a different ask is invisible to it. Now also
  re-roots a call's `path` arg through the caller's **per-profile** workspace
  subtree (`profile_workspace_path`) before symlink-resolving it, matching
  Tier-P's per-profile fs confinement.
- `src-tauri/src/hooks/session_mode.rs` — `SessionMode` (`Normal`/`Plan`/
  `AcceptEdits`) and `SessionModeHook`. Fourth hook — new since an earlier
  version of this doc, which didn't have it at all.
- `src-tauri/src/hooks/permission.rs` — `PermissionHook`, `PermissionMode`,
  `ToolRule`, `PolicySource` trait + three implementations
  (`InMemoryPolicySource`, `SqlitePolicySource`, `LayeredPolicySource`), plus
  `ToolRuleWriter`/`StorageToolRuleWriter` (the Q8 "Always allow" persist path).
  Fifth hook.
- `src-tauri/src/hooks/first_use.rs` — `FirstUseConfirmHook`. Sixth/last hook.
- `src-tauri/src/hooks/approval.rs` — the approval spine primitives:
  `ActionFingerprint`, `GrantScope`, `GrantTarget`, `ApprovalLedger` (`covers` for
  the general hooks, `covers_once` for floor-style hooks), `resolve_grant` (the
  Q8 grant×risk matrix), `persist_rule_allowed`, `ApprovalRequest`,
  `ApprovalDecision`, `ApprovalPrompter` trait. Consumed by
  `ProtectedPathHook`/`PermissionHook`/`FirstUseConfirmHook` (read) and
  `ToolDispatcher` (write).
- `src-tauri/src/hooks/headless.rs` — `QueueingPrompter` + `ApprovalQueue`, an
  `ApprovalPrompter` for an unattended body: pre-authorize via the same
  `PolicySource`/`resolve_effective_mode` the interactive path uses, else park +
  deny. Fully built and tested; **not wired into the running app anywhere** —
  see Gotchas.
- `src-tauri/src/hooks/routing.rs` — `enforce_local_routing` (the only code
  allowed to turn a `RoutingRequirement::LocalRequired` annotation into an
  actual endpoint pick — **live**) and `routing_for_turn` (the M5 "a screenshot
  forces local regardless of binding" rule — built and tested, **zero callers**
  outside its own test module; see Gotchas).
- `src-tauri/src/hooks/audit.rs` — `AuditEntry`, `AuditWriter` trait,
  `StorageAuditWriter`, `AuditObserverHook` (a registered `ObserverHook` — but
  see Gotchas for why it's currently a no-op breadcrumb, not the thing that
  writes rows), `outcome_label`/`outcome_gate_by`/`truncate_args`.
- `src-tauri/src/hooks/tests.rs` — chain-level integration tests
  (`#[cfg(test)] mod tests;` at `mod.rs:561-562`). Per-hook unit tests live
  inline in each hook's own file instead.
- `src-tauri/src/tools/dispatch.rs` — **not in this dir but the real caller and
  the other half of the approval spine.** `ToolDispatcher::dispatch_inner`
  builds the `EventContext`, calls `chain.run_gating`, and on `Ask` drives the
  prompter/ledger/re-run loop described below. The `Approve` arm runs the
  answer through `resolve_grant` and additionally pins a forced-`Once`+
  `Fingerprint` grant when a protected-path prompt is answered with anything
  broader than `Once` (`dispatch.rs:611-616`).
- `src-tauri/src/ipc/approval.rs` — Tauri-side `ApprovalPrompter` impl
  (`TauriApprovalPrompter`) and the `ApprovalRegistry` that parks a `oneshot`
  channel per pending request, keyed by request id, resolved by the
  `resolve_tool_approval` IPC command.
- `src-tauri/src/lib.rs` (`build_tool_dispatcher`, `lib.rs:433-647`) — where the
  real chain is constructed for the app: policy is *derived* from each tool's
  `RiskClass` (Safe → whole-tool `Allow` + pre-trusted; Write/External/Dangerous
  → `Ask`), then **layered** under a `SqlitePolicySource` reading persisted
  per-profile `tool_rules`.

## Key types / traits / functions

- `HookEvent` — `hooks/mod.rs:108-124`. Only `PreToolUse` is exercised today;
  `PostToolUse`/`CronFired`/`CronCompleted`/`AppLaunch` are reserved variants
  with no wiring yet.
- `RoutingRequirement` — `hooks/mod.rs:136-150`. `Unconstrained` |
  `LocalRequired { reason: String }`. `.is_local_required()` at
  `hooks/mod.rs:153-155`.
- `EventContext` — `hooks/mod.rs:167-217`. Fields: `event`, `tool_name`,
  `input`, `command_text`, `binding`, `content`, `is_cloud_endpoint`,
  `conversation_id`, `profile` (drives `SqlitePolicySource` resolution — empty
  string = no persisted rules, pre-Q8 behavior), `policy_allowed` (set by
  `PermissionHook`/`SessionModeHook` on an explicit allow, so
  `FirstUseConfirmHook` doesn't re-ask), `risk` (the resolved tool's
  `RiskClass`, stamped by the dispatcher — feeds `SessionModeHook`), `session_mode`,
  `routing`. Builder methods `pre_tool_use`, `with_content`, `with_command_text`,
  `with_binding`, `with_cloud`, `with_conversation_id`, `with_input`,
  `with_profile`, `with_risk`, `with_session_mode`.
- `HookResult` — `hooks/mod.rs:336-342`. `Continue | Allow | Deny(String) |
  Ask(String) | Modify(ToolInput)`.
- `trait GatingHook` — `hooks/mod.rs:348-353`. Sync, blocking-lane.
- `trait ObserverHook` — `hooks/mod.rs:362-376`. Fire-and-forget, never
  blocks/denies. Has a concrete implementation now (`AuditObserverHook`) — see
  Gotchas for its current no-op status.
- `HookChain` — `hooks/mod.rs:388-449`. `register_gating`, `register_observer`,
  `gating_names()`, `run_gating(&self, ctx) -> (HookResult, Option<&str>)` —
  names which hook produced a `Deny`/`Ask`. `notify_observers` fans out to
  observer hooks (fire-and-forget).
- `build_pretooluse_chain(gate, policy)` — `hooks/mod.rs:467-472`. Plain chain,
  nothing pre-confirmed, **no shared ledger** (each ask-capable hook falls back
  to its own empty ledger).
- `build_pretooluse_chain_with_confirmed(gate, policy, confirmed)` —
  `hooks/mod.rs:484-500`. Same, pre-marks `confirmed` tools in
  `FirstUseConfirmHook`. Also no shared ledger.
- `build_pretooluse_chain_full(gate, policy, confirmed, ledger, workspace_root)`
  — `hooks/mod.rs:530-559`. **This is the constructor the real app uses**
  (`lib.rs:632-638`). Threads one shared `Arc<ApprovalLedger>` into
  `ProtectedPathHook`, `PermissionHook`, and `FirstUseConfirmHook`, and (new)
  threads `workspace_root` into `ProtectedPathHook` so it can resolve a call's
  `path` arg through the same per-profile, symlink-following logic the fs tools
  use.
- `PrivacyFilterHook::on_event` — `privacy_filter.rs:36-72`. Maps
  `GateDecision::Allow → Continue`, `Block(reason) → Deny(reason)`,
  `RouteLocal → Continue` **plus** sets `ctx.routing = LocalRequired{reason}`.
- `SandboxHook::on_event` — `sandbox.rs:118-131`. Iterates a fixed
  `DENYLIST: &[DenylistEntry]` (`sandbox.rs:32-105`) matching against
  `ctx.command_text`; `Deny` on any match, else `Continue`. Bare unit struct
  (`sandbox.rs:111`) — no constructor arguments at all, which is itself the
  enforced invariant.
- `ProtectedPathHook::on_event` — `protected_path.rs:129-190`. Iterates a fixed
  `PROTECTED: &[ProtectedPathEntry]` (`protected_path.rs:47-64`) against BOTH
  `ctx.command_text` (raw-text match) AND, when a `workspace_root` is wired, the
  real symlink-resolved target of the call's `path` arg re-rooted through
  `profile_workspace_path(base, &ctx.profile)` (`protected_path.rs:143-162`) —
  the second signal is what catches a workspace symlink (`alias -> .git`) whose
  name never mentions the protected substring. On any match, checks
  `ApprovalLedger::covers_once(&fp)`, `Continue` if covered else `Ask`.
  `ProtectedPathHook::new()`/`with_ledger(...)`/`with_workspace_root(...)` are
  the only builders.
- `SessionMode` — `session_mode.rs:27-40`. `Normal` (default, no-op) |
  `Plan` (read-only: denies any tool with risk above `Safe`) | `AcceptEdits`
  (auto-approves `Write`-risk only, setting `ctx.policy_allowed`; never
  `External`/`Dangerous`). `as_str()`/`from_str_lenient()` (`session_mode.rs:44-61`)
  — an unrecognized string defaults to `Normal`, never silently unlocking
  `accept_edits`.
- `SessionModeHook::on_event` — `session_mode.rs:73-110`. Positioned (per the
  chain order below) **after** the non-overridable floors and **before**
  `PermissionHook`, which is what makes the Q8-matrix bound structural: a
  danger-floor or protected-path call has already short-circuited before this
  hook runs, so no mode can loosen it; and an explicit `Deny` rule in
  `PermissionHook` (which runs next) still wins over `accept_edits`.
- `PermissionHook::resolve` — `permission.rs:356-360`. Delegates to
  `resolve_effective_mode` (`permission.rs:368-381`) — the canonical resolution
  extracted specifically so `headless::QueueingPrompter` can apply the *same*
  precedence and never be more permissive than the interactive path.
  Most-specific matching `ToolRule` wins (specificity = literal-char count,
  `permission.rs:74-76`; `PermissionMode::priority()` at `permission.rs:35-41`
  breaks ties Deny(2) > Ask(1) > Allow(0)); falls back to
  `PolicySource::mode_for(tool_name)`; falls back to `None`.
- `PermissionHook::on_event` — `permission.rs:388-421`. `Allow → Continue` +
  `ctx.policy_allowed = true`; `Deny → Deny(reason)`; `Ask` → checks
  `ledger.covers(tool_name, fingerprint)` first, `Continue` if covered else
  `Ask(reason)`; `None → Continue` (falls through to `FirstUseConfirmHook`, NOT
  an implicit ask).
- `glob_match(pattern, text) -> bool` — `permission.rs:83-120`. Minimal
  `*`-only glob, no regex crate.
- `trait PolicySource` — `permission.rs:128-140`. `mode_for(tool_name)`,
  `rules_for(tool_name, profile)` — profile-scoped so a persisted rule in one
  profile never resolves in another.
  - `InMemoryPolicySource` (`permission.rs:146-184`) — the risk-derived
    whole-tool defaults built at boot; profile-blind.
  - `SqlitePolicySource` (`permission.rs:197-250`) — reads the SQLite
    `tool_rules` table **live** on every gating pass via `ProfileDb`, so a
    freshly persisted rule (or a Settings revoke) takes effect immediately, no
    restart; `mode_for` always `None` (whole-tool defaults stay in-memory). An
    unknown/malformed `action` value is dropped (never silently widened) and
    logged.
  - `LayeredPolicySource` (`permission.rs:260-281`) — composes an in-memory
    `defaults` source with a persisted `SqlitePolicySource`: `mode_for` comes
    from `defaults`; `rules_for` is `persisted ⧺ defaults`, so a
    Settings-authored rule competes in the *same* deny>ask>allow /
    most-specific-wins resolution as a static one. This is what
    `build_tool_dispatcher` wires (`lib.rs:627-630`).
  - `ToolRuleWriter`/`StorageToolRuleWriter` (`permission.rs:291-325`) — the
    write side of "Always allow": persists a durable per-profile `tool_rules`
    row. A write failure is surfaced loudly (not swallowed like an audit row) —
    a rule is an authorization the user relies on.
- `FirstUseConfirmHook` — `first_use.rs:25-71`. Two independent "already OK"
  sources: `seen: Mutex<HashSet<String>>` (set only via `mark_confirmed`, at
  construction time) and the shared `ledger`. `on_event`
  (`first_use.rs:78-111`) returns `Continue` if `ctx.policy_allowed`, or `seen`,
  or the ledger covers it; else `Ask` — and critically does **not** mark the
  tool seen just because it asked (`asking_does_not_mark_the_tool_seen`,
  `first_use.rs:128-142`).
- `ActionFingerprint::of(tool_name, args) -> String` (`approval.rs:61-74`) /
  `::from_ctx(ctx) -> String` (`approval.rs:76-78`) — SHA-256 over
  `tool_name + 0x00 + canonical(args)`. **Carries no session/conversation
  discriminator** — see Gotchas for the resulting known nuance.
- `GrantScope` — `approval.rs:118-127`. `Once` (consumed at execution),
  `Session` (until app restart — in-memory only), `Always` (**currently aliased
  to `Session` in the in-memory `ApprovalLedger`** — see Gotchas for how this
  differs from the *separately real* persisted `tool_rules` "Always allow").
- `GrantTarget` — `approval.rs:131-136`. `Fingerprint(String)` (the pin) |
  `Tool(String)` (the deliberate broadening).
- `ApprovalLedger` — `approval.rs:144-212`. Three `Mutex<HashSet<String>>` sets:
  `once_fps`, `session_fps`, `session_tools`. `covers(tool_name, fingerprint)`
  (`approval.rs:159-163`) — OR across all three. `covers_once(fingerprint)`
  (`approval.rs:171-173`) — `once_fps` ONLY, used by `ProtectedPathHook` so the
  floor can't be satisfied by a standing grant. `grant(target, scope)`
  (`approval.rs:178-205`) — `(Once, Tool(_))` is explicitly a no-op
  (`approval.rs:192-197`, logged): a one-time grant has no fingerprint to pin
  to. `consume_once(fingerprint)` (`approval.rs:209-211`).
- `resolve_grant(risk, scope, target, fingerprint) -> (GrantScope, GrantTarget)`
  — `approval.rs:237-260`. **The single server-side enforcement point of the
  Q8 grant×risk matrix** (documented in full at `approval.rs:228-233`):

  | Risk | Once | Session | Always |
  |---|---|---|---|
  | Safe | (Once, fp) | as-asked | as-asked |
  | Write | (Once, fp) | as-asked | as-asked |
  | External | (Once, fp) | (Session, fp) | (Session, fp) — fingerprint-pinned only, never whole-tool standing |
  | Dangerous | (Once, fp) | (Once, fp) | (Once, fp) — any standing answer collapses; runs once, records nothing |

  The call still runs (the human approved it in person); only the *standing*
  coverage is narrowed. `Once` is always forced to `Fingerprint` regardless of
  risk.
- `persist_rule_allowed(risk) -> bool` — `approval.rs:271-273`. **Only `Write`**
  may persist a durable `Always` `tool_rules` row. `External` is refused (a bare
  whole-tool standing grant for egress is never allowed — destination-scoped
  rules are the intended path); `Dangerous` is refused (invariant: never a
  standing grant); `Safe` never reaches an `Ask` at all. A refused persist
  doesn't block the call — it runs once, just records nothing durable.
- `trait ApprovalPrompter` — `approval.rs:326-331`. Implemented by
  `TauriApprovalPrompter` (`ipc/approval.rs:125-193`, interactive, 300s
  deny-by-default timeout) and `QueueingPrompter` (`headless.rs:110-165`,
  unattended, see below).
- `QueueingPrompter::preauthorize` — `headless.rs:129-164`. Two floors enforced
  *above* the rule store (so no rule can relax them): (1) `RiskClass::Dangerous`
  is **never** pre-authorized (`headless.rs:131-133`); (2) an `External` call
  additionally requires an `Allow` rule whose pattern has at least one literal
  (non-`*`) character matching the call's destination — a bare `*`/`**` rule
  Allow-wins on the command but names no destination, so it still parks + denies
  (`headless.rs:151-161`). Resolution otherwise rides
  `permission::resolve_effective_mode` against the *same* rules/precedence the
  interactive `PermissionHook` uses, so the headless path is structurally never
  more permissive than an attended one. A pre-authorized grant is always
  `(Once, Fingerprint)` — the prompter never hands itself a standing grant.
- `ApprovalQueue` — `headless.rs:63-106`. `enqueue`/`pending`/`resolve` — parks
  an unresolved request for later human review; draining it does **not**
  retroactively run the action (the dispatch that parked it already got `Deny`).
- `enforce_local_routing(routing, candidates)` — `routing.rs:50-70`. **Live.**
  `Unconstrained` → first candidate (or error if none). `LocalRequired` → first
  candidate where `p.is_local() && p.is_private()`, else `Err(LocalRoutingViolation)`
  — structurally cannot return a cloud provider on this branch.
- `routing_for_turn(base, has_image) -> RoutingRequirement` — `routing.rs:89-101`.
  **Built and tested, zero production callers.** The M5 rule: an image-bearing
  turn is forced `LocalRequired` regardless of binding (a screenshot can't be
  privacy-classified, and `Binding::Public`'s text-level cloud opt-in must not
  extend to whatever the screen happened to show). Never downgrades an existing
  `LocalRequired`. See Gotchas.

## Data flow / how it fits

1. `ToolDispatcher::dispatch_inner` (`tools/dispatch.rs:436-695`) is the real
   entry point. Given a parsed `ToolCall`, `ExecCtx`, `Binding`, and `is_cloud`:
   resolves the tool, checks `tool.available(&env)`, computes `canonical`
   (feeds `command_text`) and `fingerprint = ActionFingerprint::of(name, args)`.
2. Loop (bounded `MAX_APPROVAL_ROUNDS = 4`, `dispatch.rs:467`): builds a fresh
   `EventContext` — now also stamping `.with_profile(ctx.profile)` and
   `.with_risk(tool.risk())`/`.with_session_mode(ctx.session_mode)` — and calls
   `self.chain.run_gating(&mut ev)`.
   - `Continue`/`Allow` → consumes any once-grant for this fingerprint
     (`ledger.consume_once`, done *before* checking routing so a once-grant
     can't stay armed if routing blocks it), then checks
     `ev.routing.is_local_required() && is_cloud` — if true, returns
     `ToolOutcome::NeedsLocalReroute` (the caller, which owns providers, may
     retry against a local endpoint) rather than running the tool and leaking
     the result to a cloud model next turn. Otherwise runs `tool.run(...)`.
   - `Deny(reason)` → `ToolOutcome::Denied { by, reason }`, tool never runs.
   - `Ask(prompt)` → no `approver` wired ⇒ surfaces `ToolOutcome::Ask`. An
     approver wired ⇒ builds an `ApprovalRequest`, awaits
     `approver.request(req)`.
     - `Approve(scope, target)` → `resolve_grant(tool.risk(), scope, target,
       &fingerprint)` narrows the answer per the Q8 matrix, `ledger.grant(...)`
       records it. **If** `by == "protected_path"` and the narrowed/requested
       scope is broader than `Once`, the dispatcher *additionally* pins a
       forced `(Once, Fingerprint)` grant for this exact call
       (`dispatch.rs:611-616`) — the floor's `covers_once` only ever sees
       `once_fps`, so this is what lets the re-run settle without ever
       upgrading the floor itself to standing coverage. Then `continue`s —
       **re-runs the whole chain from the top** (Sandbox/ProtectedPath/Privacy
       all re-checked, not just Permission).
     - `Persist(rule)` → only honored for `Write` risk (`persist_rule_allowed`);
       writes a durable per-profile `tool_rules` row via the wired
       `ToolRuleWriter`, and — regardless of whether the persist succeeded —
       still pins a `(Once, Fingerprint)` grant so *this* approved call
       settles. A persist failure is logged loudly, never silently swallowed.
     - `Deny`/`Timeout` → `ToolOutcome::Denied`.
   - Exhausting all 4 rounds is treated as a bug and fails closed.
3. `TauriApprovalPrompter::request` (`ipc/approval.rs:142-192`) parks a
   `oneshot` channel in `ApprovalRegistry` keyed by a UUID, emits
   `tool:approval_request` to the frontend, and awaits with a 300s timeout
   (deny-by-default). The frontend answers via `resolve_tool_approval`, which
   touches *only* the registry, never the agent loop's stream lock
   (`ipc/approval.rs:1-13`).
4. **The unattended path** (`QueueingPrompter`) is a complete, independently
   tested alternative `ApprovalPrompter` — pre-authorize via a rule (never for
   `Dangerous`; `External` needs a destination-naming rule), else park in
   `ApprovalQueue` and deny. **Nothing in `lib.rs`/`build_tool_dispatcher`
   constructs one today** — there is no headless body yet for it to serve. It
   exists as server-track prep, fully tested against `hooks::headless::tests`.
5. Downstream, once a call that was `LocalRequired` is allowed and executed,
   whatever picks the actual model endpoint for a subsequent call is expected
   to run it through `enforce_local_routing` — a caller obligation, not
   something the hook chain enforces after `dispatch` returns.

## Invariants (do NOT break)

- **First "no" wins, in fixed order.** The REAL app chain
  (`build_pretooluse_chain_full`) is
  `[PrivacyFilterHook, SandboxHook, ProtectedPathHook, SessionModeHook,
  PermissionHook, FirstUseConfirmHook]` — six hooks, one more than an earlier
  version of this doc described (`SessionModeHook` is new). The bare
  `build_pretooluse_chain`/`build_pretooluse_chain_with_confirmed` constructors
  build a five-hook chain with **no** `SessionModeHook` and no shared ledger;
  `default_pretooluse_chain_is_in_spec_order` (`hooks/tests.rs:183-191`) asserts
  that five-hook order specifically, so don't read it as asserting the full
  app chain. `HookChain::run_gating` short-circuits on the first `Deny`/`Ask`.
- **The sandbox floor always runs and cannot be configured away.** `SandboxHook`
  is a bare unit struct — no constructor args, no `SandboxConfig` wiring in this
  hook at all. Positioned immediately after the Deny-only `PrivacyFilterHook`
  and before every hook capable of `Ask`. Enforced by
  `sandbox_denies_even_when_permission_would_allow` and
  `sandbox_runs_before_any_hook_that_can_ask` (`hooks/tests.rs`), plus
  `cannot_be_overridden_by_any_config` (`sandbox.rs:301-318`).
- **The protected-paths floor is non-overridable, Once-only, and now
  per-profile.** No constructor args, no `PolicySource`/config wiring; its `Ask`
  is satisfiable only by a fresh `Once`+`Fingerprint` grant
  (`ApprovalLedger::covers_once`). Positioned between `SandboxHook` and
  `SessionModeHook` so the sandbox's hard-Deny floor still runs first. Its
  resolved-path signal re-roots through `profile_workspace_path(base,
  &ctx.profile)` before symlink-following, so a `.git` symlink living in one
  profile's subtree is only caught for calls under *that* profile — verified by
  `resolved_signal_re_roots_to_the_call_s_profile` (`protected_path.rs:311-343`)
  and the symlink-bypass regression
  (`symlink_to_git_is_caught_via_canonical_resolution_even_though_raw_text_never_mentions_git`,
  `protected_path.rs:274-308`).
- **`SessionMode` can never widen beyond its documented bound.** `Plan` only
  ever *denies* (never Allows/Continues something it wouldn't otherwise);
  `AcceptEdits` only ever auto-approves `Write` — `External`/`Dangerous` fall
  straight through to normal gating untouched, and `SessionModeHook` runs after
  both non-overridable floors, so neither mode can bypass them. Enforced by
  `accept_edits_auto_approves_write_but_never_widens_external_or_dangerous` and
  `plan_mode_allows_reads_and_denies_every_mutation_through_the_full_chain`
  (`hooks/tests.rs:366-444`), which run this **through the real full chain**, not
  the hook in isolation.
- **`RouteLocal` never silently degrades to "allow on cloud."**
  `PrivacyFilterHook` maps `RouteLocal` to `Continue` *plus* sets
  `ctx.routing = LocalRequired`; only `enforce_local_routing` may resolve that
  annotation, and it fails loudly (`LocalRoutingViolation`) rather than falling
  back to the first (possibly cloud) candidate
  (`local_required_never_returns_a_cloud_provider`, `routing.rs:151-158`).
  `ToolDispatcher::dispatch_inner` additionally returns `NeedsLocalReroute`
  (never runs the tool) when `ev.routing.is_local_required() && is_cloud` —
  covered by `local_required_call_needs_reroute_on_a_cloud_endpoint`
  (`dispatch.rs:1327`).
- **Asked ≠ approved.** `FirstUseConfirmHook::on_event` never marks a tool
  `seen` merely for having returned `Ask` — only `mark_confirmed`
  (construction-time pre-trust) or an actual ledger grant (recorded by the
  dispatcher after a real user "yes") can flip a later call to `Continue`.
  Enforced by `asking_does_not_mark_the_tool_seen` (`first_use.rs:128-142`).
- **A `Once` grant is per-action, never per-tool.**
  `ApprovalLedger::grant((Once, Tool(_)))` is explicitly a no-op
  (`approval.rs:192-197`). Test: `a_once_grant_for_a_whole_tool_grants_nothing`
  (`approval.rs:381`).
- **Deny wins ties in `PermissionHook`.** Equally-specific matching rules break
  toward `Deny` (2) > `Ask` (1) > `Allow` (0)
  (`deny_wins_tiebreak_among_equally_specific_matches`, `permission.rs:525`).
- **An unconfigured tool falls through to `Continue`, not an implicit `Ask`.**
  `PermissionHook::resolve` returning `None` maps to `Continue` so
  `FirstUseConfirmHook` gets the final say
  (`unconfigured_tool_falls_through_as_continue`, `permission.rs:471`).
- **The dispatcher and the chain must share one `Arc<ApprovalLedger>`** — passing
  two different instances silently breaks grant visibility (no compiler error,
  just approvals that never take effect). `lib.rs:632-644` does this correctly.
- **`resolve_tool_approval` never touches the agent loop's stream lock**
  (`ipc/approval.rs:1-13`, reiterated in `dispatch.rs`'s module docs) — this is
  what keeps a pending approval from deadlocking a concurrent send.
- **The headless pre-authorizer can never be more permissive than the
  interactive path.** `QueueingPrompter::preauthorize` calls the *same*
  `resolve_effective_mode` function `PermissionHook` uses
  (`permission.rs:368-381`), against the same rules and precedence — verified by
  `a_specific_ask_carveout_beats_a_broad_allow` (`headless.rs:333-365`).
  `Dangerous` is never pre-authorized regardless of rules
  (`dangerous_is_never_preauthorized_even_with_a_matching_rule`,
  `headless.rs:256-266`).

## Gotchas / watch-items

- **The privacy-filter hook's KNOWN GAP (documented in its own doc comment,
  `privacy_filter.rs:41-57`): tool-action content is gated at the DEFAULT
  `ClassifierConfig` thresholds, not the active profile's.** The per-profile
  strictness knob (PLAN §11) is threaded into the message-egress gate
  (`AgentLoop::process_message`) but not into this hook — carrying the
  profile's config through `EventContext` from the dispatcher is a tracked
  follow-up. For a profile configured *stricter* than default, tool-action
  content in the borderline band is gated *less* strictly here than that same
  profile's chat messages — a real, acknowledged inconsistency. Bounded by two
  things: (1) it's no leakier than the pre-existing baseline (the whole app
  used these same fixed constants everywhere before), and (2) the rules-layer
  floor (SSN/keys/email/etc.) is un-tunable and still fires regardless of
  thresholds, so structured PII in tool args is always caught — the residual
  gap is free-text semantic PII in the narrow borderline window.
- **`SandboxConfig`/`SandboxNetworkConfig` are now only *partially* dead
  config** — a claim in an earlier version of this doc ("not read by anything
  today") is now false. `SandboxHook` itself still never reads it — the hardline
  denylist is unconditional, config-free, by design. But
  `tools::exec::ShellExecTool::effective_network` **does** read it (via
  `Storage::open_profile(profile).get_sandbox_config()`) and enforces
  `permits_shell_network()` as a live per-profile network **ceiling** on
  `shell_exec`. That plumbing is real and tested — see
  `docs/codebase/tools.md`'s Gotchas for why it's still practically unreachable
  today (no writer exists for `sandbox_config` anywhere in the IPC/UI surface).
  `network.allowed_domains`'s fine-grained per-domain enforcement is still not
  implemented anywhere — `permits_shell_network()` is coarse (localhost-allowed
  OR any allowed domain ⇒ full outbound), by design, per its own doc comment
  (`sandbox.rs:157-174`).
- **`ObserverHook` now HAS a concrete implementation, but it doesn't do the
  thing its name implies yet.** `AuditObserverHook` is registered in the real
  chain (`lib.rs:639-641`, via `chain.register_observer`), but `EventContext`
  is built *before* a tool runs and carries no `ToolOutcome`, so its `on_event`
  is presently a no-op that only logs a trace breadcrumb (`audit.rs:274-286`).
  **The audit rows that actually land in `tool_audit` come from
  `ToolDispatcher::dispatch`'s direct call to `AuditWriter::write_audit`**
  (`dispatch.rs:419-430`, `fire_audit` at `dispatch.rs:288-331`), which holds
  the *same* `Arc<dyn AuditWriter>` the observer hook holds — so migrating to a
  real `notify_observers`-driven audit path later is a dispatcher refactor, not
  a persistence-layer one. Don't assume "an `ObserverHook` is registered" means
  "the audit trail comes from the observer lane" — it doesn't, yet.
- **`headless::QueueingPrompter`/`ApprovalQueue` are fully built and tested but
  wired nowhere in production**, the same status as `computer_use.rs` in the
  tools subsystem. Grepping `QueueingPrompter`/`ApprovalQueue` outside
  `headless.rs` finds only the `pub use` re-export in `hooks/mod.rs:90` — no
  call site constructs one. This is deliberate server-track prep (no headless
  body exists yet to plug it into), not a bug, but don't assume an unattended
  cron/server run today gets rule-based pre-authorization — it doesn't exist as
  a running path yet.
- **`routing_for_turn` (the M5 "screenshot forces local" rule) is built and
  tested but has ZERO callers outside its own test module** — grep
  `routing_for_turn` outside `hooks/routing.rs` and it's gone. `enforce_local_routing`
  (the thing that actually turns a `RoutingRequirement` into an endpoint pick)
  **is** live and called from the agent loop. So today, a turn carrying a
  screenshot is not yet structurally forced local by this mechanism — that
  wiring is pending M5's native computer-use backend landing.
- **`ActionFingerprint` hashes only `(tool_name, canonicalized args)` — no
  session or conversation discriminator.** Known, documented nuance worth
  internalizing before touching the approval spine: because `ApprovalLedger`
  is shared by `Arc` across the main dispatcher and every `restricted`/
  `headless` sub-dispatcher (delegated helpers, a future cron run), an
  `External` tool call granted `Session` scope in one interactive conversation
  is, in principle, byte-identically replayable by a headless/background call
  with the same tool + args later in the *same app process lifetime* — the
  ledger has no way to tell "this session's interactive grant" from "a
  different, unattended caller." This is bounded by `resolve_grant`'s narrowing
  (an `External` grant is never whole-tool, always fingerprint-pinned) and by
  headless sub-dispatchers not being wired to raise interactive prompts in the
  first place today — but if/when the headless path (`QueueingPrompter`) is
  wired in, this is the first thing to revisit: either fingerprint in a
  session/conversation discriminator, or keep headless dispatchers on
  per-run/ungranted ledgers rather than the shared one.
- **`Always` in the in-memory `ApprovalLedger` still behaves like `Session`** —
  it does not survive an app restart (`approval.rs:20-24`, `approval.rs:125-126`).
  This is a **separate mechanism** from the persisted `tool_rules` "Always
  allow" that Q8 actually shipped: `ApprovalDecision::Persist(rule)` (not
  `Approve(Always, _)`) is what writes a durable per-profile row via
  `ToolRuleWriter`, gated to `Write` risk only by `persist_rule_allowed`. Don't
  conflate the two — a UI/API surface that still offers `GrantScope::Always`
  through the ledger path (rather than `Persist`) gets session-only behavior
  regardless of the label.
- **Only `PreToolUse` is live.** Every hook's `on_event` starts with
  `if ctx.event != HookEvent::PreToolUse { return Continue; }`.
- **`command_text` vs `content` divergence is easy to get backwards.**
  `EventContext::with_content` sets both to the same string by default;
  `with_command_text` overrides only the latter. `PrivacyFilterHook` reads
  `content`+`binding`; `SandboxHook`/`ProtectedPathHook`/`PermissionHook` read
  `command_text`. In production `ToolDispatcher::dispatch_inner` sets both from
  the same canonical `"{name} {args}"` string, so they're identical in
  practice; tests exploit setting them differently to isolate one hook.
- **`glob_match`'s specificity heuristic is naive** — literal-character count,
  not glob "narrowness" in any formal sense. Fine for today's short,
  profile-authored rules.
- **`FirstUseConfirmHook`/`PermissionHook`/`ProtectedPathHook` each
  default-construct their own empty `ApprovalLedger`** if `.with_ledger` isn't
  called — the two non-`_full` chain constructors don't wire a shared ledger,
  so any `Ask` from those chains can never be satisfied by a grant.
- **Sandbox denylist is substring/heuristic matching on lowercased text**, not a
  shell parser — deliberately recall-biased, not a hardened sandbox. A
  `Continue` from `SandboxHook` means "didn't match the fixed floor list," not
  "this command is safe."
- **`MAX_APPROVAL_ROUNDS = 4`** (`dispatch.rs:467`) is a backstop, not a
  designed retry count.
- **PLAN.md vs code**: skim `docs/PLAN.md` §8 and `docs/tooling-and-skills.md`
  §3.4/§3.1/§10/§11 for the spec framing; PLAN.md's build-order item numbering
  is a planning artifact, not something enforced by the code.

## How to extend

- **Add a new gating rule that should be non-overridable** (like the sandbox
  floor): add an entry to `sandbox.rs`'s `DENYLIST` array, not a new hook.
- **Add a new always-`Ask` floor for a hardcoded list** (like protected-paths):
  mirror `protected_path.rs` — no constructor args, consult only
  `ApprovalLedger::covers_once`. Register it in all three `build_pretooluse_chain*`
  constructors between `SandboxHook` and `SessionModeHook`/`PermissionHook`.
  Update `default_pretooluse_chain_is_in_spec_order` and add a
  "runs before permission even under an Allow policy" test.
- **Add a new configurable per-tool policy default:** wire it into whatever
  builds the risk-derived `InMemoryPolicySource` (`lib.rs:598-608`) — don't
  hand-special-case tool names in `PermissionHook` itself.
- **Add a new `GatingHook`:** implement the trait, decide where it belongs
  relative to the Deny-only privacy filter / hardline sandbox / protected-paths
  floor / session-mode / permission+first-use hooks, and register it in
  `build_pretooluse_chain*`. If it can `Ask`, decide whether it should consult
  the shared `ApprovalLedger` via a `.with_ledger` builder, and whether it must
  remain satisfiable only by a fresh `Once` grant (`covers_once`, floor-style)
  or by any covering grant (`covers`, normal).
- **Persist `Always` grants across restarts for risk classes beyond `Write`
  (if ever decided):** change `persist_rule_allowed` (`approval.rs:271-273`)
  deliberately — this is currently an explicit invariant (#8: `External`/
  `Dangerous` never get a standing grant), not an oversight, so changing it
  needs a real decision, not just a code change.
- **Wire the headless path for real:** construct a `QueueingPrompter` +
  `ApprovalQueue` against the body's `PolicySource`, pass it as the `approver`
  to a `ToolDispatcher::with_approval` (or, for delegated/cron sub-dispatchers,
  reconsider whether `restricted`/`headless` should keep `approver: None` or
  switch to a `QueueingPrompter` — see the `ActionFingerprint` gotcha above
  before doing this). Also decide the review UI that drains `ApprovalQueue::pending()`.
- **Wire `routing_for_turn`:** call it from wherever the agent loop currently
  builds/uses `RoutingRequirement` for a turn that may carry a screenshot (M5
  computer-use), folding its result into the routing decision before
  `enforce_local_routing` runs.
- **Close the privacy-filter profile-thresholds gap:** thread the active
  profile's `ClassifierConfig` through `EventContext` (a new field, or reuse
  `profile` to look it up at hook-construction/call time) and have
  `PrivacyFilterHook::on_event` use it instead of `ClassifierConfig::default()`
  (`privacy_filter.rs:57`).
- **Change the chain order:** don't, without re-reading the "why" in
  `mod.rs`'s module doc (`hooks/mod.rs:1-61`) and updating
  `default_pretooluse_chain_is_in_spec_order` +
  `sandbox_runs_before_any_hook_that_can_ask` +
  `protected_path_runs_before_permission_even_under_an_allow_policy` +
  the full-chain session-mode tests in `hooks/tests.rs`.

## Tests

- Per-hook unit tests live inline in each hook's file: `privacy_filter.rs:75-167`,
  `sandbox.rs:198-319` (incl. `cannot_be_overridden_by_any_config` at `sandbox.rs:301-318`),
  `protected_path.rs:193-378` (incl. the per-profile re-root and symlink-bypass
  regressions), `session_mode.rs:113-184`, `permission.rs:424-695`, `first_use.rs:114-195`,
  `approval.rs:333-553` (incl. `covers_once_only_sees_once_fps_not_session_grants` at
  `approval.rs:393`), `routing.rs:103-236` (incl. the M5 screenshot-forces-local
  tests), `headless.rs:190-382`.
- Chain-level integration tests: `src-tauri/src/hooks/tests.rs` — ordering,
  short-circuit, deny-wins, RouteLocal-survives-the-chain, and the two
  full-chain session-mode tests (`accept_edits_auto_approves_write_but_never_widens_external_or_dangerous`
  at `hooks/tests.rs:367`, `plan_mode_allows_reads_and_denies_every_mutation_through_the_full_chain`
  at `hooks/tests.rs:414`).
- Dispatcher-level (the real caller) tests: `src-tauri/src/tools/dispatch.rs`
  — sandbox-denies-under-an-Allow-policy, cloud-vs-local reroute
  (`dispatch.rs:1327`, `1356`), restricted/headless sub-dispatcher gating
  (`1224`, `1253`), the full interactive approval pause/resume/grant/deny/timeout
  suite via `MockPrompter`, the protected-paths floor tests
  (`dispatch.rs:2472`, `2510`), budgets/repeat/cascade (the "Q4 do-now item 2"
  block starting at `dispatch.rs:1902`), and the audit-row-per-denial tests
  including `repeat_detection_denial_produces_an_audit_row` (`dispatch.rs:2841`).
- Tauri prompter/registry tests: `src-tauri/src/ipc/approval.rs:195-228`.
- Run just this subsystem: `cd src-tauri && cargo test hooks::` (unit + chain
  tests) and `cargo test tools::dispatch::` (dispatcher integration tests) and
  `cargo test ipc::approval::` (Tauri prompter tests). Run the whole crate with
  `cargo test` from `src-tauri/`.

---
*Verified against `src-tauri/src` at HEAD `ca54251` (2026-07-21): every file/line
reference above was read directly, not inferred. 542 lib tests passing at that
commit. If you change this subsystem materially, update this doc in the same
change — a wrong doc is worse than none.*
