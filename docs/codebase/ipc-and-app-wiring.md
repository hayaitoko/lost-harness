# IPC and App Wiring

- **Purpose** — The Tauri command surface the Svelte frontend calls into
  (`src-tauri/src/ipc/`), the `AppState` it's injected with, the interactive
  tool-approval and ask-human round-trips (`ipc/approval.rs`, `ipc/ask_human.rs`),
  and the `lib.rs::run` bootstrap that builds every long-lived service
  (storage, classifier + embedder, model manager, privacy gate, approval
  spine, tool dispatcher, agent loop, background work runner) and hands them
  to Tauri via `.manage()`. The command surface has grown a lot since this
  doc was last written straight through — **45 commands** registered in
  `lib.rs`'s `invoke_handler!` (`lib.rs:234-279`), not the 5-command M1
  surface an earlier version of this doc described, and `AppState` carries
  **7** `Arc` fields, not 4.

- **Command surface — all 45, grouped by area** (`lib.rs:234-279`; every name
  is a fn in `ipc/mod.rs`):
  - **conversation/chat** (8) — `get_app_version`, `get_active_profile`,
    `set_active_profile`, `list_profiles`, `list_conversations`,
    `create_conversation`, `get_messages`, `send_message`.
  - **providers/models** (4) — `list_providers`, `add_provider`,
    `remove_provider`, `list_models`.
  - **classifier/privacy** (5) — `get_classifier_settings`,
    `set_classifier_settings`, `set_redaction_enabled`,
    `reset_classifier_settings`, `explain_classification`.
  - **memory** (6) — `list_memory`, `save_memory`, `delete_memory`,
    `set_memory_pinned`, `get_memory_settings`, `set_memory_settings`.
  - **skills** (5) — `list_skills`, `set_skill_approval`, `delete_skill`,
    `get_skill_reflect_enabled`, `set_skill_reflect_enabled`.
  - **agent types** (3) — `list_agent_types`, `set_agent_type_approval`,
    `delete_agent_type`.
  - **seats** (3) — `list_seat_bindings`, `set_seat_binding`,
    `delete_seat_binding`.
  - **packs** (1) — `install_pack`.
  - **usage** (1) — `get_usage_summary`.
  - **sandbox/permissions/tool_rules** (3) — `resolve_tool_approval`,
    `list_tool_rules`, `delete_tool_rule`. (`sandbox_config` itself has no
    command at all — see the README's watch-item (a); this bucket is
    permission grants + tool-rule review only.)
  - **cron** (0) — **no IPC command exists.** `tools/cron.rs`'s
    `ListCronJobsTool`/`ManageCronTool` are agent tools only, dispatched
    through the model's fenced tool-call surface — see Gotchas below.
  - **ask_human** (1) — `resolve_ask_human`.
  - **hardware/catalog/download** (5) — `probe_hardware`,
    `list_model_catalog`, `download_model`, `list_local_models`,
    `remove_local_model`.
  - 7+4+5+6+5+3+3+1+1+3+0+1+5 = **44**, matching the `invoke_handler!` list
    exactly (`grep -c "ipc::" ` over that block, minus the macro line itself,
    is a quick sanity check if this drifts).

- **Files**
  - `src-tauri/src/ipc/mod.rs` (1844 lines) — `AppState`, all
    `#[tauri::command]` handlers, request/response DTOs, arg-parsing helpers.
    This is the only place command names are defined.
  - `src-tauri/src/ipc/approval.rs` (228 lines) — `ApprovalRegistry` (parks
    pending tool approvals keyed by request id) + `TauriApprovalPrompter`
    (implements `hooks::ApprovalPrompter`: emits `tool:approval_request`,
    awaits a oneshot with a deny-by-default timeout).
  - `src-tauri/src/ipc/ask_human.rs` (160 lines) — the blocking `ask_human`
    tool's Tauri-side half: `AskHumanRegistry` (parks pending questions) +
    `TauriHumanPrompter` (implements `tools::ask_human::HumanPrompter`: emits
    `tool:ask_human_request`, awaits a oneshot with a decline-by-default
    timeout). Structurally a near-mirror of `approval.rs` — same
    park/emit/await/take shape, different registry and event name, longer
    timeout (600s vs. 300s, since answering a question is deliberative, not
    a yes/no gate).
  - `src-tauri/src/ipc/contract_tests.rs` (499 lines, `#[cfg(test)]`, gated
    at `ipc/mod.rs:29-30`) — regression lock for the Tauri v2 "args nested
    under `args`" wrapping bug; drives real IPC dispatch
    (`tauri::test::get_ipc_response`) against a `MockRuntime` app built with
    a *subset* of the real `invoke_handler` table. See "Tests" below for
    exactly which commands that subset covers — it's smaller than you'd
    expect.
  - `src-tauri/src/lib.rs` (648 lines) — `run()`: builds `Storage`, runs
    crash recovery, seeds built-in agent types, hydrates providers, loads
    the classifier (trained ensemble with a rules-only fallback), sets up
    the embedder handle + boot-time backfill, builds the `PrivacyGate`, the
    approval spine, the ask-human spine, `build_tool_dispatcher` (now 18
    tools, not 6), the `AgentLoop`, spawns the `WorkQueueRunner`, then
    `app.manage(AppState{...})` and `.invoke_handler(tauri::generate_handler![...])`.
  - `src-tauri/src/main.rs` — unchanged, a 9-line shim calling
    `lost_harness_product_lib::run()`.
  - `src-tauri/tauri.conf.json` — window config; `app.windows[0].url = "app.html"`
    is the fix for a blank-GUI bug (see Gotchas); `app.security.csp` is
    `null` (still true — see Gotchas).
  - `src/lib/api/tauri.ts` — the frontend-side mirror of this contract.
    **Its header comment has not kept up with the command surface** — see
    `frontend-svelte.md` for how stale it's gotten (11 of 54 exported
    functions documented there).

- **Key types / traits / functions**
  - `AppState` — `ipc/mod.rs:56-74`. **7** fields, all `Arc` (one `Option`),
    no outer `Mutex`: `agent_loop: Arc<AgentLoop>` (57), `model_manager:
    Arc<ModelManager>` (58), `storage: Arc<Storage>` (59), `approvals:
    Arc<ApprovalRegistry>` (62), `ask_human: Arc<AskHumanRegistry>` (65),
    `classifier: Arc<dyn Classifier>` (69, backs `explain_classification`),
    `embedder: Option<Arc<EmbedderHandle>>` (73, `None` when no embedder
    model dir is configured — see `storage.md`/memory docs). Injected into
    commands via `state: State<'_, AppState>`.
  - `SendMessageArgs` / `SendMessageResponse` — `ipc/mod.rs:186-202` /
    `80-92`. `profile` is required (not `Option`), deliberately, to avoid
    silently writing to the wrong profile db. `SendMessageArgs` also carries
    an optional `mode: Option<String>` (Q11 session mode —
    normal/plan/accept_edits) added since the original surface, parsed via
    `hooks::SessionMode::from_str_lenient` with a lenient default.
  - `latest_assistant_routing(rows: &[Message]) -> Option<(String, String)>`
    — `ipc/mod.rs:413-422`. Pure helper: picks the most recent `assistant`
    row's real persisted `routing_decision` ("allow"/"route_local"),
    defaulting to `"allow"` only when that row has none set. **This is the
    fix for what used to be a hardcoded `"allow"` response** — see below.
  - `send_message(app, state, args: SendMessageArgs) -> Result<SendMessageResponse, String>`
    — `ipc/mod.rs:429-492`. Calls `state.agent_loop.process_message(...)`,
    then re-queries the profile db via `latest_assistant_routing` for the
    id **and the real gate decision** of the message just persisted (there's
    still no direct return of either from `process_message` itself).
  - `ResolveApprovalArgs` — `ipc/mod.rs:496-517`. `decision: String`
    ("approve"/anything-else=deny), `scope: Option<String>`
    ("once"|"session"|"always"), `target: Option<String>`
    ("action"|"tool"), `pattern: Option<String>` (glob for an "always" rule,
    default `"*"`).
  - `resolve_tool_approval(state, args) -> Result<bool, String>` —
    `ipc/mod.rs:524-567`. Builds an `ApprovalDecision` from the frontend's
    approve/deny + scope/target/pattern and calls `state.approvals.answer(...)`.
    **Enforces `Once ⇒ action`-only** even if the frontend sends
    `target: "tool"` with `scope: "once"` (`ipc/mod.rs:542-543`) — defense
    in depth against a "just this once" answer silently becoming a
    whole-tool grant. An `"always"` scope is routed to
    `ApprovalDecision::Persist(ToolRule::new(...))` — a durable
    `tool_rules` row, not a ledger entry (`ipc/mod.rs:550-558`).
  - `resolve_ask_human(state, args: ResolveAskHumanArgs) -> Result<bool, String>`
    — `ipc/mod.rs:1064-1075`. Normalizes an all-whitespace answer to a
    decline, then `state.ask_human.answer(&args.id, answer)`.
  - `ApprovalRegistry` — `ipc/approval.rs:63-121` (the `Pending` struct at
    56-61 holds the sender + fingerprint + tool name). `park`/`take`/`answer`
    — `answer`'s closure `mk: FnOnce(&str, &str) -> ApprovalDecision` is
    handed the stored `(fingerprint, tool_name)`, so the frontend never has
    to echo the fingerprint back.
  - `TauriApprovalPrompter` — `ipc/approval.rs:125-193`. Implements
    `hooks::ApprovalPrompter::request()`: parks a channel, emits
    `tool:approval_request`, then `tokio::time::timeout(self.timeout, rx).await`
    (built with a 300s timeout at `lib.rs:158`); on timeout or a dropped
    sender, resolves to `ApprovalDecision::Timeout` (fail closed).
  - `AskHumanRegistry` / `TauriHumanPrompter` — `ipc/ask_human.rs:39-73` /
    `77-129`. Same shape as the approval pair, but the payload is a plain
    question string and the answer is `Option<String>` (`None` = declined).
    Built with a 600s timeout at `lib.rs:169`.
  - `build_tool_dispatcher(base_path, classifier, ledger, approver, storage,
    embedder, app_handle, human_prompter, model_manager) -> ToolDispatcher`
    — `lib.rs:431-647`. Nine parameters now (`#[allow(clippy::too_many_arguments)]`
    twice over, `lib.rs:431-432` — one place threads every tool dependency).
    Registers **18 tools** (up from 6): the 6 fs tools (`lib.rs:484-489`),
    `recall_memory`/`remember` (496-504), `session_search` (507-509),
    `system_status` (512-514), `list_cron_jobs`/`manage_cron` (519-524),
    `fetch_url` (530), `ask_human` (535-537), `search_skills`/`save_skill`
    (542-547), `delegate` (554-557), and `shell_exec` (578-588, wired with
    `.with_storage(...)` so it can read the caller's per-profile
    `sandbox_config` ceiling — see README's watch-item (a), because nothing
    ever writes one). Policy is still derived purely from each tool's
    `RiskClass` (`Safe` ⇒ `Allow` + pre-trusted; `Write|External|Dangerous`
    ⇒ `Ask`), layered with the persisted per-profile `tool_rules`
    (`SqlitePolicySource`, `lib.rs:623-630`) over the in-memory
    risk-derived defaults.
  - `run()` — `lib.rs:44-282`. Sequential setup, now with more steps than
    the original M1 surface: storage open (58-62) → crash-recovery boot
    pass (64-76, best-effort, never `?`-propagated) → seed built-in agent
    types (78-85, best-effort) → hydrate providers from storage (89-90) →
    load the classifier — trained ONNX ensemble with a rules-only fallback
    (92-119, see below) → set up the (lazy) embedder handle + spawn a
    boot-time embedding-backfill pass on a blocking thread (121-143) →
    `PrivacyGate::new` (145-146) → approval spine: ledger + registry +
    prompter (148-159) → ask-human spine: registry + prompter (161-170) →
    `build_tool_dispatcher` (172-188) → `AgentLoop::new` with
    `.with_embedder()`/`.with_flush_classifier()`/`.with_skill_drafter()`
    (190-208) → `spawn_work_runner` (210-218, fire-and-forget for the life
    of the process) → assemble `AppState` + `app.manage()` (220-229) →
    `invoke_handler![...]` (234-279).

- **The classifier boot sequence is live, not a stub.** `lib.rs:100-119`:
  `EnsembleClassifier::load(&classifier_models)` is tried first (the trained
  bge-small + distilbert ONNX ensemble fused with the rules layer, loaded
  from `<storage>/models/classifier/`); on any load error (most commonly: no
  model files installed there), it falls back to `RulesClassifier::new()`
  (deterministic rules only). **An earlier version of this doc (and of
  `README.md`) said "the trained ONNX layer is a stub" — that's now false.**
  The ensemble is real, wired via the `ort` crate, and this exact
  try-then-fall-back shape is what actually ships; see `classifier.md` for
  the ensemble's internals.

- **Data flow / how it fits**
  1. **Boot**: `main.rs` → `lib.rs::run()`, the sequence above. Classifier
     must exist before the gate and the tool dispatcher, since both share
     the *same* `Arc<dyn Classifier>` so message-gate and tool-gate classify
     identically (comment at `lib.rs:92-99`).
  2. **Normal command call**: frontend `tauriInvoke("cmd", { args: {...} })`
     → Tauri deserializes into the command's typed `args` param → handler
     runs against `State<AppState>` → returns `Result<T, String>` (Err
     becomes a JS-thrown string).
  3. **send_message**: frontend calls `send_message` → `AgentLoop::process_message`
     (`agent/loop_mod.rs`) does gate → route → stream, emitting `stream:token`
     and `stream:error` events → the command's own return value carries the
     final assembled text **and now the real routing decision**
     (`latest_assistant_routing`), arriving after streaming is done.
  4. **Tool approval round-trip** (`Write`/`External`/`Dangerous` tools):
     `ToolDispatcher::dispatch` hits an `Ask` → `approver.request(...)` →
     `TauriApprovalPrompter` parks a oneshot + emits `tool:approval_request`
     → frontend shows `ApprovalDialog` and calls `resolve_tool_approval` →
     looks the id up in the same `ApprovalRegistry` and sends the decision
     down the oneshot → dispatch wakes, grants into `ApprovalLedger` if
     approved, and re-runs the full gating chain (bounded to
     `MAX_APPROVAL_ROUNDS = 4`) before executing.
  5. **`ask_human` round-trip** (unchanged shape, separate spine): the
     `ask_human` tool calls `TauriHumanPrompter::ask()` → parks a oneshot in
     `AskHumanRegistry` + emits `tool:ask_human_request` → frontend shows
     `AskHumanDialog` and calls `resolve_ask_human` → answer delivered down
     the oneshot, or a 600s timeout delivers `None` (decline).
  6. **Frontend mirror**: `src/lib/api/tauri.ts` is the sole frontend entry
     point into all of this. Its own header-comment contract only lists the
     original 11 commands — see `frontend-svelte.md`.

- **Invariants (do NOT break)**
  - **Args-nesting contract**: every command whose signature is
    `fn cmd(state: State, args: SomeArgs)` requires
    `invoke("cmd", { args: {...snake_case} })`. Bare-scalar-param commands
    (`remove_provider(id)`) are the sole exception and take `{ id }`
    unwrapped. Enforced by `ipc/contract_tests.rs` for the subset of
    commands it covers (see Tests) — **not** for the ~30 commands added
    since the original M1 surface (memory, skills, seats, agent types,
    packs, hardware/catalog/download, tool rules, `explain_classification`,
    `get_usage_summary`, `resolve_tool_approval`, `resolve_ask_human`): a
    wrapping regression in any of those would only be caught by hand-testing
    the app, not by `cargo test`.
  - **API keys never round-trip to the frontend.** `ProviderInfo` and its
    `From<Provider>` impl omit `api_key`; both `ipc/mod.rs::tests::provider_info_omits_api_key`
    and `contract_tests.rs::add_provider_correct_shape_dispatches_and_succeeds`
    assert the raw JSON text doesn't contain the test secret.
  - **A `Once`-scope approval is action-scoped only, never tool-scoped.**
    Enforced twice: `resolve_tool_approval`'s `want_tool` gate
    (`ipc/mod.rs:542-543`) and `ApprovalLedger::grant`'s defensive no-op arm
    for `(Once, Tool(_))` (`hooks/approval.rs:192-197` — it logs a warning
    and records nothing rather than silently widening the grant; see
    `hooks-gating-and-approval.md` for the rest of `grant`'s match arms).
  - **Approval and ask-human resolution never touch the agent loop's stream
    lock.** `resolve_tool_approval` only touches `state.approvals`;
    `resolve_ask_human` only touches `state.ask_human` — so either can
    always answer a parked request even while `send_message` holds
    `AgentLoop`'s internal `tokio::sync::Mutex`. This deadlock-free property
    is why there are now *two* independent registries in `AppState` instead
    of one.
  - **Fail-closed on prompt failure or timeout**, for both spines. If
    `app.emit(...)` fails (no window yet) or nothing answers in time,
    `TauriApprovalPrompter` resolves to `ApprovalDecision::Timeout` (denied)
    and `TauriHumanPrompter` resolves to `None` (declined) — never left
    hanging, never defaulted to permissive.
  - **Tool risk gating is derived, not hand-maintained.** `build_tool_dispatcher`
    sets policy purely from `tool.risk()` (`lib.rs:598-608`) — a new
    `Write`/`External`/`Dangerous` tool is `Ask` by construction; there is
    no per-tool allowlist to remember to update.
  - **`Storage`'s `Send + Sync`** is genuine (see `storage.md`) — every
    `GlobalDb`/`ProfileDb` connection sits behind its own `parking_lot::Mutex`,
    so concurrent IPC commands and the agent loop can all hold a `Storage`
    handle at once with no manual `unsafe impl` at the `AppState` level.
  - **`app.windows[0].url` must stay `"app.html"`**, matching
    `vite.config.ts`'s `rollupOptions.input`. Previously unset/defaulted to
    `/`, which 404'd (blank GUI) — see Gotchas.

- **Gotchas / watch-items**
  - **`contract_tests.rs`'s `MockRuntime` harness only registers 14 of the
    44 production commands** (`contract_tests.rs:96-111`): `get_app_version`,
    `get_active_profile`, `set_active_profile`, `list_profiles`,
    `list_conversations`, `create_conversation`, `get_messages`,
    `list_providers`, `add_provider`, `remove_provider`, `list_models`,
    `get_classifier_settings`, `set_classifier_settings`,
    `set_redaction_enabled`, `reset_classifier_settings`. Of those, only
    **seven command-groups** have real coverage: a correct-shape/broken-shape
    test pair for `create_conversation`, `list_conversations`, `get_messages`,
    `add_provider`, `list_models`; the classifier-settings group (one
    round-trip test, `classifier_settings_round_trip_through_real_ipc`,
    plus one broken-shape test); and the active-profile group (a set→get
    round-trip, `active_profile_round_trips_through_real_ipc`, plus a
    validation-rejection test, `set_active_profile_rejects_a_confusable_name`).
    **Every command added since the M1 surface — all of memory, skills,
    seats, agent types, packs, hardware/catalog/download, tool rules,
    `explain_classification`, `get_usage_summary`, `resolve_tool_approval`,
    `resolve_ask_human` — has zero coverage here.** `send_message` is
    excluded on purpose (its bare `AppHandle` param hard-codes `Wry`,
    can't compile against `MockRuntime`; covered instead by
    `agent::loop_tests` for business logic, never through real IPC arg
    deserialization).
  - **`get_active_profile` now persists; `list_profiles` is still a stub.**
    `get_active_profile` reads the `active_profile` row from `global.db`'s
    `app_settings` (via `GlobalDb::active_profile`), falling back to
    `"personal"` only when nothing is stored yet or the stored value fails the
    profile-name allowlist. Its writer is `set_active_profile`, which validates
    the id with `crate::storage::validate_profile_name` before the write; the
    frontend's `switchProfile` (`stores/profiles.ts`) calls it through
    `api.setActiveProfile`, so the last-used profile survives an app restart.
    `list_profiles` (`ipc/mod.rs`) is unchanged — still the hardcoded 4-name
    list; no per-profile routing is wired at this layer beyond the active-id
    round-trip. Round-trip + validation are locked by
    `active_profile_round_trips_through_real_ipc` /
    `set_active_profile_rejects_a_confusable_name` in `contract_tests.rs`.
  - **`get_app_version` returns a hardcoded literal** `"0.1.0-m1"`
    (`ipc/mod.rs:247-250`), not `tauri.conf.json`'s `version` (`"0.1.0"`) or
    `CARGO_PKG_VERSION`. Unchanged from the original M1 surface — still
    drifts if you bump the app version.
  - **There is no IPC command surface for cron at all.** `ListCronJobsTool`
    and `ManageCronTool` (`tools/cron.rs`) are registered as **agent tools**
    in `build_tool_dispatcher` (`lib.rs:519-524`) — reachable only through
    the model's fenced tool-call dialect inside a conversation, gated like
    any other tool. There is no `list_cron_jobs`/`manage_cron` (or similar)
    `#[tauri::command]` in `ipc/mod.rs`, so Settings' "Scheduled jobs"
    section (if/when built) would need a *new* IPC command — it can't just
    call an existing one.
  - **`send_message`'s `message_id` is recovered by a heuristic re-query**,
    not returned directly by `process_message`: `latest_assistant_routing`
    (`ipc/mod.rs:413-422`) picks the most recent `assistant` row, falling
    back to a fresh random UUID if the query comes back empty. Unchanged
    behavior from the original surface; **what did change** is that this
    same helper now also returns the real `routing_decision` instead of a
    hardcoded string (see below).
  - **Fixed: `send_message` no longer hardcodes `routing_decision: "allow"`.**
    Earlier versions of this doc (and `README.md`) flagged this as a live
    bug with a visible frontend cost (a live send could never show an
    honest `route_local` badge). It's fixed: `send_message`
    (`ipc/mod.rs:481-482`) calls `latest_assistant_routing(&rows)`, which
    reads the real persisted decision off the assistant row
    `process_message` just wrote, defaulting to `"allow"` only when that
    row genuinely carries no decision. Covered by four unit tests
    (`ipc/mod.rs:1810-1843`), including
    `latest_assistant_routing_picks_the_most_recent_assistant` (guards
    against picking a stale earlier turn) and
    `latest_assistant_routing_reads_the_real_decision` (the regression the
    fix targets). Don't reintroduce a hardcoded literal here.
  - **The known, deferred single-in-flight limitation** still holds:
    `process_message` holds the agent loop's stream lock across
    `approver.request(...).await`, so while a tool-approval prompt is
    outstanding, a second `send_message` call blocks until the prompt is
    answered or times out. The same is true of the `ask_human` prompt now
    that it exists too — an outstanding question blocks the app the same
    way an outstanding approval does. Intentional-for-now; needs a
    concurrency-model refactor to fix for real.
  - **`ApprovalRegistry.answer`/`TauriApprovalPrompter.request` race** is
    explicitly documented and accepted (`ipc/approval.rs:178-190`): in a
    vanishingly small window, both a timeout and a concurrent `answer` can
    both "complete," so `resolve_tool_approval`'s return value can lie about
    delivery even though the *security* outcome (deny-by-default) stays
    correct. The `ask_human` spine has the equivalent shape (`ipc/ask_human.rs`'s
    `take`-after-timeout cleanup) but isn't separately called out in its own
    doc comment the way the approval one is — same reasoning applies.
    Don't chase either as a bug.
  - **`tauri.conf.json`'s `app.security.csp` is still `null`** — no CSP is
    enforced. Unchanged. First thing to look at if this subsystem ever
    needs to tighten security posture.
  - **The frontend bridge is a hand-maintained mirror, not generated, and
    its own documentation has drifted.** `src/lib/api/tauri.ts`'s header
    comment lists only the original 11 commands; the file now exports 54
    functions/consts. See `frontend-svelte.md` for the detail — flagged
    here too because it means this doc and that comment are now the *only*
    two places a command's existence is documented, and they disagree on
    completeness.
  - **`ProviderKind` still serializes lowercase**
    (`#[serde(rename_all = "lowercase")]`, exercised at
    `contract_tests.rs:333-337`) and the frontend compares against lowercase
    strings — a regression to PascalCase would silently break provider-kind
    checks in the UI with no type error (`ProviderInfo.kind` is typed
    `string` on the TS side).
  - `ipc/mod.rs`'s own module doc (`ipc/mod.rs:1-28`) is a good first read
    before touching this file — command naming, `Result<T,String>`, event
    naming `<domain>:<action>`.

- **How to extend**
  - **New command**: add a `#[tauri::command]` fn in `ipc/mod.rs` (an `Args`
    struct if it needs more than a trivial scalar param), add it to
    `lib.rs`'s `tauri::generate_handler![...]` list, and — if it's model-free
    and can build against `MockRuntime` — add it plus a correct-shape/
    broken-shape test pair to `contract_tests.rs`. Given how much of the
    current surface has skipped that last step (see Gotchas), doing it for
    a new command is the single highest-leverage thing you can do to keep
    this subsystem's test coverage from falling further behind its size.
    Mirror the function + any new type in `src/lib/api/tauri.ts` (and,
    ideally, its now-stale header comment).
  - **New event**: choose a `<domain>:<action>` name, emit via
    `app.emit(name, payload)`, add a matching TS interface + `on<Event>`
    helper in `tauri.ts`.
  - **New gated (state-changing) tool**: implement under `tools/`, give it
    the right `RiskClass`, register it in `build_tool_dispatcher`
    (`lib.rs`, alongside the other 18) — the approval UI path is already
    generic over tool name/fingerprint and needs no changes.
  - **New blocking human-input primitive**: if you ever need a third
    "pause and wait for the human" spine beyond approval and ask-human,
    copy the `ipc/ask_human.rs` shape (registry + prompter + resolve
    command) rather than overloading either existing one — they're
    intentionally near-identical and cheap to clone.
  - **Changing a timeout**: approval is the `Duration::from_secs(300)`
    literal at `lib.rs:158`; ask-human is `Duration::from_secs(600)` at
    `lib.rs:169`.
  - **Adding a cron IPC surface**: there isn't one today (see Gotchas) — a
    new pane that lets the user manage cron jobs outside of asking the
    agent needs new `#[tauri::command]`s in `ipc/mod.rs`, not a wrapper
    around the existing agent tools.
  - **If the GUI goes blank again**: check `tauri.conf.json`'s
    `app.windows[0].url` still points at `"app.html"` and matches
    `vite.config.ts`'s `rollupOptions.input`.

- **Tests**
  - `src-tauri/src/ipc/mod.rs::tests` (bottom of file) — unit tests for
    `get_app_version`, `list_profiles`, `parse_binding`, `parse_kind`, the
    API-key-omission guarantee on `ProviderInfo`, and the four
    `latest_assistant_routing` tests that pin the real-routing-decision fix.
    (`get_active_profile` is no longer a bare-fn unit test here — now that it
    takes `State<AppState>` and reads the persisted `app_settings` row, its
    default + round-trip is covered through the real IPC boundary in
    `contract_tests` and at the storage layer in `storage::tests`.)
  - `src-tauri/src/ipc/mod.rs::explain_tests` (`ipc/mod.rs:1399-1459`) —
    unit tests for `build_explanation`/`category_display`, the pure mapping
    behind `explain_classification`'s "why" sidebar: rule spans get labels
    and hard-block flags, `PROPRIETARY` is hard-blocked, benign text has no
    spans, and memory-sensitivity routing (`route_memory_sensitivity`) is
    exercised for shared/never-persist/private-local outcomes.
  - `src-tauri/src/ipc/approval.rs::tests` (`ipc/approval.rs:195-228`) —
    `ApprovalRegistry` unit tests: unknown-id answer returns false;
    park-then-answer delivers the decision and a second answer is a no-op.
  - `src-tauri/src/ipc/ask_human.rs::tests` (`ipc/ask_human.rs:131-160`) —
    the `AskHumanRegistry` equivalent: unknown id, park-then-answer
    delivers the text, and a decline delivers `None`.
  - `src-tauri/src/ipc/contract_tests.rs` — the real-IPC-boundary
    regression suite for the args-wrapping contract. See "Gotchas" above
    for exactly which 14 commands are registered in its `MockRuntime`
    harness and which 6 of those actually have a shape-regression test
    pair; everything else registered under `lib.rs`'s real
    `invoke_handler!` has no test coverage at this layer.
  - Run everything relevant:
    `cd src-tauri && cargo test --lib ipc::` (or narrower:
    `cargo test --lib ipc::contract_tests::`, `cargo test --lib ipc::approval::`,
    `cargo test --lib ipc::ask_human::`). Full suite: `cargo test --lib`
    (542 tests as of HEAD `ca54251`, 2026-07-21).
  - Related but outside this subsystem's own files: `agent::loop_tests`
    covers `AgentLoop::process_message`'s business logic (gate → route →
    stream → persist) without going through the Tauri IPC boundary — the
    only coverage `send_message` gets, since it has no contract test.
