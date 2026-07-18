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

> **As of 2026-07-18: M3 COMPLETE. M4 well underway. WAVE 1 + WAVE 2 (ready work)
> COMPLETE; WAVE 3 largely landed (3.2 ledger+cost, 3.3 compaction, 3.5 flush).**
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
> with the first consumer. **453 → 458 tests.** **Next fronts (per the plan):**
> the 4.4 scheduler+executor (needs the first consumer + the AppHandle-decoupling
> `ResultSink`); 4.1 skills (a skill = a Tool riding the registry+gate chain);
> 4.3 agent registry (needs 3.1 seats); then 4.2 (a near-copy of `memory_flush`)
> + 4.5 packs. The 3.2 budget governor stays server-track.
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
> **Prior state (still true):**
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
cd /Users/hayai/Desktop/lost-harness-product/src-tauri && cargo test --lib   # expect: 458 passed, 0 failed
cd /Users/hayai/Desktop/lost-harness-product && npm run build               # expect: clean
cd /Users/hayai/Desktop/lost-harness-product && npm run check               # expect: 0 errors (1 pre-existing tsconfig warning is known noise)
# The trained classifier is behind a default-on feature. To run its ONNX parity test:
cd .../src-tauri && LHP_CLASSIFIER_MODELS_DIR="$HOME/Documents/Lost-Harness/models/classifier" cargo test --lib parity_tests
# The rules-only fallback (no native ONNX Runtime dep) must also build:
cd .../src-tauri && cargo build --lib --no-default-features
```

Optional env-gated live/model tests (not part of the 385; run manually):
```bash
# Memory embedder sanity + gate calibration on the live INT8 model:
LHP_EMBEDDER_MODELS_DIR="$HOME/Documents/Lost-Harness/models/embedder" cargo test --lib embedder::
# Native tool-use against a live endpoint (needs a native-capable server, auth off or token set):
LHP_NATIVE_ENDPOINT="http://127.0.0.1:1234/v1" LHP_NATIVE_MODEL="qwen/qwen3.6-35b-a3b" \
  cargo test --lib live_native_tool_call_roundtrip -- --nocapture
```

Last verified: 2026-07-17 (Wave 4.4 substrate landed:
**458 passed**, `--no-default-features` builds clean, `cargo clippy --lib` 0 errors,
frontend build + svelte-check clean, tree clean; embedder live test passes on the installed model).

---

## Milestone board

| Milestone | What it is | Status |
|---|---|---|
| **M0** — bootstrap | Tauri + Svelte + Tailwind + CI | ✅ **Done** |
| **M1** — vertical slice | message → classify → route → model → stream → save | ✅ **Done + verified** (contract tests at the real IPC boundary) |
| **M2** — UI shell | design system, profiles, command palette | 🟡 **Mostly done** — design-system port landed and wired for chat/sidebar/settings; profile switching works. Superseded components deleted + dev screen-switcher removed (2026-07-16). Remaining gaps: `CommandPalette.svelte` is ported but mounted nowhere; 7 screens are visual-only (see Loose ends). |
| **M3** — tool registry + spine | the whole security/tool foundation | ✅ **Done** (2026-07-16) — all 8 do-now items + approval spine + write/shell/MCP tools, every round adversarially reviewed. Exception: the durability trio's persisted-journal half is deliberately deferred to the first external-effect tool (see PLAN §8 / build plan Q3). |
| **M4** — model manager + skills/agents | native tool-use, seats, usage ledger, budget governor, cache-shaped prompts; skills & agents track | 🔵 **In progress** — Q8 (grant×risk matrix + persisted `tool_rules` + risk-badged dialog) done 2026-07-16. **Native tool-use (Q1) DONE + PROVEN LIVE 2026-07-17** (`d203a9a`): per-endpoint `supports_native_tools` flag, structured `tool_calls` transport + fenced fallback, one transport-blind pipeline, fingerprint parity tested, and verified end-to-end against LM Studio qwen3.6-35b-a3b (3 clean runs). **Usage ledger (3.2) booking side DONE 2026-07-17** (`7141d34`): per-profile `usage_events` cost ledger, every model call booked, local=$0/cloud=unknown-flagged. **Cache-shaped prompts + context compaction (3.3) DONE 2026-07-17** (`f543250`): byte-stable prefix + deterministic `compact_history` at the stream seam, emits the 3.5 pre-compaction signal (+ a HIGH cloud-history privacy fix `c026e33`). Not started: budget governor (needs SSE cost capture first), model seats (3.1), capability registry (3.4), skills & agents. **Wave 3.5 (pre-compaction flush) is now unblocked** — swap the `on_pre_compaction` seam body. |
| **Memory system** | curated summary + searchable archive (hybrid FTS5 + sqlite-vec), profile wall, 3-bucket sensitivity routing | 🟢 **HYBRID + LIVE (meaning lane landed 2026-07-17, `bfb5721`).** Storage + IPC + Settings "Memory" tab + `recall_memory`/`remember` tools + endpoint-aware `allow_private_memory` + auto-injection + non-silent recall banner (all earlier), PLUS now: the **sqlite-vec meaning lane** — a stock **bge-small-en-v1.5 INT8 embedder** (`embedder.rs`, same ONNX runtime/install/fallback as the classifier; installed at `~/Documents/Lost-Harness/models/embedder/`) feeds hybrid keyword+semantic search fused by **Reciprocal Rank Fusion**; the **private vector index is a physically-separate table** (`memory_vectors_private`) so a cloud turn never queries it; distance gates **calibrated on the live model** (inject 0.38 / recall 0.48); **stopword-filtered** FTS so the injection relevance gate doesn't fire on "the"/"is"; **boot-time backfill** embeds facts saved pre-install. **Wave 1 (2026-07-17) closed four of the remaining gaps:** curated-summary **snapshot-at-turn-1** (frozen per conversation, privacy-filtered per turn), a per-profile **semantic-search toggle** (lazy embedder load; keyword-only when off), the inline **"remembered" save event**, and **walled-profile DB routing** (a walled profile's memory lives in its own physically-separate DB, proven to survive toggling back). **All write triggers DONE (Wave 3.5 complete):** trigger #1 save-as-you-go (`remember`, earlier), **#2 pre-compaction flush** (`f89536e`) and **#3 new-chat nudge** (`e0929f0`) — `agent/memory_flush.rs`: a LOCAL model extracts durable facts (guard-wrapped input) + saves via the exact sensitivity-routed path, async/off the stream lock, at-most-once (shared high-water) + content-deduped. **Still remaining:** embedder bundling into the packaged app (M9 / Wave 7.1). Design: PLAN §9. |
| **Skills system** | reusable playbooks, approve-first vs autonomous, teacher-escalation | 📐 **Designed in full, not built.** Design: PLAN §10. |
| **Privacy classifier** | rules layer + trained ONNX ensemble + redaction UX | 🟢 **DONE (item 3 complete)** — trained bge-small + distilbert INT8 ONNX ensemble in-process via `ort` (fused with layer-0 rules, parity-verified), the "why this was routed" annotated sidebar, **per-profile runtime thresholds** (settings page), AND **partial-delegation redact-and-send** (rule-value spans blacked out → re-classified → safe remainder to cloud → rehydrated; per-profile toggle). Only optional cosmetic `gate.rs` §7 renames remain (deferred, low-value). |
| **M5** — computer use | cross-platform screen control, the flagship | ⬜ **Not started** (stubs in `src-tauri/src/platform/`) |
| **M6** — voice | on-device STT/TTS, barge-in | ⬜ **Not started** (stub in `src-tauri/src/audio/`) |
| **M7** — per-profile isolation | email/calendar/tasks, Capability Packs, real OS sandbox enforcement | ⬜ **Not started** |
| **M8** — settings/onboarding/hardware | hardware probing, model catalog, first-run | ⬜ **Not started** (Onboarding screen exists visually only) |
| **M9** — polish | auto-update, signing, tray, Windows depth | ⬜ **Not started** |
| **M10** — beta | | ⬜ **Not started** |
| **Server companion** | the optional always-on twin | 📐 **Designed in full (nothing left to decide), zero code.** Gated on M4 landing. Design: PLAN §5. |

---

## What's left — near term, in recommended order

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
   absent — the dev/fallback path). **Remaining:** **embedder bundled into the app + a memory
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
   Q4). STILL OPEN (Tier-A ∥): model seats (3.1 — no consumer until 4.3 agent
   registry), cache-shaped prompt assembly + context compaction (3.3), capability
   registry that refuses (3.4 — no refusal-triggering consumer yet; native-tools
   has a valid fenced fallback). Then the skills & agents track (do the
   one-queue-model unification pass 4.4 before locking its schemas).
7. **[~] Remaining core tools** — DONE: session search, system status (2.1, `6a97695`);
   **cron management** (`9008cfb` — `list_cron_jobs`/`manage_cron`); **headless browser →
   `fetch_url`** (`f9e49eb` — the first External/egress tool, SSRF-guarded); **`ask_human`**
   (`e5da77f` — the single blocking "ask the user" tool, Safe, `HumanPrompter` +
   `AskHumanDialog`). STILL OPEN: only **`delegate`** (blocked on the 4.3 agent-type
   registry — a real delegate dispatches a sub-agent). Each rides the existing approval spine.

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
