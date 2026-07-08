# Lost Harness Product — Agent Handoff

**Repo**: `/Users/hayai/Desktop/lost-harness-product/` (Tauri 2.0 + Svelte 5 + Rust)
**Electron prototype (reference only)**: `/Users/hayai/Desktop/lost-harness-app/` — read-only, do NOT build new features here
**Spec source**: `/Volumes/SSD-Nas/Obsidian/Obsidian/lab/Projects/lost-harness-product/` (architecture.md, planning.md, spec.md, milestones.md)

---

## Project status

This is the **real product** — a Rust/Tauri/Svelte rewrite per the spec. The Electron app was a prototype to validate UX decisions; it's now a read-only reference. All new work goes in the Tauri project.

**Current milestone**: M1 (vertical slice) — nearly complete. The agent loop, privacy gate, model manager, storage, and IPC are built. One more round (frontend rewiring) connects the Svelte UI to the real backend.

**Next directions (per Lukas)**:
1. Finish M1 frontend rewiring (prompt written, ready to fire)
2. Lukas wants to "float an idea" — unclear what, wait for it
3. UI review round — test the app, identify issues
4. Agent loop structuring — proper tool-calling architecture (spec §9, §10, §12)

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

## What M1 still needs

The frontend was built against the OLD IPC stubs. The Rust backend now has real commands with different signatures. One more round of work rewrites `tauri.ts`, `chat.ts`, and `providers.svelte.ts` to call the real IPC. The prompt for this is written and ready to fire.

After that, M1 exit criteria are met: user types a message → TRM classifies → routes → model streams response → persisted to SQLite.

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

**Known environment issue**: This MacBook has both x64 (`/usr/local/bin/node`) and arm64 (`/opt/homebrew/bin/node`) Node. `npm run tauri dev` may fail with native binding errors. Workaround: `npm run build` + `cargo build` separately, or use `npx tauri dev` with explicit node path. CI on fresh runners works fine.

---

## Open items

1. **Frontend rewiring** — prompt written, ready to fire. Connects Svelte stores to real IPC.
2. **Lukas has an idea to float** — unknown, wait for it before planning further.
3. **UI review** — test the app after rewiring, identify issues.
4. **Agent loop structuring** — proper tool-calling architecture (spec §9, §10, §12). This is the next major milestone after M1 completes.
5. **TRM model** — associate is actively training. When it arrives, swap `TrmEngine::load()` stub for real GGUF loading via llama-cpp-2.
6. **The Electron app** at `/Users/hayai/Desktop/lost-harness-app/` has 15 banked UI notes from Lukas (onboarding refinements, session organization, tiling, provider logos, input bar glow) that should be ported to the Tauri frontend in future UI rounds.
