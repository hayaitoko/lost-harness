# Tools subsystem (`src-tauri/src/tools/`)

- **Purpose** — Defines what a "tool" is (`Tool` trait + `Capability`/`RiskClass`
  metadata), the fenced text dialect small local models use to call tools (plus a
  native structured-tool-call transport that folds into the identical downstream
  path) and the untrusted-content guard-wrapping that stops prompt injection from
  forging calls, the dispatcher that resolves/gates/executes a call end-to-end
  (budgets, repeat detection, deny-cascade, the interactive-approval pause/resume
  loop), and the full tool surface: 18 real tools spanning workspace-confined
  filesystem, a sandboxed shell, web fetch, cron CRUD, memory, session search,
  system status, skills, and background-agent delegation. MCP is a built, tested
  trust spine with no live wire client yet; a computer-use action/risk model
  exists but is not registered anywhere.

## Files

- `src-tauri/src/tools/mod.rs` — `Capability`, `BodyEnv`, `ToolInput`/`ToolResult`/`ExecCtx`,
  `ConversationReads` (the read-before-write guard's state), `RiskClass`, the `Tool`
  trait, `ToolRegistry`. Also three trivial example tools (`EchoTool`, `ScreenshotTool`,
  `SyncFileTool`) used only in tests to exercise capability filtering — not real
  product tools.
- `src-tauri/src/tools/calling.rs` — the fenced ` ```tool ` dialect: `parse_tool_calls`,
  `ParsedToolCall`, `assemble_native_calls` (folds a provider's native streamed
  tool-call fragments into the same `ParsedToolCall` shape the fenced parser
  produces — the normalization point where both transports meet before dispatch),
  `render_tool_catalog` (system-prompt fragment listing available tools),
  `guard_wrap`/`guard_wrap_stable`/`neutralize_untrusted` (injection defense for
  untrusted content re-entering the model's context).
- `src-tauri/src/tools/dispatch.rs` — `ToolDispatcher`: `dispatch()` (resolve →
  availability → gating chain → approval pause/resume → execute), `run_turn()`
  (fenced dialect: parse the model's own output, drive every call) and
  `run_turn_native()` (native transport: same driver, calls already parsed).
  Both funnel through the shared `drive()` (budgets, repeat detection,
  deny-cascade, local-reroute handling) and `dispatch()`. `ToolOutcome`/`TurnOutcome`
  enums, `format_outcome()`.
- `src-tauri/src/tools/fs.rs` — the six filesystem tools: `ReadFileTool`,
  `ListDirTool`, `SearchFilesTool` (`RiskClass::Safe`) and `WriteFileTool`,
  `EditFileTool`, `DeleteFileTool` (`RiskClass::Write`). `profile_workspace_path`
  (the per-profile subtree resolver), `migrate_legacy_workspace` (one-time Tier-P
  migration), path-safety helpers `resolve_within`/`resolve_within_new`,
  `atomic_write`, `canonicalize_best_effort`.
- `src-tauri/src/tools/exec.rs` — `ShellExecTool` (`shell_exec`, `RiskClass::Dangerous`),
  the `SandboxedSpawn` trait, `MacSeatbeltSpawn` (macOS Seatbelt backend, always on),
  `UnsupportedSandbox` (every other platform — hard-errors, never runs unsandboxed).
- `src-tauri/src/tools/fetch.rs` — `FetchUrlTool` (`fetch_url`, the first
  `RiskClass::External` tool), the manually-followed-redirect SSRF guard
  (`ssrf_check`, `is_blocked_ipv4`, `embedded_ipv4`).
- `src-tauri/src/tools/cron.rs` — `ListCronJobsTool` (`list_cron_jobs`, Safe),
  `ManageCronTool` (`manage_cron`, Dangerous), the `cron_due` schedule matcher
  (minute/hour/dom/dow/month, lists/ranges/steps/names, `@hourly`/`@daily` macros).
- `src-tauri/src/tools/delegate.rs` — `DelegateTool` (`delegate`, Dangerous) — enqueues
  a `work_items` row for the background `WorkQueueRunner` rather than running
  anything itself (avoids a circular `AgentLoop → ToolDispatcher → delegate →
  AgentLoop` dependency).
- `src-tauri/src/tools/ask_human.rs` — `AskHumanTool` (`ask_human`, Safe — pre-trusted;
  blocks the loop awaiting a real answer), the `HumanPrompter` trait.
- `src-tauri/src/tools/memory.rs` — `RecallMemoryTool` (`recall_memory`, Safe) and
  `RememberMemoryTool` (`remember`, Write); `route_memory_sensitivity`,
  `semantic_search_enabled`.
- `src-tauri/src/tools/session_search.rs` — `SessionSearchTool` (`session_search`, Safe
  — recall over past conversations, distinct from memory).
- `src-tauri/src/tools/system_status.rs` — `SystemStatusTool` (`system_status`, Safe —
  OS/arch, profiles, model install state).
- `src-tauri/src/tools/skills.rs` — `SearchSkillsTool` (`search_skills`, Safe) and
  `SaveSkillTool` (`save_skill`, **Dangerous** — deliberately not `Write`, so
  `accept_edits` can never mint a standing, cross-profile, auto-loaded skill
  without a human seeing its content).
- `src-tauri/src/tools/mcp.rs` — `McpTool`, `McpTrustTier`, `McpServerConfig`,
  `mcp_risk`/`mcp_capabilities`, `UnwiredTransport`. `#![allow(dead_code)]` at the
  top of the file — nothing in the production path constructs these types yet.
- `src-tauri/src/tools/computer_use.rs` — `ActionTarget`, `ComputerAction`,
  `Reversibility`, `reversibility()`, `risk_class()`. Pure logic, fully tested,
  **not a `Tool` impl and not registered anywhere** — the native backends
  (macOS AX/CGEvent, Windows UIA, Linux AT-SPI) are on-target M5 work; this is
  only the "how hard to gate an on-screen action" model, built ahead of them.
- `src-tauri/src/tools/tests.rs` — registry/`restricted_to` unit tests, wired via
  `#[cfg(test)] mod tests;` at the bottom of `mod.rs:507`.
- Wiring lives outside this dir: `src-tauri/src/lib.rs:433-647` (`build_tool_dispatcher`)
  — the one place that registers all 18 real tools, derives a `PermissionMode`
  policy from each tool's `risk()`, layers the persisted per-profile `tool_rules`
  store over it, and builds the pretooluse hook chain (`crate::hooks`) the
  dispatcher runs calls through. See `docs/codebase/hooks-gating-and-approval.md`
  for the chain itself.

## The full tool surface (registration order, `lib.rs:484-588`)

| # | name | risk | capability | notes |
|---|---|---|---|---|
| 1 | `read_file` | Safe | Filesystem | `fs.rs:266-336` |
| 2 | `list_dir` | Safe | Filesystem | `fs.rs:341-396` |
| 3 | `search_files` | Safe | Filesystem | `fs.rs:401-455`; first-line-only content match, see Gotchas |
| 4 | `write_file` | Write | Filesystem | `fs.rs:644-745`; read-before-write on overwrite |
| 5 | `edit_file` | Write | Filesystem | `fs.rs:752-844`; unique-match + read-before-write |
| 6 | `delete_file` | Write | Filesystem | `fs.rs:849-902`; files only |
| 7 | `recall_memory` | Safe | (none) | `memory.rs:100-205`; shared always, private-local conditionally |
| 8 | `remember` | Write | (none) | `memory.rs:212-347`; routed by sensitivity, a secret is never saved |
| 9 | `session_search` | Safe | (none) | `session_search.rs` |
| 10 | `system_status` | Safe | (none) | `system_status.rs` |
| 11 | `list_cron_jobs` | Safe | (none) | `cron.rs:286-335` |
| 12 | `manage_cron` | Dangerous | (none) | `cron.rs:336-510`; Once-only, never `accept_edits`-covered |
| 13 | `fetch_url` | **External** | WebResearch | `fetch.rs:57-130`; SSRF-guarded, destination surfaced |
| 14 | `ask_human` | Safe | (none) | `ask_human.rs:49-129`; blocks the loop |
| 15 | `search_skills` | Safe | (none) | `skills.rs:61-121` |
| 16 | `save_skill` | **Dangerous** | (none) | `skills.rs:125-252`; content-showing Once prompt |
| 17 | `delegate` | **Dangerous** | (none) | `delegate.rs:46-211`; enqueues, never runs inline |
| 18 | `shell_exec` | **Dangerous** | Shell | `exec.rs:411-571`; Seatbelt-sandboxed, per-profile roots |

Not registered anywhere in `build_tool_dispatcher` (dormant): the MCP tool spine
(`mcp.rs`) and the computer-use action model (`computer_use.rs`).

## Key types / traits / functions

- `Capability` — `mod.rs:50-74`. Enum of environment capabilities (`Filesystem`,
  `Network`, `Shell`, `Display`, `Audio`, `ComputerUse`, `Email`, `Calendar`,
  `WebResearch`, `LongCompute`).
- `BodyEnv` — `mod.rs:82-141`. `BodyEnv::app_default()` (`mod.rs:106-116`) is what
  the Tauri desktop app uses (Filesystem, Network, Shell, Display, Audio,
  ComputerUse, WebResearch — no `Email`/`Calendar`/`LongCompute`).
  `BodyEnv::headless_server_default()` (`mod.rs:121-130`) is the companion-server
  shape (no Display/Audio/ComputerUse; has Email/Calendar/LongCompute) — still
  unused in the product today; no code builds a headless dispatcher outside
  `tools/tests.rs`. `has_all()` (`mod.rs:138-140`) is a strict set-intersection
  check, not "any of."
- `ConversationReads` — `mod.rs:181-202`. Per-conversation set of canonical paths
  the agent has `read_file`'d this session — the state behind the read-before-write
  guard (see Invariants). `record`/`contains`, keyed by conversation id, values are
  **canonicalized** paths so a read and a later write agree on identity regardless
  of how the path was spelled.
- `ExecCtx` — `mod.rs:210-244`. Carries `conversation_id`, `profile`,
  `reads: Option<Arc<ConversationReads>>`, `allow_private_memory` (stamped
  `!is_cloud` by the dispatcher — safe default `false`), `session_mode`,
  `caller_provider_id`/`caller_model` (Wave 4.3c: what `delegate`'s
  seat-inherit-fallback resolves against), `binding` (stamped by the dispatcher so
  `delegate` can make a helper inherit it).
- `RiskClass` — `mod.rs:253-276`. `Safe | Write | External | Dangerous`. **All four
  are live** — this is a fix to a stale claim in an earlier version of this doc,
  which said `External`/`Dangerous` were "reserved, nothing constructs them."
  `External` is constructed by `fetch_url` (`fetch.rs:87-90`) and by
  `mcp::mcp_risk` for a remote MCP server (`mcp.rs:108-109`, unwired but real
  logic). `Dangerous` is constructed by `shell_exec` (`exec.rs:482-484`),
  `manage_cron` (`cron.rs:362-366`), `save_skill` (`skills.rs:151-156`), and
  `delegate` (`delegate.rs:75-83`).
- `trait Tool` — `mod.rs:285-354`. Required: `name()`, `requires() -> &[Capability]`,
  `run(input, ctx) -> Pin<Box<dyn Future<Output = ToolResult> + Send>>`. Defaulted:
  `description()` (empty), `risk()` (defaults `Safe` — **every mutating tool must
  override this explicitly**), `schema()` (permissive object — native tool-use
  endpoints consume it verbatim), `destination()` (`None` — for `External` tools,
  the human-readable egress target surfaced in the approval dialog, e.g.
  `fetch_url`'s URL host, `fetch.rs:105-108`), `available()` (default:
  `env.has_all(self.requires())`), `match_text()` (defaults to the canonical
  `"{name} {args}"` — `shell_exec` overrides it to the bare decoded command,
  `exec.rs:493-498`, so the sandbox/permission pattern hooks match on the real
  command, not its JSON envelope).
- `ToolRegistry` — `mod.rs:363-431`. `register()` (`mod.rs:375-377`, converts a
  `Box<dyn Tool>` to `Arc<dyn Tool>` internally), `get(name)` (ignores
  availability, `mod.rs:409-414`), `all_names()` (`mod.rs:418-420`, every
  registered name regardless of env — feeds `ToolDispatcher::headless()`),
  `available_tools(env)` (filters by `Tool::available`, preserves registration
  order, `mod.rs:424-430`).
- `ToolRegistry::restricted_to(allowed)` — `mod.rs:387-396`. A bounded sub-registry
  sharing the same `Arc`'d tool instances (no rebuild): the effective belt is
  `allowed ∩ registered`, an **intersection, never a widening** — a name in
  `allowed` but not registered yields nothing, and a registered tool not in
  `allowed` is physically absent from the result, so it can't be listed
  (`available_tools`/catalog) or looked up (`get`) — enforcement is the
  registry's *contents*, not a filter some call site might skip.
- `ToolCall` / `ParsedToolCall` — `calling.rs:37-50`. `ParsedToolCall::Malformed`
  surfaces bad JSON rather than silently dropping it.
- `parse_tool_calls(own: &crate::models::OwnOutput) -> Vec<ParsedToolCall>` —
  `calling.rs:111-139`. Scans lines for an opening fence matching ` ```tool `
  **exactly** (case-insensitive after trim), collects until the closing ` ``` `,
  JSON-decodes each body. **The safety contract is enforced at the type level**:
  the parameter is `&OwnOutput` (a newtype in `models::client`, `client.rs:61`,
  whose only constructor `OwnOutput::from_stream_assembly` is `pub(crate)`,
  `client.rs:70`), and the agent loop mints one exactly once, right after the
  SSE-delta assembly loop (`agent/loop_mod.rs:1419`). A bare `&str` from a tool
  result, web page, or history is a type error. The one caller in the tree,
  `ToolDispatcher::run_turn`, honors this (`dispatch.rs:719`).
- `assemble_native_calls(fragments) -> Vec<ParsedToolCall>` — `calling.rs:59-95`.
  The **other** transport: when a provider supports native structured tool-calling
  (`provider.supports_native_tools`) and the turn built a native tool spec, the
  agent loop assembles the provider's streamed call fragments into
  `ParsedToolCall`s here instead of running the fenced parser at all
  (`agent/loop_mod.rs:1525-1537`) — a native turn never invokes `parse_tool_calls`,
  so there's no second listener for a forged fence on that path. Both transports
  converge on the same `ParsedToolCall` shape and the same `ToolDispatcher::drive`,
  so gating is identical regardless of which one produced the call.
- `render_tool_catalog(tools: &[&dyn Tool]) -> String` — `calling.rs:169-204`. Builds
  the system-prompt fragment teaching the dialect + rules + tool list. Returns
  `""` for an empty slice.
- `neutralize_untrusted(s: &str) -> String` — `calling.rs:231-236`. Replaces
  ` ``` ` → `'''`, `[UNTRUSTED TOOL OUTPUT` → `[untrusted-tool-output`,
  `[END UNTRUSTED TOOL OUTPUT]` → `[end-untrusted-tool-output]`, `LH-UNTRUSTED` →
  `lh-untrusted`.
- `guard_wrap(source, body) -> String` — `calling.rs:246-249`. Wraps untrusted
  content in a labeled block with a **random** per-call nonce
  (`<<<LH-UNTRUSTED:{uuid} … LH-UNTRUSTED:{uuid}>>>`) after neutralizing both
  `source` and `body`.
- `guard_wrap_stable(source, body, seed) -> String` — `calling.rs:260-266`. Same
  wrapper but with a **deterministic** nonce (SHA-256 of `seed`), so
  `(source, body, seed)` wraps byte-identically every call — used for the
  cache-stable curated-memory block (seed = conversation id) so the prompt prefix
  is reused across a conversation's turns. Safe despite the predictable nonce: the
  seed is an unguessable conversation uuid a poisoned fact's author never saw, and
  `neutralize_untrusted` still strips any `LH-UNTRUSTED` markers in the body
  regardless.
- `ToolDispatcher` — `dispatch.rs:147-184`. Owns `registry`, `chain: HookChain`,
  `env: BodyEnv`, `ledger: Arc<ApprovalLedger>`, `approver: Option<Arc<dyn
  ApprovalPrompter>>`, `reads: Arc<ConversationReads>`, `run_state: Mutex<RunState>`
  (budgets/repeat-detection), `audit_writer: Option<Arc<dyn AuditWriter>>`,
  `rule_writer: Option<Arc<dyn ToolRuleWriter>>`.
  - `new(registry, chain, env)` — `dispatch.rs:187-199`, empty ledger + no
    approver.
  - `restricted(&self, allowed) -> ToolDispatcher` — `dispatch.rs:231-243`. A
    bounded sub-dispatcher for a delegated helper: `registry.restricted_to(allowed)`,
    but the **same** `chain` (cloned `Arc`s), `ledger`, `reads`, `audit_writer`,
    `rule_writer` — so a helper's calls pass the identical gate the parent's do,
    never a weaker one. Fresh `run_state` (its own per-run budget). `approver` is
    always `None` — a delegated helper runs headless (see Gotchas).
  - `headless(&self) -> ToolDispatcher` — `dispatch.rs:227-229`. `self.restricted(&self.registry.all_names())`
    — the full tool belt, but still headless (`approver: None`). Meant for an
    unattended cron/server run.
  - `with_approval(ledger, approver)` — `dispatch.rs:248-256`. `ledger` must be the
    *same* `Arc` passed to `build_pretooluse_chain_full`.
  - `with_audit_writer(writer)` / `with_rule_writer(writer)` — `dispatch.rs:264-275`.
  - `empty()` — `dispatch.rs:336-338`. No tools/no hooks; used where a dispatcher
    is structurally required but never exercised.
  - `begin_run()` — `dispatch.rs:348-352`. Zeroes the per-run dispatch counter and
    clears the repeat-detection ring. Called once per user message, before the
    first `run_turn`/`run_turn_native` of that run.
  - `async fn dispatch(&self, call, ctx, binding, is_cloud) -> ToolOutcome` —
    `dispatch.rs:419-430`, a thin wrapper over `dispatch_inner` that always fires
    one post-tool-use audit entry (`fire_audit`, `dispatch.rs:288-331`) on every
    return path.
  - `dispatch_inner` — `dispatch.rs:436-695`. The resolve → availability → gating →
    execute pipeline (see Data flow).
  - `async fn run_turn(&self, own_output: &OwnOutput, ctx, binding, is_cloud) -> TurnOutcome`
    — `dispatch.rs:712-725`. Fenced-dialect entry point.
  - `async fn run_turn_native(&self, calls: Vec<ParsedToolCall>, ctx, binding, is_cloud) -> TurnOutcome`
    — `dispatch.rs:395-407`. Native-transport entry point; same driver as `run_turn`.
  - `async fn drive(...)` — `dispatch.rs:757-917`. The shared driver: per-turn call
    ceiling, per-run dispatch ceiling + repeat detection, deny-cascade, and the
    `NeedsLocalReroute` early-return (see Data flow and Invariants).
  - `deny_and_continue_turn` / `resume_after_local_switch` — `dispatch.rs:926-955`,
    `dispatch.rs:963-1002`. The two ways the agent loop resolves a
    `TurnOutcome::NeedsLocalReroute` and continues driving the rest of the batch.
- `ToolOutcome` — `dispatch.rs:74-100`. `Ok(Value) | Err(String) | Denied{by,reason}
  | Ask{by,prompt} | Unavailable(String) | Unknown(String) | NeedsLocalReroute{reason}`
  — every non-`Ok` variant is a distinct, explainable reason, not a silent nothing.
  `NeedsLocalReroute` is typed distinctly from `Denied` so the caller (which owns
  providers) can try to switch to a local endpoint and re-issue the call instead
  of just failing.
- `TurnOutcome` — `dispatch.rs:106-131`. `NoToolCalls | Feedback(ChatMessage) |
  NeedsLocalReroute{reason, call, prior_sections, remaining, turn_call_count,
  cascade_active}` — the last variant carries enough turn-local state
  (`turn_call_count`, `cascade_active`) that a reroute continuation resumes the
  *same* turn's budget/cascade accounting rather than restarting it.
- `format_outcome(name, outcome) -> String` — `dispatch.rs:1015-1047`. Guard-wraps the
  tool's actual returned data (`Ok`); runs every interpolated
  error/reason/prompt/tool-name string through `neutralize_untrusted` (not full
  `guard_wrap`).
- `profile_workspace_path(base, profile) -> PathBuf` — `fs.rs:98-108`. The
  per-profile workspace root: `base.join(profile)` after a denylist check
  (rejects empty, `/`, `\`, `..`, a leading `.`) — **no trim**. This mirrors
  `Storage::open_profile`'s denylist byte-for-byte on purpose: `open_profile` keys
  a distinct `profiles/<name>.db` off the *raw* (untrimmed) name, so a trimmed
  resolver here would bucket `" work"`/`"work "`/`"work"` into one filesystem tree
  while `open_profile` treats them as three distinct profiles. Called at every fs
  tool's, `shell_exec`'s, and `ProtectedPathHook`'s call site to resolve the
  active profile's subtree.
- `migrate_legacy_workspace(workspace_root, default_profile)` — `fs.rs:154-261`.
  One-time, idempotent (content-checked marker, `fs.rs:110-120`) migration of the
  pre-Tier-P shared `workspace/*` into the default profile's subtree. **Moves
  regular files ONLY, never a directory** — after Tier-P the root only ever holds
  per-profile directories + the marker, so a loose file can only be legacy data,
  while a directory is inherently ambiguous (live tree vs. orphaned vs. legacy
  subdir) and is left in place + logged rather than guessed at. Uses
  `symlink_metadata` (lstat, no follow) to detect a collision at the destination
  so a dangling destination symlink can't be silently clobbered (`fs.rs:219`).
  Called once at boot (`lib.rs:470-476`), before any tool is registered.
- `resolve_within(root, rel)` / `resolve_within_new(root, rel)` — `fs.rs:39-64`,
  `fs.rs:537-584`. `resolve_within` requires the target to already exist
  (canonicalizes it directly); `resolve_within_new` canonicalizes the *parent*
  instead (for a write target that may not exist yet) and explicitly refuses to
  write through an existing symlink leaf (`fs.rs:576-582`). Both reject absolute
  paths and any `..` component and require the canonical result to start with the
  canonical root.
- `atomic_write(target, content)` — `fs.rs:615-638`. Writes a `.{name}.tmp-{uuid}`
  file in the same directory, then `rename()`s over the target; cleans up the
  temp file on any failure (write or rename).
- `canonicalize_best_effort(root, rel)` — `fs.rs:606-610`. `resolve_within`,
  falling back to `resolve_within_new`, so `ProtectedPathHook` can peek at a
  call's real on-disk target using the SAME symlink-resolution algorithm the fs
  tools use, without duplicating it.

## Data flow / how it fits

1. **Startup wiring** (`lib.rs:433-647`, `build_tool_dispatcher`): creates
   `<storage>/workspace/`, runs `migrate_legacy_workspace`, registers all 18 tools
   (fs × 6, memory × 2, session_search, system_status, cron × 2, fetch_url,
   ask_human, skills × 2, delegate, shell_exec) into a `ToolRegistry`, builds
   `BodyEnv::app_default()`, then derives an `InMemoryPolicySource` from each
   tool's `risk()` — `Safe` → `PermissionMode::Allow` + pre-trusted (skips
   first-use confirm too); `Write`/`External`/`Dangerous` → `PermissionMode::Ask`.
   That default is then **layered** (`LayeredPolicySource`) under the persisted
   per-profile `tool_rules` store (`SqlitePolicySource`) so a durable "Always
   allow" rule participates in the same resolution. Builds the pretooluse
   `HookChain` via `hooks::build_pretooluse_chain_full` and registers an
   `AuditObserverHook` as an observer (see the hooks doc for the chain's own
   contents — it now includes `SessionModeHook`, which this subsystem doesn't own).
2. **Model turn → tool call, two transports, one driver**: the agent loop either
   (a) assembles the model's native streamed tool-call fragments via
   `assemble_native_calls` and calls `run_turn_native`, or (b) feeds the model's
   own freshly-generated text to `run_turn`, which runs `parse_tool_calls` first.
   Both call the shared `drive()`.
3. **`drive()`'s pre-dispatch circuit breakers** (`dispatch.rs:757-917`), all
   denying with `ToolOutcome::Denied{by:"budget"|"batch", ..}` **before**
   `dispatch()` is reached:
   - **Per-turn call ceiling** (`PER_TURN_CALL_CEILING = 8`, `dispatch.rs:48`) —
     every parsed item counts, malformed included; once hit, the remaining items
     in that reply are denied without being attempted and the turn stops early.
   - **Per-run dispatch ceiling** (`PER_RUN_DISPATCH_CEILING = 50`, `dispatch.rs:52`)
     — only calls actually passed to `dispatch()` count, tracked in
     `run_state.dispatch_count`, reset only by `begin_run()` (once per user
     message) — the real cross-turn runaway bound, since one message can drive
     many `run_turn` rounds.
   - **Repeat detection** (`REPEAT_DETECTION_THRESHOLD = 3`, `dispatch.rs:55`) —
     the same `ActionFingerprint` (tool + args) reaching `dispatch()` a 3rd time
     within one run is denied instead of running again; tracked in a capped
     `VecDeque` of recent fingerprints.
   - **Deny-cascade** — once any call in this turn is denied with `by:"user"`
     (an interactive decline), every not-yet-run non-`Safe` call in the *same*
     batch is auto-denied (`by:"batch"`) without prompting; `Safe` reads still
     run. An unresolvable (unknown) tool is treated as non-`Safe` (fail closed).
     Policy/sandbox/privacy-filter denials do **not** trip the cascade — only an
     actual human "no."
   Every circuit-breaker denial is still audited (`fire_audit`) even though it
   never reaches `dispatch()`, so Activity/audit history doesn't have a blind
   spot for these.
4. **Per-call `dispatch_inner`** (`dispatch.rs:436-695`):
   - registry lookup → `Unknown` if absent; `tool.available(&env)` → `Unavailable`
     if the body lacks a required capability.
   - builds a canonical `"{name} {args}"` (feeds `command_text`) and an
     `ActionFingerprint::of(name, args)`.
   - loop up to `MAX_APPROVAL_ROUNDS = 4` (`dispatch.rs:467`): builds a fresh
     `EventContext` (stamping `tool.match_text()` as `command_text`, the call's
     `risk()`, the conversation's `session_mode`, and the active `profile`) and
     runs `chain.run_gating(&mut ev)`.
     - `Continue`/`Allow` → consumes any one-time grant for this fingerprint
       first, then checks `ev.routing.is_local_required() && is_cloud` — if true,
       returns `ToolOutcome::NeedsLocalReroute` (never runs the tool on a cloud
       endpoint). Otherwise injects the shared `ConversationReads` handle and
       `allow_private_memory: !is_cloud` into a fresh `ExecCtx` and calls
       `tool.run(...)`.
     - `Deny(reason)` → `Denied{by, reason}`, tool never runs.
     - `Ask(prompt)` → no `approver` wired ⇒ surface `Ask{by, prompt}` to the
       model (headless/round-1 fallback). An approver wired ⇒ await
       `approver.request(...)`: `Approve(scope, target)` runs the answer through
       `hooks::resolve_grant` (the Q8 grant×risk matrix — see the hooks doc),
       records the (possibly narrowed) grant, and **re-runs the whole chain**;
       `Persist(rule)` (a durable "Always") is honored only for `Write` risk
       (`persist_rule_allowed`), else silently degrades to a one-time grant;
       `Deny`/`Timeout` → `Denied`.
   - Exhausting all 4 rounds is treated as a bug and fails closed.
5. `run_turn`/`run_turn_native` collect one `format_outcome` section per call
   (plus a guard-wrapped "malformed, fix your JSON" section per unparseable
   fenced block) into one `TurnOutcome::Feedback(ChatMessage::user(...))`, or
   pause mid-batch with `TurnOutcome::NeedsLocalReroute` for the caller to
   resolve via `deny_and_continue_turn`/`resume_after_local_switch`.
6. **`shell_exec`'s own execution-layer gating** (separate from the hook chain,
   which only decides *whether* the call may run at all): every call is wrapped
   in `sandbox-exec` on macOS (`MacSeatbeltSpawn`, `exec.rs:302-352`) confined to
   the caller's **per-profile** workspace + tmp subpaths
   (`profile_workspace_path`, re-rooted at `exec.rs:539-540` — without this, one
   profile's shell could `cat ../other_profile/secret.txt` across the shared
   Seatbelt grant). Network is off by default; the call's own `network: true`
   request is a **maximum**, further capped by `effective_network()`
   (`exec.rs:450-469`) reading the caller's profile's `sandbox_config` as a
   **ceiling** — a locked-down profile denies shell network even when the call
   asks for it, and a corrupt/unreadable config fails safe (denies) rather than
   defaulting open. On any non-macOS platform, `UnsupportedSandbox::spawn` always
   returns `ExecError::SandboxApply` — the tool stays registered (the model sees
   it) but can never run unsandboxed.
7. **`fetch_url`'s SSRF guard** (`fetch.rs:134-238`): redirects are followed
   **manually** (client built with `redirect::Policy::none()`) so `ssrf_check`
   re-runs on every hop, not just the first request — each hop is validated for
   scheme (`http`/`https` only), the string-level private-host check
   (`agent::egress::is_private_endpoint`), and a DNS resolution whose every
   resolved IP is checked against a full block-list (loopback, RFC-1918,
   link-local incl. the `169.254.169.254` cloud-metadata address, CGNAT,
   IPv6 ULA/link-local, and IPv4-mapped forms of all of those —
   `is_blocked_ipv4`/`embedded_ipv4` (`fetch.rs:338-361`, `362-431`). A documented
   residual: a DNS-rebind TOCTOU between the resolve-check and reqwest's own
   connection resolve — closing it needs a custom connector pinned to the vetted
   IP.

## Invariants (do NOT break)

- **`parse_tool_calls` must only ever be called on the model's own current-turn
  output.** Enforced at the type level: the parameter is `&OwnOutput`, whose only
  constructor is `pub(crate) OwnOutput::from_stream_assembly` (`models/client.rs:70`),
  called exactly once by the agent loop right after SSE-delta assembly
  (`agent/loop_mod.rs:1419`). A bare `&str` is a type error. On a **native**
  tool-calling turn, `parse_tool_calls` is never invoked at all — there is no
  second listener for a forged fence on that path either, since native calls come
  from the provider's own structured `tool_calls` field, not fenced text.
- **Every mutating tool must override `risk()` to something other than `Safe`.**
  The trait default is `Safe` (`mod.rs:303-305`) so a forgotten override only ever
  *under*-restricts a read tool's own claims — but `build_tool_dispatcher` trusts
  `risk()` to derive gating, so a mislabeled write tool would be pre-trusted and
  skip approval entirely.
- **Untrusted content must be `guard_wrap`ped (or at minimum `neutralize_untrusted`d)
  before it re-enters model context.** `format_outcome` guard-wraps `Ok` payloads
  and neutralizes every other interpolated string — see the "smuggled fence"
  regression test covering a forged fence hidden inside an unknown tool's *name*
  field (`dispatch.rs:1634-1650`).
- **Filesystem tools are workspace-confined, per-profile.** `resolve_within`/
  `resolve_within_new` reject absolute paths, any `..` component, and (via
  canonicalize) symlink escapes; every fs tool first resolves `profile_workspace_path`
  before either resolver runs, so a `work`-profile call can never resolve into
  `personal`'s subtree. Any new fs tool must route through both steps.
- **`atomic_write` never leaves a half-written file or orphaned temp file** —
  both the temp-write failure path and the rename failure path clean up the temp
  file (`fs.rs:630-637`).
- **`edit_file` requires a unique match.** Zero or ambiguous (>1) matches is an
  error and the file is left untouched.
- **The read-before-write guard IS enforced** (a fix to a stale claim in an
  earlier version of this doc). `WriteFileTool` (on an *existing* file only —
  a brand-new file is exempt) and `EditFileTool` both refuse to touch a path the
  agent hasn't `read_file`'d in this conversation (`fs.rs:701-723`,
  `fs.rs:804-813`), tracked via the dispatcher-owned, `Arc`-shared
  `ConversationReads` injected into `ExecCtx.reads` at the `tool.run` call site
  (`dispatch.rs:527-535`). `MAX_READ_BYTES` is deliberately kept in lockstep with
  `MAX_WRITE_BYTES` (`fs.rs:26`) specifically so there's no "writable but
  unreadable" dead zone that would make the guard permanently un-satisfiable for
  a file between the two old caps.
- **The must-stay-local routing floor is enforced at dispatch, not just
  annotated.** `PrivacyFilterHook` only sets `ev.routing`; `dispatch_inner` is
  what actually refuses to run a `LocalRequired` call on a cloud endpoint,
  returning `NeedsLocalReroute` rather than a silent no-op.
- **A `Deny` from any gating hook means `Tool::run` is never called** — proven by
  `sandbox_denied_call_never_runs_the_tool` (`dispatch.rs:1193`).
- **A one-time (`Once`) approval grant is consumed the instant gating passes,
  before the local-required routing check**, so it can't remain armed to silently
  cover a later identical call if this particular run gets refused for an
  unrelated reason.
- **`ToolRegistry::restricted_to` is a structural intersection, never a
  widening.** An out-of-belt tool is physically absent from the sub-registry, so
  it can't be listed or looked up by a delegated helper or a headless
  sub-dispatcher — proven by `restricted_dispatcher_refuses_an_out_of_belt_tool`
  (`dispatch.rs:1224`) and `restricted_dispatcher_still_enforces_the_full_gate`
  (`dispatch.rs:1253`).
- **`shell_exec` fails closed on a sandbox-apply failure** — `ExecError::SandboxApply`/
  `Io` are both hard tool errors; there is no code path from a spawn failure to a
  bare, unsandboxed `Command::new` (`exec.rs:1-16` module doc, enforced by
  `UnsupportedSandbox` on every non-macOS platform).

## Gotchas / watch-items

- **`RiskClass::External`/`Dangerous` are fully live, not reserved.** An earlier
  version of this doc claimed nothing constructed them; that was already false
  and is more false now. `External` gates `fetch_url` and (unwired) remote MCP
  tools; `Dangerous` gates `shell_exec`, `manage_cron`, `save_skill`, and
  `delegate`.
- **`shell_exec`'s per-profile `sandbox_config` network ceiling is live code but
  currently unreachable in practice.** `effective_network()` (`exec.rs:450-469`)
  correctly reads `Storage::open_profile(profile).get_sandbox_config()` and
  enforces it as a ceiling on every call — the logic is real and tested
  (`exec.rs:727-…`, `shell_exec_applies_the_per_profile_sandbox_config_network_ceiling`).
  But **no IPC command or UI writes `sandbox_config`** — grepping
  `set_sandbox_config` outside tests turns up only its definition
  (`storage/profile.rs:1361`), never a caller. So today every profile reads back
  `None` (unconfigured → `effective_network` returns "unconstrained," i.e.
  today's pre-Tier-K behavior) unless a test seeds it directly.
- **MCP (`mcp.rs`) is a built, tested trust/gating spine with no live wire
  client.** `McpTool` folds into the registry with zero special-casing and
  `mcp_risk`/`mcp_capabilities` correctly derive risk/capabilities from a
  server's declared trust tier — but `UnwiredTransport` is the only
  `McpTransport` impl, it fails every call, and `build_tool_dispatcher` never
  registers an MCP server (no persisted server-config store or registration UI
  exists). The whole module carries `#![allow(dead_code)]`.
- **`computer_use.rs` is dormant** — `ActionTarget`/`ComputerAction`/
  `Reversibility`/`risk_class` are fully built and tested but there is no `Tool`
  impl wrapping them and nothing registers them in `lib.rs`; grepping
  `computer_use::` outside the file itself returns nothing. It's the
  action/risk-classification half of M5, waiting on the native per-OS backends.
- **`migrate_legacy_workspace` moves regular files only, never a directory** —
  a legacy top-level directory (a live profile tree, an orphaned tree, or a
  legacy subdir) is left in place and logged, never guessed at, because no
  name-based heuristic can safely disambiguate the three cases (`fs.rs:132-145`).
- **`recall_memory` is not strictly "shared-store-only."** It always searches the
  active profile's shared store, but it can *also* surface private-local facts
  when `ctx.allow_private_memory` is true (a non-cloud turn, stamped
  `!is_cloud` by the dispatcher) **and** the fact's origin profile matches the
  active one — a private-local fact never crosses the profile boundary, and a
  cloud turn always stays shared-only (`memory.rs:176-188`).
- **`delegate` snapshots the persona's toolbelt and seat at enqueue time, not at
  run time.** The `work_items` payload embeds `persona.tools_allowlist` and the
  already-resolved `(provider, model)` (`delegate.rs:173-182`) — a later edit to
  the persona's toolbelt or seat binding doesn't retroactively change an
  already-queued dispatch. The helper also **inherits the parent turn's
  `binding`** verbatim (Private stays Private) — never silently downgraded to
  `Auto`/cloud (`delegate.rs:166-172`, tested at `delegate.rs:250-282`).
- **A delegated helper's sub-dispatcher (`restricted`) and a headless cron
  sub-dispatcher (`headless`) both run with `approver: None`.** An `Ask` from the
  chain is surfaced as "not granted this round" rather than raising a prompt —
  deliberately, so a background task can't (a) surprise-prompt for up to the
  5-minute deny-default, or (b) record an interactive grant into the **shared**
  ledger that would silently authorize the parent and every sibling helper. Only
  pre-authorized `tool_rules` (resolved to `Allow` before any `Ask`) still apply.
- **Two tool-calling transports converge on one gate, but they're not
  symmetric in one respect**: a native turn assembles calls via
  `assemble_native_calls` and skips `parse_tool_calls` entirely; a fenced turn
  goes through the type-checked `OwnOutput` path. Both end up in the same
  `drive()`/`dispatch()`, so gating, budgets, and guard-wrapping are identical —
  but if you're auditing "does anything call `parse_tool_calls` on untrusted
  content," remember the native path doesn't call it at all, by construction,
  not by discipline.
- **`ToolDispatcher::empty()` has no gating chain at all** — fine for tests/
  contract-only scaffolding, but wiring it anywhere that dispatches a real tool
  would skip every gate.
- **`MAX_READ_BYTES`/`MAX_WRITE_BYTES`/search bounds are hardcoded consts in
  `fs.rs`**, not config-driven.
- **`search_files`'s content-match only looks for the substring on the *first*
  matching line** (`fs.rs:504-511`, `.find(...)` via `.lines().enumerate().find(...)`)
  — it won't report every match within a file.
- **`MAX_APPROVAL_ROUNDS = 4`** (`dispatch.rs:467`) is a backstop against a
  misbehaving prompter, not a normal path — a normal approve flow settles in 2
  passes (ask, then re-run once granted).
- **The per-turn/per-run/repeat-detection state is a single mutable slot**
  (`run_state: Mutex<RunState>`), safe only because `AgentLoop::stream_lock`
  serializes `process_message` so exactly one run is ever in flight against a
  given dispatcher (`dispatch.rs:164-170`). If concurrent runs are ever allowed
  against one dispatcher, this needs to become per-conversation-keyed.

## How to extend

- **Add a new tool**: implement `Tool` (any `fs.rs`/`cron.rs`/`skills.rs` tool as
  a template), pick the right `risk()` (default `Safe` is only correct for pure
  reads), declare `requires()` honestly, then register it in `build_tool_dispatcher`
  (`lib.rs:484-588`) — gating is then automatic via the `risk()`-driven policy loop
  (`lib.rs:598-608`). Add unit tests alongside the tool's own file's
  `#[cfg(test)] mod tests`.
- **Wire MCP for real**: implement a real `McpTransport` (spawn stdio children /
  SSE / HTTP JSON-RPC, a `tools/list`+`tools/call` handshake) alongside
  `UnwiredTransport`, add a persisted server-config store + registration UI, and
  have `build_tool_dispatcher` (or an equivalent runtime path) register the
  resulting `McpTool`s into the registry like any other tool — no changes needed
  to `ToolRegistry`/`ToolDispatcher` themselves, by design.
- **Wire computer-use for real**: build the native per-OS backend (macOS
  AX/CGEvent/ScreenCaptureKit first), wrap `ComputerAction`/`ActionTarget` in a
  `Tool` impl that calls `computer_use::risk_class`/`reversibility`, and register
  it gated on `Capability::ComputerUse`. The action/risk model in
  `computer_use.rs` needs no changes to support this — it was built ahead of the
  backend deliberately.
- **Add a new capability**: add a variant to `Capability` (`mod.rs:50-74`), then
  decide whether `BodyEnv::app_default()`/`headless_server_default()` should
  grant it.
- **Add a new `ToolOutcome`/denial reason**: extend the enum (`dispatch.rs:74-100`)
  and add a match arm in `format_outcome` (`dispatch.rs:1015-1047`) — any
  interpolated model/tool-controlled string must go through `neutralize_untrusted`.
- **Change gating behavior for a class of tools**: not in this subsystem — it's
  the `risk()` → `PermissionMode` derivation in `build_tool_dispatcher`
  (`lib.rs:598-608`) plus the hook chain in `crate::hooks`. See
  `docs/codebase/hooks-gating-and-approval.md`.
- **Actually enforce `sandbox_config`'s network ceiling in practice**: add an IPC
  command + Settings UI to write `SandboxConfig`/`SandboxNetworkConfig`
  (`storage/profile.rs:1361`, `set_sandbox_config`) — `effective_network()`
  already reads and enforces it correctly the moment a row exists.

## Tests

- `src-tauri/src/tools/tests.rs` (registry + `restricted_to` intersection
  semantics + capability filtering) and inline `#[cfg(test)] mod tests` blocks at
  the bottom of `calling.rs` (dialect parsing, native-fragment assembly,
  guard-wrap/neutralize), `fs.rs` (path safety, atomic write, unique-edit,
  symlink refusal, read-before-write, per-profile workspace resolution,
  migration), `exec.rs` (sandbox-apply failure closes, timeout, the per-profile
  network ceiling test), `fetch.rs` (SSRF block-list, redirect re-validation),
  `cron.rs` (`cron_due` matcher), `delegate.rs` (binding inheritance, unapproved
  agent type refusal), `skills.rs`, `memory.rs`, `mcp.rs` (trust-tier risk
  derivation, `UnwiredTransport` failing loud), `computer_use.rs`
  (reversibility/risk classification), and `dispatch.rs` (dispatch outcomes,
  budgets, repeat detection, deny-cascade, sandbox-deny-never-runs, cloud/local
  reroute, restricted/headless sub-dispatchers, the full interactive-approval
  pause/resume flow via `MockPrompter`, the smuggled-fence regression).
- Run just this subsystem:
  `cd /Users/hayai/Desktop/lost-harness-product/src-tauri && cargo test tools::`
- Run everything (recommended before landing a change here, since `dispatch.rs`
  tests pull in `crate::hooks` and `crate::agent::gate`):
  `cd /Users/hayai/Desktop/lost-harness-product/src-tauri && cargo test`

---
*Verified against `src-tauri/src` at HEAD `ca54251` (2026-07-21): every file/line
reference above was read directly, not inferred. 542 lib tests passing at that
commit. If you change this subsystem materially, update this doc in the same
change — a wrong doc is worse than none.*
