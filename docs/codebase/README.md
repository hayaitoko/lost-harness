# Lost Harness — codebase guide (for agents)

**Start here if you're picking up this codebase.** This is a map of the code
*as it actually is*, written for an agent about to change it. For the *design*
(what the product is and why), read [`../PLAN.md`](../PLAN.md) — the source of
truth. For *current status / what's next*, read [`../../HANDOFF.md`](../../HANDOFF.md).
Where the code and PLAN.md disagree, the code wins and the subsystem doc notes it.

## What this app is (in one breath)

A local-first personal-AI desktop app: a **Rust core** compiled into a **Tauri 2**
shell with a **Svelte 5** frontend. Its defining feature is a **privacy filter** —
every call out to a model is classified and routed (kept local, sent to cloud, or
blocked) *before* it can leave the machine. Milestones M0–M1 (core loop) and the
bulk of M3 (tool spine + approval spine + first state-changing tools) are done and
tested; the classifier's deterministic rules layer is live, its trained ONNX layer
is a stub. See HANDOFF for the precise line.

## The request flow (the spine)

```
user message
  → classify (classifier::RulesClassifier → Label: Private|Public|Uncertain)
  → gate (agent::PrivacyGate: binding + label + endpoint → Allow | Block | RouteLocal)
  → route (RouteLocal requires a provider that is BOTH local AND private, else Err)
  → stream (models::ModelClient, OpenAI-compatible SSE)
  → agentic tool loop (bounded): parse the model's OWN output for fenced ```tool calls
      → dispatch (tools::ToolDispatcher)
          → gating chain (hooks): PrivacyFilter → Sandbox(floor) → Permission → FirstUseConfirm
          → approval spine (pause → prompt user → resume) for state-changing tools
          → execute → guard-wrap the result → feed back
  → persist transcript (storage: profiles/<name>.db)
```

## Subsystem docs

| Doc | Covers |
|---|---|
| [agent-loop-and-privacy-filter.md](agent-loop-and-privacy-filter.md) | `PrivacyGate` (routing decision) + `egress::is_private_endpoint` + `AgentLoop` (the loop above) |
| [classifier.md](classifier.md) | The `Classifier` trait; `RulesClassifier` (active), `HeuristicClassifier` (legacy/test), `EnsembleClassifier` (ONNX stub) |
| [hooks-gating-and-approval.md](hooks-gating-and-approval.md) | The unified PreToolUse gating chain + the approval spine (fingerprints, ledger, prompter) |
| [tools.md](tools.md) | `Tool` trait/registry/`RiskClass`, the fenced tool-call dialect + injection defense, dispatch, the fs tools |
| [models.md](models.md) | `ModelManager`, providers, the OpenAI-compatible HTTP client + SSE (text-only, no native tool_use yet) |
| [storage.md](storage.md) | Two-DB SQLite (global + per-profile), schema/migrations, sqlite-vec + FTS5, `trm_logs` audit |
| [ipc-and-app-wiring.md](ipc-and-app-wiring.md) | Tauri command surface + `AppState`, the approval IPC round-trip, `lib.rs::run` wiring |
| [frontend-svelte.md](frontend-svelte.md) | The Svelte 5 shell, `tauri.ts` (the only IPC bridge), stores, components |

## Load-bearing invariants (do NOT break these)

These are the guarantees the whole product rests on. Each subsystem doc says where
its own are enforced; the cross-cutting ones:

- **The privacy filter fails closed.** Sensitive content is never silently sent to
  the cloud. `RouteLocal` only proceeds on a provider that is both local *and*
  private, else the call errors rather than falling back (agent). A tool call flagged
  `LocalRequired` on a cloud endpoint is denied outright (tools/dispatch) — a second,
  independent enforcement point.
- **The sandbox floor is non-overridable.** The hardline danger denylist runs before
  any ask-capable hook and nothing (no setting, no "just let it run") can disable it
  (hooks).
- **Parse only the model's own current-turn output** for tool calls, and guard-wrap
  all untrusted tool output. Content the agent merely *read* can never forge a call
  (tools/calling).
- **"Asked" is not "approved."** An unattended agent cannot self-grant a gated tool
  by attempting it; only a recorded approval flips a call through (hooks/approval).
- **Workspace confinement.** The fs tools cannot touch anything outside `workspace/` —
  not via `..`, absolute paths, or symlinks (tools/fs).
- **Audit logs never store plaintext.** `trm_logs` stores a hash of the message, never
  the text (storage / agent).

## How to run, test, build

```bash
# from the repo root
cd src-tauri && cargo test --lib      # Rust unit/contract tests (216 as of 2026-07-10)
cd src-tauri && cargo build           # Rust core
npm run tauri dev                     # full app (native window) — see gotcha below
npm run build && npm run check        # frontend build + svelte-check
```

## Toolchain gotchas (will bite you)

- **Run `cargo` from `src-tauri/`,** not the repo root — the Rust project lives there.
- **This machine's Rust toolchain is x86_64 (runs under Rosetta);** Node is arm64.
  Builds/tests work via translation — the arch mismatch is expected, not a bug.
- **The shell resets cwd between commands** — use absolute paths or `cd` inside one
  compound command.
- **Tauri v2 struct args nest under `args`:** a command `fn cmd(state, args: T)` is
  called from JS as `invoke("cmd", { args: { ...snake_case } })`. Flat/camelCase
  compiles + passes the browser mock but fails in the real shell. (See
  `ipc/contract_tests.rs` — the regression lock.)
- **The window loads `app.html`, not `/`.** `tauri.conf.json` sets
  `windows[0].url = "app.html"` (Vite root is `src/`, entry is `src/app.html`).
  Loading `/` 404s → blank white window.

## Watch-items the review surfaced (not yet fixed)

Flagged here so they're not rediscovered the hard way — details in the subsystem docs:

- **Stale module doc-comments.** `gate.rs`/`loop_mod.rs`/`classifier/mod.rs` still
  say the classifier is "the trained model or heuristic fallback"; production actually
  wires `RulesClassifier`.
- **`ipc::send_message` returns `routing_decision: "allow"` hardcoded** regardless of
  the real decision — the true decision is in the persisted `Message`/`trm_logs`, not
  the response.
- **`EnsembleClassifier` is a stub** (`load()` always errs); the ONNX layer isn't
  wired (no `ort` dep; blocked on exported `.onnx` artifacts).
- **`loop_tests.rs` tests a reimplementation** (`TestLoop`), not the real `AgentLoop` —
  `MAX_TOOL_ROUNDS`, real re-routing, and `stream_lock` concurrency are unit-untested.
- **`trm_logs` / `TrmLog` kept their old names** after the `trm`→`classifier` rename
  (migration cost) — grep both `classifier` and `trm`.

*Generated 2026-07-10 by a subsystem-by-subsystem review fleet. If you change a
subsystem materially, update its doc — a wrong doc is worse than none.*
