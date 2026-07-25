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

use std::collections::HashSet;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::email::gmail::{build_rfc822, GmailApi, GmailClient, ReqwestGmailHttp};
use crate::email::google::GoogleClient;
use crate::email::oauth::TokenEndpoint;
use crate::email::token_provider::{KeychainTokenProvider, NEEDS_RECONNECT_MARKER};
use crate::secrets::ProviderSecretStore;
use crate::tools::{Capability, ExecCtx, RiskClass, Tool, ToolInput, ToolResult};

/// How many messages `email_search` will fetch bodies for at most — each is
/// its own Gmail API call, so this bounds the burst.
const SEARCH_MAX_CAP: u32 = 10;

/// `email_read` body cap (chars) — a huge thread can't flood the context.
const READ_BODY_CAP: usize = 40_000;

/// Shared constructor context for the three tools.
#[derive(Clone)]
pub struct EmailToolDeps {
    pub secrets: Arc<dyn ProviderSecretStore>,
    pub endpoint: Arc<dyn TokenEndpoint>,
    /// Profiles whose last Gmail call failed with a dead grant — the SAME
    /// `Arc` as `ipc::EmailRuntime`'s set (see that type's doc comment).
    /// Without this being shared, an agent-only dead-token failure would
    /// never light the screen's reconnect banner, since only the screen IPC
    /// path (`ipc::mod::note_reconnect_if_needed`) used to touch it.
    pub needs_reconnect: Arc<Mutex<HashSet<String>>>,
}

impl EmailToolDeps {
    /// A per-call Gmail client for `profile`. Cheap: two small structs; the
    /// keychain reads happen lazily inside the token provider.
    fn token_provider(&self, profile: &str) -> Box<KeychainTokenProvider> {
        Box::new(KeychainTokenProvider::new(
            profile,
            Arc::clone(&self.secrets),
            Arc::clone(&self.endpoint),
        ))
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
        GoogleClient::new(self.token_provider(profile))
    }
}

/// If `err` carries [`NEEDS_RECONNECT_MARKER`], flip the shared reconnect
/// flag for `profile` — mirrors `ipc::mod::note_reconnect_if_needed` so the
/// agent tool path lights the same banner the screen IPC path does.
pub(crate) fn note_reconnect_if_needed(deps: &EmailToolDeps, profile: &str, err: &str) {
    if err.contains(NEEDS_RECONNECT_MARKER) {
        deps.needs_reconnect.lock().insert(profile.to_string());
    }
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
            let query = input.args.get("query").and_then(|v| v.as_str()).map(String::from);
            let max = input
                .args
                .get("max")
                .and_then(|v| v.as_u64())
                .map(|m| (m as u32).clamp(1, SEARCH_MAX_CAP))
                .unwrap_or(5);

            let client = match self.deps.client(&ctx.profile) {
                Ok(c) => c,
                Err(e) => {
                    let msg = e.to_string();
                    note_reconnect_if_needed(&self.deps, &ctx.profile, &msg);
                    return ToolResult::Err(msg);
                }
            };
            let metas = match client.list_messages(query.as_deref(), max).await {
                Ok(m) => m,
                Err(e) => {
                    let msg = e.to_string();
                    note_reconnect_if_needed(&self.deps, &ctx.profile, &msg);
                    return ToolResult::Err(msg);
                }
            };
            // Fetch each hit for headers + snippet. Bounded by SEARCH_MAX_CAP.
            let mut rows = Vec::new();
            for meta in metas.iter().take(max as usize) {
                match client.get_message(&meta.id).await {
                    Ok(m) => rows.push(serde_json::json!({
                        "id": m.id,
                        "from": m.from,
                        "subject": m.subject,
                        "date": m.date,
                        "snippet": m.snippet,
                    })),
                    // One bad message shouldn't sink the search — record it.
                    Err(e) => rows.push(serde_json::json!({
                        "id": meta.id,
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
            let client = match self.deps.client(&ctx.profile) {
                Ok(c) => c,
                Err(e) => {
                    let msg = e.to_string();
                    note_reconnect_if_needed(&self.deps, &ctx.profile, &msg);
                    return ToolResult::Err(msg);
                }
            };
            match client.get_message(id).await {
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
                Err(e) => {
                    let msg = e.to_string();
                    note_reconnect_if_needed(&self.deps, &ctx.profile, &msg);
                    ToolResult::Err(msg)
                }
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
        let to = args.get("to").and_then(|v| v.as_str()).unwrap_or("<missing recipient>");
        Some(format!("{to} (via gmail.googleapis.com)"))
    }

    fn run<'a>(
        &'a self,
        input: ToolInput,
        ctx: &'a ExecCtx,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let get = |k: &str| input.args.get(k).and_then(|v| v.as_str()).map(String::from);
            let (Some(to), Some(subject), Some(body)) =
                (get("to"), get("subject"), get("body"))
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
            let client = match self.deps.client(&ctx.profile) {
                Ok(c) => c,
                Err(e) => {
                    let msg = e.to_string();
                    note_reconnect_if_needed(&self.deps, &ctx.profile, &msg);
                    return ToolResult::Err(msg);
                }
            };
            match client.send(&raw).await {
                Ok(id) => ToolResult::Ok(serde_json::json!({
                    "sent": true,
                    "message_id": id,
                    "to": to,
                })),
                Err(e) => {
                    let msg = format!("send failed: {e}");
                    note_reconnect_if_needed(&self.deps, &ctx.profile, &msg);
                    ToolResult::Err(msg)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_sanity_rejects_garbage_and_injection_shapes() {
        for bad in ["", "nope", "a@b", "a b@c.d", "x@.com", "x@com.", "a@b.c\r\nBcc: e@f.g"] {
            assert!(!plausible_address(bad), "{bad:?} should be rejected");
        }
        for good in ["a@b.co", "first.last+tag@sub.domain.org"] {
            assert!(plausible_address(good), "{good:?} should pass");
        }
    }

    fn empty_reconnect_set() -> Arc<Mutex<HashSet<String>>> {
        Arc::new(Mutex::new(HashSet::new()))
    }

    #[test]
    fn risk_classes_match_the_trust_posture() {
        let deps = EmailToolDeps {
            secrets: Arc::new(crate::secrets::MemoryProviderSecretStore::default()),
            endpoint: Arc::new(NoopEndpoint),
            needs_reconnect: empty_reconnect_set(),
        };
        assert_eq!(EmailSearchTool::new(deps.clone()).risk(), RiskClass::External);
        assert_eq!(EmailReadTool::new(deps.clone()).risk(), RiskClass::External);
        assert_eq!(EmailSendTool::new(deps.clone()).risk(), RiskClass::Dangerous);
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
        let deps = EmailToolDeps {
            secrets: Arc::new(crate::secrets::MemoryProviderSecretStore::default()),
            endpoint: Arc::new(NoopEndpoint),
            needs_reconnect: empty_reconnect_set(),
        };
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
    /// `invalid_grant`), so `KeychainTokenProvider::refresh_now` bails with
    /// `NEEDS_RECONNECT_MARKER` — the exact shape a revoked/expired
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

    /// The finding this pins: only the screen IPC path used to flip
    /// `needs_reconnect`, so an agent-only dead-token failure never lit the
    /// reconnect banner. Now the tool path inserts into the SAME shared set.
    #[tokio::test]
    async fn needs_reconnect_marker_flips_the_shared_flag() {
        let secrets = crate::secrets::MemoryProviderSecretStore::default();
        secrets
            .set(crate::email::SECRET_GMAIL_CLIENT_ID, "id-1.apps.googleusercontent.com")
            .unwrap();
        secrets.set(crate::email::SECRET_GMAIL_CLIENT_SECRET, "shhh").unwrap();
        secrets
            .set(&crate::email::secret_gmail_refresh_token("personal"), "rt-dead")
            .unwrap();

        let shared = empty_reconnect_set();
        let deps = EmailToolDeps {
            secrets: Arc::new(secrets),
            endpoint: Arc::new(DeadGrantEndpoint),
            needs_reconnect: Arc::clone(&shared),
        };
        let ctx = ExecCtx {
            profile: "personal".into(),
            ..Default::default()
        };

        let out = EmailSearchTool::new(deps)
            .run(ToolInput::new(serde_json::json!({})), &ctx)
            .await;
        match out {
            ToolResult::Err(e) => assert!(e.contains(NEEDS_RECONNECT_MARKER), "got: {e}"),
            other => panic!("expected a NeedsReconnect error, got {other:?}"),
        }
        assert!(
            shared.lock().contains("personal"),
            "the agent tool path must flip the shared reconnect flag, not a private one"
        );
    }
}
