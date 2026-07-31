//! Email tools (the email round, stage 2): `email_search`, `email_read`,
//! `email_send` over the `email::gmail` client.
//!
//! Trust posture (from `email/mod.rs`, binding):
//! - Search/read are **off-box egress** — the query goes to Google and mail
//!   content (an indirect-prompt-injection carrier) comes back. Both are
//!   `RiskClass::External` with a surfaced destination, so they route through
//!   the approval spine, and `Tool::egresses_offbox` folds them into the
//!   privacy gate (a Private turn on a local model still can't reach Gmail).
//!   The dispatcher guard-wraps their output like every tool result.
//! - Send is **irreversible** — `RiskClass::Dangerous` (always an explicit
//!   Once-only Ask; never auto-approvable, never rule-grantable), and the C2
//!   durability journal records the dispatch like every mutating action. The
//!   approval dialog shows the recipient via `destination()`.
//! - A missing GCP client / unconnected profile is a NORMAL state: the tools
//!   return a clean, setup-pointing error (never a panic) so the agent can
//!   tell the user what to do.
//!
//! Per-profile: every call builds its client from `ctx.profile` at dispatch
//! time, so a `work` chat can never read the `personal` mailbox.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::Semaphore;

use crate::email::api_error::GoogleApi;
use crate::email::gmail::{build_rfc822, GmailApi, GmailClient, ReqwestGmailHttp, TokenProvider};
use crate::email::google::GoogleClient;
use crate::email::oauth::TokenEndpoint;
use crate::email::token_provider::KeychainTokenProvider;
use crate::secrets::ProviderSecretStore;
use crate::tools::{Capability, ExecCtx, RiskClass, Tool, ToolInput, ToolResult};

/// How many messages `email_search` will fetch bodies for at most — each is
/// its own Gmail API call, so this bounds the burst.
const SEARCH_MAX_CAP: u32 = 10;

/// `email_read` body cap (chars) — a huge thread can't flood the context.
const READ_BODY_CAP: usize = 40_000;

/// How many `email_search` preview fetches may be in flight at once. Above 1
/// so one slow message can't stall the rest of the page; small enough to stay
/// a polite burst against Gmail's per-user rate limit.
const SEARCH_CONCURRENCY: usize = 3;

/// Shared constructor context for the three tools.
#[derive(Clone)]
pub struct EmailToolDeps {
    pub secrets: Arc<dyn ProviderSecretStore>,
    pub endpoint: Arc<dyn TokenEndpoint>,
    /// The recoverable-failure state for every profile — the SAME `Arc` as
    /// `ipc::EmailRuntime`'s (see that type's doc comment). Without it being
    /// shared, an agent-only failure would never light the screen's banners,
    /// since only the screen IPC path used to record anything.
    pub google: crate::ipc::GoogleConnection,
    /// Per-profile cached token providers so the in-memory access-token cache
    /// persists across successive tool calls for the same profile (rather than
    /// forcing a fresh token refresh on every dispatch).
    token_providers: Arc<Mutex<HashMap<String, Arc<KeychainTokenProvider>>>>,
}

impl EmailToolDeps {
    pub fn new(
        secrets: Arc<dyn ProviderSecretStore>,
        endpoint: Arc<dyn TokenEndpoint>,
        google: crate::ipc::GoogleConnection,
    ) -> Self {
        Self {
            secrets,
            endpoint,
            google,
            token_providers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// A cached per-profile token provider. The inner access-token cache is
    /// reused across calls so a fresh token isn't fetched on every dispatch.
    fn token_provider(&self, profile: &str) -> Arc<KeychainTokenProvider> {
        let mut map = self.token_providers.lock();
        map.entry(profile.to_string())
            .or_insert_with(|| {
                Arc::new(KeychainTokenProvider::new(
                    profile,
                    Arc::clone(&self.secrets),
                    Arc::clone(&self.endpoint),
                ))
            })
            .clone()
    }

    fn client(&self, profile: &str) -> anyhow::Result<GmailClient> {
        Ok(GmailClient::new(
            Box::new(ReqwestGmailHttp::new()?),
            self.token_provider(profile),
        ))
    }

    /// Build the same per-profile authenticated Google client used by the
    /// Calendar and Tasks tools. It shares Gmail's keychain token/reconnect
    /// contract; it does not create a second credential store.
    pub(crate) fn google_client(&self, profile: &str) -> anyhow::Result<GoogleClient> {
        GoogleClient::new(Box::new(SharedTokenProvider(self.token_provider(profile))))
    }
}

/// Lets the SHARED per-profile token provider satisfy the owned
/// `Box<dyn TokenProvider>` that `GoogleClient::new` takes, so Calendar/Tasks
/// reuse the same access-token cache (and the same single-flight refresh) as
/// Gmail instead of minting a private provider with a cold cache.
struct SharedTokenProvider(Arc<KeychainTokenProvider>);

impl TokenProvider for SharedTokenProvider {
    fn access_token(
        &self,
        force_refresh: bool,
    ) -> crate::email::BoxFuture<'_, anyhow::Result<String>> {
        self.0.access_token(force_refresh)
    }
}

/// Record what a Google call proved, and turn the outcome into what a tool
/// hands back.
///
/// The agent path used to duplicate the screen path's recording logic (and
/// its blind spots) in a second copy that read error TEXT. Both paths now call
/// the one shared, typed implementation in `email::connection_state`, so a
/// failure the agent hits lights the same banner the screens light, and a
/// success the agent gets clears the same stale state.
pub(crate) fn observe_google_call<T>(
    deps: &EmailToolDeps,
    profile: &str,
    api: GoogleApi,
    outcome: anyhow::Result<T>,
) -> Result<T, String> {
    match outcome {
        Ok(value) => {
            deps.google.observe_success(profile, api);
            Ok(value)
        }
        Err(err) => {
            deps.google.observe_failure(profile, &err);
            Err(err.to_string())
        }
    }
}

/// A step that never reaches Google — building the client, assembling the HTTP
/// stack. Only its FAILURE is evidence.
///
/// [`EmailToolDeps::client`] is a pure constructor: `GmailClient::new` sends
/// nothing. Handing it to [`observe_google_call`] therefore recorded a
/// SUCCESSFUL Gmail call with zero proof that anything succeeded — so merely
/// dispatching `email_search` wiped a real "Gmail is switched off" state and
/// darkened its banner before a single request left the machine, and the state
/// only came back when the call that followed actually failed. Only a
/// completed API call may count as proof.
///
/// The failing half still records: a dead grant hit while minting the token
/// provider is a real verdict, and reaches `observe_failure` the same typed
/// way. (This is what `productivity.rs`'s client-build path already does.)
pub(crate) fn observe_local_step<T>(
    deps: &EmailToolDeps,
    profile: &str,
    outcome: anyhow::Result<T>,
) -> Result<T, String> {
    outcome.map_err(|err| {
        deps.google.observe_failure(profile, &err);
        err.to_string()
    })
}

/// Fetch header/snippet previews for `ids`, at most [`SEARCH_CONCURRENCY`] in
/// flight.
///
/// Returns one entry per id, IN INPUT ORDER, each paired with its own id — so
/// a failed fetch can still name the message it was for (a search result that
/// says `"id": "?"` is useless to the agent and to the user).
async fn fetch_previews(
    client: Arc<dyn GmailApi>,
    ids: Vec<String>,
) -> Vec<(String, Result<crate::email::gmail::EmailMessage, String>)> {
    let sem = Arc::new(Semaphore::new(SEARCH_CONCURRENCY));
    let mut tasks = Vec::with_capacity(ids.len());
    for id in ids.iter().cloned() {
        let client = Arc::clone(&client);
        let sem = Arc::clone(&sem);
        tasks.push(tokio::spawn(async move {
            match sem.acquire_owned().await {
                Ok(_permit) => client
                    .get_message_metadata(&id)
                    .await
                    .map_err(|e| e.to_string()),
                Err(_) => Err("the search fetch pool closed unexpectedly".to_string()),
            }
        }));
    }
    let mut out = Vec::with_capacity(tasks.len());
    for (id, task) in ids.into_iter().zip(tasks) {
        match task.await {
            Ok(res) => out.push((id, res)),
            // A join error means the fetch task panicked. Report it as a row
            // error (with its real id) rather than taking the search down.
            Err(e) => out.push((id, Err(format!("preview fetch failed: {e}")))),
        }
    }
    out
}

// ── email_search ────────────────────────────────────────────────────────────

pub struct EmailSearchTool {
    deps: EmailToolDeps,
}

impl EmailSearchTool {
    pub fn new(deps: EmailToolDeps) -> Self {
        Self { deps }
    }
}

impl Tool for EmailSearchTool {
    fn name(&self) -> &str {
        "email_search"
    }

    fn description(&self) -> &str {
        "Search the connected Gmail inbox. Args: query (Gmail search syntax, e.g. \
         'is:unread from:alice'), max (<=10). Returns id/from/subject/date/snippet per hit."
    }

    fn risk(&self) -> RiskClass {
        RiskClass::External
    }

    fn requires(&self) -> &[Capability] {
        &[Capability::Email]
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Gmail search query (optional; empty = newest mail)" },
                "max": { "type": "integer", "minimum": 1, "maximum": 10 }
            },
            "additionalProperties": false
        })
    }

    fn destination(&self, _args: &serde_json::Value) -> Option<String> {
        Some("gmail.googleapis.com".to_string())
    }

    fn run<'a>(
        &'a self,
        input: ToolInput,
        ctx: &'a ExecCtx,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let query = input
                .args
                .get("query")
                .and_then(|v| v.as_str())
                .map(String::from);
            let max = input
                .args
                .get("max")
                .and_then(|v| v.as_u64())
                .map(|m| (m as u32).clamp(1, SEARCH_MAX_CAP))
                .unwrap_or(5);

            let client: Arc<dyn GmailApi> = Arc::new(
                match observe_local_step(&self.deps, &ctx.profile, self.deps.client(&ctx.profile)) {
                    Ok(c) => c,
                    Err(msg) => return ToolResult::Err(msg),
                },
            );
            let metas = match observe_google_call(
                &self.deps,
                &ctx.profile,
                GoogleApi::Gmail,
                client.list_messages(query.as_deref(), max).await,
            ) {
                Ok(m) => m,
                Err(msg) => return ToolResult::Err(msg),
            };
            // Bounded-concurrent preview fetches: headers + snippet via
            // format=metadata (no body data), at most SEARCH_CONCURRENCY in
            // flight, so one slow message doesn't stall the whole page.
            let ids: Vec<String> = metas
                .iter()
                .take(max as usize)
                .map(|m| m.id.clone())
                .collect();
            let mut rows: Vec<serde_json::Value> = Vec::with_capacity(ids.len());
            for (id, res) in fetch_previews(client, ids).await {
                match res {
                    Ok(m) => rows.push(serde_json::json!({
                        "id": m.id,
                        "from": m.from,
                        "subject": m.subject,
                        "date": m.date,
                        "snippet": m.snippet,
                    })),
                    // One bad message shouldn't sink the search — record it
                    // against its real id so the agent can retry that one.
                    Err(e) => rows.push(serde_json::json!({
                        "id": id,
                        "error": format!("couldn't fetch: {e}"),
                    })),
                }
            }
            ToolResult::Ok(serde_json::json!({ "results": rows, "count": rows.len() }))
        })
    }
}

// ── email_read ──────────────────────────────────────────────────────────────

pub struct EmailReadTool {
    deps: EmailToolDeps,
}

impl EmailReadTool {
    pub fn new(deps: EmailToolDeps) -> Self {
        Self { deps }
    }
}

impl Tool for EmailReadTool {
    fn name(&self) -> &str {
        "email_read"
    }

    fn description(&self) -> &str {
        "Read one email from the connected Gmail inbox by id (from email_search). \
         Returns from/to/subject/date and the plain-text body."
    }

    fn risk(&self) -> RiskClass {
        RiskClass::External
    }

    fn requires(&self) -> &[Capability] {
        &[Capability::Email]
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "id": { "type": "string" } },
            "required": ["id"],
            "additionalProperties": false
        })
    }

    fn destination(&self, _args: &serde_json::Value) -> Option<String> {
        Some("gmail.googleapis.com".to_string())
    }

    fn run<'a>(
        &'a self,
        input: ToolInput,
        ctx: &'a ExecCtx,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let Some(id) = input.args.get("id").and_then(|v| v.as_str()) else {
                return ToolResult::Err("email_read needs an id (from email_search)".into());
            };
            let client = match observe_local_step(
                &self.deps,
                &ctx.profile,
                self.deps.client(&ctx.profile),
            ) {
                Ok(c) => c,
                Err(msg) => return ToolResult::Err(msg),
            };
            match observe_google_call(
                &self.deps,
                &ctx.profile,
                GoogleApi::Gmail,
                client.get_message(id).await,
            ) {
                Ok(m) => {
                    let mut body = m.body_text;
                    if body.chars().count() > READ_BODY_CAP {
                        body = body.chars().take(READ_BODY_CAP).collect::<String>()
                            + "\n[… truncated — the full message is longer]";
                    }
                    ToolResult::Ok(serde_json::json!({
                        "id": m.id,
                        "from": m.from,
                        "to": m.to,
                        "subject": m.subject,
                        "date": m.date,
                        "body": body,
                    }))
                }
                Err(msg) => ToolResult::Err(msg),
            }
        })
    }
}

// ── email_send ──────────────────────────────────────────────────────────────

pub struct EmailSendTool {
    deps: EmailToolDeps,
}

impl EmailSendTool {
    pub fn new(deps: EmailToolDeps) -> Self {
        Self { deps }
    }
}

/// Minimal recipient sanity: one non-empty local part, one `@`, a dot-bearing
/// domain, no header-breaking characters. NOT full RFC 5322 — `build_rfc822`
/// separately refuses header injection; this just catches obvious garbage
/// before the approval dialog shows a nonsense recipient.
fn plausible_address(to: &str) -> bool {
    if to.is_empty() || to.len() > 254 || to.contains(['\r', '\n', ' ']) {
        return false;
    }
    let Some((local, domain)) = to.split_once('@') else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

impl Tool for EmailSendTool {
    fn name(&self) -> &str {
        "email_send"
    }

    fn description(&self) -> &str {
        "Send an email from the connected Gmail account. Args: to, subject, body. \
         IRREVERSIBLE — the user approves each send."
    }

    fn risk(&self) -> RiskClass {
        // Irreversible outward effect: always an explicit Once-only Ask (the
        // grant×risk matrix never lets Dangerous earn a standing rule), and
        // the C2 durability journal records the dispatch.
        RiskClass::Dangerous
    }

    fn requires(&self) -> &[Capability] {
        &[Capability::Email]
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "to": { "type": "string" },
                "subject": { "type": "string" },
                "body": { "type": "string" }
            },
            "required": ["to", "subject", "body"],
            "additionalProperties": false
        })
    }

    fn destination(&self, args: &serde_json::Value) -> Option<String> {
        // The approval dialog shows WHO this send reaches, server-derived
        // from the call args (never client-supplied display text).
        let to = args
            .get("to")
            .and_then(|v| v.as_str())
            .unwrap_or("<missing recipient>");
        Some(format!("{to} (via gmail.googleapis.com)"))
    }

    fn run<'a>(
        &'a self,
        input: ToolInput,
        ctx: &'a ExecCtx,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let get = |k: &str| input.args.get(k).and_then(|v| v.as_str()).map(String::from);
            let (Some(to), Some(subject), Some(body)) = (get("to"), get("subject"), get("body"))
            else {
                return ToolResult::Err("email_send needs to, subject and body".into());
            };
            if !plausible_address(&to) {
                return ToolResult::Err(format!(
                    "\"{to}\" doesn't look like a valid email address — refusing to send"
                ));
            }
            // build_rfc822 refuses header injection (CR/LF in to/subject).
            let raw = match build_rfc822(&to, &subject, &body) {
                Ok(r) => r,
                Err(e) => return ToolResult::Err(e.to_string()),
            };
            let client = match observe_local_step(
                &self.deps,
                &ctx.profile,
                self.deps.client(&ctx.profile),
            ) {
                Ok(c) => c,
                Err(msg) => return ToolResult::Err(msg),
            };
            match observe_google_call(
                &self.deps,
                &ctx.profile,
                GoogleApi::Gmail,
                client.send(&raw).await,
            ) {
                Ok(id) => ToolResult::Ok(serde_json::json!({
                    "sent": true,
                    "message_id": id,
                    "to": to,
                })),
                Err(msg) => ToolResult::Err(format!("send failed: {msg}")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_sanity_rejects_garbage_and_injection_shapes() {
        for bad in [
            "",
            "nope",
            "a@b",
            "a b@c.d",
            "x@.com",
            "x@com.",
            "a@b.c\r\nBcc: e@f.g",
        ] {
            assert!(!plausible_address(bad), "{bad:?} should be rejected");
        }
        for good in ["a@b.co", "first.last+tag@sub.domain.org"] {
            assert!(plausible_address(good), "{good:?} should pass");
        }
    }

    fn fresh_connection_state() -> crate::ipc::GoogleConnection {
        Arc::new(crate::email::connection_state::GoogleConnectionState::new())
    }

    #[test]
    fn risk_classes_match_the_trust_posture() {
        let deps = EmailToolDeps::new(
            Arc::new(crate::secrets::MemoryProviderSecretStore::default()),
            Arc::new(NoopEndpoint),
            fresh_connection_state(),
        );
        assert_eq!(
            EmailSearchTool::new(deps.clone()).risk(),
            RiskClass::External
        );
        assert_eq!(EmailReadTool::new(deps.clone()).risk(), RiskClass::External);
        assert_eq!(
            EmailSendTool::new(deps.clone()).risk(),
            RiskClass::Dangerous
        );
        // External ⇒ egresses_offbox folds into the privacy gate (F2).
        assert!(EmailSearchTool::new(deps.clone()).egresses_offbox());
        // The send dialog names the recipient.
        let d = EmailSendTool::new(deps)
            .destination(&serde_json::json!({ "to": "a@b.co" }))
            .unwrap();
        assert!(d.contains("a@b.co"));
    }

    struct NoopEndpoint;
    impl TokenEndpoint for NoopEndpoint {
        fn post_form(
            &self,
            _form: Vec<(String, String)>,
        ) -> crate::email::BoxFuture<'_, anyhow::Result<(u16, String)>> {
            Box::pin(async { anyhow::bail!("no network in unit tests") })
        }
    }

    #[tokio::test]
    async fn unconfigured_profile_gets_a_setup_pointing_error_not_a_panic() {
        let deps = EmailToolDeps::new(
            Arc::new(crate::secrets::MemoryProviderSecretStore::default()),
            Arc::new(NoopEndpoint),
            fresh_connection_state(),
        );
        let ctx = ExecCtx {
            profile: "personal".into(),
            ..Default::default()
        };
        let out = EmailSearchTool::new(deps)
            .run(ToolInput::new(serde_json::json!({})), &ctx)
            .await;
        match out {
            ToolResult::Err(e) => assert!(e.contains("Settings → Email"), "got: {e}"),
            other => panic!("expected a setup-pointing error, got {other:?}"),
        }
    }

    /// A token endpoint that always answers with a dead grant (Google's
    /// `invalid_grant`), so `KeychainTokenProvider::refresh_now` bails with the
    /// typed `NeedsReconnect` — the exact failure a revoked/expired
    /// Testing-mode refresh token produces in production.
    struct DeadGrantEndpoint;
    impl TokenEndpoint for DeadGrantEndpoint {
        fn post_form(
            &self,
            _form: Vec<(String, String)>,
        ) -> crate::email::BoxFuture<'_, anyhow::Result<(u16, String)>> {
            Box::pin(async {
                Ok((
                    400,
                    r#"{"error":"invalid_grant","error_description":"revoked"}"#.to_string(),
                ))
            })
        }
    }

    /// A mailbox where one id ("slow") takes far longer than the rest, and
    /// which records both the completion order and the high-water mark of
    /// concurrent in-flight fetches.
    struct PacedMailbox {
        slow_id: String,
        slow: std::time::Duration,
        fast: std::time::Duration,
        completed: Mutex<Vec<String>>,
        in_flight: Mutex<usize>,
        peak_in_flight: Mutex<usize>,
        fail_id: Option<String>,
    }

    impl PacedMailbox {
        fn new(fail_id: Option<&str>) -> Self {
            Self {
                slow_id: "slow".into(),
                slow: std::time::Duration::from_millis(300),
                fast: std::time::Duration::from_millis(40),
                completed: Mutex::new(Vec::new()),
                in_flight: Mutex::new(0),
                peak_in_flight: Mutex::new(0),
                fail_id: fail_id.map(String::from),
            }
        }
    }

    impl crate::email::gmail::GmailApi for PacedMailbox {
        fn list_messages<'a>(
            &'a self,
            _query: Option<&'a str>,
            _max: u32,
        ) -> crate::email::BoxFuture<'a, anyhow::Result<Vec<crate::email::gmail::MessageMeta>>>
        {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn get_message<'a>(
            &'a self,
            _id: &'a str,
        ) -> crate::email::BoxFuture<'a, anyhow::Result<crate::email::gmail::EmailMessage>>
        {
            Box::pin(async { anyhow::bail!("not used by this test") })
        }

        fn get_message_metadata<'a>(
            &'a self,
            id: &'a str,
        ) -> crate::email::BoxFuture<'a, anyhow::Result<crate::email::gmail::EmailMessage>>
        {
            Box::pin(async move {
                {
                    let mut n = self.in_flight.lock();
                    *n += 1;
                    let mut peak = self.peak_in_flight.lock();
                    *peak = (*peak).max(*n);
                }
                let wait = if id == self.slow_id {
                    self.slow
                } else {
                    self.fast
                };
                tokio::time::sleep(wait).await;
                *self.in_flight.lock() -= 1;
                self.completed.lock().push(id.to_string());
                if self.fail_id.as_deref() == Some(id) {
                    anyhow::bail!("Gmail API HTTP 404");
                }
                Ok(crate::email::gmail::EmailMessage {
                    id: id.to_string(),
                    from: "a@b.co".into(),
                    to: "me@b.co".into(),
                    subject: format!("subject for {id}"),
                    date: "Tue, 1 Jul 2025 00:00:00 +0000".into(),
                    snippet: "snip".into(),
                    body_text: String::new(),
                })
            })
        }

        fn send<'a>(
            &'a self,
            _raw: &'a str,
        ) -> crate::email::BoxFuture<'a, anyhow::Result<String>> {
            Box::pin(async { anyhow::bail!("not used by this test") })
        }

        fn get_profile<'a>(&'a self) -> crate::email::BoxFuture<'a, anyhow::Result<String>> {
            Box::pin(async { anyhow::bail!("not used by this test") })
        }
    }

    /// The finding this pins: previews used to be fetched strictly one after
    /// another, so a single slow message serialized the whole search. Now up
    /// to SEARCH_CONCURRENCY run at once — the fast ones finish while the slow
    /// one is still in flight — and the concurrency stays bounded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_slow_preview_does_not_stall_the_others() {
        let mailbox = Arc::new(PacedMailbox::new(None));
        let ids: Vec<String> = vec![
            "slow".into(),
            "f1".into(),
            "f2".into(),
            "f3".into(),
            "f4".into(),
        ];

        let started = std::time::Instant::now();
        let out = fetch_previews(mailbox.clone(), ids.clone()).await;
        let elapsed = started.elapsed();

        // Results keep input order and each carries its own id.
        assert_eq!(
            out.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>(),
            ids
        );
        assert!(out.iter().all(|(_, r)| r.is_ok()));

        // The four fast fetches all completed before the slow one did.
        let order = mailbox.completed.lock().clone();
        assert_eq!(
            order.last().map(String::as_str),
            Some("slow"),
            "order was {order:?}"
        );

        // Concurrency stayed inside the semaphore's budget. Pinned to the
        // literal 3, NOT to SEARCH_CONCURRENCY: comparing the observed peak
        // against the constant that produced it is self-referential — it would
        // keep passing if the bound were raised, which is exactly the change
        // this assertion exists to catch.
        assert_eq!(*mailbox.peak_in_flight.lock(), 3);

        // Serial would be 300ms + 4×40ms = 460ms; bounded-concurrent is about
        // the slow fetch's own latency. Generous bound to stay non-flaky.
        assert!(
            elapsed < std::time::Duration::from_millis(430),
            "took {elapsed:?} — that's serial, not concurrent"
        );
    }

    /// The finding this pins: the failed-preview row reported the literal id
    /// `"?"`, so the agent could never retry or name the message that failed.
    #[tokio::test]
    async fn a_failed_preview_row_keeps_its_real_message_id() {
        let mailbox = Arc::new(PacedMailbox::new(Some("f2")));
        let ids: Vec<String> = vec!["f1".into(), "f2".into(), "f3".into()];
        let out = fetch_previews(mailbox, ids).await;
        let (id, res) = &out[1];
        assert_eq!(id, "f2");
        assert!(res.is_err(), "f2 was scripted to fail");
        // The neighbours are unaffected and keep their own ids.
        assert_eq!(out[0].0, "f1");
        assert!(out[0].1.is_ok());
        assert_eq!(out[2].0, "f3");
        assert!(out[2].1.is_ok());
    }

    /// The finding this pins: only the screen IPC path used to flip
    /// `needs_reconnect`, so an agent-only dead-token failure never lit the
    /// reconnect banner. Now the tool path inserts into the SAME shared set.
    #[tokio::test]
    async fn needs_reconnect_marker_flips_the_shared_flag() {
        let secrets = crate::secrets::MemoryProviderSecretStore::default();
        secrets
            .set(
                crate::email::SECRET_GMAIL_CLIENT_ID,
                "id-1.apps.googleusercontent.com",
            )
            .unwrap();
        secrets
            .set(crate::email::SECRET_GMAIL_CLIENT_SECRET, "shhh")
            .unwrap();
        secrets
            .set(
                &crate::email::secret_gmail_refresh_token("personal"),
                "rt-dead",
            )
            .unwrap();

        let shared = fresh_connection_state();
        let deps = EmailToolDeps::new(
            Arc::new(secrets),
            Arc::new(DeadGrantEndpoint),
            Arc::clone(&shared),
        );
        let ctx = ExecCtx {
            profile: "personal".into(),
            ..Default::default()
        };

        let out = EmailSearchTool::new(deps)
            .run(ToolInput::new(serde_json::json!({})), &ctx)
            .await;
        match out {
            ToolResult::Err(e) => assert!(e.contains("Reconnect in Settings"), "got: {e}"),
            other => panic!("expected a NeedsReconnect error, got {other:?}"),
        }
        assert!(
            shared.needs_reconnect("personal"),
            "the agent tool path must flip the SHARED reconnect state, not a private one"
        );
    }

    /// The finding this pins: this file DUPLICATED the screen path's
    /// recording logic, so the agent-tool path (email_search/email_read/
    /// email_send and, through `productivity.rs`, every Calendar/Tasks tool)
    /// had the identical blind spot the screen path had — a disabled-API 403
    /// recorded nothing, and the agent could only hand the user raw
    /// `Google API HTTP 403` text.
    ///
    /// There is now ONE implementation, called with the TYPED error; this
    /// asserts the agent path reaches it, in both directions of the separation
    /// (neither 403 may set the other's state) and in both directions of the
    /// disabled state (a success is what clears it).
    #[test]
    fn the_tool_path_records_both_403_states_and_never_confuses_them() {
        use crate::email::api_error::google_api_error;
        const CONSOLE: &str =
            "https://console.developers.google.com/apis/api/calendar-json.googleapis.com/overview?project=3";

        let shared = fresh_connection_state();
        let deps = EmailToolDeps::new(
            Arc::new(crate::secrets::MemoryProviderSecretStore::default()),
            Arc::new(NoopEndpoint),
            Arc::clone(&shared),
        );

        let scope: Result<(), String> = observe_google_call(
            &deps,
            "personal",
            GoogleApi::Calendar,
            Err(google_api_error(
                GoogleApi::Calendar,
                403,
                r#"{"error":{"errors":[{"reason":"insufficientPermissions"}],"code":403}}"#,
                "snip",
            )),
        );
        assert!(scope.is_err(), "the tool still reports the failure");
        assert!(shared.needs_reconnect("personal"));
        assert_eq!(
            shared.disabled_apis("personal"),
            None,
            "a scope-short grant is not a disabled API"
        );

        let shared = fresh_connection_state();
        let deps = EmailToolDeps::new(
            Arc::new(crate::secrets::MemoryProviderSecretStore::default()),
            Arc::new(NoopEndpoint),
            Arc::clone(&shared),
        );
        let _: Result<(), String> = observe_google_call(
            &deps,
            "work",
            GoogleApi::Calendar,
            Err(google_api_error(
                GoogleApi::Calendar,
                403,
                &format!(
                    r#"{{"error":{{"code":403,"status":"PERMISSION_DENIED","details":[
                {{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"SERVICE_DISABLED"}},
                {{"@type":"type.googleapis.com/google.rpc.Help","links":[
                  {{"url":"{CONSOLE}"}}]}}]}}}}"#
                ),
                "snip",
            )),
        );
        assert!(
            !shared.needs_reconnect("work"),
            "the agent path must not send the user into a reconnect loop either"
        );
        assert_eq!(
            shared.disabled_apis("work"),
            Some(crate::email::connection_state::GoogleApiDisabled {
                apis: vec![crate::email::connection_state::DisabledApi {
                    id: "calendar",
                    label: "Google Calendar",
                    console_url: Some(CONSOLE.to_string()),
                }],
            })
        );

        // A later Calendar call that WORKS is the evidence that clears it, on
        // the agent path exactly as on the screen path.
        let _ = observe_google_call(&deps, "work", GoogleApi::Calendar, Ok(()));
        assert_eq!(shared.disabled_apis("work"), None);
    }

    /// The finding this pins: all three tools handed the CLIENT BUILD to
    /// `observe_google_call`, and `EmailToolDeps::client` is a pure
    /// constructor — `GmailClient::new` sends nothing. Its `Ok` was therefore
    /// recorded as a successful Gmail call, so merely dispatching a tool wiped
    /// a real "Gmail is switched off" state (and darkened its banner) with no
    /// evidence whatsoever. Only a completed API call may count as proof.
    ///
    /// Driven through the real `run` of each tool: the profile is
    /// unconfigured, so the client builds fine and the first call that
    /// actually reaches for a token fails with an UNCLASSIFIED error — which
    /// records nothing. Anything that changed the state here can only have
    /// come from the build.
    #[tokio::test]
    async fn merely_building_the_client_is_not_proof_that_gmail_is_switched_on() {
        use crate::email::api_error::google_api_error;
        const DISABLED: &str = r#"{"error":{"code":403,"status":"PERMISSION_DENIED","details":[
            {"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"SERVICE_DISABLED"}]}}"#;

        let ctx = ExecCtx {
            profile: "personal".into(),
            ..Default::default()
        };
        for (name, args) in [
            ("email_search", serde_json::json!({})),
            ("email_read", serde_json::json!({ "id": "m1" })),
            (
                "email_send",
                serde_json::json!({ "to": "a@b.co", "subject": "s", "body": "b" }),
            ),
        ] {
            let shared = fresh_connection_state();
            shared.observe_failure(
                "personal",
                &google_api_error(GoogleApi::Gmail, 403, DISABLED, "snip"),
            );
            assert!(
                shared.disabled_apis("personal").is_some(),
                "{name}: precondition — Gmail starts out recorded as switched off"
            );

            let deps = EmailToolDeps::new(
                Arc::new(crate::secrets::MemoryProviderSecretStore::default()),
                Arc::new(NoopEndpoint),
                Arc::clone(&shared),
            );
            let input = ToolInput::new(args);
            let out = match name {
                "email_search" => EmailSearchTool::new(deps).run(input, &ctx).await,
                "email_read" => EmailReadTool::new(deps).run(input, &ctx).await,
                _ => EmailSendTool::new(deps).run(input, &ctx).await,
            };
            assert!(
                matches!(out, ToolResult::Err(_)),
                "{name}: nothing could reach Gmail, so the tool must report a failure — got {out:?}"
            );
            assert!(
                shared.disabled_apis("personal").is_some(),
                "{name}: a dispatch that never reached Google cleared the disabled-API state"
            );
        }
    }
}
