# Lost Harness Product — Agent Handoff

**Repo**: `/Users/hayai/Desktop/lost-harness-product/` (Tauri 2.0 + Svelte 5 + Rust), branch `main`, working tree clean.
**Electron prototype (reference only, abandoned)**: `/Users/hayai/Desktop/lost-harness-app/` — read-only UX reference. Do NOT build new features here.
**Spec source**: `/Volumes/SSD-Nas/Obsidian/Obsidian/lab/Projects/lost-harness-product/` (architecture.md, planning.md, spec.md, milestones.md) — the original binding spec. Where it disagrees with `docs/PLAN.md`, **PLAN.md wins**.

**Read this first, in this order:**
1. This file — current state, what's next, gotchas.
2. [`docs/ROADMAP.md`](docs/ROADMAP.md) — the **stage tracker / status board**: what milestone we're at, what's left in what order, what's blocked. When Lukas asks "where are we," answer from there. Keep it updated as you land work.
3. [`docs/PLAN.md`](docs/PLAN.md) — the **source of truth**. Everything decided lives here: what the product is, the architecture, the build order, the open decisions. Now includes full Memory system and Skills system sections.
4. [`docs/codebase/README.md`](docs/codebase/README.md) — the **code-as-it-actually-is guide**: architecture map, one doc per subsystem (with `file:line`), the cross-cutting load-bearing invariants, how-to-run/test, toolchain gotchas, and a watch-items list. Read this when you're about to *change code* (PLAN is the design; this is the implementation).
5. [`docs/server-companion.md`](docs/server-companion.md), [`docs/tooling-and-skills.md`](docs/tooling-and-skills.md), [`docs/argos-review.md`](docs/argos-review.md) — deeper reasoning behind specific PLAN.md decisions. Read these when you need the "why," not the "what."

---

## Project status

This is the **real product** — a Rust/Tauri/Svelte rewrite per the spec. The Electron app was a prototype to validate UX decisions; it's now a read-only reference. All new work goes in the Tauri project.

**Current milestone:** **M3 is COMPLETE; M4 has begun** (its first item, Q8, landed 2026-07-16). M0 and M1 are done and verified. Everything is committed to `main` — there is nothing uncommitted or in-progress to pick up. The live stage tracker is [`docs/ROADMAP.md`](docs/ROADMAP.md).

| Subsystem | Status |
|---|---|
| M0 — project bootstrap (Tauri + Svelte + Tailwind + CI) | Done |
| M1 — the core loop end-to-end (message → privacy-filter classification → route → model → stream → save) | Done + verified at the real Tauri IPC boundary by a contract-test suite |
| M3 spine — tool registry (filtered per body) + the unified "one-gate" hook chain | Built |
| M3 round 1 — the spine is now LOAD-BEARING | **Done.** A live conversation can call a tool: fenced tool-call dialect + "parse only your own output" rule (now type-enforced via `OwnOutput` newtype), untrusted-output guard-wrapping, a `ToolDispatcher` that runs every call through the hook chain before executing, three read-only workspace-confined filesystem tools, and the agentic tool loop wired into `AgentLoop`. |
| M3 — read-before-write guard (blind-clobber protection) | **Done** (2026-07-15, commit `5724f73`). Adversarially reviewed → 4 fixes. |
| M3 — OwnOutput newtype (item 1) | **Done** (commit `14c7122`). `parse_tool_calls` now takes `&OwnOutput` — the "parse only the model's own current-turn output" rule is a compile-time fact, not a doc comment. |
| M3 — Tool-call budgets + repeat detection + deny-cascade (item 2) | **Done** (commit `af2226d`). Per-turn ceiling (8), per-run ceiling (50), repeat detection (threshold 3), deny-cascade (user-deny only, Safe reads exempt). `begin_run()` resets per user message. |
| M3 — Protected-paths always-Ask floor hook (item 3) | **Done** (commit `d13d71a`). `ProtectedPathHook` between SandboxHook and PermissionHook; forces Once-only Ask for `.git/`/`config/secrets`/`.env`/`.ssh/` regardless of policy. `covers_once` on ApprovalLedger. |
| M3 — tool_audit table + PostToolUse observer (item 5) | **Done** (commit `f72a7f9`). Append-only `tool_audit` table in per-profile DB (migration v2). `AuditWriter` trait + `StorageAuditWriter` + `AuditObserverHook`. `dispatch()` fires one audit row per call on every return path. |
| M3/M4 — Q8: grant×risk matrix + persisted per-profile `tool_rules` + risk-badged dialog | **Done** (2026-07-16, `06826ca`→`a651002`, 6 commits). First Part-2 item. `resolve_grant` = single server-side matrix enforcement (Dangerous→Once-only structural, invariant #8); per-profile SQLite `tool_rules` read live; risk-badged dialog (matrix-legal buttons only); `ctx.policy_allowed` makes "Always allow" bypass first-use (zero prompts). Reviewed clean (1 LOW reconciled) + dialog visually QA'd. Follow-up: the Settings "Permissions" revoke pane. |
| M3 — Crash-recovery boot pass + tool.interrupted event (item 4) | **Done** (commit `8fe04aa`). On app launch, terminalizes any conversation left mid-tool-call by writing a `role="tool"`, `error="interrupted_by_crash"`, `aborted=true` repair row. Idempotent. `contains_open_tool_fence` pure check. "No half-durability" doc in `approval.rs`. |
| Frontend — design-system port + backend wiring | **Done** (2026-07-15). Sidebar, MainScreen (chat loop + routing badge), and Settings wired to real backend. Routing-badge fix: `send_message` returns real `routing_decision` from the persisted row (commit `7ecf2d8`). Email/Files/Whiteboard/Scheduled-jobs/Editor/Onboarding/EmptyState still visual-only. |
| sqlite-vec (semantic memory search engine) | Wired + proven — registered on every DB open, a smoke test does a real nearest-neighbour query |
| Memory system (hybrid keyword+meaning search, curated summary + archive) | **Designed in full, not built.** See PLAN.md §"Memory system." |
| Skills system (reusable playbooks, approve-first vs. autonomous) | **Designed in full, not built.** See PLAN.md §"Skills system." |

**Tests:** `cargo test --lib` → **339 passing**, 0 failed. Frontend `npm run build` + `npm run check` clean.

**2026-07-16 (latest): near-term items 1–3-core DONE — the trained privacy classifier is LIVE.** Three landings this session: (1) the Q8 **Permissions pane** (`f38fd2c`) — Settings tab lists/revokes persisted "Always allow" `tool_rules`, verified live in the browser preview; (2) **frontend housekeeping** (`6dfcf12`) — deleted the 5 superseded components, removed the dev screen-switcher + theme toggle, and fixed the `ModelPicker` name collision (options now carry a composite `providerId::name` key — verified live with two `default` models); (3) the **classifier ONNX integration** (`283789b`) — ran the bundle's `export_onnx.py` (both encoders → fp32 + INT8), then wired `classifier/engine.rs` to run the real INT8 ensemble via `ort`, mirroring `serve.py` (rules layer-0 short-circuit → sliding-128-window max-prob over both encoders → fusion at 0.5/0.05). Behind a default-on `onnx-classifier` feature (rules-only fallback with `--no-default-features`, both build clean). Models installed live at `~/Documents/Lost-Harness/models/classifier/` (98 MB, NOT in git). **Gotcha caught:** the exported `tokenizer.json` bakes in Fixed(128) padding/truncation — left on, it feeds the model garbage (distilbert scored 0.999 on "capital of France"); disabled on load. An env-gated parity test (`LHP_CLASSIFIER_MODELS_DIR`) loads the live INT8 models and matches the Python reference probs (`docs/classifier-parity.json`). **Remaining in item 3:** the annotated-redaction sidebar UX (PLAN §11 — the engine's there, no UI); the `gate.rs`/§7 cosmetic renames were judged low-value/high-churn and deferred (gate.rs cleanly delegates to the `Classifier` trait, no rewrite needed).

**2026-07-16 (continued — same session): the "why" sidebar + the memory foundation.** Two more landings after the classifier went live: (4) the **annotated "why" routing sidebar** — `explain_classification` IPC (`9bff6c2`: label + spans with char offsets, category, friendly label, hard-block flag, layer; `tauri.ts` wrapper; 3 tests) + MainScreen's routing panel wired to it (`914ac74`): the last user message renders with detected spans marked inline (amber soft / red hard-block, sliced over a code-point array so multi-byte chars don't shift marks) + a "what tripped the guard" legend; browser-QA'd end-to-end with a temporary dev seed (email/SSN amber, "confidential" red hard-block), seed reverted. This is PLAN §11 decisions c+d done. (5) the **memory-system storage foundation** (`3ee9790`, item 5 / PLAN §9) — global migration **v2**: sensitivity buckets with the **private-local bucket in a physically-separate table** (`memory_facts_private`) so a cloud search never even queries it (the "separate store, not a filtered view" guarantee), FTS5 external-content keyword indexes over both stores (trigger-synced), and `pinned` for the curated summary; `GlobalDb` API — `MemoryBucket`, `insert_memory_fact_in`, `search_memory(query, allow_private, limit)` (bm25-ranked, private index touched only when `allow_private`), `curated_summary`, `set_memory_pinned`, `fts_match_expr` (injection-safe); 3 tests incl. the structural private-exclusion. **`MemoryFact` gained a `pinned: bool` field** (update any external constructors). **Remaining memory work:** the sqlite-vec meaning lane (needs a local embedder — the classifier's bge is a *classification* head, not an embedder, so a separate small embed model is the open choice), write triggers (agent-loop), relevance-gated injection + pinned search tool, non-silent memory events, walled-profile DB routing, UI. **Item 4 (native tool-use) is the next roadmap item but is blocked on configuring a native-tool-capable model endpoint.**

**2026-07-16 (later): Q8 — the FIRST Part-2/M4 item — is COMPLETE + reviewed + visually QA'd.** Grant-scope × risk matrix + persisted per-profile `tool_rules` + risk-badged approval dialog — the item items 7 & 8 both explicitly deferred to (External/Dangerous were getting the *same* `Ask` as Write; only a narrow dispatch hack held the Dangerous line). Shipped in 6 commits (`06826ca` matrix core → `79fe853` risk-badged dialog → `28ac84e` storage → `385c045` read path → `1cd79b5` write path → `a651002` list/revoke IPC), preceded by a full spec (build plan "Part 2A") and a 4-lens design critique. `resolve_grant` is now the single server-side enforcement of the matrix (invariant #8 structural — Dangerous can never get a standing grant); persisted `Always` = per-profile SQLite rules read live; the dialog badges risk and offers only matrix-legal buttons; `ctx.policy_allowed` makes "Always allow" bypass first-use confirm (floor-safe) so it means ZERO prompts. 315 → 332 tests, adversarial code review clean (1 LOW doc-nit reconciled), dialog screenshotted live for all 3 risk states. **One follow-up:** the Settings "Permissions" pane (backend `list_tool_rules`/`delete_tool_rule` exist; the UI to list/revoke doesn't yet). See build plan "Part 2A" + the Q8 progress-log row.

**2026-07-16 (earlier): ALL 8 tool-system do-now items are COMPLETE + reviewed.** Items 6 (`NeedsLocalReroute` + loop reroute, `e03999b`), 7 (guarded executor + `shell_exec`, `bd20f38` — real macOS Seatbelt verified working on this machine), and 8 (MCP into the registry, `e63bca8`) all landed, then a 4-lens adversarial review found 6 real defects, all fixed (`ad87971`, 315 tests). The earlier round's 3 findings + LOW routing cleanup are also all fixed (269 → 278, commit `a73f43c`). See the build plan's "⚠ Review findings" sections and the Progress-Log narrative. The one deliberately-not-fully-fixed item: a `setsid()`-detached shell_exec descendant escapes the timeout group-kill but stays Seatbelt-confined (bounded runaway, documented; durable fix = VM isolation).

**Tool-system build plan:** `docs/tool-system-build-plan.md` is the executable build bible. **All 8 Part-1 do-now items are done, plus Q8 (the first Part-2 item).** What remains of it is the rest of Part 2 (M4/later pointers: Q1 native tool-use, Q11 permission modes, Q3 durability journal, Q6 reroute UX, Q5 headless approval queue). Read its "How to use this doc" header and the Progress Log at the bottom for the full trail.

---

## What "M3 spine" actually means (read this before touching tools/hooks code)

Two things landed in commit `f9223c9`:

1. **The tool registry** — a `Capability` enum (Filesystem, Network, Shell, Display, Audio, ComputerUse, Email, Calendar, WebResearch, LongCompute) plus a `Tool` trait plus a registry that filters which tools are even offered based on what the running "body" (the desktop app vs. a future headless server) can actually provide. A tool that needs a screen simply isn't offered on the headless server — the agent is told why instead of the tool failing at call time.
2. **The unified "one-gate" hook chain** — every tool action a real tool takes will pass through one ordered chain of checks: `[PrivacyFilter, Sandbox, Permission, FirstUseConfirm]`. First "no" wins. This replaces four previously-scattered gates with one auditable place. Two things worth knowing about it:
   - The **privacy filter step is wrapped, not rewritten** — it calls the existing gate logic verbatim, so nothing about privacy behavior changed, it's just now a step in a chain instead of its own standalone thing.
   - It includes a hard **"must-not-leave-this-host" rule**: if a request is flagged sensitive, it structurally cannot fail over to a cloud model even under pressure (no local model available, etc.) — it fails loudly instead of silently going to the cloud.
   - It includes an **immutable hardline danger-blocklist** (`rm -rf /`, `curl | sh`, credential exfiltration, and similar) that nothing — no setting, no future "just let it run" mode — can disable.

**Round 1 changed this — the chain is now load-bearing (commit for round 1 is the most recent on `main`).** A live conversation *can* call a tool now. What round 1 added on top of the spine above:

- **`tools/calling.rs`** — the fenced tool-call dialect (```` ```tool ```` blocks) and `guard_wrap`. `parse_tool_calls` is only ever fed the model's own current-turn output (the "parse only your own output" rule, enforced at the `AgentLoop` call site), so read content can't forge a call. `guard_wrap` fences untrusted tool output with a nonce and neutralizes backticks so a forged block can't survive an echo.
- **`tools/fs.rs`** — `read_file` / `list_dir` / `search_files`, all `Capability::Filesystem`, all confined to a `workspace/` dir under the storage root (rejects `..`, absolute paths, and symlink escapes via canonicalize).
- **`tools/dispatch.rs`** — `ToolDispatcher`: resolves the call in the registry → checks env availability (refuse-with-reason) → runs the **hook chain** → executes. `run_turn` parses the model's output, dispatches, and returns the guard-wrapped feedback message. This is the junction that makes the registry + chain load-bearing.
- **`agent/loop_mod.rs`** — `stream_to_provider` is now a bounded agentic loop (stream a turn → run its tool calls → feed guard-wrapped results back → repeat, ≤6 rounds).
- **Wiring** in `lib.rs` (`build_tool_dispatcher`): tools registered against `BodyEnv::app_default()`, behind `build_pretooluse_chain_with_confirmed`.

**Round-1 decision (made, flag if you disagree):** the three read-only tools are **pre-trusted** — whole-tool `Allow` in the policy *and* pre-marked confirmed in `FirstUseConfirmHook` — because there's no interactive approval UX yet and a workspace-confined read can't mutate anything or leave the box. Any state-changing tool (write/delete/shell/network) will NOT be pre-trusted; it waits for the approval spine (next round).

**Round 1 was adversarially reviewed** (a 4-lens multi-agent pass + verification); it surfaced 3 real issues, all fixed with regression tests before commit: (1) the privacy filter's `LocalRequired` annotation was a silent no-op in tool dispatch — now the dispatcher **fails closed** (blocks a must-stay-local tool call when the conversation is on a cloud endpoint); (2) `guard_wrap` neutralized backticks but not the trust-boundary banner it teaches the model — now both are neutralized; (3) `format_outcome` spliced model-controlled tool names/errors in raw — now all interpolated untrusted text runs through `neutralize_untrusted`.

**Still NOT wired (updated 2026-07-16):** the headless browser, delegate/ask-human/system-status/cron/session-search tools, and the persisted-journal half of the durability trio (deliberately deferred to the first external-effect tool). Everything else once listed here has since shipped: write/delete tools, the approval spine, `shell_exec` (Seatbelt-sandboxed), MCP-into-registry, and reroute-to-local plumbing (`NeedsLocalReroute`).

---

## Toolchain gotchas (read before running anything)

A fresh session needs these or it will lose time rediscovering them:

- **The platform-pin bug is fixed.** `package.json` used to hardcode x64-only native bindings (rolldown, tauri CLI, etc.) even though this Mac's Node is arm64, which broke builds. That's fixed — `package.json` now lets npm resolve the binding per-platform, and arm64 native bindings work normally. You should not need any node_modules workaround.
- **The Rust toolchain on this machine is x86_64, not arm64-native.** The Homebrew-installed cargo/rustc identify as `x86_64-apple-darwin` and run under Rosetta. Builds and tests work fine this way — it's just translation, not a blocker. Installing a real arm64 Rust toolchain (via rustup, targeting `aarch64-apple-darwin`) would be a nice cleanup someday, but nobody needs to do it to keep working.
- **Node itself is arm64.** So: Rust runs under Rosetta, Node runs native. Both work; just don't be surprised the two toolchains report different architectures.
- **Run cargo from `src-tauri`, not the repo root.** The Rust project lives there.
- **The shell resets its working directory between commands.** Use absolute paths, or `cd` inside a single compound command — don't assume `cd` from a previous command persists.

**How to verify the whole thing is healthy:**
```bash
cd /Users/hayai/Desktop/lost-harness-product/src-tauri && cargo test --lib   # expect: 332 passed
cd /Users/hayai/Desktop/lost-harness-product && npm run build               # frontend build, should be clean
cd /Users/hayai/Desktop/lost-harness-product && npm run check               # svelte-check, should be clean
```

---

## What's next (in order)

> **The ordered, maintained version of this list now lives in [`docs/ROADMAP.md`](docs/ROADMAP.md)** — start there. The short version (2026-07-16): ① Settings "Permissions" pane (small, do first) → ② frontend housekeeping (delete superseded components, dev switcher, ModelPicker collision) → ③ classifier integration round (run the bundle's `export_onnx.py` — the ONNX ensemble is blocked on that one action, not on code — then wire `engine.rs`, do the deferred `gate.rs` renames, build the annotated-redaction sidebar) → ④ native tool-use (Q1) → ⑤ memory system → ⑥ rest of M4 → ⑦ remaining core tools. Detail below is kept for context.

1. **M3 do-now items 1–8 AND the first Part-2 item (Q8) are DONE.** Q8 (grant×risk matrix + persisted per-profile `tool_rules` + risk-badged dialog) landed 2026-07-16 in 6 commits and passed a 4-lens adversarial review (1 LOW, reconciled) + a live dialog visual QA. Full spec is the build plan's **"Part 2A"** section. Remaining **Part 2 (M4 / later)** items:
   - **Q8 follow-up — the Settings "Permissions" pane:** the backend `list_tool_rules`/`delete_tool_rule` commands + `tauri.ts` wrappers exist; a Settings tab to list persisted "Always allow" rules and revoke them is the one piece of Q8 not yet built. Small, self-contained.
   - **Native tool-use + `Tool::schema()` (Q1):** per-endpoint capability flag; both transports normalize to `ToolCall`; native results still guard-wrapped; a fingerprint-parity-across-transports regression test (the fenced dialect stays the fallback). Needs a native-tool-capable endpoint to prove end-to-end.
   - **`UserPromptSubmit` hook + permission modes (plan / accept-edits) (Q11):** designed against Q8's matrix so a mode can never widen `External`/`Dangerous`.
   - **The durability trio** (deferred per Q3): persisted action journal + idempotency keys — moves to the first non-idempotent external-effect tool (email/calendar/delegate — M7/server track).
   - **Reroute auto-switch UX** (M4, Q6): toast styling, model-manager-first-class-endpoint object — the plumbing from Item 6 ships now, the UX ships in M4.
2. **Finish wiring the ported frontend.** The design-system port (2026-07-15, commits `a22855e`/`55ad9d5`) is visually complete and wired for chat/sidebar/providers; near-term cleanup and gaps surfaced by that work:
   - **Delete the superseded old components** — `src/lib/components/{Sidebar,ChatPanel,ModelPicker,PrivacyIndicator,ProviderSettings}.svelte` are unused now that `App.svelte` renders from `src/lib/design/`. `ApprovalDialog.svelte` in the same folder is still used (backend-driven) — keep it.
   - **Remove the dev floating screen-switcher + theme toggle** in `src/App.svelte` — it's a QA aid, meant to go once real nav cross-links between screens are wired.
   - **Fix the `ModelPicker` flat model-name namespace** — two providers exposing an identically-named model collide today (only the last-registered is addressable).
   - **Wire the remaining visual-only screens** (Email, Files, Whiteboard, Scheduled-jobs, Editor, Onboarding, EmptyState) to real backends as those subsystems land — they currently render sample data only.
3. **Build the memory system.** Full design is in `docs/PLAN.md` under "Memory system" — the short version: a small always-loaded "curated summary" plus a full searchable "archive," combining keyword search (already-bundled FTS5) and meaning search (sqlite-vec, already wired) into one hybrid search.
4. **Build the skills system.** Full design in `docs/PLAN.md` under "Skills system" — reusable playbooks, with a per-profile toggle for whether the agent can teach itself new ones or has to ask first.
5. **The remaining from-scratch gaps** (things with no equivalent in the reference material we studied, ours to build unassisted): computer/screen control (M5), voice (M6), and local-model lifecycle — detecting the user's hardware, offering a curated model catalog, downloading/verifying models (M8).
6. **The server-companion track** — the optional always-on "second brain" add-on. Starts once M4 lands; design is fully resolved (see `docs/PLAN.md` §5), nothing left to decide, just to build.

---

## Memory-sharing decision — DECIDED 2026-07-08

The last open product decision ("shared vs. walled memory across profiles?") is now settled: **a per-profile toggle, and a walled profile gets its own separate memory database.** There are no product decisions left waiting on Lukas.

- **Default: shared.** Facts live in `global.db`, each tagged with the profile it came from — one coherent assistant that remembers you across every profile.
- **Per-profile "keep this profile's memory private" toggle → full island.** When on, that profile's memory lives in its **own separate, profile-scoped database**, physically apart from the shared pool. It reads nothing from shared memory and writes nothing back.
- **Why a separate database and not a query filter:** the separation must be physical so that switching the toggle off later — or a bug — can never retroactively spill what was written while the profile was private. A filter would leave that data sitting in the shared pool; a separate DB never puts it there. This is the version a genuinely locked-down/regulated user requires, whose need runs **both** directions (no work facts out, no personal context in).
- A one-way variant (a walled profile may *read* shared memory but never write back) is a possible finer setting later, not v1.

Full write-up in [`docs/PLAN.md`](docs/PLAN.md) §7 and §9. **This unblocks the memory milestone's storage schema** — the schema branches on the toggle (shared → tagged rows in `global.db`; walled → the profile's own memory DB).

---

## Pending cleanup (tracked, not urgent)

The `trm/` module was renamed to `classifier/` on 2026-07-09 (the "TRM" Tiny-Recursive-Model approach was evaluated and dropped by the classifier author — see PLAN §11). The `trm_logs` **audit table** kept its name deliberately (renaming a persisted table needs a migration — not worth it).

Still pending: the **code** uses the old names `§7` and `PrivacyGate` in a few places (`agent/gate.rs`, `GateDecision`, doc comments), while the docs say "privacy filter." This rename was deferred to the **classifier-integration round** — that round rewrites `gate.rs`/`engine.rs` to run the real ONNX classifier anyway, so it's one coherent touch of that code rather than a separate rename-only pass.

---

## Architecture (file map)

```
lost-harness-product/
├── src-tauri/
│   ├── Cargo.toml          # tauri 2.11, serde, tokio, rusqlite (bundled), reqwest, sha2, regex, sqlite-vec, etc.
│   ├── tauri.conf.json     # window "Lost Harness" 1280x800
│   └── src/
│       ├── main.rs         # bin entry, calls lib::run()
│       ├── lib.rs          # Tauri builder, state management, IPC handler registration
│       ├── agent/          # the privacy filter (still named "gate"/"§7" in code) + agent loop
│       │   ├── gate.rs     # PrivacyGate, Binding {Auto,Public,Private}, GateDecision {Allow,Block,RouteLocal}
│       │   ├── egress.rs   # is_private_endpoint() — detects private-network destinations
│       │   ├── loop_mod.rs # AgentLoop — message→classify→gate→model→stream→[tool loop]→persist
│       │   └── *_tests.rs
│       ├── classifier/     # labels a message Private/Public/Uncertain (was "trm/")
│       │   ├── heuristic.rs # regex-based PII detector (SSN, credit card, API keys, health, etc.) — active today
│       │   └── engine.rs   # EnsembleClassifier — stub for the real trained model (bge+distilbert ONNX), not wired
│       ├── models/         # model manager — OpenAI-compatible HTTP client, provider config, SSE streaming
│       ├── storage/        # SQLite: global.db (shared) + one profiles/<name>.db per profile
│       │   ├── schema.rs   # every table definition, incl. memory_facts / memory_vectors (still a raw-BLOB placeholder) / skills
│       │   └── migrations.rs
│       ├── tools/          # M3: registry + Tool trait; calling.rs (fenced dialect + guard_wrap),
│       │   │               #     fs.rs (read_file/list_dir/search_files), dispatch.rs (ToolDispatcher)
│       ├── hooks/          # M3: the unified gate chain [PrivacyFilter, Sandbox, Permission, FirstUseConfirm]
│       ├── ipc/            # Tauri command handlers + AppState
│       ├── platform/       # computer-use stubs, one submodule per OS (M5, not built)
│       └── audio/          # voice stub (M6, not built)
├── src/                    # Svelte 5 frontend — app entry is /app.html (NOT /)
│   ├── App.svelte          # renders the current screen from the nav store; hydrates profiles→providers+conversations on mount; DEV floating screen-switcher + theme toggle (QA aid, remove later)
│   ├── lib/
│   │   ├── design/         # PORTED design system (2026-07-15) — Svelte 5, Tailwind-translated from ~/Desktop/lost-harness-ui; this is the current frontend
│   │   │   ├── components/ # 37 .svelte components (Sidebar, MainScreen's chat bits, RoutingBadge, etc.) + knot-geometry.ts
│   │   │   ├── screens/    # 9 .svelte screens + shell-data.ts — MainScreen/Settings wired to backend; Email/Files/Whiteboard/ScheduledJobs/Editor/Onboarding/EmptyState still sample data
│   │   │   ├── types.ts    # Route/Binding/ScreenId
│   │   │   ├── nav.svelte.ts # runes-based screen-router store
│   │   │   └── CONVENTIONS.md # the porting rules (Tailwind translation, not a CSS import)
│   │   ├── stores/         # chat.ts (extended for routing_decision/model/provider_id + bindingOverride), providers.svelte.ts, profiles.ts, settings.ts — real backend-backed stores
│   │   └── components/     # SUPERSEDED old hand-built UI (Sidebar/ChatPanel/ModelPicker/PrivacyIndicator/ProviderSettings.svelte) — unused, no longer imported, pending deletion; ApprovalDialog.svelte here is still used (backend-driven)
│   └── app.css              # Tailwind v4 @theme inline — design tokens mapped in (colors only; radii/shadows use arbitrary values), + global .lh-range slider CSS
├── docs/
│   ├── PLAN.md              # SOURCE OF TRUTH — read this
│   ├── server-companion.md  # deep design reasoning for the optional server
│   ├── tooling-and-skills.md # deep design reasoning for tools/skills/hooks (claude-code-inspired)
│   └── argos-review.md      # review of a third-party reference project, inspiration notes
└── .github/workflows/build.yml # CI matrix (mac/win/linux)
```

---

## How to run

```bash
cd /Users/hayai/Desktop/lost-harness-product

# Frontend dev (browser, no Tauri)
npm run dev                # Vite dev server, open localhost:1420

# Full Tauri dev
npm run tauri dev          # Tauri + Vite HMR

# Build
npm run build                 # Frontend only
cd src-tauri && cargo build   # Rust only

# Test
cd src-tauri && cargo test --lib   # 332 tests
npm run build                      # Frontend compile check
npm run check                      # svelte-check

# CI
# .github/workflows/build.yml — mac/win/linux matrix
```

---

## Key design decisions (durable, unlikely to change)

1. **Two codebases.** Electron app (`lost-harness-app`) is a read-only reference, abandoned. Tauri project (`lost-harness-product`) is the real product. All new work in Tauri.
2. **Rust core + Tauri 2.0 + Svelte 5 (runes) + Tailwind.** No Electron, no Python runtime in the shipped product.
3. **Two-database SQLite architecture:** `global.db` (shared: endpoints, memory, skills, settings) + `profiles/<name>.db` (per-profile: conversations, messages, folders, tags, classifier logs).
4. **The privacy filter is load-bearing, not cosmetic.** Every place the app calls out to any model — a chat reply, a background summary, a memory pass, an embedding — is checked first. Full behavior detail is in PLAN.md §1 and §4.
5. **One Rust core, two possible "bodies."** The exact same core compiles into the desktop app and (later) an optional headless server companion; each just offers a different set of capabilities to the same tool registry. See PLAN.md §2.
6. **Heuristic classifier is the active fallback** until a real trained classifier model arrives. The classifier is defined as a swappable interface, so the real model drops in without touching the privacy filter itself.

---

## Session log — 2026-07-08

Picked up from the 2026-07-07 handoff (M1 frontend rewiring landed and committed).

- **Reviewed, fixed, and committed M1.** Confirmed the core loop (message → privacy-filter classification → route → model → stream → save) is proven end-to-end at the real Tauri IPC boundary by a contract-test suite, not just on paper.
- **Built and committed the M3 spine** (`f9223c9`): the tool registry (Capability + Tool trait + per-body filtering) and the unified one-gate hook chain, including the hard local-only routing rule and the immutable danger-blocklist. Not yet wired into live tool dispatch — no real tools exist yet.
- **Wired and proved sqlite-vec** (`7172d01`): the semantic-search half of memory is now a real, tested dependency, registered on every database connection. The keyword half (FTS5) was already available via bundled SQLite. `memory_vectors` is still a placeholder table — turning it into the real hybrid-search system is the memory milestone, not yet done.
- **Consolidated the docs and renamed "privacy gate/§7" to "the privacy filter"** throughout the prose docs (the code still uses the old names — see "Pending cleanup," above).
- **Designed the memory system and the skills system in depth**, in conversation — now folded into `docs/PLAN.md` as full sections (see this repo's PLAN.md for the complete design).
- **Resolved the remaining server/privacy open decisions**: the baton/multi-device model, per-profile opt-in server sync, and product-owned pairing/authentication (not dependent on Tailscale or any specific network) — all captured in PLAN.md §2, §5, §7.
- **Resolved the last open product decision — memory sharing across profiles.** A per-profile toggle: shared by default (tagged rows in `global.db`), and a walled profile gets its **own separate memory database** (full island, physical separation) rather than a filtered view — so switching the toggle off can never retroactively leak. The memory milestone's storage schema is now unblocked; it branches on the toggle. See PLAN §7/§9 and the decision section above.

## Session log — 2026-07-09

Ratified the memory-toggle decision (committed `docs:` on `main`), then ran **M3 round 1 — made the tool spine load-bearing.**

- **Built the tool-calling substrate** (`tools/calling.rs`): the fenced ```` ```tool ```` dialect for small local models, `parse_tool_calls` (fed only the model's own turn — the "parse only your own output" rule), and `guard_wrap` (nonce-delimited untrusted-output fencing + backtick neutralization so a forged block can't survive being echoed).
- **Built three read-only filesystem tools** (`tools/fs.rs`): `read_file` / `list_dir` / `search_files`, `Capability::Filesystem`, confined to a `workspace/` dir (rejects `..`, absolute paths, symlink escape via canonicalize).
- **Built the `ToolDispatcher`** (`tools/dispatch.rs`): resolve → env-availability (refuse-with-reason) → **hook chain** → execute; `run_turn` parses the model's output, dispatches, returns guard-wrapped feedback. This is the junction that makes the previously-inert registry + chain load-bearing.
- **Wired the agentic loop** into `AgentLoop::stream_to_provider` (bounded ≤6 tool rounds) and into app state (`lib.rs::build_tool_dispatcher`), adding `build_pretooluse_chain_with_confirmed` so read-only tools ship pre-trusted.
- **Tests:** 151 → **171** (+20: dialect, guard-wrap, fs path-safety, gated dispatch incl. "a sandbox-denied call never runs the tool"). Clippy: no new real issues; frontend clean.
- **Decision made (not blocking):** read-only workspace-confined tools are pre-trusted (no first-use prompt); state-changing tools will require the approval spine (next round).
- Ran an adversarial multi-agent review over the round-1 diff before committing (security + correctness lenses, each finding verified).

**Later the same day — the real privacy classifier arrived** (external collaborator; bundle at `~/Desktop/Classifier Model + Install Guide for Claude/`). It's a rules + bge-small + distilbert ONNX ensemble, CPU-only (~25–80 ms, ~100 MB), explicitly built for our Tauri/Rust/ONNX stack (Python stays training-side). It unblocks the `engine.rs` stub. Recorded the full assessment + integration path in PLAN §11 and the [[lost-harness-privacy-classifier]] memory. Lukas decided its UX (§11): adopt span redaction/partial delegation, a dedicated classifier settings page, and a non-blocking "message censored" alert with a button that opens the right sidebar showing the original text annotated by what tripped the filter. Also renamed `trm/` → `classifier/` (TRM approach was dropped).

**Classifier integration — increment 1 done:** the deterministic **rules layer (layer 0) is ported to Rust** (`classifier/rules.rs`) and is **now the active classifier** (`RulesClassifier` wired in `lib.rs`). It's a strict superset of the old heuristic's hard detectors + adds span offsets, obfuscation handling (unicode-digit folding, letter-swap), and confidentiality-cue/PROPRIETARY detection. **Behavior change worth knowing:** it's recall-biased (more false-positives → more messages routed local) vs. the old heuristic's low-FP bias — the safe direction for a privacy filter, and one-line reversible if it's too aggressive. `Classification` gained a `spans` field (feeds the annotated-redaction UI). 194 tests. **Still blocked:** the ONNX ensemble (layer 1) needs the exported `.onnx` files (see the ONNX-artifact note above).

**Approval spine — BUILT end-to-end (3 commits).** Interactive tool confirmation now exists:
- `hooks/approval.rs` — `ActionFingerprint` (sha256 of tool + canonical args, the anti-drift pin), `ApprovalLedger` (Once/Session grants; Always→Session until a persistent policy store lands), `ApprovalPrompter` trait.
- `permission.rs`/`first_use.rs` are ledger-aware; first_use no longer self-marks on ask ("asked" ≠ "approved" — an unattended agent can't self-grant).
- `tools/dispatch.rs` — bounded pause→ask→grant→re-run loop; the re-run always re-checks the Sandbox floor; deny/timeout fail closed; `consume_once` ties a one-time grant to one execution.
- `ipc/approval.rs` + `resolve_tool_approval` command — `TauriApprovalPrompter` emits `tool:approval_request`, awaits a oneshot with a 5-min deny-by-default timeout; the resolve command touches only the registry (no stream-lock ⇒ no deadlock).
- Frontend: `ApprovalDialog.svelte` + the `tauri.ts` bridge (Deny / Allow once / Allow for this session).
- 208 lib tests; svelte-check + frontend build clean. Adversarially reviewed (4 lenses + verification): 8 confirmed findings, all fail-closed (no invariant broken). Fixed 5 — the prompt now shows the tool's args (not just its name); a Once grant blocked by the routing floor is now consumed (was staying armed); `grant` no longer widens Once+Tool into a session grant; "Allow once" is the primary button (was the broadest); the dialog handles a stale/expired click. Documented + deferred (fail-closed): a nanosecond timeout-vs-resolve TOCTOU race, and the stream-lock-held-across-the-wait single-in-flight constraint (needs a concurrency refactor + cancel command).

**Update 2026-07-10 — write tools SHIPPED + reviewed.** `write_file`/`edit_file`/`delete_file` are built, workspace-confined, and gated by a `RiskClass` that *derives* the policy (Safe→pre-trusted, Write→Ask through the approval spine); an end-to-end test proves an approved write actually writes. So the approval dialog now fires for real. Adversarially reviewed → 2 correctness findings fixed (silent symlink clobber; temp-file leak on disk-full). Also fixed a **blank-GUI bug** the first real `npm run tauri dev` surfaced: the window loaded `/` (404) instead of the app's `app.html` entry — fixed via `windows[0].url = "app.html"` (the M1 "eyeball the GUI" gap). GUI needs a real design pass — **Lukas is actively speccing the frontend mockup (2026-07-10); UI work waits for that spec — don't freelance the interface.**

**Next, per the Claude Code parity check (PLAN §12):** (1) **read-before-write** (high, next — refuse to write/edit a file not read this session); (2) **native tool-use when the endpoint supports it** (high, M4 — fenced dialect stays the fallback); (3) protected-paths always-prompt floor; (4) permission modes + `UserPromptSubmit` hook. Then the remaining core tools (browser/delegate/ask-human/system-status/cron/session-search), the durability trio, reroute-to-local, and MCP-into-registry. A **visual QA pass on the approval dialog** (live `npm run tauri dev`) is worthwhile once a model is configured to trigger a write.

## Session log — 2026-07-15 (tool-system rounds)

Orchestrator session: Zed (GLM-5.2) directed MiniMax M3 subagents for items 1–5 of the tool-system build plan, plus a routing-badge quick win. All verified against actual source before commit.

- **Routing-badge fix** (`7ecf2d8`): `send_message` returns real `routing_decision` from the persisted assistant row instead of hardcoded `"allow"`. Backend-only, 226 tests.
- **Item 1 — OwnOutput newtype** (`14c7122` + `2cac2c2`): `parse_tool_calls` and `run_turn` take `&OwnOutput`; the "parse only your own current-turn text" rule is now a compile error, not a doc comment. 226 tests.
- **Item 2 — Budgets + repeat detection + deny-cascade** (`af2226d` + `c21b058`): Per-turn ceiling (8), per-run ceiling (50), repeat detection (threshold 3, exact reason strings), deny-cascade (only `by:"user"` triggers, Safe reads exempt). `begin_run()` resets per user message. Mutex guard never held across `.await`. 234 tests.
- **Item 3 — Protected-paths floor hook** (`d13d71a` + `a47a591`): `ProtectedPathHook` between SandboxHook and PermissionHook in all three chain constructors. Forces Once-only Ask for `.git/`/`config/secrets`/`.env`/`.ssh/`. `covers_once` on ApprovalLedger ignores session/tool grants. Forced-Once piggyback in dispatch's Approve arm. 245 tests.
- **Item 5 — tool_audit + PostToolUse observer** (`f72a7f9` + `23293c8`): Append-only `tool_audit` table (per-profile, migration v2, `PROFILE_SCHEMA_VERSION` split from `GLOBAL_SCHEMA_VERSION`). `AuditWriter` trait + `StorageAuditWriter` + `AuditObserverHook`. `dispatch()` fires one audit row per call on every return path. `grant_used`/`decision` left None for now. 258 tests.
- **Item 4 — Crash-recovery boot pass** (`8fe04aa` + `3434059`): `run_boot_pass` at core init terminalizes conversations left mid-tool-call. `contains_open_tool_fence` pure check (never parses JSON). Idempotent by construction. "No half-durability" doc in `approval.rs`. 269 tests.

**Next agent — start here:** Item 6 in the build plan — `NeedsLocalReroute` typed outcome + loop consults `enforce_local_routing`. Spec at `docs/tool-system-build-plan.md` lines 748+. Most structurally invasive remaining item — splits `run_turn`'s return from `Option<ChatMessage>` to a `TurnOutcome` enum. Then items 7 (shell_exec, the big one) and 8 (MCP into registry).



Three commits landed on `main`: read-before-write (backend), then a full frontend design-system port, then backend wiring for that port.

- **Read-before-write SHIPPED** (`5724f73`) — the item flagged "NEXT" in the previous session log. A conversation-scoped read-set, `ConversationReads` (`Mutex<HashMap<conv_id, HashSet<PathBuf>>>`, `src-tauri/src/tools/mod.rs`), is owned by `ToolDispatcher` (`src-tauri/src/tools/dispatch.rs`) and injected into `ExecCtx.reads` at the `tool.run` call site (`AgentLoop` leaves it `None`). In `src-tauri/src/tools/fs.rs`: `read_file` records the canonical resolved path on success; `write_file` (only when the target already *exists*) and `edit_file` refuse if the path isn't in the conversation's read-set; a *new* file and `delete_file` stay exempt; a successful write self-records. **Adversarially reviewed → 4 fixes:** (a) `write_file` now canonicalizes the existing target for the membership check — `resolve_within` (read, canonical leaf) vs. `resolve_within_new` (write, raw leaf) could otherwise disagree on a macOS case-insensitive/Unicode path and falsely refuse a real read→write; (b) `MAX_READ_BYTES` raised to equal `MAX_WRITE_BYTES` (1 MiB), killing a 256 KiB–1 MiB "writable but unreadable" dead zone; (c) write self-records so create-then-overwrite isn't refused; (d) added case-insensitive/subdir/large-file/cross-dispatch regression tests. **Tests: 208 → 226**, 0 failed.
- **UI PORT SHIPPED** (`a22855e`) — the React design source at `~/Desktop/lost-harness-ui` was ported to Svelte 5 under `src/lib/design/`: `components/` (37 `.svelte` + `knot-geometry.ts`), `screens/` (9 `.svelte` + `shell-data.ts`), `types.ts` (`Route`/`Binding`/`ScreenId`), `nav.svelte.ts` (a runes screen-router store), and `CONVENTIONS.md` (the porting rules). **Decision (Lukas):** keep Tailwind — translate the design's plain CSS into Tailwind utility classes rather than importing it; port the whole design system in one pass. Design tokens are mapped into Tailwind v4 `@theme inline` in `src/app.css` (colors only; radii/shadows use arbitrary values like `rounded-[var(--r)]`/`shadow-[var(--shadow-pop)]` to dodge the built-in `rounded-r` collision), staying theme-reactive via `:root` + `:root[data-theme="light"]`. The global `.lh-range` slider CSS also lives in `app.css` (vendor pseudo-elements trip `svelte2tsx` in a scoped `<style>`). `App.svelte` was rewired to render the current screen from the nav store (each screen is self-contained, including its own Sidebar), plus a DEV floating screen-switcher + theme toggle (a QA aid, remove later). **App entry is `/app.html`, not `/`.**
- **BACKEND WIRING SHIPPED** (`55ad9d5`), for the screens that have a backend: `Sidebar.svelte` → real `$conversations` + new-chat (`createConversation`) + profile switcher (`$profiles`/`switchProfile`); `MainScreen.svelte` → the real chat loop (messages from `$activeConversation`, send via `sendMessage`, streaming, model picker from `providersStore`, an Auto/Public/Private binding pill feeding `sendMessage`, and a per-message `RoutingBadge` driven by the real gate decision — un-stubs the old client-side privacy indicator); `Settings.svelte` → Models tab wired to `providersStore` (list/add/remove/select model), Appearance tab wired to the theme store. `src/App.svelte` hydrates profiles→providers+conversations on mount. `src/lib/stores/chat.ts` extended additively: `Message` gained optional `routing_decision`/`model`/`provider_id` (were previously dropped by `msgFromInfo`); `sendMessage` gained an optional 4th `bindingOverride` param — backward-compatible, the old 3-arg `ChatPanel` call still works.
- **Known gaps surfaced, not yet fixed** (see "What's next" above): `ipc::send_message` returns `routing_decision: "allow"` **hardcoded**, so a live send can't yet surface a `route_local` badge from the response (the real decision is persisted, just not returned). `ModelPicker` uses a flat model-name namespace — two providers with an identically-named model collide. The old `src/lib/components/{Sidebar,ChatPanel,ModelPicker,PrivacyIndicator,ProviderSettings}.svelte` are now superseded and unused (`App` no longer imports them) — `ApprovalDialog.svelte` is still used and kept. The dev screen-switcher in `App.svelte` is a QA aid pending removal. **Still visual only, no backend:** Email, Files, Whiteboard, Scheduled-jobs, Editor, Onboarding, EmptyState screens.
- **Tests:** `cargo test --lib` → 226 passing, 0 failed (unchanged by the two UI commits). Frontend `npm run build` + `npm run check` clean.

## Session log — 2026-07-16 (checkup + docs refresh)

Full repo health audit (requested by Lukas: "what's left, what's missing, what's broken"), then a documentation pass so the docs answer that question themselves.

- **Verified health independently** (not just from the docs): `cargo test --lib` → **332 passed, 0 failed**; `npm run build` clean; `npm run check` 0 errors; git tree clean. Every "done" claim in the docs checked out against the actual code and git log. **Verdict: nothing broken; no fires.**
- **Created [`docs/ROADMAP.md`](docs/ROADMAP.md)** — the stage tracker / status board. Milestone board (M0–M10 + memory/skills/classifier/server tracks), ordered what's-left checklist, blocked items, accepted quirks, and instructions for agents on how to report the current stage to Lukas and keep the file current. **This is now the second thing to read** (after this file) and the place to answer "what stage are we at."
- **De-staled this file:** current-milestone line (M3 complete, M4 begun), the "items 6–8 remain" build-plan note, the round-1-era "Still NOT wired" list, test counts (315/226 → 332), and pointed "What's next" at the roadmap.
- **Annotated PLAN.md** §8's round-1 M3 status block with a completion note (M3 done 2026-07-16) and marked the §12 parity gaps that have since closed (protected paths, shell guardrails).
- **Findings worth acting on** (all now in the roadmap): the Settings "Permissions" pane is the one unfinished piece of Q8 (users can grant standing permissions but can't see/revoke them); the ONNX ensemble is blocked only on running `export_onnx.py` from the classifier bundle — an action, not code; `CommandPalette.svelte` is ported but mounted nowhere (M2 leftover).
- No product code touched — docs-only session.
