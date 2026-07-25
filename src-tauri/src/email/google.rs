//! Shared authenticated Google REST client for the Calendar and Tasks
//! integrations. It deliberately reuses the keychain-backed token provider
//! from Gmail: one Google account connection per profile, short-lived access
//! tokens only in memory, and one bounded retry after a 401.

use std::time::Duration;

use super::gmail::TokenProvider;

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
}

impl GoogleClient {
    pub fn new(tokens: Box<dyn TokenProvider>) -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10))
                .build()?,
            tokens,
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
        let text = response.text().await.unwrap_or_default();
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
            anyhow::bail!("Google API HTTP {status}: {compact}");
        }
        if text.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("Google API returned invalid JSON: {e}"))
    }
}
