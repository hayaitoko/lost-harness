//! OpenAI-compatible HTTP client.
//!
//! One `ModelClient` per `Provider`. The client holds a `reqwest::Client`
//! (connection-pooled, idle-timeout 60s) and knows the provider's `base_url`.
//!
//! Wire format: every method speaks the OpenAI chat-completions surface
//! (`/v1/chat/completions` and `/v1/models`). Providers that follow the same
//! shape — OpenRouter, Together, Groq, LM Studio, Ollama's OpenAI-compat mode,
//! llama.cpp's server — all work without per-provider translation.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::provider::Provider;
use super::sse::SseStream;

/// A single message in a chat history. OpenAI's `role` is a string at the
/// wire level (`"system" | "user" | "assistant" | "tool"`); we keep it as a
/// string here so non-OpenAI extensions (e.g. Anthropic-via-proxy) can
/// round-trip without translation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    /// Convenience constructor for the common case.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }
}

/// The model's own freshly-generated current-turn text, as assembled from
/// its SSE stream. This exists so `tools::calling::parse_tool_calls` can
/// require `&OwnOutput` instead of `&str` in its signature — the "parse
/// only the model's own current-turn output" safety rule (never a tool
/// result, a web page, or history) becomes a type mismatch for any call
/// site that doesn't go through the constructor below, instead of being
/// enforced only by a doc comment.
#[derive(Debug, Clone)]
pub struct OwnOutput(String);

impl OwnOutput {
    /// Mint an `OwnOutput` from text assembled out of a live model stream.
    /// `pub(crate)` — callable from anywhere in this crate, but the name
    /// and doc make any call site that isn't the stream-assembly point in
    /// `agent::loop_mod::AgentLoop::process_message` immediately suspect
    /// in review/grep. The tuple field stays private so `OwnOutput(s)`
    /// struct-literal construction is impossible outside this module.
    pub(crate) fn from_stream_assembly(text: String) -> Self {
        OwnOutput(text)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Body of a `POST /chat/completions` request. `tools` (OpenAI function-call
/// spec, Q1 native transport) is omitted from the JSON entirely when `None`
/// so non-tool-capable servers never see the field.
#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a serde_json::Value>,
}

/// Response body of `GET /models` and `POST /chat/completions` (non-stream).
/// Only the fields we need are decoded; everything else is ignored.
#[derive(Debug, Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: Option<ResponseMessage>,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    content: Option<String>,
}

/// Per-provider HTTP client.
pub struct ModelClient {
    client: reqwest::Client,
    provider: Provider,
}

impl ModelClient {
    /// Build a client. Sets a 60s idle timeout on the connection pool
    /// (matches ChatGPT / Claude desktop behaviour) and a 30s connect timeout
    /// so a dead endpoint fails fast instead of hanging the agent loop.
    pub fn new(provider: Provider) -> Result<Self> {
        let client = reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(30))
            .build()
            .context("failed to build reqwest client")?;
        Ok(Self { client, provider })
    }

    /// The provider this client was built from.
    pub fn provider(&self) -> &Provider {
        &self.provider
    }

    /// `GET {base_url}/models` — return the list of model ids the provider
    /// exposes. Used by the model picker (§4) and by `list_models_for`.
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let url = format!("{}/models", self.provider.base_url.trim_end_matches('/'));
        let mut req = self.client.get(&url);
        if let Some(key) = &self.provider.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("GET {url} failed"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("list_models: HTTP {status} — {body}");
        }
        let body: ModelsResponse = resp
            .json()
            .await
            .context("list_models: failed to decode response JSON")?;
        Ok(body.data.into_iter().map(|m| m.id).collect())
    }

    /// `POST {base_url}/chat/completions` with `stream: true`. Returns the
    /// raw SSE stream for the caller to consume via `SseStream::next_event`.
    pub async fn stream_chat(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
    ) -> Result<SseStream> {
        self.stream_chat_with_tools(model, messages, None).await
    }

    /// `stream_chat` with an optional native `tools` array (Q1). Pass the
    /// dispatcher-rendered OpenAI function-call spec on endpoints whose
    /// `supports_native_tools` flag is set; `None` behaves exactly like
    /// `stream_chat` (the field never appears on the wire).
    pub async fn stream_chat_with_tools(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
        tools: Option<&serde_json::Value>,
    ) -> Result<SseStream> {
        let url = format!(
            "{}/chat/completions",
            self.provider.base_url.trim_end_matches('/')
        );
        let body = ChatRequest {
            model,
            messages: &messages,
            stream: true,
            tools,
        };
        let mut req = self.client.post(&url).json(&body);
        if let Some(key) = &self.provider.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("POST {url} failed"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("stream_chat: HTTP {status} — {body}");
        }
        Ok(SseStream::new(resp))
    }

    /// `POST {base_url}/chat/completions` with `stream: false`. Returns the
    /// full assistant text. Useful for short prompts (titles, routing TRM
    /// calls) where the streaming overhead isn't worth it.
    pub async fn complete(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
    ) -> Result<String> {
        let url = format!(
            "{}/chat/completions",
            self.provider.base_url.trim_end_matches('/')
        );
        let body = ChatRequest {
            model,
            messages: &messages,
            stream: false,
            tools: None,
        };
        let mut req = self.client.post(&url).json(&body);
        if let Some(key) = &self.provider.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("POST {url} failed"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("complete: HTTP {status} — {body}");
        }
        let body: ChatResponse = resp
            .json()
            .await
            .context("complete: failed to decode response JSON")?;
        let choice = body
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("complete: no choices in response"))?;
        Ok(choice
            .message
            .and_then(|m| m.content)
            .or(choice.text)
            .unwrap_or_default())
    }
}
