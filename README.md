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

## Support matrix

| Component | Constraint |
|-----------|-----------|
| **Operating system** | macOS only. There is no Linux or Windows build, and CI runs on macOS alone. |
| **Architecture** | Apple Silicon (arm64) only. No x86_64 runtime files are shipped — the vendored `llama-server` binary and its dylibs are arm64-only Mach-O. |
| **Minimum macOS** | **macOS 26 (Tahoe)** or later. <!-- MIN_MACOS=26.0 --> |
| **Xcode** | Xcode Command Line Tools, or Xcode.app. |
| **Node.js** | 20 or newer. |
| **Rust** | 1.82 or newer via `rustup`. |

### Why the minimum is macOS 26

The floor is set by the vendored llama.cpp runtime, not by the app's own code.
Every Mach-O file in `src-tauri/vendor/llama-cpp/macos-arm64/` (the
`llama-server` executable and all ten dylibs, llama.cpp build `b10088`) declares
a macOS 26.0 deployment target:

```sh
otool -l src-tauri/vendor/llama-cpp/macos-arm64/llama-server | grep -A3 LC_BUILD_VERSION
#      cmd LC_BUILD_VERSION
#  platform 1
#     minos 26.0
#       sdk 26.5
```

That is what the binaries promise the loader, so it is what this README
promises users. Lowering it is a real build task — llama.cpp has to be
recompiled with an older `CMAKE_OSX_DEPLOYMENT_TARGET` and re-vendored — not a
documentation change. Until that happens, macOS 26 is the honest minimum, even
though nothing else in the app is known to require it.

The two numbers are kept in sync by CI: the `build` workflow runs `otool -l`
and `lipo -archs` over every vendored binary and fails if the declared `minos`
drifts from the `MIN_MACOS` marker in the table above, or if any file is not
arm64-only. Re-vendoring llama.cpp therefore forces a matching README edit.

## Development setup

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

## Releases and updating

The app can update itself from this repository's public GitHub releases. On
launch it makes at most one anonymous request — for a version manifest,
carrying no conversations, files or account — and only if the **Settings →
About** toggle is on. Nothing downloads or installs without a click, and a
payload is refused unless it is signed by the project's key and comes from this
repository's own release assets.

Cutting a release is a `vX.Y.Z` tag: CI builds, signs, verifies the signature
against the public key the app ships, and opens a **draft** release for a human
to publish.

See [docs/releasing.md](docs/releasing.md) for the full release runbook, the
signing-key location and rotation procedure, the updater's egress behaviour, and
an explicit account of which parts of the update loop have been verified and
which have not.

## Contributing

Please read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.
Security-sensitive issues are covered by [SECURITY.md](SECURITY.md).

## License

Licensed under the MIT License. See [LICENSE](LICENSE) for the full text.

This project vendors third-party binaries under their own licenses; see
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) for details.
