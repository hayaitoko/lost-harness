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

> **As of 2026-07-16: M3 COMPLETE. M4 in progress. Near-term items 1, 2, 3, and the
> memory-system foundation (item 5) have all had major landings this session.**
> Done + verified: the Q8 Permissions pane; the frontend housekeeping; the trained
> bge-small + distilbert INT8 ONNX ensemble running in-process via `ort`
> (parity-verified), fused with the rules layer; the annotated "why this was routed"
> sidebar wired to the real classifier; and the **memory storage foundation**
> (sensitivity buckets in physically-separate stores + FTS5 keyword search + curated
> summary) — now **usable end-to-end**: IPC + the Settings "Memory" tab wired to real
> facts, plus the `recall_memory` (Safe/pre-trusted, shared-only) and `remember`
> (Write/approval-gated, sensitivity-routed) agent tools. Item 4 (native tool-use, Q1)
> is **blocked** on configuring a native-tool-capable model endpoint. Open fronts:
> item 3's tail (partial-delegation + classifier settings) and the rest of memory
> (meaning-search lane needs an embedder; automatic injection / curated-summary-at-start
> / write-triggers / non-silent events are agent-loop work). **Skills** remains fully
> designed, zero code.

**Health check (run this before believing anything below; update expected numbers when they change):**

```bash
cd /Users/hayai/Desktop/lost-harness-product/src-tauri && cargo test --lib   # expect: 353 passed, 0 failed
cd /Users/hayai/Desktop/lost-harness-product && npm run build               # expect: clean
cd /Users/hayai/Desktop/lost-harness-product && npm run check               # expect: 0 errors (1 pre-existing tsconfig warning is known noise)
# The trained classifier is behind a default-on feature. To run its ONNX parity test:
cd .../src-tauri && LHP_CLASSIFIER_MODELS_DIR="$HOME/Documents/Lost-Harness/models/classifier" cargo test --lib parity_tests
# The rules-only fallback (no native ONNX Runtime dep) must also build:
cd .../src-tauri && cargo build --lib --no-default-features
```

Last verified: 2026-07-16 (classifier ONNX ensemble wired: 333 green; full-feature +
`--no-default-features` builds clean; parity test passes on the live INT8 models).

---

## Milestone board

| Milestone | What it is | Status |
|---|---|---|
| **M0** — bootstrap | Tauri + Svelte + Tailwind + CI | ✅ **Done** |
| **M1** — vertical slice | message → classify → route → model → stream → save | ✅ **Done + verified** (contract tests at the real IPC boundary) |
| **M2** — UI shell | design system, profiles, command palette | 🟡 **Mostly done** — design-system port landed and wired for chat/sidebar/settings; profile switching works. Superseded components deleted + dev screen-switcher removed (2026-07-16). Remaining gaps: `CommandPalette.svelte` is ported but mounted nowhere; 7 screens are visual-only (see Loose ends). |
| **M3** — tool registry + spine | the whole security/tool foundation | ✅ **Done** (2026-07-16) — all 8 do-now items + approval spine + write/shell/MCP tools, every round adversarially reviewed. Exception: the durability trio's persisted-journal half is deliberately deferred to the first external-effect tool (see PLAN §8 / build plan Q3). |
| **M4** — model manager + skills/agents | native tool-use, seats, usage ledger, budget governor, cache-shaped prompts; skills & agents track | 🔵 **In progress** — Q8 (grant×risk matrix + persisted `tool_rules` + risk-badged dialog) done 2026-07-16. Everything else not started. |
| **Memory system** | curated summary + searchable archive (hybrid FTS5 + sqlite-vec), profile wall, 3-bucket sensitivity routing | 🔵 **Usable — storage + IPC + UI + agent tools built (2026-07-16, `3ee9790`→`cdc8d6f`).** Storage foundation (buckets in physically-separate stores, FTS5 keyword search, curated-summary pinning); IPC + the Settings "Memory" tab wired to real facts (add/forget/pin, sensitivity badges); the `recall_memory` tool (Safe/pre-trusted, shared-only so it can't leak private facts) + the `remember` tool (Write/approval-gated, sensitivity-routed). **Remaining:** sqlite-vec meaning lane (needs a local embedder), automatic relevance-gated injection + curated-summary-at-conversation-start (agent-loop/context assembly), non-silent memory events, endpoint-aware private recall (needs `ExecCtx` endpoint kind), walled-profile DB routing. Design: PLAN §9. |
| **Skills system** | reusable playbooks, approve-first vs autonomous, teacher-escalation | 📐 **Designed in full, not built.** Design: PLAN §10. |
| **Privacy classifier** | rules layer + trained ONNX ensemble + redaction UX | 🟢 **Ensemble live + annotated sidebar done** — the trained bge-small + distilbert INT8 ONNX ensemble runs in-process via `ort` (`classifier/engine.rs`), fused with the layer-0 rules, parity-verified. Active when its models are installed (`~/Documents/Lost-Harness/models/classifier/`), rules-only fallback otherwise. The "why this was routed" sidebar renders real detected spans (inline marks + legend). Remaining tail: partial-delegation redact-and-send + the per-profile classifier settings page. |
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
3. **[~] Classifier integration round** — **per-profile settings page DONE
   2026-07-16** (the classifier-settings round): `ClassifierConfig` (tau_block /
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
   rule/model layer), verdict-driven heading, browser-QA'd end-to-end. **Remaining
   (item-3 tail):** (a) partial-delegation redact-and-send flow (`serve.py /redact`
   `safe_text`: merge spans → `[REDACTED:CODE]` → re-classify → send only if clean +
   rehydrate — a deeper agent-loop change); (b) the per-profile classifier settings page
   (strictness/band/redaction/hard-block — needs runtime-tunable thresholds, currently
   hardcoded `TAU_BLOCK=0.5`/`TAU_BAND=0.05`); (c) OPTIONAL cosmetic `gate.rs` §7 renames
   (deferred, low-value/high-churn).
4. **[ ] Native tool-use + `Tool::schema()` (Q1, M4)** — per-endpoint capability flag;
   native `tool_use` path for models that support it; fenced dialect stays the fallback;
   fingerprint-parity regression test across transports. Needs a native-tool-capable
   endpoint configured to prove end-to-end.
5. **[~] Memory system** — **usable end-to-end** (`3ee9790`→`cdc8d6f`): storage
   foundation (sensitivity buckets, physically-separate private store, FTS5 keyword
   search, curated-summary pinning); IPC + the Settings "Memory" tab wired to real facts
   (add/forget/pin + "on device only" badges); the **`recall_memory`** tool
   (Safe/pre-trusted, shared-only — can't leak private facts into model context) and the
   **`remember`** tool (Write/approval-gated, sensitivity-routed — credential dropped,
   private→local, benign→shared); routing hoisted canonical in `tools::memory`. All
   tested. **Remaining:** the sqlite-vec **meaning lane** (needs a local embedder — the
   classifier's bge is a *classification* head, not an embedder, so a separate small
   embed model is the open choice); **automatic relevance-gated injection** + loading the
   **curated summary at conversation start** (agent-loop / context assembly — also the
   home of the pre-compaction/new-chat write triggers); **non-silent memory events** (the
   event-bar language); **endpoint-aware private recall** (needs `ExecCtx` to carry the
   turn's endpoint kind — until then recall is conservatively shared-only); walled-profile
   DB routing (§7 toggle → per-profile memory DB). Design complete
   (PLAN §9 incl. the 2026-07-15 refinements: 3 sensitivity buckets, relevance-gated
   injection, non-silent memory events). Storage schema branches on the per-profile
   privacy toggle (decided 2026-07-08).
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
