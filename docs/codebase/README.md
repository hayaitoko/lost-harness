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
blocked) *before* it can leave the machine. The core loop, the tool spine +
approval spine, and a large surface of state-changing tools are done and tested —
including a conversation-scoped **read-before-write** guard (`write_file`
on an existing target, and `edit_file`, both refuse unless that path was read first
in the same conversation). The privacy classifier is **live in both layers**: the
deterministic rules layer, fused with a trained ONNX ensemble (bge-small +
distilbert) when its models are installed under `<storage>/models/classifier/`,
falling back to rules-only otherwise (`lib.rs:100-119`) — it is not a stub. The
frontend was reskinned onto the ported `src/lib/design/` design system (Svelte
port of the React design source); most of it is now wired to the real backend —
chat, routing + the "why" explainability sidebar, and all of Settings' 10 tabs.
**As of the 2026-07-24 UI bridge campaign the app is functionally complete:**
every reachable screen (Main, Files, Scheduled jobs, Settings) is wired to the
real backend — the old Email/Whiteboard/Editor/Onboarding/EmptyState mockups
were DELETED (their backends don't exist yet; design reference lives in the
mockup repo), Files browses the real Tier-P workspace read-only, ScheduledJobs
manages the real per-profile cron store, and Settings→Models carries the M8 S5
interactive HF search + hardware calculator. See `frontend-svelte.md` for the
per-component map and HANDOFF's 2026-07-24 entry for the campaign detail.

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
| [agent-loop-and-privacy-filter.md](agent-loop-and-privacy-filter.md) | `PrivacyGate` (routing decision) + `egress::is_private_endpoint` + `AgentLoop` (the loop above), plus the agent module's satellite files: `agent/compaction.rs` (context compaction), `agent/memory_flush.rs` (pre-compaction memory flush), `agent/skill_reflect.rs` (autonomous skill drafting), `agent/work_runner.rs` (`WorkQueueRunner`, draining `queue/mod.rs`'s work-item substrate for `delegate` + cron), `agent/result_sink.rs` (`ResultSink`, decouples streaming from a live `AppHandle`), `agent/crash_recovery.rs` (boot-time reconciliation), and `audio/privacy.rs` (`AudioEgressGate`, the voice-specific egress re-vet — dormant, awaiting native audio) |
| [classifier.md](classifier.md) | The `Classifier` trait; `RulesClassifier` (active), `HeuristicClassifier` (legacy/test), `EnsembleClassifier` (the trained ONNX ensemble — **live**, not a stub; see the note below) |
| [hooks-gating-and-approval.md](hooks-gating-and-approval.md) | The unified PreToolUse gating chain + the approval spine (fingerprints, ledger, prompter), plus `hooks/audit.rs` (the `tool_audit` append-only observer), `hooks/headless.rs` (unattended-body `ApprovalPrompter`, dormant — no headless body exists yet), `hooks/routing.rs` (`enforce_local_routing`/`routing_for_turn`, the `RouteLocal`→endpoint enforcement), and `hooks/session_mode.rs` (Q11 normal/plan/accept-edits) |
| [tools.md](tools.md) | `Tool` trait/registry/`RiskClass`, the fenced tool-call dialect + injection defense, dispatch, the fs tools, plus the rest of the registry: `tools/ask_human.rs`, `tools/computer_use.rs` (dormant M5 slice), `tools/cron.rs`, `tools/delegate.rs`, `tools/exec.rs` (the shell tool), `tools/fetch.rs`, `tools/mcp.rs`, `tools/memory.rs`, `tools/session_search.rs`, `tools/skills.rs`, `tools/system_status.rs` |
| [models.md](models.md) | `ModelManager`, providers, the OpenAI-compatible HTTP client + SSE (text-only, no native tool_use yet), plus `models/content.rs` (multimodal wire format, dormant), `models/pricing.rs` (usage-ledger cost), `models/catalog.rs` (the curated download catalog), `models/download.rs` (verified-before-runnable installer), `models/hardware.rs` (hardware probe for onboarding), `models/seat.rs` (model seats) |
| [storage.md](storage.md) | Two-DB SQLite (global + per-profile), schema/migrations, sqlite-vec + FTS5, `trm_logs` audit, plus `embedder.rs` (the on-device text embedder feeding memory's sqlite-vec meaning lane) |
| [ipc-and-app-wiring.md](ipc-and-app-wiring.md) | Tauri command surface + `AppState` (70 commands as of 2026-07-24, 9 `AppState` fields — incl. the Gmail surface + `EmailRuntime`), the approval IPC round-trip, `lib.rs::run` wiring, plus `ipc/ask_human.rs` (the ask-human IPC round-trip) and `packs/mod.rs` (Capability Packs, installed via the `install_pack` command) |
| [frontend-svelte.md](frontend-svelte.md) | The Svelte 5 shell, `tauri.ts` (the only IPC bridge), stores, components — the ported `src/lib/design/` design system (components/screens reskinned from the React source at lost-harness-ui), now mostly wired to real backend stores (see that doc for exactly which screens/tabs still aren't) |
| [../releasing.md](../releasing.md) | `updater/mod.rs` — the app's self-update: the launch gate (dev build + Settings toggle, the only update egress path), signature verification, the download-host constraint, and the pending-update slot. Also the release runbook (tag → CI → signed draft), where the signing key lives + how to rotate it, and an explicit **proven vs not proven** section — the end-to-end install/relaunch loop has never been executed. Read it before touching `.github/workflows/build.yml`'s `release` job |

## Load-bearing invariants (do NOT break these)

These are the guarantees the whole product rests on. Each subsystem doc says where
its own are enforced; the cross-cutting ones:

- **The privacy filter fails closed — for the classified chat turn and each tool's
  own destination.** A turn the §7
  classifier flags is never silently sent to the cloud: `RouteLocal` only proceeds on a
  provider that is both local *and* private, else the call errors rather than falling
  back (agent), and a tool call flagged `LocalRequired` on a cloud endpoint is denied
  outright (tools/dispatch) — a second, independent enforcement point. **Scope the
  2026-07-23 external review pinned down (see the ledger in `HANDOFF.md`):** this
  guarantee now covers both the model endpoint and a tool's own off-box destination:
  `Tool::egresses_offbox` is folded into the dispatch gate, Remote-tier MCP tools can
  never be downgraded below `External`, and rerouting only the model cannot authorize
  a remote tool. A `Public`-binding turn still bypasses the classifier by deliberate
  product decision (F4). Explicit, user-driven network surfaces such as the Settings
  model search/download IPC remain outside the chat gate and must identify their
  destination in their UI (F13).
- **The sandbox floor cannot be disabled — but it is a pattern denylist, not a semantic
  guarantee.** The hardline danger denylist runs before any ask-capable hook and no
  setting can turn it off (hooks). It is substring matching over command text, so
  obfuscation (`$IFS`, quoting, alternate interpreters, base64) can evade the *pattern*
  match (F8) — this is defense-in-depth *behind* the mandatory per-call human approval
  and the deny-by-default Seatbelt jail, not a standalone semantic command-safety
  guarantee. It also does not govern an MCP server's *own child process* (F1).
- **Parse only the model's own current-turn output** for tool calls, and guard-wrap
  all untrusted tool output. Content the agent merely *read* can never *forge* a tool
  call (tools/calling). Scope (F9): guard-wrap makes the data/instruction boundary
  structurally unforgeable but does not stop *indirect* prompt injection — untrusted
  content can still persuade the model to emit a *legitimate new* call; the approval
  gate on Write/External/Dangerous tools is what bounds that.
- **"Asked" is not "approved."** An unattended agent cannot self-grant a gated tool
  by attempting it; only a recorded approval flips a call through (hooks/approval).
- **Workspace confinement.** The fs tools cannot touch anything outside `workspace/` —
  not via `..`, absolute paths, or symlinks (tools/fs).
- **Persisted routing/audit logs do not retain message or tool-argument plaintext.**
  `trm_logs` stores a message hash, never the text. The production
  `StorageAuditWriter` replaces canonical tool arguments with a redacted marker plus
  the existing action fingerprint before inserting `tool_audit`. The hourly retention
  sweep keeps TRM logs for 7 days, terminal work items for 30 days, and tool-audit rows
  for 90 days; usage events are intentionally retained for budgets and spend history.

## How to run, test, build

```bash
# from the repo root
cd src-tauri && cargo test --lib      # Rust unit/contract tests (721 as of 2026-07-24)
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

Flagged here so they're not rediscovered the hard way — each verified directly
against the code as of this pass:

- **`sandbox_config`'s network ceiling is live code but unreachable.** Nothing
  ever writes a `sandbox_config` row: `set_sandbox_config` (`storage/profile.rs:1361`)
  is called only from `tools/exec.rs`'s own tests — no IPC command and no UI
  surfaces it. `ShellExecTool::effective_network` (`tools/exec.rs:450-469`)
  therefore takes the `Ok(None) => true` ("unconfigured → unconstrained")
  branch at line 458 for every real profile today; the ceiling only bites
  once something writes the row.
- **`audio/privacy.rs`'s `stt_egress` deviates from the M6 design.** It
  content-classifies the transcript by delegating straight to `tts_egress`
  (`audio/privacy.rs:87-95`), but the real pre-transcription STT decision has
  to be content-free (binding-based) — you can't classify audio before it's
  transcribed. There is also no direct test for `stt_egress` (only
  `tts_egress` is exercised, `audio/privacy.rs:98-172`). Fix the design
  mismatch before wiring native STT.
- **No end-to-end test proves a cloud-bound seat can't defeat `RouteLocal`
  through `run_subagent`.** `models/seat.rs:9-13` states the invariant
  ("a seat may PREFER a cloud model but can never defeat a `RouteLocal`/
  `LocalRequired` verdict"), and `AgentLoop::run_subagent` (`agent/loop_mod.rs:504`)
  goes through the same gate as `process_message` — but no test calls
  `run_subagent` at all (`grep -rn "run_subagent" agent/ tools/` outside its
  own definition and doc comments turns up nothing). The invariant holds by
  construction only.
- **Two Wave-4.3c paths are implemented but untested:** the delegated-helper
  guard-wrap-on-re-entry branch (`agent/loop_mod.rs:1240-1256` — a delegated
  helper's result re-entering the main agent's context is neutralized like
  tool output, never replayed as a trusted assistant turn) and
  `work_runner`'s `HELPER_DEADLINE` (declared `agent/work_runner.rs:43`, 300s;
  applied via `tokio::time::timeout` at `agent/work_runner.rs:178`) plus its
  panic-supervisor path (`agent/work_runner.rs:81-106`). `work_runner.rs`'s
  only test covers cron scheduling (`agent/work_runner.rs:349-350`); the deadline
  and panic paths have zero coverage.
- **Cron's "never egresses" claim has a session-replay nuance.**
  `ActionFingerprint::of` (`hooks/approval.rs:61-78`) hashes only
  `(tool_name, canonicalized args)` — no session or conversation
  discriminator — and `ApprovalLedger`'s `session_fps`/`session_tools`
  (`hooks/approval.rs:144-151`) are flat, app-lifetime sets shared across
  every conversation in the process. A Session-scoped `External`-tool grant
  made interactively could in principle be "already granted" for a
  byte-identical headless cron call later in the same app session.
- **`open_profile` accepts whitespace-padded profile names.**
  `storage/mod.rs:178-204` rejects an empty name, one containing `/`, `\`, or
  `..`, or one starting with `.` — but never trims. `" work"`, `"work "`, and
  `"work"` are three distinct,
  confusable profiles (three different `.db` files, three different cache
  entries). `ipc::SendMessageArgs.profile` and the other `*ProfileArgs`
  structs (`ipc/mod.rs`) pass whatever the frontend sends straight through
  with no trim either. Tighten `open_profile` and the `send_message` IPC
  boundary together.
- **Tool-action `PrivacyFilterHook` gates at default classifier thresholds,
  not the profile's** (`hooks/privacy_filter.rs:41-57`, acknowledged in the
  hook's own doc comment). The per-profile strictness knob (PLAN §11) is
  threaded into the message-egress gate but not yet into the tool-gating
  chain — a profile configured stricter than default is, for tool-call
  content specifically, gated *less* strictly than that same profile's chat
  messages. Bounded by the un-tunable rules floor (structured PII is always
  caught regardless), but real for free-text semantic PII in the narrow
  band.
- **Non-streaming `complete()` calls book no usage row.** `ModelClient::complete`
  (`models/client.rs:235-278`) is called by `agent/memory_flush.rs:138` and
  `agent/skill_reflect.rs:149` today, and its own doc comment earmarks it for
  "titles, routing TRM calls" too (`models/client.rs:232-234`) — but
  `record_usage` has exactly one call site in the whole codebase
  (`agent/loop_mod.rs:1491`, inside the streaming path). Every `complete()`-based
  call, current or future, is invisible to the Settings "Usage" view.
- **Dormant-by-design, awaiting a native/on-target slice:** `tools/computer_use.rs`
  (declared as a module but never registered as a tool in
  `build_tool_dispatcher`), `models/content.rs`'s `assemble_content` (called
  only by its own tests — the multimodal wire format has no producer yet),
  `hooks/routing.rs`'s `routing_for_turn` (called only by its own tests — no
  caller passes it a real `has_image` signal yet), and `audio/privacy.rs`'s
  `AudioEgressGate` (zero callers outside its own module — no native audio
  I/O exists to consume its verdict). None of these are bugs; don't be
  surprised when grepping for callers turns up nothing.
- **The model catalog ships placeholder hashes.** All four entries in
  `models/catalog.json` have `"sha256": "TODO-CURATE"`. `CatalogEntry::is_curated()`
  (`models/catalog.rs:48`) gates `download_model`, so this fails closed —
  nothing in the catalog is installable until the real hashes are filled in
  before release.

*Regenerated 2026-07-21 against 542 tests / HEAD `ca54251`; test count refreshed
2026-07-24 to 685 (external-review fix batch + the UI bridge campaign — new
cron + workspace-listing IPC, M8 S5 search/calculator UI, mock screens
deleted). If you change a subsystem materially, update its doc — a wrong doc
is worse than none.*
