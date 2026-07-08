# Lost Harness — Optional Server Companion ("Second Brain")

**Status:** Design note. NOT built. Shapes M3 (tool registry) and the §9 agent
loop; the companion itself is a post-beta track. Captured from Lukas's design
conversation (orchestrator session 2026-07-07).

**Relationship to the spec:** This is an *extension* beyond the current binding
spec (which assumes a single, local, native app — "no server"). It does not
change any existing spec decision. The spec's Decision Log / milestones should
gain a "Server Companion" entry once reconciled with Fable.

---

## Core principle: optional capability multiplier, never a dependency

The desktop app is a complete, standalone product. The server companion is an
opt-in add-on that grants **24/7 capabilities** (crons that run while the laptop
is asleep, always-on email/calendar monitoring, long-running background work).
Disconnect it and the app loses *nothing* — every feature falls back to
local-only.

- The app never blocks on the server. Local SQLite is always the source of truth.
- A standalone user never sees any of this. Server UI/affordances appear only
  once a backend is configured (Settings → "Connect a Lost Harness backend").

## Architecture: same Rust core, two binaries (peers, not client/server)

The agent loop, TRM, privacy filter, tool registry, storage — all of it — compiles
into two targets:

| Binary | Environment | Has | Lacks |
|--------|-------------|-----|-------|
| `lost-harness` (Tauri app) | user's device | UI, computer-use, local FS, audio/voice | 24/7 uptime |
| `lost-harness-server` (Docker) | always-on host | headless agent loop, own model access, email/calendar/web, own storage | display, audio, computer-use |

Each runs a **complete agent loop**. They are peers with *different capability
sets based on environment* — not a thin client calling a fat server. This is
literally what Friday (server) + Zed (Mac) already do manually; the companion
productizes it.

## Connection security: pairing, not passwords (decided)

The security of the app↔server connection is **product-owned, not
network-dependent** — it does not lean on Tailscale, or on any other network
being trusted, to be safe. Tailscale/LAN/internet are all just transport; the
connection is safe over any of them because of what the product itself does:

- **One-time pairing.** The server shows a code / QR / token; it's entered
  once in the app. This establishes a shared cryptographic trust both sides
  remember. No password is ever sent over the wire.
- **Every later connection proves that trust without sending the secret**,
  over an **always-on, end-to-end-encrypted** channel. There is no insecure
  mode — encryption is not a setting that can be turned off.
- **Mutual.** The server also proves it's really the user's server, so a
  look-alike on the network can't impersonate it and harvest a connection
  attempt.
- **Revocable.** Unpair a lost or decommissioned device at any time, no
  server-side reinstall needed.

This is a hard prerequisite for the rest of this doc: "the server is the same
trust tier as local" (see Privacy boundary, below) is only true because of
this pairing scheme, not because the network happens to be Tailscale. Once
paired, Tailscale/LAN/internet reachability is a convenience — which network
gets you there — never the thing standing between the connection and an
attacker.

## Multi-device: the baton generalized (decided)

**Revises the earlier two-peers-only framing.** The baton (PLAN.md §2) isn't
just an app↔server handoff — it's a **per-project lock that any linked
device can hold, one at a time**. A laptop, a desktop, and eventually a phone
all compete for the same lock the app and server always did; this is the
baton grown up, not a new subsystem sitting alongside it.

- Whoever holds a project's lock is the only one who can edit it. Every
  other linked device sees that project **read-only**, with a clear warning
  ("your desktop is working on this — open read-only, or ask it to hand
  over").
- **Multi-device requires a connected server.** The server is the referee —
  the one thing every linked device can always reach to arbitrate who holds
  a lock. A single device with no server configured still works fully
  offline; it just doesn't get multi-device, since there's no referee.
- **Offline editing** works for the one device that grabbed a project's lock
  before going offline — nothing else can touch that project while it's
  gone, because only the lock holder can edit. An offline edit made
  *without* holding the lock is surfaced on reconnect for the user to
  resolve, never silently merged.
- **Takeover.** A device that wants a held lock requests it. The current
  holder releases cleanly if it's reachable. If the holder is unreachable,
  the server times the lock out and grants it to the requester, flagged
  "taken over from an offline device" so nothing is quietly overwritten.

This reuses the same heartbeat/liveness machinery this doc already defines
for cron fallback detection below — the server was always going to need a
way to tell who's alive; project-lock arbitration is the same mechanism
pointed at a different key.

## Environment-agnostic tools (the foundation, lands in M3)

Every tool declares the capabilities its environment must provide. The loop
calls `registry.available_tools()` and never hardcodes "this runs locally."

```rust
trait Tool {
    fn name(&self) -> &str;
    fn requires(&self) -> &[Capability];   // Display | Filesystem | Network | Audio | ComputerUse | Email | ...
    fn available(&self) -> bool;           // does THIS environment satisfy requires()?
    async fn execute(&self, input: Value) -> Result<Value>;
}
```

- Tauri app registers: filesystem, display, audio, computer_use, + network tools.
- Server registers: email, calendar, web_research, long_compute, + network tools.
- Per-OS differences (macOS/Windows/Linux) stay behind `#[cfg(target_os=...)]`
  impls; the headless server is just another "platform profile."
- **Consequence for the UI:** the cron editor validates the chosen execution
  location against the tools a cron uses. A cron that calls `computer_use`
  (Display/ComputerUse capability) can only run Local — Server/Fallback options
  grey out with an explanation.

## Cron execution location (UI dropdown, per cron)

The spec already plans a local `manage_cron` tool + `cron_jobs` table (M3) + a
notification center. This adds an `execution_location` field:

| Mode | Runs on | If laptop asleep at trigger | Requires server |
|------|---------|-----------------------------|-----------------|
| **Local** (default) | device only | skipped; logged as "missed while offline" | no |
| **Server** | server, 24/7 | n/a — server never sleeps | yes |
| **Local → Fallback Server** | prefers device | **server runs it instead** | yes |

Local is the only enabled option when no backend is connected; the other two
grey out.

### The one hard problem: avoiding double execution in Fallback mode

Every linked node holding the schedule could fire it. The always-on server is
the **failure detector / tiebreaker**:

- Each linked device sends a **heartbeat** (~30–60s) over the persistent
  connection.
- Each cron fire is keyed `(cron_id, scheduled_at)` in a **run-ledger**.
- The device that owns the cron owns the first attempt; on running (or at
  least claiming) a fire-time, it writes an **ack** for that key.
- For a **Fallback** cron, the server executes *only if*, after a short grace
  window past `scheduled_at`, it has seen **no recent heartbeat** AND **no ack**
  for that key. Then it records its own run under the same key.
- All sides dedupe on the key ⇒ **exactly-once** per fire-time. No distributed
  lock is needed — just a ledger plus a liveness check, cheap because the
  server is always up and acts as the tie-breaking arbiter regardless of how
  many devices are linked (see "Multi-device," above).

Local-only crons: no server involvement; a missed run is just logged (optionally
surfaced as "missed while offline"). Server-only crons: server owns them
outright; local never fires them.

## Result return: durable server-side outbox + ack'd drain (answers "queue")

Server→local delivery must survive the laptop being offline. Pattern: **the
server enqueues, the local node drains-and-acks on reconnect.**

Server table:

```
outbound_events(
  id, target_node, kind, payload_json,
  created_at, delivered_at NULL, acked_at NULL
)
-- kind: cron_result | notification | email_summary | research_finding
--     | cron_failed | missed_run | ...
```

- When a server cron finishes (or anything the local should know about happens),
  the server **enqueues** an event — it never assumes the laptop is listening.
- **Connected:** push over WebSocket immediately.
- **Offline:** the row waits (`delivered_at = NULL`).
- **On reconnect:** the server streams all undelivered events (or local
  long-polls `GET /outbound?since=<cursor>`). The local node writes each into
  its own SQLite (chat message / notification-center entry) and **acks by id**;
  the server stamps `acked_at`. Idempotent — the local dedupes on event id, so a
  re-delivered event isn't double-applied.
- **Retention / backpressure:** cap the queue (N days or M events); roll up the
  overflow into a single "…and 42 more while you were away" summary so a week
  offline doesn't dump thousands of toasts. Nothing is ever silently lost.

This outbox is for **results and notifications in transit** — work the server
finished that the app hasn't picked up yet. It is not where the user's actual
history lives; see "Event history: stays local," next.

## Event history: stays local (decided)

The event journal — conversations, memory updates, everything the agent did —
lives in the app's own local database. The app never needs the server to read
or write its own history; that's what keeps "works fully without the server"
true even once a server is connected. Two different things share that
journal, and they're retained differently:

- The **permanent record** (conversations, memory, what the agent did) is
  kept in full and searchable. It is never silently dropped.
- The **catch-up buffer** is a bounded rolling window (on the order of a few
  days) whose only job is letting a device that stepped away briefly replay
  the gap on reconnect — this is what the outbox above actually delivers
  into. A device gone longer than the window just does a full re-sync
  instead of replaying; nothing is lost, it's just resynced rather than
  replayed.

Retention only ever concerns the throwaway catch-up buffer — never the user's
actual data. The exact buffer window size is a build-time tuning detail, to
be picked from real usage, not a design decision.

## Sync model (one-way both directions, no two-way conflict resolution)

- **Local → server, per-profile opt-in (decided):** each profile has its own
  "let the server handle this profile? yes/no" setting. Only opted-in
  profiles push their memory facts, profile metadata, cron definitions, and
  the context those crons need, plus model config; a profile left off never
  leaves the device. The server gets a working copy; it does not write these
  back. (The same single-writer property that makes this sync simple — only
  the baton holder ever writes — is why the eventual shared-working-directory
  file sync described in PLAN.md §5 doesn't need a general-purpose
  bidirectional sync engine either: just a push of what changed, gated by
  the baton.)
- **Server → local:** push results/notifications via the outbox above.
- Neither side blocks on the other. Sync is eventual and best-effort.

## Privacy boundary

The user's own server is *their* infrastructure — same trust tier as a local
model, NOT a new "cloud" dimension in the privacy filter. That claim is only
true because of how the connection itself is secured — see "Connection
security," above; trust tier is a statement about the data, and it only
holds if the channel carrying it can't be read or spoofed by anything else on
the network. Caveat: egress *from* the server to third-party model APIs still
passes through the privacy filter on the server side (the filter compiles
into the server binary too).

## Where this lands in the plan

1. **Now/soon (M3):** build the tool registry with the `Capability` /
   `requires()` / `available()` model from the start; extend the `cron_jobs`
   schema with `execution_location` + a `run_ledger`. Keep the §9 loop
   environment-agnostic so it compiles into a future headless binary.
2. **Post-beta track ("Server Companion"):** the Docker image, the two-binary
   split, the pairing/mutual-auth flow, the heartbeat/outbox/run-ledger
   protocol (now also arbitrating per-project lock takeover for multi-device),
   and the Settings connection UI. New milestone (e.g. M11) or v1.1 roadmap
   item.
