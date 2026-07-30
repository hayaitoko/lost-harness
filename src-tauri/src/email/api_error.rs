//! Google API failure classification — the recovery seam for non-2xx
//! responses from Gmail / Calendar / Tasks.
//!
//! ## Why this exists
//!
//! Before this module, EVERY non-2xx from a Google REST call bailed with a
//! plain `"… API HTTP {status}: {body}"` string. The reconnect banner is
//! driven by a substring match on
//! [`NEEDS_RECONNECT_MARKER`](super::token_provider::NEEDS_RECONNECT_MARKER),
//! and that marker is only ever embedded by the OAuth *token-refresh*
//! classifier — so a real Google `403` could never light any banner no matter
//! how many callers checked for it. The Planner just failed forever with raw
//! `Google API HTTP 403` text and no route out.
//!
//! ## The two recoverable 403s (and why they must stay separate)
//!
//! - **Scope insufficient** (`insufficientPermissions` /
//!   `ACCESS_TOKEN_SCOPE_INSUFFICIENT`) — the stored grant predates a scope
//!   the app now needs (e.g. a Gmail-only grant hitting Calendar). A
//!   reconnect genuinely fixes this: `begin_auth` re-consents with all four
//!   scopes and `prompt=consent`. → flip the EXISTING `needs_reconnect`.
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

use super::token_provider::{embed_enable_url, API_NOT_ENABLED_MARKER, NEEDS_RECONNECT_MARKER};

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

/// Per-profile record that a Google API this profile needs is switched off in
/// the user's Cloud project. Lives here (not in `ipc`) so the agent-tool path
/// and the screen IPC path can share ONE type and one shared map.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GoogleApiDisabled {
    /// Google's own console activation link, validated (https + a
    /// `google.com` host) before it is ever stored or rendered. `None` when
    /// the response carried no usable link — the UI then points at the
    /// console in prose rather than inventing a URL.
    pub console_url: Option<String>,
}

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
        if let Some(url) = first_google_url_in(message) {
            return Some(url);
        }
    }
    None
}

/// Longest a console link may be before we decline to surface it. Google's
/// real ones are ~110 chars; this only stops an absurd body from bloating an
/// error string.
const MAX_CONSOLE_URL_LEN: usize = 400;

/// Accept a URL only if it is one we are willing to render as a clickable
/// link: `https`, a `google.com` host, bounded, and free of the `]` that
/// delimits it inside an error string.
///
/// This body is UNTRUSTED input (it is whatever the endpoint we contacted
/// sent back). Handing an unvalidated URL to the UI would turn a Google 403
/// into an arbitrary-link surface, so the check is a real gate, not a
/// formality.
pub fn sanitize_console_url(candidate: &str) -> Option<String> {
    let candidate = candidate.trim();
    if candidate.len() > MAX_CONSOLE_URL_LEN || candidate.contains(']') {
        return None;
    }
    let parsed = url::Url::parse(candidate).ok()?;
    if parsed.scheme() != "https" {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    if host != "google.com" && !host.ends_with(".google.com") {
        return None;
    }
    Some(parsed.to_string())
}

/// The first `https://…google.com/…` URL embedded in prose, if any.
fn first_google_url_in(text: &str) -> Option<String> {
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

/// Build the error a Google REST caller bails with, carrying the marker the
/// IPC/tool layers match on.
///
/// `api` names the caller's surface ("Gmail API" / "Google API") and `snippet`
/// is the caller's OWN bounded body excerpt — each caller keeps the excerpt
/// budget it already had.
pub fn google_api_error(api: &str, status: u16, body: &str, snippet: &str) -> anyhow::Error {
    match classify_google_api_failure(status, body) {
        GoogleApiFailure::ScopeInsufficient => anyhow::anyhow!(
            "{NEEDS_RECONNECT_MARKER} {api} HTTP {status}: this Google connection was granted \
             without the access this needs. Reconnect in Settings → Email to re-grant Gmail, \
             Calendar and Tasks. ({snippet})"
        ),
        GoogleApiFailure::ApiNotEnabled { console_url } => {
            let link = console_url
                .as_deref()
                .map(embed_enable_url)
                .unwrap_or_default();
            anyhow::anyhow!(
                "{API_NOT_ENABLED_MARKER}{link} {api} HTTP {status}: this Google API is switched \
                 off in your Google Cloud project. Reconnecting can't switch it on — enable it \
                 in the Google Cloud console, then try again. ({snippet})"
            )
        }
        GoogleApiFailure::Other => anyhow::anyhow!("{api} HTTP {status}: {snippet}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::email::token_provider::extract_enable_url;

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
    /// state. Scope-403 carries the reconnect marker (and NOT the disabled
    /// one); disabled-403 carries the disabled marker (and NOT the reconnect
    /// one, or the user gets an infinite reconnect loop).
    #[test]
    fn the_two_403s_carry_different_markers_and_never_each_others() {
        let scope = google_api_error("Google API", 403, LEGACY_SCOPE, "snip").to_string();
        assert!(scope.contains(NEEDS_RECONNECT_MARKER), "got: {scope}");
        assert!(!scope.contains(API_NOT_ENABLED_MARKER), "got: {scope}");

        let disabled =
            google_api_error("Google API", 403, &modern_not_enabled(), "snip").to_string();
        assert!(disabled.contains(API_NOT_ENABLED_MARKER), "got: {disabled}");
        assert!(
            !disabled.contains(NEEDS_RECONNECT_MARKER),
            "reconnecting can never enable an API: {disabled}"
        );
        assert_eq!(extract_enable_url(&disabled).as_deref(), Some(CONSOLE));
    }

    /// An unmatched 403 keeps EXACTLY the plain-string shape that shipped
    /// before this module existed — no marker, no guess.
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
            let err = google_api_error("Google API", 403, body, "snip").to_string();
            assert_eq!(err, "Google API HTTP 403: snip");
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
        let err = google_api_error("Google API", 403, body, "snip").to_string();
        assert!(err.contains(API_NOT_ENABLED_MARKER), "got: {err}");
        assert_eq!(extract_enable_url(&err), None);
    }

    /// The response body is untrusted. A link we would render must be https
    /// and Google's own; anything else is dropped rather than surfaced.
    #[test]
    fn only_https_google_links_survive_sanitising() {
        for bad in [
            "javascript:alert(1)",
            "http://console.cloud.google.com/apis",
            "https://console.cloud.google.com.evil.test/apis",
            "https://evil.test/console.cloud.google.com",
            "https://notgoogle.com/x",
            "not a url",
        ] {
            assert_eq!(sanitize_console_url(bad), None, "must be refused: {bad}");
        }
        assert!(sanitize_console_url(CONSOLE).is_some());
        assert!(sanitize_console_url("https://google.com/x").is_some());
    }

    /// A hostile body must not be able to smuggle a non-Google link into the
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

    /// Round-trip of the embedded-link encoding, including the "no link" case.
    #[test]
    fn the_enable_url_round_trips_through_the_marker() {
        let embedded = embed_enable_url(CONSOLE);
        assert_eq!(
            extract_enable_url(&format!("{API_NOT_ENABLED_MARKER}{embedded} prose")).as_deref(),
            Some(CONSOLE)
        );
        assert_eq!(extract_enable_url("no marker here"), None);
        assert_eq!(extract_enable_url("[google:enable_url=]"), None);
    }
}
