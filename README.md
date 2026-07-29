# Lost Harness

Lost Harness is a macOS-first desktop workspace for privacy-conscious AI work.
It combines a Tauri 2 / Rust backend with a Svelte 5 frontend, local or remote
model providers, per-profile storage, gated tools, and local-first routing.

> Status: active pre-release development. Treat builds as development builds;
> they are not yet signed or notarized for general distribution.

## What is in the app

- Local and OpenAI-compatible model providers, including LM Studio.
- Per-profile conversations, workspaces, memory, permissions, and model seats.
- Tool calls behind privacy, sandbox, approval, and audit gates.
- Gmail, Calendar, and Tasks through a user-owned Google OAuth client stored in
  the operating-system keychain.
- Live Hugging Face model discovery rather than a stale built-in model catalogue.
- Planner, scheduled jobs, MCP (stdio and Streamable HTTP), and native macOS
  accessibility-backed computer actions.

See [HANDOFF.md](HANDOFF.md) for the current engineering state, and
[docs/ROADMAP.md](docs/ROADMAP.md) for planned work.

## Development setup

Requirements:

- macOS with Xcode Command Line Tools (the primary supported platform).
- Node.js 20 or newer.
- Rust 1.82 or newer via `rustup`.

```sh
git clone https://github.com/hayaitoko/lost-harness.git
cd lost-harness
npm ci
npm run tauri dev
```

Useful checks:

```sh
npm run check
npm run build
(cd src-tauri && cargo test --lib)
```

To create a debug macOS bundle:

```sh
npm run tauri build -- --debug
```

The application keeps its local data under `~/Documents/Lost-Harness/`. Never
commit that data, downloaded model files, OAuth credentials, API keys, or
keychain exports.

## Using a local model

Install LM Studio independently, load a model, start its local server, then add
the OpenAI-compatible provider in Lost Harness. The standard endpoint is
`http://127.0.0.1:1234/v1`.

## Contributing

Please read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.
Security-sensitive issues are covered by [SECURITY.md](SECURITY.md).

## License

Licensed under the MIT License. See [LICENSE](LICENSE) for the full text.

This project vendors third-party binaries under their own licenses; see
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) for details.
