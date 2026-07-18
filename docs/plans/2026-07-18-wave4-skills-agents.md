# Wave 4 — Skills & Agents: implementation plan

**Status:** design landed 2026-07-18 (from a 4-agent code-grounded mapping pass). Not yet
built. This is the executable plan for the Skills & Agents subsystem (BUILD-MANIFEST Wave 4:
items 4.1–4.5). Read it alongside PLAN.md §10 (skills) + §4 (agents/seats/packs) and
`tooling-and-skills.md`. Every "add" below cites where it plugs into the existing spine.

## The one idea

A skill and an agent are **not new execution paths** — they ride the machinery already
built. A *skill* is just a `Tool` in the existing `ToolRegistry` (so it inherits capability
filtering + the whole PreToolUse gate chain for free). An *agent* is `AgentLoop::process_message`
run with a restricted tool belt + a chosen seat. Deferred/background work (cron fires, agent
dispatch, reflect/curator/teacher, server results) all become rows in **one** `work_items`
queue rather than three hand-rolled mechanisms. So Wave 4 is mostly *composition of existing
parts* + a queue substrate — not greenfield subsystems.

## Build order (hard deps)

```
3.1 model seats ─┐                     (3.1 is a Wave-3 dep for 4.3's seat binding)
                 ▼
4.4 one-queue-model ──► 4.1 skills ──► 4.2 learning loop
        │                   │      ╲
        │                   │       ╲──► 4.5 capability packs (⇢ 4.1 + 4.3)
        └──────► 4.3 agent registry ─────┘  (4.3 ⇢ 4.4 + 3.1)
```

- **4.4 first** — it settles the schemas 4.1/4.3/4.2 build their work-payloads against, and
  it *also* subsumes the deferred **2.5 durability journal** (the `work_items` row IS that
  journal — idempotency key + row-before-side-effect + boot reconcile). Do not build a
  fourth deferred-work construct.
- **3.1 model seats** is an upstream dep for 4.3 (persona → seat) and 4.2's teacher (bigger
  seat). If 3.1 isn't built when 4.3 starts, stub `resolve_seat` to `inherit` (caller's
  provider/model) so 4.3 can proceed and 3.1 drops in later.

## Cross-cutting invariants (hold for every item)

1. **Same gate chain, no exceptions.** Every tool call a skill/agent/scheduled-run makes
   passes the unchanged `[PrivacyFilter, Sandbox, ProtectedPath, Permission, FirstUse]` chain
   + the `ApprovalLedger`. A skill/agent can never exceed its profile's existing permissions
   (PLAN §10 "why autonomous is still safe"). Autonomy toggles only the *human review of the
   playbook text*, never a gate. A `Dangerous` call inside a sub-agent still can't earn a
   standing grant (invariant #8).
2. **Bounded toolbelt is an INTERSECTION, never a widening.** A persona's effective belt =
   `allowlist ∩ available_tools(env)`. A tool named in the allowlist but absent/ungranted in
   the parent registry yields *nothing*. Test: a persona listing `write_file` on a body/
   profile that lacks it still cannot see or call it.
3. **Untrusted content guard-wrapped at both ends.** A skill body / a pack's skill bodies /
   an agent persona's system-prompt / a sub-agent's returned result are all agent- or
   third-party-authored text → `guard_wrap`/`neutralize_untrusted` when they enter model
   context (PLAN §10 "skills treated as untrusted text").
4. **Privacy routing is hard, not default.** Any local-model background work (reflect,
   teacher, curator) that mines a possibly-private transcript resolves a `is_local() &&
   is_private()` provider ONLY (the `memory_flush.rs::LocalModelExtractor` predicate) and
   `enforce_local_routing` fails loud rather than escalating a private task to cloud.
5. **Approve-first is the secure default**; autonomous is per-profile opt-in only. Manual
   authoring always available.

---

## 4.4 — one-queue-model unification (THE FOUNDATION)

**Today (five separate deferred-work constructs, none unified):** cron (`cron_jobs` table,
persisted, **no runner** — `tools/cron.rs` doc says the runner IS 4.4); agent dispatch (not
built; designed `tooling-and-skills.md:144`); server results (not built; `server-companion.md`
outbox); the headless `ApprovalQueue`/`QueueingPrompter` (built, **unwired** — 4.4 is its
first caller); the durability journal (2.5, deferred *to* 4.4).

### Storage — PROFILE migration v8 (bump `PROFILE_SCHEMA_VERSION` 7→8)

Per-profile (isolation wall, same as `cron_jobs`). Dual-define the CREATE in
`PROFILE_SCHEMA_SQL` + the v8 migration (the tool_audit/tool_rules convention).

```sql
CREATE TABLE work_items (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,              -- 'cron' | 'agent_dispatch' | 'server_result'
  state TEXT NOT NULL,             -- 'queued'|'running'|'done'|'failed'|'parked'|'cancelled'
  source_ref TEXT,                 -- cron_jobs.id | parent conv id | server event id
  input_json TEXT NOT NULL,        -- opaque typed WorkInput envelope (consumer-defined)
  result_json TEXT, error TEXT,
  scheduled_at INTEGER,            -- fire-time; NULL = run ASAP
  claim_key TEXT,                  -- '(cron_id, scheduled_at)' exactly-once dedupe
  idempotency_key TEXT,            -- 2.5 durability guard
  attempts INTEGER NOT NULL DEFAULT 0,
  target_conversation_id TEXT,
  created_at INTEGER NOT NULL, started_at INTEGER, finished_at INTEGER
);
CREATE UNIQUE INDEX idx_work_items_claim ON work_items(claim_key) WHERE claim_key IS NOT NULL;
CREATE INDEX idx_work_items_state_sched ON work_items(state, scheduled_at);
```

Also add `execution_location TEXT NOT NULL DEFAULT 'local'` to `cron_jobs` in the SAME
migration (avoids a re-migration when Wave 6 adds Local/Server/Fallback).

### Core types — new `src-tauri/src/queue/mod.rs`

- `enum WorkKind { Cron, AgentDispatch, ServerResult }` (extensible; str serde).
- `enum WorkState { Queued, Running, Done, Failed, Parked, Cancelled }` + `is_terminal()` +
  **`can_transition_to()`** (the checked lifecycle state machine — the key unit-tested logic:
  Queued→Running|Cancelled; Running→Done|Failed|Parked; Parked→Queued|Cancelled; terminals
  don't transition).
- `struct WorkItem { .. }` mirroring the row.
- `enum WorkInput { Prompt { text, binding, seat_or_provider, model }, ServerPayload { .. } }`
  — the envelope 4.1/4.2/4.3 build payloads into (serialized to `input_json`). *Keep the
  substrate consumer-agnostic: store `input_json` as an opaque String; consumers own their
  payload shape.*
- `enum WorkResult { AssistantText(String), Applied, Notification { .. } }`.
- `trait WorkExecutor { async fn execute(&self, item, sink: &dyn ResultSink) -> Result<WorkResult>; fn kind() -> WorkKind }`
  — cron & dispatch executors wrap an `AgentLoop::process_message`; the server-result
  executor writes into local storage.
- `trait ResultSink { fn deliver(item_id, WorkResult); fn progress(..) }` — **the load-bearing
  decoupling**: `process_message` currently *requires* a Tauri `AppHandle` to emit
  `stream:*`/`memory:event`; a headless work-item run has none. One `ResultSink` impl over
  `AppHandle`, one (later) over the server outbox.

### Storage methods (`profile.rs`)

`insert_work_item`, `claim_next_due(now) -> Option<WorkItem>` (atomic
`UPDATE ... SET state='running' WHERE id=(SELECT id ... state='queued' AND (scheduled_at IS NULL OR scheduled_at<=?) ORDER BY scheduled_at LIMIT 1) RETURNING *`),
`finish_work_item(id, state, result/err)`, `terminalize_orphans()`, `list_work_items`.

### Scheduler (`queue/scheduler.rs`)

`WorkQueueRunner { storage, executors: HashMap<WorkKind, Arc<dyn WorkExecutor>>, sink }` on a
tokio interval: (a) materialize due cron fire-times into queued items (next-run from
`cron.rs::validate_cron`'s parse), (b) claim + dispatch (cron serial; agent_dispatch up to a
concurrency cap).

### Reuse (add little)

Execution rides `AgentLoop::process_message` → the same hook chain + budgets. Unattended runs
swap the interactive prompter for the already-built `QueueingPrompter` (`hooks/headless.rs`)
— 4.4 is its first real caller. **2.5 durability** = the `work_items` row (write it before the
executor's external effect; boot reconciles intent-without-effect). Crash recovery: add
`terminalize_orphans()` into `crash_recovery::run_boot_pass` (same per-profile transaction).

### Suggested self-contained first increment

The **substrate** — table + types + `can_transition_to` state machine + `claim_next_due`
(atomic, exactly-once via the unique `claim_key` index) + `terminalize_orphans` + tests —
lands independently of the scheduler/executor (which arrive with the first consumer + the
`ResultSink` refactor). This is the "4.4 first" foundation 4.1/4.3 build their payloads on.

---

## 4.1 — skills system

**Today:** `skills` is a bare stub `(id, name, content, created_at)` (`schema.rs:96`); the
spine to hang skills on is fully built.

### Storage — GLOBAL migration v5 (bump `GLOBAL_SCHEMA_VERSION` 4→5)

`ALTER TABLE skills ADD COLUMN` (per `tooling-and-skills.md:119`): `description TEXT NOT NULL
DEFAULT ''`, `capabilities_required TEXT NOT NULL DEFAULT '[]'`, `approval_status TEXT NOT
NULL DEFAULT 'pending'`, `path TEXT NOT NULL DEFAULT ''`, `version TEXT NOT NULL DEFAULT
'0.1.0'`, `embedding BLOB`. Widen `global.rs::Skill` + `enum SkillApproval{Pending,Approved,
Rejected}`; add `list_approved_skills`, `set_approval_status`, `upsert_skill_embedding`,
`search_skills_hybrid(query, query_vec, limit)` (approved-only) — direct analogues of the
memory-fact accessors.

### `tools/skills.rs`

- `SearchSkillsTool` (`search_skills`, **Safe** → pre-trusted like `recall_memory`): hybrid
  search over APPROVED skills; returns Tier-1 metadata (+Tier-2 body on match, guard-wrapped).
- `SkillTool` (`skill:<name>` — namespaced to avoid native-tool collision): `requires()` =
  the skill's `capabilities_required` (so `available()`'s `requires() ⊆ env` auto-hides a
  `ComputerUse` skill on headless); `risk()` derived (v1 prompt/resource load = Safe;
  script-carrying = Write/External/Dangerous once execution lands); `run()` loads body + reads
  `path` resources (Tier 3). *v1 returns them as context; script exec is deferred — reuse the
  `tools/exec.rs` Seatbelt spawner with a Capability allowlist.*
- `SaveSkillTool` (`save_skill`, Write): lint → approval-gated (reuses `ApprovalRequest`/
  `ApprovalPrompter`; `approval_status='approved'` is the trust boundary).

### Progressive disclosure (3-tier)

name/desc always in the catalog (Tier 1) → body on trigger (Tier 2) → scripts/resources on use
(Tier 3). A skill is JUST a Tool, so registry capability-filtering + the gate chain + the
RiskClass→policy derivation all apply with near-zero new spine.

---

## 4.3 — agent-type registry

**Today:** greenfield (no `AgentType`, no `delegate`, no seat code).

### Storage — `agent_types` in GLOBAL (with 4.1's v5, or its own bump)

`agent_types(id, name, description, system_prompt, tools_allowlist TEXT '[]', seat TEXT,
trigger_examples, created_at)`. Ship 3–5 built-in personas (code-reviewer, research-explorer)
whose `tools_allowlist` names tools that ALREADY exist (`read_file`/`search_files`/
`recall_memory`/`session_search`) so the intersection is non-empty on day one.

### The build

- `AgentType`/`Seat` types + CRUD.
- **Bounded toolbelt = intersection** (invariant #2). Decide the sharing shape (open
  question): either `ToolRegistry` over `Arc<dyn Tool>` for a real intersected sub-registry
  (cleaner, wider blast radius) OR an `ExecCtx.tool_allowlist` filter enforced at catalog +
  dispatch (less invasive, must be enforced at *both* sites or it leaks). Land the security
  tests here.
- **Seat binding** (needs 3.1): `resolve_seat(seat, target) -> (provider_id, model)` supplies
  the pair `process_message` already takes; the per-turn privacy gate + `enforce_local_routing`
  run unchanged (a cloud seat still can't exfiltrate must-stay-local content). `inherit` =
  caller's provider/model.
- **Concurrent dispatch** (the prime 4.4 consumer): a `delegate` tool enqueues a work item per
  agent; the parent turn ends **once all are dispatched, not completed**; results drain async
  via 4.4's `ResultSink`. Each sub-agent reuses `AgentLoop` (its own `stream_lock`/`RunState`).

---

## 4.2 — skills learning loop + 4.5 — capability packs

**4.2 is a near-mechanical copy of `agent/memory_flush.rs`** (which already established the
testable-extractor-seam + detached best-effort spawn + guard-wrapped input + at-most-once +
sensitivity-routing pattern for memory). Reuse that shape:

- **Reflect-and-draft** → new `agent/skill_reflect.rs`: `trait SkillDrafter { available();
  draft(transcript, tool_trace) -> Option<DraftedSkill> }` + `LocalModelDrafter` (same
  `is_local()&&is_private()` predicate). `worth_drafting(trace)` heuristic + a deterministic
  lint pre-check. On draft: branch on the toggle — approve_first ⇒ insert `pending` (surfaced
  in a "what I taught myself" feed); autonomous ⇒ enqueue a curator self-test, insert
  `approved` on pass.
- **Per-profile toggle** → PROFILE `skill_settings` single-row table (mirror `memory_settings`
  exactly): `creation_mode TEXT DEFAULT 'approve_first'`; `skill_autonomous_enabled(storage,
  profile)` helper mirroring `semantic_search_enabled`.
- **Teacher-escalation** → a PROFILE `task_attempts` ledger `(task_signature, fail_count,
  last_failed_at, last_conversation_id)`; on the 2nd failure enqueue a `TeacherSolve` work item
  (bigger seat solves it + drafts a skill). Needs 3.1 seats + 4.3 dispatch. **Last in 4.2.**
- **Curator rot-check** → new `agent/skill_curator.rs`: `run_curator(skill_id)` re-runs the
  skill's declared self-test through the SAME gate chain; updates a `health`/`last_verified_at`
  column. A recurring 4.4 work item.

**4.5 capability packs** → new `src-tauri/src/packs/`: `PackManifest` (toml:
`name/version/author/requires=[Capability]` + `skills/`, `agent-types/`, `cron-templates/`,
`mcp.toml`); `install_pack(root)` = a SINGLE storage transaction inserting skills + agent_types
+ cron + tool_rules + a `capability_packs` registry row (rollback on any failure);
`uninstall_pack(id)` deletes by `origin_pack_id`. Pack `tool_rules` are subject to the grant
matrix (a pack can't silently self-install an External/Dangerous standing rule). Marketplace
fetch is explicitly later (out of 4.5 core).

### What each area REQUIRES from 4.4 (the consumer contract that made 4.4 designable)

- **4.3 agent dispatch:** work-item lifecycle ending at `Dispatched`; input general enough for
  subagent+cron (`{source, agent_type_id, prompt, resolved endpoint, target, tools_allowlist
  snapshot, profile, parent_conversation_id, tolerates_async}`); a typed result channel (one
  mechanism shared with 4.2); concurrency-bounded dispatch + claim/ack.
- **4.2:** three new work-kinds — `ReflectDraft{conv, profile}` (one-shot deferred),
  `CuratorRetest{skill_id}` (recurring), `TeacherSolve{task_sig, conv, seat}` (one-shot);
  typed results (curator needs pass/fail back; teacher needs answer + drafted skill back).
- **4.5:** register a pack's cron-templates THROUGH 4.4 (never a second scheduler).

---

## Open questions for Lukas (design decisions not settled by the specs)

1. **Sub-agent conversation scope** — does a delegated sub-run get its own (hidden child)
   conversation row / scratch DB scope, or write into the parent? Affects result attribution +
   transcript rendering.
2. **Registry sharing shape** (4.3) — `Arc<dyn Tool>` sub-registry vs `ExecCtx` allowlist
   filter. Security-critical; pick in the 4.3 build.
3. **`delegate` RiskClass + a per-parent-turn dispatch ceiling** (analogous to
   `PER_TURN_CALL_CEILING`) to bound unattended fan-out.
4. **Teacher task-signature** — normalized prompt hash (brittle) vs intent embedding (needs the
   embedder). And when the "twice" counter resets.
5. **Reflect trigger point** — after every finished task (heuristic-gated) vs an explicit
   signal ("task finished" is as unreliable as "conversation ended", PLAN §9's own warning).
6. **Autonomous skill provisional-until-tested?** PLAN §10 implies a synchronous does-it-work
   gate before trust, not pure fire-and-forget.
7. **Pack `tool_rules` on install — applied (with floors) or proposed into the approval feed?**
8. **Seed skills** — which handful ship pre-loaded, and the `source` label for built-ins.
9. **Skill self-test location** — a `tests:` frontmatter block, a bundled script, or a
   model-graded rubric (the curator needs a deterministic-enough success check).

---

*Provenance: this plan synthesizes a 4-agent code-grounded mapping pass (2026-07-18) over
PLAN §10/§4, `tooling-and-skills.md`, `server-companion.md`, and the existing tools/hooks/
storage/agent-loop spine. Grounded in real `file:symbol` references throughout.*
