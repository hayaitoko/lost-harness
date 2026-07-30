//! Minimal Gmail REST v1 client: list, read, send. Nothing else.
//!
//! WHY this shape: stage 2's email tools consume the high-level [`GmailApi`]
//! trait (so tool tests fake the whole mailbox), while THIS file's own tests
//! fake one level lower — the [`GmailHttp`] transport and the
//! [`TokenProvider`] authorization seam — so the real client's URL building,
//! 401-retry-once, and response parsing are all covered without network.
//! Every parse/decode step is a pure helper with fixtures, following the
//! `hf_search.rs` house pattern.
//!
//! Authorization: the client never touches the keychain or the refresh
//! endpoint itself. It asks [`TokenProvider`] for a bearer token, and on a
//! 401 asks ONCE more with `force_refresh = true` (stage 2 implements that
//! call over `oauth::refresh` + the keychain), then retries the request
//! exactly once. A second 401 fails honestly — no retry loops on a dead
//! credential.
//!
//! Body extraction honesty: we prefer the `text/plain` part(s) of a message.
//! When a message is HTML-only we do a MINIMAL tag-strip (drop
//! script/style blocks, drop tags, decode a handful of entities). That is a
//! readability fallback, not an HTML renderer or sanitizer — tables, links'
//! hrefs, and layout are lost, and the output is still untrusted content
//! (the dispatcher guard-wraps it like any tool result).

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use regex::Regex;
use serde::Deserialize;

use super::BoxFuture;

/// Gmail REST v1 origin. The real client builds every URL from this.
pub const GMAIL_API_BASE: &str = "https://gmail.googleapis.com";

/// Gmail caps `maxResults` at 500; we clamp rather than let the API 400.
const MAX_RESULTS_CAP: u32 = 500;

/// Bound on error-body snippets carried into error strings.
const ERROR_SNIPPET_CHARS: usize = 300;

/// Hard ceiling on a single Gmail API response body, in bytes.
///
/// WHY: `resp.text()` buffers whatever the server sends, so a hostile or
/// runaway response could exhaust memory before any of our own caps (the
/// `email_read` body cap) ever see it. Gmail's own message-size limit is
/// 25 MB and `messages.get?format=full` returns oversized attachment parts as
/// an `attachmentId` rather than inline data, so 32 MiB sits above anything
/// legitimate while still being a bound. Exceeding it fails the request
/// cleanly instead of buffering.
///
/// Shared with the sibling Google fetch layer (`super::google`), which has the
/// same exposure through the same reqwest surface.
pub(super) const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Public data shapes (what stage 2's tools serialize)
// ---------------------------------------------------------------------------

/// One row of a message listing — ids only (Gmail's list endpoint returns no
/// content; fetch via [`GmailApi::get_message`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MessageMeta {
    pub id: String,
    pub thread_id: String,
}

/// A fetched message, flattened to what an agent needs. Header fields are the
/// raw header values (empty string when the header is absent — honestly
/// absent, never fabricated).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EmailMessage {
    pub id: String,
    pub from: String,
    pub to: String,
    pub subject: String,
    /// The raw `Date` header (RFC 2822 format), unparsed.
    pub date: String,
    /// Gmail's own short snippet.
    pub snippet: String,
    /// Extracted body text — see the module docs for the plain/html rules.
    pub body_text: String,
}

// ---------------------------------------------------------------------------
// Seams
// ---------------------------------------------------------------------------

/// Supplies a bearer token for Gmail calls. Stage 2's impl holds the
/// in-memory access token and, on `force_refresh`, goes through
/// `oauth::refresh` + the keychain (surfacing
/// [`super::oauth::RefreshError::NeedsReconnect`] upward untouched).
pub trait TokenProvider: Send + Sync {
    /// A bearer token. `force_refresh = false` may serve a cached token;
    /// `true` is only sent after a 401 and MUST mint a fresh one (or fail).
    fn access_token(&self, force_refresh: bool) -> BoxFuture<'_, anyhow::Result<String>>;
}

/// HTTP verbs the Gmail surface needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
}

/// The transport seam: one authorized HTTP request → `(status, body)`.
/// Real impl is reqwest; tests script it.
pub trait GmailHttp: Send + Sync {
    fn request<'a>(
        &'a self,
        method: HttpMethod,
        url: &'a str,
        bearer: &'a str,
        json_body: Option<&'a serde_json::Value>,
    ) -> BoxFuture<'a, anyhow::Result<(u16, String)>>;
}

/// The high-level mailbox surface stage 2's tools consume (and fake).
pub trait GmailApi: Send + Sync {
    /// `GET /gmail/v1/users/me/messages` — newest-first ids matching `query`
    /// (Gmail search syntax, e.g. `"is:unread from:x"`), at most `max` rows.
    fn list_messages<'a>(
        &'a self,
        query: Option<&'a str>,
        max: u32,
    ) -> BoxFuture<'a, anyhow::Result<Vec<MessageMeta>>>;

    /// `GET /gmail/v1/users/me/messages/{id}?format=full` — one message with
    /// headers + extracted body text.
    fn get_message<'a>(&'a self, id: &'a str) -> BoxFuture<'a, anyhow::Result<EmailMessage>>;

    /// `GET /gmail/v1/users/me/messages/{id}?format=metadata&metadataHeaders=...`
    /// — lightweight: headers + snippet only, no body data. Use for search
    /// previews where the full body is unnecessary.
    fn get_message_metadata<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<EmailMessage>>;

    /// `POST /gmail/v1/users/me/messages/send` — send a raw RFC 822 message
    /// (build it with [`build_rfc822`]). Returns the sent message's id.
    /// IRREVERSIBLE — the tool wrapping this is `Dangerous` (see module docs).
    fn send<'a>(&'a self, raw_rfc822: &'a str) -> BoxFuture<'a, anyhow::Result<String>>;

    /// `GET /gmail/v1/users/me/profile` — the connected account's email
    /// address, captured once at connect time so the UI can show
    /// "connected as x@gmail.com" without further network calls.
    fn get_profile<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<String>>;
}

// ---------------------------------------------------------------------------
// The real client
// ---------------------------------------------------------------------------

/// Production transport: reqwest with short timeouts (mail calls are small;
/// a hung call must not wedge an agent turn).
pub struct ReqwestGmailHttp {
    client: reqwest::Client,
    /// Response-body ceiling in bytes (see [`MAX_RESPONSE_BYTES`]). A field
    /// rather than a bare const so tests can drive the refusal path with a
    /// small body instead of allocating tens of megabytes.
    max_response_bytes: usize,
}

impl ReqwestGmailHttp {
    pub fn new() -> anyhow::Result<Self> {
        Self::with_response_cap(MAX_RESPONSE_BYTES)
    }

    fn with_response_cap(max_response_bytes: usize) -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10))
                .build()?,
            max_response_bytes,
        })
    }
}

/// Buffer a response body with a hard byte ceiling.
///
/// Two gates: the declared `Content-Length` (cheap, refuses before a single
/// body byte is read) and the running total while streaming (the honest one —
/// a chunked or mis-declared response only reveals its size as it arrives).
/// Either way we stop at `cap` rather than growing the buffer.
///
/// `what` names the caller's API in the error text ("Gmail API", "Google API",
/// "OAuth token endpoint") — the same defect exists on every reqwest fetch
/// layer in this module, so they all share this one implementation.
pub(super) async fn read_body_capped(
    mut resp: reqwest::Response,
    cap: usize,
    what: &str,
) -> anyhow::Result<String> {
    if let Some(declared) = resp.content_length() {
        if declared > cap as u64 {
            anyhow::bail!(
                "{what} response too large: declared {declared} bytes, cap is {cap} bytes"
            );
        }
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| anyhow::anyhow!("reading the {what} response failed: {e}"))?
    {
        if buf.len() + chunk.len() > cap {
            anyhow::bail!("{what} response too large: exceeded the {cap}-byte cap");
        }
        buf.extend_from_slice(&chunk);
    }
    // Lossy on purpose: the body is untrusted bytes, and a decode failure
    // should surface as a parse error downstream, not a panic here.
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

impl GmailHttp for ReqwestGmailHttp {
    fn request<'a>(
        &'a self,
        method: HttpMethod,
        url: &'a str,
        bearer: &'a str,
        json_body: Option<&'a serde_json::Value>,
    ) -> BoxFuture<'a, anyhow::Result<(u16, String)>> {
        Box::pin(async move {
            let mut req = match method {
                HttpMethod::Get => self.client.get(url),
                HttpMethod::Post => self.client.post(url),
            };
            req = req.bearer_auth(bearer);
            if let Some(body) = json_body {
                req = req.json(body);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("{method:?} Gmail API failed: {e}"))?;
            let status = resp.status().as_u16();
            let body = read_body_capped(resp, self.max_response_bytes, "Gmail API").await?;
            Ok((status, body))
        })
    }
}

/// The real Gmail client: URL building + auth + 401-retry-once over the two
/// seams. All parsing is delegated to the pure helpers below.
pub struct GmailClient {
    http: Box<dyn GmailHttp>,
    tokens: Arc<dyn TokenProvider>,
    base_url: String,
}

impl GmailClient {
    pub fn new(http: Box<dyn GmailHttp>, tokens: Arc<dyn TokenProvider>) -> Self {
        Self {
            http,
            tokens,
            base_url: GMAIL_API_BASE.to_string(),
        }
    }

    /// One authorized call with the 401-retry-once policy: cached token →
    /// request → on 401, force-refresh ONCE → retry → any second failure is
    /// final. Never loops.
    async fn execute(
        &self,
        method: HttpMethod,
        url: &str,
        body: Option<&serde_json::Value>,
    ) -> anyhow::Result<String> {
        let token = self.tokens.access_token(false).await?;
        let (mut status, mut text) = self.http.request(method, url, &token, body).await?;
        if status == 401 {
            let fresh = self.tokens.access_token(true).await.map_err(|e| {
                anyhow::anyhow!("Gmail rejected the access token and refresh failed: {e}")
            })?;
            (status, text) = self.http.request(method, url, &fresh, body).await?;
        }
        if !(200..300).contains(&status) {
            // Same recovery seam the Calendar/Tasks client uses: Gmail returns
            // the identical 403 envelopes, so a scope-short grant here must
            // light the same reconnect banner rather than dead-ending in raw
            // `Gmail API HTTP 403` text. Unmatched statuses are untouched.
            return Err(crate::email::api_error::google_api_error(
                "Gmail API",
                status,
                &text,
                &snippet(&text),
            ));
        }
        Ok(text)
    }

    fn list_url(&self, query: Option<&str>, max: u32) -> String {
        let mut u = url::Url::parse(&format!("{}/gmail/v1/users/me/messages", self.base_url))
            .expect("static base + fixed path");
        {
            let mut q = u.query_pairs_mut();
            q.append_pair("maxResults", &max.clamp(1, MAX_RESULTS_CAP).to_string());
            if let Some(query) = query {
                if !query.trim().is_empty() {
                    q.append_pair("q", query.trim());
                }
            }
        }
        u.into()
    }
}

impl GmailApi for GmailClient {
    fn list_messages<'a>(
        &'a self,
        query: Option<&'a str>,
        max: u32,
    ) -> BoxFuture<'a, anyhow::Result<Vec<MessageMeta>>> {
        Box::pin(async move {
            let url = self.list_url(query, max);
            let body = self.execute(HttpMethod::Get, &url, None).await?;
            parse_message_list(&body)
        })
    }

    fn get_message<'a>(&'a self, id: &'a str) -> BoxFuture<'a, anyhow::Result<EmailMessage>> {
        Box::pin(async move {
            // The id is spliced into a URL path — refuse anything outside
            // Gmail's id alphabet so a crafted id can't smuggle a path or
            // query into the request (same discipline as `valid_model_id`).
            if !valid_message_id(id) {
                anyhow::bail!("malformed Gmail message id: {id:?}");
            }
            let url = format!(
                "{}/gmail/v1/users/me/messages/{id}?format=full",
                self.base_url
            );
            let body = self.execute(HttpMethod::Get, &url, None).await?;
            parse_message(&body)
        })
    }

    fn get_message_metadata<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<EmailMessage>> {
        Box::pin(async move {
            if !valid_message_id(id) {
                anyhow::bail!("malformed Gmail message id: {id:?}");
            }
            let url = format!(
                "{}/gmail/v1/users/me/messages/{id}?format=metadata&metadataHeaders=From&metadataHeaders=Subject&metadataHeaders=Date",
                self.base_url
            );
            let body = self.execute(HttpMethod::Get, &url, None).await?;
            parse_message(&body)
        })
    }

    fn send<'a>(&'a self, raw_rfc822: &'a str) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(async move {
            let url = format!("{}/gmail/v1/users/me/messages/send", self.base_url);
            // Gmail wants the raw message base64url-encoded (unpadded ok).
            let body = serde_json::json!({ "raw": URL_SAFE_NO_PAD.encode(raw_rfc822.as_bytes()) });
            let resp = self.execute(HttpMethod::Post, &url, Some(&body)).await?;
            let sent: SentResponse = serde_json::from_str(&resp)
                .map_err(|e| anyhow::anyhow!("send response didn't parse: {e}"))?;
            Ok(sent.id)
        })
    }

    fn get_profile<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(async move {
            let url = format!("{}/gmail/v1/users/me/profile", self.base_url);
            let body = self.execute(HttpMethod::Get, &url, None).await?;
            let v: serde_json::Value = serde_json::from_str(&body)
                .map_err(|e| anyhow::anyhow!("profile response didn't parse: {e}"))?;
            v.get("emailAddress")
                .and_then(|e| e.as_str())
                .map(String::from)
                .ok_or_else(|| anyhow::anyhow!("profile response had no emailAddress"))
        })
    }
}

/// Is this a plausible Gmail message id (hex-ish opaque token)? Conservative
/// alphabet check before splicing into a URL path.
fn valid_message_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn snippet(body: &str) -> String {
    let mut s: String = body.chars().take(ERROR_SNIPPET_CHARS).collect();
    if s.len() < body.len() {
        s.push('…');
    }
    s
}

// ---------------------------------------------------------------------------
// Wire shapes (Gmail JSON; camelCase on the wire)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ListResponse {
    #[serde(default)]
    messages: Vec<ListEntry>,
}

#[derive(Deserialize)]
struct ListEntry {
    id: String,
    #[serde(rename = "threadId", default)]
    thread_id: String,
}

#[derive(Deserialize)]
struct SentResponse {
    id: String,
}

#[derive(Deserialize)]
struct WireMessage {
    id: String,
    #[serde(default)]
    snippet: String,
    #[serde(default)]
    payload: Option<WirePart>,
}

/// One MIME part in `format=full`. Recursive: multiparts carry `parts`.
#[derive(Deserialize, Default)]
struct WirePart {
    #[serde(rename = "mimeType", default)]
    mime_type: String,
    #[serde(default)]
    headers: Vec<WireHeader>,
    #[serde(default)]
    body: Option<WireBody>,
    #[serde(default)]
    parts: Vec<WirePart>,
}

#[derive(Deserialize)]
struct WireHeader {
    name: String,
    value: String,
}

#[derive(Deserialize, Default)]
struct WireBody {
    /// base64url-encoded content. Absent for attachment stubs (those carry an
    /// `attachmentId` instead — we never fetch attachments).
    #[serde(default)]
    data: Option<String>,
}

// ---------------------------------------------------------------------------
// Pure parsers/decoders (fixture-tested)
// ---------------------------------------------------------------------------

/// Parse a list response. An absent `messages` array (empty mailbox / no
/// matches) is an empty vec, not an error. Pure.
pub fn parse_message_list(json: &str) -> anyhow::Result<Vec<MessageMeta>> {
    let list: ListResponse = serde_json::from_str(json)
        .map_err(|e| anyhow::anyhow!("message list didn't parse: {e}"))?;
    Ok(list
        .messages
        .into_iter()
        .map(|m| MessageMeta {
            id: m.id,
            thread_id: m.thread_id,
        })
        .collect())
}

/// Parse a `format=full` message into an [`EmailMessage`]. Pure.
pub fn parse_message(json: &str) -> anyhow::Result<EmailMessage> {
    let msg: WireMessage =
        serde_json::from_str(json).map_err(|e| anyhow::anyhow!("message didn't parse: {e}"))?;
    let payload = msg.payload.unwrap_or_default();
    Ok(EmailMessage {
        id: msg.id,
        from: header_value(&payload, "From"),
        to: header_value(&payload, "To"),
        subject: header_value(&payload, "Subject"),
        date: header_value(&payload, "Date"),
        snippet: msg.snippet,
        body_text: extract_body_text(&payload),
    })
}

/// Top-level header lookup, case-insensitive (RFC 5322 header names are).
fn header_value(payload: &WirePart, name: &str) -> String {
    payload
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.clone())
        .unwrap_or_default()
}

/// Decode a Gmail base64url body blob. Gmail emits the URL-safe alphabet,
/// sometimes padded — strip padding, then decode unpadded. Not `unwrap`:
/// server data is untrusted.
pub fn decode_b64url(data: &str) -> anyhow::Result<Vec<u8>> {
    let cleaned: String = data
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect::<String>()
        .trim_end_matches('=')
        .to_string();
    URL_SAFE_NO_PAD
        .decode(cleaned.as_bytes())
        .map_err(|e| anyhow::anyhow!("body data isn't valid base64url: {e}"))
}

/// Extract readable body text from a message payload tree.
///
/// Preference order:
/// 1. every `text/plain` part with inline data, depth-first, joined by a
///    blank line (multipart/mixed can legitimately carry several);
/// 2. else the FIRST `text/html` part, minimally tag-stripped (see
///    [`strip_html_minimal`] for exactly how little that promises);
/// 3. else empty string — honestly nothing we can show.
fn extract_body_text(payload: &WirePart) -> String {
    let mut plains: Vec<String> = Vec::new();
    collect_parts(payload, "text/plain", &mut plains);
    if !plains.is_empty() {
        return plains.join("\n\n");
    }
    let mut htmls: Vec<String> = Vec::new();
    collect_parts(payload, "text/html", &mut htmls);
    match htmls.into_iter().next() {
        Some(html) => strip_html_minimal(&html),
        None => String::new(),
    }
}

/// Depth-first collect of decoded bodies for a mime type. A part whose data
/// fails to decode is skipped (bad server data must not sink the whole
/// message — the other parts may still be readable).
fn collect_parts(part: &WirePart, mime: &str, out: &mut Vec<String>) {
    if part.mime_type.eq_ignore_ascii_case(mime) {
        if let Some(data) = part.body.as_ref().and_then(|b| b.data.as_deref()) {
            if let Ok(bytes) = decode_b64url(data) {
                out.push(String::from_utf8_lossy(&bytes).into_owned());
            }
        }
    }
    for child in &part.parts {
        collect_parts(child, mime, out);
    }
}

/// Minimal HTML→text: drop `<script>`/`<style>` blocks, turn `<br>`/`</p>`/
/// `</div>` into newlines, strip remaining tags, decode the six common
/// entities, collapse blank-line runs. That's ALL it does — it is a
/// readability fallback for HTML-only mail, not a renderer or sanitizer;
/// link targets, tables, and layout are lost.
pub fn strip_html_minimal(html: &str) -> String {
    static SCRIPT_STYLE: OnceLock<Regex> = OnceLock::new();
    static BREAKS: OnceLock<Regex> = OnceLock::new();
    static TAGS: OnceLock<Regex> = OnceLock::new();
    let script_style = SCRIPT_STYLE.get_or_init(|| {
        Regex::new(r"(?is)<(script|style)\b[^>]*>.*?</(script|style)\s*>").expect("static regex")
    });
    let breaks = BREAKS
        .get_or_init(|| Regex::new(r"(?i)<br\s*/?>|</p\s*>|</div\s*>").expect("static regex"));
    let tags = TAGS.get_or_init(|| Regex::new(r"<[^>]*>").expect("static regex"));

    let no_blocks = script_style.replace_all(html, "");
    let with_breaks = breaks.replace_all(&no_blocks, "\n");
    let no_tags = tags.replace_all(&with_breaks, "");
    let decoded = no_tags
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        // &amp; last, so "&amp;lt;" decodes to the literal "&lt;" not "<".
        .replace("&amp;", "&");

    // Trim per-line, collapse runs of blank lines to one.
    let mut out = String::with_capacity(decoded.len());
    let mut blank_run = true; // swallow leading blanks
    for line in decoded.lines() {
        let line = line.trim();
        if line.is_empty() {
            if !blank_run {
                out.push('\n');
            }
            blank_run = true;
        } else {
            out.push_str(line);
            out.push('\n');
            blank_run = false;
        }
    }
    out.trim_end().to_string()
}

// ---------------------------------------------------------------------------
// RFC 822 builder (for send)
// ---------------------------------------------------------------------------

/// Build a minimal RFC 822/2045 message: CRLF headers, UTF-8 text/plain body,
/// base64 content-transfer-encoding ALWAYS (the simplest correct choice — no
/// quoted-printable, no dot-stuffing, no line-length worries, valid for both
/// ASCII and non-ASCII bodies). Non-ASCII subjects become RFC 2047 UTF-8/B
/// encoded-words, folded to spec length. `From` is intentionally omitted:
/// Gmail stamps the authenticated account's address, which is also the honest
/// one. Recipients with line breaks are refused (header injection).
pub fn build_rfc822(to: &str, subject: &str, body: &str) -> anyhow::Result<String> {
    if to.is_empty() {
        anyhow::bail!("recipient is empty");
    }
    if to.chars().any(|c| c == '\r' || c == '\n') {
        anyhow::bail!("recipient must not contain line breaks (header injection)");
    }
    if subject.chars().any(|c| c == '\r' || c == '\n') {
        anyhow::bail!("subject must not contain line breaks (header injection)");
    }
    let encoded_body = wrap_b64(&STANDARD.encode(body.as_bytes()));
    Ok(format!(
        "To: {to}\r\nSubject: {subj}\r\nMIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Transfer-Encoding: base64\r\n\r\n{encoded_body}",
        subj = encode_subject(subject),
    ))
}

/// RFC 2047 subject encoding: pure-ASCII printable subjects pass through;
/// anything else becomes one or more `=?UTF-8?B?…?=` encoded-words, chunked
/// at UTF-8 char boundaries so each stays inside the 75-char limit, joined
/// with folded whitespace.
fn encode_subject(subject: &str) -> String {
    if subject.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return subject.to_string();
    }
    // 45 raw bytes → 60 base64 chars → 72 chars with the =?UTF-8?B?…?= frame.
    const CHUNK_BYTES: usize = 45;
    let mut words: Vec<String> = Vec::new();
    let mut buf = String::new();
    for ch in subject.chars() {
        if buf.len() + ch.len_utf8() > CHUNK_BYTES {
            words.push(format!("=?UTF-8?B?{}?=", STANDARD.encode(buf.as_bytes())));
            buf.clear();
        }
        buf.push(ch);
    }
    if !buf.is_empty() {
        words.push(format!("=?UTF-8?B?{}?=", STANDARD.encode(buf.as_bytes())));
    }
    // Folded continuation: CRLF + space keeps it one logical header.
    words.join("\r\n ")
}

/// Wrap base64 at the RFC 2045 76-char line limit, CRLF line endings.
fn wrap_b64(b64: &str) -> String {
    b64.as_bytes()
        .chunks(76)
        .map(|c| std::str::from_utf8(c).expect("base64 is ASCII"))
        .collect::<Vec<_>>()
        .join("\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    // -- pure decoding/extraction ------------------------------------------

    fn b64url(s: &str) -> String {
        URL_SAFE_NO_PAD.encode(s.as_bytes())
    }

    #[test]
    fn b64url_decode_tolerates_padding_and_rejects_garbage() {
        assert_eq!(decode_b64url(&b64url("hi")).unwrap(), b"hi");
        // Padded variant (4 bytes → two '=' pads) decodes identically.
        let padded = base64::engine::general_purpose::URL_SAFE.encode(b"hi!!");
        assert!(padded.ends_with('='));
        assert_eq!(decode_b64url(&padded).unwrap(), b"hi!!");
        assert!(
            decode_b64url("not base64 !!!").is_err(),
            "no unwrap on untrusted data"
        );
    }

    /// The nested-multipart fixture the task asks for:
    /// multipart/mixed → [ multipart/alternative → [plain, html], attachment ].
    fn nested_message_json() -> String {
        format!(
            r#"{{
              "id": "m1", "threadId": "t1",
              "snippet": "Lunch tomorrow?",
              "payload": {{
                "mimeType": "multipart/mixed",
                "headers": [
                  {{"name": "From", "value": "Ada <ada@example.com>"}},
                  {{"name": "to", "value": "lukas@example.com"}},
                  {{"name": "SUBJECT", "value": "Lunch"}},
                  {{"name": "Date", "value": "Thu, 23 Jul 2026 09:00:00 -0700"}}
                ],
                "parts": [
                  {{
                    "mimeType": "multipart/alternative",
                    "parts": [
                      {{"mimeType": "text/plain", "body": {{"data": "{plain}"}}}},
                      {{"mimeType": "text/html", "body": {{"data": "{html}"}}}}
                    ]
                  }},
                  {{"mimeType": "image/png", "filename": "map.png",
                    "body": {{"attachmentId": "att-1", "size": 12345}}}}
                ]
              }}
            }}"#,
            plain = b64url("Lunch tomorrow at noon?\n— Ada"),
            html = b64url("<p>Lunch tomorrow at <b>noon</b>?</p>")
        )
    }

    #[test]
    fn nested_multipart_prefers_text_plain_and_reads_headers() {
        let msg = parse_message(&nested_message_json()).unwrap();
        assert_eq!(msg.id, "m1");
        assert_eq!(msg.from, "Ada <ada@example.com>");
        assert_eq!(
            msg.to, "lukas@example.com",
            "header lookup is case-insensitive"
        );
        assert_eq!(msg.subject, "Lunch");
        assert_eq!(msg.date, "Thu, 23 Jul 2026 09:00:00 -0700");
        assert_eq!(msg.snippet, "Lunch tomorrow?");
        assert_eq!(
            msg.body_text, "Lunch tomorrow at noon?\n— Ada",
            "text/plain wins over text/html; the attachment stub is ignored"
        );
    }

    #[test]
    fn html_only_message_falls_back_to_minimal_strip() {
        let json = format!(
            r#"{{"id":"m2","threadId":"t2","snippet":"s",
                "payload":{{"mimeType":"text/html","headers":[],
                "body":{{"data":"{}"}}}}}}"#,
            b64url(
                "<html><style>p{color:red}</style><body><p>Hello&nbsp;there</p>\
                 <script>alert('x')</script><div>A &amp; B &lt;ok&gt;</div></body></html>"
            )
        );
        let msg = parse_message(&json).unwrap();
        assert_eq!(msg.body_text, "Hello there\nA & B <ok>");
    }

    #[test]
    fn message_with_no_readable_part_yields_empty_body() {
        let json = r#"{"id":"m3","threadId":"t3","snippet":"",
            "payload":{"mimeType":"image/png","headers":[],
            "body":{"attachmentId":"a1"}}}"#;
        assert_eq!(parse_message(json).unwrap().body_text, "");
    }

    #[test]
    fn message_list_parses_and_empty_mailbox_is_empty_not_error() {
        let json = r#"{"messages":[
            {"id":"a","threadId":"ta"},{"id":"b","threadId":"tb"}],
            "resultSizeEstimate":2}"#;
        let list = parse_message_list(json).unwrap();
        assert_eq!(
            list,
            vec![
                MessageMeta {
                    id: "a".into(),
                    thread_id: "ta".into()
                },
                MessageMeta {
                    id: "b".into(),
                    thread_id: "tb".into()
                },
            ]
        );
        // Gmail omits `messages` entirely when nothing matches.
        assert_eq!(
            parse_message_list(r#"{"resultSizeEstimate":0}"#).unwrap(),
            vec![]
        );
    }

    // -- rfc822 ------------------------------------------------------------

    /// Split a built message into (headers, decoded body) for round-tripping.
    fn parse_built(raw: &str) -> (Vec<String>, String) {
        let (head, body) = raw.split_once("\r\n\r\n").expect("blank line");
        let headers = head.split("\r\n").map(str::to_string).collect();
        let b64: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        let bytes = STANDARD.decode(b64.as_bytes()).expect("valid base64 body");
        (headers, String::from_utf8(bytes).expect("utf-8 body"))
    }

    #[test]
    fn rfc822_roundtrips_ascii() {
        let raw = build_rfc822("dst@example.com", "Plain subject", "Line one.\nLine two.").unwrap();
        let (headers, body) = parse_built(&raw);
        assert!(headers.contains(&"To: dst@example.com".to_string()));
        assert!(headers.contains(&"Subject: Plain subject".to_string()));
        assert!(headers.contains(&"MIME-Version: 1.0".to_string()));
        assert!(headers.contains(&"Content-Type: text/plain; charset=utf-8".to_string()));
        assert!(headers.contains(&"Content-Transfer-Encoding: base64".to_string()));
        assert_eq!(body, "Line one.\nLine two.");
    }

    #[test]
    fn rfc822_roundtrips_unicode_subject_and_body() {
        let subject = "Grüße aus München — 日本語のテスト with a long tail to force chunking";
        let body_in = "Ünïcode body 🎉\nsecond line";
        let raw = build_rfc822("dst@example.com", subject, body_in).unwrap();
        let (headers, body) = parse_built(&raw);
        assert_eq!(body, body_in);
        // The subject header (with folds unfolded) is encoded words only.
        let subj_folded = raw
            .split("\r\nMIME-Version")
            .next()
            .unwrap()
            .split("\r\nSubject: ")
            .nth(1)
            .unwrap()
            .to_string();
        let unfolded = subj_folded.replace("\r\n ", " ");
        let mut decoded = String::new();
        for word in unfolded.split_whitespace() {
            let b64 = word
                .strip_prefix("=?UTF-8?B?")
                .and_then(|w| w.strip_suffix("?="))
                .unwrap_or_else(|| panic!("not an encoded word: {word}"));
            assert!(
                word.len() <= 75,
                "encoded word over RFC 2047 length: {word}"
            );
            decoded.push_str(&String::from_utf8(STANDARD.decode(b64.as_bytes()).unwrap()).unwrap());
        }
        assert_eq!(decoded, subject);
        // Every header line is CRLF-separated ASCII.
        assert!(headers.iter().all(|h| h.is_ascii()));
    }

    #[test]
    fn rfc822_refuses_header_injection() {
        assert!(build_rfc822("a@b\r\nBcc: evil@x", "s", "b").is_err());
        assert!(build_rfc822("a@b", "s\r\nBcc: evil@x", "b").is_err());
        assert!(build_rfc822("", "s", "b").is_err());
    }

    #[test]
    fn base64_body_lines_stay_within_rfc2045_limit() {
        let raw = build_rfc822("a@b.c", "s", &"x".repeat(1000)).unwrap();
        let body = raw.split_once("\r\n\r\n").unwrap().1;
        assert!(body.lines().all(|l| l.len() <= 76), "76-char base64 lines");
    }

    // -- the client over fake seams ----------------------------------------

    /// (method, url, bearer, body) as the fake transport saw it.
    type RecordedCall = (HttpMethod, String, String, Option<serde_json::Value>);

    struct FakeHttp {
        calls: Mutex<Vec<RecordedCall>>,
        responses: Mutex<VecDeque<(u16, String)>>,
    }

    impl FakeHttp {
        fn scripted(responses: Vec<(u16, &str)>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(
                    responses
                        .into_iter()
                        .map(|(s, b)| (s, b.to_string()))
                        .collect(),
                ),
            }
        }
    }

    impl GmailHttp for FakeHttp {
        fn request<'a>(
            &'a self,
            method: HttpMethod,
            url: &'a str,
            bearer: &'a str,
            json_body: Option<&'a serde_json::Value>,
        ) -> BoxFuture<'a, anyhow::Result<(u16, String)>> {
            self.calls.lock().unwrap().push((
                method,
                url.to_string(),
                bearer.to_string(),
                json_body.cloned(),
            ));
            let resp = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted response");
            Box::pin(async move { Ok(resp) })
        }
    }

    struct FakeTokens {
        forced: Mutex<u32>,
    }

    impl FakeTokens {
        fn new() -> Self {
            Self {
                forced: Mutex::new(0),
            }
        }
    }

    impl TokenProvider for FakeTokens {
        fn access_token(&self, force_refresh: bool) -> BoxFuture<'_, anyhow::Result<String>> {
            let token = if force_refresh {
                *self.forced.lock().unwrap() += 1;
                "fresh-token".to_string()
            } else {
                "stale-token".to_string()
            };
            Box::pin(async move { Ok(token) })
        }
    }

    #[tokio::test]
    async fn a_401_triggers_exactly_one_refresh_then_succeeds() {
        let ((client, http), tokens) = arc_client(FakeHttp::scripted(vec![
            (401, r#"{"error":{"code":401}}"#),
            (200, r#"{"messages":[{"id":"a","threadId":"t"}]}"#),
        ]));
        let list = client.list_messages(None, 10).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(
            *tokens.forced.lock().unwrap(),
            1,
            "exactly one forced refresh"
        );
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].2, "stale-token");
        assert_eq!(
            calls[1].2, "fresh-token",
            "retry carries the refreshed token"
        );
        assert_eq!(calls[0].1, calls[1].1, "identical request retried");
    }

    #[tokio::test]
    async fn a_second_401_fails_without_looping() {
        let ((client, http), tokens) =
            arc_client(FakeHttp::scripted(vec![(401, "no"), (401, "still no")]));
        let err = client.list_messages(None, 5).await.unwrap_err();
        assert!(
            err.to_string().contains("401"),
            "honest status in the error: {err}"
        );
        assert_eq!(
            *tokens.forced.lock().unwrap(),
            1,
            "refresh attempted once, never looped"
        );
        assert_eq!(http.calls.lock().unwrap().len(), 2);
    }

    /// Gmail returns the same 403 envelopes Calendar/Tasks do, so it needs the
    /// same recovery seam: a scope-short grant must carry the reconnect
    /// marker, a disabled API must carry the DISTINCT one (never the reconnect
    /// one — reconnecting cannot switch an API on), and anything else must
    /// keep the plain `Gmail API HTTP …` string it always had.
    #[tokio::test]
    async fn a_403_is_classified_into_its_recovery_state() {
        const CONSOLE: &str =
            "https://console.developers.google.com/apis/api/gmail.googleapis.com/overview?project=7";

        let (client, _tokens) = arc_client(FakeHttp::scripted(vec![(
            403,
            r#"{"error":{"code":403,"status":"PERMISSION_DENIED","details":[
                {"@type":"type.googleapis.com/google.rpc.ErrorInfo",
                 "reason":"ACCESS_TOKEN_SCOPE_INSUFFICIENT"}]}}"#,
        )]));
        let err = client
            .0
            .list_messages(None, 5)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(crate::email::token_provider::NEEDS_RECONNECT_MARKER),
            "got: {err}"
        );

        let disabled = format!(
            r#"{{"error":{{"errors":[{{"reason":"accessNotConfigured",
            "message":"Access Not Configured. Enable it by visiting {CONSOLE} then retry."}}],
            "code":403}}}}"#
        );
        let (client, _tokens) = arc_client(FakeHttp::scripted(vec![(403, &disabled)]));
        let err = client
            .0
            .list_messages(None, 5)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(crate::email::token_provider::API_NOT_ENABLED_MARKER),
            "got: {err}"
        );
        assert!(
            !err.contains(crate::email::token_provider::NEEDS_RECONNECT_MARKER),
            "got: {err}"
        );
        assert_eq!(
            crate::email::token_provider::extract_enable_url(&err).as_deref(),
            Some(CONSOLE)
        );

        let (client, _tokens) = arc_client(FakeHttp::scripted(vec![(
            403,
            r#"{"error":{"code":403,"message":"The caller does not have permission"}}"#,
        )]));
        let err = client
            .0
            .list_messages(None, 5)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("Gmail API HTTP 403: "), "got: {err}");
        assert!(
            !err.contains("[google:") && !err.contains("[gmail:"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn list_url_carries_query_and_clamped_max() {
        let (client, _tokens) = arc_client(FakeHttp::scripted(vec![(200, r#"{"messages":[]}"#)]));
        let (client, http) = client;
        client
            .list_messages(Some("is:unread from:ada"), 9999)
            .await
            .unwrap();
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls[0].0, HttpMethod::Get);
        let url = url::Url::parse(&calls[0].1).unwrap();
        assert_eq!(url.path(), "/gmail/v1/users/me/messages");
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["q"], "is:unread from:ada");
        assert_eq!(q["maxResults"], "500", "clamped to Gmail's cap");
    }

    #[tokio::test]
    async fn get_message_refuses_a_malformed_id_before_any_request() {
        let (client, _tokens) = arc_client(FakeHttp::scripted(vec![]));
        let (client, http) = client;
        let err = client
            .get_message("../users/other/messages/x")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("malformed"), "{err}");
        assert!(http.calls.lock().unwrap().is_empty(), "no request was sent");
    }

    #[tokio::test]
    async fn send_posts_base64url_raw_and_returns_the_id() {
        let (client, _tokens) = arc_client(FakeHttp::scripted(vec![(
            200,
            r#"{"id":"sent-1","threadId":"t"}"#,
        )]));
        let (client, http) = client;
        let raw = build_rfc822("a@b.c", "Hi", "Body").unwrap();
        let id = client.send(&raw).await.unwrap();
        assert_eq!(id, "sent-1");
        let calls = http.calls.lock().unwrap();
        assert_eq!(calls[0].0, HttpMethod::Post);
        assert!(calls[0].1.ends_with("/gmail/v1/users/me/messages/send"));
        let body = calls[0].3.as_ref().expect("json body");
        let encoded = body["raw"].as_str().expect("raw field");
        assert!(!encoded.contains('='), "unpadded base64url");
        let decoded = URL_SAFE_NO_PAD.decode(encoded.as_bytes()).unwrap();
        assert_eq!(
            String::from_utf8(decoded).unwrap(),
            raw,
            "raw survives the encode"
        );
    }

    // -- the real reqwest transport against a loopback server ---------------

    /// Serve exactly one raw HTTP response on a loopback port, then close.
    /// Returns the bound `http://127.0.0.1:PORT` origin.
    async fn serve_once(response: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                // Drain the request line + headers so the client isn't reset
                // before it can read our response.
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

    /// The finding: `resp.text()` buffered any body the server chose to send.
    /// Now the fetch layer refuses past a byte ceiling — both when the length
    /// is declared and when it only shows up while streaming.
    #[tokio::test]
    async fn oversized_response_bodies_are_refused_at_the_fetch_layer() {
        let http = ReqwestGmailHttp::with_response_cap(1024).unwrap();

        // 1. Declared Content-Length over the cap → refused up front.
        let url = serve_once(with_content_length(&vec![b'a'; 4096])).await;
        let err = http
            .request(HttpMethod::Get, &url, "tok", None)
            .await
            .expect_err("a 4 KiB body must not pass a 1 KiB cap");
        assert!(
            err.to_string().contains("too large") && err.to_string().contains("declared 4096"),
            "got: {err}"
        );

        // 2. Chunked (no declared length) over the cap → refused mid-stream.
        let url = serve_once(chunked(4096, 512)).await;
        let err = http
            .request(HttpMethod::Get, &url, "tok", None)
            .await
            .expect_err("a chunked 4 KiB body must not pass a 1 KiB cap");
        assert!(
            err.to_string().contains("exceeded the 1024-byte cap"),
            "got: {err}"
        );

        // 3. Control: a body under the cap still comes back intact.
        let url = serve_once(with_content_length(br#"{"id":"m-1"}"#)).await;
        let (status, body) = http
            .request(HttpMethod::Get, &url, "tok", None)
            .await
            .unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, r#"{"id":"m-1"}"#);
    }

    /// Build a client whose FakeHttp stays reachable for assertions.
    fn arc_client(
        http: FakeHttp,
    ) -> (
        (GmailClient, std::sync::Arc<FakeHttp>),
        std::sync::Arc<FakeTokens>,
    ) {
        let http = std::sync::Arc::new(http);
        let tokens = std::sync::Arc::new(FakeTokens::new());
        struct SharedHttp(std::sync::Arc<FakeHttp>);
        impl GmailHttp for SharedHttp {
            fn request<'a>(
                &'a self,
                method: HttpMethod,
                url: &'a str,
                bearer: &'a str,
                json_body: Option<&'a serde_json::Value>,
            ) -> BoxFuture<'a, anyhow::Result<(u16, String)>> {
                self.0.request(method, url, bearer, json_body)
            }
        }
        struct SharedTokens(std::sync::Arc<FakeTokens>);
        impl TokenProvider for SharedTokens {
            fn access_token(&self, force_refresh: bool) -> BoxFuture<'_, anyhow::Result<String>> {
                self.0.access_token(force_refresh)
            }
        }
        (
            (
                GmailClient::new(
                    Box::new(SharedHttp(http.clone())),
                    Arc::new(SharedTokens(tokens.clone())),
                ),
                http,
            ),
            tokens,
        )
    }
}
