# External review prompt (for ChatGPT / another model)

Paste the block below into a fresh chat. Because the reviewer can't clone the
repo, attach the files listed under "Attach these" so the review is grounded in
real code, not just this description. If you can't attach files, the reviewer
should mark every code-level claim as an assumption to verify.

---

You are a skeptical senior staff engineer doing a pre-release architecture and
security review of a desktop application. Be adversarial and concrete. Do not
compliment the design; your job is to find what will break, leak, or rot. When
you assert a flaw, state the exact failure scenario (inputs → wrong outcome). If
you can't ground a claim in the attached code, label it an assumption and say
what you'd need to confirm it.

## What the product is

"Lost Harness" is a privacy-first personal AI assistant. Its defining promise:
the user's data is routed to the safest place that can do the job. A local
on-device model handles anything sensitive; cloud models are used only when a
message is classified as safe to send. It is an *agentic* app — the model can
call tools (filesystem, shell, web fetch, delegate to sub-agents, cron,
computer-use/UI automation, MCP servers) under a permission system.

Stack: Tauri 2 (Rust core + a WebView UI), Svelte 5 + TypeScript frontend,
SQLite (rusqlite) for storage, an ONNX privacy classifier (rules + a trained
transformer ensemble) run in-process, and a bundled llama.cpp sidecar for local
inference. macOS-first (Seatbelt sandboxing for shell); Windows/Linux backends
are planned, not built.

## The security invariants the design claims to hold (challenge each one)

1. **Privacy filter is load-bearing.** Every user turn is classified before any
   cloud egress; a message classified private/uncertain is never sent to a cloud
   model. Prior turns are re-checked so a later "safe" turn can't replay earlier
   sensitive history to the cloud.
2. **Local-first / refuse-don't-degrade.** If nothing safe can run a task, the
   app refuses rather than silently downgrading privacy. A screenshot/image
   forces local routing regardless of the message's own classification.
3. **Danger floor is non-overridable.** Certain dangerous actions always require
   an explicit human "allow this once" and can never be pre-authorized, even in
   headless/unattended mode.
4. **Untrusted content is guard-wrapped.** Tool outputs, web pages, and
   delegated-helper results are framed as untrusted data; the model is instructed
   (and type-enforced) to parse only its own current-turn output as commands.
5. **Profile isolation ("the wall").** Each profile has a physically separate
   workspace subtree and its own SQLite DB; a "work" profile's files and memory
   never mix with "personal".
6. **Honest-Unknown.** When cost, hardware fit, or model speed can't be known,
   the app shows "unknown" rather than fabricating a number.
7. **Least-privilege sub-agents.** A delegated helper gets an intersected
   toolbelt, runs headless (can't earn a standing permission grant), and inherits
   the parent turn's privacy binding.

## Where I most want your scrutiny

- **Egress paths.** Is there ANY path where private content reaches a cloud
  endpoint? Consider: multi-turn history replay, tool arguments (a tool that
  fetches a URL built from private text), memory injection into a later cloud
  turn, sub-agent delegation, error/telemetry strings, the model-search feature
  hitting HuggingFace, MCP servers.
- **Prompt injection → tool misuse** (OWASP Agentic ASI01/ASI02). A malicious
  web page or file tells the agent to exfiltrate or run a destructive command.
  Does guard-wrapping + the permission gate actually stop it, or only slow it?
- **The "non-overridable" danger floor.** It reportedly matches dangerous
  command *substrings* in shell text (e.g. `curl`, `| sh`). How would you defeat
  that with obfuscation (quoting, `$IFS`, alternate interpreters, base64)? What's
  the blast radius given Seatbelt still confines the process?
- **Concurrency.** Heavy synchronous work (ONNX inference, all SQLite) runs on
  the async runtime's worker threads with almost no `spawn_blocking`, one
  Mutex-wrapped DB connection per profile, and background cron/sub-agent tasks
  that don't take the app's "one stream at a time" lock. Where does this stall or
  deadlock under load? What's the worst realistic contention scenario?
- **Data lifecycle.** Several tables (audit log, usage ledger, work queue) grow
  unbounded; a 7-day log-retention function exists but isn't wired up. What are
  the privacy and disk consequences of never pruning?
- **Ship-readiness.** CSP is disabled in the WebView; the bundle config
  references icon files that don't exist; `Cargo.lock` isn't committed; CI is
  softer than the local gates (clippy failures don't fail the build; the
  no-classifier fallback build and the frontend type-check never run; no
  dependency vulnerability scan). Which of these is a real risk vs. cosmetic?
- **Supply chain** (OWASP Agentic ASI04). The local-model download verifies a
  SHA-256 fetched from HuggingFace's own API at download time — same host, same
  moment as the bytes. What can and can't that catch? MCP servers and skills are
  user/third-party supplied — what's the trust model?

## What to produce

1. A ranked list of the most serious findings. For each: the invariant or
   component at risk, the concrete failure scenario, severity, and a one-line
   fix direction.
2. Any invariant above you believe is *not actually enforceable* as described,
   with your reasoning.
3. A short "what I'd test first if I were red-teaming this" list.
4. A one-paragraph overall verdict: is this a sound architecture with normal
   pre-release gaps, or are there structural problems? Benchmark it against the
   OWASP Top 10 for LLM Apps (2025) and the OWASP Top 10 for Agentic
   Applications (2026), and against how LM Studio / Ollama / Jan approach local
   model management.

Assume the authors are competent and the security spine was built deliberately —
so spend your effort on the seams between subsystems, the paths that were added
after the original design, and the gap between what the code claims and what it
enforces.

## Attach these (ground truth — pick from what you can share)

- `docs/PLAN.md` — the design source of truth (what the product is + architecture)
- `docs/codebase/README.md` + the `docs/codebase/*.md` subsystem docs — the
  code-as-it-is map with file:line references
- `HANDOFF.md` — current state, the "DISCOVERED-BUT-DEFERRED" ledger, gotchas
- Backend hot spots: `src-tauri/src/agent/loop_mod.rs`, `agent/gate.rs`,
  `hooks/` (sandbox.rs, privacy_filter.rs, routing.rs, permission.rs, headless.rs),
  `tools/` (dispatch.rs, exec.rs, fetch.rs, delegate.rs, calling.rs),
  `classifier/engine.rs`, `storage/` (global.rs, profile.rs, migrations.rs)
- Frontend: `src/lib/api/tauri.ts` (the IPC surface), `src/lib/stores/*`,
  `src/lib/design/screens/MainScreen.svelte` + `Settings.svelte`
- Config: `src-tauri/tauri.conf.json`, `src-tauri/capabilities/*`,
  `src-tauri/Cargo.toml`, `.github/workflows/*`
