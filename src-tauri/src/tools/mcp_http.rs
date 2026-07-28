//! MCP Streamable HTTP transport for remote servers. The MCP specification
//! replaced the legacy HTTP+SSE transport with this single POST/GET endpoint;
//! responses may still be JSON or SSE, so both are accepted here.

use std::net::IpAddr;
use std::sync::atomic::{AtomicI64, Ordering};

use super::mcp::{McpToolAnnotations, McpToolDescriptor, McpTransport};

const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
const RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_REDIRECT_HOPS: usize = 5;

/// A validated Streamable HTTP endpoint. HTTPS is mandatory except for a
/// loopback HTTP endpoint used for local development; credentials/fragments in
/// URLs are rejected so they cannot leak into logs or persisted settings.
pub fn validate_endpoint(raw: &str) -> Result<url::Url, String> {
    let url =
        url::Url::parse(raw.trim()).map_err(|_| "MCP endpoint must be a valid URL".to_string())?;
    if url.username() != "" || url.password().is_some() || url.fragment().is_some() {
        return Err("MCP endpoint URLs may not contain credentials or fragments".to_string());
    }
    // L-04: Parse the host as an IP address using url::Host so that all IPv6
    // representations (::1, 0:0:0:0:0:0:0:1, etc.) are handled by the standard
    // library's is_loopback() instead of naive string matching.
    let loopback = match url.host() {
        Some(url::Host::Ipv4(addr)) => addr.is_loopback(),
        Some(url::Host::Ipv6(addr)) => addr.is_loopback(),
        Some(url::Host::Domain(d)) => {
            d.eq_ignore_ascii_case("localhost") || d.eq_ignore_ascii_case("localhost.")
        }
        None => false,
    };
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(
            "MCP endpoints must use HTTPS (HTTP is allowed only for localhost)".to_string(),
        );
    }
    if url.query().is_some() {
        return Err("MCP endpoint URLs may not contain a query string".to_string());
    }
    Ok(url)
}

pub struct HttpMcpTransport {
    endpoint: url::Url,
    client: reqwest::Client,
    session_id: tokio::sync::Mutex<Option<String>>,
    next_id: AtomicI64,
}

impl HttpMcpTransport {
    /// Build and initialize a remote MCP connection. A failed initialize never
    /// enters the runtime or the persisted registration store.
    pub async fn connect(raw_endpoint: &str) -> Result<Self, String> {
        let endpoint = validate_endpoint(raw_endpoint)?;
        let client = reqwest::Client::builder()
            .timeout(RPC_TIMEOUT)
            .connect_timeout(std::time::Duration::from_secs(10))
            // H-02: Disable automatic redirects — we follow manually with
            // per-hop security checks so cross-origin session headers never
            // leak and private destinations are rejected.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("couldn't build MCP HTTP client: {e}"))?;
        let transport = Self {
            endpoint,
            client,
            session_id: tokio::sync::Mutex::new(None),
            next_id: AtomicI64::new(0),
        };
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

    /// Send a POST, manually following redirects with per-hop security checks.
    ///
    /// H-02: Redirects are not automatic. Every hop re-validates the endpoint,
    /// rejects scheme downgrades, blocks private/loopback destinations, and
    /// never forwards the session token across origins.
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
            let mut request = self
                .client
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

                // H-02: Re-run full endpoint validation every hop.
                validate_endpoint(new_url.as_str()).map_err(|e| {
                    format!("MCP redirect target rejected by endpoint validation: {e}")
                })?;

                // H-02: Reject scheme downgrade (HTTPS → HTTP).
                if url.scheme() == "https" && new_url.scheme() != "https" {
                    return Err("MCP redirect scheme downgrade from HTTPS is rejected".to_string());
                }

                // H-02: Reject private / loopback / unspecified destinations.
                if is_private_destination(&new_url).await? {
                    return Err(
                        "MCP redirect to a private or loopback address is rejected".to_string()
                    );
                }

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
        self.capture_session(&mut response).await;
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

/// H-02: Check whether a resolved URL points to a private, loopback, or
/// unspecified IP address. For domain names, DNS-resolve and inspect every
/// returned address.
async fn is_private_destination(url: &url::Url) -> Result<bool, String> {
    match url.host() {
        Some(url::Host::Ipv4(addr)) => {
            Ok(addr.is_private() || addr.is_loopback() || addr.is_unspecified())
        }
        Some(url::Host::Ipv6(addr)) => {
            Ok(addr.is_loopback() || addr.is_unspecified() || addr.is_unique_local())
        }
        Some(url::Host::Domain(host)) => {
            let port = url.port_or_known_default().unwrap_or(443);
            let mut addrs = tokio::net::lookup_host((host, port))
                .await
                .map_err(|e| format!("MCP redirect DNS resolution failed: {e}"))?;
            let mut has_private = false;
            for addr in addrs.by_ref() {
                let is_private = match addr.ip() {
                    IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_unspecified(),
                    IpAddr::V6(v6) => {
                        v6.is_loopback() || v6.is_unspecified() || v6.is_unique_local()
                    }
                };
                if is_private {
                    has_private = true;
                    break;
                }
            }
            Ok(has_private)
        }
        None => Ok(true), // No host → fail closed.
    }
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
}
