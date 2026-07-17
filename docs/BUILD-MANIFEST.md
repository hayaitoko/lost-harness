# Lost Harness — Build Manifest (the ultracode backlog)

**Directive (Lukas, 2026-07-17):** *build everything that's been spec'd*, then — as a
separate phase — prove it all works. This document is the executable backlog for the
build phase: every spec'd-but-unbuilt item, dependency-ordered into waves, with the
independent items in each wave marked parallel so a multi-agent run can fan out.

**This is an index, not a spec.** The real designs live in [`PLAN.md`](PLAN.md) (source of
truth), [`tool-system-build-plan.md`](tool-system-build-plan.md) + [`tool-system-decisions.md`](tool-system-decisions.md)
(the tool/approval spine, "Qn" items), [`server-companion.md`](server-companion.md), and
[`tooling-and-skills.md`](tooling-and-skills.md). Each item below points into them. **Do not
re-derive a design that already exists — read the pointer first.**

---

## How the orchestrator should use this

1. **Go wave by wave.** A wave's items may assume every earlier wave is merged. Within a
   wave, items tagged **∥ parallel** have no ordering dependency on each other — fan them
   out concurrently. Items tagged **⇢ after N** wait on item N.
2. **Respect the tier flag.**
   - **Tier A — build directly.** An executable-grade spec exists; go straight to
     implement → adversarially review → verify → commit.
   - **Tier B — design pass FIRST.** The spec is architecture-level, not
     implementation-level. The item's *first* sub-task is a design pass (produce a
     `docs/plans/…md` in the house format, land it, then build against it). Never fan a
     swarm straight at a Tier-B item — it will produce plausible-but-wrong code.
3. **Honor the invariants.** The load-bearing rules in `tool-system-build-plan.md`
   ("Locked invariants") and PLAN §1 non-negotiables hold for every item. The privacy
   filter is load-bearing; the danger-floor is non-overridable; untrusted content is
   guard-wrapped; local-first / works-offline is not negotiable.
4. **Verify per item, then per wave.** Per item: `cargo test --lib` green +
   `npm run build`/`check` clean + `cargo build --lib --no-default-features` clean +
   `cargo clippy --lib` 0 errors. Per wave: update [`ROADMAP.md`](ROADMAP.md) (stage line,
   milestone board, checklist) and add a [`../HANDOFF.md`](../HANDOFF.md) session-log entry.
5. **Adversarial review is not optional.** Every non-trivial item gets a fresh-context
   multi-lens review before commit — that pattern is *why* this repo is healthy (see the
   HANDOFF session logs). Flag findings, fix, re-verify.
6. **The "prove it works" phase is OUT OF SCOPE here** — it's the next directive after this
   backlog is drained. Do not divert into end-to-end dogfooding mid-build; just keep each
   item's own verification tight.

**Baseline at manifest time (2026-07-17):** `cargo test --lib` → **385 passing**, frontend
build + `svelte-check` clean, `--no-default-features` clean, tree clean on `main`. Schema
versions: GLOBAL v4, PROFILE v5.

---

## Wave 1 — Finish the started subsystems  *(all Tier A, all ∥ parallel)*

The subsystems that are live-but-partial. Small, well-specified, no cross-dependencies.

| # | Item | Spec | Done when |
|---|---|---|---|
| 1.1 | **Native-tool UI checkbox** — add a `supports_native_tools` control to the add-provider Settings form; thread it through the `addProvider` store call + `AddProviderArgs`. The flag/persistence/hydration + backend already exist and are live-proven; only the UI to *set* it is missing, so everyday chat still uses the fenced fallback against a native-capable endpoint. | ROADMAP "next round"; Q1 | A provider added via the UI with the box ticked uses the native transport in a real send (not just the env-gated test). |
| 1.2 | **Memory embedder settings toggle** — a per-profile "semantic memory search" on/off setting that gates whether the embedder loads + whether the meaning lane runs (keyword-only when off). | PLAN §9 ("bundled and settings-gated") | Toggling it off makes memory search keyword-only and skips embedder load; on restores hybrid. Setting persists per-profile like the classifier settings. |
| 1.3 | **Curated-summary snapshot at turn 1** — freeze the curated summary once per conversation instead of re-reading it live every turn (PLAN §9 wants it stable for prompt-cache reuse; a fact saved mid-conversation shows up *next* conversation). | PLAN §9 "Timing and trust" | The summary injected into a conversation is computed once at its first turn and reused; a mid-conversation `remember` does not alter the current conversation's loaded summary. |
| 1.4 | **Inline "remembered" save event** — a non-silent, content-free `memory:event {kind:"remembered"}` → transient banner, matching the existing "recalled" event, so a save leaves a visible trace beyond the approval prompt. | PLAN §9 "Memory is non-silent" | Saving a fact (agent `remember` or manual) surfaces a dismissible "remembered …" trace in the same event-bar language as recall. |
| 1.5 | **Walled-profile memory DB routing** — the §7 "keep this profile's memory private" toggle: a walled profile's facts live in its OWN physically-separate profile-scoped memory DB (not `global.db`), reading nothing shared / writing nothing back. Physical separation, not a query filter. | PLAN §7 + §9 "Storage" | Flipping a profile to walled routes its memory reads/writes to a separate DB; shared facts are invisible to it and its facts never enter `global.db`; proven by a test that the wall survives toggling back. |
| 1.6 | **Classifier `gate.rs`/§7 rename cleanup** *(low-value, optional)* — code still says `PrivacyGate`/"§7" while docs say "the privacy filter". Cosmetic; do only if a wave has slack. | HANDOFF "Pending cleanup"; PLAN §11 | Names align with docs, no behavior change, tests green. |

---

## Wave 2 — Remaining core tools + the tool-system Part-2 spine  *(Tier A)*

Everything the approval/hook spine was built to carry, now filled in. The tools (2.1) are
∥ parallel with each other and with the spine items (2.2–2.5).

| # | Item | Spec | Dep | Done when |
|---|---|---|---|---|
| 2.1 | **The remaining core tools** — `headless browser`, `delegate`, `ask-human`, `system status`, `cron management`, `session search`. Each rides the existing registry + approval spine; each declares its `RiskClass`, `Capability` needs, and `schema()`. `ask-human` is the single blocking "ask the user" tool. | PLAN §8 M3 item 10; §6; Fable memory/turn-discipline | ∥ | Each tool dispatches through the hook chain, is gated per its risk, guard-wraps its output, and has tests incl. a denied-call-never-runs case. |
| 2.2 | **`UserPromptSubmit` hook + permission modes** — gate/annotate the user message before processing (natural home for the privacy filter on the inbound side); plan/read-only + accept-edits modes, designed against Q8's matrix so a mode can never widen `External`/`Dangerous`. | Q11 items 2–3; PLAN §12 item 4–5 | ∥ | The privacy filter runs as a `UserPromptSubmit` hook; a plan/accept-edits mode changes gating within the matrix bounds; tests prove a mode can't widen a Dangerous grant. |
| 2.3 | **Reroute auto-switch UX** — the loop-level reroute-to-local plumbing shipped (item 6); this is the M4 UX: toast styling + a first-class "the local endpoint" object in the model manager. | Q6; ROADMAP | ⇢ 3.1 (model-manager endpoint object) | A cloud turn that must stay local visibly rers to the configured local endpoint with a clear, non-alarming toast. |
| 2.4 | **Headless approval queue + rule-based pre-authorization** — `QueueingPrompter` implementing `ApprovalPrompter`; park-and-queue instead of block when unattended; rules ride the Q8 `PolicySource`. Server-track prep. | Q5 | ∥ | An unattended approval parks in a queue (fail-closed floor intact) and can be pre-authorized by a rule; nothing auto-grants a Dangerous action. |
| 2.5 | **Durability journal + idempotency keys** *(Q3 deferred half)* — persisted action journal + idempotency keys on mutating actions; obey "no half-durability"; design it *by* the one-queue-model unification pass (4.4), not before. Lands with the first non-idempotent external-effect tool (email/calendar/delegate). | Q3; PLAN §3 durability trio | ⇢ 4.4 | A double-fired mutating action executes once; a crash mid-action leaves no half-state; journal replays cleanly. |

---

## Wave 3 — The rest of M4 (model manager)  *(Tier A)*

| # | Item | Spec | Dep | Done when |
|---|---|---|---|---|
| 3.1 | **Model seats** — named seats (Writer, Reviewer, Coding, …) resolved to an actual model at run time, not hardwired names; the add-provider/model-manager surface to bind them. | PLAN §4, §8 M4 | ∥ | An agent/tool references a seat; the seat resolves to the bound model; rebinding a seat changes behavior with no code change. |
| 3.2 | **Usage ledger + budget governor** *(per-profile)* — real cost accounting (local = $0, unknown = a visible "flying blind" flag, never a silent guess) + a budget cap for unattended work. Count-based budgets already exist (Q4); this adds the cost ledger. | PLAN §3 (usage ledger), §8 M4; Q4/Q8 | ∥ | Every model call books to a per-profile ledger; an unknown-cost call is flagged not guessed; a budget cap halts unattended spend. |
| 3.3 | **Cache-shaped prompt assembly + context compaction** — frozen prefix tiers, live data quarantined to the tail (KV-cache reuse); AND the context-compaction pass itself. Compaction is the prerequisite that makes Memory's pre-compaction flush trigger (3.5) real. | PLAN §3 (cache-shaped assembly), §9 | ∥ | Prompt prefix is stable across turns for cache reuse; a long conversation compacts deterministically; compaction emits the signal 3.5 hooks. |
| 3.4 | **Capability registry that refuses instead of degrading** — if a model can't honor a request (tools, structured output, vision), the app says so loudly instead of mishandling it. | PLAN §3 (capability registry), §6 | ∥ | A request needing a capability the seat's model lacks fails loud with a reason, never silently degrades. |
| 3.5 | **Memory pre-compaction flush + new-chat nudge** *(the last memory write-triggers)* — sweep about-to-be-trimmed context for durable facts before compaction; soft consolidation nudge on new chat. | PLAN §9 "When memory gets written" | ⇢ 3.3 | A durable fact in soon-to-be-trimmed context is saved before it's lost; a new chat runs a consolidation pass. |

---

## Wave 4 — Skills & Agents  *(Tier A, large, self-contained)*

The last of the three "designed in full, not built" subsystems. 4.1–4.2 (skills) and 4.3
(agents) are largely ∥; 4.4 must precede locking any of their schemas.

| # | Item | Spec | Dep | Done when |
|---|---|---|---|---|
| 4.4 | **One-queue-model unification pass** — make cron jobs, subagent dispatch, and server results share ONE underlying queue model, not three overlapping ones. Do this FIRST, before 4.1–4.3 lock schemas. | PLAN §8 M4 ("before this locks its schemas"); Fable unified-work-queue | first | A single queue abstraction backs cron + agent dispatch + (future) server results; documented; schemas for 4.1–4.3 build on it. |
| 4.1 | **Skills system** — schema; `search_skills`; the skill-as-tool wrapper; the lint + on-screen approval flow; three-tier progressive disclosure (name/desc always, body on trigger, scripts/resources on use); seed-skills decision. | PLAN §10; tooling-and-skills.md | ⇢ 4.4 | The agent can search, load (progressively), and run a skill through the same gate chain as any tool; a new skill gets a lint + literal review before trust. |
| 4.2 | **Skills learning loop** — reflect-and-draft flywheel; per-profile approve-first↔autonomous toggle; teacher-escalation (a bigger model solves a twice-failed task AND writes a skill); curator rot-check re-tests existing skills. | PLAN §10 | ⇢ 4.1 | A finished task can yield a drafted skill (gated by the toggle); a twice-failed local task escalates + produces a reusable skill; the curator flags a broken skill. |
| 4.3 | **Agent-type registry** — declarative named personas (code reviewer, research explorer) with a bounded toolbelt (intersection with the main belt) and a seat binding; concurrent multi-agent dispatch with async result collection. | PLAN §4, §8 M4 | ⇢ 4.4, 3.1 | A named agent runs with only its allowed tools and its seat's model; several dispatch concurrently and their results come back async. |
| 4.5 | **Capability Packs** — the manifest + loader: a single installable bundle of skill + tool config + agent persona + cron template. | PLAN §4, §8 M7 | ⇢ 4.1, 4.3 | Installing a pack registers its skill/tools/agent/cron atomically, usable by a non-technical user without hand-editing config. |

---

## Wave 5 — The from-scratch flagships  *(Tier B — DESIGN PASS FIRST, then build)*

PLAN §6 describes these at the architecture level; none has an executable-grade spec. **Each
item's first sub-task is a design pass** landed as a `docs/plans/…md`, then build against it.
These are large; treat each as its own mini-milestone. 5.1–5.4 are ∥ (independent stacks).

| # | Item | Spec seed | Notes |
|---|---|---|---|
| 5.1 | **M5 — Computer use (cross-platform)** — native-app accessibility trees, OS-level click/keystroke synthesis, screenshot loop, per-OS permission flows (macOS accessibility, Windows UAC, Linux portals); guard-wrap screen/clipboard as untrusted; account for screenshots in the prompt budget; a distinct approval UX for irreversible on-screen actions ("this would click Send"). | PLAN §6, §8 M5 | The flagship differentiator. The shell-command approval flow does NOT generalize to "which pixel, reversible?" for free — that's its own design pass. |
| 5.2 | **M6 — Voice** — on-device STT/TTS by default, streaming playback, barge-in as a real latency requirement, the audio-specific privacy check (withhold sensitive audio from cloud TTS without confirm). Ships as a settings toggle, not an architecture fork. | PLAN §6, §8 M6 | Local-first polarity (opposite of cloud-first voice). |
| 5.3 | **M8 — Local-model lifecycle + onboarding** — hardware detection, a curated downloadable model catalog sized to the detected hardware, download/verify, seat assignment as first-run setup. Wire the visual-only Onboarding screen to this. | PLAN §6, §8 M8 | This is "local-first made real." Pairs with 3.1 (seats). |
| 5.4 | **M7 — Per-profile isolation + OS sandbox** — per-profile email/calendar/tasks; wire memory/seat/permission defaults to profile activation; replace the v1 no-op sandbox passthrough with real OS-level enforcement; server-flavored default permission seeding (server-track prep). Wire the visual-only Email screen. | PLAN §8 M7 | The OS-sandbox enforcement is the security-critical half — re-check permission output against it once it lands (PLAN §12 item 4). |

---

## Wave 6 — Server companion  *(Tier A spec, large; starts once M4 lands)*

Fully designed (PLAN §5, `server-companion.md`) — nothing left to *decide*, a lot to build.
Ordered: 6.1 is the hard prerequisite (nothing is "same trust tier as local" until it
exists); the rest layer on. This whole wave is the "twin, not daemon" second body.

| # | Item | Spec | Dep |
|---|---|---|---|
| 6.1 | **Pairing + mutual auth + always-on encryption** — product-owned, network-independent (Tailscale/LAN = transport only); one-time code/QR/token, revocable. | PLAN §5, server-companion.md "Connection security" | first |
| 6.2 | **Per-profile opt-in sync** — only opted-in profiles send cron defs + needed context to the server. | PLAN §5 "Per-profile opt-in" | ⇢ 6.1 |
| 6.3 | **The baton protocol** — heartbeat reuse, claim-ledger, single-active-writer handoff, per-project-lock takeover (multi-device). | PLAN §2, §5; server-companion.md "baton" | ⇢ 6.1 |
| 6.4 | **Shutdown protocol** — clean/unclean detection; on unclean exit, surface the conflict, never auto-merge. | PLAN §5 "Shutdown protocol" | ⇢ 6.3 |
| 6.5 | **Result queue / outbox** — durable server-side outbox, ack'd drain, `HEARTBEAT_OK`/`[SILENT]` suppression sentinel, "away for a while" rollup. | PLAN §5 "Result queue"; Fable sentinel | ⇢ 6.1 |
| 6.6 | **Event-journal replay over the sync channel** — the durable sequenced journal applied to app↔server catch-up (journal itself stays local per §5). | PLAN §5 "Event history"; Fable journal | ⇢ 6.3 |
| 6.7 | **`delegate target: local\|server\|auto`** — dispatch a specialist to whichever body can run its tools. | PLAN §5; extends 4.3 | ⇢ 4.3, 6.3 |
| 6.8 | **Server-hosted always-on skills** — a skill that lives on the server permanently ("watch this inbox nightly"), approved once locally. | PLAN §5; extends 4.1 | ⇢ 4.1, 6.3 |
| 6.9 | **Shared working directory + file explorer + offline pinning** — the custom "send what changed when the lock moves" file-sync engine; in-app explorer panel; "download locally" pin. | PLAN §5 "File sync"; server-companion.md "Sync model" | ⇢ 6.3 |

---

## Wave 7 — Polish → beta  *(Tier B for signing/distribution; Tier A for the rest)*

| # | Item | Spec |
|---|---|---|
| 7.1 | **Model bundling into the packaged app** — bundle the classifier models + the embedder + the ONNX Runtime dylib into the shipped `tauri build` (today they load from `~/Documents/…` in dev). | ROADMAP "Accepted quirks"; PLAN §9 |
| 7.2 | **Native-OS-citizen polish** — tray, menu bar, global hotkeys, OS notification-center integration. | PLAN §8 M9 |
| 7.3 | **Auto-update, code signing, distribution** *(design pass first — platform specifics)*. | PLAN §8 M9 |
| 7.4 | **Windows depth** — PowerShell/cmd paths, Windows service equivalents; smoke-test on Windows continuously, not discovered late. | PLAN §6, §8 M9 |
| 7.5 | **M10 — beta release**. | PLAN §8 M10 |

---

## Dependency graph (the short version)

```
Wave 1 (finish partials) ─┐
Wave 2 (tools + spine)  ──┼─→ Wave 3 (M4 model mgr) ─→ Wave 4 (skills & agents) ─→ Wave 6 (server) ─→ Wave 7 (polish → beta)
                          │            │                        │
                          │            └─ 3.3 compaction ─→ 3.5 memory flush
                          └─ 2.5 durability ⇢ 4.4 one-queue
Wave 5 (flagships: M5/M6/M7/M8) ── design-pass-first, independent stacks, can run alongside 3–4 once their design lands
```

Waves 1–2 can start immediately and in parallel. Wave 5's *design passes* can run early
(they gate nothing); their *builds* are large and independent. Wave 6 is the one hard
"after M4" gate. Everything funnels to Wave 7 → M10.

---

## Scope honesty (read before firing)

- This backlog is **the whole remaining product**, M4→M10 + the server twin. It is big by
  design — that's the directive ("everything spec'd"). Drain it wave by wave; don't try to
  hold it all in one fan-out.
- **Tier B items (Wave 5, 7.3) are not safe to build blind.** Their first deliverable is a
  design doc, reviewed, *then* code. An orchestrator that skips the design pass on computer-use
  or voice will burn tokens on wrong architecture.
- **The privacy filter, the danger-floor, guard-wrapping, and local-first are invariants**,
  not features to trade off for velocity. Every wave keeps `--no-default-features` building
  (rules-only / embedder-absent fallback) — local-first means the app never *requires* a model
  download or a server.
- **"Prove it works" is the NEXT directive**, after this backlog is drained. Keep per-item
  verification tight, but the end-to-end dogf/QA campaign is deliberately not in this document.
