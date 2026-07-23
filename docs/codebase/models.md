# Models subsystem (`src-tauri/src/models/`)

- **Purpose** — Registry of model endpoints (`Provider`/`ProviderKind`), an
  OpenAI-compatible HTTP client (`ModelClient`) with an incremental SSE parser
  (`SseStream`), all fronted by `ModelManager`; plus a cluster of Wave 5.3/M8
  "model lifecycle" modules (hardware probing, a curated download catalog,
  verified download) and Wave 3.1/3.2 helpers (per-profile model "seats",
  cloud pricing for the usage ledger). **This module is no longer text-only.**
  The previous version of this doc claimed "no native `tool_use` path" —
  that's flatly false as of Q1 (2026-07-17): `SseEvent` decodes native
  streamed `tool_calls`, `ChatRequest` carries an optional `tools` array, and
  the agent loop picks native-vs-fenced transport per round. The fenced
  dialect (`tools::calling`) remains the fallback for endpoints that don't
  support native tool calls — it hasn't been removed, just demoted to "one of
  two transports" instead of "the only one."

- **Files**
  - `mod.rs` (31 lines) — module doc + re-exports: `ChatMessage`,
    `ModelClient`, `OwnOutput`, `ModelManager`, `Provider`, `ProviderKind`,
    `resolve_seat`. Read this first for the module map.
  - `provider.rs` (95 lines) — `ProviderKind` + `Provider`: static endpoint
    config (id/name/base_url/api_key/kind) plus `is_local()`/`is_private()`
    and the Q1 `supports_native_tools` flag + `with_native_tools()` builder.
  - `client.rs` (279 lines) — `ModelClient`: one `reqwest::Client` per
    provider; `list_models`, `stream_chat`, `stream_chat_with_tools`
    (the Q1 native-transport entry point), `complete` against the OpenAI
    chat-completions surface. `OwnOutput` (the type-level "parse only the
    model's own current-turn output" enforcement) also lives here.
  - `sse.rs` (370 lines, up from ~264) — `SseStream`/`SseEvent`: incremental
    line-buffered SSE parser. `SseEvent` now has **five** variants, not
    four: `Delta`, `Done`, `KeepAlive`, `Error`, plus Q1's `ToolCalls(Vec<
    ToolCallFragment>)` and Wave 3.2's `Usage { prompt_tokens,
    completion_tokens }`.
  - `content.rs` (144 lines, **new file**) — `ImageBlock` + `assemble_content`:
    the multimodal wire-format primitive. Built and unit-tested (5 tests) but
    has **zero callers** anywhere in the tree — see Gotchas.
  - `pricing.rs` (86 lines, **new file**) — `cost_usd(model, prompt_tokens,
    completion_tokens) -> Option<f64>`: the Wave 3.2 usage-ledger price table.
  - `seat.rs` (145 lines, **new file**) — `resolve_seat`: Wave 3.1 per-profile
    model-seat preference resolver.
  - `hardware.rs` (131 lines, **new file**) — M8 `HardwareProfile`/`probe()`/
    `fits()`: sizes the model catalog against the machine's RAM.
  - `catalog.rs` (166 lines, **new file**) — M8 curated model catalog
    (`catalog.json`, bundled via `include_str!`), `CatalogEntry`,
    `catalog_for(profile)`.
  - `download.rs` (189 lines, **new file**) — M8 model download: HF-only host
    allowlist, SHA-256 verify-before-install, atomic rename.
  - `manager.rs` (110 lines) — `ModelManager`: `RwLock`-guarded registry of
    `Provider`s + a cache of built `ModelClient`s, keyed by provider id.
    Unchanged in shape since the last doc pass.
  - `tests.rs` (446 lines, `#[cfg(test)]`, wired in via `mod.rs:25-26`) — unit
    tests for everything above, plus the native-tool-call SSE decode/assemble
    round trip and an opt-in **live** test against a real endpoint (see
    "Native tool-use" below).

- **Key types / traits / functions**
  - `ProviderKind` (`provider.rs:20-28`) — `Local | Cloud | Custom`,
    `#[serde(rename_all = "lowercase")]` (`provider.rs:19`) — still
    load-bearing for the frontend's `p.kind === "local"` checks; unchanged.
  - `Provider` (`provider.rs:36-52`) — gained `supports_native_tools: bool`
    (`provider.rs:47-51`, `#[serde(default)]` so old persisted rows without
    the field still deserialize) and the builder `with_native_tools(self,
    supported: bool) -> Self` (`provider.rs:76-79`). `is_local()`
    (`provider.rs:83-85`) and `is_private()` (`provider.rs:92-94`, still
    delegates to `crate::agent::egress::is_private_endpoint`) are unchanged.
  - `ModelClient` (`client.rs:131-134`) — `stream_chat(model, messages)`
    (`client.rs:180-186`) is now a **thin wrapper** around
    `stream_chat_with_tools(model, messages, None)`.
    `stream_chat_with_tools(model, messages, tools: Option<&Value>)`
    (`client.rs:192-230`) is the Q1 entry point: builds a `ChatRequest` with
    the optional `tools` array and a `stream_options.include_usage` that is
    **only set when the endpoint is NOT private**
    (`(!self.provider.is_private()).then_some(...)`, `client.rs:207-208`) —
    a local/private call is always `$0` and never consults `usage`, so
    there's no reason to ask a (more likely strict) self-hosted server for
    it. `complete(model, messages)` (`client.rs:235-278`) is the non-stream
    variant — **still has no caller outside `models/` production code
    besides `agent/memory_flush.rs:138` and `agent/skill_reflect.rs:149`**
    (see Gotchas for the usage-ledger gap this creates).
  - `ChatRequest` (`client.rs:82-95`) — `tools: Option<&Value>` and
    `stream_options: Option<StreamOptions>` both `#[serde(skip_serializing_if
    = "Option::is_none")]` — a non-tool-capable / non-billed request never
    sees either field on the wire.
  - `OwnOutput` (`client.rs:60-77`) — unchanged: constructible only via the
    `pub(crate)` `from_stream_assembly`, called exactly once per turn by the
    agent loop right after assembling the model's SSE deltas. This is what
    lets `tools::calling::parse_tool_calls` require `&OwnOutput` instead of
    `&str` — a type-level guarantee that only the model's own current-turn
    text can be parsed for (fenced) tool calls.
  - **`SseEvent`** (`sse.rs:32-58`):
    - `ToolCalls(Vec<ToolCallFragment>)` — one streamed piece of a native
      tool call per fragment (`index`, optional `name`, an `arguments`
      string fragment to concatenate). `ToolCallFragment` at `sse.rs:62-66`.
    - `Usage { prompt_tokens: u32, completion_tokens: u32 }` — the final
      chunk's token totals, when the endpoint reports them.
  - **`parse_line`** (`sse.rs:181-241`) dispatch order: comment/non-`data:`/
    empty/`[DONE]` → `error` field → content delta (`delta.content` →
    `message.content` → `text` fallback chain, `first_delta`,
    `sse.rs:323-339`) → **tool-call fragments** (`tool_call_fragments`,
    `sse.rs:345-369`, reading `delta.tool_calls` falling back to
    `message.tool_calls`) → **usage** (`sse.rs:223-238`) → `KeepAlive`.
    Content and tool-calls are treated as mutually exclusive per chunk in
    practice (content wins if a server ever mixes them).
  - **Usage parsing is deliberately lenient.** `SsePayload.usage` is kept as
    a permissive `serde_json::Value`, **not** a typed struct
    (`sse.rs:259-266`, doc comment explains why): a malformed `usage` object
    (string/float token counts, or `usage` riding on a content-bearing chunk)
    must never fail the whole line's parse and drop a co-located `content`
    delta. Token fields are pulled leniently at the decode site
    (`sse.rs:224-229`, non-integer → treated as absent/0) and a zero/absent
    usage never emits a `Usage` event (→ the ledger records an honest
    "unknown" cost rather than `0`). Regression-tested:
    `sse_malformed_usage_never_drops_co_located_content`
    (`models/tests.rs:154-166`), `sse_ignores_a_zero_usage_chunk`
    (`models/tests.rs:169-176`).
  - **Native tool-use assembly** — `assemble_native_calls` lives in
    `tools/calling.rs:59-95` (not in `models/`, but the direct consumer of
    `models::sse::ToolCallFragment`): it folds fragments per call-slot
    `index` (name arrives once, `arguments` streams as string pieces to
    concatenate) into the **same `ParsedToolCall`** enum the fenced parser
    produces — a missing name or unparseable/non-object arguments become
    `ParsedToolCall::Malformed`, fed back to the model to retry exactly like
    a bad fenced block. This is the normalization point where the two
    transports converge on one downstream pipeline
    (budgets/repeat-detection/hooks/audit in `tools::dispatch` are
    transport-blind).
  - `pricing::cost_usd(model, prompt_tokens, completion_tokens) ->
    Option<f64>` (`pricing.rs:42-51`) — looks up `model` (lowercased) against
    a small substring table (`pricing.rs:18-36`) ordered **most-specific
    match first** (e.g. `"gpt-4o-mini"` before `"gpt-4o"`, so the cheaper
    variant is never mis-billed at the pricier rate); an unrecognized model
    returns `None` — never a nearest-guess.
  - `seat::resolve_seat(storage, model_manager, profile, seat,
    caller_provider_id, caller_model) -> (String, String)` (`seat.rs:26-49`)
    — resolves a user-defined seat name to a concrete `(provider_id, model)`
    via the profile's `seat_bindings` table, falling back to
    `(caller_provider_id, caller_model)` when the seat is empty/`"inherit"`
    (case-insensitive)/unbound/bound to a since-deleted provider. **This is a
    preference resolver only — it never touches privacy.** The doc comment
    (`seat.rs:8-13`) is explicit: the pair it returns is a candidate that the
    per-turn privacy gate and `enforce_local_routing` still get the final
    say over downstream, so a seat can prefer cloud but can never defeat a
    `RouteLocal`/`LocalRequired` verdict. Called from `tools/delegate.rs:145`
    (Wave 4.3c persona dispatch).
  - `hardware::probe() -> HardwareProfile` (`hardware.rs:31-44`) — total RAM
    + CPU cores (via `sysinfo`) + OS/arch. `fits(model_bytes, profile) ->
    Fit` (`hardware.rs:68-82`) is a **pure** sizing function
    (`Fits`/`Tight`/`TooLarge` against a `1.3×` working-set overhead and a
    `0.7` "comfortable" fraction of total RAM) that **fails closed to
    `TooLarge` when `total_ram_bytes == 0`** (`hardware.rs:69-72`) — an
    unknown-RAM probe failure never claims a model fits.
  - `catalog::CatalogEntry::is_curated()` (`catalog.rs:48-51`) — true only for
    a real 64-hex-char SHA-256, not the bundled placeholder.
    `catalog_for(profile) -> Vec<CatalogEntryView>` (`catalog.rs:93-95`)
    annotates each bundled entry with its `Fit` and `installable` (=
    `is_curated()`) for the onboarding picker.
  - `download::host_allowed(url)` (`download.rs:25-39`) — HTTPS + Hugging
    Face root domains only (`huggingface.co`, `hf.co`, and true subdomains —
    a suffix-spoof like `huggingface.co.evil.com` is rejected,
    `download.rs:140`). `verify_and_install(partial, final_path,
    expected_sha256)` (`download.rs:58-73`) — hashes the downloaded file,
    and on any mismatch **or** a non-curated placeholder hash, deletes the
    partial and installs nothing (`bail!`); on match, an atomic
    `std::fs::rename` publishes it. `download_to_partial(url, partial,
    on_progress)` (`download.rs:78-119`) streams with `Range`-header resume.

- **Data flow / how it fits**
  1. **Startup**: `lib.rs::hydrate_providers_from_storage`
     (`src-tauri/src/lib.rs:304-332`) reads `storage.global().list_endpoints()`
     and calls `add_provider` for each persisted row, building
     `Provider::new(...).with_native_tools(ep.supports_native_tools)`
     (`lib.rs:327-328`). `ep.kind` is matched leniently
     (`"local"|"cloud"|_ => Custom`, `lib.rs:316-319`) — an unrecognized kind
     string silently becomes `Custom`, not an error (see Gotchas for the
     inconsistency with the IPC write path).
  2. **IPC surface** (`ipc/mod.rs`) — `AppState` is defined **here**
     (`ipc/mod.rs:56-74`), not in `agent/loop_mod.rs` as an earlier version of
     this doc said; it holds `model_manager: Arc<ModelManager>`,
     `storage: Arc<Storage>`, and `embedder: Option<Arc<EmbedderHandle>>`
     among others. `add_provider` (`ipc/mod.rs:343`), `remove_provider`
     (`ipc/mod.rs:376`), `list_models` (`ipc/mod.rs:390`); `parse_kind`
     (`ipc/mod.rs:1704`) **rejects** unknown kind strings — inconsistent with
     the lenient hydration path above (see Gotchas). The M8 lifecycle is also
     IPC-wired: `probe_hardware` (`ipc/mod.rs:685`), `list_model_catalog`
     (`ipc/mod.rs:692-694`), `download_model` (`ipc/mod.rs:771-810`, streams
     `model:download-progress` events, refuses a non-curated entry outright
     at `ipc/mod.rs:780-784` before ever touching the network).
  3. **Agent loop** (`agent/loop_mod.rs`, 1825 lines — grown substantially
     since the last doc pass): per round, `native_mode = provider
     .supports_native_tools && native_spec.is_some()` (`loop_mod.rs:1340`,
     `native_spec` is `self.tools.native_tools_spec()` — the OpenAI
     function-call array built once per turn from `Tool::schema()` across
     every available tool, `tools/dispatch.rs:366-385`). The round calls
     `client.stream_chat_with_tools(&model, compaction.sent, if native_mode {
     native_spec.as_ref() } else { None })` (`loop_mod.rs:1367-1374`) and
     pumps events (`loop_mod.rs:1381-1415`): `Delta` → token sink;
     `ToolCalls` → accumulated into `native_frags` **only if `native_mode`**
     (an endpoint that streams `tool_calls` without the flag set is logged
     and ignored, `loop_mod.rs:1391-1399` — the flag is the user-set
     capability contract, not a sniff); `Usage` → `round_usage`; `Error` →
     abort the turn. `native_frags` are turned into `ParsedToolCall`s via
     `tools::calling::assemble_native_calls` (`loop_mod.rs:1526`) and driven
     through `run_turn_native`, which **never invokes the fenced
     `parse_tool_calls`** — the "a forged fence can't mint a call" invariant
     becomes structural on a native turn, not just a convention.
  4. **Usage ledger booking** (Wave 3.2, `loop_mod.rs:1474-1502`): after each
     streamed round, `cost_usd` is `Some(0.0)` for a non-cloud (`!is_cloud`)
     call, else `round_usage.and_then(|(pt,ct)| pricing::cost_usd(&model, pt,
     ct))` — `None` when the endpoint didn't report usage or the model isn't
     priced. `profile_db.record_usage(...)` books the row; a ledger-write
     failure only logs a warning, never fails the turn.
     **`ModelClient::complete` books no usage row.** The two production
     callers of `complete` — `agent/memory_flush.rs:138` (Wave 3.5 durable-
     fact extraction) and `agent/skill_reflect.rs:149` (Wave 4.2 autonomous
     skill drafting) — call it and use the returned text directly; neither
     calls `record_usage` afterward. Any cloud model used for these
     background calls is invisible to the cost ledger today — a known gap,
     not yet flagged as a TODO in code.
  5. `find_local_provider`, the §7 `RouteLocal` gate, and
     `crate::agent::egress` are unchanged in shape from the previous doc
     pass — `models` still only exposes `is_private`/`is_local` as data for
     `agent::gate` to make the actual blocking/routing decision.
  6. **The multimodal content assembler is built but wired to nothing.**
     `content::assemble_content(text, images, multimodal) -> serde_json::Value`
     (`content.rs:65-91`) is fully unit-tested (`content.rs:93-144`) but
     `grep`ping the tree for `assemble_content`/`ImageBlock` outside
     `content.rs` itself returns nothing — no `ExecCtx`, tool, or IPC command
     constructs an `ImageBlock` yet, and `ChatMessage.content` (`client.rs:26`)
     is still a plain `String`, not the `Value` this function returns. The
     `WIRING NOTE` doc comment at `content.rs:16-22` is explicit about the
     integration hazard for whoever does this next: the returned `Value` must
     be carried through to serialization **as-is**, not `.to_string()`'d (that
     would stringify the JSON array into a literal `"[{...}]"` text field
     instead of an actual multimodal wire payload).

- **Native tool-use: proven live, not just built.** `Q1` shipped
  2026-07-17 (`d203a9a`) and was verified end-to-end
  (`models/tests.rs:382-446`, `live_native_tool_call_roundtrip`, opt-in via
  `LHP_NATIVE_ENDPOINT`/`LHP_NATIVE_MODEL`/`LHP_NATIVE_TOKEN` env vars) against
  a real **LM Studio `qwen3.6-35b-a3b`** endpoint — three clean runs per
  `docs/ROADMAP.md`. The non-live regression test
  `sse_decodes_native_tool_call_deltas` (`models/tests.rs:341-373`) exercises
  the same decode → `assemble_native_calls` path with synthetic byte chunks
  and needs no network. What's still open (per `docs/ROADMAP.md`, M4 row): an
  add-provider UI checkbox to *set* `supports_native_tools` from Settings —
  the flag, persistence, hydration, and backend are done; only the everyday
  on/off control is missing, so ordinary chat against a native-capable
  endpoint still defaults to the fenced dialect unless the row was flagged
  some other way (e.g. a test, or a direct DB edit).

- **Invariants (do NOT break)**
  - `ProviderKind` must serialize lowercase (`provider.rs:19`) — unchanged,
    still load-bearing for the frontend.
  - `Provider::is_private` must stay delegated to
    `crate::agent::egress::is_private_endpoint` (`provider.rs:92-94`) — do
    not duplicate private-range logic here.
  - `add_provider` must drop the cached client on upsert (`manager.rs:56`) —
    unchanged; otherwise an edited `api_key`/`base_url` would silently keep
    using the stale client until restart.
  - `SseStream::parse_line` must never panic on malformed input
    (`sse.rs:199`: `Err(_) => return SseEvent::KeepAlive`) — the boundary
    between an untrusted network stream and the agent loop.
  - **A native turn must never fall through to `parse_tool_calls`.** The
    transport choice (`native_mode` at `loop_mod.rs:1340`) is a hard branch,
    not a "try native, then also try fenced" — mixing them would reopen the
    "content the agent merely read forges a call" hole that `OwnOutput`
    exists to close.
  - `stream_chat_with_tools`/`complete`/`list_models` (`stream_chat` just
    delegates to `stream_chat_with_tools`) all check `status.is_success()`
    before decoding JSON (`client.rs:225`, `260`, `167`) — non-2xx responses
    become `anyhow` errors with the body text included.
  - **The download pipeline is verify-or-nothing.** `verify_and_install`
    (`download.rs:58-73`) must never publish `final_path` on a hash mismatch
    or a non-curated placeholder — both paths delete the partial and error.
    Don't add a "trust it anyway" override.
  - **`resolve_seat` must never be allowed to override the privacy gate.**
    It's a preference lookup only; any refactor that lets a seat's resolved
    provider skip `enforce_local_routing`/the §7 gate reintroduces exactly
    the hole `seat.rs:8-13`'s doc comment calls out.

- **Gotchas / watch-items**
  - **Provider API keys are OS-credential-store-backed.** SQLite's historical
    `api_key_encrypted` column now contains only a `keychain:v1` presence
    marker. `secrets.rs` performs the idempotent legacy migration and
    `hydrate_providers_from_storage` reads through `ProviderSecretStore`; CI
    tests inject an in-memory fake and never require a logged-in keychain.
  - **Kind-parsing is still inconsistent between the two write paths.**
    `hydrate_providers_from_storage` (`lib.rs:316-319`) silently maps any
    unrecognized `kind` to `Custom`; the IPC `add_provider` command's
    `parse_kind` (`ipc/mod.rs:1704`) *rejects* unknown strings with `Err`. A
    bad/legacy kind string written some other way loads as `Custom` on next
    boot without complaint — still an inconsistent failure mode for the same
    conceptual data.
  - **`is_local()` vs `is_private()` are still easy to confuse** — unchanged;
    `find_local_provider`-style call sites must check both, not just
    `is_local()`, for a "never egresses" guarantee.
  - **`ModelClient::complete` has real callers now** (unlike the earlier doc
    version's "appears unused outside tests") — `agent/memory_flush.rs:138`
    and `agent/skill_reflect.rs:149` — but **neither books a usage-ledger
    row**. If someone later notices "local extraction/reflection calls are
    invisible in the cost ledger," this is why; fixing it means threading a
    `record_usage` call (or at least a `None`/`Some(0.0)` row) through both
    call sites, mirroring what the streaming path already does at
    `loop_mod.rs:1474-1502`.
  - **`content::assemble_content`/`ImageBlock` are dormant** — built, tested,
    zero callers, `ChatMessage.content` is still a bare `String`. Don't
    assume screenshots or any image content can reach a model today; the
    `WIRING NOTE` at `content.rs:16-22` is the map for whoever picks this up
    (the on-target Slice 1 work — a `capture_screen` tool + platform backend
    — is a separate, not-yet-built piece from this wire-format primitive).
  - **The bundled model catalog ships placeholder hashes.** Every entry in
    `models/catalog.json` currently has `sha256 = "TODO-CURATE"` (or similar
    non-hex placeholder) — `CatalogEntry::is_curated()` correctly returns
    `false` for all of them today (`catalog.rs`'s own test,
    `placeholder_sha256_is_not_installable_curated_is`, asserts this), and
    `download_model` refuses to even start a download for a non-curated
    entry (`ipc/mod.rs:780-784`). **Nothing in the bundled catalog is
    actually installable** until real hashes are curated and shipped — this
    is a content/curation gap, not a code bug, and is headless-buildable
    (doesn't need Lukas's machine).
  - **`set_model_status`/boot-time integrity re-check has no production
    caller.** `GlobalDb::set_model_status` (`storage/global.rs:623-629`) can
    flip a `model_catalog` row between `"ready"`/`"quarantined"`, and the
    doc comments describe an integrity re-check "at boot" — but grepping the
    tree turns up no caller of `set_model_status` anywhere outside its own
    definition. The verified-before-runnable invariant holds at *install*
    time (verify-or-nothing in `download.rs`); the *ongoing* "did this file
    on disk still match its hash" re-check is deferred to the S4
    `llama-server` sidecar slice per `docs/ROADMAP.md` (M8 row) — don't
    assume a tampered/corrupted local model file gets caught today.
  - **SSE parser still buffers the whole trailing partial line in memory**
    (`self.buffer: String`, `sse.rs:78`) with no size cap — unchanged, still
    not guarded against a misbehaving provider sending one huge unterminated
    line.

- **How to extend**
  - **Add/adjust a provider capability flag**: extend `ProviderKind` or add a
    field to `Provider` in `provider.rs`; update both kind-parsing sites
    (`lib.rs:316-319` hydration, `ipc/mod.rs:1704` `parse_kind`) together.
  - **Wire the native-tools UI checkbox** (the one open M4 item): thread a
    `supports_native_tools` control through the add-provider Settings form →
    `AddProviderArgs` → the existing `parse_kind`/`add_provider` path; no
    backend change needed, per `docs/BUILD-MANIFEST.md` item 1.1.
  - **Wire multimodal content**: follow the `WIRING NOTE` at
    `content.rs:16-22` — bridge `assemble_content`'s `Value` output into
    `ChatMessage`/`ChatRequest` serialization (today a bare `String`) without
    stringifying the array, then give it a real caller (a screenshot tool +
    a way to mark a provider/model as vision-capable).
  - **Book usage for `complete()` callers**: mirror the streaming path's
    `record_usage` block (`loop_mod.rs:1474-1502`) in `memory_flush.rs`/
    `skill_reflect.rs` if closing the cost-ledger gap becomes a priority.
  - **Curate the model catalog**: replace `catalog.json`'s placeholder
    `sha256` values with real, verified Hugging Face file hashes — this
    alone makes the M8 download pipeline installable; no code changes
    required.
  - **Add a new HTTP method to the client**: follow `list_models`/`complete`
    in `client.rs` — build the URL from `provider.base_url`, attach bearer
    auth conditionally, check `status.is_success()`, wrap errors with
    `anyhow::Context`.
  - **Change SSE wire-format handling**: extend `SsePayload`/`SseChoice`/
    `SseMessage`/`SseToolCallDelta` in `sse.rs:251-302` and the fallback
    chains in `first_delta`/`tool_call_fragments` (`sse.rs:323-369`) — keep
    per-provider branching out of `ModelClient`; the SSE parser is the one
    place that normalizes shapes.
  - **Tests**: add unit tests to `models/tests.rs` for `provider.rs`/
    `manager.rs`/`sse.rs`/`pricing.rs`/`seat.rs`/`hardware.rs`/`catalog.rs`;
    `download.rs`'s network path (`download_to_partial`) has no mock-server
    harness today — only its pure helpers (`host_allowed`, `verify_and_install`,
    `file_sha256`) are unit-tested. For `client.rs` HTTP request-building
    there's still no mock-server dependency in `Cargo.toml` as of this
    writing; the SSE tests bypass HTTP entirely via
    `SseStream::from_byte_stream` (`sse.rs:105-115`).

- **Tests**
  - Location: `src-tauri/src/models/tests.rs`, gated `#[cfg(test)]` from
    `mod.rs:25-26`.
  - Run just this module: `cd src-tauri && cargo test --lib models::`
  - Live/opt-in: `LHP_NATIVE_ENDPOINT="http://127.0.0.1:1234/v1"
    LHP_NATIVE_MODEL="qwen/qwen3.6-35b-a3b" cargo test --lib
    live_native_tool_call_roundtrip -- --nocapture` (skips itself with a
    printed message when the env var isn't set — safe in CI).
  - Related tests exercising this module indirectly: `agent/loop_tests.rs`
    (fake `ModelStreamer`), `ipc/contract_tests.rs` (`list_models_for` error
    surfacing, around `contract_tests.rs:369-412`), `hooks/tests.rs`/
    `hooks/routing.rs` (`Provider`/`ProviderKind` fixtures for routing
    decisions), `tools/delegate.rs`'s own tests (`resolve_seat` integration).
  - Full suite: `cargo test --lib` from `src-tauri/` — 542 tests passing as
    of 2026-07-21 (HEAD `ca54251`).
