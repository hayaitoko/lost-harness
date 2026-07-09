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
while your laptop is asleep, an inbox that gets watched overnight, the same
project followed across a laptop and a desktop — you can optionally connect
a server you own, and the app gets more capable without ever becoming
dependent on that server.

**Non-negotiables:**

- **Local-first.** The app is the product. A server is a bonus, never a
  requirement. Every feature has to work with the server absent.
- **Works fully offline.** No "please connect to the internet" wall for core
  functionality. Local models, local storage, local tools.
- **The privacy filter is load-bearing, not cosmetic.** Every place the app
  calls out to a model — a chat reply, a background summary, a memory
  compaction pass, an embedding — is checked first. Sensitive content is
  routed to a local model or blocked from leaving the device; it is never
  silently sent to the cloud because a code path forgot to check.
- **The optional server is a peer, not a boss.** It runs the same brain the
  app runs. It never becomes a dependency the app quietly grows to need.

---

## 2. Architecture at a glance

There is **one Rust core** — the agent loop, the privacy filter, the tool
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

**The baton.** Only one device is ever allowed to write to a given project
at a time — that's the "baton." Every other linked device sees that project
**read-only**, with a clear warning ("your desktop is working on this — open
read-only, or ask it to hand over"). In the simplest case — just the app, no
server — this is trivial: the app always holds its own baton. Once a server
is connected, the baton is also what lets the app close without losing
anything: it passes to the server, which keeps going on whatever it was
doing (or picks up scheduled work), and hands it back the moment the app
reopens and finishes whatever single step it's mid-way through. Because
there is never more than one writer at a time, **there is no merge-conflict
problem to solve** in the common case — nothing needs to be reconciled,
because nothing was ever written twice.

**Multi-device (decided: supported).** The baton generalizes the same way
to more devices — it's really a **per-project lock that any linked device
can hold, one at a time**. Laptop, desktop, and eventually phone all compete
for the same lock the app and server always did; this is the baton grown
up, not a new subsystem. Two consequences follow directly:

- **Multi-device requires a connected server.** The server is the referee —
  the one thing every linked device can always reach to arbitrate who holds
  a project's lock. A single device with no server configured still works
  fully offline, exactly as before; you just don't get multi-device without
  a server to hand the baton around.
- **Offline editing still works, for whichever device holds the lock.**
  Whichever device grabbed a project's lock before going offline is the
  only one that can edit it, so nothing else touches that project while
  it's gone. An edit made *without* holding the lock is surfaced for the
  user to resolve on reconnect — never silently merged.

**Takeover.** A device that wants a held lock requests it; the current
holder releases cleanly if it's online. If the holder is unreachable, the
server times the lock out and grants it to the requester, flagged "taken
over from an offline device" so nothing is quietly overwritten. Both sides
always know whether the other is online, reusing the same heartbeat
mechanism already designed for cron fallback detection (see §5).

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
| Pairing-based mutual authentication (one-time code/QR/token exchange, then always-on end-to-end-encrypted, revocable trust — never dependent on the network it runs over) | Ours | Makes "the server is the same trust tier as local" actually true rather than true only on paper; Tailscale/LAN reachability becomes a convenience, not the security | Committed |
| Usage ledger + budget governor (local = $0, unknown = a visible "flying blind" flag, never a silent guess) | Fable's reference spec | Real cost accounting — something the app has none of today — and a budget cap for unattended server work where nobody is watching the bill | Committed |
| Per-profile opt-in server sync (only profiles the user opts in send cron definitions + the context those crons need to the server) | Ours | Respects both profile isolation and privacy — a personal profile can stay 100% local while a work profile uses the office server | Committed |
| Durable sequenced event journal + replay | Fable's reference spec | A clean way for a reconnecting client to catch up on what happened while it was away, without re-deriving state by guesswork | Committed |
| Durability trio: crash-recovery boot sequence, idempotency keys on every mutating action, loud-vs-silent failure handling | Fable's reference spec | A desktop app gets force-quit and restarted constantly — this makes that safe: no half-finished work left in a broken state, no duplicate sends from a double-click, no failure that vanishes because the window that triggered it reloaded | Committed |
| Harness-delivered `HEARTBEAT_OK`/`[SILENT]` sentinel + one unified work queue | Fable's reference spec | Solves "don't spam the user with a wall of no-op notifications after a week away," and forces us to consolidate cron jobs, subagent dispatch, and server results into one queue model instead of three overlapping ones | Committed |
| Risk-class taxonomy on every tool (safe / write / external / dangerous) | Fable's reference spec | One deterministic property drives approval prompts, memory scope, and UI badges instead of re-deriving "how risky is this" ad hoc each time | Proposed |
| Memory/turn discipline: frozen per-session snapshot, pre-compaction flush, non-destructive session lineage, a single blocking "ask the user" tool | Fable's reference spec | Avoids stale self-knowledge, long-session amnesia, and a corrupted "waiting for input" state on restart | Proposed |
| `Capability`/`Tool` trait + one shared registry | claude-code | One definition of "what can the agent do" that both bodies use, filtered per-environment instead of hand-coded per-platform | Committed |
| Unified `Hook` gating chain (privacy filter + permissions + sandbox + first-use confirmation as one chain) | claude-code (mechanism) + ours (the four gates it unifies) | One place that answers "can this run, and if not, why" — today those four checks are scattered; a new rule becomes one addition instead of a four-file edit | Committed |
| Skill packaging + three-tier progressive disclosure (name/description always loaded, body loaded on trigger, scripts/resources loaded only on use) | claude-code | Lets the agent learn an unbounded number of playbooks without bloating every prompt with all of them | Committed |
| Rule-granular permissions (`allow`/`ask`/`deny` per tool, plus pattern rules like "allow git commit, deny rm -rf") | claude-code | Finer control than today's whole-tool on/off switch, without a confirmation dialog for every single action | Committed |
| Declarative agent types (named personas with a bounded toolbelt and a model "seat," not a hardcoded model name) | claude-code | Reusable specialists (a code reviewer, a research explorer) that can't accidentally use tools outside their job, and stay portable across whichever model is assigned to their seat | Committed |
| Capability Packs (a bundle format for installing skill + tool config + agent type + cron template together, like a plugin) | claude-code | A single install unit for a whole new capability, usable by non-technical users without hand-editing config | Committed |
| The baton (single-active-writer handoff, generalized to a per-project lock any linked device can hold) | Ours | Eliminates merge conflicts by construction — there is never a second writer to conflict with, even as more devices join | Committed |
| Configurable-host privacy filter (on-device by default, or a designated trusted host) with per-profile cost + history tracking | Ours | Lets a household or small company point the privacy classifier at their own trusted machine instead of every device running its own, while keeping spend and history separated by profile | Committed |
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
  checks runs. This is where the privacy filter, the permission system, the
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

- **Connecting a server: pairing, not passwords (decided).** The security
  of the app↔server connection is built into the product, not borrowed from
  whatever network it happens to run over — Tailscale, plain LAN, or the
  open internet are all just transport, never the thing keeping the
  connection safe. A one-time **pairing** step (the server shows a code /
  QR / token, entered once in the app) establishes a shared cryptographic
  trust both sides remember; no password is ever sent over the wire. Every
  later connection proves that trust without re-sending the secret, over an
  **always-on, end-to-end-encrypted** channel — there is no insecure mode.
  The proof is **mutual**: the server also proves it's really the user's
  server, so a look-alike on the network can't impersonate it. Pairing is
  **revocable** — unpair a lost device at any time. This is what makes "the
  server is the same trust tier as local" actually true, not just true on
  paper.
- **Per-profile opt-in (decided).** Each profile (personal, work, school,
  developer, ...) has its own "let the server handle this profile? yes/no"
  setting. Only opted-in profiles send their relevant data — cron
  definitions and the context those crons need — to the server; a profile
  left off never leaves the device. Within an opted-in profile, the
  existing per-cron Local/Server/Fallback choice is the fine-grained
  control underneath. This respects both the profile-isolation wall and
  privacy — personal can stay 100% local while work runs on the office
  server.
- **The baton handoff.** Described in §2, now generalized to a per-project
  lock any linked device can hold. In practice: each device sends a
  heartbeat every 30–60 seconds while open. A scheduled job is claimed by
  whichever side reaches it first and writes an acknowledgment under a
  `(job_id, scheduled_time)` key; the other side sees that ack and skips
  it. No distributed lock is needed — the server is always up and acts as
  the referee, so a liveness check plus a claim-ledger is enough for
  exactly-once execution and for arbitrating project-lock takeover as more
  devices join.
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
- **Event history stays local (decided).** The event journal — every
  conversation, memory update, and thing the agent did — lives in the
  app's own local database; the app never needs the server to read or
  write its own history, which is what keeps "works fully without the
  server" true. Two different things share that journal: the *permanent
  record* (kept in full, searchable, never silently dropped) and a
  *catch-up buffer* — a bounded rolling window of the last few days whose
  only job is letting a device that stepped away briefly replay what it
  missed. A device gone longer than the buffer window just does a full
  re-sync instead — nothing is ever lost, because retention only ever
  applies to the throwaway catch-up buffer, never to the user's actual
  data. The exact buffer window size is a build-time tuning detail, not a
  design decision — pick it during implementation from real usage.

**File sync — decided: we build our own.** The engine underneath the
shared-directory feature is real, standalone engineering, but it doesn't
need to be a heavyweight bidirectional sync tool: because the baton
guarantees only one device is ever writing to a project at a time, sync is
just "send what changed, when the lock moves" over our own authenticated
app↔server channel — a simple, custom-built push, not a general-purpose
file-sync product. It is explicitly **not** in scope for M1–M4; see the
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

All items formerly tracked here are now resolved — see §5 for the server
ones, and the last bullet below for memory:

- File-sync engine choice → **we build our own** (a simple "send what
  changed when the lock moves" push over our own authenticated channel).
- Event journal retention → **it's all local**; retention only ever applies
  to the throwaway catch-up buffer, never to the permanent record.
- Multi-device → **supported**, via the server as hub/referee; the baton
  generalizes to a per-project lock any linked device can hold.
- Per-profile-scoped sync (e.g. cron definitions) → **per-profile opt-in**;
  only profiles the user opts in send data to the server.
- Authenticated app↔server channel → **product-owned pairing + mutual auth
  + always-on encryption**, not dependent on Tailscale or any other network.
- **Memory sharing across profiles → a per-profile toggle; a walled
  profile gets its own separate memory database (decided 2026-07-08).**
  Default is **shared**: facts live in `global.db`, each tagged with the
  profile it came from, so one coherent assistant remembers you across
  every profile. A per-profile **"keep this profile's memory private"**
  toggle turns that profile into a **full island** — its memory lives in
  its own separate, profile-scoped database, physically apart from the
  shared pool; it reads nothing from shared memory and writes nothing back.
  The separation is **physical, not a query filter**, specifically so that
  switching the toggle off later (or a bug) can never retroactively spill
  what was written while the profile was private — that data was never in
  the shared pool to begin with. This is the version a genuinely
  locked-down / regulated user requires, whose need runs **both**
  directions (no work facts leak out, no personal context leaks in). A
  one-way variant (a walled profile may *read* shared memory but never
  write back) is noted as a possible finer setting later, not the v1
  default. See §9 for how this lands in storage.

One build-time tuning detail carries forward, not a real open decision: the
exact size of the catch-up-buffer window (on the order of a few days) isn't
fixed here — pick it during implementation from real usage.

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
      privacy filter here**, while it's already being touched.
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
barge-in, and the privacy filter's audio-specific check (withhold sensitive
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
M5–M9, ships post-beta.** The two prerequisites below are decided (§5), not
open — but the build order still starts with them, since everything else in
this track depends on a secure, working, correctly-scoped connection:
   1. Product-owned pairing + mutual auth + always-on encryption (§5) — hard
      prerequisite; "server = local trust tier" isn't true until this
      exists. Tailscale/LAN reachability is convenience, not the security.
   2. Wire up the per-profile opt-in sync path (§5) for cron definitions
      and the context they need to cross the sync boundary.
   3. The baton protocol itself: heartbeat reuse, claim-ledger, handoff,
      and the per-project-lock takeover flow that makes multi-device work.
   4. The shutdown protocol (clean/unclean detection, conflict surfacing).
   5. The result queue / outbox, with the `HEARTBEAT_OK`/`[SILENT]`
      delivery-suppression sentinel and the "away for a while" rollup.
   6. The durable sequenced event journal + replay, applied specifically
      to the app↔server sync channel (not the in-process UI-to-core link,
      which has no network and no reconnect problem to solve). The journal
      itself stays local per §5 — this item is about replay over the sync
      channel, not about where the journal lives.
   7. `delegate target: local|server|auto` — dispatching a specialist agent
      to whichever body can actually run it, based on the tools it needs.
   8. Server-hosted always-on skills (a skill that just lives on the
      server permanently — "watch this inbox," "summarize this feed
      nightly" — approved once locally, then running unattended).
   9. The shared working directory, in-app file explorer, and offline
      "download locally" pinning (§5) — this is its own real subsystem
      (the file-sync engine — decided as a custom send-what-changed push,
      see §5) and is explicitly not scoped before this point.

---

## 9. Memory system

**Status: designed in full, not built.** `sqlite-vec` (the meaning-search
engine, see below) is already wired and proven — see the build order (§8,
M3). The rest — the actual hybrid search, the curated summary, the write
triggers — is the memory milestone, not yet started.

### Two tiers

Memory has two distinct layers, each doing a different job:

1. **The curated summary** — a small, bounded, *always-loaded* set of
   durable facts about the user, like sticky notes always in view on a
   desk. It's injected into every conversation automatically; there's no
   separate "go look this up" step. It stays small on purpose — bounded
   size keeps it sharp and cheap — so when it fills up, that forces a
   tidy-up pass. Nothing is lost when that happens: whatever gets trimmed
   out of the summary is still sitting in the archive, just no longer
   pinned to the front page.
2. **The archive** — a full, unbounded, *searchable* record of everything
   the agent has ever saved, like a filing cabinet. It is not preloaded
   into every conversation; it's searched on demand, mid-conversation,
   whenever something relevant might be in there.

### Storage

SQLite — already the app's database — is the engine for both tiers. The
human-facing side of memory (the curated summary and daily notes) is also
kept as plain markdown files on disk, so the user can open and edit their
own memory in any text editor, and could optionally point a tool like
Obsidian at that folder if they want to — the product itself never depends
on Obsidian or any other external app to function.

**Where a fact physically lives depends on the profile's privacy toggle
(decided, see §7).** By default a profile is **shared**: its memory facts
live in the shared `global.db`, each tagged with the profile it came from,
so the assistant carries one coherent memory of you across profiles. A
profile with its **"keep memory private"** toggle on is a **full island**:
its facts live in a **separate, profile-scoped memory database** instead of
`global.db`, physically apart from the shared pool — it neither reads shared
facts nor writes into them. Physical separation (a distinct database, not a
filtered view of one shared table) is the whole point: the wall then holds
even if the toggle is later switched off, which is exactly what a
locked-down/regulated profile requires. This means the storage schema
branches on the toggle — the shared path writes tagged rows into
`global.db`; the walled path provisions and targets that profile's own
memory DB.

Two things deliberately **not** in v1:

- **No separate vector database.** No Qdrant, no external server —
  `sqlite-vec` keeps the meaning-search index inside the same one embedded
  SQLite file the rest of the app already uses. One file, no service to
  run.
- **No knowledge graph / linked notes.** Just simple tags — profile,
  project, date. A real graph structure could come later if it ever proves
  necessary, but it's not a v1 feature.

### Search — hybrid, on demand

Every search runs two kinds of matching together and merges the results:

- **Keyword matching** — SQLite's bundled FTS5 full-text search.
- **Meaning matching** — `sqlite-vec`, now wired and proven (§8, M3). This
  is what lets a search for "that thing about the deploy key" find a fact
  that was actually phrased completely differently, because it matches on
  meaning, not exact words.

Results from both are merged and ranked by a mix of relevance and
recency, and only the top handful come back — never the whole archive
dumped into context. The "meaning fingerprint" for each saved fact is
computed by a small **local** model, so even the act of indexing memory
never sends anything off the device.

### When memory gets written

This is the part worth getting right, because "detect when the
conversation ended" is not a reliable signal to build on — conversations
don't reliably announce their own end. Instead there are three separate
triggers, layered so no single one has to be perfect:

1. **Save-as-you-go (primary).** The moment a durable fact appears in
   conversation, the agent saves it right then — it doesn't wait for
   anything else to happen first.
2. **Pre-compaction flush (safety net).** Right before a long
   conversation gets trimmed down to fit in the model's context window,
   a cheap pass sweeps whatever's about to be trimmed for anything
   durable and saves it first. This is triggered by a concrete, real
   signal — "context is filling up" — not a guess about intent.
3. **New-chat nudge (backstop).** When a new conversation starts, a soft
   nudge prompts a consolidation pass, catching anything the first two
   missed.

The one actual judgment call anywhere in this is "is this fact durable, or
is it just chatter" — that's a call the model makes, and it's gated by
whatever the profile's approval setting says (i.e., this is subject to the
same permission system as everything else the agent does, not a special
carve-out).

### Timing and trust

The curated summary is loaded once, at the start of a conversation, and
deliberately **not** re-churned mid-conversation — this keeps prompt
caching stable and keeps costs down. That does **not** mean the archive is
unavailable mid-conversation: it's searchable at any time through an
always-available, pinned search tool the agent can reach for whenever it
suspects something relevant might be saved. A fact saved mid-conversation
persists immediately and shows up in the curated summary starting next
conversation — it just doesn't retroactively rewrite the summary that's
already loaded into the current one.

Anything recalled from memory is treated as **untrusted content**, the
same guard-wrapping used for web pages and tool output (see §4) — a
poisoned or tampered memory entry can't be mistaken for an instruction and
hijack the agent. And memory obeys the privacy filter on **every** model
call it's involved in, not just the visible chat reply — a work-profile
memory genuinely cannot leak into a personal-context cloud call, because
the same check runs regardless of which call triggered it.

### The genuinely hard part

Flagged honestly, not as a blocker: the hard part of memory isn't storage
or search mechanics, it's **retrieval quality** — surfacing the *right*
memory at the *right* moment, and the agent knowing *when* it should even
bother searching. Two mitigations, not full solutions: the curated summary
hints at what kinds of things exist in the archive (so the agent has a
sense of what's worth searching for), and the agent is nudged to search
whenever the context feels thin. This will need real usage to tune well —
call it out explicitly when building it rather than assuming the hybrid
search alone solves it.

---

## 10. Skills system

**Status: designed in full, not built.** See `docs/tooling-and-skills.md`
for the deeper claude-code-derived reasoning; this section is the decided
shape.

### What a skill is

A **skill** is a reusable playbook — "here's how to do this recurring job
well" — bundling a set of steps plus, optionally, reference files or
scripts. It's a different thing from a **tool**: a tool is one single
capability (read a file, browse the web); a skill is know-how that
*combines* tools with judgment to handle a repeat task well, the same way
a checklist is not itself a tool but tells you which tools to use and in
what order.

### Progressive disclosure

A skill is expensive to keep fully loaded and cheap to keep as a pointer,
so it's loaded in three stages: the name and one-line description are
always cheap to see (so the agent always knows a skill exists without
paying for its content); the full body loads only once the skill is
actually triggered; any bundled scripts or reference files load only when
the skill actually uses them. This is what lets the agent accumulate an
unbounded number of playbooks over time without every conversation paying
the cost of all of them.

### Creation autonomy — a per-profile toggle

Whether the agent can create new skills on its own is a **per-profile
setting**, with two modes:

- **Approve-first** (the **default**) — after finishing a task, the agent
  may draft a new skill, but it's shown to the user to approve, edit, or
  reject before it's ever used for real.
- **Autonomous** — the agent teaches itself new skills and starts using
  them without asking first.

Default is approve-first because it's the secure choice; autonomous is an
opt-in convenience for whichever profile the user trusts with it.
**Manually authoring a skill yourself is always available**, in any
profile, regardless of which mode that profile is in — the toggle only
governs whether the *agent* can create skills unsupervised, never whether
the user can.

### Why "autonomous" is still safe

Autonomous mode only removes the *human approval step for the playbook
itself* — it does **not** expand what the agent is allowed to do. Every
tool call a skill makes still passes through the same one-gate hook chain
as everything else (privacy filter, danger blocklist, permissions — see
§4), so a self-taught skill can never exceed whatever its profile is
already permitted to do; it can only ever get better at doing things it
could already do. On top of that, even autonomous skills:

- get an automated "does it actually work" check (a curator test) before
  being trusted,
- are themselves treated as untrusted text (same guard-wrapping as
  memory and tool output — see §9 and §4), and
- appear in a reviewable "what I taught myself" feed the user can look
  through and edit or delete at any time.

### The learning loop

The intended flywheel: the agent finishes a task → reflects on whether it
was worth saving as a playbook → drafts a skill → (gated by the toggle
above) becomes available for next time. On top of that baseline loop,
**teacher-escalation**: when a small local model fails the same kind of
task twice, a bigger model steps in, solves it, *and* writes a skill so
the small local model can handle that exact kind of task itself next
time — meaning the harness gets more capable the more it's actually used,
not just when someone deliberately sits down to add a feature. A curator
process also periodically re-tests existing skills to catch ones that
have quietly stopped working (a "rot" check).

### Seed skills — not yet decided

Which handful of common recurring tasks should ship pre-loaded as
built-in skills is intentionally **TBD** — a decision for when the skills
system is actually being built, not a design question to resolve now.

---

## Appendix: naming note

Fable's internal reference spec is referred to throughout as "Fable's
reference spec." Its own internal codename is not used anywhere in this
document, and should not be used as a name for anything in Lost Harness —
Lost Harness is the product name, full stop.
