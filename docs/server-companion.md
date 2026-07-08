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

The agent loop, TRM, privacy gate, tool registry, storage — all of it — compiles
into two targets:

| Binary | Environment | Has | Lacks |
|--------|-------------|-----|-------|
| `lost-harness` (Tauri app) | user's device | UI, computer-use, local FS, audio/voice | 24/7 uptime |
| `lost-harness-server` (Docker) | always-on host | headless agent loop, own model access, email/calendar/web, own storage | display, audio, computer-use |

Each runs a **complete agent loop**. They are peers with *different capability
sets based on environment* — not a thin client calling a fat server. This is
literally what Friday (server) + Zed (Mac) already do manually; the companion
productizes it.

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

Both nodes hold the schedule, so both could fire. The always-on server is the
**failure detector / tiebreaker**:

- The local node sends a **heartbeat** (~30–60s) over the persistent connection.
- Each cron fire is keyed `(cron_id, scheduled_at)` in a **run-ledger**.
- The local node owns the first attempt; on running (or at least claiming) a
  fire-time, it writes an **ack** for that key.
- For a **Fallback** cron, the server executes *only if*, after a short grace
  window past `scheduled_at`, it has seen **no recent heartbeat** AND **no ack**
  for that key. Then it records its own run under the same key.
- Both sides dedupe on the key ⇒ **exactly-once** per fire-time. No distributed
  lock — just a ledger + a liveness check (cheap because there are only two
  nodes and one is always up).

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

## Sync model (one-way both directions, no two-way conflict resolution)

- **Local → server:** push memory facts, profile metadata, cron definitions,
  model config. The server gets a working copy; it does not write these back.
- **Server → local:** push results/notifications via the outbox above.
- Neither side blocks on the other. Sync is eventual and best-effort.

## Privacy boundary

The user's own server is *their* infrastructure — same trust tier as a local
model, NOT a new "cloud" dimension in the §7 gate. Caveat: egress *from* the
server to third-party model APIs still passes through the §7 gate on the server
side (the gate compiles into the server binary too).

## Where this lands in the plan

1. **Now/soon (M3):** build the tool registry with the `Capability` /
   `requires()` / `available()` model from the start; extend the `cron_jobs`
   schema with `execution_location` + a `run_ledger`. Keep the §9 loop
   environment-agnostic so it compiles into a future headless binary.
2. **Post-beta track ("Server Companion"):** the Docker image, the two-binary
   split, the heartbeat/outbox/run-ledger protocol, and the Settings connection
   UI. New milestone (e.g. M11) or v1.1 roadmap item.
