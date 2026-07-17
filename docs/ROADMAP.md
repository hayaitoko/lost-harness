# Lost Harness — Roadmap & Stage Tracker

**Purpose:** the one file that answers *"what stage are we at, what's left, what's
next."* When Lukas asks for a status, answer from here. Keep it honest: update the
**Stage** line and the checklists every time a work round lands, and move items
between sections rather than duplicating them.

Where things live: design decisions → [`PLAN.md`](PLAN.md) (source of truth);
implementation detail → [`codebase/README.md`](codebase/README.md); the executable
specs for tool-system work → [`tool-system-build-plan.md`](tool-system-build-plan.md);
current-session context and gotchas → [`../HANDOFF.md`](../HANDOFF.md). This file is
the status board sitting on top of all of them.

---

## Stage

> **As of 2026-07-16 (evening): M3 COMPLETE. M4 in progress. Near-term items 1, 2,
> and 3 are DONE; memory (item 5) is LIVE in conversations.**
> The classifier round is **fully closed**: trained INT8 ONNX ensemble in-process
> (parity-verified), the annotated "why" sidebar, per-profile runtime-tunable
> thresholds (Settings "Privacy guard"), and partial-delegation redact-and-send.
> Memory is **live in real turns**: curated summary + relevance-gated FTS snippet
> injection (guard-wrapped, profile-scoped), endpoint-aware private recall (private
> facts readable only on non-cloud, same-profile turns), and a non-silent recall
> banner. Item 4 (native tool-use, Q1) is **blocked** on configuring a
> native-tool-capable model endpoint — a Lukas action, not code. **The next open
> engineering front is memory's meaning lane: choosing + wiring a small local
> embedder so search matches meaning, not just keywords** (then: summary
> snapshot-at-turn-1, write-trigger backstops, walled-profile DB routing). **Skills**
> remains fully designed, zero code.

**Health check (run this before believing anything below; update expected numbers when they change):**

```bash
cd /Users/hayai/Desktop/lost-harness-product/src-tauri && cargo test --lib   # expect: 369 passed, 0 failed
cd /Users/hayai/Desktop/lost-harness-product && npm run build               # expect: clean
cd /Users/hayai/Desktop/lost-harness-product && npm run check               # expect: 0 errors (1 pre-existing tsconfig warning is known noise)
# The trained classifier is behind a default-on feature. To run its ONNX parity test:
cd .../src-tauri && LHP_CLASSIFIER_MODELS_DIR="$HOME/Documents/Lost-Harness/models/classifier" cargo test --lib parity_tests
# The rules-only fallback (no native ONNX Runtime dep) must also build:
cd .../src-tauri && cargo build --lib --no-default-features
```

Last verified: 2026-07-16 evening (independent audit after the memory-live round:
**369 passed**, frontend build + svelte-check clean, git tree clean).

---

## Milestone board

| Milestone | What it is | Status |
|---|---|---|
| **M0** — bootstrap | Tauri + Svelte + Tailwind + CI | ✅ **Done** |
| **M1** — vertical slice | message → classify → route → model → stream → save | ✅ **Done + verified** (contract tests at the real IPC boundary) |
| **M2** — UI shell | design system, profiles, command palette | 🟡 **Mostly done** — design-system port landed and wired for chat/sidebar/settings; profile switching works. Superseded components deleted + dev screen-switcher removed (2026-07-16). Remaining gaps: `CommandPalette.svelte` is ported but mounted nowhere; 7 screens are visual-only (see Loose ends). |
| **M3** — tool registry + spine | the whole security/tool foundation | ✅ **Done** (2026-07-16) — all 8 do-now items + approval spine + write/shell/MCP tools, every round adversarially reviewed. Exception: the durability trio's persisted-journal half is deliberately deferred to the first external-effect tool (see PLAN §8 / build plan Q3). |
| **M4** — model manager + skills/agents | native tool-use, seats, usage ledger, budget governor, cache-shaped prompts; skills & agents track | 🔵 **In progress** — Q8 (grant×risk matrix + persisted `tool_rules` + risk-badged dialog) done 2026-07-16. Everything else not started. |
| **Memory system** | curated summary + searchable archive (hybrid FTS5 + sqlite-vec), profile wall, 3-bucket sensitivity routing | 🟢 **LIVE in conversations (2026-07-16).** Storage foundation + IPC + Settings "Memory" tab + `recall_memory`/`remember` tools (all earlier), PLUS now: **endpoint-aware `ExecCtx`** (`allow_private_memory`) so recall reads private-local facts only on a non-cloud, same-profile turn; **curated summary + relevance-gated FTS injection** into each turn (guard-wrapped, endpoint-aware, profile-scoped); **non-silent `memory:event`** → transient recall banner. Cross-profile private-recall leak found in review + fixed (private scoped to the active profile; shared stays cross-profile). **Remaining:** the sqlite-vec **meaning lane** (needs a local embedder — the open choice); curated-summary snapshot-at-turn-1 (currently re-injected live each turn); pre-compaction/new-chat write triggers; walled-profile per-profile DB routing; an inline "remembered" save event (the approval prompt already surfaces saves). Design: PLAN §9. |
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
4. **[ ] Native tool-use + `Tool::schema()` (Q1, M4)** — per-endpoint capability flag;
   native `tool_use` path for models that support it; fenced dialect stays the fallback;
   fingerprint-parity regression test across transports. Needs a native-tool-capable
   endpoint configured to prove end-to-end.
5. **[~] Memory system — LIVE in conversations** (`3ee9790`→`6115eb9`). Built and
   live: storage foundation (sensitivity buckets in physically-separate stores, FTS5
   keyword search, curated-summary pinning); IPC + the Settings "Memory" tab
   (add/forget/pin + "on device only" badges); the **`recall_memory`** tool
   (Safe/pre-trusted) and **`remember`** tool (Write/approval-gated,
   sensitivity-routed); **endpoint-aware private recall** (`ExecCtx.allow_private_memory`
   stamped by the dispatcher — private-local facts readable only on a non-cloud,
   *same-profile* turn; a cross-profile private-recall leak was caught in review and
   fixed); **automatic injection** (`assemble_memory_context`: curated summary + ≤3
   FTS-matched snippets per turn, guard-wrapped as untrusted, profile-scoped; a storage
   error never blocks the send); **non-silent recall** (`memory:event` → transient
   MainScreen banner). **Remaining:** the sqlite-vec **meaning lane** — needs a small
   local **embedder** (the classifier's bge is a *classification* head, not an
   embedder; picking the embed model is the open decision) which upgrades injection +
   recall from keyword-only to true hybrid; **curated-summary snapshot at turn 1**
   (currently re-read live each turn — PLAN §9 wants it frozen per conversation for
   cache stability); **pre-compaction flush + new-chat nudge** write triggers (flush
   is moot until context compaction exists at all); an inline **"remembered" save
   event** (recall has its banner; saves currently surface only via the approval
   prompt); **walled-profile DB routing** (§7 toggle → the profile's own memory DB).
   Design: PLAN §9 (incl. the 2026-07-15 refinements).
6. **[ ] Rest of M4** — model seats, usage ledger + budget governor (per-profile),
   cache-shaped prompt assembly, capability registry that refuses; then the skills &
   agents track (do the one-queue-model unification pass before locking its schemas).
7. **[ ] Remaining core tools** — headless browser, delegate, ask-human, system status,
   cron management, session search (PLAN §8 M3 item 10 leftovers; each rides the
   existing approval spine).

**Also queued in M4/later (pointers in build plan Part 2):** `UserPromptSubmit` hook +
permission modes (Q11), reroute auto-switch UX (Q6), persisted action journal +
idempotency keys (Q3 deferred half), headless approval queue (Q5, server-track prep).

---

## Blocked / waiting on something

- **Nothing.** (The ONNX ensemble export — previously the only blocker — was run on
  2026-07-16; artifacts are produced and preserved. Item 3's remaining work is
  ordinary Rust wiring, no external dependency.)

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
