# Lost Harness — Roadmap & Stage Tracker

**Purpose:** the one file that answers *"what stage are we at, what's left, what's
next."* When Lukas asks for a status, answer from here. Keep it honest: update the
**Stage** line and the checklists every time a work round lands, and move items
between sections rather than duplicating them.

> **ACTIVE DIRECTIVE (2026-07-17): build everything spec'd, then prove it.** The
> ordered build backlog for that is **[`BUILD-MANIFEST.md`](BUILD-MANIFEST.md)** —
> wave-by-wave, parallelizable, tiered — meant for a multi-agent (ultracode) run.
> This ROADMAP stays the human-facing status board; the manifest is the machine-facing
> work queue. Keep both current as waves land.

Where things live: design decisions → [`PLAN.md`](PLAN.md) (source of truth);
implementation detail → [`codebase/README.md`](codebase/README.md); the executable
specs for tool-system work → [`tool-system-build-plan.md`](tool-system-build-plan.md);
current-session context and gotchas → [`../HANDOFF.md`](../HANDOFF.md). This file is
the status board sitting on top of all of them.

---

## Stage

> **As of 2026-07-21: FULL PROJECT REVIEW (4-agent audit + doc reconciliation). No regressions; all gates green (542 tests / clippy 0 errors / `--no-default-features` clean / frontend build+check clean, tree clean at `ca54251`).** The audit confirmed the Wave 1–5 work substantially matches its claims — M7 Tier-P fs confinement is the strongest-evidenced area — and surfaced these NEW findings, now tracked on the milestone board + "What's left": **(1) HIGH:** the promised end-to-end "a cloud seat can't defeat RouteLocal" test was never written (invariant holds by construction only). **(2) MED-HIGH:** the M7 Tier-K network ceiling is live code but UNREACHABLE — no IPC/UI writes `sandbox_config`, every profile takes the unconfigured branch. **(3) MED:** M6 `stt_egress` deviates from its design (content-classifies a transcript; the real pre-transcription decision must be content-free) — fix before wiring native STT. **(4) MED:** untested security-reviewed paths — delegated-helper guard-wrap-on-re-entry, work-runner 5-min deadline + panic supervisor. **(5) MED:** cron "never egresses" has a narrow caveat — a Session-scoped External grant made interactively could be replayed by a byte-identical headless cron call (fingerprint has no session discriminator). **(6)** Two items were stale-blocked in these docs but are actually UNBLOCKED: reroute UX (2.3, dep 3.1 shipped) and the budget governor (3.2 tail, dep cost-capture shipped). Also: `open_profile` still accepts whitespace-padded confusable profile names (flagged 2026-07-18, still open); the `docs/codebase/` guide was fully regenerated this session (it predated Waves 1–5).
>
> **As of 2026-07-18: WAVES 1–4 COMPLETE; Wave 5 flagship SECURITY CORES landing.** Wave 1 + Wave 2
> (ready work) + Wave 3 (3.1 seats, 3.2 ledger+cost, 3.3 compaction, 3.5 flush) + **all of Wave 4
> (4.1 skills, 4.2 learning loop, 4.3 agents, 4.4 one-queue/cron, 4.5 packs)** are
> landed, adversarially reviewed, and committed. **Wave 5 in progress:** M8 (hardware probe +
> curated catalog + verified download), M6 audio-egress gate, M5 reversibility classifier, and
> now **M7 Tier-P per-profile filesystem confinement** are built (security cores; native
> backends remain on-target). **Remaining: Wave 5 native backends (M5/M6/M7-Tier-K, target
> machine) + Waves 6–7** (server twin + polish). Plus a Connection-Mutex soundness fix. 528 tests.
> **Latest (2026-07-18): M7 Tier-P (5.4, Slice 1) COMPLETE.** Every profile now gets a
> physically-separate workspace subtree `workspace/<profile>`, resolved at call time from
> `ExecCtx.profile`: all 6 fs tools + `shell_exec` (its cwd AND tmp scratch, so both macOS
> Seatbelt subpaths are per-profile) + `ProtectedPathHook`'s resolved-path signal re-root per
> profile. `profile_workspace_path` mirrors `Storage::open_profile`'s denylist byte-for-byte (no
> trim). A one-time legacy-workspace migration moves pre-Tier-P pooled data into the default
> profile — **files-only, never a directory**, a structural invariant that makes profile
> mis-attribution impossible (a dir is always ambiguous → left in place + logged).
> **Adversarially reviewed across 3 workflows + a lean skeptic (~14 agents): 9 confirmed findings
> ALL fixed** — 5 HIGH (trim divergence; `shell_exec` workspace bypass; `shell_exec` shared-tmp
> exfil; migration sweeping live trees; `known_profiles` DB-desync), 3 MEDIUM (missing migration;
> profile-named-file ENOTDIR; dangling-symlink clobber), 1 LOW (marker spoof) — several *dissolved*
> by the files-only pivot. **M7 Tier-K Slice 2 (macOS) also landed:** the dead per-profile
> `SandboxConfig` is now live as a **network CEILING on `shell_exec`** — a locked-down profile
> (no localhost, no allowed domains) can't use shell network even when the call asks; unconfigured
> profiles keep today's behavior; fails safe (corrupt/unreadable config → denied); the Seatbelt
> confinement stays always-on (never an unsandboxed run). Per-profile `sandbox_config` table
> (PROFILE v10). Adversarially reviewed (2 agents, ceiling-bypass + storage/migration): clean.
> Tier-K remainder = Linux Landlock/seccomp + Windows backends (Slices 3–4), finer network
> precision — all on-target. **M5 Slice 0 (multimodal wire format) also landed:** `models/content.rs`
> — `ImageBlock` + `assemble_content` emits an OpenAI image-array to a vision seat and a placeholder
> to a text-only seat (image bytes never reach an endpoint that can't read them); plain text turns
> unchanged. Reviewed clean. The screenshot SOURCE (capture tool + native backend) is on-target
> Slice 1. **538 tests.**
> **Latest (2026-07-18):** **Wave 3.5 COMPLETE** — both memory write-triggers:
> the pre-compaction flush (`f89536e`, trigger #2: a LOCAL model sweeps
> about-to-be-trimmed turns) AND the new-chat consolidation nudge (`e0929f0`,
> trigger #3: on a new chat, sweep the prior conversation). Sensitivity-routed,
> async/off the stream lock, at-most-once (shared high-water) + content-deduped.
> **WAVE 4 KICKED OFF (2026-07-18):** a code-grounded **Wave 4 implementation
> plan** landed (`docs/plans/2026-07-18-wave4-skills-agents.md`, from a 4-agent
> mapping pass — build order, invariants, concrete shapes, the consumer contract
> that de-speculates 4.4, + 9 open questions for Lukas), and the **4.4
> one-queue-model substrate** (`a0b00ee`): a new `queue` module (`WorkKind`, a
> checked `WorkState` lifecycle, `WorkItem`) + a per-profile `work_items` table
> (PROFILE v7→v8) with atomic claim (exactly-once via `claim_key`),
> lifecycle-guarded finish, and crash `terminalize` (also settles the 2.5
> durability journal). The scheduler + `WorkExecutor`/`ResultSink` traits arrive
> with the first consumer. **Wave 4.1 skills core** (`814ff50`, GLOBAL v4→v5):
> the `skills` stub grows into a real row; `search_skills` (Safe, approved-only,
> guard-wrapped) + `save_skill` (**Dangerous** — an always-shown review immune to
> accept_edits, per the cron precedent, caught by the review); a skill's body
> re-gates whatever it drives. Plus the **Settings → Skills** management pane
> (`4ea9dd3`): `list_skills`/`set_skill_approval`/`delete_skill` IPC + a review
> surface (status badges, capability chips, an auto-escaped body expander,
> approve/reject, two-click delete) — the review gate for the 4.2 draft-first
> loop, browser-verified. **WAVE 4.2 COMPLETE** — the **draft-first learning
> loop** (`e5537dd`, `agent/skill_reflect.rs`): on a new chat a LOCAL model
> reflects the prior conversation into a skill draft, ALWAYS saved `Pending`
> (inert until the human approves it — automation may propose, never mint), gated
> by an opt-in `skill_reflect_enabled` toggle (default OFF, "Propose skills
> automatically" in the Skills pane). Mirrors `memory_flush` (local-only,
> guard-wrapped, at-most-once). Adversarially reviewed (4-lens/12-agent
> find→verify): fixed a HIGH UTF-8 panic (`strip_label` byte-sliced the `&str` →
> crashed on any non-English model output), a MEDIUM concurrent-`global.db`
> race (flush + reflect now run sequentially in one task), + 2 LOW parser bugs;
> plus conservative walled-profile + empty-prior guards. (Surfaced a PRE-EXISTING
> soundness debt — background tasks touch the `!Sync` connection concurrently with
> the main loop — now **FIXED** in `ff64b3a`: the `rusqlite::Connection` lives
> behind a `parking_lot::Mutex` in `GlobalDb`/`ProfileDb`, all 4 `unsafe impl
> Send+Sync` removed, the types genuinely thread-safe.)
> **WAVE 3.1 COMPLETE — model seats** (`ec6c852`, PROFILE v8→v9): after Lukas
> settled the spec-absent design (**seats are user-definable strings, per-profile,
> unbound→inherit**), a per-profile `seat_bindings` table + `models::resolve_seat`
> (empty/`inherit`/unbound/dangling-provider → the caller's own model, so a seat
> is a PREFERENCE the privacy gate still overrides) + a "Seats" pane in
> Settings→Models. **This UNBLOCKS Wave 4.3** (agent registry: persona→seat).
> **WAVE 4.3a — agent-type registry storage** (`ab4b2fc`, GLOBAL v5→v6): the
> `agent_types` table + `AgentType`/`AgentTypeApproval` + CRUD + two seeded
> built-ins (code-reviewer, research-explorer) whose toolbelts name only existing
> tools, mirroring the skills model. Pure data — dispatch (4.3b/c) consumes it.
> **WAVE 4.3b — bounded toolbelt intersection** (`25e8da6`): `ToolRegistry` now
> holds `Arc<dyn Tool>` (register sites unchanged) + `restricted_to(allowlist)` —
> a persona's belt is the sub-registry's CONTENTS (`allowed ∩ registered ∩
> env`), so an out-of-belt tool is physically absent (not listable OR
> lookupable) — a structural security boundary, 4 security tests. **WAVE 4.3 CORE
> COMPLETE:** 4.3c(1) restricted sub-dispatcher (`2595967`), 4.3c(2) ResultSink
> AppHandle-decoupling (`2c43832`), + the **delegate + dispatch runtime**
> (`e1e1f8c`): `delegate` (Dangerous) enqueues an AgentDispatch work_item; a
> background `WorkQueueRunner` (4-way semaphore) drains it + runs `run_subagent`
> (fresh AgentLoop w/ restricted belt + seat model + HeadlessSink) + posts the
> outcome into the parent conversation. **Adversarially reviewed (18-agent
> find→verify); all 11 confirmed findings fixed** — incl. 3 HIGH: helper-result
> injection (guard-wrapped when re-fed to the main agent), Private-parent→cloud
> downgrade (helper inherits the turn binding), cross-agent grant leak
> (sub-dispatcher runs headless). **Done-when met.** Plus **4.3d** — the Settings
> → Agent types management UI (`cbc774c`, approve/reject/delete personas). **WAVE
> 4.3 FULLY COMPLETE.** **WAVE 4.5 COMPLETE** — Capability Packs (`fdda26c`): a
> portable `Pack` JSON (skills + agent-types + cron templates) + `install_pack`
> (everything lands INERT — skills/agents Pending, cron disabled, so a pack adds
> capabilities to review, never arms one) + `export_pack` + an "Install a pack"
> UI. **WAVE 4.4 COMPLETE** — the cron runner (`4cf9976`): a real `cron_due`
> matcher (*, ranges, steps, names, @macros, dom/dow OR, Sunday 0/7), a headless
> full-belt `ToolDispatcher::headless()`, `AgentLoop::run_cron` (unattended =
> LOCAL-only + Private binding, never egresses), and the `WorkQueueRunner` now
> enqueues due cron jobs as Cron work_items (exactly-once/minute via a claim_key
> + last_run guard) + executes them. **⇒ WAVE 4 (Skills & Agents) FULLY COMPLETE
> — 4.1–4.5.** **453 → 498 tests.** **WAVE 5 DESIGN PASSES DONE** (`9d85299`,
> `docs/plans/2026-07-18-m{5,6,7,8}-*.md`) — the manifest-mandated design-first
> deliverable for all four flagships, each with a code-grounded skeptical review
> folded in (3 NEEDS-REVISION: the review caught wrong-architecture BEFORE code;
> m8 SOLID). **Remaining: the Wave 5 BUILDS** (large native — each revises against
> its review first; m5 needs a multimodal model client prereq; m8 needs a
> catalog-source decision), then **Wave 6 server** + **Wave 7 polish→beta** — the
> big native/networking half, multi-session, some gated on native APIs / external
> resources / Lukas decisions. **M8 model-lifecycle SUBSTANTIALLY BUILT** (Lukas:
> build all flagships, HF source): S1 hardware probe (`97c260b`), S2 curated
> catalog, S3 download + SHA-verify + resume — the verified-before-runnable
> invariant (`fa155b4`), the download IPC (`97ca856`), the Settings→Models catalog
> UI (`30ed0ea`). **453 → 510 tests.** Deferred: S4 sidecar (native binary),
> release-curation of the catalog sha256s. **The remaining flagships — M5
> computer-use, M6 voice, M7 OS-sandbox — are HEAVY NATIVE builds** (macOS
> accessibility/CGEvent/ScreenCaptureKit; Core Audio/whisper/TTS;
> seatbelt/namespaces/AppContainer) requiring the TARGET OS + native APIs to build
> AND verify — not meaningfully doable in a headless analysis context, and their
> designs flagged NEEDS-REVISION — **now ALL REVISED → build-ready** (`00e2745`):
> m5's semantic-locator-args reframe dissolves the fingerprint gap (no
> HookResult::Modify, covers() aligns), m6's cooperative CancellationToken +
> AudioEgressGate, m7's honest Tier-K/Tier-P + per-profile ProtectedPathHook
> re-root. Each grounded in the real dispatcher/gate/hooks with honest per-slice
> gates. Wave 6 server needs external infra; Wave 7 is release engineering. **⇒
> Everything buildable+verifiable in a headless env is DONE; the native flagship
> BUILDS follow their build-ready designs on the target machine.** (A hard session
> usage limit was also hit 2026-07-18, resets ~9pm PT.) **M6 audio-egress gate
> IMPLEMENTED** (`b91d784`, `audio/privacy.rs`): the ONE non-native security core
> of voice — `AudioEgressGate` re-vets cloud STT/TTS egress on the cumulative
> reply prefix (audio egress ≤ text egress; Private as-local-spoken-as-typed;
> Public still hits the floor), reusing the §7 gate, fully unit-tested (515
> tests). The SIBLING non-native cores exist but are multi-file SECURITY-CRITICAL
> integrations. **M5 reversibility core IMPLEMENTED** (`ae95d81`,
> `tools/computer_use.rs`): the security core — `ActionTarget` semantic locators
> + `reversibility()` (irreversible verb set) + `risk_class()` mapping onto the
> real matrix (Reversible→Safe, Consequential/Irreversible→External + covers_once
> floor). Additive, tested. **519 tests.** **Remaining flagship cores** (do fresh,
> un-capped, on-target): **M7 Tier-P** — a COHERENT fs+ProtectedPathHook change
> (all fs tools' fixed root → `profile_workspace(base, ctx.profile)` AND re-root
> the hook via `EventContext.profile`; half-done is a security REGRESSION since
> the hook guards base-relative paths the per-profile tools no longer write to —
> confirmed `protected_path.rs:86` holds a fixed `workspace_root`; also a
> behavior-change/product call: isolated-per-profile vs shared workspace). **M5
> live wiring** — the `OnScreenActionHook` + tool registration + native backend.
> **M6 native** — whisper/piper/AEC behind the `voice` feature. **Next fronts (per the
> plan):** 4.1's skill-as-Tool wrapper + hybrid search + UI; 4.3 agent registry
> (needs 3.1 seats); 4.2 (a near-copy of `memory_flush`); 4.5 packs; the 4.4
> scheduler+executor (needs the first consumer + the AppHandle-decoupling
> `ResultSink`). The 3.2 budget governor stays server-track.
> **Wave 2 core tools + queue (2026-07-17, `9008cfb`→`e5da77f`):** four more
> items landed — **cron management** (2.1: `list_cron_jobs` Safe +
> `manage_cron` Dangerous, profile-scoped, cron-string-validated), **`fetch_url`**
> (2.1: the FIRST External/egress tool — SSRF-guarded HTTP GET + readable-text
> extraction, every hop DNS-re-checked against a full internal-IP block-list incl.
> the 169.254 metadata endpoint + all IPv4-in-IPv6 embeddings; surfaces its
> destination for consent), the **headless approval queue** (2.4 / Q5:
> `QueueingPrompter` — park-and-queue + rule pre-authorization riding the Q8
> PolicySource; Dangerous never pre-authorized, External needs a
> destination-naming rule, resolved with PermissionHook's exact precedence so it
> can't be more permissive than attended), and **`ask_human`** (2.1: the single
> blocking "ask the user" tool — Safe/pre-trusted, `HumanPrompter` trait +
> `TauriHumanPrompter` + `AskHumanDialog.svelte`; unblocked without touching the
> stream lock, so no deadlock). All four adversarially reviewed (findings fixed:
> cron Write→Dangerous, fetch IPv6-embedding SSRF gaps, queue `**`/precedence
> bypasses; ask_human clean SHIP). 399 → **423 tests**. **Wave 2 remaining (all
> blocked on later waves):** `delegate` (2.1 — dep 4.3 agent registry, a real
> delegate dispatches a sub-agent); reroute UX (2.3 — dep 3.1); durability
> journal (2.5 — dep 4.4). **⇒ Wave 2's ready work is drained.**
> **Wave 3.3 — cache-shaped prompt assembly + context compaction (2026-07-17,
> `f543250`):** prompt assembly is now cache-shaped (curated summary → byte-stable
> system PREFIX via a deterministic `guard_wrap_stable`; volatile snippets moved
> into the current user turn at the tail) and the model-facing history is
> compacted to a char budget by a pure deterministic `compact_history` (leading
> prefix + pinned recent tail kept, oldest middle dropped WHOLE with a marker,
> the trimmed set returned as the Wave 3.5 signal via the `on_pre_compaction`
> seam) — the stored transcript is never touched. Ran a 3-approach design panel
> + a 5-lens find→verify review (multi-agent); fixed the confirmed defect that a
> deep tool loop could compact the user's question out (now pinned) + merged the
> snippet block into the current turn. **This unblocks Wave 3.5** (pre-compaction
> flush: swap the `on_pre_compaction` body to sweep the trimmed turns for durable
> facts). **Plus a HIGH privacy fix (`c026e33`):** the review surfaced a
> pre-existing leak — a plain Allow cloud turn replayed prior turns' persisted
> plaintext (incl. an earlier private/redacted turn) to cloud without re-vetting;
> now the Allow→cloud path is gated by the whole-history cloud-safe check (routes
> local if unsafe), with a per-conversation incremental cache so long benign
> cloud chats aren't forced local. 425 → **438 tests**.
> **Wave 3.2 — usage ledger booking side (2026-07-17, `7141d34`):** the
> per-profile `usage_events` cost ledger + booking landed (PROFILE schema v6→v7):
> every model call books one row; local=$0, cloud=unknown-flagged (never a
> guess — SUM skips NULLs, unknown count surfaced separately). Reviewed SHIP.
> 423 → **425 tests**. **Cost capture DONE (2026-07-18, `649f3fa`):** the ledger
> now records REAL cost — SSE `usage` parsing (`SseEvent::Usage`) +
> `stream_options.include_usage` (non-private endpoints only) + a `pricing.rs`
> table (known cloud models → $/Mtok); priced only when usage was reported AND
> the model is known, else `None` (never a guess). Review fixed a MEDIUM
> streaming regression (a malformed `usage` could drop co-located content —
> now parsed leniently). **Spend-so-far surface (`24cf6aa`):** `get_usage_summary`
> IPC + a Settings "Usage" section (total calls, known $ cost, unpriced count) —
> browser-verified. **Rest of 3.2 (budget governor) deferred:** a cost-cap
> that halts UNATTENDED spend needs an unattended-mode concept (server-track).
> **Other Wave 3 items
> (Tier-A ∥, not started):** model seats (3.1 — no consumer until the 4.3 agent
> registry), cache-shaped prompt assembly + compaction (3.3), capability registry
> that refuses (3.4 — native-tools has a valid fenced fallback, so no refusal
> trigger exists yet). Neither 3.1 nor 3.4 has a present consumer — building them
> now would be speculative infra; 3.3 (compaction) is the next substantive item.
> **Wave 2.2 — permission modes** (`5bf3c37`): a session-wide `SessionMode`
> (normal / plan / accept-edits) enforced by a `SessionModeHook` placed after the
> danger/protected-path floors and before `PermissionHook`, so it's *structurally*
> matrix-bounded — plan is read-only (denies risk > Safe), accept-edits
> auto-approves `Write` only (never `External`/`Dangerous`). Threaded through
> `send_message` → loop → `ExecCtx` → dispatcher, with a chat-header mode pill.
> Full-chain tests prove a mode can't widen Dangerous. 390 → **396 tests**. The
> `UserPromptSubmit` hook half of 2.2 is deferred (Q11 rates it structural /
> zero-coverage-gain). **Wave 2 still open:** remaining core tools (system_status,
> session_search, ask_human, headless browser, delegate, cron), reroute UX (2.3,
> dep 3.1), headless approval queue (2.4), durability journal (2.5, dep 4.4).
>
> **Wave 1 (2026-07-17): started subsystems finished** —
> Wave 1 (BUILD-MANIFEST.md) landed all its items: the **native-tool add-provider
> UI checkbox** (1.1 — everyday chat can now use the native transport), a
> **per-profile semantic-memory toggle** (1.2 — hard off switch for computing a
> meaning fingerprint; lazy embedder load), **curated-summary snapshot at turn 1**
> (1.3 — frozen per conversation for prompt-cache stability), the **inline
> "remembered" save event** (1.4), and **walled-profile memory DB routing** (1.5 —
> a walled profile's facts live in their own physically-separate DB, proven to
> survive toggling the wall back off). 385 → **389 tests**. (1.6, the cosmetic
> `gate.rs` §7 rename, stays deferred — low-value/high-churn.)
>
> **Prior state (HISTORICAL — 2026-07-17 snapshot, several claims below superseded by the entries above: skills ARE now built, seats/compaction/write-triggers/native-tool-UI all landed; only the budget governor from its "next fronts" list is still open):**
> Classifier round **fully closed** (INT8 ONNX ensemble + "why" sidebar +
> per-profile thresholds + redact-and-send). **Memory is now HYBRID** — the
> meaning lane shipped: a stock bge-small-en-v1.5 INT8 embedder (same ONNX
> runtime/install/fallback as the classifier) powers sqlite-vec semantic search
> fused with the keyword lane by rank; the private vector index is physically
> separate (cloud turns never query it); gates calibrated on the live model;
> boot-time backfill embeds old facts. **Native tool-use (Q1) is DONE + PROVEN
> LIVE** — a per-endpoint `supports_native_tools` flag picks the native structured
> `tool_calls` transport (fenced dialect stays the fallback), both normalizing to
> one transport-blind pipeline; fingerprint parity across transports is tested;
> and it's **verified end-to-end against LM Studio qwen3.6-35b-a3b** (2026-07-17,
> three clean runs — the model chose the tool, streamed native `tool_calls`, our
> parser reconstructed the call). **Next engineering fronts:** memory's
> curated-summary snapshot-at-turn-1, write-trigger backstops, walled-profile DB
> routing; then the rest of M4 (model seats, usage ledger, budget governor). Also
> ripe: mark real endpoints `supports_native_tools` in the UI (add-provider
> checkbox) so day-to-day chat uses the native path. **Skills** remains fully
> designed, zero code.

**Health check (run this before believing anything below; update expected numbers when they change):**

```bash
cd /Users/hayai/Desktop/lost-harness-product/src-tauri && cargo test --lib   # expect: 542 passed, 0 failed
cd /Users/hayai/Desktop/lost-harness-product && npm run build               # expect: clean
cd /Users/hayai/Desktop/lost-harness-product && npm run check               # expect: 0 errors (1 pre-existing tsconfig warning is known noise)
# The trained classifier is behind a default-on feature. To run its ONNX parity test:
cd .../src-tauri && LHP_CLASSIFIER_MODELS_DIR="$HOME/Documents/Lost-Harness/models/classifier" cargo test --lib parity_tests
# The rules-only fallback (no native ONNX Runtime dep) must also build:
cd .../src-tauri && cargo build --lib --no-default-features
```

Optional env-gated live/model tests (not part of the 542; run manually):
```bash
# Memory embedder sanity + gate calibration on the live INT8 model:
LHP_EMBEDDER_MODELS_DIR="$HOME/Documents/Lost-Harness/models/embedder" cargo test --lib embedder::
# Native tool-use against a live endpoint (needs a native-capable server, auth off or token set):
LHP_NATIVE_ENDPOINT="http://127.0.0.1:1234/v1" LHP_NATIVE_MODEL="qwen/qwen3.6-35b-a3b" \
  cargo test --lib live_native_tool_call_roundtrip -- --nocapture
```

Last verified: 2026-07-21 (project-review session: **542 passed**, `--no-default-features`
builds clean, `cargo clippy --lib` 0 errors (117 warnings), frontend build + svelte-check
clean (1 known tsconfig warning), tree clean on `main` at `ca54251`).

---

## Milestone board

| Milestone | What it is | Status |
|---|---|---|
| **M0** — bootstrap | Tauri + Svelte + Tailwind + CI | ✅ **Done** |
| **M1** — vertical slice | message → classify → route → model → stream → save | ✅ **Done + verified** (contract tests at the real IPC boundary) |
| **M2** — UI shell | design system, profiles, command palette | 🟡 **Mostly done** — design-system port landed and wired for chat/sidebar/settings; profile switching works. Superseded components deleted + dev screen-switcher removed (2026-07-16). Remaining gaps: `CommandPalette.svelte` is ported but mounted nowhere; 7 screens are visual-only (see Loose ends). |
| **M3** — tool registry + spine | the whole security/tool foundation | ✅ **Done** (2026-07-16) — all 8 do-now items + approval spine + write/shell/MCP tools, every round adversarially reviewed. Exception: the durability trio's persisted-journal half is deliberately deferred to the first external-effect tool (see PLAN §8 / build plan Q3). |
| **M4** — model manager + skills/agents | native tool-use, seats, usage ledger, budget governor, cache-shaped prompts; skills & agents track | 🔵 **In progress** — Q8 (grant×risk matrix + persisted `tool_rules` + risk-badged dialog) done 2026-07-16. **Native tool-use (Q1) DONE + PROVEN LIVE 2026-07-17** (`d203a9a`): per-endpoint `supports_native_tools` flag, structured `tool_calls` transport + fenced fallback, one transport-blind pipeline, fingerprint parity tested, and verified end-to-end against LM Studio qwen3.6-35b-a3b (3 clean runs). **Usage ledger (3.2) booking side DONE 2026-07-17** (`7141d34`): per-profile `usage_events` cost ledger, every model call booked, local=$0/cloud=unknown-flagged. **Cache-shaped prompts + context compaction (3.3) DONE 2026-07-17** (`f543250`): byte-stable prefix + deterministic `compact_history` at the stream seam, emits the 3.5 pre-compaction signal (+ a HIGH cloud-history privacy fix `c026e33`). **Model seats (3.1) DONE 2026-07-18** (`ec6c852`, per-profile, user-definable strings, unbound→inherit). **Real cost capture DONE 2026-07-18** (`649f3fa`). **Wave 3.5 flush DONE.** **The whole skills & agents track (Wave 4.1–4.5) DONE 2026-07-18** — skills, learning loop, agent registry + `delegate` runtime, one-queue + cron runner, capability packs. **Still open in M4:** budget governor (3.2 tail — its SSE-cost-capture prerequisite is now met, so it's genuinely buildable; needs the unattended-mode tie-in), capability registry that refuses (3.4, no consumer yet), reroute auto-switch UX (2.3 — unblocked since 3.1 landed, pure frontend). ⚠ 2026-07-21 audit: the promised end-to-end "a cloud seat can't defeat RouteLocal" regression test was never written — invariant holds by construction through the unmodified gate; write the test. |
| **Memory system** | curated summary + searchable archive (hybrid FTS5 + sqlite-vec), profile wall, 3-bucket sensitivity routing | 🟢 **HYBRID + LIVE (meaning lane landed 2026-07-17, `bfb5721`).** Storage + IPC + Settings "Memory" tab + `recall_memory`/`remember` tools + endpoint-aware `allow_private_memory` + auto-injection + non-silent recall banner (all earlier), PLUS now: the **sqlite-vec meaning lane** — a stock **bge-small-en-v1.5 INT8 embedder** (`embedder.rs`, same ONNX runtime/install/fallback as the classifier; installed at `~/Documents/Lost-Harness/models/embedder/`) feeds hybrid keyword+semantic search fused by **Reciprocal Rank Fusion**; the **private vector index is a physically-separate table** (`memory_vectors_private`) so a cloud turn never queries it; distance gates **calibrated on the live model** (inject 0.38 / recall 0.48); **stopword-filtered** FTS so the injection relevance gate doesn't fire on "the"/"is"; **boot-time backfill** embeds facts saved pre-install. **Wave 1 (2026-07-17) closed four of the remaining gaps:** curated-summary **snapshot-at-turn-1** (frozen per conversation, privacy-filtered per turn), a per-profile **semantic-search toggle** (lazy embedder load; keyword-only when off), the inline **"remembered" save event**, and **walled-profile DB routing** (a walled profile's memory lives in its own physically-separate DB, proven to survive toggling back). **All write triggers DONE (Wave 3.5 complete):** trigger #1 save-as-you-go (`remember`, earlier), **#2 pre-compaction flush** (`f89536e`) and **#3 new-chat nudge** (`e0929f0`) — `agent/memory_flush.rs`: a LOCAL model extracts durable facts (guard-wrapped input) + saves via the exact sensitivity-routed path, async/off the stream lock, at-most-once (shared high-water) + content-deduped. **Still remaining:** embedder bundling into the packaged app (M9 / Wave 7.1). Design: PLAN §9. |
| **Skills system** | reusable playbooks, approve-first vs autonomous, teacher-escalation | 🟢 **Built (Wave 4.1–4.2, 2026-07-18):** skills CRUD (fail-closed to Pending) + `search_skills` (Safe, approved-only) + `save_skill` (Dangerous) + the draft-first learning loop (`agent/skill_reflect.rs`, drafts always Pending, default-off toggle) + the Settings→Skills review pane. **Not built from the original PLAN §10 design:** teacher-escalation (bigger model solves a twice-failed task and writes a skill), curator rot-check, the skill-as-Tool wrapper, Tier-3 script exec, seed skills. |
| **Privacy classifier** | rules layer + trained ONNX ensemble + redaction UX | 🟢 **DONE (item 3 complete)** — trained bge-small + distilbert INT8 ONNX ensemble in-process via `ort` (fused with layer-0 rules, parity-verified), the "why this was routed" annotated sidebar, **per-profile runtime thresholds** (settings page), AND **partial-delegation redact-and-send** (rule-value spans blacked out → re-classified → safe remainder to cloud → rehydrated; per-profile toggle). Only optional cosmetic `gate.rs` §7 renames remain (deferred, low-value). |
| **M5** — computer use | cross-platform screen control, the flagship | 🟡 **Security cores landed, all DORMANT by design** (design: `docs/plans/2026-07-18-m5-computer-use-design.md`): reversibility classifier (`tools/computer_use.rs` — not registered as a tool yet), multimodal wire format (`models/content.rs` — zero callers, `ChatMessage.content` still a plain `String`), screenshot-forces-local routing (`hooks/routing.rs::routing_for_turn` — zero callers). All tested; nothing wired until the on-target Slices 1–6 (capture/AX backend, input synthesis, `OnScreenActionHook`, Win/Linux). Audit 2026-07-21: cores match their design. **Note:** the design says Slice 3's logic half (mock-backend hook wiring) is headless-buildable. |
| **M6** — voice | on-device STT/TTS, barge-in | 🟡 **Audio-egress gate built + tested (`audio/privacy.rs`), dormant** — no caller until native voice lands. **⚠ Two design deviations found by the 2026-07-21 audit, fix BEFORE wiring native STT:** (1) `stt_egress` content-classifies a transcript, but the design mandates a content-free pre-transcription decision (you can't have a transcript before deciding whether audio goes to cloud STT) — untested; (2) Public+floor unconditionally withholds where the design mandates one confirm via the approval spine (stricter than spec, but a real deviation). Native STT/TTS/AEC on-target. |
| **M7** — per-profile isolation | email/calendar/tasks, Capability Packs, real OS sandbox enforcement | 🟡 **Tier-P (per-profile fs confinement) COMPLETE + LIVE** (`3801a0e` — strongest-evidenced area of the 2026-07-21 audit). Capability Packs shipped with Wave 4.5. **Tier-K partial:** the macOS shell-network ceiling enforcement is live code (`tools/exec.rs::effective_network`) **but unreachable in practice — no IPC/UI ever writes a `sandbox_config` row**, so every real profile takes the unconfigured branch today; needs a small settings surface (headless-buildable). Linux/Windows sandbox backends on-target. Email/calendar/tasks not started (needs a Lukas backend decision, M7 Q2). |
| **M8** — settings/onboarding/hardware | hardware probing, model catalog, first-run | 🟡 **Substantially built + LIVE in Settings→Models:** hardware probe (fails closed on unknown RAM), curated catalog, download + SHA-verify + resume (HF-only host allowlist), model list/remove. **Open:** catalog ships `sha256="TODO-CURATE"` placeholders — fails closed, so NOTHING is installable until real hashes are curated (headless-buildable); S4 `llama-server` sidecar (native, needs the C9/M8-Q2 Lukas decision); boot-time integrity re-check primitive exists but is unwired. Onboarding screen still visual-only. **DECIDED 2026-07-21 (Lukas): bundled sidecar = YES** — the app must be able to run its own models for users without external infra; the hardware probe must make an EDUCATED choice (memory bandwidth + dense-vs-MoE + GPU count/topology + quant, not just RAM capacity); external OpenAI-compatible endpoints (Lukas's own use case) remain a first-class, equally-easy path. macOS/Metal sidecar first; Win/Linux sidecar backends ride Wave 7.4. An ultracode run is being fired at this. |
| **M9** — polish | auto-update, signing, tray, Windows depth | ⬜ **Not started** |
| **M10** — beta | | ⬜ **Not started** |
| **Server companion** | the optional always-on twin | 📐 **Designed in full (nothing left to decide), zero code.** Gated on M4 landing. Design: PLAN §5. |

---

## What's left — near term, in recommended order

> **CURRENT LIST (2026-07-21, from the full project review — supersedes the numbered
> historical list below, which is kept for the record with its stale claims annotated).**
> All of these are headless-buildable on this Mac, none needs a Lukas decision:
>
> 1. **Write the missing "cloud seat can't defeat RouteLocal" end-to-end test** (HIGH from
>    the audit): bind a seat to a cloud provider, dispatch a `delegate` helper under a
>    `Private` binding through `run_subagent`, assert no cloud client is ever invoked.
>    ~20 lines; closes the one promised-but-missing regression test.
> 2. **Make the M7 Tier-K network ceiling reachable**: `get/set_sandbox_config` IPC + a
>    small Settings surface (per-profile "shell network" section). Until this exists the
>    ceiling enforcement is dead in practice.
> 3. **Fix `open_profile` whitespace-padding** (confusable profile names, flagged
>    2026-07-18): reject padded names at `open_profile` + the `send_message` IPC boundary.
> 4. **Thread per-profile `ClassifierConfig` into the tool-action gate**
>    (`hooks/privacy_filter.rs` gates at default thresholds today — stricter-than-default
>    profiles get weaker tool gating than chat gating).
> 5. **Reroute auto-switch UX (2.3)** — was stale-marked "blocked on 3.1"; 3.1 shipped.
>    Backend plumbing (`NeedsLocalReroute`, `stream:local_reroute`) exists; this is pure
>    frontend (toast + first-class local-endpoint object).
> 6. **Budget governor (3.2 tail)** — was stale-marked "needs cost capture"; cost capture
>    shipped. Hang the cap check off the existing `QueueingPrompter` unattended concept.
> 7. **Test debt from the audit:** the delegated-helper guard-wrap-on-re-entry branch,
>    the work-runner deadline/panic paths, and (shared root cause) a fake `ModelStreamer`
>    injectable into the REAL `process_message` so the cloud-safe guard / redact-and-send /
>    usage booking get true end-to-end coverage.
> 8. **M8 catalog sha256 curation** — download + hash the 4 catalog models so the
>    Settings→Models catalog can actually install something.
> 9. Then the bigger headless fronts: **MCP real wire transport**, **durability journal
>    (2.5, dep 4.4 now met)**, **M5 Slice 3 logic half** (mock-backend `OnScreenActionHook`
>    wiring), **M6 Slice 4a** (cooperative-cancel plumbing, fake-provider testable).

1. **[x] Settings "Permissions" pane** *(DONE 2026-07-16, `f38fd2c`)* — a "Permissions"
   section in Settings (between Privacy guard and Models) lists the active profile's
   persisted "Always allow" rules via `list_tool_rules` and revokes them via
   `delete_tool_rule` (two-click confirm). Verified live in the browser preview.
2. **[x] Frontend housekeeping** *(DONE 2026-07-16, `6dfcf12`)* — deleted the 5
   superseded components (kept `ApprovalDialog.svelte`); removed the dev floating
   screen-switcher + theme toggle from `App.svelte`; fixed the `ModelPicker` name
   collision (options now carry a composite `providerId::name` key — two same-named
   models list & select independently, verified live with LM Studio + Anthropic
   `default`). CSS bundle dropped 63.6 → 56.4 kB.
3. **[x] Classifier integration round — DONE 2026-07-16** (all three sub-rounds:
   ONNX wiring `283789b`, settings page `819df8c`, redact-and-send `7d7dae5`; only
   the optional cosmetic `gate.rs` §7 renames remain, deferred as low-value).
   **Per-profile settings page** (the classifier-settings round): `ClassifierConfig` (tau_block /
   tau_band) is now per-profile runtime-tunable via a back-compat `classify_with`
   trait method, a per-profile `classifier_settings` table (migration v4),
   `get/set/reset_classifier_settings` IPC, and a live Settings "Privacy guard"
   section (strictness slider + uncertainty band + reset). **Strictness drives
   `tau_band`** (the actual egress line — Private/Uncertain route identically, so
   `tau_block` alone never gates egress; the review caught this), band drives
   `tau_block` (the Private/Uncertain *labeling* split, shown in the "why"
   sidebar). `sanitized()` clamps to the reachable UI range so a corrupt row can't
   make the filter looser than strictness 0. `remember`/`save_memory` route under
   the profile config too. Adversarially reviewed (3 lenses) → 5 findings fixed
   (leaky `sanitized`, inert strictness knob, `remember` bypass, inverted copy,
   overclaiming hook comment). 353 tests. Tool-action gate still uses default
   thresholds (documented follow-up). **Below: the earlier ONNX-wiring work.**

   **[~] Classifier integration round (ONNX)** — **export + ONNX wiring DONE 2026-07-16**
   (`283789b`). Export: ran the bundle's `export_onnx.py` (Python 3.11 arm64 venv) →
   both encoders to fp32 + INT8, preserved at `~/Desktop/Classifier Model + Install
   Guide for Claude/onnx-export/`. Wiring: `classifier/engine.rs` now runs the real
   INT8 ensemble via `ort` (rules layer-0 short-circuit → windowed max-prob over both
   encoders → fusion at 0.5/0.05), mirroring `serve.py` exactly; behind a default-on
   `onnx-classifier` feature (rules-only fallback with `--no-default-features`). Models
   installed live at `~/Documents/Lost-Harness/models/classifier/` (98 MB); parity test
   passes on them. **The annotated review sidebar is DONE** (PLAN §11 decisions c+d):
   `explain_classification` IPC (`9bff6c2`) + MainScreen's routing panel wired to it
   (`914ac74`) — the last user message renders with detected spans marked inline (amber
   soft / red hard-block), a "what tripped the guard" legend (category · hard-flag ·
   rule/model layer), verdict-driven heading, browser-QA'd end-to-end. **The item-3
   tail is now closed:** (a) partial-delegation redact-and-send — DONE (`7d7dae5`:
   rule-value spans blacked out → redacted text re-classified → only a clean remainder
   goes to cloud → reply rehydrated; per-profile toggle); (b) per-profile classifier
   settings page — DONE (`819df8c`, see above); (c) OPTIONAL cosmetic `gate.rs` §7
   renames stay deferred (low-value/high-churn).
4. **[x] Native tool-use + `Tool::schema()` (Q1, M4)** — **DONE + PROVEN LIVE 2026-07-17** (`d203a9a`).
   Per-endpoint `supports_native_tools` flag (endpoints v4 column, threads through
   Provider/ProviderInfo/AddProviderArgs, persisted + hydrated); `Tool::schema()` →
   `dispatcher.native_tools_spec()` (OpenAI function-call array, name/desc neutralized);
   `ChatRequest.tools` + `stream_chat_with_tools`; SSE decodes `delta.tool_calls` →
   `assemble_native_calls` normalizes to the same `ParsedToolCall` as the fenced path;
   the loop picks transport per round and NEVER runs the fenced parser on a native turn
   (invariant #5 structural). Fenced dialect stays the fallback. **Fingerprint parity
   across transports is tested**, plus SSE wire decode + assembly unit tests. **LIVE proof
   DONE** — `live_native_tool_call_roundtrip` ran green 3× against LM Studio qwen3.6-35b-a3b
   (2026-07-17): the model chose `get_weather`, streamed native `tool_calls`, our parser
   reconstructed `get_weather(city=…)`. Remaining polish (not blocking): an add-provider UI
   checkbox to set `supports_native_tools` so day-to-day chat uses the native path.
5. **[~] Memory system — HYBRID + LIVE** (`3ee9790`→`bfb5721`). Built and live: the
   full earlier stack (storage buckets in physically-separate stores, FTS5 keyword search,
   curated-summary pinning, Settings "Memory" tab, `recall_memory`/`remember` tools,
   endpoint-aware private recall, auto-injection, non-silent recall banner) PLUS the
   **meaning lane** (2026-07-17): a stock **bge-small-en-v1.5 INT8** embedder (`embedder.rs`,
   same ONNX runtime/install/fallback as the classifier — deliberately NOT the classifier's
   bge, which is a fine-tuned classification head with no general-purpose embeddings);
   **hybrid keyword+semantic search fused by Reciprocal Rank Fusion**; the **private vector
   index is a physically-separate table** (`memory_vectors_private`) so a cloud turn never
   queries it (same wall as the fact tables); distance gates **calibrated on the live model**
   (inject 0.38 / recall 0.48, from real measured bands ≈0.33 related / ≈0.43 adjacent /
   ≈0.54+ unrelated); **FTS stopword-filtered** so the injection relevance gate stops firing
   on "the"/"is"; **boot-time backfill** embeds facts saved before the model was installed.
   Model at `~/Documents/Lost-Harness/models/embedder/` (34 MB, not in git; keyword-only if
   absent — the dev/fallback path). **Remaining (2026-07-21 note: everything below EXCEPT
   embedder bundling has since landed — Wave 1 items 1.2–1.5 + Wave 3.5; kept as written
   for the record):** **embedder bundled into the app + a memory
   settings toggle** (decided 2026-07-17, PLAN §9 — the model is the app's OWN bundled
   component, NOT a user download or a served endpoint like LM Studio's nomic model; it loads
   only when the user enables semantic memory search, else keyword-only. Bundling itself is
   the M9 packaging task alongside the classifier + ORT dylib; the settings toggle is
   near-term); **curated-summary snapshot at turn 1** (currently re-read live each turn —
   PLAN §9 wants it frozen per conversation for cache stability); **pre-compaction flush +
   new-chat nudge** write triggers (flush is moot until context compaction exists at all); an
   inline **"remembered" save event** (recall has its banner; saves surface only via the
   approval prompt); **walled-profile DB routing** (§7 toggle → the profile's own memory DB).
   Design: PLAN §9 (incl. the 2026-07-15 refinements).
6. **[~] Rest of M4 (Wave 3)** — **[~] usage ledger (3.2)**: the per-profile
   `usage_events` cost ledger + booking landed (`7141d34`, PROFILE v6→v7) — every
   model call books one row, local=$0 / cloud=unknown-flagged (never guessed).
   **Follow-up (rest of 3.2): the budget governor** — a cost-cap that halts
   unattended spend; needs real per-call cost capture first (SSE `usage` token
   parsing + a pricing table — cloud costs are "unknown" until then, so a
   cost-cap can't meaningfully fire yet; count-based budgets already exist from
   Q4). *(2026-07-21 note: cost capture has since SHIPPED — the governor is now
   genuinely buildable, see the current list above.)* STILL OPEN (Tier-A ∥):
   ~~model seats (3.1)~~ *(DONE `ec6c852`)*, ~~cache-shaped prompt assembly +
   context compaction (3.3)~~ *(DONE `f543250`)*, capability registry that
   refuses (3.4 — no refusal-triggering consumer yet; native-tools has a valid
   fenced fallback). ~~Then the skills & agents track~~ *(Wave 4.1–4.5 all DONE
   2026-07-18)*.
7. **[~] Remaining core tools** — DONE: session search, system status (2.1, `6a97695`);
   **cron management** (`9008cfb` — `list_cron_jobs`/`manage_cron`); **headless browser →
   `fetch_url`** (`f9e49eb` — the first External/egress tool, SSRF-guarded); **`ask_human`**
   (`e5da77f` — the single blocking "ask the user" tool, Safe, `HumanPrompter` +
   `AskHumanDialog`). ~~STILL OPEN: only **`delegate`**~~ *(2026-07-21: `delegate` is
   DONE — Wave 4.3c runtime `e1e1f8c`, `tools/delegate.rs`. This item is fully closed;
   every core tool rides the approval spine.)*

**[x] Wave 1 of the build manifest — DONE 2026-07-17.** All started subsystems finished:
- **[x] Native-tool UI checkbox** (1.1) — the add-provider Settings form now has a "Native
  tool-calling" toggle threaded through `addProvider` → `AddProviderArgs`, so a provider marked
  native uses the native transport in everyday chat (not just the env-gated test).
- **[x] Memory semantic-search toggle** (1.2) — per-profile setting gating the meaning-lane
  embedder; the ~34 MB model now loads **lazily** (`EmbedderHandle`) and only when a profile has
  semantic search on, so "off" computes no fingerprint and never loads the model.
- **[x] Curated-summary snapshot at turn 1** (1.3) — frozen per conversation (cache-stable
  prompt prefix); a mid-conversation `remember` shows up next conversation, not this one.
- **[x] Inline "remembered" save event** (1.4) — non-silent `memory:event {kind:"remembered"}`
  → transient banner, matching the "recalled" event.
- **[x] Walled-profile memory DB routing** (1.5) — the §7 island: a walled profile's memory
  routes to its own physically-separate DB under `walled-memory/<name>.db`, never `global.db`;
  the wall survives toggling back off (tested).
- Still pending (moved to later waves): **embedder bundling** into the packaged app (M9 / Wave
  7.1); **write triggers** need context compaction first (Wave 3.3 → 3.5).

**Also queued in M4/later (pointers in build plan Part 2):** `UserPromptSubmit` hook +
permission modes (Q11 — modes landed `5bf3c37`; the `UserPromptSubmit` half is deferred),
reroute auto-switch UX (Q6, dep 3.1), persisted action journal + idempotency keys (Q3
deferred half, dep 4.4). **[x] Headless approval queue (Q5, server-track prep) — DONE
2026-07-17 (`26d775e`):** `QueueingPrompter` + `ApprovalQueue`, rule pre-authorization via
the Q8 PolicySource, Dangerous/External floors enforced in-prompter, adversarially reviewed.
Not wired into a live body yet (no headless body exists until Wave 6).

---

## Blocked / waiting on something

- **Nothing.** (The native-tool-use live proof — previously blocked on LM Studio's
  require-API-token toggle — was cleared 2026-07-17: Lukas turned auth off, the live test
  passed 3× against qwen3.6-35b-a3b. Item 4 is fully done.)

## Accepted quirks (documented, not bugs to fix)

- The `onnx-classifier` feature (default on) pulls `ort`, which downloads the ONNX
  Runtime native lib at build time. If a CI runner can't reach that CDN, build with
  `--no-default-features` (rules-only, no native dep) — the classifier degrades to
  layer 0, nothing breaks. Bundling the ORT dylib into the shipped app is an M9
  (packaging) task, not done yet — the classifier is live in `cargo`/dev, not yet in
  a `tauri build` bundle. Model files (~98 MB INT8) are NOT in git; they live in the
  app's storage dir and are installed out-of-band (see item 3).

- A `setsid()`-detached `shell_exec` descendant escapes the timeout group-kill but stays
  Seatbelt-confined. Bounded runaway; durable fix = VM isolation, far-future.
- Rust toolchain on this Mac is x86_64 under Rosetta. Works; arm64 rustup is optional cleanup.
- `trm_logs` table keeps its legacy name (renaming a persisted table needs a migration).
- svelte-check emits 1 pre-existing tsconfig warning (`svelte.config.js` overwrite note) — noise.

## Loose ends (tracked, not urgent)

- 7 screens render sample data only: Email, Files, Whiteboard, ScheduledJobs, Editor,
  Onboarding, EmptyState. They wire up as their subsystems land — don't wire them early.
- Now that the dev screen-switcher is gone, Onboarding / Editor / EmptyState have no
  in-app nav path yet (they're reached programmatically via `nav.go`). They get real
  entry points when their subsystems land — sidebar/composer nav already reaches the
  rest. To eyeball one during dev, call `nav.go('onboarding')` or temporarily route to it.
- `CommandPalette.svelte` ported but not mounted anywhere (M2 leftover).
- App entry is `/app.html`, **not** `/` — regressing this reproduces the blank-GUI bug.

---

## For agents: how to use and maintain this file

- **When Lukas asks "what stage are we at":** read the **Stage** blockquote + the
  milestone board, verify with the health-check commands if anything might have
  changed, and answer in plain terms (he's an infra architect, not a programmer —
  outcomes and analogies, not code jargon).
- **When you finish a round of work:** update the Stage line and date, tick/move the
  checklist items, update the expected test count in the health check, and add your
  session entry to `HANDOFF.md` as usual. Keep PLAN.md for *design* changes only.
- **Don't re-litigate decided things.** Every open product decision is resolved (PLAN
  §7). If something here contradicts PLAN.md, PLAN.md wins — then fix this file.
