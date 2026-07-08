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

**Current milestone:** partway through M3 (the tool registry + gating spine). M0 and M1 are done and verified. Everything below is committed to `main` — there is nothing uncommitted or in-progress to pick up.

| Subsystem | Status |
|---|---|
| M0 — project bootstrap (Tauri + Svelte + Tailwind + CI) | Done |
| M1 — the core loop end-to-end (message → privacy-filter classification → route → model → stream → save) | Done + verified at the real Tauri IPC boundary by a contract-test suite |
| M3 spine — tool registry (what the agent can do, filtered per body) + the unified "one-gate" hook chain | Built + committed, **not yet wired into live tool dispatch** (no real tools exist yet — proven by unit tests only) |
| sqlite-vec (semantic memory search engine) | Wired + proven — registered on every DB open, a smoke test does a real nearest-neighbour query |
| Memory system (hybrid keyword+meaning search, curated summary + archive) | **Designed in full, not built.** See PLAN.md §"Memory system." |
| Skills system (reusable playbooks, approve-first vs. autonomous) | **Designed in full, not built.** See PLAN.md §"Skills system." |

**Tests:** `cargo test --lib` → **151 passing**, 0 failed.

---

## What "M3 spine" actually means (read this before touching tools/hooks code)

Two things landed in commit `f9223c9`:

1. **The tool registry** — a `Capability` enum (Filesystem, Network, Shell, Display, Audio, ComputerUse, Email, Calendar, WebResearch, LongCompute) plus a `Tool` trait plus a registry that filters which tools are even offered based on what the running "body" (the desktop app vs. a future headless server) can actually provide. A tool that needs a screen simply isn't offered on the headless server — the agent is told why instead of the tool failing at call time.
2. **The unified "one-gate" hook chain** — every tool action a real tool takes will pass through one ordered chain of checks: `[PrivacyFilter, Sandbox, Permission, FirstUseConfirm]`. First "no" wins. This replaces four previously-scattered gates with one auditable place. Two things worth knowing about it:
   - The **privacy filter step is wrapped, not rewritten** — it calls the existing gate logic verbatim, so nothing about privacy behavior changed, it's just now a step in a chain instead of its own standalone thing.
   - It includes a hard **"must-not-leave-this-host" rule**: if a request is flagged sensitive, it structurally cannot fail over to a cloud model even under pressure (no local model available, etc.) — it fails loudly instead of silently going to the cloud.
   - It includes an **immutable hardline danger-blocklist** (`rm -rf /`, `curl | sh`, credential exfiltration, and similar) that nothing — no setting, no future "just let it run" mode — can disable.

**What's NOT true yet:** there are no real tools wired to this chain. No file-read tool, no browser tool, nothing an actual conversation can trigger. The chain and registry exist and are proven by unit tests, but a live conversation today cannot yet call a tool through them. That wiring is the first item under "What's next," below.

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
cd /Users/hayai/Desktop/lost-harness-product/src-tauri && cargo test --lib   # expect: 151 passed
cd /Users/hayai/Desktop/lost-harness-product && npm run build               # frontend build, should be clean
cd /Users/hayai/Desktop/lost-harness-product && npm run check               # svelte-check, should be clean
```

---

## What's next (in order)

1. **Finish M3 — the tools themselves.** This is the biggest remaining chunk of foundational work:
   - Wire the hook chain (above) into real tool dispatch — right now it's proven but not load-bearing.
   - Build the tool-calling format: a "fenced" text dialect so small local models (which don't have native tool-calling support) can still reliably call tools, plus a safety rule that the agent only ever parses tool calls out of its *own* current output — so a webpage or email it reads can never forge a fake tool call.
   - Guard-wrap untrusted content (web pages, tool output, anything the agent didn't generate itself) so it can never be mistaken for an instruction — this is the core prompt-injection defense.
   - The "durability trio": crash-recovery on startup, idempotency keys so a double-click or a restart never double-runs something, and a clear split between failures that should interrupt the user loudly vs. ones that can fail silently and just get logged.
   - The approval spine (layered allow/ask/deny policy, with the ability to pin/lock an approval so it can't silently drift into approving something different later).
   - The roughly ten core tools: read/write/list/search files, a headless browser, delegating to a specialist sub-agent, asking the human a question, system status, managing scheduled jobs (cron), and searching past sessions.
2. **Build the memory system.** Full design is in `docs/PLAN.md` under "Memory system" — the short version: a small always-loaded "curated summary" plus a full searchable "archive," combining keyword search (already-bundled FTS5) and meaning search (sqlite-vec, already wired) into one hybrid search.
3. **Build the skills system.** Full design in `docs/PLAN.md` under "Skills system" — reusable playbooks, with a per-profile toggle for whether the agent can teach itself new ones or has to ask first.
4. **The remaining from-scratch gaps** (things with no equivalent in the reference material we studied, ours to build unassisted): computer/screen control (M5), voice (M6), and local-model lifecycle — detecting the user's hardware, offering a curated model catalog, downloading/verifying models (M8).
5. **The server-companion track** — the optional always-on "second brain" add-on. Starts once M4 lands; design is fully resolved (see `docs/PLAN.md` §5), nothing left to decide, just to build.

---

## Open decision flagged for Lukas (not yet decided)

**Should memory be shared across all profiles, or walled off per profile?**

Two options:
- **Shared** — one coherent assistant that remembers everything about you across every profile (work, personal, etc.).
- **Walled** — each profile has its own separate memory, so a work profile and a personal profile never bleed into each other.

Fable's reference spec (one of the two projects this design borrows mechanisms from) chose a middle path: memory is shared, but every fact is tagged with which profile it came from, so it *could* be filtered per-profile later without being a hard wall today. That's a reasonable default, but it has not been ratified as Lost Harness's answer — flag it to Lukas before the memory system gets built, since the storage schema will differ depending on which way this goes.

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
│       │   ├── loop_mod.rs # AgentLoop::process_message() — message→classify→gate→model→stream→persist
│       │   └── *_tests.rs
│       ├── trm/            # the classifier that labels a message Private/Public/Uncertain
│       │   ├── heuristic.rs # regex-based PII detector (SSN, credit card, API keys, health, etc.) — active today
│       │   └── engine.rs   # stub for the real trained classifier model, not built yet
│       ├── models/         # model manager — OpenAI-compatible HTTP client, provider config, SSE streaming
│       ├── storage/        # SQLite: global.db (shared) + one profiles/<name>.db per profile
│       │   ├── schema.rs   # every table definition, incl. memory_facts / memory_vectors (still a raw-BLOB placeholder) / skills
│       │   └── migrations.rs
│       ├── tools/          # M3: Capability enum + Tool trait + registry — built, not yet driving real tools
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
