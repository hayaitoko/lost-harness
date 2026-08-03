//! `fetch_url` — the web-content core tool (PLAN §8 M3 item 10, the "headless
//! browser" slot at a v1 fidelity: an HTTP GET + readable-text extraction, NOT
//! a JS-rendering engine — real page *control* is the M5 computer-use flagship).
//!
//! This is the FIRST `External` (network-egress) tool, so it is the security
//! spine's first real off-box reach. Two invariants shape it:
//!
//! * **Egress consent** — `RiskClass::External` ⇒ it routes through the
//!   approval spine and surfaces its `destination` (the URL host) so the user
//!   consents to *where* the call goes, not just *that* a tool ran.
//! * **SSRF is the load-bearing guard.** An agent-controlled fetcher is a
//!   textbook server-side-request-forgery vector: pointed at `localhost`, an
//!   RFC-1918 host, or the cloud-metadata endpoint `169.254.169.254`, it would
//!   read internal resources. So every hop (initial + each redirect) is
//!   re-validated: scheme ∈ {http,https}, the string-level private-host check
//!   (`agent::egress::is_private_endpoint`, catches `.local`/`localhost`/
//!   literals), AND a DNS resolution whose every resolved IP is classified
//!   against a full block-list (loopback, RFC-1918, link-local incl. metadata,
//!   CGNAT, unspecified, multicast/reserved, IPv6 ULA/link-local, and
//!   IPv4-mapped forms of all of those). Redirects are followed **manually** so
//!   the DNS re-check runs on every hop — reqwest's own redirect policy can't
//!   `await` a resolution.
//!
//! Residual (documented): a DNS-rebind TOCTOU between the resolve-check and
//! reqwest's own resolution for the actual connection. Closing it needs a
//! custom connector pinned to the vetted IP; noted like the other accepted
//! races in this codebase. The response body is size-capped, content-type is
//! restricted to text, and the extracted text is length-capped so a hostile
//! page can't blow up the prompt. Output is guard-wrapped as untrusted by the
//! dispatcher's `run_turn`, same as any tool result.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use serde_json::json;

use crate::agent::egress::is_private_endpoint;
use crate::tools::{Capability, ExecCtx, RiskClass, Tool, ToolInput, ToolResult};

/// Per-request wall-clock timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Max redirects followed (each re-validated for SSRF).
const MAX_REDIRECTS: usize = 5;
/// Cap on downloaded body bytes — refuse to buffer more than this.
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024; // 2 MiB
/// Cap on the extracted text handed back to the model.
const MAX_TEXT_CHARS: usize = 20_000;
/// A stable, honest User-Agent.
const USER_AGENT: &str = "LostHarness/0.1 (+local-first assistant; web fetch)";

/// Fetch a web page and return its readable text. Holds nothing — a fresh
/// `reqwest::Client` is built per call (redirects handled manually).
pub struct FetchUrlTool;

impl FetchUrlTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for FetchUrlTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for FetchUrlTool {
    fn name(&self) -> &str {
        "fetch_url"
    }

    fn description(&self) -> &str {
        "Fetch a web page and return its readable text. \
         args: {\"url\": \"https://example.com/page\"}. Only public http/https \
         URLs — internal, localhost, and private-network addresses are refused. \
         Returns the page title and text (truncated for long pages)."
    }

    fn requires(&self) -> &[Capability] {
        &[Capability::WebResearch]
    }

    fn risk(&self) -> RiskClass {
        // Reaches off this machine → the approval spine, with a destination.
        RiskClass::External
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "an http/https URL to fetch" }
            },
            "required": ["url"],
            "additionalProperties": false
        })
    }

    /// The egress destination surfaced for approval — the URL host. `None` if
    /// the arg isn't a parseable URL (the run path will reject it anyway).
    fn destination(&self, args: &serde_json::Value) -> Option<String> {
        let raw = args.get("url").and_then(|v| v.as_str())?;
        url::Url::parse(raw.trim())
            .ok()?
            .host_str()
            .map(str::to_string)
    }

    fn run<'a>(
        &'a self,
        input: ToolInput,
        _ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let raw = match input.args.get("url").and_then(|v| v.as_str()) {
                Some(u) if !u.trim().is_empty() => u.trim().to_string(),
                _ => {
                    return ToolResult::Err(
                        "fetch_url requires a non-empty string \"url\" arg".to_string(),
                    )
                }
            };
            match fetch_readable(&raw).await {
                Ok(v) => ToolResult::Ok(v),
                Err(e) => ToolResult::Err(e),
            }
        })
    }
}

/// The orchestration: validate + manually-redirected GET + extract. Split out
/// so it's reusable and the tool body stays thin.
async fn fetch_readable(raw: &str) -> Result<serde_json::Value, String> {
    let mut current = url::Url::parse(raw).map_err(|e| format!("not a valid URL: {e}"))?;

    let mut hops = 0usize;
    let resp = loop {
        let vetted = ssrf_check(&current).await?;
        let hostname = current.host_str().unwrap_or("?").to_string();

        // Fresh client per hop so DNS is pinned to the IPs that passed
        // ssrf_check — closes the DNS-rebind TOCTOU gap (each redirect
        // re-resolves and re-pins).
        let client = build_hop_client(&hostname, &vetted, None)?;

        let resp = client
            .get(current.clone())
            .send()
            .await
            .map_err(|e| format!("request to {hostname} failed: {e}"))?;

        if resp.status().is_redirection() {
            hops += 1;
            if hops > MAX_REDIRECTS {
                return Err(format!("too many redirects (>{MAX_REDIRECTS})"));
            }
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| "redirect response without a Location header".to_string())?;
            // Resolve relative redirects against the current URL; the next loop
            // iteration re-runs the full SSRF check on the new target.
            current = current
                .join(location)
                .map_err(|e| format!("bad redirect Location: {e}"))?;
            continue;
        }
        break resp;
    };

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("server returned HTTP {}", status.as_u16()));
    }

    // Only extract text from text-ish content; refuse binary so we never buffer
    // a media/download blob into the prompt.
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_texty = content_type.is_empty()
        || content_type.starts_with("text/")
        || content_type.contains("html")
        || content_type.contains("xml")
        || content_type.contains("json")
        || content_type.contains("javascript");
    if !is_texty {
        return Err(format!(
            "refusing to read non-text content (Content-Type: {content_type})"
        ));
    }

    let final_url = resp.url().to_string();
    let body = read_capped(resp).await?;
    let (title, text) = html_to_text(&body);

    Ok(json!({
        "requested_url": raw,
        "final_url": final_url,
        "status": status.as_u16(),
        "title": title,
        "text": text,
        "truncated": text.chars().count() >= MAX_TEXT_CHARS,
    }))
}

/// Read a response body, refusing to buffer more than [`MAX_BODY_BYTES`].
async fn read_capped(resp: reqwest::Response) -> Result<String, String> {
    use tokio_stream::StreamExt;
    // A declared oversized Content-Length is an early, cheap refusal.
    if let Some(len) = resp.content_length() {
        if len as usize > MAX_BODY_BYTES {
            return Err(format!(
                "response too large ({len} bytes > {MAX_BODY_BYTES} cap)"
            ));
        }
    }
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("error reading body: {e}"))?;
        if buf.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(format!("response exceeded the {MAX_BODY_BYTES}-byte cap"));
        }
        buf.extend_from_slice(&chunk);
    }
    // Lossy UTF-8: a stray non-UTF-8 byte shouldn't fail the whole read.
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Build the reqwest client used for one hop of `fetch_readable`. Pins DNS
/// resolution of `hostname` to `vetted` (the addresses `ssrf_check` already
/// classified as public) and disables all proxy usage — see the module doc
/// for why: without `.no_proxy()`, reqwest picks up HTTP_PROXY/HTTPS_PROXY/
/// ALL_PROXY at build time, and a proxied request goes to the proxy, which
/// resolves the hostname itself. The vetted address then receives nothing and
/// the DNS-rebind TOCTOU this pin exists to close is wide open.
///
/// `extra_proxy` is `None` on the production call site in `fetch_readable`.
/// It exists so the regression test can hand this SAME builder an explicit
/// proxy and prove `.no_proxy()` below wins — without this seam a test would
/// have to build its own client, and a mutation that deletes `.no_proxy()`
/// from production would survive the suite. Proven by the regression test
/// `a_configured_proxy_cannot_defeat_the_pinned_address`.
fn build_hop_client(
    hostname: &str,
    vetted: &[SocketAddr],
    extra_proxy: Option<reqwest::Proxy>,
) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(USER_AGENT)
        .resolve_to_addrs(hostname, vetted);
    if let Some(proxy) = extra_proxy {
        builder = builder.proxy(proxy);
    }
    // Order matters — reqwest's `no_proxy()` CLEARS the proxy list, so it must
    // come after any `.proxy(..)` above.
    builder
        .no_proxy()
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))
}

/// Validate one hop: scheme, the string-level private-host check, and a DNS
/// resolution whose every resolved IP must be public. Returns a human-readable
/// refusal reason on any failure.
async fn ssrf_check(url: &url::Url) -> Result<Vec<SocketAddr>, String> {
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(format!("refusing scheme \"{other}\" (only http/https)")),
    }
    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;

    // Fast, resolution-free refusal for localhost / *.local / v4-literal
    // private ranges / tailnet suffixes.
    if is_private_endpoint(url.as_str()) {
        return Err(format!(
            "refusing to fetch a private/internal address ({host})"
        ));
    }
    // Same suffix intent, but tolerant of a trailing root-label dot
    // (`localhost.`, `foo.internal.`) that defeats the exact/suffix match above.
    let norm_host = host.trim_end_matches('.').to_ascii_lowercase();
    if norm_host == "localhost"
        || norm_host.ends_with(".local")
        || norm_host.ends_with(".lan")
        || norm_host.ends_with(".internal")
        || norm_host.ends_with(".ts.net")
    {
        return Err(format!(
            "refusing to fetch a private/internal address ({host})"
        ));
    }

    // An IP literal (v4 dotted-decimal after url-crate normalization, or a
    // bracketed v6) — classify it directly, no DNS. This also stops a bracketed
    // v6 literal from failing closed at `lookup_host` (which rejects brackets).
    let port = url.port_or_known_default().unwrap_or(80);
    let literal = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = literal.parse::<IpAddr>() {
        return if is_blocked_ip(ip) {
            Err(format!("refusing to fetch a non-public address ({ip})"))
        } else {
            Ok(vec![SocketAddr::new(ip, port)])
        };
    }

    // A hostname — resolve and classify every returned IP. A host that WON'T
    // resolve is a refusal (we can't verify it's public → fail closed).
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("could not resolve host \"{host}\": {e}"))?;
    let mut vetted: Vec<SocketAddr> = Vec::new();
    for addr in addrs {
        if is_blocked_ip(addr.ip()) {
            return Err(format!(
                "refusing to fetch \"{host}\" — it resolves to a non-public address ({})",
                addr.ip()
            ));
        }
        vetted.push(addr);
    }
    if vetted.is_empty() {
        return Err(format!("host \"{host}\" resolved to no addresses"));
    }
    Ok(vetted)
}

/// Is `ip` a non-public address a web fetch must never reach? Covers loopback,
/// RFC-1918, link-local (incl. the `169.254.169.254` metadata endpoint), CGNAT,
/// unspecified, broadcast, documentation, multicast/reserved, IPv6 ULA +
/// link-local, and **every** IPv4-in-IPv6 embedding (mapped `::ffff:`,
/// deprecated compatible `::a.b.c.d`, NAT64 `64:ff9b::/96`, 6to4 `2002::/16`) —
/// so an internal IPv4 can't be smuggled through a domain's AAAA record.
pub(crate) fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_ipv4(v4),
        IpAddr::V6(v6) => {
            // If the v6 address embeds a v4 (any of the four standard forms),
            // classify that v4 — a blocked v4 wins.
            if let Some(v4) = embedded_ipv4(&v6) {
                if is_blocked_ipv4(v4) {
                    return true;
                }
            }
            let seg = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
                || (seg[0] & 0xfe00) == 0xfc00 // unique-local fc00::/7
        }
    }
}

/// Extract the IPv4 address a v6 address embeds, across all four standard
/// forms: IPv4-mapped `::ffff:a.b.c.d`, deprecated IPv4-compatible `::a.b.c.d`,
/// NAT64 well-known prefix `64:ff9b::a.b.c.d`, and 6to4 `2002:WWXX:YYZZ::`.
/// `None` for a native v6 address. Callers run the result through
/// `is_blocked_ipv4`, so a public embedded v4 (e.g. a real 6to4 relay) stays
/// allowed while an internal one is caught.
fn embedded_ipv4(v6: &std::net::Ipv6Addr) -> Option<std::net::Ipv4Addr> {
    let seg = v6.segments();
    let v4_from = |hi: u16, lo: u16| {
        let a = hi.to_be_bytes();
        let b = lo.to_be_bytes();
        std::net::Ipv4Addr::new(a[0], a[1], b[0], b[1])
    };
    let first_five_zero = seg[0] == 0 && seg[1] == 0 && seg[2] == 0 && seg[3] == 0 && seg[4] == 0;
    // Mapped (::ffff:) or compatible (::, seg[5]==0) — both put the v4 last.
    if first_five_zero && (seg[5] == 0 || seg[5] == 0xffff) {
        return Some(v4_from(seg[6], seg[7]));
    }
    // NAT64 64:ff9b::/96 — v4 in the last 32 bits.
    if seg[0] == 0x0064
        && seg[1] == 0xff9b
        && seg[2] == 0
        && seg[3] == 0
        && seg[4] == 0
        && seg[5] == 0
    {
        return Some(v4_from(seg[6], seg[7]));
    }
    // 6to4 2002:WWXX:YYZZ::/16 — v4 in bits 16..48.
    if seg[0] == 0x2002 {
        return Some(v4_from(seg[1], seg[2]));
    }
    None
}

fn is_blocked_ipv4(v4: std::net::Ipv4Addr) -> bool {
    let o = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local() // 169.254.0.0/16 — cloud metadata
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_documentation()
        || o[0] == 0 // 0.0.0.0/8 "this host"
        || (o[0] == 100 && (64..=127).contains(&o[1])) // CGNAT 100.64.0.0/10
        || o[0] >= 224 // multicast (224/4) + reserved (240/4)
}

/// Extract `(title, readable_text)` from an HTML (or plain-text) body. Strips
/// `<script>`/`<style>`, pulls `<title>`, removes remaining tags, decodes the
/// common HTML entities, collapses whitespace, and caps the length. Best-effort
/// and dependency-free — not a full parser, just a reader.
pub(crate) fn html_to_text(body: &str) -> (Option<String>, String) {
    static SCRIPT_STYLE: OnceLock<Regex> = OnceLock::new();
    static TITLE: OnceLock<Regex> = OnceLock::new();
    static TAG: OnceLock<Regex> = OnceLock::new();
    static WS: OnceLock<Regex> = OnceLock::new();

    // No backreferences in the `regex` crate — spell out each block explicitly.
    let script_style = SCRIPT_STYLE.get_or_init(|| {
        Regex::new(
            r"(?is)<script\b[^>]*>.*?</script>|<style\b[^>]*>.*?</style>|<noscript\b[^>]*>.*?</noscript>|<template\b[^>]*>.*?</template>",
        )
        .unwrap()
    });
    let title_re = TITLE.get_or_init(|| Regex::new(r"(?is)<title[^>]*>(.*?)</title>").unwrap());
    let tag = TAG.get_or_init(|| Regex::new(r"(?s)<[^>]+>").unwrap());
    let ws = WS.get_or_init(|| Regex::new(r"[ \t\r\f\v]+").unwrap());

    let title = title_re
        .captures(body)
        .and_then(|c| c.get(1))
        .map(|m| decode_entities(m.as_str()).trim().to_string())
        .filter(|t| !t.is_empty());

    // Drop script/style blocks, then turn block-ish tags into newlines so text
    // doesn't run together, then strip all remaining tags.
    let no_scripts = script_style.replace_all(body, " ");
    let block_broken = no_scripts
        .replace("</p>", "\n")
        .replace("<br>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n")
        .replace("</div>", "\n")
        .replace("</li>", "\n")
        .replace("</h1>", "\n")
        .replace("</h2>", "\n")
        .replace("</h3>", "\n");
    let no_tags = tag.replace_all(&block_broken, " ");
    let decoded = decode_entities(&no_tags);

    // Collapse intra-line whitespace, then squeeze blank lines.
    let collapsed = ws.replace_all(&decoded, " ");
    let mut out = String::new();
    let mut blank_run = 0;
    for line in collapsed.lines() {
        let t = line.trim();
        if t.is_empty() {
            blank_run += 1;
            if blank_run <= 1 && !out.is_empty() {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(t);
            out.push('\n');
        }
    }
    let text: String = out.trim().chars().take(MAX_TEXT_CHARS).collect();
    (title, text)
}

/// Decode the handful of HTML entities that matter for readability, including
/// numeric (`&#NN;` / `&#xHH;`) forms. Unknown entities are left verbatim.
fn decode_entities(s: &str) -> String {
    // Named first (cheap replaces), then numeric via a regex pass.
    let named = s
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
        .replace("&hellip;", "…");
    static NUM: OnceLock<Regex> = OnceLock::new();
    let num = NUM.get_or_init(|| Regex::new(r"&#(x?)([0-9A-Fa-f]+);").unwrap());
    num.replace_all(&named, |c: &regex::Captures| {
        let is_hex = &c[1] == "x";
        let code = if is_hex {
            u32::from_str_radix(&c[2], 16).ok()
        } else {
            c[2].parse::<u32>().ok()
        };
        code.and_then(char::from_u32)
            .map(|ch| ch.to_string())
            .unwrap_or_else(|| c[0].to_string())
    })
    .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn blocks_every_internal_ip_family() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            // Every IPv4-in-IPv6 embedding of an internal address must be caught,
            // not just the ::ffff: mapped form (review finding #1).
            "::169.254.169.254",  // deprecated IPv4-compatible
            "::127.0.0.1",        // deprecated IPv4-compatible loopback
            "64:ff9b::a9fe:a9fe", // NAT64 of 169.254.169.254
            "64:ff9b::7f00:1",    // NAT64 of 127.0.0.1
            "2002:7f00:1::",      // 6to4 embedding 127.0.0.1
            "2002:a9fe:a9fe::",   // 6to4 embedding 169.254.169.254
        ] {
            let parsed: IpAddr = ip.parse().unwrap();
            assert!(is_blocked_ip(parsed), "should block {ip}");
        }
        // Public addresses are allowed — including 6to4/NAT64 that embed a
        // PUBLIC v4 (decode-and-classify, not block-the-whole-prefix).
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "2606:2800:220:1::1",
            "64:ff9b::808:808", // NAT64 of 8.8.8.8 (public)
            "2002:808:808::",   // 6to4 of 8.8.8.8 (public)
        ] {
            let parsed: IpAddr = ip.parse().unwrap();
            assert!(!is_blocked_ip(parsed), "should allow {ip}");
        }
        // Direct family spot-checks.
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))));
        assert!(!is_blocked_ip(IpAddr::V6(Ipv6Addr::new(
            0x2606, 0x2800, 0, 0, 0, 0, 0, 1
        ))));
    }

    #[tokio::test]
    async fn run_refuses_non_http_schemes_and_private_hosts_without_network() {
        let tool = FetchUrlTool::new();
        let ctx = ExecCtx::default();
        for bad in [
            "file:///etc/passwd",
            "ftp://example.com/x",
            "http://localhost/admin",
            "http://localhost./admin", // trailing-dot bypass (finding #3)
            "http://127.0.0.1:8080/",
            "https://192.168.1.1/",
            "http://foo.local/",
            "http://[::1]/",                    // bracketed v6 loopback
            "http://[fc00::1]/",                // bracketed v6 ULA literal
            "http://[::ffff:169.254.169.254]/", // bracketed mapped metadata
            "not a url",
            "",
        ] {
            let out = tool.run(ToolInput::new(json!({ "url": bad })), &ctx).await;
            assert!(matches!(out, ToolResult::Err(_)), "should refuse {bad:?}");
        }
    }

    #[test]
    fn destination_is_the_host() {
        let tool = FetchUrlTool::new();
        assert_eq!(
            tool.destination(&json!({ "url": "https://example.com/a/b?q=1" })),
            Some("example.com".to_string())
        );
        assert_eq!(tool.destination(&json!({ "url": "garbage" })), None);
        assert_eq!(tool.destination(&json!({})), None);
    }

    #[test]
    fn html_extraction_strips_scripts_tags_and_decodes_entities() {
        let html = r#"
            <html><head><title>Hello &amp; World</title>
            <style>.x{color:red}</style></head>
            <body>
              <script>alert('nope')</script>
              <h1>Heading</h1>
              <p>First&nbsp;paragraph with &lt;b&gt; and &#233; and &#x2014;.</p>
              <div>Second block</div>
            </body></html>
        "#;
        let (title, text) = html_to_text(html);
        assert_eq!(title.as_deref(), Some("Hello & World"));
        assert!(!text.contains("alert"), "script contents must be gone");
        assert!(!text.contains("color:red"), "style contents must be gone");
        // Real markup is stripped BEFORE entity decoding, so page tags are gone…
        assert!(!text.contains("<h1>"), "page tags must be stripped");
        assert!(!text.contains("<p>"), "page tags must be stripped");
        assert!(!text.contains("<div>"), "page tags must be stripped");
        assert!(text.contains("Heading"));
        assert!(text.contains("First paragraph"), "&nbsp; → space");
        // …but a decoded entity can reintroduce a literal, inert `<b>` as text.
        assert!(text.contains("<b>"), "&lt;b&gt; decodes to literal <b>");
        assert!(text.contains('é'), "numeric entity decodes");
        assert!(text.contains('—'), "hex entity decodes");
        assert!(text.contains("Second block"));
    }

    #[test]
    fn text_output_is_length_capped() {
        let big = format!("<p>{}</p>", "word ".repeat(20_000));
        let (_t, text) = html_to_text(&big);
        assert!(text.chars().count() <= MAX_TEXT_CHARS);
    }

    #[test]
    fn ssrf_check_returns_vetted_addr_for_public_ip_literal() {
        // An IP literal bypasses DNS entirely — ssrf_check returns it as the
        // only vetted address so the caller can pin it.
        let url = url::Url::parse("https://8.8.8.8/path").unwrap();
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(ssrf_check(&url));
        assert!(result.is_ok(), "8.8.8.8 should be allowed: {:?}", result);
        let vetted = result.unwrap();
        assert_eq!(vetted.len(), 1);
        assert_eq!(
            vetted[0],
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 443)
        );
    }

    #[test]
    fn ssrf_check_refuses_private_ip_literal_and_returns_addr() {
        // A private IP literal is refused — the error may come from
        // is_private_endpoint (before the IP literal check) or from the
        // IP literal check itself depending on the address.
        for bad_url in [
            "http://127.0.0.1:9000/",
            "http://169.254.169.254/latest/meta-data/",
            "http://192.168.1.1/",
        ] {
            let url = url::Url::parse(bad_url).unwrap();
            let result = tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(ssrf_check(&url));
            assert!(
                result.is_err(),
                "private IP literal {bad_url} should be blocked"
            );
        }

        // A public IP with a non-default port — port is preserved in the
        // vetted address.
        let url = url::Url::parse("http://1.1.1.1:8080/").unwrap();
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(ssrf_check(&url));
        assert!(result.is_ok());
        let vetted = result.unwrap();
        assert_eq!(vetted.len(), 1);
        assert_eq!(
            vetted[0],
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 8080)
        );
    }

    #[tokio::test]
    async fn resolve_to_addrs_pins_connection_and_bypasses_dns() {
        // Verifies the DNS pinning mechanism that closes the rebind TOCTOU:
        // a reqwest Client built with resolve_to_addrs connects to the
        // *pinned* address even for a hostname that would resolve differently
        // if queried — no DNS re-resolution happens at .send() time.

        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        // Start a local TCP server on the tokio runtime.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local = listener.local_addr().unwrap();

        let serve = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let response =
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: text/plain\r\n\r\nOK";
            // Ignore errors — the response may race with the client closing.
            let _ = stream.write_all(response).await;
        });

        // Build a client via the PRODUCTION `build_hop_client` that pins
        // "pinned-dns-test.local" to our local server. If the client resolved
        // the hostname via real DNS it would never reach 127.0.0.1 — proving
        // the override works, against the exact function `fetch_readable` calls.
        let client = build_hop_client("pinned-dns-test.local", &[local], None).unwrap();

        let resp = client
            .get("http://pinned-dns-test.local/")
            .send()
            .await
            .expect("pinned DNS request should reach local server");

        assert!(resp.status().is_success());

        serve.await.unwrap();
    }
    #[tokio::test]
    async fn a_configured_proxy_cannot_defeat_the_pinned_address() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        // Two listeners: the PINNED target and a PROXY sink.
        let pinned = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pinned_addr = pinned.local_addr().unwrap();
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();

        let pinned_hits = Arc::new(AtomicUsize::new(0));
        let proxy_hits = Arc::new(AtomicUsize::new(0));

        let ph = pinned_hits.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = pinned.accept().await {
                ph.fetch_add(1, Ordering::SeqCst);
                let _ = s
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nPINNED")
                    .await;
            }
        });
        let xh = proxy_hits.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = proxy.accept().await {
                xh.fetch_add(1, Ordering::SeqCst);
                let _ = s
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nPROXY")
                    .await;
            }
        });

        // The PRODUCTION builder (`build_hop_client`, the same function
        // `fetch_readable` calls for every hop), handed an explicit proxy. An
        // explicit `.proxy(..)` exercises exactly the path HTTP_PROXY/ALL_PROXY
        // take (reqwest reads those at build time) without mutating
        // process-global env from a test that runs in parallel with others.
        // `build_hop_client`'s `.no_proxy()` must win, or the SSRF pin is
        // decorative.
        let client = build_hop_client(
            "pinned-proxy-probe.local",
            &[pinned_addr],
            Some(reqwest::Proxy::all(format!("http://{proxy_addr}")).unwrap()),
        )
        .unwrap();

        let body = client
            .get("http://pinned-proxy-probe.local/x")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        assert_eq!(
            body, "PINNED",
            "the vetted address must receive the request, not the proxy"
        );
        assert_eq!(
            proxy_hits.load(Ordering::SeqCst),
            0,
            "the proxy must receive no connection at all"
        );
        assert_eq!(pinned_hits.load(Ordering::SeqCst), 1);
    }
}
