# Spec — Provider endpoint correctness + connector robustness + app self-update

Date: 2026-07-29. Author: session with Lukas. Status: SPEC — build via the round-2 ultracode
campaign AFTER the review-fix batch merges. Where this disagrees with PLAN.md, PLAN.md wins.

## Item 1 — Provider endpoint routing bug (HIGH, user-observed)

**Symptom (Lukas, hands-on testing):** added a model on an OpenAI endpoint; the app appeared to
send requests to Anthropic endpoints instead.

**Code facts (verified 2026-07-29):**
- The backend model client speaks ONLY the OpenAI-compatible surface: `GET {base_url}/models`,
  `POST {base_url}/chat/completions`, Bearer auth (`models/client.rs:154-253`). There is no
  Anthropic `/v1/messages` support anywhere.
- `ProviderKind` is `local|cloud|custom` — a routing/privacy label, NOT an API format
  (`models/provider.rs:20`).
- `ModelManager::get_client(provider_id)` returns whatever provider id it is handed
  (`models/manager.rs:83`) — so a wrong-endpoint request means the WRONG PROVIDER ID arrived
  from upstream (frontend picker / chat store / seat resolution / conversation binding), or a
  silent fallback picked a different provider than the user chose.

**Root-cause candidates to investigate (in order):**
1. Model picker sets the model string but keeps a stale/default provider id (composer + route UI
   was reworked 2026-07-25, `feat: refine composer controls and route UI` — prime suspect).
2. Seat bindings (per-profile model seats) resolving the turn to a different provider than the
   picker shows.
3. The conversation's persisted provider/binding overriding a newly picked provider on an
   existing conversation.
4. A fallback path (first provider / default provider) when the model id doesn't match the
   selected provider's model list.

**Required behavior:**
- A turn MUST go to exactly the provider the user selected, or fail loudly — never silently
  fall back to a different provider (this is also a privacy invariant: a "wrong provider" can
  be a wrong TRUST ZONE, not just a wrong vendor).
- The UI must show, per turn, which provider+endpoint actually served it (extend the existing
  route indicators; see docs/TECH-DEBT.md §1 "Authoritative route state" — same contract).
- Regression tests: frontend picker→request contract test; backend test that an explicit
  provider id is honored and an unknown id is an error, not a fallback.

**Repro harness:** stand up a fake OpenAI-compatible endpoint on 127.0.0.1 (existing test
fakes can serve), add it as a provider in the live app, send a turn, and assert the request
lands there — first to reproduce the bug, then as the permanent live QA check.

**Adjacent truth-up (same area):** if the app offers an "Anthropic" preset/suggestion anywhere,
verify it points at Anthropic's OpenAI-compatible surface and actually works with this client;
otherwise remove or relabel it. An entry users can add but that can never work is a trap.

## Item 2 — Connector robustness sweep ("the others, if they're broken")

**Google connector 403 blindness (verified broken):** `GoogleClient::json` surfaces any non-2xx
as a raw string (`email/google.rs:64-88`), and the reconnect banner only lights on the
refresh-token marker (`ipc/mod.rs::note_reconnect_if_needed`, `token_provider.rs:29`). There is
NO handling of `403` anywhere in the email/ or ipc/ code. Consequence Lukas's setup can hit: an
older Gmail-only grant, or a GCP project without Calendar/Tasks APIs enabled, makes the Planner
fail forever with raw `Google API HTTP 403` text and no recovery path.

**Required behavior:**
- Classify Google 403 bodies: `insufficientPermissions`/`ACCESS_TOKEN_SCOPE_INSUFFICIENT` →
  flip `needs_reconnect` (reconnect re-consents with the full scope set — verified: begin_auth
  sends all four scopes + `prompt=consent`); `accessNotConfigured`/`SERVICE_DISABLED` → a
  distinct "enable the API in your Google project" state with the console URL surfaced calmly
  in the Email/Planner setup UI (NOT a reconnect loop, reconnecting can't fix a disabled API).
- Tests via the existing fake token-endpoint/HTTP seams for both 403 flavors + the happy path.
- While in the area: quick audit that Gmail/Calendar/Tasks IPC paths all run
  `note_reconnect_if_needed` (or the new classifier) — no connector call should be able to fail
  with a connection-state error that leaves the banner dark.

## Item 3 — App self-update from GitHub ("add update method")

**Ask (Lukas):** when the app opens, it checks GitHub for a newer build and updates itself.

**Design: Tauri v2 official updater (`tauri-plugin-updater`).**
- On launch: non-blocking check (never delays the window); if newer, a calm banner/toast
  "Update available vX.Y.Z" → user clicks → download+install → relaunch prompt. No silent
  background installs.
- Settings → About: current version (IPC already returns the real version), "Check for
  updates" button, and a toggle for the launch check. Default ON per Lukas's ask — but the
  check is a network egress in a privacy-first app, so it must be visible: label the toggle
  with exactly what is sent (a version request to GitHub, nothing else), and log it like other
  egress. Dev builds skip the check.
- Release artifacts: `latest.json` manifest + signed macOS bundle (`.app.tar.gz` + signature)
  attached to a GitHub Release per tag. CI job on tag push builds arm64-macOS (per the P17
  release-matrix decision — coordinate with the post-merge `build.yml`), signs, and uploads.
  Version source of truth: `tauri.conf.json` (currently 0.1.0); tag = `vX.Y.Z`.
- Updater signing: minisign keypair — **ceremony performed by Lukas 2026-07-29, pre-campaign.**
  Private key: `~/.tauri/lost-harness-updater.key` (password in his password manager); public
  key beside it (`.pub`); repo Actions secrets `TAURI_SIGNING_PRIVATE_KEY` +
  `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` are set. The build wires the `.pub` contents into
  `tauri.conf.json` and CI signing from those secrets. If any of the three artifacts is
  missing at build time, flag NEEDS-LUKAS — never substitute a generated placeholder key
  (the P09 lesson).

**RESOLVED (Lukas, 2026-07-29): the repo is now PUBLIC.** The updater fetches `latest.json`
and release assets anonymously from `github.com/hayaitoko/lost-harness` releases — no PAT, no
auth seam, no token storage. (Decision history: a keychain-PAT design was considered and
dropped when Lukas chose to make the repo public.)

**Acceptance:** a staged release (v0.1.0→v0.1.1 with a visible marker change) is detected,
downloaded, installed, and relaunched on the real app; a tampered/unsigned artifact is REFUSED
(test both); disabling the toggle produces zero update-related egress at launch (verify by
capture); all existing gates stay green.

## Sequencing & constraints

- **Blocked behind the review-fix batch**: build on the post-merge state (P17/P21 rewrite
  `build.yml`; P18 touches `tauri.conf.json` CSP; P04/P05 own the secrets IPC this reuses).
  Rebase, don't fork from stale main.
- The running batch's worktrees (`~/Desktop/lost-harness-fixes/wt-P*`) are OFF-LIMITS.
- No pushes/releases without Lukas's review; the staged-release test can run against a draft
  release or a temp fork.
