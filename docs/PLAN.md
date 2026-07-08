# Lost Harness — Consolidated Plan

**Status:** Source of truth. Supersedes the scattered decisions in `HANDOFF.md`,
`server-companion.md`, `tooling-and-skills.md`, `argos-review.md`, and the
Obsidian `milestones.md` on any point where they conflict — this document is
what those points were resolved *to*. Those docs remain useful for the
detailed reasoning behind each decision; this one is the map.

**Last consolidated:** 2026-07-08

---

## 1. What Lost Harness is

Lost Harness is a personal AI agent that lives on your computer, not in
someone else's data center. It's a desktop app — install it, and it works,
fully, with no internet connection required. It can chat with you, use tools
(read and write files, browse the web, run scheduled jobs, eventually see and
control your screen and talk out loud), and it knows the difference between
things that are safe to send to a cloud AI model and things that should never
leave your machine. If you later want more — a scheduler that keeps running
while your laptop is asleep, an inbox that gets watched overnight — you can
optionally connect a server you own, and the app gets more capable without
ever becoming dependent on that server.

**Non-negotiables:**

- **Local-first.** The app is the product. A server is a bonus, never a
  requirement. Every feature has to work with the server absent.
- **Works fully offline.** No "please connect to the internet" wall for core
  functionality. Local models, local storage, local tools.
- **The privacy gate is load-bearing, not cosmetic.** Every place the app
  calls out to a model — a chat reply, a background summary, a memory
  compaction pass, an embedding — is checked first. Sensitive content is
  routed to a local model or blocked from leaving the device; it is never
  silently sent to the cloud because a code path forgot to check.
- **The optional server is a peer, not a boss.** It runs the same brain the
  app runs. It never becomes a dependency the app quietly grows to need.

---

## 2. Architecture at a glance

There is **one Rust core** — the agent loop, the privacy gate, the tool
registry, the storage layer, all of it. That core compiles into two bodies:

- **The app** (Tauri 2 + Svelte 5, mac/win/linux) — the product. Has a
  screen, a keyboard/mouse it can control, a microphone/speaker, and the
  user's local filesystem. Does not run 24/7.
- **The optional server companion** — same core, running headless on a
  machine the user owns (their homelab box, a small cloud VM, whatever).
  Has no screen and no computer-control, but never sleeps, so it can watch
  an inbox overnight or keep a long job running while the laptop is closed.

They are **twins**, not a client and a daemon it depends on. Each is a
complete, independently-capable agent loop; they just have different tools
available depending on what their environment can offer (a tool that needs a
`Display` capability simply reports "not available" on the server, and the
agent is told why instead of failing mysteriously). The exact wiring between
the two bodies is still being firmed up as the server design gets built out
— treat the app-first, twin-not-daemon shape as locked, and the transport
details as flexible.

**The baton.** When both bodies are reachable, only one of them is ever
allowed to write — that's the "baton." The app holds it while the app is
open. When the app closes, the baton passes to the server, which keeps going
on whatever it was doing (or picks up scheduled work). When the app comes
back, the server finishes whatever single step it's mid-way through and
hands the baton back. Both sides always know whether the other is online,
reusing the same heartbeat mechanism already designed for cron fallback
detection (see §5). Because there is never more than one writer at a time,
**there is no merge-conflict problem to solve** in the common case — nothing
needs to be reconciled, because nothing was ever written twice.

---

## 3. What we're adopting

Two source projects fed into this design. **Fable's reference spec** is a
TypeScript/Node daemon-first agent harness; we reviewed it for mechanisms,
not architecture, and pulled the ideas that don't depend on being a daemon.
Separately, we studied the public `claude-code` project for its tool /
skill / agent / hook / permission spine and are re-implementing the good
parts natively in Rust.

| Idea | Source | What it gives us | Status |
|---|---|---|---|
| Cache-shaped prompt assembly (frozen prefix tiers, live data quarantined to the tail) | Fable's reference spec | Faster local-model responses (KV-cache reuse) and cheaper cloud calls; stops a 24/7 server from re-sending its whole context every heartbeat | Committed |
| Fenced tool-call dialect + "parse only your own current output" rule | Fable's reference spec | Lets small local models call tools reliably even without native tool-calling support; the scan-scope rule stops a webpage or email the agent reads from forging a fake tool call | Committed |
| Guard-wrapped untrusted content (web pages, email, tool output, OCR'd screen text) | Fable's reference spec | A real prompt-injection defense — content the agent didn't generate can never impersonate an instruction | Committed |
| Capability registry that refuses instead of silently degrading | Fable's reference spec | If a model can't honor what's being asked of it (tools, structured output, vision), the app says so loudly instead of quietly mishandling the request | Committed |
| Approval spine: layered deny-wins policy + a hardcoded blocklist nothing can override + pinned/locked approvals | Fable's reference spec | A composable "is this action allowed here" system, a floor that even a "just let it run" mode can't punch through, and protection against an approved action silently drifting into a different one at execution time | Committed |
| Usage ledger + budget governor (local = $0, unknown = a visible "flying blind" flag, never a silent guess) | Fable's reference spec | Real cost accounting — something the app has none of today — and a budget cap for unattended server work where nobody is watching the bill | Committed |
| Durable sequenced event journal + replay | Fable's reference spec | A clean way for a reconnecting client to catch up on what happened while it was away, without re-deriving state by guesswork | Committed |
| Durability trio: crash-recovery boot sequence, idempotency keys on every mutating action, loud-vs-silent failure handling | Fable's reference spec | A desktop app gets force-quit and restarted constantly — this makes that safe: no half-finished work left in a broken state, no duplicate sends from a double-click, no failure that vanishes because the window that triggered it reloaded | Committed |
| Harness-delivered `HEARTBEAT_OK`/`[SILENT]` sentinel + one unified work queue | Fable's reference spec | Solves "don't spam the user with a wall of no-op notifications after a week away," and forces us to consolidate cron jobs, subagent dispatch, and server results into one queue model instead of three overlapping ones | Committed |
| Risk-class taxonomy on every tool (safe / write / external / dangerous) | Fable's reference spec | One deterministic property drives approval prompts, memory scope, and UI badges instead of re-deriving "how risky is this" ad hoc each time | Proposed |
| Memory/turn discipline: frozen per-session snapshot, pre-compaction flush, non-destructive session lineage, a single blocking "ask the user" tool | Fable's reference spec | Avoids stale self-knowledge, long-session amnesia, and a corrupted "waiting for input" state on restart | Proposed |
| `Capability`/`Tool` trait + one shared registry | claude-code | One definition of "what can the agent do" that both bodies use, filtered per-environment instead of hand-coded per-platform | Committed |
| Unified `Hook` gating chain (privacy gate + permissions + sandbox + first-use confirmation as one chain) | claude-code (mechanism) + ours (the four gates it unifies) | One place that answers "can this run, and if not, why" — today those four checks are scattered; a new rule becomes one addition instead of a four-file edit | Committed |
| Skill packaging + three-tier progressive disclosure (name/description always loaded, body loaded on trigger, scripts/resources loaded only on use) | claude-code | Lets the agent learn an unbounded number of playbooks without bloating every prompt with all of them | Committed |
| Rule-granular permissions (`allow`/`ask`/`deny` per tool, plus pattern rules like "allow git commit, deny rm -rf") | claude-code | Finer control than today's whole-tool on/off switch, without a confirmation dialog for every single action | Committed |
| Declarative agent types (named personas with a bounded toolbelt and a model "seat," not a hardcoded model name) | claude-code | Reusable specialists (a code reviewer, a research explorer) that can't accidentally use tools outside their job, and stay portable across whichever model is assigned to their seat | Committed |
| Capability Packs (a bundle format for installing skill + tool config + agent type + cron template together, like a plugin) | claude-code | A single install unit for a whole new capability, usable by non-technical users without hand-editing config | Committed |
| The baton (single-active-writer handoff between app and server) | Ours | Eliminates two-way merge conflicts by construction — there is never a second writer to conflict with | Committed |
| Configurable-host privacy gate (on-device by default, or a designated trusted host) with per-profile cost + history tracking | Ours | Lets a household or small company point the privacy classifier at their own trusted machine instead of every device running its own, while keeping spend and history separated by profile | Committed |
| Hard "must-not-leave-this-host" routing enforcement | Ours (closes a gap Fable's spec left open) | A registry-level guarantee that a PII-flagged request literally cannot fail over to a cloud model under pressure — today's routing is a strong default, not a hard rule | Committed |

---

## 4. The tooling spine

Underneath everything the agent does — using a tool, learning a skill,
delegating to a specialist, reacting to an event — is one shared
foundation, borrowed and adapted from claude-code's design. It's worth
building this first and well, because it also cleans up Lost Harness's own
gating logic, which today is scattered across a few different files.

- **Tools** are things the agent can do (read a file, browse the web, send
  an email). Every tool declares what its environment needs to provide
  (a screen, a filesystem, network access, audio). The registry checks that
  automatically — a tool that needs a screen simply isn't offered on the
  headless server, and the agent is told why instead of the tool just
  failing.
- **Permissions** decide, for a given tool call, whether it's automatically
  allowed, needs to ask the user every time, or is denied outright. This
  now works at three levels of precision: a whole-tool switch, a
  pattern-based rule ("always allow committing to git, always deny `rm -rf`"),
  and — underneath both — a hardcoded floor of genuinely dangerous actions
  that no setting, including a future "just let it run" mode, can override.
- **Skills** are packaged playbooks: instructions plus optional reference
  files and scripts that the agent can look up and follow. They're kept
  cheap by only loading a skill's full content when it's actually
  triggered, similar to how a book's table of contents doesn't cost you
  the whole book. New or agent-proposed skills get a lint pass and a
  literal on-screen review before they're trusted — nothing is silently
  self-installed.
- **Agents** are named specialists — a code reviewer, a research explorer —
  each with a fixed, narrower toolbelt than the main agent, and a "seat"
  (Writer, Reviewer, Coding, etc.) that gets resolved to an actual model at
  run time, rather than being hardwired to one model's name. Multiple
  specialists can be dispatched at once and their results come back
  asynchronously.
- **Hooks** are checkpoints: before and after every tool call, and at
  lifecycle moments like a cron firing or the app launching, a chain of
  checks runs. This is where the privacy gate, the permission system, the
  sandbox rules, and "confirm before first use" all combine into one
  ordered decision instead of four separate, hard-to-audit code paths. Any
  single check saying "no" wins.
- **MCP and Capability Packs** round out the extensibility story: MCP lets
  the agent talk to external tool servers (existing mechanism, kept), and
  a Capability Pack is a single installable bundle — skill + tool config +
  agent persona + cron template together — the plugin-equivalent for
  adding a whole new capability at once.

Two things were deliberately **not** copied from claude-code: slash-command
files (a command palette covers that need) and the "managed > project >
user" settings hierarchy (there's no enterprise-admin actor here — just the
structural trick of each body shipping different default settings, which we
do keep).

---

## 5. The optional server companion

**Status: server-companion track, post-M4.** Not built yet. Everything
below is design, sequenced to land after the tooling spine (§4) exists,
because the server reuses that spine rather than inventing its own.

The server is the same Rust core as the app, running headless, with no
screen and no computer-control, but never asleep. Connecting one is entirely
optional and adds capability without creating a dependency: disconnect it
and the app loses nothing, because local storage is always the source of
truth for the app's own data.

- **The baton handoff.** Described in §2. In practice: the app sends a
  heartbeat every 30–60 seconds while open. A scheduled job is claimed by
  whichever side reaches it first and writes an acknowledgment under a
  `(job_id, scheduled_time)` key; the other side sees that ack and skips
  it. No distributed lock is needed — with only two nodes and one of them
  (the server) always up, a liveness check plus a claim-ledger is enough
  for exactly-once execution.
- **Shared working directory + in-app file explorer.** Once a server is
  connected, agent working directories — memory and project files both —
  can live in a synced folder that shows up as a real explorer panel in
  the app, so a project's work follows the user between machines instead
  of being stuck on whichever device started it.
- **Offline pinning ("download locally").** Marking a project "download
  locally" pulls a full local copy and hands the app the baton for that
  project specifically, so it keeps working with zero network — useful on
  a plane. On reconnect, the app pushes its changes up. Because the app
  held the baton the whole time it was offline, this is a clean one-writer
  push, not a merge.
- **Shutdown protocol.** The app always tries to shut down cleanly first:
  release the baton, flush anything mid-sync. It records whether its last
  shutdown was clean. If the *last* shutdown was unclean (crash,
  force-quit, power loss), the app does **not** try to auto-merge on next
  launch — it detects that a conflict is possible, tells the user plainly
  ("last session ended unexpectedly; the server did X while you were
  gone — keep yours, keep the server's, or show both"), and flags the
  unclean exit rather than quietly guessing.
- **Result queue.** Work the server finishes while the app is offline
  (a cron result, a notification, a research finding) is queued
  server-side and delivered on reconnect, acknowledged by id so nothing is
  double-applied and nothing is silently dropped. A long stretch offline
  gets rolled up into one summary ("...and 42 more while you were away")
  instead of a flood of individual notifications.

The **file-sync engine** underneath the shared-directory feature is real,
standalone engineering — its own subsystem, not a side effect of anything
else in this plan. It is explicitly **not** in scope for M1–M4; see the
build order in §8.

---

## 6. What we build from scratch

Fable's reference spec is an excellent blueprint for the invisible spine —
prompt shaping, approvals, accounting, event replay — because all of that
only depends on "a persistent agent loop exists." It has **nothing** to say
about the parts that make Lost Harness what it is, either because those
parts don't apply to a headless daemon or because the daemon-first design
actively works against them. These are ours to build, unassisted:

- **Real computer/desktop control.** Fable's spec only touches web pages
  (via a browser tool); there's no reading of native-app accessibility
  trees, no synthesizing clicks/keystrokes at the OS level, no screenshot
  loop, no handling of the OS permission prompts (macOS accessibility,
  Windows UAC, Linux portals) this requires. This is Lost Harness's
  flagship differentiator, and it needs a screen and input devices Fable's
  isolated, no-display worker processes were designed to *not* have.
- **Voice as a first-class modality.** Fable's design treats voice as
  cloud-only and optional; Lost Harness needs the opposite polarity —
  on-device speech by default, with interruption ("barge-in") handled as a
  real latency-sensitive requirement, not an edge case.
- **Local-model lifecycle.** Fable's stack deliberately *consumes* model
  endpoints; it doesn't manage them. Lost Harness has to: detect the
  user's hardware, offer a curated list of models sized to what that
  hardware can actually run, download and verify them, and let the user
  assign models to seats. This is the whole promise of "local-first" made
  real, not a side feature.
- **Hard "must-not-leave-this-host" routing enforcement.** A structural
  guarantee, not just a strong default, that a request flagged as
  sensitive cannot fail over to a cloud model even when local options are
  unavailable — it fails loudly instead.
- **Native-app UX and offline-as-the-default.** Fable's only interface is
  a browser tab served by an always-on daemon, where being offline is a
  degraded state to tolerate. Lost Harness needs to feel like a real
  program on the user's computer — a proper window, a menu bar, global
  shortcuts, OS notifications — where working with zero network is the
  common case, not an exception being tolerated.
- **The two-body baton and sync design (§2, §5).** Fable has no concept of
  two independently-capable agent loops reconciling over an intermittent
  connection at all — it's built around exactly one always-on writer. Lost
  Harness's baton model is a genuine improvement on that shape, not a gap
  we're filling in the same way; only the general "durable log you can
  replay from a cursor" idea carries over.
- **Windows support depth.** Fable's design assumes a POSIX-shaped world
  (launchd/systemd-style scheduling, a `/bin/sh` shell). Lost Harness has
  to do real work here — PowerShell/cmd paths, Windows service equivalents,
  and general parity attention, not an afterthought bolted on late.

---

## 7. Open decisions

These are named, understood, and deliberately **not** resolved yet:

1. **File-sync engine choice.** What actually keeps the shared working
   directory in sync between app and server — the specific engine or
   protocol — is unpicked. Real engineering decision, not a detail.
2. **Event journal retention/limits.** How long the durable event log is
   kept and how big it's allowed to grow before older entries roll off or
   get summarized.
3. **Whether true multi-device (more than two) sync ever happens.** The
   baton model is designed and reasoned about as exactly two peers (app +
   one server). Whether Lost Harness ever needs to support more than one
   app instance or more than one server is an open product question, not
   assumed either way.

Two narrower items from the tooling-spine review are also still open and
gate the server-companion track specifically (see §8):

4. **How a per-profile-scoped table (like scheduled jobs) syncs**, given
   the sync design as written only describes syncing the shared/global
   data, not per-profile data.
5. **Requiring an authenticated, ideally Tailscale-only, channel between
   app and server** before "the server is the same trust tier as local" is
   actually true in practice, rather than just true on paper.

---

## 8. Build order / milestones

This reconciles the existing milestone tracker (`milestones.md`, M0–M10)
with the tooling-spine build items and the server-companion track. Existing
milestone numbers and themes are kept as the backbone; spine items and gaps
are slotted into whichever milestone they naturally belong to.

**M0 — Bootstrap. DONE.**

**M1 — Vertical slice (chat, one model, TRM routing). DONE + verified.**
The core loop works end-to-end (message → privacy classification → route →
model → stream → save), is committed, and is proven at the real Tauri IPC
boundary by a contract-test suite (92 tests pass). The earlier arm64/x64
build blocker is fixed (platform pins corrected). Only a nice-to-have
remains — a human eyeballing the live GUI — plus one environment note: the
build machine's Rust toolchain is x86_64 (runs under Rosetta), so binaries
build via translation, not arm64-native; installing an arm64 Rust toolchain
is an optional cleanup, not a blocker.

**M2 — UI shell.** Tiling, profiles, command palette. This is also where
native-app UX work (§6) starts — proper window behavior, per-profile UI
isolation — with tray/menu-bar/OS-notification polish following later in
M8/M9.

**M3 — Tool registry + the spine.** The single biggest milestone in terms
of foundational weight. In dependency order:
   1. The `Capability`/`Tool` trait and the shared registry.
   2. The native `Hook` chain (the checkpoint mechanism from §4).
   3. Today's four scattered gates — privacy, permissions, sandbox,
      first-use confirmation — re-expressed as one ordered hook chain.
      **The hard local-only routing enforcement (§6) is added to the
      privacy gate here**, while it's already being touched.
   4. The non-overridable dangerous-action floor (the hardline blocklist)
      and the sandbox config shape.
   5. MCP tools folded into the same registry, so an external tool server
      is filtered by capability exactly like a built-in one.
   6. The tool-calling architecture itself: the fenced-dialect fallback for
      models without native tool-calling, and the "only parse your own
      current output" safety rule.
   7. Guard-wrapping for untrusted content, since this is when tool
      results (web pages, search results) start flowing into the agent.
   8. The durability trio — crash-recovery on boot, idempotency keys on
      every mutating command, loud-vs-silent failure handling — because a
      desktop app gets force-quit constantly and this is cheap to build in
      now rather than retrofit later.
   9. The approval spine (deny-wins layered policy, pinned/locked
      approvals) extending the permission rules.
   10. The ten core tools from the original plan (file read/write/list/
       search, headless browser, delegate, ask-human, system status,
       cron management, session search).

**M4 — Model manager, running alongside the rest of the tooling spine.**
Two independent tracks that both depend on M3's registry but not on each
other, so they can proceed in parallel:
   - *Model manager track (as originally planned):* local + cloud model
     configuration, model "seats," secure key storage — plus, now added:
     the capability registry that refuses instead of silently degrading,
     the usage ledger and budget governor (kept **per-profile**, resolving
     the earlier open question in favor of the isolation guarantee), and
     cache-shaped prompt assembly.
   - *Skills & agents track:* the skills schema, `search_skills`, the
     skill-as-tool wrapper and its lint/approval flow; the agent-type
     registry (named personas, toolbelt intersection, seat binding); and
     concurrent multi-agent dispatch. Before this locks its schemas, do a
     deliberate pass to make sure scheduled jobs, agent dispatch, and
     server results share **one** underlying queue model instead of three
     overlapping ones (the earlier draft had exactly this
     duplication risk).

**M5 — Computer use (cross-platform).** The flagship gap from §6: platform
implementations for macOS/Windows/Linux, screenshot-driven interaction,
and the OS permission flows each platform requires. Two things get added
here that Fable's spec never had to think about: extending guard-wrapped
content to cover screen/clipboard sources, and accounting for screenshots
in the prompt-budget math (images are large and don't cache the way text
does). Approval UX for irreversible on-screen actions (e.g., "this would
click Send") is its own design pass here — the shell-command-shaped
approval flow from M3 does not just generalize to "which pixel, was it
reversible" for free.

**M6 — Audio (voice).** The other flagship gap. Voice ships as a settings
toggle, on-device by default per the locked decision — this is not an
architecture fork, just a mode. Local STT/TTS, streaming playback,
barge-in, and the privacy gate's audio-specific check (withhold sensitive
audio from cloud TTS without confirmation).

**M7 — Per-profile isolation (email/calendar/tasks), plus the remaining
tooling-spine items that only make sense once profiles are real:** wiring
memory/seat/permission defaults to profile activation, the Capability Pack
manifest and loader, server-flavored default permission seeding (prep work
for the server track), and OS-level sandbox enforcement replacing the v1
no-op passthrough.

**M8 — Settings, onboarding, hardware detection.** This is where the
local-model-lifecycle gap (§6) becomes real: hardware probing, a curated
downloadable model catalog, and seat assignment as part of first-run setup.

**M9 — Polish (auto-update, signing, distribution).** Windows depth (§6)
gets dedicated attention here, though it should be smoke-tested on Windows
continuously from M3 onward rather than discovered late. Remaining
native-OS-citizen polish (tray, menu bar, global hotkeys, notification
center integration) lands here too.

**M10 — Beta release.**

**Server-companion track — starts once M4 lands, runs in parallel with
M5–M9, ships post-beta.** Gated on resolving the two sync-design decisions
in §7 (items 4 and 5) before anything else in this track proceeds:
   1. Authenticated, Tailscale-preferred app↔server channel (hard
      prerequisite — "server = local trust tier" isn't true until this
      exists).
   2. Resolve how per-profile-scoped data (like cron definitions) crosses
      the sync boundary.
   3. The baton protocol itself: heartbeat reuse, claim-ledger, handoff.
   4. The shutdown protocol (clean/unclean detection, conflict surfacing).
   5. The result queue / outbox, with the `HEARTBEAT_OK`/`[SILENT]`
      delivery-suppression sentinel and the "away for a while" rollup.
   6. The durable sequenced event journal + replay, applied specifically
      to the app↔server sync channel (not the in-process UI-to-core link,
      which has no network and no reconnect problem to solve).
   7. `delegate target: local|server|auto` — dispatching a specialist agent
      to whichever body can actually run it, based on the tools it needs.
   8. Server-hosted always-on skills (a skill that just lives on the
      server permanently — "watch this inbox," "summarize this feed
      nightly" — approved once locally, then running unattended).
   9. The shared working directory, in-app file explorer, and offline
      "download locally" pinning (§5) — this is its own real subsystem
      (the file-sync engine, §7 item 1) and is explicitly not scoped
      before this point.

---

## Appendix: naming note

Fable's internal reference spec is referred to throughout as "Fable's
reference spec." Its own internal codename is not used anywhere in this
document, and should not be used as a name for anything in Lost Harness —
Lost Harness is the product name, full stop.
