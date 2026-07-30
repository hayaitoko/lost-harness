//! MCP Streamable HTTP transport for remote servers. The MCP specification
//! replaced the legacy HTTP+SSE transport with this single POST/GET endpoint;
//! responses may still be JSON or SSE, so both are accepted here.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicI64, Ordering};

use super::fetch::is_blocked_ip;
use super::mcp::{McpToolAnnotations, McpToolDescriptor, McpTransport};

const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
const RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_REDIRECT_HOPS: usize = 5;

/// The URL rules that hold for EVERY destination this transport will talk to,
/// whether the user typed it or a server redirected to it: HTTPS is mandatory
/// except for a loopback HTTP endpoint used for local development, and
/// credentials/fragments are rejected so they cannot leak into logs or
/// persisted settings.
///
/// The query-string rule is deliberately NOT here — see [`validate_endpoint`]
/// and [`validate_redirect_target`].
fn validate_url_shape(raw: &str) -> Result<url::Url, String> {
    let url =
        url::Url::parse(raw.trim()).map_err(|_| "MCP endpoint must be a valid URL".to_string())?;
    if url.username() != "" || url.password().is_some() || url.fragment().is_some() {
        return Err("MCP endpoint URLs may not contain credentials or fragments".to_string());
    }
    if url.scheme() != "https" && !(url.scheme() == "http" && is_loopback_host(&url)) {
        return Err(
            "MCP endpoints must use HTTPS (HTTP is allowed only for localhost)".to_string(),
        );
    }
    Ok(url)
}

/// A validated Streamable HTTP endpoint as CONFIGURED BY THE USER.
///
/// On top of [`validate_url_shape`] a registered endpoint may not carry a query
/// string: the MCP spec puts no request state in the URL, so a query on a
/// hand-entered endpoint is far more likely to be a pasted secret (a token, an
/// API key) that would then be persisted in the registration store and echoed
/// into logs.
pub fn validate_endpoint(raw: &str) -> Result<url::Url, String> {
    let url = validate_url_shape(raw)?;
    if url.query().is_some() {
        return Err("MCP endpoint URLs may not contain a query string".to_string());
    }
    Ok(url)
}

/// A validated redirect target as ISSUED BY THE SERVER.
///
/// Same rules as a configured endpoint EXCEPT that a query string is allowed.
/// A server-issued `Location` is a different case from a hand-entered endpoint:
/// real MCP deployments 307 a request onto a session-bearing URL such as
/// `…/mcp?session=abc`, and refusing that made every RPC fail with no recovery
/// route. Nothing else is relaxed — the scheme rule (so no HTTPS→plain-HTTP
/// hop), the credential/fragment rule, the per-hop address vetting and the
/// hop limit all still apply, and the session header is still never forwarded
/// across the boundary.
fn validate_redirect_target(raw: &str) -> Result<url::Url, String> {
    validate_url_shape(raw)
}

/// The exact destination a cached client is pinned to: the host key plus the
/// sorted vetted addresses. A fresh vet that yields a different key must build
/// a new client, so a pooled connection never outlives the address it was
/// vetted against.
type PinKey = (String, Vec<SocketAddr>);

pub struct HttpMcpTransport {
    endpoint: url::Url,
    /// True when the REGISTERED endpoint is an explicit loopback host — the one
    /// case where a loopback destination is permitted (the local-development
    /// server `validate_endpoint` already blesses over plain HTTP). Even then
    /// only *loopback* is carved out: RFC-1918, CGNAT, ULA and the
    /// `169.254.169.254` metadata range stay blocked on every hop.
    allow_loopback: bool,
    /// The address-pinned client, kept so ordinary RPC traffic still reuses a
    /// connection pool instead of re-handshaking per call. Rebuilt whenever a
    /// fresh vet resolves to different addresses.
    pinned_client: tokio::sync::Mutex<Option<(PinKey, reqwest::Client)>>,
    session_id: tokio::sync::Mutex<Option<String>>,
    next_id: AtomicI64,
}

impl HttpMcpTransport {
    /// Assemble a transport around an ALREADY-validated endpoint. No network
    /// I/O; `connect` adds the handshake on top.
    fn from_validated(endpoint: url::Url) -> Self {
        Self {
            allow_loopback: is_loopback_host(&endpoint),
            endpoint,
            pinned_client: tokio::sync::Mutex::new(None),
            session_id: tokio::sync::Mutex::new(None),
            next_id: AtomicI64::new(0),
        }
    }

    /// Build and initialize a remote MCP connection. A failed initialize never
    /// enters the runtime or the persisted registration store.
    pub async fn connect(raw_endpoint: &str) -> Result<Self, String> {
        let transport = Self::from_validated(validate_endpoint(raw_endpoint)?);
        transport
            .rpc(
                "initialize",
                serde_json::json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "lost-harness", "version": env!("CARGO_PKG_VERSION")},
                }),
                false,
            )
            .await
            .map_err(|e| format!("MCP initialize handshake failed: {e}"))?;
        transport
            .notify("notifications/initialized", serde_json::json!({}))
            .await
            .map_err(|e| format!("MCP initialized notification failed: {e}"))?;
        Ok(transport)
    }

    /// Hand back a client whose DNS is pinned to `hop`'s vetted addresses,
    /// reusing the cached one only when the pin is byte-identical.
    async fn pinned_client_for(&self, hop: &VettedHop) -> Result<reqwest::Client, String> {
        let key: PinKey = (hop.host_key.clone(), hop.addrs.clone());
        let mut guard = self.pinned_client.lock().await;
        if let Some((cached_key, client)) = guard.as_ref() {
            if *cached_key == key {
                return Ok(client.clone());
            }
        }
        let client = build_pinned_client(&hop.host_key, &hop.addrs, None)?;
        *guard = Some((key, client.clone()));
        Ok(client)
    }

    /// Send a POST, manually following redirects with per-hop security checks.
    ///
    /// H-02: Redirects are not automatic. EVERY hop — including hop 0, the
    /// registered endpoint itself — is resolved and address-vetted, and the
    /// request is then issued through a client PINNED to the exact addresses
    /// that passed. Vetting a name and letting reqwest re-resolve it at connect
    /// time is a DNS-rebind hole: the second answer never faced the check. Each
    /// hop also re-validates the URL, rejects an HTTPS→HTTP downgrade, and never
    /// forwards the session token across a redirect boundary.
    async fn post(
        &self,
        payload: serde_json::Value,
        include_protocol: bool,
    ) -> Result<reqwest::Response, String> {
        let mut url = self.endpoint.clone();
        let mut hop = 0usize;
        // Snapshot the session token before following any redirects so we
        // never forward it across a redirect boundary.
        let session_token = self.session_id.lock().await.clone();

        loop {
            let vetted = vet_hop(&url, self.allow_loopback).await?;
            let client = self.pinned_client_for(&vetted).await?;

            let mut request = client
                .post(url.clone())
                .header(
                    reqwest::header::ACCEPT,
                    "application/json, text/event-stream",
                )
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .json(&payload);
            if include_protocol {
                request = request.header("MCP-Protocol-Version", MCP_PROTOCOL_VERSION);
            }
            // H-02: Session header only on the original hop — never forwarded
            // across a redirect boundary.
            if hop == 0 {
                if let Some(ref session) = session_token {
                    request = request.header("Mcp-Session-Id", session.clone());
                }
            }

            let response = request
                .send()
                .await
                .map_err(|e| format!("MCP HTTP POST failed: {e}"))?;

            // Manual redirect following with per-hop security gates.
            if response.status().is_redirection() {
                if hop >= MAX_REDIRECT_HOPS {
                    return Err(format!(
                        "MCP HTTP redirect limit ({MAX_REDIRECT_HOPS}) exceeded"
                    ));
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .ok_or("MCP redirect response missing Location header".to_string())?;
                let location_str = location
                    .to_str()
                    .map_err(|_| "MCP redirect Location is not valid UTF-8".to_string())?;
                let new_url = url::Url::parse(location_str)
                    .or_else(|_| url.join(location_str))
                    .map_err(|_| "MCP redirect Location could not be resolved".to_string())?;

                // H-02: URL-level hop rules (re-validation + downgrade refusal).
                // The ADDRESS-level check is not repeated here — the next loop
                // iteration's `vet_hop` both classifies the new target and
                // returns the addresses the connection is pinned to, so there is
                // exactly one resolution per hop and no check/connect gap.
                check_hop_transition(&url, &new_url)?;

                hop += 1;
                url = new_url;
                continue;
            }

            return Ok(response);
        }
    }

    async fn rpc(
        &self,
        method: &str,
        params: serde_json::Value,
        include_protocol: bool,
    ) -> Result<serde_json::Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut response = self
            .post(
                serde_json::json!({"jsonrpc":"2.0", "id": id, "method": method, "params": params}),
                include_protocol,
            )
            .await?;
        self.capture_session(&response).await;
        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("")
            .to_string();

        // M-01: Stream response body through a byte counter instead of
        // buffering everything into .bytes() before checking the limit.
        let mut body = Vec::with_capacity(4096);
        loop {
            let chunk = response
                .chunk()
                .await
                .map_err(|e| format!("MCP HTTP response read failed: {e}"))?;
            match chunk {
                Some(bytes) => {
                    if body.len().saturating_add(bytes.len()) > MAX_RESPONSE_BYTES {
                        return Err("MCP HTTP response exceeds 4 MB limit".to_string());
                    }
                    body.extend_from_slice(&bytes);
                }
                None => break,
            }
        }

        if !status.is_success() {
            return Err(format!(
                "MCP HTTP {status}: {}",
                String::from_utf8_lossy(&body)
                    .chars()
                    .take(700)
                    .collect::<String>()
            ));
        }
        if content_type.starts_with("application/json") {
            return parse_jsonrpc(&body, id);
        }
        if content_type.starts_with("text/event-stream") {
            return parse_sse_jsonrpc(&body, id);
        }
        Err(format!(
            "MCP HTTP response has unsupported Content-Type `{content_type}`"
        ))
    }

    async fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), String> {
        let response = self
            .post(
                serde_json::json!({"jsonrpc":"2.0", "method": method, "params": params}),
                true,
            )
            .await?;
        self.capture_session(&response).await;
        if response.status().is_success() || response.status() == reqwest::StatusCode::ACCEPTED {
            Ok(())
        } else {
            Err(format!("MCP notification HTTP {}", response.status()))
        }
    }

    async fn capture_session(&self, response: &reqwest::Response) {
        if let Some(value) = response
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|h| h.to_str().ok())
        {
            *self.session_id.lock().await = Some(value.to_string());
        }
    }

    pub async fn list_tools(&self) -> Result<Vec<McpToolDescriptor>, String> {
        let result = self.rpc("tools/list", serde_json::json!({}), true).await?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .ok_or("MCP tools/list result carries no `tools` array")?;
        Ok(tools
            .iter()
            .filter_map(|t| {
                let name = t.get("name")?.as_str()?.to_string();
                let ann = t.get("annotations");
                Some(McpToolDescriptor {
                    name,
                    description: t
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .to_string(),
                    annotations: McpToolAnnotations {
                        read_only_hint: ann
                            .and_then(|a| a.get("readOnlyHint"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                        destructive_hint: ann
                            .and_then(|a| a.get("destructiveHint"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                    },
                    input_schema: t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or(serde_json::json!({"type":"object"})),
                })
            })
            .collect())
    }
}

impl McpTransport for HttpMcpTransport {
    fn call_tool<'a>(
        &'a self,
        tool_name: &'a str,
        args: serde_json::Value,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            let result = self
                .rpc(
                    "tools/call",
                    serde_json::json!({"name": tool_name, "arguments": args}),
                    true,
                )
                .await?;
            if result
                .get("isError")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                return Err(format!(
                    "MCP tool `{tool_name}` reported an error: {result}"
                ));
            }
            Ok(result)
        })
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// L-04: Is this URL's host loopback? IPv4 and IPv6 literals are classified by
/// the standard library (so every spelling of `::1` works), an IPv4-mapped or
/// IPv4-compatible IPv6 literal is UNWRAPPED first (so `::ffff:127.0.0.1` counts
/// as loopback), and `localhost` is matched case-insensitively with an optional
/// trailing root-label dot.
///
/// This is the single definition of "the local-development endpoint": it decides
/// both whether plain HTTP is allowed ([`validate_endpoint`]) and whether a
/// loopback destination may be connected to ([`addr_permitted`]).
fn is_loopback_host(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(addr)) => addr.is_loopback(),
        Some(url::Host::Ipv6(addr)) => ip_is_loopback(IpAddr::V6(addr)),
        Some(url::Host::Domain(d)) => d.trim_end_matches('.').eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// Loopback, unwrapping an IPv4-mapped (`::ffff:a.b.c.d`) or IPv4-compatible
/// (`::a.b.c.d`) IPv6 address first.
fn ip_is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback() || mapped_ipv4(&v6).is_some_and(|v4| v4.is_loopback()),
    }
}

/// The IPv4 address an IPv6 address embeds in the two "v4 in the low 32 bits"
/// forms: IPv4-mapped `::ffff:a.b.c.d` and deprecated IPv4-compatible
/// `::a.b.c.d`. `None` for a native v6 address.
///
/// Only used for the *loopback* question. The block-list question goes through
/// [`is_blocked_ip`], which additionally decodes NAT64 and 6to4 embeddings —
/// deliberately NOT duplicated here.
fn mapped_ipv4(v6: &Ipv6Addr) -> Option<Ipv4Addr> {
    let seg = v6.segments();
    let first_five_zero = seg[0] == 0 && seg[1] == 0 && seg[2] == 0 && seg[3] == 0 && seg[4] == 0;
    if first_five_zero && (seg[5] == 0 || seg[5] == 0xffff) {
        let hi = seg[6].to_be_bytes();
        let lo = seg[7].to_be_bytes();
        return Some(Ipv4Addr::new(hi[0], hi[1], lo[0], lo[1]));
    }
    None
}

/// H-02: May this transport connect to `ip`?
///
/// The base rule is `fetch_url`'s full non-public block-list ([`is_blocked_ip`]):
/// loopback, RFC-1918, link-local including the `169.254.169.254` cloud-metadata
/// endpoint, CGNAT, unspecified, broadcast, documentation, multicast/reserved,
/// IPv6 ULA + link-local, and every IPv4-in-IPv6 embedding of those — so an
/// internal IPv4 cannot be smuggled in through a v6 literal or an AAAA record.
///
/// `allow_loopback` carves out ONLY loopback, and only for a registered
/// local-development endpoint. Nothing else is ever reachable.
fn addr_permitted(ip: IpAddr, allow_loopback: bool) -> bool {
    if allow_loopback && ip_is_loopback(ip) {
        return true;
    }
    !is_blocked_ip(ip)
}

/// One hop's vetted destination.
#[derive(Debug, Clone, PartialEq, Eq)]
struct VettedHop {
    /// `url.host_str()` — the key a pinned client's resolver override replaces.
    host_key: String,
    /// Every address the host resolved to, all of which passed
    /// [`addr_permitted`], sorted so the pin is order-stable across the
    /// round-robin a resolver may hand back.
    addrs: Vec<SocketAddr>,
}

/// Resolve a URL's host to concrete socket addresses. An IP literal (including a
/// bracketed IPv6 one, which `lookup_host` rejects) short-circuits DNS entirely.
async fn resolve_addrs(url: &url::Url) -> Result<Vec<SocketAddr>, String> {
    let host = url
        .host_str()
        .ok_or_else(|| "MCP endpoint URL has no host".to_string())?;
    let port = url.port_or_known_default().unwrap_or(443);
    let literal = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = literal.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("MCP endpoint DNS resolution failed for `{host}`: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!(
            "MCP endpoint host `{host}` resolved to no addresses"
        ));
    }
    Ok(addrs)
}

/// H-02: Vet one hop and RETURN the addresses that passed, so the caller pins
/// the connection to them. Resolving once and handing the result to
/// `resolve_to_addrs` is what closes the DNS-rebind TOCTOU: without the pin,
/// reqwest performs its own lookup at connect time and a second answer could
/// point the socket at an address that never faced this check.
///
/// A host that resolves to ANY refused address is refused whole — one poisoned
/// A record must not be survivable by retrying.
async fn vet_hop(url: &url::Url, allow_loopback: bool) -> Result<VettedHop, String> {
    let host = url
        .host_str()
        .ok_or_else(|| "MCP endpoint URL has no host".to_string())?
        .to_string();
    let mut addrs = resolve_addrs(url).await?;
    for addr in &addrs {
        if !addr_permitted(addr.ip(), allow_loopback) {
            return Err(format!(
                "MCP endpoint `{host}` resolves to a non-public address ({}) — refused",
                addr.ip()
            ));
        }
    }
    addrs.sort();
    addrs.dedup();
    Ok(VettedHop {
        host_key: host,
        addrs,
    })
}

/// Build a client whose resolution of `host` is PINNED to `addrs`. Redirects
/// stay disabled — `post` follows them itself so every hop is re-vetted.
///
/// `proxy` is `None` on every production call site. It exists so the regression
/// test can hand a proxy to the REAL builder and prove `.no_proxy()` below wins;
/// without that seam the test would have to build its own client and the
/// mutation that deletes `.no_proxy()` from production would survive.
fn build_pinned_client(
    host: &str,
    addrs: &[SocketAddr],
    proxy: Option<reqwest::Proxy>,
) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .timeout(RPC_TIMEOUT)
        .connect_timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(host, addrs);
    if let Some(proxy) = proxy {
        builder = builder.proxy(proxy);
    }
    builder
        // Without this the pin above is DECORATIVE. reqwest reads
        // HTTP_PROXY/HTTPS_PROXY/ALL_PROXY at `build()` time; when any is set
        // the socket goes to the proxy, which resolves the target hostname
        // ITSELF at connect time. The vetted address then receives nothing and
        // the DNS-rebind TOCTOU this transport exists to close is wide open.
        //
        // So MCP HTTP traffic deliberately does NOT honour a configured proxy:
        // a guard whose whole purpose is choosing the destination address
        // cannot permit an intermediary that re-resolves the name.
        //
        // Order matters — reqwest's `no_proxy()` CLEARS the proxy list, so it
        // must come after any `.proxy(..)` above. Proven by
        // `a_configured_proxy_cannot_defeat_the_pinned_address`.
        .no_proxy()
        .build()
        .map_err(|e| format!("couldn't build MCP HTTP client: {e}"))
}

/// H-02: The URL-level rules for moving from one hop to the next: the target
/// must pass redirect-target validation (everything a configured endpoint must
/// pass except the no-query rule — see [`validate_redirect_target`]), and an
/// HTTPS hop may never downgrade to plain HTTP (which would put the payload —
/// and any session token — in clear).
fn check_hop_transition(from: &url::Url, to: &url::Url) -> Result<(), String> {
    validate_redirect_target(to.as_str())
        .map_err(|e| format!("MCP redirect target rejected by endpoint validation: {e}"))?;
    if from.scheme() == "https" && to.scheme() != "https" {
        return Err("MCP redirect scheme downgrade from HTTPS is rejected".to_string());
    }
    Ok(())
}

fn parse_jsonrpc(raw: &[u8], id: i64) -> Result<serde_json::Value, String> {
    let message: serde_json::Value =
        serde_json::from_slice(raw).map_err(|e| format!("MCP HTTP JSON decode failed: {e}"))?;
    parse_response(message, id)
}

fn parse_sse_jsonrpc(raw: &[u8], id: i64) -> Result<serde_json::Value, String> {
    let text = std::str::from_utf8(raw).map_err(|_| "MCP SSE response is not UTF-8".to_string())?;
    let mut data = Vec::new();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start());
        }
        if line.trim().is_empty() && !data.is_empty() {
            let candidate = data.join("\n");
            data.clear();
            if let Ok(message) = serde_json::from_str::<serde_json::Value>(&candidate) {
                if message.get("id").and_then(|v| v.as_i64()) == Some(id) {
                    return parse_response(message, id);
                }
            }
        }
    }
    if !data.is_empty() {
        let candidate = data.join("\n");
        if let Ok(message) = serde_json::from_str::<serde_json::Value>(&candidate) {
            return parse_response(message, id);
        }
    }
    Err("MCP SSE response did not include the matching JSON-RPC result".to_string())
}

fn parse_response(message: serde_json::Value, id: i64) -> Result<serde_json::Value, String> {
    if message.get("id").and_then(|v| v.as_i64()) != Some(id) {
        return Err("MCP response id did not match the request".to_string());
    }
    if let Some(error) = message.get("error") {
        return Err(format!("MCP server error: {error}"));
    }
    Ok(message
        .get("result")
        .cloned()
        .unwrap_or(serde_json::Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_require_https_except_loopback() {
        assert!(validate_endpoint("https://example.com/mcp").is_ok());
        assert!(validate_endpoint("http://127.0.0.1:3000/mcp").is_ok());
        // L-04: IPv6 loopback in canonical and expanded forms.
        assert!(validate_endpoint("http://[::1]:3000/mcp").is_ok());
        assert!(validate_endpoint("http://[0:0:0:0:0:0:0:1]:3000/mcp").is_ok());
        // L-04: localhost variants — case-insensitive, trailing dot.
        assert!(validate_endpoint("http://localhost:3000/mcp").is_ok());
        assert!(validate_endpoint("http://LOCALHOST:3000/mcp").is_ok());
        assert!(validate_endpoint("http://localhosT.:3000/mcp").is_ok());
        // Non-loopback HTTP is rejected.
        assert!(validate_endpoint("http://example.com/mcp").is_err());
        // Credentials and fragments are rejected.
        assert!(validate_endpoint("https://user:secret@example.com/mcp").is_err());
    }

    #[test]
    fn json_and_sse_responses_require_matching_ids() {
        let json = br#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#;
        assert_eq!(parse_jsonrpc(json, 7).unwrap()["ok"], true);
        let sse =
            b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n\n";
        assert_eq!(parse_sse_jsonrpc(sse, 7).unwrap()["ok"], true);
        assert!(parse_jsonrpc(json, 8).is_err());
    }

    // ── live-server harness ───────────────────────────────────────────────────
    //
    // A minimal HTTP/1.1 server on 127.0.0.1: per connection it reads one
    // request (recording its head), writes the next scripted response, closes.
    // A loopback endpoint is a *valid* MCP endpoint, so these drive the real
    // `post()` loop end to end rather than a re-implementation of it.

    struct TestServer {
        addr: SocketAddr,
        heads: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl TestServer {
        fn heads(&self) -> Vec<String> {
            self.heads.lock().unwrap().clone()
        }
        fn endpoint(&self) -> url::Url {
            url::Url::parse(&format!("http://{}/mcp", self.addr)).unwrap()
        }
    }

    /// Bind first, then let `script` build the responses now that the port is
    /// known — so a scripted redirect can point back at this same server.
    async fn spawn_server<F>(script: F) -> TestServer
    where
        F: FnOnce(SocketAddr) -> Vec<Vec<u8>>,
    {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let responses = script(addr);
        let heads = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = heads.clone();
        tokio::spawn(async move {
            let mut served = 0usize;
            while let Ok((mut stream, _)) = listener.accept().await {
                if let Some(head) = read_request(&mut stream).await {
                    recorded.lock().unwrap().push(head);
                }
                let reply = responses
                    .get(served)
                    .or_else(|| responses.last())
                    .cloned()
                    .unwrap_or_default();
                served += 1;
                // Errors are expected when the client walks away mid-body (the
                // oversized-response test does exactly that).
                let _ = stream.write_all(&reply).await;
                let _ = stream.flush().await;
                let _ = stream.shutdown().await;
            }
        });
        TestServer { addr, heads }
    }

    /// Read one request — head plus its declared `Content-Length` body — and
    /// return the head.
    async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<String> {
        use tokio::io::AsyncReadExt;

        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = stream.read(&mut tmp).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&tmp[..n]);
            let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&buf[..end]).to_string();
            let mut want = 0usize;
            for line in head.lines() {
                if let Some((k, v)) = line.split_once(':') {
                    if k.eq_ignore_ascii_case("content-length") {
                        want = v.trim().parse().unwrap_or(0);
                    }
                }
            }
            if buf.len() >= end + 4 + want {
                return Some(head);
            }
        }
    }

    fn json_response(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn redirect_response(location: &str) -> Vec<u8> {
        format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .into_bytes()
    }

    // ── H-02 tests ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn session_header_is_sent_on_hop_0_and_stripped_after_a_redirect() {
        let server = spawn_server(|addr| {
            vec![
                redirect_response(&format!("http://{addr}/moved")),
                json_response(r#"{"jsonrpc":"2.0","id":0,"result":{"ok":true}}"#),
            ]
        })
        .await;

        let transport = HttpMcpTransport::from_validated(server.endpoint());
        *transport.session_id.lock().await = Some("SESSION-TOKEN-abc".to_string());

        let result = transport
            .rpc("tools/list", serde_json::json!({}), true)
            .await;
        assert_eq!(
            result.expect("the redirect must still be followed")["ok"],
            true
        );

        let heads = server.heads();
        assert_eq!(heads.len(), 2, "one request per hop: {heads:?}");
        assert!(
            heads[0]
                .to_ascii_lowercase()
                .contains("mcp-session-id: session-token-abc"),
            "hop 0 must carry the session token: {}",
            heads[0]
        );
        assert!(
            !heads[1].to_ascii_lowercase().contains("session-token-abc"),
            "the session token must NOT survive a redirect boundary: {}",
            heads[1]
        );
    }

    #[test]
    fn https_to_http_downgrade_is_refused_as_its_own_rule() {
        let from = url::Url::parse("https://example.com/mcp").unwrap();
        // A loopback HTTP target passes `validate_endpoint` on its own merits,
        // so reaching a refusal here requires the downgrade rule to exist.
        let to = url::Url::parse("http://127.0.0.1:3000/mcp").unwrap();
        assert!(
            validate_endpoint(to.as_str()).is_ok(),
            "the target is independently valid, so only the downgrade rule can refuse it"
        );
        let err = check_hop_transition(&from, &to).unwrap_err();
        assert!(err.contains("downgrade"), "got {err}");

        // Same-scheme hops are fine in both directions of the matrix.
        assert!(check_hop_transition(
            &from,
            &url::Url::parse("https://other.example/mcp").unwrap()
        )
        .is_ok());
        assert!(
            check_hop_transition(&to, &url::Url::parse("http://127.0.0.1:3001/mcp").unwrap())
                .is_ok()
        );
    }

    #[tokio::test]
    async fn redirect_hop_limit_is_enforced() {
        // A server that redirects forever — the loop must stop itself.
        let server =
            spawn_server(|addr| vec![redirect_response(&format!("http://{addr}/again"))]).await;
        let transport = HttpMcpTransport::from_validated(server.endpoint());

        let err = transport
            .rpc("tools/list", serde_json::json!({}), true)
            .await
            .unwrap_err();
        assert!(err.contains("redirect limit"), "got {err}");
        assert_eq!(
            server.heads().len(),
            MAX_REDIRECT_HOPS + 1,
            "exactly the initial request plus MAX_REDIRECT_HOPS redirects"
        );
    }

    #[tokio::test]
    async fn hop_zero_is_vetted_not_only_redirects() {
        // The REGISTERED endpoint must face the address check too. Each of these
        // passes `validate_endpoint` (they are HTTPS), so before this fix the
        // very first hop went straight to a private address.
        for private in [
            "https://10.0.0.1/mcp",
            "https://192.168.1.50/mcp",
            "https://169.254.169.254/mcp", // cloud metadata
            "https://[fd00::1]/mcp",       // IPv6 ULA
        ] {
            let url = url::Url::parse(private).unwrap();
            assert!(
                validate_endpoint(private).is_ok(),
                "{private} must pass URL validation, so only the address check can stop it"
            );
            let transport = HttpMcpTransport::from_validated(url);
            let err = transport
                .rpc("tools/list", serde_json::json!({}), true)
                .await
                .unwrap_err();
            assert!(
                err.contains("non-public address"),
                "{private} must be refused before any socket is opened: got {err}"
            );
        }
    }

    #[tokio::test]
    async fn ipv4_mapped_ipv6_is_unwrapped_in_both_gates() {
        // validate_endpoint: a mapped LOOPBACK literal is the local-dev case, so
        // plain HTTP is allowed for it…
        assert!(validate_endpoint("http://[::ffff:127.0.0.1]:3000/mcp").is_ok());
        // …while a mapped RFC-1918 literal is not loopback, so HTTP is refused.
        assert!(validate_endpoint("http://[::ffff:10.0.0.1]:3000/mcp").is_err());

        // addr_permitted: mapped-private is refused under BOTH policies; only
        // mapped-loopback is carved out, and only for a loopback endpoint.
        let mapped_private: IpAddr = "::ffff:10.0.0.1".parse().unwrap();
        let mapped_meta: IpAddr = "::ffff:169.254.169.254".parse().unwrap();
        let mapped_loop: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        for allow in [false, true] {
            assert!(
                !addr_permitted(mapped_private, allow),
                "::ffff:10.0.0.1 must never be permitted (allow_loopback={allow})"
            );
            assert!(
                !addr_permitted(mapped_meta, allow),
                "mapped metadata address must never be permitted (allow_loopback={allow})"
            );
        }
        assert!(!addr_permitted(mapped_loop, false));
        assert!(addr_permitted(mapped_loop, true));

        // vet_hop — the predicate the live path actually uses — classifies both.
        // Both are IP literals, so `resolve_addrs` short-circuits DNS and this
        // touches no network.
        assert!(
            vet_hop(
                &url::Url::parse("https://[::ffff:10.0.0.1]/mcp").unwrap(),
                false
            )
            .await
            .is_err(),
            "::ffff:10.0.0.1 must be classified private"
        );
        assert!(
            vet_hop(
                &url::Url::parse("https://[2606:2800:220:1::1]/mcp").unwrap(),
                false
            )
            .await
            .is_ok(),
            "a public v6 address must not be classified private"
        );

        // …and the live request path refuses it before opening a socket.
        let transport = HttpMcpTransport::from_validated(
            url::Url::parse("https://[::ffff:10.0.0.1]/mcp").unwrap(),
        );
        let err = transport
            .rpc("tools/list", serde_json::json!({}), true)
            .await
            .unwrap_err();
        assert!(err.contains("non-public address"), "got {err}");
    }

    // ── M-01 test ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn oversized_response_body_is_refused_while_streaming() {
        // Deliberately VALID JSON-RPC, just too big: without the streaming cap
        // this response would parse and succeed, so the assertion below can only
        // pass because the cap fires.
        let pad = "x".repeat(MAX_RESPONSE_BYTES + 1024);
        let body = format!(r#"{{"jsonrpc":"2.0","id":0,"result":{{"pad":"{pad}"}}}}"#);
        assert!(
            serde_json::from_str::<serde_json::Value>(&body).is_ok(),
            "the oversized body must be well-formed, so only the cap can reject it"
        );
        let server = spawn_server(move |_| vec![json_response(&body)]).await;

        let transport = HttpMcpTransport::from_validated(server.endpoint());
        let err = transport
            .rpc("tools/list", serde_json::json!({}), true)
            .await
            .unwrap_err();
        assert!(err.contains("exceeds 4 MB limit"), "got {err}");
    }

    // ── DNS-rebind TOCTOU test ────────────────────────────────────────────────

    #[tokio::test]
    async fn pinned_client_cannot_be_moved_by_a_second_dns_answer() {
        // Two servers. `vet_hop` vets an address and `build_pinned_client` pins
        // the hostname to exactly that address. The hostname lives in the
        // reserved `.invalid` TLD and therefore has NO DNS answer at all — so
        // any request that arrives anywhere proves the socket followed the pin
        // and never consulted a resolver. Which server receives the bytes is a
        // property of the vetted pin alone; a second, different answer for the
        // same name cannot move it.
        let a = spawn_server(|_| vec![json_response(r#"{"who":"A"}"#)]).await;
        let b = spawn_server(|_| vec![json_response(r#"{"who":"B"}"#)]).await;
        const HOST: &str = "mcp-rebind-test.invalid";

        // Baseline: unpinned, the name reaches nothing.
        let unpinned = reqwest::Client::builder().build().unwrap();
        assert!(
            unpinned
                .get(format!("http://{HOST}/mcp"))
                .send()
                .await
                .is_err(),
            "the test hostname must have no reachable DNS answer of its own"
        );

        // Vet A, then pin the NAME to precisely what was vetted.
        let vetted = vet_hop(
            &url::Url::parse(&format!("http://{}/mcp", a.addr)).unwrap(),
            true,
        )
        .await
        .expect("a loopback literal is vettable for a loopback endpoint");
        assert_eq!(
            vetted.addrs,
            vec![a.addr],
            "vet_hop must hand back the exact address it vetted"
        );

        let pinned_a = build_pinned_client(HOST, &vetted.addrs, None).unwrap();
        let body_a = pinned_a
            .get(format!("http://{HOST}/mcp"))
            .send()
            .await
            .expect("pinned request must reach the vetted address")
            .text()
            .await
            .unwrap();
        assert!(body_a.contains(r#""A""#), "landed somewhere else: {body_a}");
        assert_eq!(a.heads().len(), 1, "A served the pinned request");
        assert_eq!(
            b.heads().len(),
            0,
            "an address that was never vetted must never be contacted"
        );

        // Only re-vetting and re-pinning changes the destination — proving the
        // destination is the pin, not the name.
        let pinned_b = build_pinned_client(HOST, &[b.addr], None).unwrap();
        let body_b = pinned_b
            .get(format!("http://{HOST}/mcp"))
            .send()
            .await
            .expect("second pinned client must reach its own vetted address")
            .text()
            .await
            .unwrap();
        assert!(body_b.contains(r#""B""#), "landed somewhere else: {body_b}");
        assert_eq!(a.heads().len(), 1, "A must not have been contacted again");
    }

    #[tokio::test]
    async fn a_configured_proxy_cannot_defeat_the_pinned_address() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        use std::sync::Arc;
        use tokio::io::AsyncWriteExt;

        // Two listeners: the PINNED (vetted) target and a PROXY sink.
        let pinned = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pinned_addr = pinned.local_addr().unwrap();
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();

        let pinned_hits = Arc::new(AtomicUsize::new(0));
        let proxy_hits = Arc::new(AtomicUsize::new(0));

        let ph = pinned_hits.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = pinned.accept().await {
                ph.fetch_add(1, AtomicOrdering::SeqCst);
                let _ = s
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nPINNED",
                    )
                    .await;
            }
        });
        let xh = proxy_hits.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = proxy.accept().await {
                xh.fetch_add(1, AtomicOrdering::SeqCst);
                let _ = s
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nPROXY",
                    )
                    .await;
            }
        });

        // The PRODUCTION builder, handed an explicit proxy. An explicit
        // `.proxy(..)` exercises exactly the path `HTTP_PROXY`/`ALL_PROXY` take
        // (reqwest reads those at `build()` time) without mutating
        // process-global env from a test that runs in parallel with others.
        // `build_pinned_client`'s `.no_proxy()` must win, or the pin — and the
        // whole DNS-rebind guard — is decorative: the socket would go to the
        // proxy, which resolves the hostname itself at connect time.
        const HOST: &str = "mcp-proxy-probe.invalid";
        let client = build_pinned_client(
            HOST,
            &[pinned_addr],
            Some(reqwest::Proxy::all(format!("http://{proxy_addr}")).unwrap()),
        )
        .unwrap();

        let body = client
            .get(format!("http://{HOST}/mcp"))
            .send()
            .await
            .expect("the pinned request must reach the vetted address")
            .text()
            .await
            .unwrap();

        assert_eq!(
            body, "PINNED",
            "the vetted address must receive the request, not the proxy"
        );
        assert_eq!(
            proxy_hits.load(AtomicOrdering::SeqCst),
            0,
            "the proxy must receive no connection at all"
        );
        assert_eq!(pinned_hits.load(AtomicOrdering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_redirect_target_may_carry_a_query_string() {
        // Real MCP deployments 307 onto a session-bearing URL. Refusing that
        // failed EVERY rpc with no recovery route, so a SERVER-issued target is
        // allowed a query even though a user-CONFIGURED endpoint is not.
        let server = spawn_server(|addr| {
            vec![
                redirect_response(&format!("http://{addr}/mcp?session=abc&x=1")),
                json_response(r#"{"jsonrpc":"2.0","id":0,"result":{"ok":true}}"#),
            ]
        })
        .await;
        // The same URL as a configured endpoint is still refused — so this test
        // can only pass because the redirect case is treated differently.
        assert!(
            validate_endpoint(&format!("http://{}/mcp?session=abc&x=1", server.addr)).is_err(),
            "a configured endpoint must still refuse a query string"
        );

        let transport = HttpMcpTransport::from_validated(server.endpoint());
        let result = transport
            .rpc("tools/list", serde_json::json!({}), true)
            .await;
        assert_eq!(
            result.expect("a 307 to a session-bearing URL must be followed")["ok"],
            true
        );

        let heads = server.heads();
        assert_eq!(heads.len(), 2, "one request per hop: {heads:?}");
        assert!(
            heads[1].starts_with("POST /mcp?session=abc&x=1 "),
            "hop 1 must request the redirected path INCLUDING its query: {}",
            heads[1]
        );
    }

    #[tokio::test]
    async fn the_production_redirect_path_enforces_the_hop_gate() {
        // Guards the CALL SITE of `check_hop_transition` inside `post()`, not
        // just the helper: turning that `?` into an ignored Result must break
        // this test. Server A redirects to a CREDENTIALED URL on server B —
        // which `check_hop_transition` refuses, but which is otherwise
        // perfectly reachable (loopback, plain HTTP, permitted address, and B
        // answers a well-formed JSON-RPC result for this exact request id). So
        // if the gate's result is ignored, the rpc SUCCEEDS and B is contacted.
        let b = spawn_server(|_| {
            vec![json_response(
                r#"{"jsonrpc":"2.0","id":0,"result":{"ok":true}}"#,
            )]
        })
        .await;
        let b_addr = b.addr;
        let a = spawn_server(move |_| {
            vec![redirect_response(&format!(
                "http://user:secret@{b_addr}/mcp"
            ))]
        })
        .await;

        let transport = HttpMcpTransport::from_validated(a.endpoint());
        let err = transport
            .rpc("tools/list", serde_json::json!({}), true)
            .await
            .unwrap_err();

        assert!(
            err.contains("credentials or fragments"),
            "the hop gate must refuse a credentialed redirect target: got {err}"
        );
        assert_eq!(
            b.heads().len(),
            0,
            "the refused target must never be contacted"
        );
        assert_eq!(a.heads().len(), 1, "only hop 0 should have been sent");
    }

    #[test]
    fn configured_endpoints_refuse_a_query_but_redirect_targets_do_not() {
        // The asymmetry, stated directly.
        assert!(validate_endpoint("https://example.com/mcp?token=secret").is_err());
        assert!(validate_redirect_target("https://example.com/mcp?session=abc").is_ok());
        assert!(
            validate_endpoint("https://example.com/mcp").is_ok(),
            "a query-free endpoint is unaffected"
        );

        // Everything else a redirect target must still satisfy.
        assert!(
            validate_redirect_target("https://user:pw@example.com/mcp").is_err(),
            "credentials stay refused on a redirect target"
        );
        assert!(
            validate_redirect_target("https://example.com/mcp#frag").is_err(),
            "fragments stay refused on a redirect target"
        );
        assert!(
            validate_redirect_target("http://example.com/mcp").is_err(),
            "non-loopback plain HTTP stays refused on a redirect target"
        );
        assert!(
            validate_redirect_target("http://127.0.0.1:3000/mcp?session=abc").is_ok(),
            "a loopback HTTP redirect target with a query is the local-dev case"
        );
    }
}
