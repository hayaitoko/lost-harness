//! Shared authenticated Google REST client for the Calendar and Tasks
//! integrations. It deliberately reuses the keychain-backed token provider
//! from Gmail: one Google account connection per profile, short-lived access
//! tokens only in memory, and one bounded retry after a 401.

use std::time::Duration;

use super::gmail::{read_body_capped, TokenProvider, MAX_RESPONSE_BYTES};

#[derive(Debug, Clone, Copy)]
pub enum Method {
    Get,
    Post,
    Patch,
    Delete,
}

/// A minimal authenticated JSON client. Google API error bodies are surfaced
/// as bounded text rather than being turned into an empty success result.
pub struct GoogleClient {
    client: reqwest::Client,
    tokens: Box<dyn TokenProvider>,
    /// Response-body ceiling in bytes (see [`MAX_RESPONSE_BYTES`]). A field
    /// rather than a bare const so tests can drive the refusal path with a
    /// small body instead of allocating tens of megabytes.
    max_response_bytes: usize,
}

impl GoogleClient {
    pub fn new(tokens: Box<dyn TokenProvider>) -> anyhow::Result<Self> {
        Self::with_response_cap(tokens, MAX_RESPONSE_BYTES)
    }

    fn with_response_cap(
        tokens: Box<dyn TokenProvider>,
        max_response_bytes: usize,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10))
                .build()?,
            tokens,
            max_response_bytes,
        })
    }

    async fn request_once(
        &self,
        method: Method,
        url: &str,
        bearer: &str,
        body: Option<&serde_json::Value>,
    ) -> anyhow::Result<(u16, String)> {
        let mut request = match method {
            Method::Get => self.client.get(url),
            Method::Post => self.client.post(url),
            Method::Patch => self.client.patch(url),
            Method::Delete => self.client.delete(url),
        }
        .bearer_auth(bearer);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Google API request failed: {e}"))?;
        let status = response.status().as_u16();
        // Bounded, not `text().await.unwrap_or_default()`: an unbounded buffer
        // let a hostile or runaway response exhaust memory, and swallowing a
        // read failure turned it into an empty body (which `json` below reads
        // as a legitimate `null` on 2xx).
        let text = read_body_capped(response, self.max_response_bytes, "Google API").await?;
        Ok((status, text))
    }

    /// Perform one authorized request, refresh and retry exactly once on 401,
    /// then parse JSON. `204 No Content` maps to JSON null for DELETE calls.
    pub async fn json(
        &self,
        method: Method,
        url: &str,
        body: Option<&serde_json::Value>,
    ) -> anyhow::Result<serde_json::Value> {
        let token = self.tokens.access_token(false).await?;
        let (mut status, mut text) = self.request_once(method, url, &token, body).await?;
        if status == 401 {
            let refreshed = self.tokens.access_token(true).await?;
            (status, text) = self.request_once(method, url, &refreshed, body).await?;
        }
        if !(200..300).contains(&status) {
            let compact = text.chars().take(700).collect::<String>();
            // The structural gap this closes: EVERY non-2xx used to bail with
            // a marker-free plain string, so a real Google 403 could never
            // trip the connection-state markers no matter how many callers
            // checked for them. Classification happens HERE, once, for both
            // recoverable 403s; an unmatched status still produces exactly the
            // string it did before.
            return Err(crate::email::api_error::google_api_error(
                "Google API",
                status,
                &text,
                &compact,
            ));
        }
        if text.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("Google API returned invalid JSON: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::email::BoxFuture;

    struct FixedToken;
    impl TokenProvider for FixedToken {
        fn access_token(&self, _force_refresh: bool) -> BoxFuture<'_, anyhow::Result<String>> {
            Box::pin(async { Ok("tok".to_string()) })
        }
    }

    /// Serve exactly one raw HTTP response on a loopback port, then close.
    async fn serve_once(response: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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
        with_status(200, "OK", body)
    }

    /// Same, with an explicit status line — the only way to drive the
    /// non-2xx branch through the real transport.
    fn with_status(code: u16, reason: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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

    /// The Calendar/Tasks fetch layer had the same unbounded `resp.text()` the
    /// Gmail one did. It is now capped, on the declared length and on the
    /// running total, and an under-cap body still round-trips.
    #[tokio::test]
    async fn oversized_google_api_bodies_are_refused_at_the_fetch_layer() {
        // 1. Declared Content-Length over the cap → refused up front.
        let client = GoogleClient::with_response_cap(Box::new(FixedToken), 1024).unwrap();
        let url = serve_once(with_content_length(&vec![b'a'; 4096])).await;
        let err = client
            .json(Method::Get, &url, None)
            .await
            .expect_err("a 4 KiB body must not pass a 1 KiB cap");
        assert!(
            err.to_string().contains("Google API response too large")
                && err.to_string().contains("declared 4096"),
            "got: {err}"
        );

        // 2. Chunked (no declared length) over the cap → refused mid-stream.
        let url = serve_once(chunked(4096, 512)).await;
        let err = client
            .json(Method::Get, &url, None)
            .await
            .expect_err("a chunked 4 KiB body must not pass a 1 KiB cap");
        assert!(
            err.to_string().contains("exceeded the 1024-byte cap"),
            "got: {err}"
        );

        // 3. Control: a body under the cap still parses into JSON.
        let url = serve_once(with_content_length(br#"{"id":"evt-1"}"#)).await;
        let value = client.json(Method::Get, &url, None).await.unwrap();
        assert_eq!(value["id"], serde_json::json!("evt-1"));
    }

    const CONSOLE: &str =
        "https://console.developers.google.com/apis/api/tasks.googleapis.com/overview?project=42";

    /// The finding this pins: `json` bailed with a marker-free plain string
    /// for ANY non-2xx, so a real Google 403 could never light a banner — the
    /// Planner just failed forever with raw `Google API HTTP 403` text.
    ///
    /// Driven through the REAL transport (loopback socket → reqwest → `json`),
    /// not the pure classifier, so this covers the wiring and not just the
    /// parse.
    #[tokio::test]
    async fn a_403_reaches_callers_with_the_right_marker_for_its_flavour() {
        let client = GoogleClient::new(Box::new(FixedToken)).unwrap();

        // Scope-short grant → the reconnect marker. Reconnecting re-consents
        // all four scopes, so the reconnect banner is the honest remedy.
        let scope_body = br#"{"error":{"code":403,
            "message":"Request had insufficient authentication scopes.",
            "status":"PERMISSION_DENIED","details":[
            {"@type":"type.googleapis.com/google.rpc.ErrorInfo",
             "reason":"ACCESS_TOKEN_SCOPE_INSUFFICIENT","domain":"googleapis.com"}]}}"#;
        let url = serve_once(with_status(403, "Forbidden", scope_body)).await;
        let err = client
            .json(Method::Get, &url, None)
            .await
            .expect_err("a 403 is still a failure")
            .to_string();
        assert!(
            err.contains(crate::email::token_provider::NEEDS_RECONNECT_MARKER),
            "got: {err}"
        );
        assert!(
            !err.contains(crate::email::token_provider::API_NOT_ENABLED_MARKER),
            "got: {err}"
        );

        // Disabled API → the DISTINCT marker plus Google's console link, and
        // NOT the reconnect marker (that would loop the user forever).
        let disabled_body = format!(
            r#"{{"error":{{"errors":[{{"domain":"usageLimits","reason":"accessNotConfigured",
            "message":"Access Not Configured.","extendedHelp":"{CONSOLE}"}}],"code":403}}}}"#
        );
        let url = serve_once(with_status(403, "Forbidden", disabled_body.as_bytes())).await;
        let err = client
            .json(Method::Get, &url, None)
            .await
            .expect_err("a 403 is still a failure")
            .to_string();
        assert!(
            err.contains(crate::email::token_provider::API_NOT_ENABLED_MARKER),
            "got: {err}"
        );
        assert!(
            !err.contains(crate::email::token_provider::NEEDS_RECONNECT_MARKER),
            "reconnecting can never enable a disabled API: {err}"
        );
        assert_eq!(
            crate::email::token_provider::extract_enable_url(&err).as_deref(),
            Some(CONSOLE)
        );

        // An unmatched 403 keeps the pre-existing plain-string behaviour —
        // no marker, no guess about which recovery applies.
        let url = serve_once(with_status(
            403,
            "Forbidden",
            br#"{"error":{"code":403,"message":"The caller does not have permission"}}"#,
        ))
        .await;
        let err = client
            .json(Method::Get, &url, None)
            .await
            .expect_err("a 403 is still a failure")
            .to_string();
        assert!(err.starts_with("Google API HTTP 403: "), "got: {err}");
        assert!(
            !err.contains("[google:") && !err.contains("[gmail:"),
            "an unmatched 403 must carry no marker: {err}"
        );
    }
}
