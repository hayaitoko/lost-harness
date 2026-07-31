# Releasing and self-update

How a Lost Harness release is cut, how the app updates itself, where the signing
key lives, and — at the bottom — an honest account of which parts of this have
actually been observed working and which have not.

Round-2 item 3 added all of this: a Settings → About pane, an update banner, the
repo's first two Tauri plugins (`tauri-plugin-updater`, `tauri-plugin-process`),
and a tag-triggered release job. If you are about to touch any of it, read this
first, then `src-tauri/src/updater/mod.rs` — the module doc there explains why
the whole thing is driven from Rust rather than from the plugin's JS API.

---

## 1. What the app does, and what leaves the machine

### The launch check

On launch the app makes **at most one** update check — and none at all if the
toggle is off. The check is an anonymous `GET` for `latest.json` at

```
https://github.com/hayaitoko/lost-harness/releases/latest/download/latest.json
```

**That is two HTTP requests, to two hosts.** `latest.json` is a *release asset*,
and GitHub answers release-asset requests with a `302` to its own object CDN
(`objects.githubusercontent.com`, sometimes `release-assets.githubusercontent.com`);
the client follows the redirect. Anywhere the egress is described to a user —
README, Settings → About — it has to be described that way. The endpoint is left
as it is rather than moved somewhere redirect-free: GitHub's REST API returns its
own JSON shape rather than a Tauri manifest, and hosting the manifest off GitHub
would add a second place a release could be tampered with. Naming the redirect is
the honest fix.

Neither request carries conversations, files, an account, or identifiers of ours
— just the request. What comes back is a version string, a download URL, a
signature and (optionally) release notes.

Two gates stand in front of that request, both in
`updater::run_launch_check`:

| Gate | Behaviour |
|---|---|
| **Dev build** (`tauri::is_dev()`) | never checks. A dev build's version is whatever is in `Cargo.toml` and its bundle is not a signed `.app`. |
| **The Settings toggle** | off ⇒ never checks. |

The function takes the network call as a closure, and that closure is the only
thing in the launch path that can touch a socket — so "the toggle is off ⇒ zero
egress" is a structural property, and `updater::tests::toggle_off_makes_no_request`
proves it by asserting a counter stayed at zero.

### The toggle

Stored in the **global** SQLite database (`app_settings.update_check_enabled`),
not in the frontend's localStorage, because the reader is Rust and runs before
any webview exists. Defaults to **on**.

Three cases, and the third is the one worth knowing:

- a row reading exactly `"0"` → off;
- **no row at all** → on (the fresh-install default);
- **the read failed** → **off**.

That last one is deliberate. An unreadable setting is not consent, and this read
happens during startup, which is exactly when a transient lock failure is
plausible. Swallowing the error into the absent-row default would have made a
failed read indistinguishable from "the user never chose" and sent the request
anyway. `storage::tests::a_failed_read_of_the_update_toggle_reads_as_off` pins it.

Turning the toggle off does not disable the **Check now** button in Settings →
About. Clicking that button is its own consent, and it is the only way to check
while the launch check is off.

### Nothing installs by itself

`check` and `install` are separate commands, and only a click calls the second.
The launch check's success path emits `update:available` to the webview, which
renders a dismissable banner. Downloading, installing and relaunching are three
further explicit clicks. There is no silent-install path in the module, by
construction.

### The three constraints on an install

1. **Signature.** The payload is verified with minisign against the public key
   compiled into the build (`plugins.updater.pubkey` in `tauri.conf.json`)
   *before* anything is unpacked. This happens inside `Update::download`; a
   payload whose bytes do not match is refused there and surfaces in the banner
   as a plain error.
2. **Version.** `updater::is_strictly_newer` is the app's own second gate on top
   of the plugin's comparison. A manifest that announces the running version, an
   older one, or an unparseable one never produces a banner.
3. **Download host.** `latest.json` is remote input, and `platforms.<target>.url`
   inside it can say anything. `updater::is_permitted_download_url` requires
   `https`, no userinfo, host exactly `github.com`, the default port, and a path
   under `/hayaitoko/lost-harness/releases/download/` whose every remaining
   segment is a plain asset name *once percent-decoded*. Checked in `check_now`
   before an offer is ever staged, and again in `install_update` as the last
   statement before bytes move.

   The decode matters, and the reason is counter-intuitive. `Url::parse` already
   collapses dot segments in **every** spelling the URL standard recognises,
   percent-encoded ones included (`..`, `%2e%2e`, `.%2e`, `%2E%2E`), so a URL
   that climbs out of the prefix has already lost the prefix by the time it is
   checked. What parsing leaves alone is an escape that is not a whole segment:
   `…/releases/download/%2f..%2f..%2fother/x.tar.gz` keeps the prefix verbatim
   and decodes to a climb. `updater::tests::an_escape_that_survives_parsing_cannot_smuggle_a_climb_past_the_prefix`
   is the test; malformed and double-encoded escapes are refused rather than
   guessed at.

   The signature already protected *integrity* without this; what it adds is
   that the stated egress is the actual egress — that a swapped manifest cannot
   redirect the download to a host of its choosing.

   The one hop the app does **not** choose: GitHub answers a release-asset
   request with a redirect to its own object CDN
   (`objects.githubusercontent.com`), and the HTTP client follows it. That hop is
   GitHub's own, and it applies to the manifest check as much as to the payload
   — see the top of this document.

### The webview cannot reach the network

`capabilities/default.json` grants **no** `updater:*` permission, so
`plugin:updater|check` is not invocable from Svelte. Every path to the network
goes through `updater::check_now`, which logs the egress. `process:allow-restart`
*is* granted — that backs the relaunch button, which is not egress.

---

## 2. Cutting a release

`src-tauri/tauri.conf.json`'s `version` is the source of truth. The tag, that
file, and `Cargo.toml` must all agree; CI checks this before it builds anything.

```sh
# 1. Bump BOTH files to the new version.
#    src-tauri/tauri.conf.json  ->  "version": "0.1.1"
#    src-tauri/Cargo.toml       ->  version = "0.1.1"
#    (get_app_version reads Cargo.toml; the manifest reads tauri.conf.json.)

# 2. Commit on main.
git commit -am "release: v0.1.1"

# 3. Tag and push the tag. The tag is what triggers the release job.
git tag v0.1.1
git push origin main
git push origin v0.1.1
```

Pushing the tag runs `.github/workflows/build.yml`. The `release` job is
`needs: build`, so a tag can never publish a bundle whose tests, clippy
(correctness), `cargo fmt`, `cargo audit` or `npm audit` gates failed.

The release job then, in order:

| Step | What it refuses to let through |
|---|---|
| Assert macOS arm64 runner | an Intel image silently producing an x86_64 build |
| Tag matches `tauri.conf.json` version | a release the app either ignores forever or re-offers on every launch |
| Signing secrets are present | spending a whole build to produce an unsigned payload |
| Build and sign the arm64 bundle | — (`createUpdaterArtifacts: true` emits `.app.tar.gz` + `.sig`) |
| Stage the updater artifacts | a missing `.app.tar.gz` or a missing `.sig`; composes `latest.json` |
| **Signature verifies against the pubkey this build ships** | a signing key that has drifted from `plugins.updater.pubkey` — see below |
| **Manifest download URL is one the app will accept** | a manifest the shipped app would refuse to download from |
| Publish the draft release | — (creates a **draft**) |

The last step creates a **draft**, and that is deliberate: the moment a release
goes live, every installed copy starts offering it. Review the draft, then
publish by hand. The app only ever sees published releases.

### Why the signature-verification step exists

Every step before it proved a signature was *produced*. None of them proved it
verifies against the public key **compiled into the app**, and those are
different facts: the private key is a repository secret, the public key is a
string in a config file, and nothing keeps the two in step. Rotate the key,
paste it into the wrong secret, or edit the config without re-signing, and the
build stays green while every installed copy of the app refuses the release —
silently, forever. That is the worst failure mode this feature has, precisely
because from CI it looks like success.

The step decodes `plugins.updater.pubkey` (base64 of a minisign `.pub` *file*)
and the emitted `.sig` (base64 of a minisign `.sig` *file*) back into the formats
`minisign(1)` reads, and verifies with a tool independent of the one that signed.
It logs the key id, so a mismatch reads as a key-id diff rather than as a
mystery.

---

## 3. The signing key

| Thing | Where |
|---|---|
| Private key | `~/.tauri/lost-harness-updater.key` on Lukas's machine. **Never** in the repository, never in a log, never printed. Password-protected. |
| Public key | `~/.tauri/lost-harness-updater.key.pub`, and — byte for byte the same string — `plugins.updater.pubkey` in `src-tauri/tauri.conf.json`. Public by definition; safe to read, copy and paste. |
| CI secrets | `TAURI_SIGNING_PRIVATE_KEY` (the contents of the private key file) and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, both repository secrets. Used by exactly one workflow step; never written to the runner's disk. |

Key id of the current key: `61C357E511926DEF`.

The test fixtures under `src-tauri/tests/fixtures/updater/` are signed by a
**different, throwaway** key (id `3CB1DD63463FB192`) that was generated in a
scratch directory, used once, and never committed. Its public half *is*
committed, so the fixture tests are reproducible.

### Rotating the key

Read this whole subsection before starting. Rotation is not a routine
maintenance action.

**Rotating strands the installed base.** The public key is compiled into each
build. An app built with the old key will refuse every release signed with the
new one — correctly, that is the entire point of the mechanism. There is no
in-app migration path: existing installs have to be replaced by hand with a
build that carries the new key. Rotate only if the private key is compromised or
lost, and expect to tell every user to reinstall.

```sh
# 1. Generate a new key pair. -w writes it; keep the password somewhere real.
npx tauri signer generate -w ~/.tauri/lost-harness-updater.key

# 2. Put the PUBLIC half into the config, verbatim — the whole base64 blob,
#    exactly as the .pub file contains it, no re-wrapping.
cat ~/.tauri/lost-harness-updater.key.pub
#    -> src-tauri/tauri.conf.json  ->  plugins.updater.pubkey

# 3. Update BOTH repository secrets, together:
#      TAURI_SIGNING_PRIVATE_KEY           = contents of ~/.tauri/lost-harness-updater.key
#      TAURI_SIGNING_PRIVATE_KEY_PASSWORD  = the password chosen in step 1
#    Settings -> Secrets and variables -> Actions.

# 4. Commit the config change, then cut a release as in section 2. The
#    "Signature verifies against the pubkey this build ships" step is what
#    catches a half-done rotation — if it fails with a key-id diff, step 2 and
#    step 3 disagree.

# 5. Distribute the new build manually. Every existing install is now on the old
#    key and will refuse the new releases.
```

If the key is *lost* rather than compromised, the situation is the same minus
step 1's urgency: there is no way to sign a release the installed base will
accept, so the installed base has to be replaced regardless.

---

## 4. What is proven, and what is not

Stated plainly, because the spec's headline acceptance criterion has **not**
been executed.

### Proven — observed, by tests that run in CI

| Property | Where |
|---|---|
| The toggle off ⇒ **zero** update egress (the fetch closure is never called) | `updater::tests::toggle_off_makes_no_request` |
| A dev build never checks, toggle or no toggle | `updater::tests::dev_build_makes_no_request_even_with_the_toggle_on` |
| A **tampered** payload is refused — one flipped byte, genuine manifest and signature | `updater::signature_tests::a_tampered_payload_is_refused` |
| An **unsigned** payload is refused | `updater::signature_tests::an_unsigned_update_is_refused` |
| A payload signed by the **wrong key** is refused (verified against the app's real configured pubkey) | `updater::signature_tests::a_payload_signed_by_the_wrong_key_is_refused` |
| An **empty** pubkey does not mean "skip verification" | `updater::signature_tests::a_correctly_signed_update_is_accepted` |
| A correctly signed payload verifies, and `download` returns the bytes untouched | `updater::signature_tests::a_correctly_signed_update_is_accepted` |
| The manifest shape the release job emits is one the updater parses, resolves and verifies | `updater::signature_tests::the_release_workflows_manifest_shape_is_one_the_updater_accepts` |
| A manifest announcing the running or an older version produces no banner | `updater::tests::only_a_strictly_newer_version_is_an_update` and `..::a_manifest_announcing_the_running_version_is_not_an_update` |
| A failed read of the toggle cannot authorize egress | `storage::tests::a_failed_read_of_the_update_toggle_reads_as_off` |
| Off-host, non-https, wrong-repo and path-climbing download URLs are refused | `updater::tests::another_host_is_refused_however_plausible` and neighbours |
| The pinned host constants and the configured manifest endpoint agree | `updater::tests::constrained_host_matches_the_configured_manifest_endpoint` |
| The frontend offers an update without installing it, and never calls the plugin directly | `src/tests/updater.test.ts` |

Those signature tests are not mocks: they stand up a loopback HTTP server, serve
a real manifest and a real signed payload, and drive the **real**
`tauri-plugin-updater` `check()` → `download()` path. The refusals come from the
plugin, not from a re-implementation of its rules.

### Not proven

1. **The end-to-end acceptance criterion.** "A staged v0.1.0 → v0.1.1 release is
   detected, downloaded, installed and relaunched on the real app" has **never
   been executed**. It cannot be without publishing a GitHub release, which was
   out of scope. Nothing below the "verified bytes in hand" line — unpacking,
   replacing the running `.app`, relaunching into the new version — has been
   observed working. Do not read the green test suite as an end-to-end pass.

2. **A real bundler-produced artifact.** `updater::signature_tests::a_real_bundler_produced_artifact_verifies`
   exists and is `#[ignore]`d, because it needs a local `tauri build` (minutes,
   and ~28 MB of output that has no business in git). The committed fixture is a
   hand-made tarball, so "the *bundler's* own output is what the updater
   accepts" is asserted by construction rather than observed.

3. **Gatekeeper.** The `.app` is **not** Apple-codesigned and **not** notarized —
   the workflow says so itself. The minisign signature the updater verifies is a
   different thing entirely: it proves the payload came from the project's key,
   not that macOS will run a freshly downloaded bundle. Whether an install
   actually *succeeds* on a real machine — as opposed to being quarantined — is
   unknown, and is a plausible reason for the end-to-end run to fail when
   somebody finally does it.

4. **The new CI steps in a real workflow run.** Both step bodies were extracted
   verbatim from `build.yml` and executed locally against the committed fixtures:
   a matching key passes, a mismatched key fails with a key-id diff, a tampered
   artifact fails, and good/off-host/wrong-repo manifest URLs pass/fail/fail.
   They have not run on a GitHub runner, because that needs a tag push.

### Exactly what a human would run to prove the rest

**(a) The real bundler artifact — no publishing, ~10 minutes.**

```sh
# A throwaway signing key. NEVER the one in ~/.tauri.
npx tauri signer generate --ci -p "" -w /tmp/lh-test.key -f

# A real bundle, signed with it.
TAURI_SIGNING_PRIVATE_KEY="$(cat /tmp/lh-test.key)" \
TAURI_SIGNING_PRIVATE_KEY_PASSWORD="" \
  npm run tauri build -- --bundles app

# Point the ignored test at the bundle and the key's public half.
cd src-tauri
LH_REAL_BUNDLE_DIR="target/release/bundle/macos" \
LH_REAL_BUNDLE_PUBKEY="$(cat /tmp/lh-test.key.pub)" \
  cargo test --lib updater::signature_tests -- --ignored --nocapture
```

**(b) The full detect → download → install → relaunch loop.** This is the one
that has to be done for real.

```sh
# 1. Build and install v0.1.0 the way a user would have it.
npm run tauri build -- --target aarch64-apple-darwin
cp -R "src-tauri/target/aarch64-apple-darwin/release/bundle/macos/Lost Harness.app" /Applications/
open "/Applications/Lost Harness.app"     # confirm Settings -> About shows 0.1.0

# 2. Bump to 0.1.1 in tauri.conf.json AND Cargo.toml, with some visible marker
#    change so "did it actually relaunch into the new build?" is answerable.
#    Commit, tag v0.1.1, push the tag (section 2).

# 3. Wait for the release job. Confirm in its log:
#      - "Signature verifies against the pubkey this build ships" passed
#      - "Manifest download URL is one the app will accept" passed
#    Then PUBLISH the draft release (the app only sees published ones).

# 4. Launch the installed v0.1.0 with the toggle ON. Expect: the banner appears
#    within a few seconds of launch, and no dialog blocks the window.
#    Click Install -> expect "Version 0.1.1 is installed".
#    Click Relaunch -> expect the app to restart and About to read 0.1.1, with
#    the marker change from step 2 visible.

# 5. Then check the negative half, which is the part people skip. The app logs
#    to stderr (no log file), so run the installed binary directly to see it:
ls "/Applications/Lost Harness.app/Contents/MacOS/"      # find the executable name
"/Applications/Lost Harness.app/Contents/MacOS/<exe>" 2>&1 | grep -i 'egress\|update'
#      - toggle ON  -> expect a line: "egress: requesting the update manifest"
#      - toggle OFF -> relaunch; that line must NOT appear. Its absence is the
#        result. (`skipping the launch update check (no request made)` is
#        visible at RUST_LOG=debug if you want the positive confirmation too.)
#      - `npm run tauri dev` -> the same skip, with reason `dev_build`.
```

**(c) Gatekeeper.** After step 4 installs, before relaunching:

```sh
xattr -p com.apple.quarantine "/Applications/Lost Harness.app" 2>&1   # expect: no such xattr
codesign -dv --verbose=4 "/Applications/Lost Harness.app" 2>&1        # expect: adhoc / not Developer ID
spctl -a -vvv "/Applications/Lost Harness.app" 2>&1                   # expect: rejected (unnotarized)
```

A rejection there is the *known* state, not a regression — it is item 3 under
"Not proven". What matters is whether the relaunched app runs anyway.

**Rehearsing (b) without publishing is not straightforward, and that is a
deliberate trade.** Pointing the endpoint at a local file server no longer works
on its own, because `updater::is_permitted_download_url` refuses a
`127.0.0.1` download. A local rehearsal therefore needs a throwaway build with
*both* `plugins.updater.endpoints` and `RELEASE_HOST` /
`RELEASE_DOWNLOAD_PREFIX` temporarily repointed — and that build is not the
shipping artifact, so it proves the mechanics but not the shipped
configuration. Publishing a real draft release is the only way to test what
actually ships.

---

## 5. Where the code is

| File | What is in it |
|---|---|
| `src-tauri/src/updater/mod.rs` | the launch gate, version comparison, the download-host constraint, `check_now` (the entire update network surface), the pending-update slot |
| `src-tauri/src/updater/tests.rs` | the gate, the version rules, the host constraint |
| `src-tauri/src/updater/signature_tests.rs` | the real plugin driven against a loopback server; the tamper/unsigned/wrong-key refusals |
| `src-tauri/src/ipc/mod.rs` | the four commands: `get_/set_update_check_enabled`, `check_for_update`, `install_update` |
| `src-tauri/src/storage/global.rs` | `update_check_enabled` / `set_update_check_enabled` |
| `src-tauri/src/lib.rs` | plugin registration and the spawned launch check |
| `src-tauri/capabilities/default.json` | `process:allow-restart`, and the deliberate absence of `updater:*` |
| `src-tauri/tauri.conf.json` | `plugins.updater.pubkey` + `endpoints`, `bundle.createUpdaterArtifacts` |
| `src/lib/design/components/UpdateBanner.svelte` | the offer → installing → ready → failed strip |
| `src/lib/design/screens/Settings.svelte` | the About pane |
| `src/lib/stores/update.ts` | the single "an update was found" slot |
| `.github/workflows/build.yml` | the `release` job |
