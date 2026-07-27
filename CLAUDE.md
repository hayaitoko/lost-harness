# CLAUDE.md — working in the Lost Harness codebase

This file is auto-loaded into every Claude Code session opened in this repo. It orients a
fresh agent and states the rules that must not be broken. It is deliberately short — the
authoritative detail lives in the docs it points to. When it and a subsystem doc disagree,
the subsystem doc wins; when a doc and the code disagree, **the code wins**.

## What this is (one breath)

Lost Harness is a macOS-first, local-first personal-AI desktop app: a **Rust core**
compiled into a **Tauri 2** shell with a **Svelte 5 / Tailwind 4** frontend. Its defining
feature is a **privacy filter** — every call out to a model is classified and routed (kept
local, sent to the cloud, or blocked) *before* anything can leave the machine.

## Read these first, in this order

1. [`HANDOFF.md`](HANDOFF.md) — current state, what's next, session log. **Always the
   freshest status; start here.**
2. [`docs/ROADMAP.md`](docs/ROADMAP.md) — the stage tracker / status board. Answer "where
   are we?" from this.
3. [`docs/PLAN.md`](docs/PLAN.md) — the design **source of truth**: what the product is,
   why, the build order, the open decisions.
4. [`docs/codebase/README.md`](docs/codebase/README.md) — the code *as it actually is*: a
   subsystem map with `file:line`, the load-bearing invariants, how-to-run/test, toolchain
   gotchas, and a live watch-items list. **Read this before you change code.**
5. [`CONTRIBUTING.md`](CONTRIBUTING.md) — branch/PR/safety conventions.

Do not trust a status line or test count baked into any doc over what you can verify — run
the checks below and read the code.

## Non-negotiable invariants (do NOT weaken these)

The whole product rests on these. `docs/codebase/README.md` carries the authoritative
`file:line` version of each, plus its real scope and limits. In brief:

- **The privacy filter fails closed.** A turn the classifier flags is never silently sent
  to the cloud — `RouteLocal` only proceeds on a provider that is both local *and* private,
  else the call errors rather than falling back. A tool call flagged `LocalRequired` on a
  cloud endpoint is denied outright — a second, independent enforcement point. This covers a
  tool's *own* off-box destination, not just the model endpoint.
- **The sandbox danger floor cannot be disabled.** The hardline denylist runs before any
  ask-capable hook and no setting turns it off. (It's a substring/pattern denylist —
  defense-in-depth *behind* the mandatory per-call approval and the deny-by-default Seatbelt
  jail, not a semantic command-safety guarantee.)
- **Parse only the model's own current-turn output for tool calls, and guard-wrap all
  untrusted output.** Content the agent merely *read* can never *forge* a call. Treat
  external text, tool output, MCP tool descriptions, and model output as untrusted input.
- **"Asked" is not "approved."** An unattended agent cannot self-grant a gated tool by
  attempting it; only a recorded approval flips a call through.
- **Workspace confinement.** The filesystem tools cannot escape `workspace/` — not via
  `..`, absolute paths, or symlinks.
- **`--no-default-features` must build.** The trained ONNX classifier and memory embedder
  sit behind a default-on `onnx-classifier` feature; the rules-only build has no native ML
  dependency and is what CI and the base build use. Keep it green — never make the native
  models mandatory to compile.

New tools need tests for their capability, risk class, routing, and approval behavior.

## How to run, test, build

Run `cargo` from `src-tauri/` (the Rust project lives there). The shell resets cwd between
commands — use one compound command or absolute paths.

```bash
cd src-tauri && cargo test --lib                          # Rust unit/contract tests
cd src-tauri && cargo clippy --all-targets -- -D clippy::correctness
cd src-tauri && cargo build --lib --no-default-features   # rules-only build MUST pass
npm run check && npm run build                            # svelte-check + frontend build
npm run tauri dev                                         # full app (native window)
npm run tauri build -- --debug                            # debug .app / .dmg bundle
```

## Gotchas that will cost you time

- **Tauri v2 struct args nest under `args`.** A command `fn cmd(state, args: T)` is called
  from JS as `invoke("cmd", { args: { ...snake_case } })` — nested, snake_case struct
  fields. Flat or camelCase compiles and passes the browser mock but **fails in the real
  shell**. `src-tauri/src/ipc/contract_tests.rs` is the regression lock.
- **The window loads `app.html`, not `/`.** Vite root is `src/`, entry is `src/app.html`;
  loading `/` 404s to a blank white window.
- **An x86_64 Rust toolchain alongside arm64 Node is fine.** On the primary dev box `cargo`
  runs under Rosetta while Node is arm64; builds/tests work via translation — the arch
  mismatch is expected, not a bug.
- **The ML models are not in git.** The classifier (bge-small + distilbert INT8, ~98 MB)
  and the memory meaning-lane embedder (bge-small-en-v1.5 INT8, ~34 MB) install out-of-band
  under `<storage>/models/{classifier,embedder}/`. Without them the app falls back to
  rules-only classification and FTS-only memory search — both intended, not errors. If you
  touch the ONNX path: the exported `tokenizer.json` bakes in `Fixed(128)` padding — you
  **must** disable padding/truncation on load or the model silently scores garbage. See
  [`docs/codebase/classifier.md`](docs/codebase/classifier.md).
- **Local data lives under `~/Documents/Lost-Harness/`** — per-profile `profiles/<name>.db`
  plus a shared `global.db`. Never commit it, downloaded models, OAuth client secrets, API
  keys, or keychain material. `.gitignore` covers the usual cases; don't defeat it.

## Terminology

Call it **"the privacy filter"** in prose and UI. The *code* still uses `PrivacyGate`,
`§7`, and `trm/` names — a rename is pending. Expect the mismatch; don't do a drive-by
rename in the middle of a feature.

## Live QA of the running app

The running app can be driven for visual QA with the `cua-driver` skill. Practical notes
that cost time otherwise: the WKWebView accessibility tree populates only on the *first*
snapshot, then collapses to the menubar — take the coordinates you need from that first
tree, then click off fresh screenshots. Background clicks land, but background *typing* is
dropped (type in the foreground). Sending with no model selected yields an inline "unknown
provider id" bubble and persists no message row.

## Working style

Keep changes small and reviewable; don't mix refactors with behavior or security changes.
Keep user-visible errors actionable — don't collapse provider or transport errors into
opaque identifiers. Update a subsystem's doc when you change it materially (a wrong doc is
worse than none). Commit or push only when asked. See [`CONTRIBUTING.md`](CONTRIBUTING.md)
for the full conventions.
