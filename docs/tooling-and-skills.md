# Lost Harness — Tooling & Skills Design (borrowing from claude-code)

> **Orchestrator note (2026-07-07):** synthesized from a study of the public
> `anthropics/claude-code` repo (skills, hooks, agents, permissions, plugins,
> MCP) mapped against Fable's spec (§9/§10/§11/§12, M3) and
> [`server-companion.md`](server-companion.md). Grounded in the actual code
> (`agent/gate.rs`, `storage/schema.rs`, `tools/mod.rs`).
>
> **The spine is build-order items 1→3:** a shared `Capability`/`Tool` trait +
> a native `Hook` chain that unifies the privacy filter (§7), permissions (§10),
> and sandbox (§11) into ONE decision chain, compiled into both bodies.
> Everything else hangs off that spine — build it first, and it also cleans up
> LH's own scattered gating logic (this is worth doing even if the server
> companion never ships).
>
> **One refinement to §3.2 (my call):** for v1, keep skills **prompt + resources
> only** — they orchestrate the existing capability-tagged tools rather than
> shipping their own executable scripts. Defer *executable* skill scripts until
> the WASI sandbox (build item 22) lands. That sidesteps putting a Python
> runtime into the headless server and keeps the v1 trust surface minimal.
> Revisit when there's real demand for skills that need their own code.
>
> **Two decisions are flagged for Lukas** at the end (§6/§7): the cron
> per-profile→global sync path, and requiring an authenticated / Tailscale-only
> local↔server channel before "server = local trust tier" holds. Both are
> server-track (post-M4), not blockers for the M3 spine.
>
> **Superseded — see PLAN.md.** Both of the above are now decided: per-profile
> data sync is opt-in per profile, and the auth requirement is product-owned
> pairing + mutual auth + always-on encryption, not a Tailscale-only channel.

---


*Design doc · grounded against `src-tauri/src/{agent/gate.rs, storage/schema.rs, tools/mod.rs}` and `docs/server-companion.md` · v1*

---

## 1. North star

Lost Harness runs one Rust core that compiles into two bodies — the local Tauri desktop app and an optional always-on headless server companion — and every extensibility surface (tools, skills, agents, hooks, MCP) is expressed as **one shared trait plus one shared schema, gated by one shared decision chain, and differentiated only by an environment-specific `Capability` set and per-body default seed rows.** We borrow claude-code's genuinely good ideas — three-tier progressive disclosure for skills, per-argument permission rules, declarative agent personas, a typed hook decision protocol, and the plugin-as-distribution-unit — but re-implement each natively in Rust so it is cross-platform, privacy-filtered by default, and safe to run unattended. Nothing gets a second "server implementation"; a capability that can't run headless simply reports `available() == false` on that body and the agent is told why.

---

## 2. What we borrow from claude-code

| Capability | CC pattern | LH adaptation | Verdict |
|---|---|---|---|
| Skill packaging | `SKILL.md` dir: YAML frontmatter + body + `scripts/`/`references/`/`assets/`, three-tier progressive disclosure | Same on-disk dir shape; metadata mirrored into a `skills` row (global.db) with embedding for retrieval | **adapt** |
| Skill trigger | Inject every skill description every turn | `search_skills(query)` over embeddings + optional capped/ranked auto-manifest | **adapt** |
| Skill authoring bar | Validation checklist (3rd-person desc, trigger phrases, word count, refs exist) | Deterministic Rust lint pre-check before the `save_skill` approval prompt | **adopt** |
| Skill review | Trusted at install time (no human gate) | Full frontmatter+body+script source rendered in the in-chat `save_skill` gate | **adopt** |
| Skill script host | Ambient Bash/Python on whatever host | v1 Python subprocess w/ explicit `Capability` allowlist; post-beta WASI/wasmtime | **adapt** |
| Permission granularity | `Bash(git commit:*)` per-argument rules, allow/ask/deny, settings hierarchy | `tool_rules` table (tool, pattern, action) + tri-state `mode`; **skip** the managed/project/user hierarchy | **adapt** |
| Bash sandbox | `sandbox.enabled` + `network.allowedDomains`, `excludedCommands` | Steal the config schema now (no-op passthrough), enforce `allowedDomains` at library level immediately; OS sandbox in v2 | **adapt** |
| PreToolUse validator | Regex hook hard-blocks dangerous Bash | Built-in Rust `SandboxHook` denylist (non-overridable, ships by default) | **adapt** |
| Agent personas | `agents/*.md` frontmatter: description+examples, `tools:[]`, `model`, `color` | `agent_types` table; `tools_allowlist`; **`seat`** not literal model; trigger examples | **adapt** |
| Delegate-to-server | *(no CC analogue)* | `delegate target: local\|server\|auto`, capability-union routing, outbox result delivery | **LH-original** |
| Hooks | `hooks.json` → shell scripts at lifecycle events | Native in-process `Hook` trait; unify §7/§10/§11/§12 into one chain | **adapt** |
| Plugin bundle | `plugin.json` + auto-discovered subdirs + `${CLAUDE_PLUGIN_ROOT}` | `pack.toml` Capability Pack + `${PACK_ROOT}` | **adapt** |
| Marketplace | `marketplace.json` index, `/plugin install` | Curated opt-in-fetch JSON index in Settings | **adapt** |
| MCP client | stdio/SSE/HTTP/WS, allow-list per tool | Keep existing `mcp_*` tools; add per-server **trust tier** (Local/Remote) + fold MCP tools into `Capability` registry | **adapt** |
| Slash commands | `.md` command files + `$ARGUMENTS` | Fold argument templating into Skills; Cmd/K palette is the invocation model | **skip** |
| Settings hierarchy | managed > project > user | No enterprise actor; keep only the *structural* trick (per-body default seed rows) | **skip (actor) / adapt (structure)** |
| Hookify / plugin dotfiles | User-authored regex+shell rules; `.local.md` config | Structured "Automations" toggle UI; typed pack settings in SQLite | **skip** |

---

## 3. The system, layer by layer

### 3.1 Tools

The foundation is the `Capability`/`requires()`/`available()` trait already specified in `server-companion.md`, landed concretely in `src-tauri/src/tools/mod.rs` (currently an M3 stub).

```rust
pub enum Capability {
    Filesystem, Network, Shell, Display, Audio,
    ComputerUse, Email, Calendar, WebResearch, LongCompute,
}

pub trait Tool {
    fn name(&self) -> &str;
    fn requires(&self) -> &[Capability];
    fn available(&self, env: &BodyEnv) -> bool { /* requires ⊆ env.capabilities */ }
    fn run(&self, input: ToolInput, ctx: &ExecCtx) -> ToolResult;
}
```

`registry.available_tools(env)` filters on `requires() ⊆ env.capabilities`. Every tool call — native or MCP-provided — passes through the unified hook chain (§3.4) before `run()`.

**Permission granularity** (extends spec §10, which today is whole-tool on/off + one-shot first-use dialog). Three additive layers, none replacing the existing tool toggle:

1. **`tool_profile_permissions.mode`** — widen the `enabled: bool` to a tri-state enum `mode: allow | ask | deny`. `ask` means confirm *every* invocation forever (right for `send_email`, `computer_use`), not just first use.
2. **`tool_rules`** table (profile-scoped): `(tool_name, pattern, action)` — e.g. `(shell_exec, "git commit:*", allow)`, `(shell_exec, "rm -rf:*", deny)`, `(write_file, "~/Documents/Lost-Harness/*", allow)`. Resolution slots into §10 as **step 3.5**: after tree-restriction, before the first-use dialog. Match ⇒ use the rule; no match ⇒ fall back to first-use-confirm-then-remember. Specificity order: deny > ask > allow, most-specific wins.
3. **Built-in `SandboxHook` denylist** — a non-overridable Rust regex floor beneath the user layer (`rm -rf /`, `curl | sh`, `dd if=/dev`, credential-exfil patterns). Ships by default, mirrors the §2 `SYSTEM_DENYLIST` for paths. Closes the asymmetry where LH protects system *paths* absolutely but dangerous *shell commands* only via a one-time human click.

**Sandbox config schema — lock in now, enforce later.** Store per-profile even though v1 keeps library-level `Command::new()` + timeout:

```toml
[sandbox]                       # v1: no-op passthrough EXCEPT network.allowed_domains
enabled = false
auto_allow_if_sandboxed = false
excluded_commands = []
[sandbox.network]
allowed_domains = []            # ENFORCED at library level in v1 (cheap; composes with §12 gate)
allow_localhost = true
allow_unix_sockets = []
```

`network.allowed_domains` is the one piece that does **not** wait for v2 — it gates `shell_exec` egress at library level today, generalizing the §12 Private-route block to explicit domain allow-listing for Public-route commands. v2 enforcement (Seatbelt/`sandbox-exec` on macOS, bubblewrap/Landlock on Linux, AppContainer/job objects on Windows) consumes the same schema with no migration. **Scope note (steal CC's README wording):** sandbox applies to `shell_exec` only, not `read_file`/`write_file`/MCP/hooks — say so explicitly.

### 3.2 Skills

The only area needing real schema work beyond the current stub `skills(id, name, content, created_at)`.

**Schema (extend `storage/schema.rs`, global.db):**

```sql
ALTER TABLE skills ADD COLUMN description           TEXT NOT NULL DEFAULT '';
ALTER TABLE skills ADD COLUMN capabilities_required TEXT NOT NULL DEFAULT '[]'; -- JSON [Capability]
ALTER TABLE skills ADD COLUMN approval_status       TEXT NOT NULL DEFAULT 'pending'; -- pending|approved|rejected
ALTER TABLE skills ADD COLUMN path                  TEXT NOT NULL DEFAULT '';
ALTER TABLE skills ADD COLUMN version               TEXT NOT NULL DEFAULT '0.1.0';
ALTER TABLE skills ADD COLUMN embedding             BLOB; -- same store as memory vectors
```

**On-disk layout** (portable, mirrors CC): `<Lost-Harness>/skills/<skill-id>/skill.md` (frontmatter: `name, description, capabilities_required, version` + markdown body) plus optional `scripts/`, `references/`, `assets/`.

**Three-tier progressive disclosure:**
- **Tier 1 (metadata, always cheap):** the `skills` row — name + description + capabilities.
- **Tier 2 (body, on trigger):** `skill.md` body loaded when `search_skills` matches.
- **Tier 3 (resources, on demand):** scripts execute / references load without ever entering context.

**Trigger:** `search_skills(query)` against `embedding` is the primary path (scales to unbounded user/agent-authored skills, unlike CC's always-inject). Optionally auto-inject a **capped, usage/recency-ranked manifest** (name + one-line trigger phrase) so common skills surface without a round-trip.

**In-chat approval gate (`save_skill`, local body only):** renders the **literal** rendered frontmatter + body + bundled script source — the user reviews the artifact, not a summary. A deterministic **Rust lint runs first** (word-count body, heuristic-check description for concrete trigger phrases vs. vague language, verify every referenced `scripts:`/`references:`/`assets:` file exists in the payload). A failing lint returns the proposal to the agent with the specific violated rule — a low-quality skill never reaches the user.

**Capability-gated scripts.** Skill execution *is* a `Tool` impl wrapping a script invocation; the script's `requires: [Capability...]` frontmatter rides the same `available()` check as native tools. A `ComputerUse` skill reports unavailable on the server and the agent is told why — no runtime failure.

**Cross-platform script execution.** v1: Python only, subprocess, declared `Capability` set passed as an **explicit allowlist** — no ambient shell. Flag raw shell scripts as a portability anti-pattern (mac/win/linux/server). Post-beta: compile to WASI, run under wasmtime — identical sandboxed behavior on all four targets, no system interpreter, which is the real fix for the headless case.

### 3.3 Agents / Delegation

`delegate`/`delegate_public`/`review` + pop-out windows already exist. What's missing is a **named, reusable persona registry**.

**Schema (new global.db table):**

```sql
CREATE TABLE agent_types (
  id TEXT PRIMARY KEY, name TEXT, description TEXT,
  trigger_examples TEXT,          -- structured few-shot (Context/user/assistant/commentary)
  system_prompt TEXT,
  tools_allowlist TEXT,           -- JSON [tool_name]
  seat TEXT,                      -- Writer|Reviewer|Coding|Vision|SttTts|inherit  (NOT a model slug)
  color TEXT,
  proactive INTEGER DEFAULT 0,
  tolerates_async INTEGER DEFAULT 0
);
```

Ship 3–5 built-ins (`code-reviewer` reusing `review`'s route-aware logic, `research-explorer`, a `silent-failure-hunter`-style auditor) plus a Settings → Agent Types editor. `delegate` gains an optional `agent_type` param; omitting it keeps today's freeform behavior.

**Effective tool set** for a delegate = `agent_type.tools_allowlist ∩ registry.available_tools(env)`. This inserts a fourth, tightest layer into §10 resolution: a `code-explorer` delegate cannot even *see* `write_file` even if the profile grants it. Freeform delegates inherit the caller's resolved set.

**Seat, not model.** Bind to a Seat or `inherit`; resolve via `ModelManager.resolve_seat()` (§13's conversation > profile > global fallback). CC's `model: opus` string breaks portability across users/providers — LH already solved this better; generalize `review`'s one-off seat-pull into the schema. `resolve_seat()` gains a **`target: local|server`** param since the server has its own model access.

**`delegate target: local | server | auto`.** Resolve by the `Capability` union across the delegate's resolved tool set — **identical UX to the cron execution-location dropdown**. Touches `Display`/`Audio`/`ComputerUse` ⇒ `server` rejected with explanation. No backend connected ⇒ only `local`. `auto` prefers local while foregrounded, falls back to server only when `tolerates_async = true`.

**Fan-out.** Multiple `delegate` calls in one turn dispatch concurrently (tokio tasks); the parent turn ends once all are *dispatched*, not completed. Results land into pop-out windows / the notification center as they finish. This completes spec §9's already-named escape hatch ("if the model wants parallel calls, delegate to subagents") — just makes the concurrency semantics explicit.

**Result delivery (server-run).** Reuse the outbox verbatim: add `kind: delegate_result | delegate_progress` to `outbound_events`. Connected ⇒ stream deltas live into the pop-out (same as local today). Laptop sleeps mid-run ⇒ server keeps going, enqueues; on reconnect the window shows "was running on server while you were away" and drains, deduped by event id exactly like `cron_result`. Zero new protocol.

### 3.4 Hooks (net-new subsystem)

Not shell scripts. A native in-process `Hook` trait in the shared core crate, compiled identically into both binaries.

```rust
pub enum HookResult { Allow, Deny(String), Ask(String), Modify(ToolInput), Continue }
pub trait GatingHook  { fn on_event(&self, ctx: &EventContext) -> HookResult; }  // sequential, short-circuits
pub trait ObserverHook { fn on_event(&self, ctx: &EventContext); }              // async, never blocks
```

**Events:** `PreToolUse`, `PostToolUse`, `CronFired`, `CronCompleted`, `ProfileActivated`, `AppLaunch`/`ServerBoot`, `Notification`. Reserve a `HookHandler::ExternalCommand(PathBuf)` variant (same `Command::new()`+timeout pattern) as a narrow, capability-gated *later* escape hatch — never the default.

**What it unlocks:**

- **Unify §7 + §10 + §11 + §12 into one PreToolUse chain:** `[PrivacyGateHook, PermissionHook, SandboxHook, FirstUseConfirmHook]`, first-`Deny`/`Ask` wins. Both binaries run the full chain against their own profile config — this is the concrete implementation of `server-companion.md`'s "the privacy filter compiles into the server binary too." A future rule ("never let `computer_use` touch banking sites") becomes one new `Hook` impl, not a three-file edit, and the UI gets **one** place to explain *why* a call was blocked ("denied by: privacy filter — private tree, network tool").
- **CronFired / CronCompleted as a Stop-hook pair** — this is the single highest-leverage adoption. The run-ledger/outbox "hard problem" in `server-companion.md` *is* "deterministically gate a lifecycle transition on external state." `CronFired` checks the ledger `(cron_id, scheduled_at)` + recent heartbeat ⇒ `Skip` (another node claimed it) or `Proceed` (claim + ack). `CronCompleted` ⇒ on success enqueues `outbound_events` (server) or writes SQLite directly (local); on failure blocks/requeues. One trait, two environment-specific impls — a ~30-line impl instead of a bespoke subsystem.
- **Two lanes.** Gating lane runs sequentially and short-circuits (deny must win). Observer lane (telemetry, TRM logging per §3, notification emission) fires async, never adds tool-call latency. **On the server, observer handlers must write durably (SQLite/outbox row) before returning** — a container restart must not lose events, and a slow logging hook has no human to notice the latency.
- **Notification hook → Notification Center / outbox drain.** `ask_human`, first-use confirmations, and "missed while offline" cron messages all route through one point; the "...and 42 more while you were away" rollup becomes one handler, not bespoke UI logic.
- **ProfileActivated / AppLaunch** injects per-profile memory-tagging defaults (§1), seats (§13), and permission set (§10) in one registrable place instead of scattered call sites.

### 3.5 MCP / Plugins

**Keep** existing `mcp_list`/`mcp_execute`/`mcp_health`. Add:

- **Per-server trust tier at registration:** `Local` (spawned by / reachable only from this device — private-tree-equivalent) vs `Remote` (public-tree egress point — calls route through the privacy filter like any cloud endpoint). Without this, a "helpful" remote MCP server is a silent privacy-filter bypass.
- **Fold MCP tools into the `Capability` registry contract** (concept gap worth closing now): MCP tools declare `Capability` requirements via config override, so `registry.available_tools()` filters them through the *same* mechanism as native tools. Otherwise a stdio MCP server driving a GUI registers on the server binary and fails confusingly at call-time instead of being filtered up front.
- **stdio is inherently per-body** — each binary owns its own child processes; only *config* (command/args/capabilities) is shareable, each body independently decides whether to spawn. SSE/HTTP/WS config syncs local→server, but each body assigns trust tier and enforces its own privacy filter independently.

**Capability Packs (plugin equivalent).** `pack.toml` at pack root: `name, version, author, description, requires = [Capability...]`, conventional subdirs `skills/`, `mcp.toml`, optional `cron-templates/`, `agent-types/`. No `commands/` dir (palette covers it), no shell `hooks/` (native `Hook` only, scoped to the pack's own tools, pure-function + timeout, no ambient shell). `${PACK_ROOT}` resolved by Rust core at load. The manifest's `requires` list is exactly what self-reports local/server/both compatibility.

**Marketplace.** Curated, versioned JSON index hosted as a static file; Settings → Skills/Tools fetches on explicit "Check for packs" click (**opt-in-fetch, never auto-poll** — matches the informed-consent posture). One curated index, not public submission, to keep the trust surface small.

**Explicitly skipped:** slash-command files (Cmd/K palette + Skills-with-argument-schema wins), hookify (Automations toggle UI), markdown-dotfile plugin settings (typed SQLite + Settings form, hot-reloaded).

---

## 4. Two bodies

The organizing invariant: **one trait/schema per layer, one gating chain, environment-specific `available()` + default-seed-rows per body.**

| Layer | Local (`lost-harness`, Tauri) | Server (`lost-harness-server`, Docker) |
|---|---|---|
| **Tools** | Registers Filesystem, Display, Audio, ComputerUse, Network | Registers Filesystem (own workspace), Network, Email, Calendar, WebResearch, LongCompute. **`shell_exec`/`computer_use` globally disabled by default**; `ask` fails **closed to `deny`** (nobody to answer) |
| **Skills** | Originates + approves proposals (only body with chat UI + human). `search_skills` over global.db | Executes `approval_status='approved'` skills only, synced one-way local→server; capability-filtered per its `available()`. Never sees pending/rejected |
| **Agents** | Delegates run locally; approve/author agent_types | Runs `tolerates_async` server-target delegates; results via `outbound_events`; same `tools_allowlist ∩ available()` enforcement |
| **Hooks** | Full gating chain against local profile config; observer lane may fire-and-forget | Same chain against server profile config; observer handlers **write durably before returning** |
| **MCP** | Owns its stdio children; Local-tier servers = private-tree | Owns its own stdio children; egress to Remote-tier servers still passes the privacy filter |
| **Sync** | Source of truth; pushes global.db (memory, profile meta, model config, **skill metadata+resources**, **agent_types**) | Receives one-way push; **never writes back** |

**Server default-seed divergence** is the CC settings-hierarchy *structure* without its *actor*: same schema, a server-flavored profile ships deny-by-default seed rows for anything needing physical/interactive confirmation.

---

## 5. Server-companion concept improvements (ranked)

1. **CronFired/CronCompleted as a `Hook` trait pair.** *(post-beta server track, M)* — Highest leverage. The run-ledger/outbox "hard problem" becomes reusable infrastructure; delegate results, skill-triggered notifications, and MCP call outcomes all reuse the same `Hook`-driven outbox path instead of each inventing a delivery protocol. Unlocks everything below at near-zero marginal cost.
2. **Unified PreToolUse gating chain, compiled once, seeded per body.** *(M3, M)* — Foundational. Skills, delegates, and MCP tools all need one place answering "can this run, and if not why." Building it as a registrable chain (not three code paths) makes future rules one-impl changes and gives the UI one block-reason surface.
3. **Server-hosted skills as genuine always-on capabilities.** *(post-beta server track, M)* — The one product surface CC has **no analogue for**: a skill tagged for unattended/cron/inbox-watch execution, approved once locally, then living permanently on the server ("watch this inbox and draft replies," "summarize this RSS feed nightly"). Not "a delegate ran on the server" — a *standing* capability, discoverable via `search_skills` from either body, only meaningful with 24/7 uptime. Composes from already-designed pieces.
4. **`delegate target: local|server|auto` with capability-union routing.** *(M3 trait / M11 integration, L)* — Turns "spawn a subagent" into "dispatch a capability-bounded unit of work to whichever body can do it" — productizing what Friday+Zed do manually. Reuses zero new plumbing beyond #1 and #2.
5. **Mid-task local→server handoff for long-running delegates** *(post-beta stretch, M — speculative)* — If an in-flight local delegate's remaining tool needs are server-compatible (nothing touched Display/Audio/ComputerUse yet) and the laptop is about to sleep, offer checkpoint-and-relocate instead of silently killing it: serialize remaining plan/state into an `outbound_events`-style handoff row, server finishes, delivered via outbox-drain UI. Nearly free once #1 and #4 exist; the one new piece is a **checkpoint serialization format for in-flight agent state**. **Flag: pending real usage data on how often laptop-sleep actually interrupts delegates — do not build speculatively.**
6. **Capability Pack manifests as the atomic "install an always-on capability" unit.** *(M7 manifest / post-beta index, M)* — Bundles skill + MCP config + agent-type + cron-template behind one manifest that self-declares local/server/both via `requires()`/`available()`. Gives the "approval queue for agent-created skills" a real packaging format and non-technical users a way to add capabilities without editing config.
7. **MCP tools folded into the `Capability` registry contract.** *(M3, S)* — Closes the "MCP tool silently fails on the wrong body" gap; makes `registry.available_tools()` a true single source of truth across native + MCP.
8. **Notification rollup as one hook handler.** *(M3 groundwork / post-beta, S)* — Implements `server-companion.md`'s own "don't dump a thousand toasts after a week offline" as a reusable `Notification`-hook consumer.

---

## 6. Spec deltas

**§9 (Agent loop):**
- State that multiple `delegate` calls in one turn dispatch concurrently as independent tokio tasks; the turn ends on *dispatch*, not completion; results land async into pop-out windows / notification center.
- Add `agent_type` (optional) and `target: local|server|auto` params to `delegate`.

**§10 (Tool permissions):**
- Widen `tool_profile_permissions.enabled: bool` → `mode: allow|ask|deny` (`ask` = confirm every invocation).
- Add `tool_rules(profile_id, tool_name, pattern, action)` table; insert resolution as **step 3.5** (after tree-restriction, before first-use dialog); specificity order deny > ask > allow.
- Add a fourth resolution layer: delegate `tools_allowlist` intersection (tightest, checked first).
- Document server default-seed: `ask` → fail-closed `deny`; `shell_exec`/`computer_use` globally disabled.

**§11 (Sandbox):**
- Add the `[sandbox]` config schema now (per-profile), no-op passthrough **except** `network.allowed_domains` enforced at library level in v1.
- Add the non-overridable built-in `SandboxHook` command denylist (ships by default, mirrors §2 path denylist).
- State explicitly: sandbox applies to `shell_exec` only, not read/write/MCP/hooks.

**§12 (Privacy routing):** re-express `PrivacyGate::route()` as `PrivacyGateHook` in the unified chain (behavior unchanged, now composable).

**New §14 (Hooks):** define the `Hook` traits, event set, gating vs observer lanes, and the requirement that server observer handlers persist durably before returning.

**Milestones:**
- **M3** (tools registry): land `Capability`/`requires()`/`available()` in `tools/mod.rs` **with the unified `Hook` chain as its gating mechanism from day one** — not bolted on later. Fold MCP tools into the registry contract here.
- **M4:** `tool_rules` + tri-state mode; skills schema + `search_skills` + lint + approval-gate rendering; `agent_types` registry + `tools_allowlist` intersection; concurrent fan-out.
- **M7:** per-profile isolation w/ `ProfileActivated` hook; `pack.toml` manifest; server-flavored default-seed rows; sandbox v2 enforcement engine.
- **M11 (server track):** delegate `target` integration; `delegate_result` outbox kind; CronFired/CronCompleted hook pair; server-hosted always-on skills.

**⚠️ Blocking implementation gaps to resolve before M11:**
- **`cron_jobs` lives in per-profile SQLite, not global.db** (confirmed in `schema.rs`), yet `server-companion.md` says cron defs sync local→server alongside global.db-shaped things. The one-way sync design names only global.db payloads — it does not define how a profile-scoped table crosses. **Decision needed (Lukas):** define the sync path for profile-scoped rows explicitly — e.g. a `synced_at` cursor per profile db pushed alongside the global.db payload. Don't assume cron defs "just ride" the global mechanism.
  **Superseded — see PLAN.md.** Decided as per-profile opt-in: only a profile
  the user has opted in sends its cron definitions (and the context those
  crons need) to the server at all; a profile left off never leaves the
  device.
- **"Server = same trust tier as local"** holds only for what the privacy filter governs (model egress) *and* only while the sync channel is closed. **Hard prerequisite (Lukas decision):** require TLS + token auth on the WS/HTTP sync channel, strongly prefer Tailscale-only reachability (consistent with the existing friday/cerberus homelab pattern), before that claim is true in practice.
  **Superseded — see PLAN.md.** Decided as product-owned pairing + mutual
  auth + always-on encryption, not a Tailscale-only requirement — the
  security doesn't depend on which network the connection runs over.

---

## 7. Build order

| # | Item | Depends on | Effort |
|---|---|---|---|
| 1 | `Capability` enum + `Tool` trait + `registry.available_tools(env)` in `tools/mod.rs` | — | **M** |
| 2 | Native `Hook` traits (Gating/Observer) + event set in shared core | — | **M** |
| 3 | Unified PreToolUse chain: `[PrivacyGate, Permission, Sandbox, FirstUseConfirm]` (re-express §7/§10/§11/§12) | 1, 2 | **M** |
| 4 | Built-in `SandboxHook` command denylist + `[sandbox]` config schema (enforce `allowed_domains` only) | 3 | **S** |
| 5 | Fold MCP tools into registry (`Capability` on MCP tools) + trust-tier field | 1, 3 | **S** |
| 6 | `tool_profile_permissions.mode` tri-state + `tool_rules` table + step-3.5 resolution | 3 | **M** |
| 7 | Skills schema extension (`description/capabilities_required/approval_status/path/version/embedding`) | — | **S** |
| 8 | `search_skills` (embedding) + capped auto-manifest | 7 | **M** |
| 9 | Skill-as-`Tool` wrapper + Python-subprocess execution w/ capability allowlist | 1, 7 | **M** |
| 10 | `save_skill` lint (deterministic Rust) + in-chat approval rendering | 7, 9 | **S** |
| 11 | `agent_types` table + editor + `tools_allowlist ∩ available()` intersection | 1, 6 | **M** |
| 12 | Seat-based binding in agent_types + `resolve_seat(target)` | 11 | **S** |
| 13 | Concurrent `delegate` fan-out semantics | 11 | **S** |
| 14 | **[server]** Wire up per-profile-opt-in sync gap *(decided — see PLAN.md; was a Lukas decision)* | — | **S** |
| 15 | **[server]** Product-owned pairing + mutual auth + always-on encryption *(decided — see PLAN.md; was a Lukas decision, hard prereq)* | — | **M** |
| 16 | **[server]** CronFired/CronCompleted hook pair + run-ledger + outbox | 2, 14, 15 | **M** |
| 17 | **[server]** `delegate target: local\|server\|auto` + `delegate_result` outbox delivery | 11, 16 | **L** |
| 18 | **[server]** Server-hosted always-on skills (approved-only sync + capability filter) | 9, 16 | **M** |
| 19 | `pack.toml` Capability Pack manifest + `${PACK_ROOT}` loader + typed settings form | 1, 7, 11 | **M** |
| 20 | Notification hook + rollup handler → Notification Center / outbox drain | 2, 16 | **S** |
| 21 | Marketplace opt-in-fetch index + Settings browser | 19 | **M** |
| 22 | *(post-beta stretch)* WASI/wasmtime skill sandbox | 9 | **L** |
| 23 | *(post-beta stretch, speculative)* Mid-task local→server delegate handoff + checkpoint format | 16, 17 | **M** |
| 24 | *(v2)* OS-level sandbox enforcement (Seatbelt/bubblewrap/AppContainer) consuming §4 schema | 4 | **L** |

**Critical path:** 1 → 2 → 3 is the spine; everything else hangs off it. Items 14 and 15 were decision-gated on Lukas and block the entire server track (16–18, 23); both are now **decided — see PLAN.md** (per-profile opt-in sync; product-owned pairing + mutual auth), so they're implementation work, not open questions. Ship 1–13 (local, M3–M4) before touching the server track.

**Decisions flagged for Lukas:** (a) cron per-profile→global sync mechanism [#14] — **superseded, decided as per-profile opt-in, see PLAN.md**; (b) authenticated/Tailscale-only sync channel as prerequisite for the "same trust tier" claim [#15] — **superseded, decided as product-owned pairing + mutual auth + always-on encryption, see PLAN.md**; (c) whether mid-task handoff [#23] is worth a checkpoint-format investment or should wait for usage data — still open.