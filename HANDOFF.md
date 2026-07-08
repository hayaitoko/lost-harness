# Lost Harness Product — Agent Handoff

**Repo**: `/Users/hayai/Desktop/lost-harness-product/` (Tauri 2.0 + Svelte 5 + Rust)
**Electron prototype (reference only)**: `/Users/hayai/Desktop/lost-harness-app/` — read-only, do NOT build new features here
**Spec source**: `/Volumes/SSD-Nas/Obsidian/Obsidian/lab/Projects/lost-harness-product/` (architecture.md, planning.md, spec.md, milestones.md)

---

## Project status

This is the **real product** — a Rust/Tauri/Svelte rewrite per the spec. The Electron app was a prototype to validate UX decisions; it's now a read-only reference. All new work goes in the Tauri project.

**Current milestone**: M1 (vertical slice) — frontend wired to the real backend, reviewed, fixed, committed (`c8ee16a`). The Svelte UI now calls the real IPC (send_message, providers, conversations, messages) with the browser fallback preserved. `cargo test` 82 pass · `svelte-check` 0 errors · `npm run build` ✓. **Remaining for M1 exit: one live `tauri dev` smoke test** (blocked on the arm64 CLI pin — see "How to run").

**Next directions (per Lukas)**:
1. ~~Finish M1 frontend rewiring~~ ✅ done + committed (see "Session log 2026-07-07")
2. Lukas's idea — the optional **Server Companion** ("second brain"). Design captured in [`docs/server-companion.md`](docs/server-companion.md). Not built; it shapes M3 (tool registry) and the §9 loop.
3. UI review round — test the app, identify issues. Also port the 15 banked Electron UI notes (onboarding, session folders/tags/colors/tiling, provider logos, input-bar glow).
4. Agent loop structuring — proper tool-calling architecture (spec §9, §10, §12), with the environment-agnostic tool registry from `docs/server-companion.md` baked in.

---

## Architecture

```
lost-harness-product/
├── src-tauri/
│   ├── Cargo.toml          # tauri 2.11, serde, tokio, rusqlite (bundled), reqwest, sha2, regex, etc.
│   ├── tauri.conf.json     # window "Lost Harness" 1280x800
│   └── src/
│       ├── main.rs         # bin entry, calls lib::run()
│       ├── lib.rs          # Tauri builder, state management, IPC handler registration
│       ├── agent/          # §7 privacy gate + agent loop
│       │   ├── mod.rs      # declares gate, egress, loop_mod, gate_tests
│       │   ├── gate.rs     # PrivacyGate, Binding {Auto,Public,Private}, GateDecision {Allow,Block,RouteLocal}
│       │   ├── egress.rs   # is_private_endpoint() — ported from Electron, matches dotted-quad IPv4 + suffixes
│       │   ├── loop_mod.rs # AgentLoop::process_message() — message→TRM→gate→model→stream→persist
│       │   ├── loop_tests.rs # 5 tests (public/private/auto routing)
│       │   └── gate_tests.rs # 19 tests (gate decisions, egress control)
│       ├── trm/            # TRM (Tiny Recursive Model) interface
│       │   ├── mod.rs      # Classifier trait, Label {Private,Public,Uncertain}, Classification struct
│       │   ├── heuristic.rs # HeuristicClassifier — regex PII detector (SSN, credit card, API keys, health, etc.)
│       │   └── engine.rs   # TrmEngine — stub, returns error on load() until trained model arrives
│       ├── models/         # Model manager — OpenAI-compatible HTTP client
│       │   ├── mod.rs      # declares submodules, re-exports
│       │   ├── provider.rs # Provider struct, ProviderKind, is_private()/is_local()
│       │   ├── sse.rs      # SSE stream parser (ported from Electron's sse.mjs)
│       │   ├── client.rs   # ModelClient — list_models, stream_chat, complete (60s idle timeout)
│       │   ├── manager.rs  # ModelManager — add/remove/list providers, get_client, list_models_for
│       │   └── tests.rs    # 16 tests (provider, SSE, manager)
│       ├── storage/        # SQLite two-database architecture
│       │   ├── mod.rs      # Storage struct (global + per-profile), open/open_profile
│       │   ├── schema.rs   # CREATE TABLE constants for all spec'd tables
│       │   ├── global.rs   # GlobalDb — endpoints, memory_facts, skills, app_settings, etc.
│       │   ├── profile.rs  # ProfileDb — conversations, messages, folders, tags, trm_logs, etc.
│       │   ├── migrations.rs # schema_version system, v1 = initial
│       │   └── tests.rs    # 15 tests (tables exist, CRUD, folders, tags, migrations)
│       ├── ipc/            # Tauri command handlers
│       │   └── mod.rs      # 11 commands + AppState struct (agent_loop, model_manager, storage)
│       ├── platform/       # Computer use stubs (M5)
│       │   ├── mod.rs      # cfg'd submodules
│       │   ├── macos/
│       │   ├── windows/
│       │   └── linux/
│       ├── audio/          # Audio engine stub (M6)
│       └── tools/          # Tool registry stub (M3)
├── src/                    # Svelte 5 frontend
│   ├── App.svelte          # Root: flex layout, sidebar + chat, settings modal, hydrate on mount
│   ├── app.css             # Tailwind directives
│   ├── app.html            # Shell
│   ├── main.ts             # Svelte mount
│   └── lib/
│       ├── api/tauri.ts    # IPC bridge (NEEDS REWIRING — currently uses old stub signatures)
│       ├── stores/
│       │   ├── chat.ts             # Chat store (NEEDS REWIRING — old sendMessage signature)
│       │   ├── profiles.ts         # Profiles store
│       │   ├── providers.svelte.ts # Providers store (NEEDS REWIRING — uses localStorage not IPC)
│       │   ├── provider-catalog.ts # Known provider presets + default model lists
│       │   └── settings.ts         # Theme, sendOnEnter
│       └── components/
│           ├── ChatPanel.svelte       # Chat UI with streaming, PrivacyIndicator, ModelPicker
│           ├── Sidebar.svelte         # Conversation list, new chat, profile indicator
│           ├── PrivacyIndicator.svelte # Traffic light (green/yellow/red)
│           ├── ModelPicker.svelte     # Compact button, upward popup, search, provider grouping
│           └── ProviderSettings.svelte # Provider config modal, quick-add presets
├── package.json            # tauri 2.11, svelte 5.56, tailwind 4.3, vite 8.1
├── vite.config.ts          # $lib alias → src/lib
├── svelte.config.js        # runes: true
└── .github/workflows/build.yml # CI matrix (mac/win/linux)
```

---

## What's built (M0 + M1 core)

| Subsystem | Status | Tests |
|---|---|---|
| Project scaffold (Tauri + Svelte + Tailwind + CI) | ✅ Done | — |
| SQLite storage (global.db + per-profile, all spec tables, migrations) | ✅ Done | 15 |
| §7 privacy gate (Binding, GateDecision, conservative bias) | ✅ Done | 19 |
| Heuristic PII classifier (SSN, credit card, API keys, health, etc.) | ✅ Done | 15 |
| TRM interface (Classifier trait, engine stub) | ✅ Done | — |
| Model manager (OpenAI-compatible HTTP client, SSE streaming) | ✅ Done | 16 |
| Agent loop (message → TRM → gate → model → stream → persist) | ✅ Done | 5 |
| IPC bridge (11 commands, AppState, streaming events) | ✅ Done | — |
| Frontend chat UI (chat panel, sidebar, privacy indicator, model picker) | ✅ Built, needs rewiring | — |
| **Total tests** | | **82** |

---

## M1 status + what's left

The frontend rewiring landed and was reviewed (14 findings, adversarially verified) and fixed — commit `c8ee16a`. `tauri.ts`, `chat.ts`, `providers.svelte.ts`, and the components now call the real IPC. Fixes P1–P8 (see "Session log 2026-07-07"). All automated checks green.

**M1 exit criteria are met on paper** (type a message → TRM classifies → routes → model streams → persisted to SQLite) **but not yet proven end-to-end in the real Tauri shell** — the P1 IPC-contract fix is verified statically, not by a live run. Do one `tauri dev` smoke test to close M1 (needs the arm64 CLI pin fixed first).

**Deferred (flagged, not a correctness gap):** the "ideal" P7 backend fix — have `AgentLoop::process_message` return the real assistant message id (and the actual `routing_decision`) instead of `ipc::send_message` re-deriving it via a "last assistant row" query, and persist the user's message even on a gate-Block so a blocked turn survives reload. The current frontend guard + P8 rowid-ordering fully eliminate the id-collision corruption, so this is cleanup for a later pass.

---

## Key design decisions

1. **Two codebases**: Electron app (`lost-harness-app`) is a read-only reference. Tauri project (`lost-harness-product`) is the real product. All new work in Tauri.
2. **Rust core + Tauri 2.0 + Svelte 5 (runes) + Tailwind** — per architecture.md. No Electron, no Python runtime.
3. **Two-database SQLite architecture**: `global.db` (shared: endpoints, memory, skills, settings) + `profiles/<name>.db` (per-profile: conversations, messages, folders, tags, trm_logs). Spec §1, §5.
4. **§7 privacy gate**: Auto binding → classify → RouteLocal on Private/Uncertain + cloud endpoint. Conservative bias (spec Risk 4). Public binding → always allow. Private binding → always block cloud.
5. **Heuristic classifier is the active fallback** until the TRM model arrives (associate is actively training it). The `Classifier` trait means the real TRM drops in without changing the gate.
6. **SSE parser ported from Electron** — the Electron app's `shared/sse.mjs` was battle-tested (handles split chunks, [DONE], keep-alives). Ported to Rust.
7. **Browser fallback in frontend**: `window.__TAURI_INTERNALS__` detection → mock implementations for dev without Tauri. Must be preserved.
8. **82 tests, all passing.** 32 dead-code warnings (expected — stub modules have no consumers yet).

---

## How to run

```bash
cd /Users/hayai/Desktop/lost-harness-product

# Frontend dev (browser, no Tauri)
npm run dev                # Vite dev server, open localhost:1420

# Full Tauri dev (may hit node arch mismatch on this MacBook)
npm run tauri dev          # Tauri + Vite HMR

# Build
npm run build              # Frontend only
cd src-tauri && cargo build  # Rust only

# Test
cd src-tauri && cargo test # 82 tests
npm run build              # Frontend compile check

# CI
# .github/workflows/build.yml — mac/win/linux matrix
```

**Known environment issue — arm64/x64 native bindings (needs a real fix in `package.json`)**: `package.json` pins the **x64** native bindings (`@rolldown/binding-darwin-x64`, `@tauri-apps/cli-darwin-x64`) but the active Node on this Mac is **arm64**, so `npm run build`/`svelte-check`/`tauri dev` fail with `Cannot find native binding` for rolldown, lightningcss, `@tailwindcss/oxide`, and esbuild. Current workaround (2026-07-07): the matching **arm64** binaries were fetched via `npm pack` and dropped into `node_modules` (gitignored) — `npm run build` + `svelte-check` now pass. **Proper fix:** stop pinning x64-only; let npm resolve the platform binding (list both, or use `optionalDependencies` / drop the explicit pins). Until then, `tauri dev` still needs `@tauri-apps/cli-darwin-arm64` installed. CI on fresh runners is unaffected.

---

## Open items

1. **Live M1 smoke test** — fix the arm64 CLI pin, run `tauri dev`, send a real message end-to-end. Closes M1.
2. **Fix the arm64/x64 binding pins in `package.json`** (see "How to run") — real fix, not the node_modules workaround.
3. **Server Companion design** — [`docs/server-companion.md`](docs/server-companion.md). Optional 24/7 add-on. Not built; bake the environment-agnostic tool registry into M3.
4. **UI review + port 15 banked Electron notes** — onboarding (product key, model recommendations), stacked full-width panel buttons, session folders/tags/colors, tiling multiple sessions side-by-side, colored sessions (sidebar bg + input-bar glow), provider logos, model-picker popup next to the mic. Full detail lives in the `orchestrator 1` chat log (2026-07-07).
5. **Agent loop structuring** — proper tool-calling architecture (spec §9, §10, §12). Next major milestone after M1.
6. **Deferred P7 backend cleanup** — return the real assistant id from `process_message`; persist blocked turns (see "M1 status + what's left").
7. **TRM model** — associate is actively training. When it arrives, swap `TrmEngine::load()` stub for real GGUF loading via llama-cpp-2.

---

## Session log — 2026-07-07 (orchestrator 2)

Picked up from `orchestrator 1`'s handoff.

- **Ground-truthed** both repos, the spec (`/Volumes/SSD-Nas/…/lost-harness-product/`), and the chat log. Confirmed: Tauri product is the live line; Electron app is abandoned reference.
- **Fixed the local arm64 toolchain** so the frontend builds here (see "How to run").
- **Reviewed the uncommitted M1 rewiring** (adversarial multi-agent pass): 14 findings confirmed, 1 refuted. Headline: every struct-arg IPC command was broken (missing Tauri `args:` wrapper + camelCase inner keys) — invisible to tests/build, masked by the browser mock.
- **Fixed P1–P8** and committed (`c8ee16a`). All checks green. See "M1 status + what's left".
- **Captured Lukas's Server Companion idea** as [`docs/server-companion.md`](docs/server-companion.md) (cron execution modes incl. "Local → Fallback Server", exactly-once run-ledger, durable outbox result queue).
