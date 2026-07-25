# Contributing to Lost Harness

## Before starting

1. Sync with `main` and create a focused branch, for example
   `fix/lm-studio-tools` or `feat/planner-polish`.
2. Read [HANDOFF.md](HANDOFF.md), [docs/ROADMAP.md](docs/ROADMAP.md), and the
   relevant subsystem guide in [docs/codebase](docs/codebase/README.md).
3. Keep changes small enough to review. Do not mix refactors with behavior or
   security changes unless they are inseparable.

## Local checks

Run the checks relevant to your change before requesting review:

```sh
npm run check
npm run build
(cd src-tauri && cargo test --lib)
```

For macOS-specific work, also build a debug bundle:

```sh
npm run tauri build -- --debug
```

## Safety rules

- Never commit API keys, OAuth client secrets, refresh tokens, keychain exports,
  app databases, downloaded models, or generated application bundles.
- Preserve the privacy, sandbox, approval, and audit gates around tool actions.
  New tools need tests for their capability, risk class, routing, and approval
  behavior.
- Keep user-visible errors actionable. Do not collapse provider or transport
  errors into opaque identifiers.
- Treat external text, tool output, MCP descriptions, and model output as
  untrusted input.

## Pull requests

Use an imperative, scoped commit message such as `fix(models): normalize native
tool schemas for LM Studio`. In the PR description, explain the user-visible
change, list validation performed, and call out deferred/manual checks.

Do not force-push another contributor's branch. Prefer reviewable follow-up
commits and rebase only your own work before merge.
