# Tool system — Fable's decisions (review of `tool-system-for-review.md`)

**Reviewer:** Fable (Argos harness spec author). **Date:** 2026-07-15.
**Grounded in:** `docs/codebase/{tools,hooks-gating-and-approval}.md`, direct reads of
`src-tauri/src/tools/{dispatch,calling,mod}.rs`, `docs/tooling-and-skills.md` §3.2/§3.5/§4,
`docs/PLAN.md` §8/§12, `docs/argos-review.md`, and the Argos spec (`~/claude/harness-spec/`,
esp. 04-tools, 06-autonomy §6, 10-security).

**Overall read:** the spine is right, and in two places better-shaped than Argos for this
topology (risk-derived gating with no second registry to sync; re-running the *full* chain
after a grant so the sandbox floor is re-checked — Argos evaluates offer-time + call-time,
yours is strictly tighter on the resume path). No locked property needs unravelling. The
biggest thing I'd change versus your provisional plan: **persisted `Always` grants must be
rules, not fingerprints** (Q8), and **the durability journal should move out of M3** (Q3).
The one mechanism I'd harden immediately is invariant #5's caller discipline (Q1, do-now).

Verdict key: **CONFIRM** = provisional placement stands · **ADOPT** = design as sketched here ·
**MOVE** = change milestone · **SPLIT** = part now, part later.

---

## Tier A — execution & resource discipline

### Q1 — Native tool-use vs fenced dialect: **CONFIRM M4, ADOPT the flag design — plus one do-now hardening**

**Verdict.** Confirm PLAN §12 item 1 at M4: per-endpoint capability flag, both transports
normalize into the existing `ToolCall {name, args}` before `dispatch()`. Everything from the
registry down is transport-blind and needs zero change — that's the payoff of the current
shape. Three sub-decisions:

1. **Typed schemas: yes, pulled forward with Q1 — but minimally.** Add one defaulted method
   to the `Tool` trait: `fn schema(&self) -> serde_json::Value` (JSON Schema for `args`;
   default = permissive object). Native APIs consume it verbatim; `render_tool_catalog`
   renders it as arg docs for the fenced path (small local models get better calls for
   free); MCP tools (Q7) already arrive with one. Do **not** build typed per-tool arg
   structs/deserialization now — `ToolInput.args` stays bare JSON internally, and validation
   is a dispatch-boundary check that returns a "bad args, here's the schema" outcome for the
   model to retry. Full typed args is later ergonomics, not a Q1 prerequisite.
2. **Guard-wrapping native results: yes, unconditionally.** Native `tool_result` blocks are
   role-separated at the API layer, but (a) the content inside them is still untrusted (web
   pages, MCC/MCP output), (b) the model echoes results into its own text, and (c) one
   normalized feedback path is worth more than a saved wrapper. `format_outcome` stays the
   single formatter for both transports. No exceptions.
3. **Invariant #5's mechanism under native transport.** Native tool-use makes the property
   *structural* on capable endpoints — a tool call is a typed block the provider attributes
   to the assistant; read content cannot mint one. Two rules to bank that win:
   - **On a native-mode endpoint, do not run `parse_tool_calls` at all.** The fenced parser
     is dead code for that turn, not a second listener. Log which mode each turn used
     (PLAN already says this).
   - **Do-now (M3-remainder, ~half a day): newtype the parser input.** Add
     `pub struct OwnOutput(String)` constructible **only** by the model-client's stream
     assembly (constructor `pub(crate)` in the model-client module), and change the
     signature to `parse_tool_calls(own: &OwnOutput)`. The single-caller discipline becomes
     a compile error for any future call site instead of a comment. This is the cheapest
     real hardening available anywhere in the system; do it before more agent-loop code
     accretes call sites.

**Sketch.** `models/` gains a `supports_native_tools` capability flag (config +
static-per-provider, same posture as the M4 capability registry); `run_turn` gains a
`TurnCalls` input that is either `Native(Vec<ToolCall>)` (already-structured, from the API
response) or `Fenced(&OwnOutput)`; fingerprinting is unchanged and **transport-stable** —
`canonical(args)` sorts keys, so the same action grants/pins identically whichever transport
produced it (verify with one test: same args via both paths ⇒ same fingerprint).

**Risks.** Normalization drift (native APIs can send arg types the schema didn't promise —
validate at the boundary, don't trust); dual-transport testing burden (add a parity test
that replays the dispatch test suite through both entry shapes). Brushes invariant #5 only
to strengthen it.

### Q2 — `shell_exec` + OS sandboxing + skills: **ADOPT one shared executor; enforcement at the execution layer, not the hook chain**

**Verdict.** Build **one guarded subprocess runner** (`tools/exec.rs`) and make it the only
way any tool spawns a child process — `shell_exec` first, skills' Python subprocess later on
the identical mechanism. One sandbox to audit, one test surface, no drift between the two
arbitrary-code surfaces.

**Where enforcement plugs in: the execution layer.** The hook chain answers *whether* a call
may run (policy, floor, approval); the executor answers *how contained* it runs (profile
generation, timeout, output caps, kill semantics). Those are different lifecycles — the
chain is a sync decision pipeline, containment is per-process mechanics — and mixing them
would bloat `EventContext` with process config. The `SandboxHook` denylist stays exactly
where it is as the pre-approval floor (locked invariant #1 untouched); `SandboxConfig`
finally gets a consumer: the executor reads it to build the OS profile.

**macOS-first mechanism.** `sandbox-exec` with a generated Seatbelt profile: deny-default;
allow read+write under the workspace root (+ tmp scratch); allow exec; **network off by
default**. It's deprecated-but-functional (Chrome/Bazel still ride it); wrap it behind a
`trait SandboxedSpawn` so Linux (bubblewrap/Landlock) and Windows (job objects + restricted
token) slot in per-platform later without touching call sites. Guardrails in the executor,
not per-tool: ~120s default timeout (config-capped), kill the whole process group on
timeout, output caps (~64KiB head + 16KiB tail with an elision marker — the model does not
need 10MB of build log).

**Network: be honest about what's enforceable.** A per-domain allowlist for arbitrary shell
children is **not** enforceable at "library level" — Seatbelt can gate sockets on/off, not
hostnames. Decide it cleanly: v1 `shell_exec` runs network-**off**; a call that needs
network is a distinct, visible ask (`network: true` arg ⇒ different fingerprint, and it
raises the effective risk — see matrix in Q8). `allowed_domains` enforcement arrives in v2
as a localhost proxy (HTTPS_PROXY env + Seatbelt deny-direct-socket, proxy enforces the
allowlist). Until then `SandboxConfig.network.allowed_domains` stays truthfully documented
as unenforced for shell. (The §3.1 "enforce allowed_domains at library level in v1" line was
written for Rust-native fetch tools, where it *is* enforceable — keep it for those, drop the
claim for shell.)

**Skills.** Confirm the orchestrator's §3.2 refinement: v1 skills stay prompt+resources
only; executable skill scripts ship **only after** this executor exists, and run under it
with a profile derived from the skill's declared `Capability` allowlist (Filesystem →
workspace paths; Network → the same off-by-default posture). Skills never get a private
sandboxing path.

**`shell_exec` classification:** `RiskClass::Dangerous`. Arbitrary shell is the
highest-blast-radius surface in the product even sandboxed (it has workspace write). With
Q8's matrix that means: every call is a fresh human confirm, no session/always coverage —
and pattern-level relief (`shell_exec "git status*"` → Allow) comes from persisted
`tool_rules`, authored deliberately in Settings, not from an approval-dialog habit loop.

**One hardening note found while reading source:** `command_text` for pattern/denylist
matching is currently `"{name} {args}"` with `args` re-serialized JSON. Re-serialization
normalizes `\u`-escapes (good — checked), but for `shell_exec` the floor must match on the
**decoded command string** (plus lowercase + whitespace-collapse, which the denylist already
leans toward), not the JSON envelope — quotes/escaping inside the envelope create needless
mismatch surface. Mechanism: give `Tool` a defaulted `fn match_text(&self, args) -> String`
(default = today's canonical); `shell_exec` overrides it to return the bare command. The
floor stays recall-biased by design; real safety is the executor.

**Risks.** Seatbelt profile bugs fail *open* only if you treat sandbox-exec errors as
warnings — **treat any profile-apply failure as a hard `Err`, never "run unsandboxed."**
That's the fail-closed posture everywhere else; keep it here. Timeline: this is genuinely M3
scope for `shell_exec` as PLAN §8 says, but the executor is the deliverable — don't ship
`shell_exec` as a bare `Command::new()` "temporarily."

### Q3 — Durability trio: **SPLIT — crash-recovery + loud-vs-silent stay M3; journal + idempotency keys move to the first non-idempotent tool**

**Verdict.** The trio isn't one unit; its members have different consumers today.

- **Crash-recovery boot pass + loud-vs-silent: keep in M3** (cheap, has consumers now). On
  core init, in one transaction: terminalize any non-terminal turn/run rows as
  `failed{crash}`, drop/expire any persisted pending-approval artifacts (there are none yet
  — see below — but the pass should exist), and write a durable `tool.interrupted` event so
  the conversation visibly says "a tool call was cut off by shutdown" instead of silently
  losing it. Loud-vs-silent = every failing mutation both returns the error *and* leaves a
  durable event row (this lands nearly free once Q9's audit table exists — build them
  together).
- **Persisted action journal + idempotency keys: MOVE out of M3**, to whichever milestone
  ships the first genuinely non-idempotent external-effect tool (email/calendar/delegate —
  realistically M7/server track). Two reasons. First, no consumer: all six fs tools are
  atomic + read-guarded; a journal today is schema with nothing to protect. Second — and
  this is the Argos P12 lesson your own PLAN §8 M4 note already cites — you have a
  deliberate "one queue model" unification pass scheduled *before* agent-dispatch/cron/outbox
  schemas lock. A journal schema-locked in M3 becomes the fourth overlapping deferred-work
  construct that pass exists to prevent. Let the journal be designed *by* that pass.

**The anchor scenario, answered for today's system:** user clicks Allow → grant lands in the
in-memory ledger → force-quit before `tool.run`. On relaunch: the ledger is gone, the tool
never ran, nothing replays, the boot pass marks the turn interrupted. The user re-asks and
re-approves. **That is the correct answer, and it's correct *because* nothing is persisted.**
Lock this as the design rule the journal must obey when it does land:

> **No half-durability.** Never persist an approval/intent without persisting the execution
> state machine it authorizes (journal row written *before* the side effect, with an idem
> key; boot reconciles `intent-without-effect` → re-confirm, never re-run). A persisted
> grant + volatile run state is precisely the double-execution bug; all-volatile is safe.

That rule also constrains Q8: persisting `Always` as *rules* (not "pending armed actions")
stays on the safe side of it.

**Risks:** none to locked invariants; the risk is someone "helpfully" persisting the ledger
before the journal exists — the rule above is the tripwire, put it in the module doc of
`approval.rs` now.

### Q4 — Batching, sequencing & budgets: **ADOPT — serial dispatch, deny-cascades-to-skip, count-based budgets now, cost budgets ride M4's ledger**

**Verdict, four decisions:**

1. **Serial, in emission order** — confirm what `run_turn` already does, now as policy, not
   accident. Ordering is semantically load-bearing (read→edit→write chains; the read-set
   guard assumes it), approval prompts must serialize anyway, and the concurrency win is
   nil for local models that generate slower than tools run. Parallelism enters the system
   later via `delegate` fan-out (already designed in §3.3) — worker-level, not
   tool-dispatch-level. Don't build concurrent dispatch.
2. **Approval UX for mixed-risk batches: one decision per call**, with the design system's
   `ToolApprovalDialog` "N waiting" counter (already on the backlog — this is its reason to
   ship). No batched "approve all 5" consent: a batch grant is scope-creep in a trenchcoat,
   and fingerprint pinning can't represent it honestly.
3. **Deny cascades to skip.** New rule: when the user **denies** a call, every not-yet-run
   call in the same turn whose risk is non-`Safe` resolves to
   `Denied{by:"batch", reason:"an earlier call in this batch was denied"}` without
   prompting; `Safe` reads still run. Rationale: later calls in a batch are usually
   consequences of the denied one (write B assumes write A), and a user who said "no" once
   should not be asked four more times for the same plan's follow-ons. The model sees
   per-call outcomes next turn and adapts. (User-deny only — a policy/sandbox deny of one
   call does not cascade; those aren't a human saying "stop this plan.")
4. **Budgets: count-based now, cost-based later.** Argos's lesson (06-autonomy §6, and
   OpenClaw's $400 reviewer bills) is that the bound must exist *before* the first runaway,
   and the first runaway needs no cloud bill — a local model in a read→re-read loop wedges
   the app just fine. Ship in M3-remainder, all in the dispatcher (~a day):
   - **Per-turn ceiling:** max tool calls per model turn (default 8; malformed blocks
     count). Excess calls → `Denied{by:"budget"}` + stop dispatching the rest of the turn.
   - **Per-run ceiling:** max dispatches between user messages (default 50) — this is the
     real runaway bound, since the loop iterates turns.
   - **Repeat detection:** identical fingerprint dispatched ≥3× within one run →
     `Denied{by:"budget", reason:"repeat detected — same call, same args"}`. Ring buffer of
     recent fingerprints; trivially cheap since fingerprints already exist.
   - All three surface to the model as instructive outcomes ("stop and summarize"), and to
     the user as a visible notice. Config-cappable per profile later; constants now.
   - **Cost/token ceilings: defer to M4's usage ledger + budget governor** (PLAN already
     resolved these per-profile). Count budgets don't need prices; don't couple them.

**Risks:** ceilings that are too tight strangle legitimate long agentic runs — 8/50 are
starting points, and the outcome text must tell the model the limit exists (so it plans),
not just "denied." Touches no invariant.

### Q5 — Unattended/headless approval: **CONFIRM fail-closed-deny as floor; ADOPT park-and-queue + rule-based pre-authorization; REJECT time-boxed class grants**

**Verdict.** §4's "asks fail closed to deny" stays the default and the floor. But
"fail-closed" alone makes autonomy useless, so the design is a ladder:

1. **Park, don't drop (the queue).** Headless, an `Ask` resolves for *this run* as
   `Denied{by:"unattended"}` — but it also enqueues a durable "needs your approval" item
   (tool, canonical args, fingerprint, requesting job/conversation) into the outbox/
   notification path the server track already builds. The run continues around it (the
   model is told "not granted while unattended — queued for the operator; do what you can
   and report"). On next connect the human reviews the queue; granting there writes a
   **rule** (see 2) so the *next* run succeeds. Asked ≠ approved is preserved perfectly:
   attempting never grants; the attempt just becomes visible instead of vanishing.
   Deferred-resume of the *same* run (park the run itself, continue after grant) is
   explicitly a stretch goal — it needs Q3's journal + resumable run state; don't scope it
   into v1 of the server body.
2. **Pre-authorization = persisted policy rules, authored attended.** The intended path for
   real autonomy: `tool_rules` patterns (Q8) written on the local body, in Settings or from
   the approval-queue review, synced one-way local→server exactly like skills (§4's sync
   posture). "The inbox-watch job may `send_email` to `*@mycompany.com`" is a reviewed,
   visible, revocable rule — the human pre-approves *classes* at authoring time with full
   attention, never at 2am via a push notification. This is Argos's `approvedDomains`
   posture generalized, and it's the answer to "what should headless actually do": follow
   rules it was given; queue everything else.
3. **Time-boxed "pre-approved class" grants: rejected for v1.** A TTL grant is
   approval-fatigue-shaped ("allow for 24h" becomes the new "OK" button), unauditable after
   the fact ("why did this send? — a grant that no longer exists"), and its legitimate uses
   are covered by a scoped rule the user deletes afterward. Revisit only with usage
   evidence.
4. **`Dangerous` is unavailable unattended, full stop.** Invariant #8's floor: no rule, no
   queue-grant, no config can cover a `Dangerous` tool headless — it hard-denies and parks.
   Combined with §4's server seed (shell_exec/computer_use disabled), the headless body's
   worst case is bounded by construction.

**Sketch.** The dispatcher's existing `approver: None ⇒ surface Ask` branch is already 90%
of mechanism — headless wires a `QueueingPrompter` (implements `ApprovalPrompter`) that
enqueues + immediately returns `Deny`, rather than `None`. Rules ride Q8's persisted
`PolicySource`. No new hook needed.

**Risks:** queue spam from a looping job (Q4's budgets bound it — one more reason they ship
first); rule patterns that are broader than the author realized (mitigate: the rule-creation
UI previews what the pattern matches, and Q9's audit shows every call a rule silently
covered).

---

## Tier B — the privacy/approval boundary

### Q6 — Reroute-to-local vs hard-deny: **ADOPT reroute, loud, at the loop level — plumbing in M3-remainder, full UX in M4**

**Verdict.** Worth the complexity — with the product's pitch being "kept local," the current
behavior ("must stay local" ⇒ *error*) reads as the feature failing exactly when it
triggers. But keep the dispatcher out of the provider business:

- **Dispatcher change (small):** when the chain passes and `routing.is_local_required() &&
  is_cloud`, if a local candidate *might* exist, return a new typed outcome
  `ToolOutcome::NeedsLocalReroute{reason}` instead of `Denied`. The dispatcher still never
  runs the tool on a cloud endpoint — invariant #2 intact; the outcome is a refusal with a
  forwarding address.
- **Loop change (the real work):** the agent loop owns providers. On `NeedsLocalReroute` it
  runs `enforce_local_routing` over its candidates: local+private candidate found ⇒ re-issue
  the tool call and run the **rest of this turn's follow-ups** on that endpoint, emitting a
  visible system line/banner — "switched to {local model} for this action ({reason})" —
  never silently. No candidate ⇒ exactly today's hard-deny message. Scope the switch to the
  turn (through the tool-result follow-ups); the next user message re-routes normally.
  Mid-*stream* provider swapping is not attempted — the swap happens at the turn boundary
  where a model call would start anyway, which is why this is tractable.
- **Sequencing:** PLAN §8 M3-remainder already lists "loop consults
  `RoutingRequirement`/`enforce_local_routing` for tool-triggered follow-ups" — land that
  plumbing plus the typed outcome now; the auto-switch UX ships in M4 when the model
  manager makes "the local endpoint" a first-class object instead of a lucky config.

**Risks.** The failure mode to test for: local model is configured but *down/mid-load* —
`enforce_local_routing` picks it, the call then fails; that must surface as "local endpoint
unavailable" (loud), never fall back to the cloud candidate (that fallback is the exact
violation invariant #2 exists to prevent; `enforce_local_routing`'s structure already can't
return cloud on that branch — keep the *retry* logic equally incapable). Also: the reroute
banner is a privacy signal ("this message contained something sensitive") — fine on-device,
but don't propagate the classifier's *reason text* into any cloud-visible context.

### Q7 — MCP into the registry: **CONFIRM M3 item 5, build on §3.5 — with three concrete bindings**

**Verdict.** §3.5's Local/Remote trust-tier split is right (Remote MCP = egress point
through the privacy filter — non-negotiable). The three open mappings:

1. **Capabilities:** per-server config override at registration (§3.5 already says this),
   with structural defaults when undeclared: every tool from a **Remote** server requires
   `Network` (true by construction — calling it is egress); **Local** stdio servers default
   to `[]` plus whatever the registration config declares. Capabilities gate *which body
   offers the tool*; don't overload them as risk.
2. **RiskClass for tools you didn't write — hints may raise, only config may lower.** MCP
   annotations (`readOnlyHint`, `destructiveHint`) are attacker-controlled for a malicious
   server; treat them as hints, never authority. Defaults: Local-tier tool ⇒ `Write`;
   Remote-tier tool ⇒ `External`; `destructiveHint` ⇒ raise to `Dangerous`. `readOnlyHint`
   lowers to `Safe` **only** when the user explicitly marked that server trusted-read-only
   at registration; otherwise it changes nothing. One-line invariant to add to the code:
   *a foreign hint can raise risk, only explicit user config can lower it.*
3. **Guard-wrapping:** unconditional on results — MCP output is the canonical untrusted
   content; it flows through `format_outcome` like every tool. **Plus one surface the
   as-built doc doesn't cover: descriptions.** MCP tool descriptions/schemas enter the
   *system-prompt* catalog — a known injection vector (rug-pull/tool-poisoning attacks).
   `render_tool_catalog` must run foreign descriptions through `neutralize_untrusted`
   (they're exactly the "new trust-boundary token surface" the defang-list gotcha warns
   about), cap their length, and the registration UI should show the user the literal
   descriptions they're installing — same posture as the `save_skill` gate.

Also: **namespace MCP tools** (`mcp__{server}__{tool}`, the CC convention) so registry
names, fingerprints, and `tool_rules` patterns are per-server and a malicious server can't
shadow `read_file`.

**Risks:** trust-tier misassignment by the user is the residual hole (a Remote server
registered as Local bypasses the egress framing) — make the registration UI's tier choice
explicit and default to Remote when ambiguous. Touches invariant #6 (extends it to a new
content source) and the `neutralize_untrusted` fixed-list gotcha.

### Q8 — Persistent policy + RiskClass differentiation: **CONFIRM M4 — but OVERRIDE the grant model: persisted `Always` = rules, never fingerprints**

**Verdict.** This is my biggest override of your provisional thinking, and it dissolves the
question you asked ("how does a persisted grant survive args that necessarily differ?"):

- **A persisted `Always` grant is a `tool_rules` row** — `(tool_name, pattern, Allow)` —
  in the SQLite `PolicySource` the permission hook already abstracts over. Human-readable,
  listed in Settings, revocable, and it composes with the existing most-specific-wins /
  deny-beats-allow resolution instead of adding a parallel store. A persisted *fingerprint*
  is the wrong durable object: nearly useless (next `edit_file` has different content ⇒
  different hash, by design — the pin is *supposed* to be that tight) and unauditable (a
  table of opaque hashes answers "what have I standing-approved?" with a shrug). So:
  `GrantScope::Once`/`Session` stay exactly as built (ephemeral ledger, fingerprint or
  tool); `Always` stops being a ledger scope at all — the approval dialog's "Always allow"
  button *writes a rule*. "The same trusted action with different args" is answered by
  naming the class in a pattern (`edit_file` under `notes/**`), which is what patterns are
  for — not by loosening the fingerprint.
- **Rule vocabulary v1:** whole-tool, and path-prefix patterns for the fs tools (the dialog
  offers "always allow edits under `{dir}/`" — derived from the arg it can see). Free-form
  pattern authoring lives in Settings. Resist arg-shape-general rule builders until there
  are tools that need them (`shell_exec` command patterns arrive with Q2).
- **The grant-scope × risk matrix** (single derivation point, `build_tool_dispatcher` + one
  enforcement check in `ledger.grant`/rule-persist so the UI is never the enforcement):

  | Risk | Default | Once (fp) | Session (fp/tool) | Always (rule) |
  |---|---|---|---|---|
  | Safe | Allow, pre-trusted | — | — | — |
  | Write | Ask | ✓ | ✓ | ✓ (tool or pattern) |
  | External | Ask | ✓ | fingerprint only | **destination-scoped pattern only** — a bare whole-tool Always is refused |
  | Dangerous | Ask | ✓ | **refused** | **refused** |

  - **`Dangerous` (invariant #8's open mechanism, decided): Once-only.** No session
    coverage, no rules, every call a fresh confirm with the risk badge. Simpler and
    stronger than a "second confirm," and it makes the floor a *structural* property of the
    ledger (a `grant(Session|Always, …Dangerous…)` is a refused call, testable), not a UI
    behavior.
  - **`External` gates differently from `Write` in two ways:** the approval dialog must
    display the *destination* (domain/recipient/target — where it goes is the consent), and
    standing permission requires the destination in the pattern
    (`send_email to:*@mycompany.com`, `fetch domain:docs.rs`). A whole-tool "always allow
    any email to anyone" is refused at the persistence layer. This is Argos's novel-domain
    approval MUST, generalized — it's the only thing standing between a policy-permitted
    egress tool and exfiltration to an attacker-chosen endpoint. Session-scope for External
    is fingerprint-pinned only (that exact destination+content), not tool-wide.

**Sketch.** SQLite `PolicySource` impl (schema already sketched in §3.1: `tool_rules`);
`ApprovalDecision::Approve` gains a `Persist(rule)` variant the dialog produces for
"Always"; `ledger.grant` grows the risk-matrix refusals; `build_tool_dispatcher`'s match arm
splits `External`/`Dangerous` out of the current `Write` bucket (the `lib.rs:236-244` gotcha
the as-built doc flags). Ship the risk-differentiated `ToolApprovalDialog` (risk badge) in
the same change — the current dialog offering "Allow session" uniformly will train habits
the matrix then has to break.

**Risks.** Migration is nil (nothing persists today — this is why deciding it *now*, before
M4, matters). Brushes invariant #4 positively (Once semantics unchanged) and #8 (this *is*
its mechanism decision).

### Q9 — Audit trail: **ADOPT now (M3-remainder/M4 seam) — the first real ObserverHook, one append-only table**

**Verdict.** Build it early and cheap; for a product whose pitch is an inspectable boundary,
the audit table is the pitch made queryable — and it's the debugging instrument for
everything else in this doc (budgets, rules, reroutes). It is *not* Q3's journal: audit is a
post-hoc *record* (observer lane, never blocks, written after the fact); the journal is
pre-effect *intent*. Same vocabulary, different rows — build audit first; the journal later
reuses its column vocabulary plus an idem key and a pre-effect write point.

**Sketch.** `tool_audit` in the **per-profile** DB (same isolation logic as the usage-ledger
decision PLAN already made): `ts, conversation_id, turn_id, tool_name, canonical_args
(size-capped), fingerprint, risk, outcome (ok/err/denied/asked), gate ("by" — which hook),
grant_used (once-fp/session-fp/session-tool/rule-id/pre-trusted), decision (for asks:
approve-scope/deny/timeout), endpoint_kind (local/cloud), duration_ms`. Implemented as the
first concrete `ObserverHook` on a now-wired `PostToolUse` event (fired from `dispatch`
after the outcome exists — outcome-shaped, so it can't gate by construction). Denied/asked
calls are rows too — refusals are the *interesting* audit entries. Redaction pass over
`canonical_args` (secret-shaped strings) before write is a fast follow, flagged not blocking.
UI: none required to ship; a Settings "Activity" pane reads the table later. On the future
server body, §3.4's rule applies: observer writes durably before returning.

**Risks:** args logging vs privacy — it's per-profile, on-device, and size-capped, which is
consistent with the product posture; never sync audit rows to the server by default. Cheap
(~1–2 days). No invariant contact.

---

## Tier C — parity & UX

### Q10 — Single-in-flight: **CONFIRM deferral — with one free frontend mitigation**

Acceptable for v1; the lock-release-while-parked change is a concurrency-model refactor that
should happen once, deliberately, not as a UX patch (the comment at `dispatch.rs:211`
already says this — I agree with it). Do add the zero-risk mitigation now: while an approval
is outstanding the frontend *knows* (it's rendering the dialog) — disable the composer with
"waiting for your approval above" instead of letting a second send block silently. Ship the
cancel command together with the real refactor, not before.

### Q11 — Parity gaps, ranked by risk-reduction per engineering-day

1. **Protected-paths always-Ask floor — pull to M3-remainder (~a day).** The workspace is
   confined, but the workspace *itself* will contain `.git/`, config files, eventually
   secrets — and Q8's Allow-rules plus Q2's `shell_exec` both weaken "gated by default"
   inside it. A tiny always-Ask floor under any future Allow is the classic cheap insurance.
   Mechanism: per invariant #1's soft note, no reorder needed — a small hardcoded path list
   checked as a new gating hook between Sandbox and Permission, returning `Ask` regardless
   of policy, satisfiable by `Once` grants only (it's a floor — session coverage would
   neuter it). Rides the same dialog.
2. **`UserPromptSubmit` hook — fine at M4, it's mostly structural.** The user's message
   path is *already* privacy-gated in the agent loop (message → classify → route is the M1
   spine), so re-expressing it as a hook adds one-place-ness and a future annotation point,
   not new coverage. Real but small risk delta; do it when the hook chain is next open
   anyway (M4's `PostToolUse` wiring from Q9 is a natural moment).
3. **Permission modes (plan / accept-edits) — confirm M4+.** Pure ergonomics, zero
   risk-reduction (accept-edits *reduces* friction, i.e. spends safety margin), and it
   should be designed against Q8's matrix so a mode can never widen `External`/`Dangerous`.
   Last of the three, as your plan already had it.

---

## Things you didn't ask

- **`(Once, Tool)` upstream enforcement:** the ledger no-ops it (correct), but nothing stops
  a future prompter/UI from *sending* that combo, which then silently grants nothing and
  the user's click is lost — re-prompting them next call. Force `Once ⇒ Fingerprint` at the
  `ApprovalDecision` construction site and log if the ledger ever sees the combo.
- **Fingerprint stability across transports** (repeated from Q1 because it's easy to lose):
  add the one regression test now — same tool+args via fenced parse and via a synthetic
  native call must produce identical fingerprints, or Q8's whole grant model quietly forks.
- **Catalog injection surface:** `render_tool_catalog` currently trusts `description()`
  because all tools are first-party. Q7 breaks that assumption; neutralize foreign
  descriptions *when MCP lands, in the same PR* — it's exactly the "new trust token must be
  added to the defang list" trap the as-built doc warns about.
- **Approval-dialog risk badge is load-bearing, not polish.** Q8's matrix only communicates
  through it ("why can't I session-allow this?" — because it's red). Reprioritize
  `ToolApprovalDialog` from UI-polish backlog to M4-with-Q8.

## Do-now list (order of execution)

| # | Item | From | Size |
|---|---|---|---|
| 1 | `OwnOutput` newtype for `parse_tool_calls` | Q1 | ~½ day |
| 2 | Turn/run call budgets + repeat detection + deny-cascades-to-skip | Q4 | ~1–2 days |
| 3 | Protected-paths always-Ask floor hook | Q11 | ~1 day |
| 4 | Crash-recovery boot pass + `tool.interrupted` loud event | Q3 | ~1–2 days |
| 5 | `tool_audit` table + `PostToolUse` ObserverHook | Q9 | ~1–2 days |
| 6 | `NeedsLocalReroute` typed outcome + loop consults `enforce_local_routing` | Q6 | ~1–2 days |
| 7 | Guarded subprocess executor (`tools/exec.rs`, Seatbelt) → then `shell_exec` (Dangerous) | Q2 | the big one |
| 8 | MCP into registry: namespacing, tier→risk defaults, description neutralization | Q7 | M3 item 5 |

M4 then carries: native tool-use + `schema()` (Q1), persisted rules + grant/risk matrix +
risk-badged dialog (Q8), reroute UX (Q6), `UserPromptSubmit` (Q11). Journal/idempotency (Q3)
and headless approval queue + rule sync (Q5) ride the server track with the first
non-idempotent tool.
