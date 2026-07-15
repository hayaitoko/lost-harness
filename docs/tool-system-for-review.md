# Lost Harness — Tool System (as built), for Fable's review

**Audience:** Fable (author of the Argos harness spec). **Prepared by:** the Lost Harness build agent, 2026-07-15.
**Status of the system described:** shipped + tested on `main` (226 lib tests, 0 failed), unless a line says "planned."
**Accuracy:** every as-built claim below was fact-checked against source (`file:line` in `docs/codebase/{tools,hooks-gating-and-approval}.md`).

---

## Prompt to hand Fable (copy-paste this)

> You designed **Argos** (`~/claude/harness-spec/`) — a daemon-first TS/Node agent harness. Lost Harness
> mined its *mechanisms* (resource accounting, untrusted-content handling, prompt shaping, reconnection)
> but is the **inverse topology**: an app-first native Rust/Tauri desktop product that is fully functional
> offline, with an *optional* headless server companion. See `docs/argos-review.md` for our TAKE/MISSING/
> UNSURE read of your spec.
>
> This document describes the **tool-calling system we actually built** (the registry, the one-gate hook
> chain, the approval spine, the injection defenses, read-before-write, the filesystem tools). It's real
> Rust with tests, not a proposal.
>
> **What I want from you:** review it, then **make decisions** on the "Open questions" section and hand
> back a **finished answer**. Each question carries a *provisional placement* pulled from our own planning
> (`docs/PLAN.md`) — so your job is to **confirm or override** it, not design from a blank page. For each:
> a verdict, the rationale, a rough implementation sketch, and any risk. Lean hardest on **Tier A** (native
> tool-use, `shell_exec`/skills sandboxing, durability, tool-call budgets, unattended approval) — that's
> where your Argos experience is direct leverage. Push back on anything you think is wrong.
>
> **Constraints:** the "Locked invariants" section locks security *properties*; where a property currently
> holds only by an implementation *mechanism*, that mechanism is explicitly marked open for you to harden.
> Treat unravelling a locked *property* as a last resort and say so. Ground recommendations in what's built
> (file pointers given); read the source at `~/Desktop/lost-harness-product/src-tauri/src/`.
>
> Deliverable: a decision doc I (Lukas) can act on directly.

**Context to read (in order):** this doc → `docs/codebase/tools.md` + `docs/codebase/hooks-gating-and-approval.md`
(the precise as-built subsystem guides, with `file:line` refs) → `docs/tooling-and-skills.md` (the original
design reasoning — several open questions reference its §3.2/§3.5/§4) → `docs/PLAN.md` §8 (M3 build order) and
§12 (Claude Code parity check). Source lives at `src-tauri/src/{tools,hooks,agent,ipc}/`.

---

## 1. The frame

Lost Harness is a **privacy-boundary chat client**: every call out to a model is classified and routed
(kept local / sent to cloud / blocked) *before* it can leave the machine. Tools are how the assistant
*acts*, so the tool system is where that boundary becomes load-bearing for actions, not just text.

Two design commitments shape everything below:
1. **One Rust core, two possible "bodies."** The same core compiles into the desktop app and (later) a
   headless server companion; each offers a different **capability set** to the same tool registry.
2. **The privacy/approval boundary fails closed.** Sensitive content structurally cannot fail over to the
   cloud; state-changing actions structurally cannot self-authorize.

## 2. The spine — capabilities, tools, risk (`tools/mod.rs`)

- **`Capability`** — what a tool needs from its environment: `Filesystem, Network, Shell, Display, Audio,
  ComputerUse, Email, Calendar, WebResearch, LongCompute`.
- **`BodyEnv`** — what a running body *offers*. `app_default()` = Filesystem/Network/Shell/Display/Audio/
  ComputerUse/WebResearch. `headless_server_default()` = Filesystem/Network/Email/Calendar/WebResearch/
  LongCompute (no screen/audio/computer-use). The registry filters: a `Display`-requiring tool is simply
  **absent** from a headless environment's tool list — the model is told *why* rather than the tool
  failing at call time. *As built: the app only ever constructs `app_default()`; the headless shape is
  defined + unit-tested but no headless body exists yet.*
- **`RiskClass`** — `Safe | Write | External | Dangerous`. This one property **derives the gating**
  (`lib.rs::build_tool_dispatcher`): `Safe` → whole-tool `Allow` + pre-trusted (no prompt); everything
  else → `Ask` through the approval spine. So a new tool's gating is automatic from its `risk()` — there's
  no separate registry to keep in sync. *As built: `External`/`Dangerous` are declared but unused, and
  currently gated identically to `Write` (both → Ask).*
- **`Tool` trait** — `name`, `requires() -> &[Capability]`, `run(input, ctx) -> Future<ToolResult>`; plus
  defaulted `description`, `risk` (defaults `Safe` — **every mutating tool must override**), `available`.
  *`ToolInput.args` is a bare JSON value today; a typed per-tool schema is flagged in-code as later work
  (relevant to Q1).*

## 3. The one-gate hook chain (`hooks/`)

Every tool call passes through **one ordered chain, first "no" wins**:

```
[ PrivacyFilter ] → [ Sandbox ] → [ Permission ] → [ FirstUseConfirm ]
   deny-only        hardline,        per-tool/         "have we
   + annotates      non-overridable  pattern policy    confirmed this?"
   route-local      denylist         (Allow/Ask/Deny)
```

- **PrivacyFilter** — adapts the privacy gate. `Allow→Continue`, `Block→Deny`, `RouteLocal→Continue` **+**
  annotates `ctx.routing = LocalRequired{reason}`. It never itself denies a route-local call — it marks it.
- **Sandbox** — a fixed hardline denylist (`rm -rf /`, `curl|sh`, credential paths, …). A **bare unit
  struct with no config** — that *is* the invariant: nothing can configure it away, and it sits before any
  Ask-capable hook so a permissive policy can never let a denylisted command reach human confirmation. (A
  `SandboxConfig` *shape* exists for future OS-level enforcement but is not consulted today.)
- **Permission** — most-specific matching rule wins; `Deny > Ask > Allow` on ties; an unconfigured tool
  falls through to `Continue` (→ FirstUseConfirm decides), not an implicit ask. Backed by a `PolicySource`
  trait (in-memory today; a SQLite-backed source is the intended drop-in).
- **FirstUseConfirm** — `Continue` if pre-trusted (construction-time) or covered by a ledger grant, else
  `Ask`. Crucially, **asking does not mark the tool confirmed** — only a real grant does.

## 4. Dispatch + the approval spine (`tools/dispatch.rs`, `hooks/approval.rs`, `ipc/approval.rs`)

`ToolDispatcher::dispatch` is the load-bearing junction: **resolve → capability-availability → gating
chain → (approval pause/resume) → execute.** On an `Ask`, if an interactive prompter is wired it **pauses,
prompts the human, and on approval re-runs the FULL chain from the top** (so Sandbox/Privacy are always
re-checked, not just Permission), bounded at 4 rounds, deny/timeout fail closed.

The approval spine's anti-drift design:
- **`ActionFingerprint`** = SHA-256 over `tool_name + canonical(args)` (keys sorted, so stable). A one-time
  grant **pins to this exact action** — it can't drift to a different call.
- **`ApprovalLedger`** — `Once` (consumed at execution), `Session` (until restart), `Always` (**aliased to
  Session today** — no persistent store yet). `GrantTarget` is `Fingerprint` (this action) or `Tool` (any
  call to that tool — a deliberate broadening). `(Once, Tool)` is a no-op by design (nothing to pin to).
- **`TauriApprovalPrompter`** emits `tool:approval_request`, parks a oneshot keyed by request id, awaits
  with a 300s deny-by-default timeout; `resolve_tool_approval` touches only the registry (never the stream
  lock → no deadlock). Frontend: the currently-wired `ApprovalDialog` (Deny / Allow once / Allow session);
  the design system's richer `ToolApprovalDialog` (risk badge + "N waiting" counter) is the intended
  replacement, still on the UI-polish backlog.

## 5. Injection defense (`tools/calling.rs`)

Small local models call tools via a **fenced text dialect** (` ```tool ` blocks). Two rules make this safe:
1. **Parse only the model's own current-turn output.** `parse_tool_calls` will parse any string it's given;
   the *entire* defense is caller discipline — the one caller (`run_turn`) feeds it only the model's fresh
   text, never history / tool output / web content. So a read web page can't forge a call. (Enforced by
   discipline, not the type system — see invariant #5, whose mechanism is open for you to harden.)
2. **Guard-wrap untrusted output.** Anything the model didn't author (tool results) re-enters its context
   inside a nonce-delimited `<<<LH-UNTRUSTED:{uuid}…>>>` block, with backticks and the trust-boundary
   banner strings neutralized so a forged block can't survive being echoed. `neutralize_untrusted` is a
   fixed defang list (fence + both banners + nonce prefix) — new trust tokens must be added to it.

## 6. Read-before-write (`tools/fs.rs`, `tools/mod.rs`) — shipped 2026-07-15

A conversation-scoped read-set (`ConversationReads`) owned by the dispatcher, injected via `ExecCtx`.
`read_file` records the canonical path; `write_file` (existing target) and `edit_file` refuse a path not
in the set ("read it first"); new files + `delete_file` are exempt; a successful write self-records. This
matches Claude Code's blind-clobber guard. *(An adversarial review caught a macOS case-insensitive path bug
here — write now canonicalizes the existing target for the membership check.)*

## 7. The filesystem tools (`tools/fs.rs`) — the only real tools today

Six tools, all workspace-confined (reject `..`, absolute paths, symlink escape via canonicalize):
`read_file` / `list_dir` / `search_files` (Safe, pre-trusted) and `write_file` / `edit_file` / `delete_file`
(Write, gated through approval). `write_file` is atomic (temp + rename, cleanup on any failure) and refuses
to write through a symlink leaf; `edit_file` requires the target substring to match **exactly once**.
*(Skills — planned — will add a `Tool` that runs a Python subprocess under a capability allowlist:
`tooling-and-skills.md` §3.2. A second code-execution surface alongside `shell_exec` — see Q2.)*

---

## Built vs. planned

| Area | State |
|---|---|
| Registry + capabilities + per-body filtering | ✅ built |
| One-gate hook chain (privacy/sandbox/permission/first-use) | ✅ built |
| Fenced dialect + injection defense | ✅ built |
| Approval spine (fingerprint pin, ledger, prompter, dialog) | ✅ built |
| Read-before-write | ✅ built |
| Filesystem tools (read/list/search/write/edit/delete) | ✅ built |
| Local-required routing floor (fail-closed on cloud) | ✅ built |
| **Native tool-use** (endpoint's real tool API) | ❌ planned (PLAN §12, M4) — fenced dialect is the fallback |
| **`shell_exec`** + real OS-level sandbox | ❌ planned (PLAN §8 M3 + §11) — `SandboxConfig` shape exists, unenforced |
| Skills execution (Python subprocess `Tool`) | ❌ planned (`tooling-and-skills.md` §3.2) |
| Headless browser, delegate, ask-human, system-status, cron, session-search | ❌ planned (PLAN §8 M3) |
| **MCP tools into the registry** | ❌ planned (PLAN §8 M3 item 5) |
| **Durability trio** (crash-recovery, idempotency, loud-vs-silent) | ❌ planned (PLAN §8 M3 item 8) |
| Persistent policy store (`Always` across restart) | ❌ planned (PLAN §12, M4) |
| Reroute-to-local (vs hard-deny) for local-required calls | ❌ planned |
| Permission modes, protected-paths floor, `UserPromptSubmit` hook | ❌ planned (PLAN §12, "medium", M4+) |
| Tool-call budget / audit trail / unattended-approval semantics | ❌ not yet designed |
| Headless server body (uses `headless_server_default`) | ❌ planned (post-M4) |

## Locked invariants — properties are load-bearing; some mechanisms are open

Critique a locked *property* only as a last resort, and say so. Where a property holds today only by an
implementation *mechanism*, that mechanism is **explicitly open** for you to harden.

1. **Privacy is deny-only and evaluated first; the sandbox floor is non-overridable and runs before any
   Ask-capable hook.** *Soft:* the relative order of **Permission vs. FirstUseConfirm is NOT load-bearing** —
   permission modes (Q11) may reorder or short-circuit them.
2. **`RouteLocal` never silently degrades to "allow on cloud."** The dispatcher hard-denies a local-required
   call on a cloud endpoint; `enforce_local_routing` fails loudly (a named error), never falls back to a
   cloud candidate. *(Direct constraint on Q6.)*
3. **Asked ≠ approved.** An unattended agent can't self-grant a state-changing tool by attempting it. *(Q5
   asks what "unattended" should then actually **do**.)*
4. **A `Once` grant is per-action (fingerprint-pinned), consumed the instant gating passes.**
5. **A tool call can never be forged from content the model merely *read*.** ← **property locked.** The
   current *mechanism* — single-caller discipline feeding `parse_tool_calls` only the model's own
   current-turn text — is **open for redesign** (it's discipline, not the type system; and Q1's native
   tool-use changes the transport). Propose structural hardening here.
6. **Untrusted tool output is guard-wrapped before it re-enters model context.**
7. **Filesystem tools are workspace-confined; `atomic_write` never leaves a half-written file; `edit_file`
   requires a unique match.**
8. **An irreversible / high-blast-radius (`Dangerous`) action can never be *silently* covered by a
   Session/Always grant — a human check is a floor.** ← **property locked** (it's the fail-closed pitch).
   The confirmation *mechanism* (a second confirm? always-ask even if granted?) is open — see Q8.

---

## Open questions — please decide

Each carries its **provisional placement** from our planning (`docs/PLAN.md` §8/§12, or "new/unplaced") so
you're **confirming or overriding real prior art**, not designing from a blank page. Three tiers; Tier A is
where your Argos experience is the most direct leverage.

### Tier A — execution & resource discipline (your Argos wheelhouse)

**Q1 — Native tool-use vs. the fenced dialect.** *Provisional: PLAN §12, M4; fenced dialect stays fallback.*
Capable endpoints have real tool-calling; the fenced dialect exists for small local models. Adopt native
tool-use via a per-endpoint capability flag that switches `run_turn` between native and fenced, feeding one
normalized internal `ToolCall`? **Prerequisite to decide with it:** `ToolInput.args` is bare JSON today —
native APIs generally need a per-tool JSON schema, so does Q1 pull typed schemas forward? Do native tool
*results* still need guard-wrapping (presumably yes), and what happens to invariant #5's mechanism when the
transport is structured?

**Q2 — `shell_exec` + real OS-level sandboxing (and skills).** *Provisional: PLAN §8 M3 (shell_exec) + §11
(sandbox); `SandboxConfig` shape exists, unenforced.* Today's `SandboxHook` is a heuristic substring *floor*,
not a sandbox. Run `shell_exec` safely macOS-first (Seatbelt/`sandbox-exec`? helper process?) with the
denylist floor + network allowlist + ~2-min timeout + output caps — and where does OS enforcement plug in
(hook chain vs. execution layer)? **Fold in:** skills execute as a `Tool` wrapping a Python subprocess
(`tooling-and-skills.md` §3.2) — a *second* arbitrary-code surface, possibly shipping sooner. Should one
sandboxing mechanism cover both?

**Q3 — The durability trio.** *Provisional: PLAN §8 M3 build-order item 8.* Anchor scenario: *the approval
dialog is showing, the user clicks Allow, the app force-quits before the tool executes — what happens on
relaunch?* Today's 6 fs tools are already atomic/idempotent-ish (atomic_write + read-before-write), so the
double-run risk stays hypothetical until a genuinely non-idempotent external-effect tool exists (email send,
calendar invite, delegate). **Decide:** the minimal durable design for an app-first (not daemon) body —
persisted action journal + idempotency keys, or something lighter? And does it belong in M3 as scheduled, or
move to whichever milestone first ships a non-idempotent tool?

**Q4 — Batching, sequencing & budget of tool calls within a turn.** *Provisional: new / unplaced.*
`parse_tool_calls` returns a *Vec* — the model can emit several calls in one turn. Serial or concurrent
dispatch? How are mixed-`RiskClass` calls in one batch approved (one dialog per call, or a batched consent)?
And the resource-accounting you mined into us but we never bounded: a **per-turn / per-conversation tool-call
budget** (call-count ceiling, cost ceiling, cycle/repeat detection) — the only bound today is the 4-round
*approval-retry* cap (one call's re-prompts, not total volume). *(Distinct from Q10's single-in-flight send.)*

**Q5 — Unattended / headless approval semantics.** *Provisional: `tooling-and-skills.md` §4 says the server
"asks fail closed to deny."* Invariant #3 says an unattended agent can't self-grant — so what should a
headless body (no human to answer an `Ask`) actually **do**? Fail-closed-deny every gated call (safe but
useless for autonomy)? Pre-authorize specific tools via a policy allowlist? A time-boxed "the human
pre-approved this class" grant? The most daemon-shaped question here.

### Tier B — the privacy/approval boundary

**Q6 — Reroute-to-local vs. hard-deny.** *Provisional: planned / unplaced.* A tool-triggered `LocalRequired`
call on a cloud endpoint is hard-denied today. Better UX: switch the loop to a local endpoint for the rest of
the turn (`enforce_local_routing` exists). **Hard constraint: must not violate invariant #2** (never silently
continue on cloud). Is mid-turn client/provider re-selection worth the complexity?

**Q7 — MCP tools into the registry.** *Provisional: PLAN §8 M3 item 5; trust tiers sketched in
`tooling-and-skills.md` §3.5 — build on / critique it, don't reinvent.* §3.5 already distinguishes **Local**
MCP (spawned by this device) vs. **Remote** MCP (an egress point routed through the privacy filter). How do
MCP capabilities map onto our `Capability` enum, how is `RiskClass` assigned to a tool we didn't write, and
how does guard-wrapping apply to MCP results?

**Q8 — Persistent policy + `RiskClass` differentiation.** *Provisional: PLAN §12, M4; `Always` aliases
`Session`.* Concrete unknowns: a persisted `Always` grant — target `Fingerprint` or `Tool`? How does a
persisted grant survive a later call whose args *necessarily* differ (`edit_file` with new content) yet
should still count as "the same trusted action"? Per invariant #8, `Dangerous` can't be silently covered —
pick the concrete confirmation *mechanism*. Should `External` (reaches off-machine) gate differently from
`Write`?

**Q9 — Audit / observability trail.** *Provisional: new / unplaced.* A durable, queryable log of every tool
call — what ran, with what args, approved by whom, when. Distinct from Q3 (crash-recovery/idempotency; this
is the *record*). For a product whose whole pitch is an inspectable privacy/approval boundary, users will
want to audit this. Shape? Does it reuse the non-silent memory/event infrastructure?

### Tier C — parity & UX

**Q10 — Single-in-flight concurrency.** *Provisional: deferred (documented at `dispatch.rs:211`).* Scoped
narrowly: a **second user *send*** blocks while an approval prompt is outstanding (the stream lock is held
across the await). NOT the same as Q4 (multiple calls within one turn). Release the lock while parked + a
cancel command now, or acceptable for v1?

**Q11 — Remaining Claude-Code parity gaps.** *Provisional: PLAN §12 rates all three "medium," M4+.*
Protected-paths always-Ask floor; permission modes (plan / accept-edits, which may reorder
Permission/FirstUseConfirm — now soft per invariant #1); a `UserPromptSubmit` hook (run the privacy filter on
the *user's* message too). Rather than "which are v1-worthy," rank them by **risk-reduction per
engineering-day** and say which (if any) you'd pull earlier than M4.

## What "a finished answer" looks like

For each question: a **verdict** (confirm the provisional placement / adopt this design / defer to milestone
N / redesign as follows), **why**, a **rough implementation sketch** (files/traits), and **risks / what it
touches** (name any locked invariant it brushes). Where you're just confirming our provisional placement, one
line is fine — spend your depth where you'd *change* it. Plus anything we didn't ask that you'd add. Terse is
good — Lukas acts on it directly.
