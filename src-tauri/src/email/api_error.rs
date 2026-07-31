//! Google API failure classification — the recovery seam for non-2xx
//! responses from Gmail / Calendar / Tasks.
//!
//! ## Why this exists
//!
//! Before this module, EVERY non-2xx from a Google REST call bailed with a
//! plain `"… API HTTP {status}: {body}"` string, and the only recovery signal
//! in the app was a substring match on a marker embedded by the OAuth
//! *token-refresh* classifier — so a real Google `403` could never light any
//! banner no matter how many callers checked for it. The Planner just failed
//! forever with raw `Google API HTTP 403` text and no route out.
//!
//! ## The two recoverable 403s (and why they must stay separate)
//!
//! - **Scope insufficient** (`insufficientPermissions` /
//!   `ACCESS_TOKEN_SCOPE_INSUFFICIENT`) — the stored grant predates a scope
//!   the app now needs (e.g. a Gmail-only grant hitting Calendar). A
//!   reconnect genuinely fixes this: `begin_auth` re-consents with all four
//!   scopes and `prompt=consent`. → the EXISTING `needs_reconnect` state.
//! - **API not enabled** (`accessNotConfigured` / `SERVICE_DISABLED`) — the
//!   user's own Google Cloud project has the API switched off. Reconnecting
//!   can NEVER fix this; offering the reconnect button here would loop the
//!   user through the OAuth dance forever. → a DISTINCT state carrying the
//!   console activation link.
//!
//! Anything else — including an unmatched 403 — falls through to exactly the
//! plain-string behaviour that shipped before. Silently reclassifying an
//! unknown 403 as either specific case would be a guess dressed as a fact.
//!
//! ## The classification is DATA, not text
//!
//! [`google_api_error`] returns an `anyhow::Error` whose payload is a typed
//! [`GoogleApiError`] carrying the [`GoogleApiFailure`] and the
//! [`GoogleApi`] it happened on. Everything downstream that CHANGES STATE
//! (see [`crate::email::connection_state`]) downcasts to that value; nothing
//! re-derives a decision by scanning the message.
//!
//! That is not a style preference. The error message necessarily contains an
//! excerpt of the response body — untrusted bytes from the network. An
//! earlier revision of this file encoded the verdict as markers
//! (`[google:api_not_enabled]`, `[google:enable_url=…]`) inside that same
//! string and re-parsed them downstream, which meant a body excerpt could
//! *state its own verdict*: it bypassed the URL gate below end-to-end and
//! could promote an unknown 403 into the API-disabled state, which the rules
//! above explicitly forbid. Those markers are gone. Body excerpts are also
//! run through [`scrub_state_markers`] on the way into the message, so no
//! marker-shaped text can survive in a place a future parser might look.
//!
//! ## Structure, not substrings
//!
//! Google is mid-migration between two error envelopes and both are live:
//!
//! - legacy: `{"error":{"errors":[{"reason":"accessNotConfigured", …}], …}}`
//! - modern: `{"error":{"status":"PERMISSION_DENIED","details":[
//!   {"@type":"…/google.rpc.ErrorInfo","reason":"SERVICE_DISABLED", …},
//!   {"@type":"…/google.rpc.Help","links":[{"url":"https://…"}]}]}}`
//!
//! Both are parsed into typed shapes and matched on whole `reason` TOKENS. A
//! bare `body.contains("SERVICE_DISABLED")` would also fire on the word
//! appearing inside a mail subject echoed back in an error message.

use super::token_provider::scrub_state_markers;

/// Which Google API a call was for. Carried on the failure so the recovery
/// state is per-API rather than a single per-profile flag: the user enables
/// APIs one at a time in the console, and "Tasks is off" must not be cleared
/// by a Gmail call that worked (nor keep claiming Tasks is off after a Tasks
/// call succeeds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GoogleApi {
    Gmail,
    Calendar,
    Tasks,
}

impl GoogleApi {
    /// Every API this app talks to — the "all of them" set for a clear.
    pub const ALL: [GoogleApi; 3] = [GoogleApi::Gmail, GoogleApi::Calendar, GoogleApi::Tasks];

    /// Stable id on the IPC wire (the UI names the APIs it is able to
    /// re-test, so a re-check on one screen can't wipe another screen's
    /// state).
    pub const fn wire(self) -> &'static str {
        match self {
            GoogleApi::Gmail => "gmail",
            GoogleApi::Calendar => "calendar",
            GoogleApi::Tasks => "tasks",
        }
    }

    /// What a human calls it (banner copy).
    pub const fn label(self) -> &'static str {
        match self {
            GoogleApi::Gmail => "Gmail",
            GoogleApi::Calendar => "Google Calendar",
            GoogleApi::Tasks => "Google Tasks",
        }
    }

    /// How the API names itself at the head of an error message.
    pub const fn error_label(self) -> &'static str {
        match self {
            GoogleApi::Gmail => "Gmail API",
            GoogleApi::Calendar => "Google Calendar API",
            GoogleApi::Tasks => "Google Tasks API",
        }
    }

    /// Parse a wire id. `None` for anything unknown — an unrecognised name is
    /// refused at the IPC edge rather than silently ignored (which would look
    /// like a successful clear that cleared nothing).
    pub fn from_wire(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|api| api.wire() == value)
    }
}

/// What a non-2xx Google API response means for RECOVERY. The variant is the
/// product behaviour, exactly like `oauth::RefreshError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoogleApiFailure {
    /// The access token's grant doesn't cover this API. Reconnecting
    /// re-consents with the full scope set, so it genuinely fixes this.
    ScopeInsufficient,
    /// The API is disabled in the user's own Google Cloud project. Only the
    /// user, in the console, can fix it — `console_url` is Google's own
    /// activation link when the response carried a usable one.
    ApiNotEnabled { console_url: Option<String> },
    /// Everything else, including an unmatched 403: the caller keeps its
    /// existing plain-string behaviour.
    Other,
}

/// The error a Google REST caller bails with.
///
/// `failure` is the verdict as DATA — the single source of truth for every
/// state decision downstream. `message` is for humans and logs only; it
/// contains a bounded excerpt of an untrusted response body and must never be
/// parsed to recover the verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoogleApiError {
    /// The API the failing call was for.
    pub api: GoogleApi,
    /// The HTTP status Google returned.
    pub status: u16,
    /// The classification. Read this, never the text.
    pub failure: GoogleApiFailure,
    /// Display text (body excerpt included, markers scrubbed).
    message: String,
}

impl std::fmt::Display for GoogleApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GoogleApiError {}

// ---------------------------------------------------------------------------
// Wire shapes. Every field is optional/defaulted: these bodies come off the
// network and a missing or oddly-typed field must degrade to `Other`, never
// panic and never guess.
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize, Default)]
struct ErrorEnvelope {
    #[serde(default)]
    error: Option<ErrorBody>,
}

#[derive(serde::Deserialize, Default)]
struct ErrorBody {
    /// Legacy envelope: one entry per underlying error.
    #[serde(default)]
    errors: Vec<LegacyError>,
    /// Modern envelope: the canonical status name (`PERMISSION_DENIED`, …).
    /// Carried as a reason candidate because the spec names it, though in
    /// practice the discriminating token lives in `details[].reason`.
    #[serde(default)]
    status: Option<String>,
    /// Modern envelope: typed detail payloads (`google.rpc.ErrorInfo`,
    /// `google.rpc.Help`, …).
    #[serde(default)]
    details: Vec<ErrorDetail>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct LegacyError {
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    message: Option<String>,
    /// Legacy structural home of the activation link.
    #[serde(default, rename = "extendedHelp")]
    extended_help: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct ErrorDetail {
    #[serde(default, rename = "@type")]
    type_url: Option<String>,
    /// Present on `google.rpc.ErrorInfo`.
    #[serde(default)]
    reason: Option<String>,
    /// Present on `google.rpc.Help`.
    #[serde(default)]
    links: Vec<HelpLink>,
}

#[derive(serde::Deserialize, Default)]
struct HelpLink {
    #[serde(default)]
    url: Option<String>,
}

/// Reason tokens that mean "the API is off in the user's Cloud project".
const API_NOT_ENABLED_REASONS: [&str; 2] = ["accessNotConfigured", "SERVICE_DISABLED"];
/// Reason tokens that mean "this grant is missing a scope".
const SCOPE_INSUFFICIENT_REASONS: [&str; 2] =
    ["insufficientPermissions", "ACCESS_TOKEN_SCOPE_INSUFFICIENT"];

/// Classify a non-2xx Google API response. Pure — the honesty seam, mirroring
/// `oauth::classify_refresh_failure`.
///
/// Only `403` is classified. A 401 is already handled upstream by the
/// refresh-and-retry-once policy, and every other status is genuinely
/// "something else went wrong". Narrow on purpose: this function's job is to
/// find the two RECOVERABLE cases, not to reinterpret the whole status space.
pub fn classify_google_api_failure(status: u16, body: &str) -> GoogleApiFailure {
    if status != 403 {
        return GoogleApiFailure::Other;
    }
    let Some(error) = serde_json::from_str::<ErrorEnvelope>(body)
        .unwrap_or_default()
        .error
    else {
        return GoogleApiFailure::Other;
    };

    let reasons = reason_tokens(&error);
    let matches = |set: &[&str]| {
        reasons
            .iter()
            .any(|r| set.iter().any(|want| r.eq_ignore_ascii_case(want)))
    };

    // API-not-enabled is checked FIRST. If a body somehow carried both
    // tokens, the disabled API is the one a reconnect cannot fix, and
    // sending the user into a reconnect loop is the worse failure.
    if matches(&API_NOT_ENABLED_REASONS) {
        return GoogleApiFailure::ApiNotEnabled {
            console_url: console_url(&error),
        };
    }
    if matches(&SCOPE_INSUFFICIENT_REASONS) {
        return GoogleApiFailure::ScopeInsufficient;
    }
    GoogleApiFailure::Other
}

/// Every reason token the body offers, from both envelopes.
fn reason_tokens(error: &ErrorBody) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for e in &error.errors {
        if let Some(reason) = e.reason.as_deref() {
            out.push(reason);
        }
    }
    for detail in &error.details {
        // `reason` is an ErrorInfo field. Accept it when the entry says it is
        // an ErrorInfo, or when `@type` is absent — still structural (the
        // named field inside `details[]`), never a scan of the raw body.
        let is_error_info = detail
            .type_url
            .as_deref()
            .is_none_or(|t| t.ends_with("google.rpc.ErrorInfo"));
        if is_error_info {
            if let Some(reason) = detail.reason.as_deref() {
                out.push(reason);
            }
        }
    }
    if let Some(status) = error.status.as_deref() {
        out.push(status);
    }
    out
}

/// The console activation URL, preferring the STRUCTURAL sources over the
/// one embedded in Google's prose.
fn console_url(error: &ErrorBody) -> Option<String> {
    // 1. Legacy structural: `errors[].extendedHelp`.
    for e in &error.errors {
        if let Some(url) = e.extended_help.as_deref().and_then(sanitize_console_url) {
            return Some(url);
        }
    }
    // 2. Modern structural: a `google.rpc.Help` detail's `links[].url`.
    for detail in &error.details {
        let is_help = detail
            .type_url
            .as_deref()
            .is_some_and(|t| t.ends_with("google.rpc.Help"));
        if !is_help {
            continue;
        }
        for link in &detail.links {
            if let Some(url) = link.url.as_deref().and_then(sanitize_console_url) {
                return Some(url);
            }
        }
    }
    // 3. Legacy prose: "… Enable it by visiting https://console… then retry."
    for message in error
        .message
        .iter()
        .map(String::as_str)
        .chain(error.errors.iter().filter_map(|e| e.message.as_deref()))
    {
        if let Some(url) = first_console_url_in(message) {
            return Some(url);
        }
    }
    None
}

/// Longest a console link may be before we decline to surface it. Google's
/// real ones are ~110 chars; this only stops an absurd body from bloating an
/// error string.
const MAX_CONSOLE_URL_LEN: usize = 400;

/// The ONLY hosts an API-activation link is allowed to live on.
///
/// Deliberately exact hosts, not "some google.com domain": Google's real
/// activation links are `console.developers.google.com` (legacy
/// `extendedHelp`) and `console.cloud.google.com` (the modern console), and
/// those two are all this feature needs. A `*.google.com` rule would have let
/// an error body point the button at any Google-hosted surface that can
/// redirect, host user content, or take a `continue=` parameter — a much
/// wider target than "the page that switches this API on".
const CONSOLE_HOSTS: [&str; 2] = ["console.developers.google.com", "console.cloud.google.com"];

/// Accept a URL only if it is one we are willing to render as a clickable
/// link: `https`, one of [`CONSOLE_HOSTS`], no userinfo, the default port, and
/// bounded in length.
///
/// This body is UNTRUSTED input (it is whatever the endpoint we contacted
/// sent back). Handing an unvalidated URL to the UI would turn a Google 403
/// into an arbitrary-link surface, so the check is a real gate, not a
/// formality — and the value it returns is the only path a console URL has to
/// the UI, so the gate cannot be walked around.
///
/// USERINFO is refused because the host check alone does not stop it: in
/// `https://evil.test@console.cloud.google.com/apis` the host IS the allowed
/// one, and the URL would have been returned verbatim for the banner to render
/// — a link whose visible head reads like a different site is the classic
/// phishing display trick, and a real activation link never carries
/// credentials.
///
/// A NON-DEFAULT PORT is refused for the same reason it is never needed: the
/// real console answers on 443, and `console.cloud.google.com:8080` would only
/// be useful for pointing the button at something other than the console the
/// user thinks they are opening.
pub fn sanitize_console_url(candidate: &str) -> Option<String> {
    let candidate = candidate.trim();
    if candidate.len() > MAX_CONSOLE_URL_LEN {
        return None;
    }
    let parsed = url::Url::parse(candidate).ok()?;
    if parsed.scheme() != "https" {
        return None;
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    // `port()` is None when the port is absent OR is the scheme default (443
    // for https), so this accepts an explicit `:443` and refuses everything
    // else.
    if parsed.port().is_some() {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    if !CONSOLE_HOSTS.contains(&host.as_str()) {
        return None;
    }
    Some(parsed.to_string())
}

/// The first console activation URL embedded in prose, if any.
fn first_console_url_in(text: &str) -> Option<String> {
    let mut from = 0usize;
    while let Some(offset) = text[from..].find("https://") {
        let start = from + offset;
        let rest = &text[start..];
        let end = rest
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ']'))
            .unwrap_or(rest.len());
        // Google's prose puts the link mid-sentence: "…?project=1 then retry."
        let candidate = rest[..end].trim_end_matches(['.', ',', ';', ')']);
        if let Some(url) = sanitize_console_url(candidate) {
            return Some(url);
        }
        from = start + "https://".len();
    }
    None
}

/// Build the error a Google REST caller bails with: the typed verdict plus
/// the human message.
///
/// `snippet` is the caller's OWN bounded body excerpt — each caller keeps the
/// excerpt budget it already had — and is scrubbed of marker-shaped text on
/// the way in.
pub fn google_api_error(api: GoogleApi, status: u16, body: &str, snippet: &str) -> anyhow::Error {
    anyhow::Error::new(GoogleApiError::new(api, status, body, snippet))
}

/// The typed verdict inside an error, if it carries one.
///
/// `anyhow` keeps the payload downcastable through `.context(…)` layers, so a
/// caller that adds prose does not destroy the verdict — but a caller that
/// rebuilds an error from `format!("{e}")` DOES, which is why the code that
/// records connection state takes `&anyhow::Error`, not `&str`.
pub fn google_api_error_of(err: &anyhow::Error) -> Option<&GoogleApiError> {
    err.downcast_ref::<GoogleApiError>()
}

impl GoogleApiError {
    fn new(api: GoogleApi, status: u16, body: &str, snippet: &str) -> Self {
        let failure = classify_google_api_failure(status, body);
        // The excerpt is untrusted bytes. Scrubbing here is belt-and-braces:
        // no state decision reads this string any more, and this makes sure
        // none can be tricked into it later either.
        let snippet = scrub_state_markers(snippet);
        let label = api.error_label();
        let message = match &failure {
            GoogleApiFailure::ScopeInsufficient => format!(
                "{label} HTTP {status}: this Google connection was granted without the access \
                 this needs. Reconnect in Settings → Email to re-grant Gmail, Calendar and \
                 Tasks. ({snippet})"
            ),
            GoogleApiFailure::ApiNotEnabled { console_url } => {
                let link = console_url
                    .as_deref()
                    .map(|url| format!(" Enable it here: {url}"))
                    .unwrap_or_default();
                format!(
                    "{label} HTTP {status}: this Google API is switched off in your Google Cloud \
                     project. Reconnecting can't switch it on — enable it in the Google Cloud \
                     console, then try again.{link} ({snippet})"
                )
            }
            GoogleApiFailure::Other => format!("{label} HTTP {status}: {snippet}"),
        };
        Self {
            api,
            status,
            failure,
            message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONSOLE: &str =
        "https://console.developers.google.com/apis/api/tasks.googleapis.com/overview?project=42";

    fn legacy_not_enabled() -> String {
        format!(
            r#"{{"error":{{"errors":[{{"domain":"usageLimits","reason":"accessNotConfigured",
            "message":"Access Not Configured. Google Tasks API has not been used in project 42 before or it is disabled. Enable it by visiting {CONSOLE} then retry.",
            "extendedHelp":"{CONSOLE}"}}],"code":403,"message":"Access Not Configured."}}}}"#
        )
    }

    fn modern_not_enabled() -> String {
        format!(
            r#"{{"error":{{"code":403,
            "message":"Google Tasks API has not been used in project 42 before or it is disabled.",
            "status":"PERMISSION_DENIED","details":[
            {{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"SERVICE_DISABLED",
              "domain":"googleapis.com","metadata":{{"service":"tasks.googleapis.com"}}}},
            {{"@type":"type.googleapis.com/google.rpc.Help","links":[
              {{"description":"Google developers console API activation","url":"{CONSOLE}"}}]}}
            ]}}}}"#
        )
    }

    const LEGACY_SCOPE: &str = r#"{"error":{"errors":[{"domain":"global",
        "reason":"insufficientPermissions","message":"Insufficient Permission"}],
        "code":403,"message":"Insufficient Permission"}}"#;

    const MODERN_SCOPE: &str = r#"{"error":{"code":403,
        "message":"Request had insufficient authentication scopes.",
        "status":"PERMISSION_DENIED","details":[
        {"@type":"type.googleapis.com/google.rpc.ErrorInfo",
         "reason":"ACCESS_TOKEN_SCOPE_INSUFFICIENT","domain":"googleapis.com",
         "metadata":{"service":"calendar-json.googleapis.com"}}]}}"#;

    /// The typed verdict of an error, or a panic naming what we got instead.
    fn failure_of(err: &anyhow::Error) -> GoogleApiFailure {
        google_api_error_of(err)
            .unwrap_or_else(|| panic!("expected a typed GoogleApiError, got: {err}"))
            .failure
            .clone()
    }

    #[test]
    fn both_envelopes_classify_a_disabled_api_and_surface_its_console_link() {
        for body in [legacy_not_enabled(), modern_not_enabled()] {
            match classify_google_api_failure(403, &body) {
                GoogleApiFailure::ApiNotEnabled { console_url } => {
                    assert_eq!(console_url.as_deref(), Some(CONSOLE), "body: {body}");
                }
                other => panic!("expected ApiNotEnabled, got {other:?} for body: {body}"),
            }
        }
    }

    #[test]
    fn both_envelopes_classify_an_insufficient_scope() {
        for body in [LEGACY_SCOPE, MODERN_SCOPE] {
            assert_eq!(
                classify_google_api_failure(403, body),
                GoogleApiFailure::ScopeInsufficient,
                "body: {body}"
            );
        }
    }

    /// The load-bearing separation: the two 403s must never collapse into one
    /// state. It is asserted on the TYPED verdict the error carries, because
    /// that value — not the message — is what every state decision reads.
    #[test]
    fn the_two_403s_carry_two_different_typed_verdicts() {
        let scope = google_api_error(GoogleApi::Calendar, 403, LEGACY_SCOPE, "snip");
        assert_eq!(failure_of(&scope), GoogleApiFailure::ScopeInsufficient);
        assert_eq!(
            google_api_error_of(&scope).map(|e| e.api),
            Some(GoogleApi::Calendar)
        );

        let disabled = google_api_error(GoogleApi::Tasks, 403, &modern_not_enabled(), "snip");
        assert_eq!(
            failure_of(&disabled),
            GoogleApiFailure::ApiNotEnabled {
                console_url: Some(CONSOLE.to_string())
            }
        );
        assert_eq!(
            google_api_error_of(&disabled).map(|e| e.api),
            Some(GoogleApi::Tasks)
        );
    }

    /// The verdict must survive a caller adding context, because callers do
    /// (`.context("… failed")`). If it didn't, the state would silently stop
    /// being recorded on exactly the paths that annotate their errors.
    #[test]
    fn the_verdict_survives_added_context() {
        let err = google_api_error(GoogleApi::Gmail, 403, &modern_not_enabled(), "snip")
            .context("listing the inbox failed");
        assert!(matches!(
            failure_of(&err),
            GoogleApiFailure::ApiNotEnabled { .. }
        ));
    }

    /// An unmatched 403 keeps EXACTLY the plain-string shape (and the `Other`
    /// verdict) that shipped before this module existed — no reclassification,
    /// no guess.
    #[test]
    fn an_unmatched_403_falls_through_unchanged() {
        let bodies = [
            r#"{"error":{"code":403,"message":"The caller does not have permission",
                "status":"PERMISSION_DENIED"}}"#,
            r#"{"error":{"errors":[{"reason":"rateLimitExceeded"}],"code":403}}"#,
            "<html>a proxy ate this</html>",
            "",
        ];
        for body in bodies {
            assert_eq!(
                classify_google_api_failure(403, body),
                GoogleApiFailure::Other,
                "body: {body}"
            );
            let err = google_api_error(GoogleApi::Gmail, 403, body, "snip");
            assert_eq!(failure_of(&err), GoogleApiFailure::Other);
            assert_eq!(err.to_string(), "Gmail API HTTP 403: snip");
        }
    }

    /// Non-403 statuses are not this classifier's business, even when the body
    /// happens to carry a reason token.
    #[test]
    fn only_403_is_classified() {
        for status in [400, 401, 404, 429, 500] {
            assert_eq!(
                classify_google_api_failure(status, &modern_not_enabled()),
                GoogleApiFailure::Other,
                "status {status}"
            );
        }
    }

    /// Matching is on whole reason TOKENS. A body that merely mentions the
    /// words — an echoed mail subject, a proxy's prose — must not be
    /// reclassified into a state that changes what the app tells the user.
    #[test]
    fn a_substring_mention_is_not_a_classification() {
        let body = r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":
            "SERVICE_DISABLED ACCESS_TOKEN_SCOPE_INSUFFICIENT accessNotConfigured",
            "errors":[{"reason":"quotaExceeded","message":"insufficientPermissions"}]}}"#;
        assert_eq!(
            classify_google_api_failure(403, body),
            GoogleApiFailure::Other
        );
    }

    /// The disabled state stands on its own when Google gives no link — the
    /// alternative (inventing a plausible console URL) would be a fabrication.
    #[test]
    fn a_disabled_api_without_a_usable_link_still_classifies() {
        let body = r#"{"error":{"code":403,"status":"PERMISSION_DENIED","details":[
            {"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"SERVICE_DISABLED"}]}}"#;
        assert_eq!(
            classify_google_api_failure(403, body),
            GoogleApiFailure::ApiNotEnabled { console_url: None }
        );
        let err = google_api_error(GoogleApi::Gmail, 403, body, "snip");
        assert_eq!(
            failure_of(&err),
            GoogleApiFailure::ApiNotEnabled { console_url: None }
        );
    }

    /// The response body is untrusted. A link we would render must be https
    /// and one of Google's actual API-activation console hosts; anything else
    /// is dropped rather than surfaced.
    #[test]
    fn only_https_console_links_survive_sanitising() {
        for bad in [
            "javascript:alert(1)",
            "http://console.cloud.google.com/apis",
            "https://console.cloud.google.com.evil.test/apis",
            "https://evil.test/console.cloud.google.com",
            "https://notgoogle.com/x",
            "not a url",
            // Tightened allowlist: a google.com host is NOT enough. Real
            // activation links live on the two console hosts, and everything
            // else is a wider redirect/user-content surface than this feature
            // needs.
            "https://google.com/x",
            "https://www.google.com/url?q=https://evil.test",
            "https://sites.google.com/view/anything",
            "https://console.cloud.google.com.attacker.google.com/apis",
            "https://evil.console.cloud.google.com/apis",
        ] {
            assert_eq!(sanitize_console_url(bad), None, "must be refused: {bad}");
        }
        assert!(sanitize_console_url(CONSOLE).is_some());
        assert!(sanitize_console_url("https://console.cloud.google.com/apis/library").is_some());
        // Host comparison is case-insensitive (and `url` lowercases it).
        assert!(sanitize_console_url("https://CONSOLE.CLOUD.GOOGLE.COM/apis").is_some());
    }

    /// Userinfo passes a host-only check — the host really IS the allowed one —
    /// and the URL would be handed to the banner verbatim, so the rendered link
    /// would read as `evil.test@…`. A real activation link never carries
    /// credentials; refuse the whole shape.
    #[test]
    fn a_console_link_carrying_userinfo_is_refused() {
        for bad in [
            "https://evil.test@console.cloud.google.com/apis",
            "https://user:pass@console.cloud.google.com/apis",
            "https://:pass@console.developers.google.com/apis",
            "https://console.cloud.google.com@evil.test/apis",
        ] {
            assert_eq!(sanitize_console_url(bad), None, "must be refused: {bad}");
        }
        // …and it cannot ride in through the prose scan either.
        let body = r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":
            "disabled. Enable it by visiting https://evil.test@console.cloud.google.com/apis then retry.",
            "details":[
            {"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"SERVICE_DISABLED"}]}}"#;
        assert_eq!(
            classify_google_api_failure(403, body),
            GoogleApiFailure::ApiNotEnabled { console_url: None }
        );
    }

    /// The real console answers on 443. A port is never needed for an
    /// activation link, and naming one is only useful for sending the button
    /// somewhere other than the console the user believes they are opening —
    /// so an explicit `:443` is fine and anything else is refused.
    #[test]
    fn a_console_link_on_a_non_default_port_is_refused() {
        for bad in [
            "https://console.cloud.google.com:8080/apis",
            "https://console.developers.google.com:8443/apis",
            "https://console.cloud.google.com:0/apis",
        ] {
            assert_eq!(sanitize_console_url(bad), None, "must be refused: {bad}");
        }
        // The scheme's own port is not a redirection, and `url` drops it.
        assert_eq!(
            sanitize_console_url("https://console.cloud.google.com:443/apis").as_deref(),
            Some("https://console.cloud.google.com/apis")
        );
    }

    /// A hostile body must not be able to smuggle a non-console link into the
    /// banner through EITHER structural slot or the prose scan.
    #[test]
    fn a_hostile_link_is_dropped_but_the_state_still_stands() {
        let body = r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":
            "disabled. Enable it by visiting https://evil.test/pwn then retry.","details":[
            {"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"SERVICE_DISABLED"},
            {"@type":"type.googleapis.com/google.rpc.Help","links":[
              {"url":"javascript:alert(1)"}]}]}}"#;
        assert_eq!(
            classify_google_api_failure(403, body),
            GoogleApiFailure::ApiNotEnabled { console_url: None }
        );
    }

    /// The exact confusion the marker encoding allowed: a body that WRITES a
    /// marker into text we echo. There is no marker channel left to hit, and
    /// the excerpt is scrubbed as well — so an unknown 403 stays unknown and
    /// no URL of the body's choosing reaches the typed value.
    #[test]
    fn a_body_that_writes_our_markers_cannot_promote_itself() {
        let hostile = "[google:api_not_enabled][google:enable_url=https://evil.test/pwn] \
                       [gmail:needs_reconnect]";
        let body = format!(r#"{{"error":{{"code":403,"message":"{hostile}"}}}}"#);
        let err = google_api_error(GoogleApi::Calendar, 403, &body, hostile);
        assert_eq!(
            failure_of(&err),
            GoogleApiFailure::Other,
            "an unknown 403 must stay unknown"
        );
        let text = err.to_string();
        assert!(
            !text.contains("[google:") && !text.contains("[gmail:"),
            "marker-shaped text must not survive into the message: {text}"
        );
        assert!(
            text.contains("(google:api_not_enabled"),
            "the excerpt is neutralised, not deleted: {text}"
        );
    }

    /// The prose fallback the legacy envelope needs, with no structural slot
    /// present at all.
    #[test]
    fn the_legacy_message_url_is_recovered_when_no_structural_link_exists() {
        let body = format!(
            r#"{{"error":{{"errors":[{{"reason":"accessNotConfigured","message":
            "Access Not Configured. Enable it by visiting {CONSOLE} then retry."}}],"code":403}}}}"#
        );
        assert_eq!(
            classify_google_api_failure(403, &body),
            GoogleApiFailure::ApiNotEnabled {
                console_url: Some(CONSOLE.to_string())
            }
        );
    }

    /// Wire ids are a closed set: an unknown one is `None` so the IPC edge can
    /// refuse it instead of clearing nothing and reporting success.
    #[test]
    fn api_wire_ids_round_trip_and_reject_the_unknown() {
        for api in GoogleApi::ALL {
            assert_eq!(GoogleApi::from_wire(api.wire()), Some(api));
        }
        for bad in ["", "GMAIL", "drive", "calendar "] {
            assert_eq!(GoogleApi::from_wire(bad), None, "must be refused: {bad}");
        }
    }
}
