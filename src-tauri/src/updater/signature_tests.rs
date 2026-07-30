//! The half of the spec's acceptance criteria that can be proven WITHOUT
//! publishing anything: **a tampered or wrongly-signed artifact is refused.**
//!
//! These tests stand up a throwaway HTTP server on 127.0.0.1, serve a real
//! `latest.json` and a real signed payload, and drive the REAL
//! `tauri-plugin-updater` — the same `check()` → `download()` path the shipped
//! app uses, with the same minisign verification. Nothing here re-implements
//! the plugin's rules or asserts on a mock: the refusal comes from the plugin
//! itself.
//!
//! ## The fixtures
//!
//! `src-tauri/tests/fixtures/updater/` holds a tiny `Lost Harness.app.tar.gz`,
//! its `.sig`, and the **test-only** public key that signed it. The matching
//! private key was generated in a scratch directory, used once, and never
//! written into the repository. The production key at
//! `~/.tauri/lost-harness-updater.key` was not touched: the "wrong key" case
//! below uses the production *public* key (which is public by definition) as a
//! second, definitely-different verifier.
//!
//! ## What these do NOT prove
//!
//! `install()` is not exercised — it would replace the running `.app` bundle,
//! which a test binary has no business doing. `download()` is the step that
//! verifies the signature (`updater.rs:712`), so the refusal path is fully
//! covered; the accept path is covered up to "verified bytes in hand".

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;

use tauri::test::{mock_builder, mock_context, noop_assets, MockRuntime};
use tauri::App;
use tauri_plugin_updater::UpdaterExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The `{{target}}` key used in the served manifests. Pinned explicitly (rather
/// than letting the plugin derive `darwin-aarch64`) so these tests assert on
/// signature behaviour, not on the runner's architecture.
const TEST_TARGET: &str = "test-target";

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("updater")
}

fn payload_bytes() -> Vec<u8> {
    std::fs::read(fixture_dir().join("Lost Harness.app.tar.gz")).expect("fixture payload")
}

/// The `.sig` file's contents — exactly what goes in a manifest's `signature`.
fn payload_signature() -> String {
    std::fs::read_to_string(fixture_dir().join("Lost Harness.app.tar.gz.sig"))
        .expect("fixture signature")
        .trim()
        .to_string()
}

/// The throwaway public key that actually signed the fixture.
fn test_pubkey() -> String {
    std::fs::read_to_string(fixture_dir().join("test-signing-key.pub"))
        .expect("fixture pubkey")
        .trim()
        .to_string()
}

/// The app's REAL public key, read from the shipped config rather than from
/// `~/.tauri` — so this doubles as a check that the config carries a pubkey
/// minisign can actually parse.
fn production_pubkey() -> String {
    let conf: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json"))
            .expect("tauri.conf.json"),
    )
    .expect("tauri.conf.json parses");
    conf["plugins"]["updater"]["pubkey"]
        .as_str()
        .expect("plugins.updater.pubkey is configured")
        .to_string()
}

// ── A one-shot static file server ───────────────────────────────────────────

/// A loopback static-file server. Bound first (so the manifest can name the
/// real port), then handed its routes. Deliberately hand-rolled: the repo has
/// no HTTP-server dependency and this needs about forty lines.
struct LocalServer {
    addr: SocketAddr,
    listener: Option<TcpListener>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl LocalServer {
    async fn bind() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        Self {
            addr,
            listener: Some(listener),
            task: None,
        }
    }

    fn serve(&mut self, routes: HashMap<String, Vec<u8>>) {
        let listener = self.listener.take().expect("serve called twice");
        self.task = Some(tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let Ok(n) = socket.read(&mut buf).await else {
                        return;
                    };
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = head
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();

                    let response = match routes.get(&path) {
                        Some(body) => {
                            let mut out = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            )
                            .into_bytes();
                            out.extend_from_slice(body);
                            out
                        }
                        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                    };
                    let _ = socket.write_all(&response).await;
                    let _ = socket.flush().await;
                });
            }
        }));
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

// ── The app under test ──────────────────────────────────────────────────────

/// A mock app with the REAL updater plugin registered and `pubkey` set to
/// whatever the caller wants to verify against. `mock_context`'s package
/// version is `0.1.0`, which is what makes an announced `0.1.1` an update.
fn app_with_pubkey(pubkey: &str) -> App<MockRuntime> {
    let mut ctx = mock_context(noop_assets());
    ctx.config_mut().plugins.0.insert(
        "updater".to_string(),
        serde_json::json!({ "pubkey": pubkey, "endpoints": [] }),
    );
    mock_builder()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .build(ctx)
        .expect("mock app with the updater plugin")
}

/// A static-format `latest.json`, in **exactly the shape the release job in
/// `.github/workflows/build.yml` emits** — same keys, same nesting, same
/// `platforms` map. `target` is the platform key; the workflow writes
/// `darwin-aarch64`.
fn manifest(target: &str, version: &str, url: &str, signature: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "version": version,
        "notes": format!("Lost Harness {version}"),
        "pub_date": "2026-07-30T00:00:00Z",
        "platforms": {
            target: { "signature": signature, "url": url }
        }
    }))
    .unwrap()
}

/// Serve a manifest + payload and run `check()` → `download()` against them.
async fn check_and_download(
    pubkey: &str,
    version: &str,
    payload: Vec<u8>,
    signature: &str,
) -> Result<Vec<u8>, String> {
    check_and_download_for_target(TEST_TARGET, pubkey, version, payload, signature).await
}

async fn check_and_download_for_target(
    target: &str,
    pubkey: &str,
    version: &str,
    payload: Vec<u8>,
    signature: &str,
) -> Result<Vec<u8>, String> {
    // Bind first so the manifest can name the real port it will be served on.
    let mut server = LocalServer::bind().await;
    let payload_url = server.url("/payload.app.tar.gz");

    let mut routes = HashMap::new();
    routes.insert(
        "/latest.json".to_string(),
        manifest(target, version, &payload_url, signature),
    );
    routes.insert("/payload.app.tar.gz".to_string(), payload);
    server.serve(routes);

    let app = app_with_pubkey(pubkey);
    let updater = app
        .updater_builder()
        .target(target)
        .endpoints(vec![server.url("/latest.json").parse().unwrap()])
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())?;

    let update = updater
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no update announced".to_string())?;

    assert_eq!(update.version, version);
    assert_eq!(update.current_version, "0.1.0");

    update
        .download(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())
}

// ── The tests ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_correctly_signed_update_is_accepted() {
    let bytes = check_and_download("", "0.1.1", payload_bytes(), &payload_signature())
        .await
        .err();
    // Sanity: an EMPTY pubkey must not be treated as "skip verification".
    assert!(
        bytes.is_some(),
        "an empty pubkey must never accept a payload"
    );

    let verified = check_and_download(
        &test_pubkey(),
        "0.1.1",
        payload_bytes(),
        &payload_signature(),
    )
    .await
    .expect("a correctly signed payload must verify");

    assert_eq!(
        verified,
        payload_bytes(),
        "download returns the verified bytes untouched"
    );
}

#[tokio::test]
async fn a_tampered_payload_is_refused() {
    // One byte flipped in the middle of the archive — the manifest, the
    // signature and the announced version are all still genuine.
    let mut tampered = payload_bytes();
    let mid = tampered.len() / 2;
    tampered[mid] ^= 0xff;
    assert_ne!(tampered, payload_bytes());

    let err = check_and_download(&test_pubkey(), "0.1.1", tampered, &payload_signature())
        .await
        .expect_err("a tampered payload MUST be refused");

    assert!(
        err.to_lowercase().contains("signature")
            || err.to_lowercase().contains("verif")
            || err.to_lowercase().contains("minisign"),
        "the refusal must be a signature failure, got: {err}"
    );
}

#[tokio::test]
async fn an_unsigned_update_is_refused() {
    // A manifest that simply omits the signature (the naive "just ship it"
    // mistake) must not install.
    let err = check_and_download(&test_pubkey(), "0.1.1", payload_bytes(), "")
        .await
        .expect_err("an unsigned payload MUST be refused");
    assert!(!err.is_empty());
}

#[tokio::test]
async fn a_payload_signed_by_the_wrong_key_is_refused() {
    // Genuine payload, genuine signature — but verified against the app's REAL
    // configured public key, which did not sign it. This is the attack where
    // someone serves a correctly-formed release signed with their own key.
    let err = check_and_download(
        &production_pubkey(),
        "0.1.1",
        payload_bytes(),
        &payload_signature(),
    )
    .await
    .expect_err("a payload signed by a different key MUST be refused");
    assert!(!err.is_empty());
}

#[tokio::test]
async fn the_release_workflows_manifest_shape_is_one_the_updater_accepts() {
    // `manifest()` is written to mirror the `jq` program in the release job of
    // .github/workflows/build.yml, and the platform key here is the exact one
    // that job writes. If either drifts, a tagged release would produce a
    // manifest the shipped app can't read — this test is the tripwire.
    //
    // `darwin-aarch64` is also what a real arm64 macOS build derives for
    // itself (`{os}-{arch}`), so this is the production lookup path.
    let verified = check_and_download_for_target(
        "darwin-aarch64",
        &test_pubkey(),
        "0.1.1",
        payload_bytes(),
        &payload_signature(),
    )
    .await
    .expect("the shipped manifest shape must parse, resolve and verify");

    assert_eq!(verified, payload_bytes());
}

/// The same check → download → verify path, run against a **real bundle this
/// repo actually produced** rather than the small committed fixture. Ignored by
/// default because it needs a `tauri build` (minutes, and ~28 MB of output that
/// has no business in git).
///
/// To run it:
///
/// ```sh
/// # 1. a throwaway signing key — NEVER the one in ~/.tauri
/// npx tauri signer generate --ci -p "" -w /tmp/lh-test.key -f
/// # 2. a real bundle, signed with it
/// TAURI_SIGNING_PRIVATE_KEY="$(cat /tmp/lh-test.key)" \
/// TAURI_SIGNING_PRIVATE_KEY_PASSWORD="" \
///   npm run tauri build -- --bundles app
/// # 3. point the test at the bundle and the key's public half
/// LH_REAL_BUNDLE_DIR="src-tauri/target/release/bundle/macos" \
/// LH_REAL_BUNDLE_PUBKEY="$(cat /tmp/lh-test.key.pub)" \
///   cargo test --lib updater::signature_tests -- --ignored --nocapture
/// ```
///
/// What it adds over the fixture tests: proof that the BUNDLER's own
/// `.app.tar.gz` + `.sig` (not a hand-made tarball) are what the updater
/// accepts, end to end, over a real socket.
#[tokio::test]
#[ignore = "needs a local `tauri build`; see the doc comment for the exact steps"]
async fn a_real_bundler_produced_artifact_verifies() {
    let dir = match std::env::var("LH_REAL_BUNDLE_DIR") {
        Ok(d) => PathBuf::from(d),
        Err(_) => {
            panic!("set LH_REAL_BUNDLE_DIR (and LH_REAL_BUNDLE_PUBKEY) — see the doc comment")
        }
    };
    let pubkey = std::env::var("LH_REAL_BUNDLE_PUBKEY")
        .expect("set LH_REAL_BUNDLE_PUBKEY to the .pub that signed the bundle");

    let archive = std::fs::read_dir(&dir)
        .expect("bundle dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.to_string_lossy().ends_with(".app.tar.gz"))
        .expect("no .app.tar.gz in the bundle dir — is createUpdaterArtifacts still true?");
    let signature = std::fs::read_to_string(format!("{}.sig", archive.display()))
        .expect("the bundle has no .sig — the build did not sign the updater payload")
        .trim()
        .to_string();
    let payload = std::fs::read(&archive).expect("read the real bundle");
    eprintln!("verifying {} ({} bytes)", archive.display(), payload.len());

    let verified = check_and_download_for_target(
        "darwin-aarch64",
        pubkey.trim(),
        "0.1.1",
        payload.clone(),
        &signature,
    )
    .await
    .expect("a real, freshly signed bundle must verify");

    assert_eq!(verified, payload);
}

#[tokio::test]
async fn a_manifest_announcing_the_running_version_is_not_an_update() {
    // The plugin's own comparison should already drop this; `check_now`'s
    // `is_strictly_newer` is the app's second gate. Assert the first one here.
    let err = check_and_download(
        &test_pubkey(),
        "0.1.0",
        payload_bytes(),
        &payload_signature(),
    )
    .await
    .expect_err("0.1.0 is not newer than 0.1.0");
    assert_eq!(err, "no update announced");
}
