# Argos spec review — inspiration for Lost Harness

> **Status: inspiration, NOT commitment** (per Lukas, 2026-07-08). A critical read of
> Fable's `Argos` harness spec (`~/claude/harness-spec/`, TS/Node daemon-first design)
> against Lost Harness's needs (Rust/Tauri, app-first, two-body, computer-control + voice).
> Produced by a 16-agent review (one critical read per doc + two gap-hunts + synthesis).
> Cross-references [server-companion.md](server-companion.md) and [tooling-and-skills.md](tooling-and-skills.md).

---


## TL;DR

Argos ("argosd") is Fable's TypeScript/Node agent harness built **daemon-first**: one always-on headless daemon owns the database, scheduler, and event journal; disposable runner processes execute turns; and a thin web Console is just one of several clients. Lost Harness is the inverse — **app-first**: a native Rust/Tauri desktop app *is* the product, fully functional offline, with an *optional* headless server companion as a capability multiplier. So Argos's Node stack does not port, but its **mechanisms** do, unevenly: its resource-accounting, untrusted-content, prompt-shaping, and reconnection machinery are excellent and mostly topology-independent (TAKE), while everything that makes LH distinctive — real computer control, voice, native UX, cross-platform Windows depth, local-model lifecycle, and two cooperating bodies — is either absent or *deliberately excluded* because a headless daemon never needed it (MISSING). The genuine forks all trace back to one root tension: how much of Argos's daemon topology (wire protocol, process-per-run, single-writer journal) to inherit into an app that is fundamentally not a daemon (UNSURE). Net: Argos is a superb blueprint for LH's **server companion + autonomy + tooling spine**, and a poor one for its **native-app front half** — mine the former, build the latter yourself.

---

## ✅ TAKE — designed right, adopt into Lost Harness

Ranked by leverage. Merged across all five reviewed docs.

### 1. Cache-shaped prompt assembly with a frozen byte prefix *(highest leverage)*
**Idea:** Split the prompt into Stable/Contextual/Volatile/History/Turn-envelope tiers with explicit cache-breakpoint markers; tiers 1–3 are byte-identical across every round/turn until an explicit re-snapshot event, and all live data (clock, presence, trigger) is quarantined to the tail turn envelope.
**Why excellent:** This isn't just a cloud prompt-caching cost trick — the exact same discipline is what makes llama.cpp/vLLM **KV-cache reuse** effective, which directly determines time-to-first-token for LH's local models on tadashi. It's the single most reusable idea in the whole spec, and it explicitly refuses the "reload full bootstrap+history every idle heartbeat" failure mode a 24/7 server companion would otherwise fall into.
**LH adaptation:** Frozen `Vec<PromptBlock>` per tier, built once per profile session (each profile's AGENTS/SOUL/USER-equivalents load into tier 1); "cacheBreakpoint" becomes a KV-cache checkpoint offset for local backends and `cache_control` for cloud adapters — one code path for app and server.

### 2. Capability registry that *refuses* instead of silently degrading
**Idea:** A `model_capabilities` cache (static < provider-sync < live-probe < config-override) checked **before** dialing any model; a request's computed `requires` set (tools/json_schema/vision/prefill) drops any endpoint that can't honor it with a loud `route.refused` event, rather than sending a call the model will silently mishandle.
**Why excellent:** Converts an entire class of "the model just ignored my tools/schema" bugs into debuggable, loud refusals — exactly the trust-eroding failure a local-first app for non-technical users can't afford. Provider-agnostic; works identically cloud or local.
**LH adaptation:** Port the table verbatim into the **global** SQLite (it's a rebuildable routing cache, not user data); it's the missing routing-intelligence layer atop LH's existing OpenAI-compatible model manager.

### 3. Guard-wrapped untrusted content, enforced mechanically
**Idea:** Every non-operator string (web pages, email, tool output, MCP results, another agent) is wrapped in a `GuardBlock` and rendered just before the adapter with a fresh CSPRNG per-render ID so the payload can't forge its own closing marker; untrusted text is forbidden from ever reaching the system role.
**Why excellent:** A real, load-bearing prompt-injection defense that survives failover — and it matters **more** for LH than Argos, because LH's tool results will include OCR'd screen text, clipboard contents, and scraped web content flowing into an agent that can then click and type.
**LH adaptation:** Port the type + pre-adapter renderer as-is; add `source: screen | clipboard | voice_transcript` variants; run the §7 PII classifier on guard-block content *before* it's rendered. This is the same layer as the privacy filter.

### 4. Tool-call dialect grammar + scan-scope security rule + fenced fallback
**Idea:** Three unified dialects (native JSON / fenced ```tool_name / raw code) under one behavioral template; strict defenses: exact tool-name matching (no prefix), and — critically — the parser scans **only top-level assistant text of the current response**, never fences inside guard blocks, tool results, quoted history, or any tier-1–3 file.
**Why excellent:** The fenced fallback is *how LH gets tool-calling out of small local models at all* (Qwen3-14B and friends don't reliably emit native tool JSON) — and since LH's privacy filter *forces* local routing for PII, this is a functionality precondition, not a nicety. The scan-scope rule (§7.4) is the single most important safety line for an app doing real computer control: without it, any webpage or email the agent reads becomes a code-injection vector.
**LH adaptation:** Adopt essentially verbatim into `tooling-and-skills.md`; it composes directly with the privacy filter's untrusted-content handling.

### 5. Five-layer deny-wins policy engine + hardline blocklist + plan-hashing *(the approval spine)*
**Idea:** Three tightly-coupled primitives — (a) policy as ordered layers (Global→Agent→Lane→Sandbox-state→Channel), any `deny` wins un-overridably, evaluated both at offer-time and call-time; (b) a code-compiled **hardline blocklist** (rm -rf /, curl|sh, writes to ~/.ssh, fork bombs) that no approval mode including "yolo" can bypass; (c) **server-pinned canonical-plan hashing** so an approved `{tool,params}` executes only on byte-exact hash match — drift is a brand-new approval, never a retry.
**Why excellent:** Together they give a composable, auditable "no X in context Y" model, a yolo-proof floor, and closure of the classic approve-X-execute-X′ TOCTOU hole — cheap, static, and exactly right for a product that *will* ship a "just let it run" mode.
**LH adaptation:** Pure in-process Rust module (collapse daemon/runner to the core as trust boundary); add **profile** as a natural 6th policy layer, add computer-control entries to the blocklist, and reuse the identical hashing crate in the server companion for its autonomous runs.

### 6. Risk-class taxonomy as a static tool property
**Idea:** A 4-value enum (safe/write/external/dangerous) declared per tool/action, orthogonal to policy, driving approvals + RAG scope + UI badges without re-deriving risk ad hoc.
**Why excellent:** Right level of abstraction, deterministic, reusable everywhere.
**LH adaptation:** Rust enum on `ToolDef`; add a flag/5th class for `dangerous+irreversible-on-host` — "clicked the Send button in a GUI" isn't captured by shell-command framing.

### 7. Usage ledger (local = $0, unknown = visible zero) + budget governor
**Idea:** One immutable row per model call (including failures/probes); cost waterfall = provider-reported > registry-computed > `local`/free = exactly $0 > unknown = $0 **plus a persistent "flying blind" badge** (never a silent guess). Pre-call reservation leases enforce per-lane budgets with degrade/block/notify.
**Why excellent:** LH has *no* cost-accounting concept today, and the explicit local-$0 + loud-unknown posture is a headline feature for a local-first product, not an afterthought. Budget governance is *more* urgent for the unattended server companion (nobody watching the bill).
**LH adaptation:** Port the ledger schema (minus daemon fields); see UNSURE for which DB owns it.

### 8. Durable seq'd event journal + replay/snapshot-fallback + durable/transient frame split
**Idea:** One `events` table (monotonic seq, past-tense names, JSON payload) is the sole "what happened" broadcast; reconnecting clients send `{seq}` and get REPLAY, a fresh consistent Snapshot (single read txn), or "evicted." Separately, ephemeral high-frequency deltas are `stream` frames — never seq'd, never journaled — and clients reconstruct final state only from coarse durable milestones.
**Why excellent:** Solves reconnect-consistency cleanly, and LH's `outbound_events` table is already a weaker hand-rolled version (delivered_at/acked_at instead of seq + snapshot-fallback). The stream/durable split is what keeps the journal from being swamped once voice audio chunks and computer-use telemetry arrive.
**LH adaptation:** Lift the algorithm onto the **local↔server sync channel** (near drop-in upgrade to the outbox). Do **not** use it for the in-process Tauri-UI↔core link — no network, no reconnect problem there. Classify voice/computer-use per-step telemetry as `stream`-only so the journal never sees high-volume traffic. (Retention window and seq-ownership were open — see UNSURE-B, now superseded — see PLAN.md.)

### 9. Crash-recovery boot sequence + idempotency keys + loud-vs-silent failures *(the durability trio)*
**Idea:** (a) On every boot, in one txn: terminalize non-terminal runs as `failed{crash}`, release budget leases, requeue claimed work — no partial run survives as anything but terminal. (b) Side-effecting calls carry a client `idem` key; a 24h dedupe table replays byte-identical responses and errors on param-mismatch. (c) A user-triggered failure both returns an error *and* writes a durable `*.failed` event.
**Why excellent:** For a desktop app, "the core process restarted" (Cmd+Q, force-quit, crash mid-run) is a *frequent* first-class event — more so than for a daemon meant to stay up. Idempotency kills double-click / double-send / double-cron bugs under retry. Loud-vs-silent guarantees a failure isn't lost just because the window that issued it was reloading.
**LH adaptation:** Run recovery as step 1 of core init (and server-companion boot); require `idem` UUIDs on every mutating Tauri command and every sync RPC; append `*.failed` to the events table on any failing mutation.

### 10. Memory & turn discipline: frozen volatile-snapshot + pre-compaction flush + session-lineage rollover + `ui.ask`
**Idea:** Four small, self-contained patterns — (a) MEMORY/notes snapshotted once per session, refreshed only on 4 enumerated re-snapshot triggers (bounds staleness precisely); (b) before compacting, a bounded best-effort flush turn on a cheap model with only memory+scoped-write tools extracts durable facts; (c) compaction creates a *new* session row with `parent_session_id` and a guard-wrapped summary rather than destructively rewriting history; (d) `ui.ask` is the only `endsTurn` tool — it drives the run to `done` and persists the question on the *session* (72h lazy TTL), so no "waiting" run state exists to corrupt on restart.
**Why excellent:** Each avoids a classic agent-loop bug (stale self-knowledge, long-session amnesia, lost FTS/audit trail, blocked-process-waiting-on-input). `ui.ask` in particular maps beautifully onto app-first: it becomes a real native modal, and because the run is fully terminated, an app relaunch or profile switch mid-question just works.
**LH adaptation:** All four fit the per-profile SQLite directly; force the flush/compression role to a **local** model to keep the privacy filter intact (see UNSURE), and tag summaries with the guard/untrusted provenance LH's classifier already needs.

### 11. Autonomy hygiene: harness-delivers (`HEARTBEAT_OK`/`[SILENT]`) + one-queue model
**Idea:** (P2) The *harness*, not the agent, decides delivery — a sentinel suppresses no-op heartbeat/cron output so silent autonomous work doesn't spam the user. (P12) A single queue model for all deferred/dispatched work instead of separate constructs.
**Why excellent:** P2 directly solves LH's own "…and 42 more while you were away" rollup problem with a proven pattern. P12 is the **most actionable warning today**: LH's `cron_jobs`, `delegate`/subagent fan-out, and `outbound_events`/run-ledger are 3–4 overlapping "deferred work" constructs — exactly the fragmentation P12 exists to name — and it's cheaper to unify before M11 locks the schemas than after.
**LH adaptation:** Adopt the sentinel for both local notifications and server cron/outbox delivery; audit the queue constructs for consolidation now.

*(Also worth quietly taking, lower rank: unconfigured-provider-hides-the-tool-entirely; "no keyword frozensets — corrections are visible scoped config, never hidden heuristics"; the hook system's type-level "plugin hooks can't mutate tool input" and single per-hook observe/enforce knob. Good, not headline.)*

---

## ⚠️ MISSING — gaps we'd have to add ourselves

Severity-ranked. Items marked **[scope choice]** exist because Argos is deliberately daemon-first/headless — they are *not* oversights, and four of them (computer control, voice, local-model lifecycle, two-body sync) are **permanently graveyarded** in Argos on purpose. Treat those as "build from scratch," not "adapt."

1. **Native computer/desktop control — CRITICAL [scope choice].** Argos's only GUI-adjacent tool is `browser.*` (Playwright, web pages only). No accessibility-tree walking of native apps, no OS input synthesis (AX API / UIA+SendInput / AT-SPI), no screenshot-act loop, no OS permission gating (macOS TCC, Windows UAC, Linux portals). Worse, its runner model (`--network none`, no display) is *actively hostile* to hosting this — LH's M5 flagship needs real display/input access, the opposite of Argos's isolation posture. Zero borrowable mechanism.

2. **Multimodal screenshots in the prompt-budget math — HIGH [scope choice].** Tightly coupled to #1: tool results and tier-budget accounting are defined purely in text tokens. A click→screenshot→click loop injects large, non-cacheable vision blocks that blow the 25% tier ceiling. LH's budget math needs an image/vision dimension Argos never modeled.

3. **Voice as a first-class modality — HIGH [scope choice, wrong polarity].** Voice-first is a *permanent non-goal* in Argos; `media.tts`/`transcribe` are cloud-only, off by default, no local adapter, no `audio` stream kind, no wake-word/VAD/barge-in. LH's privacy filter requires sensitive audio to route **on-device by default** (whisper-rs/Piper, M6) — Argos assumes the reverse trust direction. Barge-in specifically (interrupt-and-re-listen) is a hard latency-sensitive requirement Argos's deliberate discrete `run.abort` doesn't address.

4. **Local model/GPU lifecycle & download — HIGH [scope choice, explicitly refused].** Graveyard item: *"Argos consumes endpoints, it does not run them."* No hardware probing (Metal/CUDA/Vulkan/RAM/disk), no curated downloadable catalog, no bundled TRM with SHA256 auto-update, no per-seat local/cloud assignment. This is LH's product promise and the one place Argos's *decision*, not just its stack, is wrong for us. Build from scratch.

5. **Two-body local↔server reconciliation — HIGH [scope choice, LH is ahead].** Argos is single-writer, one daemon, one `state.db`; P1 ("one daemon, many workers") is worker isolation *within* one daemon, a different axis. It has no concept of two independently-authoritative agent loops that dedupe cron work and reconcile over an intermittent link. LH's `server-companion.md` already solves this better (capability routing, heartbeat + run-ledger exactly-once dedup, ack-drain outbox, one-way sync). Borrow only the general "durable event log, replay by cursor" idea (§8 above); build the rest.

6. **Native-app UX + offline-as-default — HIGH [scope choice, inversion].** Argos's only UI is a daemon-served PWA; offline is a *degraded state to tolerate*, not the default. No native window model, tray, menu bar, global hotkey, single-instance, deep links, OS notification center, or "no backend configured — hide these affordances" mode. LH's whole differentiator is being a real OS citizen that works with zero network as the common case. Argos's replay solves "a tab reconnected to the always-up daemon" — a genuinely different problem from "the app was dark for a week."

7. **Approval UX for irreversible native-GUI actions — MED-HIGH (tied to #1).** The approval spine (§5 above) is excellent but shaped entirely around shell/fs/msg actions — no way to classify "which pixel, was it a Send button, was it reversible," no native modal / Touch-ID-gated confirmation contract (a native app dialog blocks the UI thread differently than "post a message and wait on a channel"). Treat as its own design item under the computer-use milestone; don't assume the shell-shaped broker generalizes for free.

8. **Hard `local_only` routing enforcement for the privacy filter — MED-HIGH.** The capability registry (§2 above) has no "must not leave the device" capability; routing is role/cost/failover-driven, and a role default can be overridden by failover to a cloud fallback. LH needs a registry-enforced `local_only` requirement + a `local_required` failure class so a PII-flagged request *literally cannot* route to cloud even under failover pressure, and fails loud when local candidates are exhausted. Small structural addition, high stakes.

9. **Windows support depth — MED-HIGH [scope choice, POSIX-first].** launchd + systemd only (no Windows service); `shell.run` hardcodes `/bin/sh -c` (no PowerShell/cmd path); Docker sandbox assumes trivial availability (WSL2 friction unaddressed). LH's own milestones already flag this. Partial credit: Windows Credential Manager *is* named for secrets.

10. **Per-profile isolation threaded through session/routing/ledger — MED [LH is ahead].** `session_key` has no profile axis; events fan out to all connections; config/credentials/spend are single-global. LH's two-DB isolation needs `profile_id` woven through sessions/runs/memory-snapshot/lineage, per-profile credential sets, and profile-scoped event fan-out. Broadcasting all events to all connections is *actively wrong* if a window or the server should only see one profile. Don't weaken LH's design to match Argos's single-tenant shape.

11. **Offline/local-model degradation path — MED [scope choice].** Compaction/flush/summary calls assume a model endpoint is basically always reachable with clean failover. For LH "fully offline, local model slow or mid-load" is routine, not an edge case — needs first-class, tested handling (defer compaction until reconnect / degraded local with wider truncation), not inherited optimism.

12. **Native OS integration chrome — LOW-MED [scope choice].** The *notification policy* (quiet hours, payload discipline, badge counts) is a solid borrowable pattern; the Web Push *transport* and the absence of tray/menu-bar/hotkey/"open with" are not, because the Console is a browser client, never an OS citizen.

*(Also note: the fixed 25% tier ceiling / 80% compaction threshold are tuned for 200k cloud windows and need per-model-capability recalibration for 4k–32k local windows — LOW.)*

---

## ❓ UNSURE — your call

Each is a real fork where I have a lean but the decision is genuinely yours because it trades off LH's core identity or locks in schema.

### A. **The daemon-first vs app-first topology question — BIGGEST, and it's really three linked decisions**
**The decision:** How much of Argos's daemon *process topology* to inherit — (i) a real socket/WS wire protocol between Svelte UI and Rust core, vs native Tauri IPC; (ii) process-per-run isolation (spawn an OS process per agent turn) vs in-process async tasks; (iii) whether the server companion binary bundles scheduler-liveness and agent-loop execution in one process.
**Tradeoff:** A wire protocol + process-per-run buys multi-window consistency, crash containment, and a clean server seam, but adds serialization/spawn latency that's *actively bad* for computer-use and voice (they want direct low-latency display/input/audio access), and it's oversized for a single-operator app. Native IPC + in-process is faster and simpler but risks each window inventing ad-hoc state-sync. Crucially, Argos's P1 (the daemon *never* runs the thing that can hang) exists precisely so a stuck turn can't stall the cron ticker — and LH's current server sketch bundles them, which is arguably the exact mistake P1 warns against.
**My lean:** Steal the **data model, not the transport** — implement the events/seq/idem/snapshot *schema* as the shared substrate under both native Tauri IPC (primary UI path) and the server sync channel. Keep the agent loop **in-process** (Rust panic/catch_unwind + task cancellation gets most of the crash-containment Node needed processes for). Satisfy P1's *intent* in the server without a warm-spare runner pool: run genuinely dangerous tool execution (shell, browser, skills) as sandboxed OS **subprocesses**, and structure the scheduler tick so it's never blocked on an in-flight model call. Don't build argosd.
**Why yours:** It sets the core's fundamental shape and the latency budget for the two flagship features; it's expensive to reverse.

### B. **Who owns the canonical event `seq` in a two-writer world**
**The decision:** Argos's replay is coherent only because one daemon owns one `state.db` and one global-monotonic `seq`. LH has two independently-authoritative SQLite DBs. If you want Argos-grade replay on the shared conversation, which side (local or server) owns the canonical seq?
**Tradeoff:** Local-owns keeps "local is truth" pure but the server can't advance seq while the app is dark for a week; server-owns contradicts the source-of-truth rule. Neither is "the" writer the way argosd is.
**My lean:** Local owns seq for its own conversation; the server's outbox stays a *second consumer* keyed by its own id-space, reconciled one-way — i.e. don't force a single global seq you don't need. Revisit only if true multi-device sync arrives.
**Why yours:** It's a load-bearing architectural decision LH hasn't made, and it interacts with the retention-window sizing (72h/50k is tuned for daily reconnects, wrong for week-plus offline).
**Superseded — see PLAN.md.** Multi-device sync is now decided (supported, via the server as hub/referee arbitrating a per-project lock), and the event journal is decided as fully local — the "retention window" question only ever applies to a bounded, throwaway catch-up buffer, never to the permanent record. Local owning seq for its own conversation still holds.

### C. **Which DB owns the usage ledger**
**The decision:** Capability registry clearly belongs in the global DB (rebuildable cache). But per-call cost rows — global DB with a `profile_id` column, or split per-profile?
**Tradeoff:** Global = one place for cross-profile dashboards; per-profile = matches the isolation pillar (a work profile's spend shouldn't be queryable from a personal-scoped connection).
**My lean:** Per-profile ledger tables with an opt-in aggregation view — privacy isolation is a stated pillar and should win the tie.
**Why yours:** It's a direct expression of how literally you take the isolation guarantee.

### D. **Cloud-first vs local-first default for STT/TTS (voice polarity)**
**The decision:** Argos ships media roles cloud-only-by-default, local as plugin territory. Flip it and ship bundled local whisper.cpp/Piper as p0?
**Tradeoff:** Local-first is more v1 engineering (cross-platform model packaging) but is the only default consistent with LH's privacy-first, voice-capable identity; cloud-first is faster but means a privacy-conscious user's very first voice interaction goes to a cloud API.
**My lean:** Flip it — local STT/TTS as p0, cloud as opt-in upgrade. This is a roadmap/identity call, not an architecture detail.
**Why yours:** It's a product-positioning decision with real v1 scope cost.

### E. **Does the privacy filter intercept *every* model-call site, or just the main chat call?**
**The decision:** Argos treats the compaction/flush "compression role" as an operator config choice, *not* a data-driven routing decision. If a work-profile session with PII gets compacted via a cloud compression role by default, that's a privacy-filter bypass through a path the filter doesn't currently watch.
**Tradeoff:** Simpler to gate only the visible chat call; correct to gate flush, summary, and finalization rounds too.
**My lean:** The privacy filter must intercept **all** model_ref selection points (flush, summary, finalization, embedding), stated explicitly in the spec — not assumed.
**Why yours:** It's a security-scope confirmation only you can ratify against the §7 spec.

*(Lower-stakes forks I'd resolve without you: keep strict per-session FIFO but expect finer-grained session_keys for voice responsiveness; keep hooks `observe` by default but ship computer-control/fs-write/shell matchers defaulted to `enforce`; take OpenRouter's `kind:"aggregator"` shape + forced-param-passthrough but defer credits-polling/BYOK/`:free` probing post-v1; make round-limits per-lane-configurable rather than Argos's flat 40.)*

---

## How I'd actually use this

Treat Argos as the reference design for Lost Harness's **invisible spine — the server companion, the autonomy loop, and the tooling/model plumbing — not for the app users actually see.** Everything Argos does well is about a persistent process disciplining itself: accounting for every token (ledger + budget governor), never trusting external content (guard blocks), never silently degrading (capability registry, loud-vs-silent, hardline blocklist), shaping prompts so caches stay warm, degrading gracefully onto small local models (fenced dialect), and reconciling state after a disconnect (seq'd journal + replay). All of that maps cleanly onto LH's server companion (which *is* a daemon) and onto both bodies' shared agent-loop and policy engine, because it depends only on "a persistent process exists" or "an agent loop exists," never on daemon-first topology. Mine those sections aggressively — several are near-drop-in upgrades to designs LH has already hand-rolled more weakly. But build the entire native-app front half — computer control, voice, offline-first UX, cross-platform packaging, local-model lifecycle, two-body reconciliation — as original work, because Argos didn't just skip those; it deliberately excluded them, and its process model actively fights several of them. Inspiration for the governance layer; blank sheet for the product.