//! Google OAuth 2.0 for installed apps: loopback redirect (RFC 8252) + PKCE
//! (RFC 7636, S256), token exchange and refresh.
//!
//! WHY this shape: a desktop app cannot keep a client secret secret and
//! cannot receive a normal web redirect, so the sanctioned flow is (1) open
//! the system browser at Google's consent page, (2) catch the one redirect on
//! an ephemeral `127.0.0.1` listener we bind ourselves, (3) exchange the
//! authorization code — bound to our PKCE verifier so a code intercepted by
//! another local process is useless — for tokens. `access_type=offline` +
//! `prompt=consent` force a refresh token every time (Google only issues one
//! on a consenting grant), because the refresh token is the durable credential
//! stage 2 puts in the keychain.
//!
//! Seams: all token-endpoint HTTP goes through the [`TokenEndpoint`] trait
//! (real impl = one reqwest form POST), so PKCE generation, redirect parsing,
//! state (CSRF) rejection, and refresh-failure classification are all
//! unit-tested without network. The loopback listener is real tokio TCP and
//! is tested by driving an actual local `TcpStream`.
//!
//! Failure honesty: [`refresh`] classifies `invalid_grant` as
//! [`RefreshError::NeedsReconnect`] — the refresh token is dead (Google
//! Testing-mode ~7-day expiry, or user revocation) and retrying is lying to
//! the user. See the module docs in [`super`] for why the UI must treat that
//! as a normal state.

use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::BoxFuture;

/// Google's consent page for the authorization-code flow.
pub const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";

/// Google's token endpoint (code exchange + refresh).
pub const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// The Google productivity scopes this product asks for: Gmail read/send plus
/// the narrow Calendar event and Google Tasks surfaces. They are requested in
/// one per-profile consent flow so Calendar and Tasks never need a second
/// credential store. Calendar is limited to events, not calendar settings or
/// ACLs; Gmail still omits modify/delete.
pub const GOOGLE_PRODUCTIVITY_SCOPES: &str =
    "https://www.googleapis.com/auth/gmail.readonly https://www.googleapis.com/auth/gmail.send https://www.googleapis.com/auth/calendar.events https://www.googleapis.com/auth/tasks";

/// The whole begin→redirect→exchange dance must finish inside this window;
/// after it the listener is dropped and the attempt is void.
pub const AUTH_TIMEOUT_SECS: u64 = 300;

/// Per-connection read budget: a stray speculative connection (browsers
/// pre-connect without sending) must not wedge the real redirect behind it.
/// Kept short — connections are handled serially (one `accept` at a time),
/// so this IS the worst-case delay a single silent/garbage connection can
/// add in front of the real browser redirect landing. That residual bound
/// is accepted rather than spawning per-connection tasks over an mpsc: the
/// listener only lives for the few seconds of a human clicking through
/// consent, so a multi-second stall from a pathological local process is a
/// tolerable ceiling, not a real DoS (see [`wait_and_exchange`] for the
/// actual DoS fix: a forged/mismatched-state request can no longer end the
/// flow at all, so this timeout is now only about a silent connection, not
/// a malicious one).
const CONN_READ_TIMEOUT_SECS: u64 = 3;

/// Cap on the request head we will buffer from the loopback redirect.
const MAX_REQUEST_HEAD: usize = 16 * 1024;

/// The user's own GCP OAuth client, pasted in Settings (stage 3) and stored
/// under [`super::SECRET_GMAIL_CLIENT_ID`]/[`super::SECRET_GMAIL_CLIENT_SECRET`].
#[derive(Clone)]
pub struct GcpClient {
    pub client_id: String,
    pub client_secret: String,
}

impl std::fmt::Debug for GcpClient {
    /// Redacted: the client secret must never reach logs or error chains.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcpClient")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

/// A PKCE verifier/challenge pair (RFC 7636, S256 only — the plain method is
/// deliberately not implemented).
#[derive(Clone)]
pub struct PkcePair {
    /// 86 chars of base64url alphabet (subset of the RFC's unreserved set),
    /// within the mandated 43..=128 length window. Kept until the exchange.
    pub verifier: String,
    /// `base64url_nopad(sha256(verifier))` — what the auth URL carries.
    pub challenge: String,
}

impl std::fmt::Debug for PkcePair {
    /// Redacted: the verifier is the secret half of the code binding.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PkcePair")
            .field("verifier", &"<redacted>")
            .field("challenge", &self.challenge)
            .finish()
    }
}

/// Tokens as returned by the token endpoint. The access token is memory-only
/// (never persisted); the refresh token — when present — is what stage 2
/// writes to the keychain.
#[derive(Clone)]
pub struct TokenSet {
    pub access_token: String,
    /// Present on a consenting code exchange (`prompt=consent` forces it);
    /// usually absent on a refresh (Google rarely rotates it — keep the old
    /// one when this is `None`).
    pub refresh_token: Option<String>,
    /// Seconds until the access token expires (Google sends 3599).
    pub expires_in_secs: u64,
}

impl std::fmt::Debug for TokenSet {
    /// Redacted: token material must never reach logs or error chains.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSet")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &if self.refresh_token.is_some() {
                    "<present>"
                } else {
                    "<absent>"
                },
            )
            .field("expires_in_secs", &self.expires_in_secs)
            .finish()
    }
}

/// Everything that can go wrong between "open the browser" and "we hold
/// tokens". Typed so the caller can distinguish user-denial from plumbing
/// without string-matching. NOTE: a `state` mismatch/absence is NOT a
/// variant here — it's not a distinguishable failure of THIS attempt, it's
/// how a forged or unrelated request is told apart from the real redirect,
/// so it's silently absorbed by the accept loop (404 + keep waiting) rather
/// than surfaced as an error. See [`wait_and_exchange`] for why: a request
/// without the correct state is never authoritative, so it must never be
/// able to END the flow — surfacing it as a terminal error was exactly the
/// CSRF/DoS this design closes.
#[derive(Debug, thiserror::Error)]
pub enum OauthError {
    #[error("authorization timed out (no browser redirect within 5 minutes)")]
    Timeout,
    #[error("Google reported an authorization error: {0}")]
    Provider(String),
    #[error("malformed authorization redirect: {0}")]
    Protocol(String),
    #[error("token endpoint rejected the code exchange (HTTP {status}): {detail}")]
    TokenEndpoint { status: u16, detail: String },
    #[error("i/o during authorization: {0}")]
    Io(String),
}

/// Refresh-failure classification. The variant IS the product behavior:
/// `NeedsReconnect` renders as a calm re-auth prompt, `Misconfigured` points
/// at the pasted client credentials, `Transient` is the only retryable one.
#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    /// `invalid_grant`: the refresh token is dead — expired (Google
    /// Testing-mode apps expire them after ~7 days) or revoked by the user.
    /// Retrying can never succeed; the account must be reconnected.
    #[error("Gmail connection expired or was revoked — reconnect the account ({detail})")]
    NeedsReconnect { detail: String },
    /// The OAuth client itself was rejected (`invalid_client` and kin, or any
    /// other 4xx): the pasted client id/secret is wrong or the GCP app is
    /// broken. Reconnecting with the same credentials would fail identically.
    #[error("OAuth client rejected ({detail}) — check the pasted GCP client id/secret")]
    Misconfigured { detail: String },
    /// Network trouble, 5xx, or rate limiting — retry later with the same
    /// refresh token.
    #[error("transient token-refresh failure: {detail}")]
    Transient { detail: String },
}

// ---------------------------------------------------------------------------
// The token-endpoint seam
// ---------------------------------------------------------------------------

/// The one HTTP interaction this flow needs: a form POST to the token
/// endpoint, returned as `(status, body)` so classification stays pure and
/// fake-testable (same object-safe boxed-future shape as
/// `models::runner::HealthCheck`).
pub trait TokenEndpoint: Send + Sync {
    fn post_form(
        &self,
        form: Vec<(String, String)>,
    ) -> BoxFuture<'_, anyhow::Result<(u16, String)>>;
}

/// Hard ceiling on a token-endpoint response body, in bytes.
///
/// WHY: an OAuth token response is a small JSON object — a few hundred bytes,
/// or a short RFC 6749 error object. There is no legitimate reason for it to
/// approach a megabyte, so a much tighter bound than the Gmail/Calendar fetch
/// ceiling is appropriate here. Anything past it is refused rather than
/// buffered.
const MAX_TOKEN_RESPONSE_BYTES: usize = 1024 * 1024;

/// The real endpoint: one reqwest form POST to [`GOOGLE_TOKEN_URL`].
pub struct HttpTokenEndpoint {
    client: reqwest::Client,
    url: String,
    /// Response-body ceiling in bytes (see [`MAX_TOKEN_RESPONSE_BYTES`]). A
    /// field rather than a bare const so tests can drive the refusal path
    /// without allocating a megabyte.
    max_response_bytes: usize,
}

impl HttpTokenEndpoint {
    /// Build the production endpoint. Short timeouts: a token POST is tiny,
    /// and a hung exchange must not eat the whole auth window.
    pub fn new() -> anyhow::Result<Self> {
        Self::build(GOOGLE_TOKEN_URL.to_string(), MAX_TOKEN_RESPONSE_BYTES)
    }

    fn build(url: String, max_response_bytes: usize) -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10))
                .build()?,
            url,
            max_response_bytes,
        })
    }
}

impl TokenEndpoint for HttpTokenEndpoint {
    fn post_form(
        &self,
        form: Vec<(String, String)>,
    ) -> BoxFuture<'_, anyhow::Result<(u16, String)>> {
        Box::pin(async move {
            let resp = self
                .client
                .post(&self.url)
                .form(&form)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("POST token endpoint failed: {e}"))?;
            let status = resp.status().as_u16();
            // Bounded, not `text().await.unwrap_or_default()`: the unbounded
            // buffer let whatever answered this POST decide our memory usage,
            // and swallowing a read failure produced an empty body that
            // classification would read as a malformed-but-present response.
            let body = super::gmail::read_body_capped(
                resp,
                self.max_response_bytes,
                "OAuth token endpoint",
            )
            .await?;
            Ok((status, body))
        })
    }
}

// ---------------------------------------------------------------------------
// PKCE + state generation (pure given entropy)
// ---------------------------------------------------------------------------

/// `n * 16` bytes of OS-CSPRNG entropy via the crate's existing `uuid` dep
/// (v4 = `getrandom`-backed). Six of each UUID's 128 bits are fixed
/// version/variant bits, so entropy is `n * 122` bits — callers size `n` so
/// that clears the RFC 7636 256-bit recommendation with margin, without
/// pulling a new RNG crate into the tree.
fn random_bytes(n_uuids: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n_uuids * 16);
    for _ in 0..n_uuids {
        out.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    out
}

/// Generate a fresh PKCE pair: 64 entropy bytes (~488 random bits) →
/// 86-char base64url verifier; challenge = `base64url_nopad(sha256(verifier))`.
pub fn generate_pkce() -> PkcePair {
    let verifier = URL_SAFE_NO_PAD.encode(random_bytes(4));
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    PkcePair {
        verifier,
        challenge,
    }
}

/// Generate the anti-CSRF `state` token: 32 entropy bytes → 43 chars.
pub fn generate_state() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes(2))
}

// ---------------------------------------------------------------------------
// The flow: begin (bind + URL) → finish (one redirect → exchange)
// ---------------------------------------------------------------------------

/// A begun authorization: the loopback listener is ALREADY BOUND (so the
/// `redirect_uri` in `auth_url` is guaranteed answerable) and single-use.
/// Open `auth_url` in the system browser, then call [`PendingAuth::finish`].
pub struct PendingAuth {
    listener: TcpListener,
    /// The ephemeral loopback port the OS assigned. (Diagnostic metadata —
    /// the consent URL already embeds it; kept for logging/tests.)
    #[allow(dead_code)]
    pub port: u16,
    /// The fully-assembled consent URL to open in the system browser.
    pub auth_url: String,
    redirect_uri: String,
    pkce: PkcePair,
    state: String,
}

/// Bind the loopback listener and assemble the consent URL. No network I/O
/// beyond the local bind; nothing is sent to Google until the browser opens
/// the URL.
pub async fn begin_auth(gcp: &GcpClient) -> Result<PendingAuth, OauthError> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| OauthError::Io(format!("couldn't bind loopback listener: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| OauthError::Io(format!("couldn't read bound port: {e}")))?
        .port();
    // RFC 8252 §7.3: loopback IP literal, NOT `localhost` (resolvers can remap
    // it). Stored so the exchange reuses the byte-identical string — Google
    // rejects a mismatched redirect_uri.
    let redirect_uri = format!("http://127.0.0.1:{port}");

    let pkce = generate_pkce();
    let state = generate_state();

    let mut u = url::Url::parse(GOOGLE_AUTH_URL).expect("static auth base URL");
    u.query_pairs_mut()
        .append_pair("client_id", &gcp.client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", GOOGLE_PRODUCTIVITY_SCOPES)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        // Offline + forced consent: the only combination that reliably yields
        // a refresh token (Google omits it on silent re-grants).
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");

    Ok(PendingAuth {
        listener,
        port,
        auth_url: u.into(),
        redirect_uri,
        pkce,
        state,
    })
}

/// What the browser redirect carried. `None` fields are honestly absent.
#[derive(Debug, PartialEq, Eq)]
struct RedirectParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Parse an HTTP request head into OAuth redirect params. Returns `None` for
/// anything that isn't a plausible redirect (wrong method, unparseable
/// request line, or a query carrying none of code/state/error — e.g. the
/// browser's follow-up `/favicon.ico` fetch), so the accept loop can 404 it
/// and keep waiting. Percent-decoding is delegated to the `url` crate.
fn parse_redirect_request(head: &str) -> Option<RedirectParams> {
    let line = head.lines().next()?;
    let mut parts = line.split_ascii_whitespace();
    if parts.next()? != "GET" {
        return None;
    }
    let path = parts.next()?;
    if !path.starts_with('/') {
        return None;
    }
    let parsed = url::Url::parse(&format!("http://127.0.0.1{path}")).ok()?;
    let mut out = RedirectParams {
        code: None,
        state: None,
        error: None,
    };
    for (k, v) in parsed.query_pairs() {
        match k.as_ref() {
            "code" => out.code = Some(v.into_owned()),
            "state" => out.state = Some(v.into_owned()),
            "error" => out.error = Some(v.into_owned()),
            _ => {}
        }
    }
    if out.code.is_none() && out.state.is_none() && out.error.is_none() {
        return None;
    }
    Some(out)
}

/// The self-contained page the browser lands on. No external resources — it
/// must render with the network gone and leaks nothing.
const SUCCESS_HTML: &str = "<!doctype html><meta charset=\"utf-8\"><title>Lost Harness</title>\
<body style=\"font-family:system-ui,sans-serif;display:grid;place-items:center;height:90vh\">\
<div style=\"text-align:center\"><h1>Connected</h1>\
<p>You can close this tab — Lost Harness is connected.</p></div></body>";

const REJECT_HTML: &str = "<!doctype html><meta charset=\"utf-8\"><title>Lost Harness</title>\
<body style=\"font-family:system-ui,sans-serif;display:grid;place-items:center;height:90vh\">\
<div style=\"text-align:center\"><h1>Not connected</h1>\
<p>The authorization was rejected. You can close this tab and try again from Lost Harness.</p>\
</div></body>";

/// Write a minimal HTTP/1.1 response and close. Best-effort — a browser that
/// hung up early must not fail the flow.
async fn write_response(stream: &mut TcpStream, status: u16, reason: &str, html: &str) {
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Read a request head (through the blank line), capped at
/// [`MAX_REQUEST_HEAD`]. Returns what was read even if the terminator never
/// arrived — the parser only needs the request line.
async fn read_head(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= MAX_REQUEST_HEAD {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

impl PendingAuth {
    /// The state token bound to this attempt (stage 2 has no need for it —
    /// exposed for tests and diagnostics).
    #[allow(dead_code)] // test/diagnostic accessor; the flow reads the field directly
    pub fn state(&self) -> &str {
        &self.state
    }

    /// Wait for the browser redirect, vet it, answer it with a tiny local
    /// page, and exchange the code for tokens. Consumes `self`: the listener
    /// is single-use and drops on every path out of here.
    ///
    /// - The whole wait+exchange runs under a 5-minute timeout.
    /// - Stray requests (favicon fetches, speculative pre-connects) get a 404
    ///   and the wait continues; only ONE plausible OAuth redirect is ever
    ///   processed.
    /// - `state` is validated BEFORE anything else. A request with no state
    ///   or a MISMATCHED state is NOT treated as the real redirect — it is
    ///   NOT authoritative for this attempt, so it gets a 404 and the wait
    ///   continues, exactly like stray noise. This is deliberate: any local
    ///   process can connect to this loopback port, so a forged
    ///   `GET /?error=...` or a stolen-looking `GET /?code=...` with the
    ///   wrong (or no) state must never be able to END the flow — doing so
    ///   would drop the single-use listener out from under the REAL browser
    ///   redirect, which then hits a closed port and silently fails
    ///   (a local CSRF/DoS). The code in a forged redirect is therefore
    ///   never sent anywhere. Only once `state` matches do we look at
    ///   `error`/`code` — a genuine Google denial redirect carries the
    ///   correct state too, so denial handling still works.
    /// - The browser page is answered BEFORE the exchange (per the flow
    ///   design), so a failed exchange surfaces in the app, not the tab.
    pub async fn finish(
        self,
        endpoint: &dyn TokenEndpoint,
        gcp: &GcpClient,
    ) -> Result<TokenSet, OauthError> {
        let PendingAuth {
            listener,
            redirect_uri,
            pkce,
            state,
            ..
        } = self;
        tokio::time::timeout(
            Duration::from_secs(AUTH_TIMEOUT_SECS),
            wait_and_exchange(listener, endpoint, gcp, &redirect_uri, &pkce, &state),
        )
        .await
        .map_err(|_| OauthError::Timeout)?
    }
}

/// The un-timed inner flow — see [`PendingAuth::finish`] for the contract.
async fn wait_and_exchange(
    listener: TcpListener,
    endpoint: &dyn TokenEndpoint,
    gcp: &GcpClient,
    redirect_uri: &str,
    pkce: &PkcePair,
    expected_state: &str,
) -> Result<TokenSet, OauthError> {
    let code = loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| OauthError::Io(format!("accept failed: {e}")))?;
        // Bounded read: a connection that sends nothing (speculative
        // pre-connect) is dropped, not allowed to wedge the real redirect.
        let head = match tokio::time::timeout(
            Duration::from_secs(CONN_READ_TIMEOUT_SECS),
            read_head(&mut stream),
        )
        .await
        {
            Ok(Ok(head)) => head,
            Ok(Err(_)) | Err(_) => continue,
        };
        let Some(params) = parse_redirect_request(&head) else {
            write_response(&mut stream, 404, "Not Found", "").await;
            continue;
        };
        // `state` FIRST, before looking at `error`/`code`: a request that
        // doesn't carry OUR state token is not authoritative for this
        // attempt — forged, stale, or simply a different in-flight
        // attempt's redirect. Treat it exactly like stray noise (404, keep
        // waiting) rather than terminating the loop, so it can never end
        // the flow out from under the real browser redirect still on its
        // way (see `finish`'s doc comment for why that matters).
        if params.state.as_deref() != Some(expected_state) {
            write_response(&mut stream, 404, "Not Found", "").await;
            continue;
        }
        // From here `state` matches: this IS the redirect for OUR attempt —
        // vet it, answer it, stop listening. A genuine Google denial
        // redirect carries the correct state too, so this still fires for
        // real user-denial.
        if let Some(err) = params.error {
            write_response(&mut stream, 200, "OK", REJECT_HTML).await;
            return Err(OauthError::Provider(err));
        }
        let Some(code) = params.code else {
            write_response(&mut stream, 400, "Bad Request", REJECT_HTML).await;
            return Err(OauthError::Protocol("redirect carried no code".into()));
        };
        write_response(&mut stream, 200, "OK", SUCCESS_HTML).await;
        break code;
    };
    // Listener drops here — single-use by construction.
    drop(listener);
    exchange_code(endpoint, gcp, &code, &pkce.verifier, redirect_uri).await
}

/// Exchange an authorization code (+ PKCE verifier) for tokens.
pub async fn exchange_code(
    endpoint: &dyn TokenEndpoint,
    gcp: &GcpClient,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Result<TokenSet, OauthError> {
    let form = vec![
        ("client_id".to_string(), gcp.client_id.clone()),
        ("client_secret".to_string(), gcp.client_secret.clone()),
        ("code".to_string(), code.to_string()),
        ("code_verifier".to_string(), code_verifier.to_string()),
        ("grant_type".to_string(), "authorization_code".to_string()),
        ("redirect_uri".to_string(), redirect_uri.to_string()),
    ];
    let (status, body) = endpoint
        .post_form(form)
        .await
        .map_err(|e| OauthError::Io(e.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(OauthError::TokenEndpoint {
            status,
            detail: snippet(&body),
        });
    }
    parse_token_response(&body)
        .map_err(|e| OauthError::Protocol(format!("token response didn't parse: {e}")))
}

/// Refresh an access token. Classifies failure — see [`RefreshError`].
pub async fn refresh(
    endpoint: &dyn TokenEndpoint,
    gcp: &GcpClient,
    refresh_token: &str,
) -> Result<TokenSet, RefreshError> {
    let form = vec![
        ("client_id".to_string(), gcp.client_id.clone()),
        ("client_secret".to_string(), gcp.client_secret.clone()),
        ("refresh_token".to_string(), refresh_token.to_string()),
        ("grant_type".to_string(), "refresh_token".to_string()),
    ];
    let (status, body) = endpoint
        .post_form(form)
        .await
        .map_err(|e| RefreshError::Transient {
            detail: e.to_string(),
        })?;
    if !(200..300).contains(&status) {
        return Err(classify_refresh_failure(status, &body));
    }
    parse_token_response(&body).map_err(|e| RefreshError::Transient {
        detail: format!("2xx refresh response didn't parse: {e}"),
    })
}

/// Wire shape of a token-endpoint success body. Only what we need.
#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// Parse a 2xx token-endpoint body. Pure.
fn parse_token_response(body: &str) -> anyhow::Result<TokenSet> {
    let t: TokenResponse = serde_json::from_str(body)?;
    Ok(TokenSet {
        access_token: t.access_token,
        refresh_token: t.refresh_token,
        // Google always sends expires_in; default conservatively if a proxy ate it.
        expires_in_secs: t.expires_in.unwrap_or(3600),
    })
}

/// Wire shape of a token-endpoint error body (RFC 6749 §5.2).
#[derive(serde::Deserialize, Default)]
struct TokenErrorBody {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// Classify a non-2xx refresh response. Pure — this is the honesty seam the
/// task pins: `invalid_grant` is a DEAD token, never a retry.
pub fn classify_refresh_failure(status: u16, body: &str) -> RefreshError {
    let parsed: TokenErrorBody = serde_json::from_str(body).unwrap_or_default();
    let err = parsed.error.unwrap_or_default();
    let detail = match parsed.error_description {
        Some(d) if !d.is_empty() => format!("{err}: {d}"),
        _ if !err.is_empty() => err.clone(),
        _ => format!("HTTP {status}"),
    };
    match (status, err.as_str()) {
        (_, "invalid_grant") => RefreshError::NeedsReconnect { detail },
        // 5xx and 429: the server's problem or rate limiting — same token can
        // succeed later. Everything else in 4xx means OUR request/client is
        // wrong, and identical retries would fail identically.
        (500..=599, _) | (429, _) => RefreshError::Transient { detail },
        (400..=499, _) => RefreshError::Misconfigured { detail },
        _ => RefreshError::Transient { detail },
    }
}

/// First ~300 chars of an error body — enough to diagnose, bounded so a huge
/// or hostile body can't blow up an error string. Token-endpoint FAILURE
/// bodies never contain token material.
fn snippet(body: &str) -> String {
    let mut s: String = body.chars().take(300).collect();
    if s.len() < body.len() {
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn gcp() -> GcpClient {
        GcpClient {
            client_id: "id-123.apps.googleusercontent.com".into(),
            client_secret: "shhh".into(),
        }
    }

    /// Scripted fake endpoint: records every form, replays queued responses.
    struct FakeEndpoint {
        forms: Mutex<Vec<Vec<(String, String)>>>,
        responses: Mutex<Vec<anyhow::Result<(u16, String)>>>,
    }

    impl FakeEndpoint {
        fn respond(status: u16, body: &str) -> Self {
            Self {
                forms: Mutex::new(Vec::new()),
                responses: Mutex::new(vec![Ok((status, body.to_string()))]),
            }
        }
        fn calls(&self) -> usize {
            self.forms.lock().unwrap().len()
        }
        fn form_value(&self, call: usize, key: &str) -> Option<String> {
            self.forms
                .lock()
                .unwrap()
                .get(call)
                .and_then(|f| f.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()))
        }
    }

    impl TokenEndpoint for FakeEndpoint {
        fn post_form(
            &self,
            form: Vec<(String, String)>,
        ) -> BoxFuture<'_, anyhow::Result<(u16, String)>> {
            self.forms.lock().unwrap().push(form);
            let resp = self
                .responses
                .lock()
                .unwrap()
                .pop()
                .expect("scripted response");
            Box::pin(async move { resp })
        }
    }

    const TOKEN_OK: &str = r#"{"access_token":"at-1","refresh_token":"rt-1",
        "expires_in":3599,"token_type":"Bearer","scope":"x"}"#;

    #[test]
    fn pkce_pair_meets_rfc7636() {
        let p = generate_pkce();
        assert!(
            (43..=128).contains(&p.verifier.len()),
            "len {}",
            p.verifier.len()
        );
        assert!(
            p.verifier
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "base64url alphabet ⊂ RFC 7636 unreserved"
        );
        // The challenge is exactly base64url_nopad(sha256(verifier)).
        let want = URL_SAFE_NO_PAD.encode(Sha256::digest(p.verifier.as_bytes()));
        assert_eq!(p.challenge, want);
        assert!(!p.challenge.contains('='), "no padding");
        // Fresh entropy per call.
        assert_ne!(generate_pkce().verifier, p.verifier);
        assert_ne!(generate_state(), generate_state());
    }

    #[tokio::test]
    async fn begin_auth_binds_first_and_builds_a_complete_url() {
        let pending = begin_auth(&gcp()).await.unwrap();
        assert_ne!(pending.port, 0, "a real ephemeral port was assigned");
        let u = url::Url::parse(&pending.auth_url).unwrap();
        assert_eq!(
            u.origin().ascii_serialization(),
            "https://accounts.google.com"
        );
        let q: std::collections::HashMap<_, _> = u.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], gcp().client_id);
        assert_eq!(
            q["redirect_uri"],
            format!("http://127.0.0.1:{}", pending.port)
        );
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["scope"], GOOGLE_PRODUCTIVITY_SCOPES);
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["code_challenge"], pending.pkce.challenge);
        assert_eq!(q["state"], pending.state);
        assert_eq!(
            q["access_type"], "offline",
            "refresh token requires offline"
        );
        assert_eq!(q["prompt"], "consent", "consent forces a refresh token");
    }

    #[test]
    fn redirect_parsing_accepts_the_oauth_redirect_and_ignores_noise() {
        let got = parse_redirect_request(
            "GET /?code=4%2Fabc&state=st1 HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        )
        .unwrap();
        assert_eq!(got.code.as_deref(), Some("4/abc"), "percent-decoded");
        assert_eq!(got.state.as_deref(), Some("st1"));
        assert_eq!(got.error, None);
        // Denial redirect.
        let denied =
            parse_redirect_request("GET /?error=access_denied&state=s HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(denied.error.as_deref(), Some("access_denied"));
        // Noise: favicon, non-GET, garbage — all None (404 + keep waiting).
        assert!(parse_redirect_request("GET /favicon.ico HTTP/1.1\r\n\r\n").is_none());
        assert!(parse_redirect_request("POST /?code=x HTTP/1.1\r\n\r\n").is_none());
        assert!(parse_redirect_request("").is_none());
    }

    /// Drive the real listener with a real local TcpStream: happy path.
    #[tokio::test]
    async fn loopback_redirect_roundtrip_exchanges_the_code() {
        let pending = begin_auth(&gcp()).await.unwrap();
        let port = pending.port;
        let state = pending.state.clone();
        let verifier = pending.pkce.verifier.clone();

        let browser = tokio::spawn(async move {
            let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let req = format!("GET /?state={state}&code=auth-code-1 HTTP/1.1\r\nHost: x\r\n\r\n");
            s.write_all(req.as_bytes()).await.unwrap();
            let mut resp = String::new();
            s.read_to_string(&mut resp).await.unwrap();
            resp
        });

        let endpoint = FakeEndpoint::respond(200, TOKEN_OK);
        let tokens = pending.finish(&endpoint, &gcp()).await.unwrap();
        assert_eq!(tokens.access_token, "at-1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt-1"));
        assert_eq!(tokens.expires_in_secs, 3599);

        // The browser saw the self-contained success page.
        let page = browser.await.unwrap();
        assert!(
            page.starts_with("HTTP/1.1 200"),
            "got: {}",
            &page[..40.min(page.len())]
        );
        assert!(page.contains("You can close this tab"));
        assert!(
            !page.contains("http://"),
            "no external resources on the page"
        );

        // The exchange carried the code AND the PKCE verifier.
        assert_eq!(endpoint.calls(), 1);
        assert_eq!(
            endpoint.form_value(0, "code").as_deref(),
            Some("auth-code-1")
        );
        assert_eq!(
            endpoint.form_value(0, "code_verifier").as_deref(),
            Some(&*verifier)
        );
        assert_eq!(
            endpoint.form_value(0, "redirect_uri").as_deref(),
            Some(&*format!("http://127.0.0.1:{port}"))
        );
        assert_eq!(
            endpoint.form_value(0, "grant_type").as_deref(),
            Some("authorization_code")
        );
    }

    /// CSRF/DoS: a redirect with the wrong state is NOT authoritative — its
    /// code is never sent to the token endpoint, AND (the bug this test now
    /// pins) it must NOT be able to end the flow either. A forged request
    /// used to terminate the whole `finish` call before the state check was
    /// reordered, which meant a local process racing the real browser
    /// redirect could kill the single-use listener out from under it. Now
    /// the forged request is 404'd and silently ignored, and the real,
    /// correctly-stated redirect still completes the flow.
    #[tokio::test]
    async fn forged_state_is_ignored_and_the_real_redirect_still_completes() {
        let pending = begin_auth(&gcp()).await.unwrap();
        let port = pending.port;
        let state = pending.state.clone();
        let browser = tokio::spawn(async move {
            // A local process racing the real browser: wrong state, a
            // real-looking stolen code.
            let mut s1 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            s1.write_all(b"GET /?state=FORGED&code=stolen HTTP/1.1\r\nHost: x\r\n\r\n")
                .await
                .unwrap();
            let mut r1 = String::new();
            s1.read_to_string(&mut r1).await.unwrap();
            assert!(
                r1.starts_with("HTTP/1.1 404"),
                "forged state → 404, got {r1}"
            );

            // The flow must still be alive to receive this.
            let mut s2 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let req = format!("GET /?state={state}&code=real HTTP/1.1\r\nHost: x\r\n\r\n");
            s2.write_all(req.as_bytes()).await.unwrap();
            let mut r2 = String::new();
            s2.read_to_string(&mut r2).await.unwrap();
            assert!(r2.starts_with("HTTP/1.1 200"), "got {r2}");
        });

        let endpoint = FakeEndpoint::respond(200, TOKEN_OK);
        let tokens = pending.finish(&endpoint, &gcp()).await.unwrap();
        assert_eq!(tokens.access_token, "at-1");
        assert_eq!(
            endpoint.calls(),
            1,
            "only the correctly-stated redirect is ever exchanged"
        );
        assert_eq!(
            endpoint.form_value(0, "code").as_deref(),
            Some("real"),
            "the forged/stolen code must never reach the endpoint"
        );
        browser.await.unwrap();
    }

    /// The exact bug in the finding: an `error=` redirect with NO state at
    /// all used to be checked (and could terminate the flow) BEFORE the
    /// `state` check ran. Now `state` is validated first, so a stateless
    /// forged error probe is silently ignored and the real redirect still
    /// lands.
    #[tokio::test]
    async fn forged_error_with_no_state_does_not_end_the_flow() {
        let pending = begin_auth(&gcp()).await.unwrap();
        let port = pending.port;
        let state = pending.state.clone();
        let browser = tokio::spawn(async move {
            let mut s1 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            s1.write_all(b"GET /?error=access_denied HTTP/1.1\r\nHost: x\r\n\r\n")
                .await
                .unwrap();
            let mut r1 = String::new();
            s1.read_to_string(&mut r1).await.unwrap();
            assert!(
                r1.starts_with("HTTP/1.1 404"),
                "stateless error → 404, got {r1}"
            );

            let mut s2 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let req = format!("GET /?state={state}&code=real HTTP/1.1\r\nHost: x\r\n\r\n");
            s2.write_all(req.as_bytes()).await.unwrap();
            let mut r2 = String::new();
            s2.read_to_string(&mut r2).await.unwrap();
            assert!(r2.starts_with("HTTP/1.1 200"), "got {r2}");
        });

        let endpoint = FakeEndpoint::respond(200, TOKEN_OK);
        let tokens = pending.finish(&endpoint, &gcp()).await.unwrap();
        assert_eq!(tokens.access_token, "at-1");
        assert_eq!(endpoint.calls(), 1);
        assert_eq!(endpoint.form_value(0, "code").as_deref(), Some("real"));
        browser.await.unwrap();
    }

    /// A stray request (favicon) before the real redirect gets a 404 and the
    /// flow still completes — one listener, one processed redirect.
    #[tokio::test]
    async fn stray_request_gets_404_then_real_redirect_still_lands() {
        let pending = begin_auth(&gcp()).await.unwrap();
        let port = pending.port;
        let state = pending.state.clone();
        let browser = tokio::spawn(async move {
            let mut s1 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            s1.write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: x\r\n\r\n")
                .await
                .unwrap();
            let mut r1 = String::new();
            s1.read_to_string(&mut r1).await.unwrap();
            assert!(
                r1.starts_with("HTTP/1.1 404"),
                "stray request → 404, got {r1}"
            );

            let mut s2 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let req = format!("GET /?state={state}&code=c2 HTTP/1.1\r\nHost: x\r\n\r\n");
            s2.write_all(req.as_bytes()).await.unwrap();
            let mut r2 = String::new();
            s2.read_to_string(&mut r2).await.unwrap();
            assert!(r2.starts_with("HTTP/1.1 200"));
        });

        let endpoint = FakeEndpoint::respond(200, TOKEN_OK);
        let tokens = pending.finish(&endpoint, &gcp()).await.unwrap();
        assert_eq!(tokens.access_token, "at-1");
        assert_eq!(endpoint.form_value(0, "code").as_deref(), Some("c2"));
        browser.await.unwrap();
    }

    /// The user clicking "deny" on the consent page surfaces as Provider.
    #[tokio::test]
    async fn user_denial_surfaces_as_provider_error() {
        let pending = begin_auth(&gcp()).await.unwrap();
        let port = pending.port;
        let state = pending.state.clone();
        tokio::spawn(async move {
            let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let req = format!("GET /?error=access_denied&state={state} HTTP/1.1\r\n\r\n");
            s.write_all(req.as_bytes()).await.unwrap();
            let mut r = String::new();
            let _ = s.read_to_string(&mut r).await;
        });
        let endpoint = FakeEndpoint::respond(200, TOKEN_OK);
        let err = pending.finish(&endpoint, &gcp()).await.unwrap_err();
        assert!(
            matches!(err, OauthError::Provider(ref e) if e == "access_denied"),
            "got {err:?}"
        );
        assert_eq!(endpoint.calls(), 0);
    }

    #[tokio::test]
    async fn refresh_success_parses_tokens_and_sends_the_grant() {
        let endpoint = FakeEndpoint::respond(200, r#"{"access_token":"at-2","expires_in":3599}"#);
        let t = refresh(&endpoint, &gcp(), "rt-old").await.unwrap();
        assert_eq!(t.access_token, "at-2");
        assert_eq!(
            t.refresh_token, None,
            "Google usually doesn't rotate — keep the old one"
        );
        assert_eq!(
            endpoint.form_value(0, "grant_type").as_deref(),
            Some("refresh_token")
        );
        assert_eq!(
            endpoint.form_value(0, "refresh_token").as_deref(),
            Some("rt-old")
        );
    }

    #[tokio::test]
    async fn invalid_grant_refresh_is_needs_reconnect_not_retryable() {
        let endpoint = FakeEndpoint::respond(
            400,
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
        );
        let err = refresh(&endpoint, &gcp(), "rt-dead").await.unwrap_err();
        match err {
            RefreshError::NeedsReconnect { detail } => {
                assert!(detail.contains("invalid_grant"));
                assert!(detail.contains("expired or revoked"));
            }
            other => panic!("invalid_grant must classify as NeedsReconnect, got {other:?}"),
        }
    }

    #[test]
    fn refresh_failure_classification_is_honest() {
        // invalid_grant → dead token, regardless of status code.
        assert!(matches!(
            classify_refresh_failure(400, r#"{"error":"invalid_grant"}"#),
            RefreshError::NeedsReconnect { .. }
        ));
        // Wrong client credentials → misconfigured, not "try again later".
        assert!(matches!(
            classify_refresh_failure(401, r#"{"error":"invalid_client"}"#),
            RefreshError::Misconfigured { .. }
        ));
        // Server trouble / rate limit → transient.
        assert!(matches!(
            classify_refresh_failure(500, "internal error"),
            RefreshError::Transient { .. }
        ));
        assert!(matches!(
            classify_refresh_failure(429, r#"{"error":"rate_limit_exceeded"}"#),
            RefreshError::Transient { .. }
        ));
        // Unparseable 4xx body still classifies (as misconfigured request).
        assert!(matches!(
            classify_refresh_failure(400, "<html>proxy error</html>"),
            RefreshError::Misconfigured { .. }
        ));
    }

    #[test]
    fn debug_impls_redact_secret_material() {
        let t = TokenSet {
            access_token: "SECRET-AT".into(),
            refresh_token: Some("SECRET-RT".into()),
            expires_in_secs: 10,
        };
        let s = format!("{t:?}");
        assert!(!s.contains("SECRET"), "tokens must never Debug-print: {s}");
        let g = format!("{:?}", gcp());
        assert!(
            !g.contains("shhh"),
            "client secret must never Debug-print: {g}"
        );
        let p = format!("{:?}", generate_pkce());
        assert!(!p.contains("verifier: \"") || p.contains("<redacted>"));
    }

    // -- the real reqwest token endpoint against a loopback server ----------

    /// Serve exactly one raw HTTP response on a loopback port, then close.
    async fn serve_once(response: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(&response).await;
                let _ = sock.flush().await;
                let _ = sock.shutdown().await;
            }
        });
        format!("http://{addr}")
    }

    fn with_content_length(body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    /// A chunked response — the size is NOT knowable up front, so only the
    /// running-total gate can stop it.
    fn chunked(total: usize, chunk: usize) -> Vec<u8> {
        let mut out =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
                .to_vec();
        let mut sent = 0;
        while sent < total {
            let n = chunk.min(total - sent);
            out.extend_from_slice(format!("{n:x}\r\n").as_bytes());
            out.extend(std::iter::repeat(b'a').take(n));
            out.extend_from_slice(b"\r\n");
            sent += n;
        }
        out.extend_from_slice(b"0\r\n\r\n");
        out
    }

    /// The token endpoint had the same unbounded `resp.text()` the Gmail fetch
    /// layer did — and it is reachable before any account is even connected.
    /// It is now capped on the declared length and on the running total, and a
    /// normal-sized token response still comes back intact.
    #[tokio::test]
    async fn oversized_token_endpoint_bodies_are_refused() {
        // 1. Declared Content-Length over the cap → refused up front.
        let url = serve_once(with_content_length(&vec![b'a'; 4096])).await;
        let ep = HttpTokenEndpoint::build(url, 1024).unwrap();
        let err = ep
            .post_form(vec![("grant_type".into(), "refresh_token".into())])
            .await
            .expect_err("a 4 KiB token body must not pass a 1 KiB cap");
        assert!(
            err.to_string()
                .contains("OAuth token endpoint response too large")
                && err.to_string().contains("declared 4096"),
            "got: {err}"
        );

        // 2. Chunked (no declared length) over the cap → refused mid-stream.
        let url = serve_once(chunked(4096, 512)).await;
        let ep = HttpTokenEndpoint::build(url, 1024).unwrap();
        let err = ep
            .post_form(vec![("grant_type".into(), "refresh_token".into())])
            .await
            .expect_err("a chunked 4 KiB token body must not pass a 1 KiB cap");
        assert!(
            err.to_string().contains("exceeded the 1024-byte cap"),
            "got: {err}"
        );

        // 3. Control: a real-shaped token response under the cap round-trips.
        let json = br#"{"access_token":"at-1","expires_in":3599}"#;
        let url = serve_once(with_content_length(json)).await;
        let ep = HttpTokenEndpoint::build(url, 1024).unwrap();
        let (status, body) = ep
            .post_form(vec![("grant_type".into(), "refresh_token".into())])
            .await
            .unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, String::from_utf8(json.to_vec()).unwrap());
    }
}
