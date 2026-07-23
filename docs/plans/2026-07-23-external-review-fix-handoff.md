# Handoff — external-review fix implementation (2026-07-23)

> **COMPLETED 2026-07-23.** The build directives below are implemented and
> verified. F12 generated the complete icon set, made clippy correctness
> release-blocking, added advisory RustSec audit plus frontend type-check CI,
> and a real debug Tauri bundle produced both `.app` and `.dmg`. F1 scrubs MCP
> child environments and records the installation-warning contract. F5 exposes
> `trusted_by_name`. F11 applies 7/30/90-day retention and redacts production
> audit args. F2 gates each tool's own off-box destination and prevents Remote
> MCP risk downgrades. F6 moves provider keys to the OS credential store with an
> idempotent, failure-safe legacy migration. F3/F4 remain deliberately
> permissive as directed. Final gates: **683 Rust tests**, clippy correctness,
> no-default-features build, frontend check/build, and debug bundle all green.

**For the next agent.** This continues the 2026-07-23 external security review
(`docs/review.md`) triage. The review was ground-truthed by a 13-agent
verification fan-out; full per-finding evidence lives in that workflow's journal
and in the **`HANDOFF.md` 2026-07-23c entry** (read that first for the scope
corrections and the decision ledger).

## State at handoff

- **Branch `main`, tree clean.** HEAD = `53b6c0d`.
- Relevant recent commits: `e2febfd` (the SAFE review fixes already landed:
  TRM-log purge wired, `Cargo.lock` committed, overclaimed invariants corrected,
  stale `mcp.rs` header fixed), `a0ef23c`/`8f2a18d`/`fceb6fd` (the two frontend
  bug fixes).
- **Gates (must stay green): `cd src-tauri && cargo test --lib` → 675 pass;
  `cargo clippy --lib` → 0 errors (125 pre-existing warnings); `cargo build
  --lib --no-default-features` clean; `npm run build` + `npm run check` clean.**
  Run `cargo` from `src-tauri/`. Toolchain is x86_64-under-Rosetta (expected).
- Adversarially review each change before committing, the way the prior campaign
  did (find→verify). Keep `HANDOFF.md` + `docs/ROADMAP.md` updated per change.

## Lukas's decisions (2026-07-23) — what to build

Four decisions were captured. **Build these four; do NOT build F3/F4.**

| # | Decision | Verdict |
|---|---|---|
| **F6** secrets at rest | **Add OS Keychain now** | BUILD |
| **F2/F13** tool-destination egress gate | **Close it now** | BUILD |
| **F3/F4** fail-closed classifier + Public floor | **Keep permissive** | **DO NOT BUILD** — Public stays a pure user waiver; the rules-only fallback stays as-is. Leave a code comment noting this was a deliberate decision so it isn't "re-fixed" later. |
| **Hardening batch** (F1, F5, F11-B, F12) | **Implement my defaults now** | BUILD all four |

Suggested order: **F12 icons → F1 → F5 → F11-B → F2 → F6** (cheap/isolated
first; F2 and F6 are the two behavior-changing / dependency-adding ones — do them
last, each on its own commit with its own review).

---

## Work items

### F12 — release-CI batch (small; do first)
- **Missing bundle icons (ship-blocker).** `src-tauri/tauri.conf.json:36-41`
  lists `icons/icon.icns` + `icons/icon.ico`; `src-tauri/icons/` has only
  `32x32.png`, `128x128.png`, `128x128@2x.png`. A real `tauri build` fails
  today. Fix: generate the full icon set from a 1024×1024 source with
  `npm run tauri icon <path-to-1024.png>` (writes `.icns`/`.ico` + all PNGs), or
  hand-generate `.icns` via `iconutil` and `.ico`. Verify `tauri build` at least
  gets past the icon step (a full bundle also needs signing — out of scope).
- **`cargo audit` advisory step.** Add to `.github/workflows/build.yml` as a
  non-gating (`continue-on-error: true`) step first — the heavy tree
  (`ort`/onnxruntime, tauri) may trip a pre-existing RUSTSEC advisory; triage,
  then tighten to gating.
- **Clippy gating.** `build.yml:78-81` has `continue-on-error: true`. **CAUTION:
  there are 125 pre-existing clippy warnings**, so you cannot simply gate on
  `-D warnings` — CI would go permanently red. Either (a) clear the 125 warnings
  first (`cargo clippy --fix` handles ~17) then remove `continue-on-error`, or
  (b) leave advisory with a tracking note. Recommended: clear what `--fix`
  auto-fixes, assess the remainder, gate only once clean.
- Also add `npm run check` to the frontend CI step (currently build-only).

### F1 — MCP child hardening (small)
- **Env-scrub the spawned child.** `src-tauri/src/tools/mcp_stdio.rs:55-63`
  spawns with a bare `tokio::process::Command::new(command).args(args)` — no
  `.env_clear()`, so the MCP server inherits the app's full environment
  (including any secrets). Add `.env_clear()` + an explicit allowlist re-inject
  of `PATH`, `HOME`, and (macOS) `TMPDIR`, `USER`, `LANG`. **Test after:** a
  stdio fixture server that needs `PATH` must still start (the existing live
  MCP `sh`-fixture test in the C3 suite is the check).
- **Registration warning.** `register_mcp_server` (`src-tauri/src/ipc/mod.rs`
  ~:1117-1183) is currently only reachable from `tauri.ts:881` — there is **no
  Settings UI** for MCP yet. So the warning belongs in the eventual MCP-settings
  screen (UI phase). For now: add a doc-comment on the IPC command stating MCP
  servers run unsandboxed with full user privileges and auto-respawn at boot
  (`lib.rs:351`), so whoever builds the UI surfaces it. Do NOT sandbox the child
  with the existing `exec.rs` Seatbelt profile — it forbids network + non-workspace
  writes and would break legitimate MCP servers; per-server capability profiles
  are a later, larger slice.

### F5 — LAN/name-trust notice (small)
- `src-tauri/src/agent/egress.rs:24-76` (`is_private_endpoint`) trusts
  `.local`/`.lan`/`.internal`/`.ts.net` by NAME (lines ~70-75, never resolved)
  and RFC1918 IPs as "private." Keep the posture (matches the trusted homelab),
  but add a **one-time, non-blocking UI notice** when an endpoint is trusted by a
  DNS/mDNS *name* (vs a loopback/RFC1918 IP literal): "trusted by name — only
  use on a network you control." The backend can expose a boolean
  (`trusted_by_name`) on the provider/endpoint info; the notice itself is UI-phase.
  Minimum now: the backend flag + a code comment. `provider.rs:122-129` mirrors
  the same check and is the load-bearing one for the gate.

### F11-B — retention windows (small–medium)
- `trm_logs` is now swept hourly (`lib.rs`, done in `e2febfd`). Extend that same
  spawned sweep to the other unbounded tables. **Windows (defaults — confirm if
  you want different):** `work_items` done/failed rows > 30 days; `usage_events`
  — **do NOT hard-delete** (backs budget month-to-date + spend history); instead
  either keep, or roll up to monthly aggregates > 12 months. `tool_audit` is the
  deliberately-defensible audit chain (append-only by design,
  `storage/profile.rs` comment) — add a purge fn with a conservative window
  (e.g. 90 days) AND **redact/hash sensitive canonical args** on insert, since it
  currently stores tool arguments verbatim (the F11 plaintext-retention gap).
  Add `purge_*` fns next to `purge_trm_logs_older_than` (`storage/profile.rs:1214`)
  and call them from the same `lib.rs` sweep loop.

### F2/F13 — tool-destination privacy gate (medium; behavior-changing)
The core fix: the privacy gate keys on the **model** endpoint, not the tool's own
destination, so a `Private` conversation on a **local** model can hand private
args to a Remote-tier MCP / `External` tool.
- `is_cloud` is derived at `src-tauri/src/agent/loop_mod.rs:465`
  (`!is_private_endpoint(provider.base_url)`), threaded via
  `dispatch.rs:681` (`.with_cloud(is_cloud)`), consumed by the gate at
  `agent/gate.rs:112-135` and `hooks/privacy_filter.rs:47-52`.
- **Fix:** at the tool-dispatch privacy check, compute an **effective cloud
  flag = `is_cloud(model) OR tool_egresses_offbox`**, where `tool_egresses_offbox`
  = `RiskClass::External` OR the tool is an `McpTool` whose server tier ==
  `Remote`. Feed THAT into the gate so a `Private` binding (or Auto carrying
  private content) categorically blocks an off-box tool even under a local model.
  The dispatcher already has `tool.risk()`/`requires()`; the MCP tier is on the
  `McpTool`.
- **Also bar the downgrade:** `src-tauri/src/tools/mcp.rs:107-121` (`mcp_risk`)
  lets `Remote` + `read_only_hint` + user `trusted_read_only` drop to
  `RiskClass::Safe`, which bypasses the "External always floors to Ask" backstop
  (`tools/mod.rs:291-295`, `hooks/approval.rs:182-185`, `hooks/permission.rs:404`).
  Never lower a `Remote`-tier tool below `External`.
- **Behavior change to call out in the UI/testing:** after this, a Private chat
  that invokes `fetch_url` or a remote MCP tool will block or require an Ask.
  That's intended. Regression-test it: Private binding + local model + a Remote
  tool ⇒ no off-box send without an explicit approval; and a `trusted_read_only`
  Remote tool still floors to Ask.
- **F13 (search_models → HF):** same theme, but model search inherently must hit
  HF. Minimum: tighten the egress-invariant wording (search/download are
  explicit user-initiated network features, out of the §7 chat gate) and, UI
  phase, warn before a Private-profile search sends text to HF. Not a hard gate
  (would break legitimate search).

### F6 — secrets in OS Keychain (large; do last, own commit)
Provider API keys are plaintext in `global.db` despite the name.
- Write sites: `src-tauri/src/ipc/mod.rs:433` (`add_provider`), `:469`
  (`update_provider`) — `k.as_bytes().to_vec()`. Persist SQL:
  `storage/global.rs:545-551` (INSERT), `:595-601` (UPDATE), row map `:1582`,
  struct field `:33`. Schema: `storage/schema.rs:71` (`api_key_encrypted BLOB`).
  Read/hydrate: `lib.rs` (there's a "M4+ real encryption" deferral comment).
- **Plan:** add the `keyring` crate (macOS Keychain / Windows DPAPI-Credential-
  Manager / Linux libsecret). Store the secret under a per-endpoint
  service/account key (e.g. service `"lost-harness"`, account = endpoint id);
  keep only a reference (or NULL) in the `endpoints` row. Add a **one-time boot
  migration** that moves existing plaintext blobs into the keychain then zeroes
  the column. Rename the column honestly (or repurpose it as a
  present/absent flag).
- **Test caution:** `keyring` needs a real secret store; unit tests + CI must not
  depend on it. Put the keychain access behind a trait with an in-memory fake
  (mirror the `HardwareSource`/`SandboxedSpawn` fake-seam pattern) so tests and
  `--no-default-features` stay green.

---

## Explicitly NOT in this batch
- **F3/F4** — decided KEEP PERMISSIVE (see table). Do not implement fail-closed
  classifier or a Public hard-stop.
- **F7** model attestation — latent; revisit only when the HF search→download
  bridge is actually wired.
- **F10** concurrency (spawn_blocking / lock scope) — deferred robustness, not a
  vuln in single-user local use. The one cheap piece worth doing if you touch the
  IPC boundary: bound `send_message` content length.
- **The two unwired UI event-consumers** (`stream:local_reroute` reroute toast,
  `stream:budget_warning` budget cap — backend emits, no frontend listener) are
  **UI-phase** work Lukas wants his own eyes on. Don't build them here; they're
  the top of the UI backlog. Same for M5 native computer-use backends and MCP
  SSE/HTTP transports.

## After the batch
Re-run all gates, update `HANDOFF.md` (mark each finding resolved with its commit,
and flip the 2026-07-23c ledger items to done), update `docs/ROADMAP.md`, and note
the F2 behavior change prominently so it isn't mistaken for a testing bug.
