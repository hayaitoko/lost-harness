# Google productivity round — Gmail, Calendar, and Tasks (2026-07-24/25)

The deferred M7 email/calendar round, built Gmail-first and completed with
Calendar and Tasks on the same per-profile Google connection.

## The decision (M7-Q2)

**Per-user OAuth client + guided in-app setup. No vendor client. No Lost
Harness server.** Each user creates their own Google Cloud OAuth client through
an in-app walkthrough. Client id/secret are install-global (one pasted pair per
install); the Gmail connection (refresh token) is per-profile. Every secret
lives in the OS keychain (the F6 `ProviderSecretStore`), never SQLite.

Why: maximal privacy (mail flows through the user's own credential, never
ours), zero vendor infrastructure, no Google verification / 100-user cap /
warning screens to manage. Cost: a one-time ~5–10-min guided setup — hence the
wizard is a first-class surface.

**Deferred alternative, no rework:** a vendor-owned OAuth client (one-click
connect for users) can be added later as the default path — the core treats
client credentials as data, so it slots in beside the per-user path. It would
require owning a GCP project + passing Google's Gmail-scope security assessment.

## Load-bearing caveat: Testing-status token expiry

An unverified GCP client left in "Testing" publishing status has its refresh
tokens expired by Google after ~7 days (and shows a one-time "unverified app"
consent screen). So **`NeedsReconnect` is a NORMAL state**, rendered as a calm
"reconnect" button — never an error. Publishing the consent screen to
Production (a one-time "unverified" click-through) stops the expiry; the app
works either way with no code change.

## Architecture (as built)

- `src-tauri/src/email/oauth.rs` — PKCE installed-app flow: ephemeral loopback
  listener bound before the consent URL, single-use redirect accept with a
  **state (CSRF) check before any action**, code exchange + refresh, and a
  3-way refresh-failure classification: `NeedsReconnect` (dead grant — never
  retried), `Misconfigured` (wrong client creds), `Transient` (retryable).
  Token-endpoint HTTP behind the `TokenEndpoint` seam.
- `src-tauri/src/email/gmail.rs` — minimal Gmail REST v1 behind `GmailApi` /
  `GmailHttp` / `TokenProvider` seams: list/get (multipart text extraction,
  padding-tolerant base64url) + send (RFC-822 build, header-injection refusal)
  + get_profile. 401-retry-once.
- `src-tauri/src/email/token_provider.rs` — `KeychainTokenProvider`:
  per-profile, in-memory access-token cache, rotated refresh tokens persisted,
  `NeedsReconnect` carried via `NEEDS_RECONNECT_MARKER`.
- `src-tauri/src/email/mod.rs` — keychain key contract (the single source of
  truth for account-key strings). **Secrets have redacting `Debug` impls,
  locked by tests — do not add `derive(Debug)`.**
- `src-tauri/src/tools/email.rs` — `email_search`/`email_read` (**External** —
  off-box egress, surfaced destination, the F2 `egresses_offbox` gate applies)
  and `email_send` (**Dangerous** — irreversible, Once-only Ask, C2-journaled,
  recipient in the approval dialog). Per-call client keyed off `ctx.profile`.
- `src-tauri/src/email/google.rs`, `calendar.rs`, and `tasks.rs` — shared
  authenticated Google JSON transport (bounded 401 refresh retry), Calendar
  v3 events, and Google Tasks v1 default-list operations. `tools/productivity.rs`
  exposes list operations as **External** and mutations as **Dangerous**; the
  Planner screen is the direct human-control path.
- `src-tauri/src/ipc/mod.rs` — `EmailRuntime` (pending OAuth dances + shared
  needs-reconnect flags), Gmail commands, plus live Calendar/Tasks list,
  create/update/delete commands.
- Frontend — `GmailSetupWizard.svelte` (the 6-step per-user walkthrough with
  console deep links for Gmail, Calendar, and Tasks APIs + localStorage
  step-resume + a reconnect variant), `Email.svelte` (live inbox/read/compose;
  escaped text only), and `Planner.svelte` (live events/tasks). Nav re-added.

## Trust posture

- Reads are off-box egress (untrusted content — guard-wrapped by the
  dispatcher). Send is irreversible → Dangerous → always an explicit Ask,
  never a standing grant, C2-journaled.
- The agent path and the screen (human-click) path are separate: a human
  clicking Send in the compose modal IS the consent; the agent's `email_send`
  tool has its own gate.
- **Scope breadth is by design (and matches the app's other egress tools):**
  Google offers no finer Gmail OAuth scope than `gmail.readonly` +
  `gmail.send`, so a connected account grants broad read+send. The guard is the
  per-call gate + approval, not scope narrowing. If narrower is ever needed,
  `gmail.metadata` (headers only, no bodies) is the only lever.

## Completion and manual verification

- **S4 is complete** in `4150d75`: the state-first loopback check,
  total-failure inbox error, agent-tool reconnect flag, client-change token
  cleanup, and compose/loading guards are all landed and covered by the final
  review pass.
- **A live end-to-end connect test** — needs a real GCP client + a browser
  consent, so it can't run in headless CI. Manual: create a Testing client,
  run the wizard, connect, list/read/send, let the token expire → reconnect.
- **Calendar + Tasks are complete.** The existing OAuth client is reused with
  `calendar.events` and `tasks` scopes. Existing Gmail-only grants must reconnect
  once to grant the newly requested scopes; the wizard explicitly asks users to
  enable all three Google APIs.
- **Vendor-client path** (optional, later) — see the deferred alternative above.
