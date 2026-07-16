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

> **As of 2026-07-16: M3 is COMPLETE. M4 in progress. Items 1–2 of the near-term
> list are DONE, and the classifier ONNX export (item 3's one blocker) is done.**
> The security/tool spine is finished and adversarially reviewed; the Q8
> Permissions pane and the frontend housekeeping have now landed. The trained
> privacy classifier is exported to ONNX (fp32 + INT8, ~96 MB INT8) and preserved;
> what remains of item 3 is the in-Rust `ort` wiring + `gate.rs` renames + the
> redaction sidebar. After that: native tool-use (Q1). The two big unbuilt
> user-facing systems are **memory** and **skills** — both fully designed, zero code.

**Health check (run this before believing anything below; update expected numbers when they change):**

```bash
cd /Users/hayai/Desktop/lost-harness-product/src-tauri && cargo test --lib   # expect: 332 passed, 0 failed
cd /Users/hayai/Desktop/lost-harness-product && npm run build               # expect: clean
cd /Users/hayai/Desktop/lost-harness-product && npm run check               # expect: 0 errors (1 pre-existing tsconfig warning is known noise)
```

Last verified: 2026-07-16 (full checkup — all three green, git tree clean).

---

## Milestone board

| Milestone | What it is | Status |
|---|---|---|
| **M0** — bootstrap | Tauri + Svelte + Tailwind + CI | ✅ **Done** |
| **M1** — vertical slice | message → classify → route → model → stream → save | ✅ **Done + verified** (contract tests at the real IPC boundary) |
| **M2** — UI shell | design system, profiles, command palette | 🟡 **Mostly done** — design-system port landed and wired for chat/sidebar/settings; profile switching works. Superseded components deleted + dev screen-switcher removed (2026-07-16). Remaining gaps: `CommandPalette.svelte` is ported but mounted nowhere; 7 screens are visual-only (see Loose ends). |
| **M3** — tool registry + spine | the whole security/tool foundation | ✅ **Done** (2026-07-16) — all 8 do-now items + approval spine + write/shell/MCP tools, every round adversarially reviewed. Exception: the durability trio's persisted-journal half is deliberately deferred to the first external-effect tool (see PLAN §8 / build plan Q3). |
| **M4** — model manager + skills/agents | native tool-use, seats, usage ledger, budget governor, cache-shaped prompts; skills & agents track | 🔵 **In progress** — Q8 (grant×risk matrix + persisted `tool_rules` + risk-badged dialog) done 2026-07-16. Everything else not started. |
| **Memory system** | curated summary + searchable archive (hybrid FTS5 + sqlite-vec), profile wall, 3-bucket sensitivity routing | 📐 **Designed in full, not built.** Search engine (sqlite-vec) already wired + proven. Design: PLAN §9. |
| **Skills system** | reusable playbooks, approve-first vs autonomous, teacher-escalation | 📐 **Designed in full, not built.** Design: PLAN §10. |
| **Privacy classifier** | rules layer + trained ONNX ensemble + redaction UX | 🟡 **Export done, wiring pending** — rules layer (layer 0) is live and is the active classifier. The trained ensemble (layer 1) is **exported** (2026-07-16: both encoders → fp32 + INT8 ONNX, preserved at `~/Desktop/Classifier Model + Install Guide for Claude/onnx-export/`). Remaining: wire them via `ort` into the `classifier/engine.rs` stub. The annotated-redaction sidebar UX (decided, PLAN §11) has no engine behind it until then. |
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
3. **[~] Classifier integration round** — **export DONE 2026-07-16**: ran the bundle's
   `export_onnx.py` (Python 3.11 arm64 venv, torch/transformers/onnx) → both encoders
   exported to fp32 + INT8; artifacts preserved at
   `~/Desktop/Classifier Model + Install Guide for Claude/onnx-export/` (INT8 ~96 MB).
   **Remaining:** load them via `ort` in `classifier/engine.rs` (mirror the bundle's
   `ensemble.py`/`rules.py`/`serve.py` — rules OR bge OR distilbert, per-model
   `thresholds.txt`); do the deferred `gate.rs`/`PrivacyGate`/"§7" → "privacy filter"
   renames in the same touch; build the annotated-redaction right-sidebar UX (PLAN §11).
4. **[ ] Native tool-use + `Tool::schema()` (Q1, M4)** — per-endpoint capability flag;
   native `tool_use` path for models that support it; fenced dialect stays the fallback;
   fingerprint-parity regression test across transports. Needs a native-tool-capable
   endpoint configured to prove end-to-end.
5. **[ ] Memory system** — the biggest missing user-facing capability. Design complete
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
