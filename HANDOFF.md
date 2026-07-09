# Lost Harness Product — Agent Handoff

**Repo**: `/Users/hayai/Desktop/lost-harness-product/` (Tauri 2.0 + Svelte 5 + Rust), branch `main`, working tree clean.
**Electron prototype (reference only, abandoned)**: `/Users/hayai/Desktop/lost-harness-app/` — read-only UX reference. Do NOT build new features here.
**Spec source**: `/Volumes/SSD-Nas/Obsidian/Obsidian/lab/Projects/lost-harness-product/` (architecture.md, planning.md, spec.md, milestones.md) — the original binding spec. Where it disagrees with `docs/PLAN.md`, **PLAN.md wins**.

**Read this first, in this order:**
1. This file — current state, what's next, gotchas.
2. [`docs/PLAN.md`](docs/PLAN.md) — the **source of truth**. Everything decided lives here: what the product is, the architecture, the build order, the open decisions. Now includes full Memory system and Skills system sections.
3. [`docs/server-companion.md`](docs/server-companion.md), [`docs/tooling-and-skills.md`](docs/tooling-and-skills.md), [`docs/argos-review.md`](docs/argos-review.md) — deeper reasoning behind specific PLAN.md decisions. Read these when you need the "why," not the "what."

---

## Project status

This is the **real product** — a Rust/Tauri/Svelte rewrite per the spec. The Electron app was a prototype to validate UX decisions; it's now a read-only reference. All new work goes in the Tauri project.

**Current milestone:** in M3, **round 1 landed** (the tool spine is now load-bearing). M0 and M1 are done and verified. Everything below is committed to `main` — there is nothing uncommitted or in-progress to pick up.

| Subsystem | Status |
|---|---|
| M0 — project bootstrap (Tauri + Svelte + Tailwind + CI) | Done |
| M1 — the core loop end-to-end (message → privacy-filter classification → route → model → stream → save) | Done + verified at the real Tauri IPC boundary by a contract-test suite |
| M3 spine — tool registry (filtered per body) + the unified "one-gate" hook chain | Built |
| **M3 round 1 — the spine is now LOAD-BEARING** | **Done.** A live conversation can call a tool: fenced tool-call dialect + "parse only your own output" rule, untrusted-output guard-wrapping, a `ToolDispatcher` that runs every call through the hook chain before executing, three read-only workspace-confined filesystem tools (`read_file`/`list_dir`/`search_files`), and the agentic tool loop wired into `AgentLoop`. |
| sqlite-vec (semantic memory search engine) | Wired + proven — registered on every DB open, a smoke test does a real nearest-neighbour query |
| Memory system (hybrid keyword+meaning search, curated summary + archive) | **Designed in full, not built.** See PLAN.md §"Memory system." |
| Skills system (reusable playbooks, approve-first vs. autonomous) | **Designed in full, not built.** See PLAN.md §"Skills system." |

**Tests:** `cargo test --lib` → **171 passing**, 0 failed. (+20 from round 1.) Frontend `npm run build` + `npm run check` clean.

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

**Still NOT wired:** write/delete tools, the headless browser, delegate/ask-human/system-status/cron/session-search, the durability trio, the approval spine, and MCP-into-registry. The privacy filter now *fails closed* for tool calls on a cloud endpoint (blocks); **rerouting the loop to a local endpoint** instead of blocking is the enhancement — see "What's next."

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
cd /Users/hayai/Desktop/lost-harness-product/src-tauri && cargo test --lib   # expect: 171 passed
cd /Users/hayai/Desktop/lost-harness-product && npm run build               # frontend build, should be clean
cd /Users/hayai/Desktop/lost-harness-product && npm run check               # svelte-check, should be clean
```

---

## What's next (in order)

1. **Finish M3 — the rest of the tools.** Round 1 (above) wired the spine + read-only tools. Remaining, roughly in order:
   - **The approval spine** — layered allow/ask/deny with pin/lock, and an *interactive* confirmation path so `FirstUseConfirmHook`/`Ask` can actually be resolved by the user instead of being surfaced as "not granted this round" (the round-1 placeholder). This unblocks every state-changing tool.
   - **Write/state-changing tools** on top of that spine: write/edit/delete files, then the headless browser, delegate-to-subagent, ask-human, system status, cron management, session search.
   - **The durability trio**: crash-recovery on startup, idempotency keys so a double-click or restart never double-runs a tool, and a loud-vs-silent failure split.
   - **Reroute (don't just block) for tool-triggered local-required calls** — round 1 made the dispatcher fail closed (blocks a must-stay-local tool call on a cloud endpoint). Better UX: call `enforce_local_routing` against the registered providers and switch the loop to a local endpoint for the rest of the turn instead of refusing. Needs the loop to re-select `client`/`provider`/`is_cloud` mid-loop (today they're fixed at entry).
   - **MCP tools folded into the same registry** (filtered by capability like a built-in).
   - Consider a `RiskClass` on `Tool` (safe/write/external/dangerous — "Proposed" in PLAN §3) to drive approvals/UI instead of per-tool policy strings.
2. **Build the memory system.** Full design is in `docs/PLAN.md` under "Memory system" — the short version: a small always-loaded "curated summary" plus a full searchable "archive," combining keyword search (already-bundled FTS5) and meaning search (sqlite-vec, already wired) into one hybrid search.
3. **Build the skills system.** Full design in `docs/PLAN.md` under "Skills system" — reusable playbooks, with a per-profile toggle for whether the agent can teach itself new ones or has to ask first.
4. **The remaining from-scratch gaps** (things with no equivalent in the reference material we studied, ours to build unassisted): computer/screen control (M5), voice (M6), and local-model lifecycle — detecting the user's hardware, offering a curated model catalog, downloading/verifying models (M8).
5. **The server-companion track** — the optional always-on "second brain" add-on. Starts once M4 lands; design is fully resolved (see `docs/PLAN.md` §5), nothing left to decide, just to build.

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

The **code** still uses the old names `§7` and `PrivacyGate` in a few places (module doc comments, struct names). The **docs** have all been renamed to "the privacy filter" for clarity, but the code rename was deliberately deferred — do it in a later pass, ideally at the same time the privacy filter gets wired into live tool dispatch (item 1 above), so it's one coherent touch of that code instead of a separate rename-only pass.

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
│       ├── trm/            # the classifier that labels a message Private/Public/Uncertain
│       │   ├── heuristic.rs # regex-based PII detector (SSN, credit card, API keys, health, etc.) — active today
│       │   └── engine.rs   # stub for the real trained classifier model, not built yet
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
├── src/                    # Svelte 5 frontend (chat UI, sidebar, privacy indicator, model picker, settings)
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
cd src-tauri && cargo test --lib   # 151 tests
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
