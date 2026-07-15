# Tools subsystem (`src-tauri/src/tools/`)

- **Purpose** — Defines what a "tool" is (`Tool` trait + `Capability`/`RiskClass`
  metadata), the fenced text dialect small local models use to call tools plus
  the untrusted-content guard-wrapping that stops prompt injection from forging
  calls, the dispatcher that resolves/gates/executes a call end-to-end
  (including the interactive-approval pause/resume loop), and the first real
  tool implementations (workspace-confined file read/list/search/write/edit/
  delete).

## Files

- `src-tauri/src/tools/mod.rs` — `Capability`, `BodyEnv`, `ToolInput`/`ToolResult`/`ExecCtx`,
  `RiskClass`, the `Tool` trait, `ToolRegistry`. Also three trivial example tools
  (`EchoTool`, `ScreenshotTool`, `SyncFileTool`) used only in tests to exercise
  capability filtering — not real product tools.
- `src-tauri/src/tools/calling.rs` — the fenced ` ```tool ` dialect: `parse_tool_calls`,
  `ParsedToolCall`, `render_tool_catalog` (system-prompt fragment listing available
  tools), `guard_wrap`/`neutralize_untrusted` (injection defense for untrusted content
  re-entering the model's context).
- `src-tauri/src/tools/dispatch.rs` — `ToolDispatcher`: `dispatch()` (resolve → availability
  → gating chain → approval pause/resume → execute) and `run_turn()` (parse the model's own
  output, dispatch every call, build the feedback message). `ToolOutcome` enum, `format_outcome()`.
- `src-tauri/src/tools/fs.rs` — `ReadFileTool`, `ListDirTool`, `SearchFilesTool` (read-only,
  `RiskClass::Safe`) and `WriteFileTool`, `EditFileTool`, `DeleteFileTool` (state-changing,
  `RiskClass::Write`). Path-safety helpers `resolve_within` / `resolve_within_new`, `atomic_write`.
- `src-tauri/src/tools/tests.rs` — registry/capability-filtering unit tests (`mod tests` at the
  bottom of `mod.rs`).
- Wiring lives outside this dir: `src-tauri/src/lib.rs:198-255` (`build_tool_dispatcher`) —
  the one place that registers the real fs tools, derives a `PermissionMode` policy from each
  tool's `risk()`, and builds the pretooluse hook chain (`crate::hooks`) the dispatcher runs
  calls through.

## Key types / traits / functions

- `Capability` — `mod.rs:34-58`. Enum of environment capabilities (`Filesystem`, `Network`,
  `Shell`, `Display`, `Audio`, `ComputerUse`, `Email`, `Calendar`, `WebResearch`, `LongCompute`).
- `BodyEnv` — `mod.rs:66-125`. A capability set for "what can this running body actually offer."
  `BodyEnv::app_default()` (`mod.rs:90-100`) is what the Tauri desktop app uses (Filesystem,
  Network, Shell, Display, Audio, ComputerUse, WebResearch — note: no `Email`/`Calendar`/
  `LongCompute`). `BodyEnv::headless_server_default()` (`mod.rs:105-114`) is the companion-server
  shape (no Display/Audio/ComputerUse; has Email/Calendar/LongCompute). `has_all()` is a strict
  set-intersection check, not "any of."
- `RiskClass` — `mod.rs:172-181`. `Safe | Write | External | Dangerous`. `External`/`Dangerous`
  are declared but nothing in the tree currently constructs them — reserved for future tools
  (network egress, email send, etc).
- `trait Tool` — `mod.rs:190-229`. Required: `name()`, `requires() -> &[Capability]`,
  `run(input: ToolInput, ctx: &ExecCtx) -> Pin<Box<dyn Future<Output = ToolResult> + Send>>`.
  Defaulted: `description() -> &str` (empty), `risk() -> RiskClass` (defaults to `Safe` —
  **every mutating tool must override this explicitly**, since the default only errs safe for
  reads), `available(&self, env) -> bool` (default: `env.has_all(self.requires())`).
- `ToolRegistry` — `mod.rs:236-278`. `register()`, `get(name)` (ignores availability),
  `available_tools(env)` (filters by `Tool::available`, preserves registration order).
- `ToolCall` / `ParsedToolCall` — `calling.rs:37-50`. `ParsedToolCall::Malformed { raw, error }`
  surfaces bad JSON rather than silently dropping it, so the loop can tell the model to retry.
- `parse_tool_calls(own_output: &str) -> Vec<ParsedToolCall>` — `calling.rs:64-91`. Scans lines
  for an opening fence matching ` ```tool ` **exactly** (case-insensitive after trim — not
  ` ```json ` or any other fence), collects until the closing ` ``` `, JSON-decodes the body.
  **The safety contract is entirely at the call site**: this function will parse whatever string
  it's given, so the caller must pass only the model's own freshly-generated current-turn text.
  The one caller in the tree, `ToolDispatcher::run_turn`, honors this (`dispatch.rs:271`).
- `render_tool_catalog(tools: &[&dyn Tool]) -> String` — `calling.rs:121-148`. Builds the
  system-prompt fragment teaching the dialect + rules + tool list. Returns `""` for an empty
  slice so the caller can skip adding a system message.
- `neutralize_untrusted(s: &str) -> String` — `calling.rs:162-167`. Replaces ` ``` ` → `'''`,
  `[UNTRUSTED TOOL OUTPUT` → `[untrusted-tool-output`, `[END UNTRUSTED TOOL OUTPUT]` →
  `[end-untrusted-tool-output]`, `LH-UNTRUSTED` → `lh-untrusted`.
- `guard_wrap(source: &str, body: &str) -> String` — `calling.rs:177-188`. Wraps untrusted
  content in a labeled, nonce-delimited block (`<<<LH-UNTRUSTED:{uuid} … LH-UNTRUSTED:{uuid}>>>`)
  after running both `source` and `body` through `neutralize_untrusted`.
- `ToolDispatcher` — `dispatch.rs:61-295`. Owns `registry`, `chain: HookChain`, `env: BodyEnv`,
  `ledger: Arc<ApprovalLedger>`, `approver: Option<Arc<dyn ApprovalPrompter>>`.
  - `new(registry, chain, env)` — `dispatch.rs:76-84`, empty ledger + no approver (round-1 /
    headless fallback behavior).
  - `with_approval(ledger, approver)` — `dispatch.rs:89-97`, wires interactive approval; `ledger`
    must be the *same* `Arc` passed to `build_pretooluse_chain_full`.
  - `empty()` — `dispatch.rs:102-104`, no tools/no hooks, used where a dispatcher is structurally
    required but never exercised.
  - `catalog() -> String` — `dispatch.rs:108-110`.
  - `async fn dispatch(&self, call: &ToolCall, ctx: &ExecCtx, binding: Binding, is_cloud: bool) -> ToolOutcome`
    — `dispatch.rs:114-255`. The core resolve→gate→execute path (see Data flow below).
  - `async fn run_turn(&self, own_output: &str, ctx: &ExecCtx, binding: Binding, is_cloud: bool) -> Option<ChatMessage>`
    — `dispatch.rs:264-294`. Parses, dispatches every call, joins `format_outcome` sections into
    one `ChatMessage::user(...)`, or `None` if no ` ```tool ` block was found.
- `ToolOutcome` — `dispatch.rs:40-57`. `Ok(Value) | Err(String) | Denied{by,reason} |
  Ask{by,prompt} | Unavailable(String) | Unknown(String)` — every non-Ok variant is a distinct,
  explainable reason, not a silent nothing.
- `format_outcome(name: &str, outcome: ToolOutcome) -> String` — `dispatch.rs:307-329`. Guard-wraps
  the tool's actual returned data (`Ok`); runs the tool `name` and every interpolated
  error/reason/prompt string through `neutralize_untrusted` (not through full `guard_wrap`) since
  those are status lines the harness composes, just with model/tool-controlled substrings spliced in.
- `resolve_within(root, rel) -> Result<PathBuf, String>` — `fs.rs:36-61`. For paths that must
  already exist: rejects absolute paths and any `..` component, then `canonicalize()`s both root
  and target and requires the target's canonical path to start with the canonical root (defeats
  symlink escapes).
- `resolve_within_new(root, rel) -> Result<PathBuf, String>` — `fs.rs:326-373`. For paths that may
  not exist yet (write targets): same `..`/absolute rejection, but canonicalizes the **parent**
  directory (which must exist) instead of the target, then re-attaches the filename, and explicitly
  refuses if the resolved leaf is itself a symlink (`symlink_metadata`/lstat check, `fs.rs:365-371`).
- `atomic_write(target, content) -> Result<(), String>` — `fs.rs:378-401`. Writes a `.{name}.tmp-{uuid}`
  file in the same directory, then `rename()`s over the target; cleans up the temp file on any
  failure (write or rename) so a failed write leaves the workspace untouched.
- Tool structs in `fs.rs`, all constructed with `::new(root: impl Into<PathBuf>)`:
  `ReadFileTool` (`fs.rs:70-129`, `MAX_READ_BYTES` 256 KiB), `ListDirTool` (`fs.rs:134-187`),
  `SearchFilesTool` (`fs.rs:192-244`, bounded by `SEARCH_MAX_DEPTH`=8, `SEARCH_MAX_FILES_SCANNED`=4000,
  `SEARCH_MAX_RESULTS`=50, `SEARCH_MAX_FILE_BYTES`=256 KiB — case-insensitive substring match on
  filename and, for small text files, first matching line), `WriteFileTool` (`fs.rs:407-473`,
  `MAX_WRITE_BYTES` 1 MiB, refuses to clobber a directory, reports `created: bool`), `EditFileTool`
  (`fs.rs:480-560`, requires the `old` substring to match **exactly once** — 0 or >1 matches is an
  error, nothing is written on failure), `DeleteFileTool` (`fs.rs:565-616`, files only, refuses
  directories).

## Data flow / how it fits

1. **Startup wiring** (`lib.rs:198-255`, `build_tool_dispatcher`): creates `<storage>/workspace/`,
   registers the six fs tools into a `ToolRegistry`, builds `BodyEnv::app_default()`, then derives
   an `InMemoryPolicySource` from each tool's `risk()` — `Safe` tools get `PermissionMode::Allow`
   *and* are added to a `pre_trusted` list (skips the first-use confirm too); every `Write`/
   `External`/`Dangerous` tool gets `PermissionMode::Ask`. This derivation means **a new tool's
   gating is automatic from its `risk()` override** — there is no separate place to remember to
   list it as dangerous.
2. Builds the pretooluse `HookChain` via `hooks::build_pretooluse_chain_full(PrivacyGate, policy,
   pre_trusted, ledger)` — chain order is `[PrivacyFilter, Sandbox, Permission, FirstUseConfirm]`
   (see `dispatch.rs:11-13` module doc; the actual chain construction lives in `crate::hooks`, not
   in this subsystem).
3. **Model turn → tool call**: the agent loop feeds the model's own freshly-generated text to
   `ToolDispatcher::run_turn`. `parse_tool_calls` extracts ` ```tool ` blocks (only from that exact
   text, never from history/tool-output/web content).
4. **Per-call dispatch** (`dispatch.rs:114-255`):
   - registry lookup → `Unknown` if absent.
   - `tool.available(&self.env)` → `Unavailable` if the body lacks a required capability.
   - build a canonical `"{name} {args}"` string and an `ActionFingerprint::of(name, args)` (the
     "pin" a one-time approval grant binds to).
   - loop up to `MAX_APPROVAL_ROUNDS = 4`: build an `EventContext::pre_tool_use(...)` and run
     `chain.run_gating(&mut ev)`.
     - `Continue`/`Allow` → consume any one-time grant for this fingerprint (so a `Once` grant
       can't survive to silently authorize a later identical call even if something below denies),
       then check `ev.routing.is_local_required() && is_cloud` — if the privacy filter marked this
       call must-stay-local and we're on a cloud endpoint, **fail closed with `Denied{by:
       "privacy-filter", ...}`** rather than let the annotation be a silent no-op. Otherwise call
       `tool.run(ev.input, ctx).await` and map `ToolResult` → `ToolOutcome`.
     - `Deny(reason)` → `Denied{by, reason}`, tool never runs.
     - `Ask(prompt)` → if no `approver` wired, surface `Ask{by, prompt}` to the model (round-1/
       headless fallback — "not granted this round"). If an approver is wired, call
       `approver.request(...)`: `Approve(scope, target)` records a grant on the shared `ledger`
       and **re-runs the full chain from the top** (so Sandbox/Privacy are re-checked, not just
       Permission); `Deny` → `Denied{by:"user"}`; `Timeout` → `Denied{by:"approval"}` (fails closed).
     - `Modify(_)` is consumed inside `chain.run_gating` itself and can never reach this match arm
       as a terminal result — if it ever does, that's treated as an internal-error bug (`ToolOutcome::Err`).
   - Exhausting all 4 rounds without settling is treated as a bug and fails closed
     (`Denied{by:"approval", reason:"too many confirmation rounds..."}`).
5. `run_turn` collects one `format_outcome` section per parsed call (plus a "malformed, fix your
   JSON" section per unparseable block, itself `guard_wrap`ped) and joins them into a single
   `ChatMessage::user(...)` fed back to the model next turn.
6. **Known deferred concurrency note** (`dispatch.rs:211-217`): `process_message` holds the agent
   loop's stream lock across the `approver.request(...).await`, so while an approval prompt is
   outstanding the app is effectively single-in-flight (a second `send_message` blocks until the
   user answers or it times out). Not a deadlock, just a UX limitation — releasing the lock while
   parked plus a cancel command is deferred future work.

## Invariants (do NOT break)

- **`parse_tool_calls` must only ever be called on the model's own current-turn output.** This is
  the entire defense against a read web page/email/tool-result forging a tool call — it is not
  enforced by the type system, only by caller discipline (`calling.rs:58-63`, `dispatch.rs:14-19`,
  `264-267`). If you add a new call site, it must uphold this.
- **Every mutating tool must override `risk()` to something other than `Safe`.** The trait default
  is `Safe` (`mod.rs:208-210`) specifically so a forgotten override only ever *under*-restricts a
  read tool's own claims, never a write tool's — but `build_tool_dispatcher` trusts `risk()` to
  derive gating, so a mislabeled write tool would be pre-trusted and skip approval entirely.
- **Untrusted content must be `guard_wrap`ped (or at minimum `neutralize_untrusted`d) before it
  re-enters model context.** `format_outcome` guard-wraps `Ok` payloads and neutralizes every other
  interpolated string (tool name, error text, denial reason, ask prompt) — see the "smuggled fence"
  regression test `dispatch.rs:558-577` which specifically covers a forged fence hidden inside an
  unknown tool's *name* field, not just its body.
- **Filesystem tools are workspace-confined.** `resolve_within`/`resolve_within_new` reject
  absolute paths, any `..` component, and (via canonicalize) symlink escapes; `resolve_within_new`
  additionally refuses to write through an existing symlink leaf rather than silently replacing or
  following it (`fs.rs:360-371`). Any new fs tool must route through one of these two resolvers —
  do not hand-roll path joining.
- **`atomic_write` never leaves a half-written file or orphaned temp file.** Both the temp-write
  failure path and the rename failure path clean up the temp file (`fs.rs:393-400`).
- **`edit_file` requires a unique match.** Zero or ambiguous (>1) matches is an error and the file
  is left untouched — this is what makes an LLM-issued edit safe to apply without a diff preview.
- **The must-stay-local routing floor is enforced at dispatch, not just annotated.** `PrivacyGate`
  only sets `ev.routing`; `ToolDispatcher::dispatch` is what actually refuses to run a
  `LocalRequired` call on a cloud endpoint (`dispatch.rs:169-182`). Don't let a refactor move this
  check somewhere it could be skipped.
- **A `Deny` from any gating hook means `Tool::run` is never called** — proven by
  `sandbox_denied_call_never_runs_the_tool` (`dispatch.rs:435-461`) using a `SpyTool` that records
  whether it actually ran.
- **A one-time (`Once`) approval grant is consumed the instant gating passes, before the
  local-required routing check** (`dispatch.rs:159-163`), so it can't remain armed to silently
  cover a later identical call if this particular run gets refused for an unrelated reason.

## Gotchas / watch-items

- **Read-before-write is NOT enforced.** The module docstring for `fs.rs` and the task brief both
  flag this: `EditFileTool`/`WriteFileTool` do not require the caller (model) to have `read_file`d
  the target first in this turn/session. There's no state tracking "has this path been read." If
  you're asked to add that, it needs new state threaded through `ExecCtx` or the dispatcher — there
  is currently nowhere to hang it.
- **`RiskClass::External` and `RiskClass::Dangerous` are unused today** — declared for future tools
  (network egress, email send, shell exec) but no current tool returns them. `build_tool_dispatcher`
  treats them identically to `Write` (both get `PermissionMode::Ask`), so adding an `External` tool
  today gets the same gating as a `Write` tool, not something stricter — if that's not what you
  want, the differentiation needs to be added to `build_tool_dispatcher`'s match arm
  (`lib.rs:236-244`) and/or to the hook chain.
- **`BodyEnv::headless_server_default()` is unused in the product today.** The app body only ever
  builds `BodyEnv::app_default()` (`lib.rs:232`, `Filesystem`/`Network`/`Shell`/`Display`/`Audio`/
  `ComputerUse`/`WebResearch` — notably no `Email`/`Calendar`/`LongCompute`). The headless shape
  (`Email`/`Calendar`/`WebResearch`/`LongCompute`, no `Display`/`Audio`/`ComputerUse`) is the
  mirror-image capability set for a future companion-server body; no code currently constructs a
  headless dispatcher, and it's exercised only by `tools/tests.rs`.
- **`EchoTool`/`ScreenshotTool`/`SyncFileTool` in `mod.rs` are test fixtures, not product tools.**
  Don't mistake them for real capabilities — they're not registered anywhere in `lib.rs`.
- **The fenced dialect matches ` ```tool ` only, case-insensitively, after trim** — a model emitting
  ` ```Tool ` or `  \`\`\`tool  ` (leading/trailing whitespace on the fence line) still parses,
  but ` ```json ` or any other fence never does, by design (`calling.rs:52-56` and the
  `a_plain_code_fence_is_not_a_tool_call` test).
  If a closing fence is never found, `parse_tool_calls` still emits nothing for that block only
  when the body is empty (`calling.rs:83-86`) — an unclosed block *with* content still gets parsed
  as a (likely malformed) call, so a truncated model generation with an open fence isn't silently
  dropped.
- **`neutralize_untrusted` is a fixed string-replace list, not a general escaping scheme.** It
  specifically defangs the four structural tokens the model is taught to trust (fence, both banner
  strings, nonce prefix). If new trust-boundary tokens are introduced elsewhere (e.g. a second kind
  of fence or banner), they must be added here too or they become a new injection vector.
  `render_tool_catalog` teaches the model to trust `[UNTRUSTED TOOL OUTPUT]`/`[END UNTRUSTED TOOL
  OUTPUT]` as the *only* boundary cue — that's exactly what `guard_wrap`/`neutralize_untrusted`
  protect.
- **`ToolDispatcher::empty()` has no gating chain at all** — fine for tests/contract-only scaffolding
  cited in its doc comment, but wiring it into anywhere that dispatches a real tool would skip every
  gate.
- **`MAX_READ_BYTES`/`MAX_WRITE_BYTES`/search bounds are hardcoded consts in `fs.rs`**, not
  config-driven. If a future need calls for per-tool or per-profile limits, this is a straight
  refactor of those consts into parameters.
- **`search_files`' content-match only looks for the substring on the *first* matching line**
  (`fs.rs:293-299`, `.find(...)`) — it won't report every match within a file, only the first one,
  even if the same file could contribute multiple lines.
- **Approval-round loop bound (`MAX_APPROVAL_ROUNDS = 4`)** is a backstop against a misbehaving
  prompter, not a normal path — normal approve flows settle in 2 passes (ask, then re-run once
  granted). If you see this limit being hit in practice, that's a sign the ledger/grant plumbing is
  broken, not that the cap needs raising.

## How to extend

- **Add a new tool**: implement `Tool` (see any `fs.rs` tool as a template), pick the right
  `risk()` (default `Safe` is only correct for pure reads), declare `requires()` honestly, then
  register it in `build_tool_dispatcher` (`lib.rs:219-225`) — gating is then automatic via the
  `risk()`-driven policy loop (`lib.rs:235-245`). Add unit tests alongside the existing ones in
  `fs.rs`'s `#[cfg(test)] mod tests` (or a new sibling file + `mod` declaration if the tool doesn't
  belong in `fs.rs`).
- **Add a new capability**: add a variant to `Capability` (`mod.rs:34-58`), then decide whether
  `BodyEnv::app_default()`/`headless_server_default()` should grant it.
- **Add a new `ToolOutcome`/denial reason**: extend the enum in `dispatch.rs:40-57` and add a match
  arm in `format_outcome` (`dispatch.rs:307-329`) — remember any interpolated model/tool-controlled
  string must go through `neutralize_untrusted`.
- **Change gating behavior for a class of tools**: that logic is NOT in this subsystem — it's the
  `risk()` → `PermissionMode` derivation in `build_tool_dispatcher` (`lib.rs:236-244`) plus the
  actual hook chain in `crate::hooks` (`hooks/permission.rs`, `hooks/sandbox.rs`,
  `hooks/first_use.rs`, `hooks/approval.rs`). This subsystem only *calls* `chain.run_gating`.
- **Add read-before-write enforcement**: would need new per-conversation/per-turn state (a set of
  paths read this turn/session) threaded into `ExecCtx` or held by the dispatcher, checked in
  `WriteFileTool`/`EditFileTool::run` or as a new gating hook. Currently no such state exists
  anywhere in this subsystem.

## Tests

- `src-tauri/src/tools/tests.rs` (registry + capability filtering) and inline `#[cfg(test)] mod
  tests` blocks at the bottom of `calling.rs` (dialect parsing + guard-wrap/neutralize), `fs.rs`
  (path safety, atomic write, unique-edit, symlink refusal), and `dispatch.rs` (dispatch outcomes,
  sandbox-deny-never-runs, cloud/local routing, full interactive-approval pause/resume flow via
  `MockPrompter`).
- Run just this subsystem:
  `cd /Users/hayai/Desktop/lost-harness-product/src-tauri && cargo test tools::`
- Run everything (recommended before landing a change here, since `dispatch.rs` tests pull in
  `crate::hooks` and `crate::agent::gate`):
  `cd /Users/hayai/Desktop/lost-harness-product/src-tauri && cargo test`
