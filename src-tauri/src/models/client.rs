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
use reqwest::Response;
use serde::{Deserialize, Serialize};

use super::provider::Provider;
use super::sse::SseStream;

/// Maximum bytes read from a non-success HTTP response body for error
/// reporting. Prevents OOM from a provider that sends back an unbounded
/// error payload (M-01).
const ERROR_BODY_MAX_BYTES: usize = 4096;

/// Maximum bytes read from a successful HTTP response body for JSON
/// deserialization. Prevents OOM from a provider that sends back a
/// gigabyte-sized response (M-01).
const JSON_BODY_MAX_BYTES: usize = 10 * 1024 * 1024; // 10 MiB

/// Read at most `ERROR_BODY_MAX_BYTES` from a response body for error
/// diagnostics. Large bodies are truncated rather than discarded so an
/// actionable error prefix is still visible.
async fn capped_error_body(resp: &mut Response) -> String {
    let mut buf: Vec<u8> = Vec::with_capacity(ERROR_BODY_MAX_BYTES.min(256));
    while let Ok(Some(chunk)) = resp.chunk().await {
        if buf.len() >= ERROR_BODY_MAX_BYTES {
            buf.extend_from_slice(b"... (truncated)");
            break;
        }
        let remaining = ERROR_BODY_MAX_BYTES.saturating_sub(buf.len());
        let end = remaining.min(chunk.len());
        buf.extend_from_slice(&chunk[..end]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Read at most `JSON_BODY_MAX_BYTES` from a response body, then
/// deserialize it as JSON. Returns an error when the body exceeds the cap
/// or cannot be parsed.
async fn capped_json_body<'a, T: serde::de::DeserializeOwned>(resp: &'a mut Response) -> Result<T> {
    let mut buf: Vec<u8> = Vec::with_capacity(JSON_BODY_MAX_BYTES.min(4096));
    while let Some(chunk) = resp.chunk().await? {
        if buf.len() >= JSON_BODY_MAX_BYTES {
            anyhow::bail!("response body exceeds {} byte cap", JSON_BODY_MAX_BYTES);
        }
        let remaining = JSON_BODY_MAX_BYTES.saturating_sub(buf.len());
        let end = remaining.min(chunk.len());
        buf.extend_from_slice(&chunk[..end]);
    }
    Ok(serde_json::from_slice(&buf)?)
}
/// A single message in a chat history. OpenAI's `role` is a string at the
/// wire level (`"system" | "user" | "assistant" | "tool"`); we keep it as a
/// string here so non-OpenAI extensions (e.g. Anthropic-via-proxy) can
/// round-trip without translation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Ask the server to include a final `usage` chunk (Wave 3.2 cost ledger).
    /// Standard OpenAI field; servers that don't recognize it ignore it (they
    /// just don't send usage → the ledger records an unknown cost). Only set on
    /// streaming requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct StreamOptions {
    include_usage: bool,
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

/// Collapse a `GET /models` listing to DISTINCT ids, first-seen order kept.
///
/// Nothing in the OpenAI-compatible spec forbids an endpoint from listing the
/// same id twice, and real ones do — a proxy that fans out to several upstreams
/// (LiteLLM, OpenRouter-style gateways, two llama.cpp servers behind one route)
/// happily returns `gpt-4o` once per upstream. The frontend keys its model
/// lists by name, so a duplicate id is not cosmetic there: it throws
/// `each_key_duplicate` and takes the whole screen down.
///
/// Deduping HERE, at the point the list is produced, is what makes every
/// consumer safe at once — the composer's picker, Settings → Models, and any
/// future caller. Patching one call site only moves the crash to the next one
/// (which is exactly what happened: the composer was fixed, Settings still
/// crashed).
///
/// Order is preserved rather than sorted: the endpoint's own ordering is
/// meaningful (`ensure_running` and the cron path both take `[0]` as "the
/// model this endpoint leads with"), so this only ever REMOVES later repeats.
pub(super) fn distinct_model_ids(ids: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    ids.into_iter()
        .filter(|id| seen.insert(id.clone()))
        .collect()
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
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("failed to build reqwest client")?;
        Ok(Self { client, provider })
    }

    /// The provider this client was built from.
    pub fn provider(&self) -> &Provider {
        &self.provider
    }

    /// `GET {base_url}/models` — return the DISTINCT model ids the provider
    /// exposes, in the order the endpoint listed them. Used by the model picker
    /// (§4) and by `list_models_for`.
    ///
    /// See [`distinct_model_ids`] for why the dedup lives here and not in the
    /// UI: an endpoint that lists one id twice used to crash whichever screen
    /// keyed its list by model name.
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let url = format!("{}/models", self.provider.base_url.trim_end_matches('/'));
        let mut req = self.client.get(&url);
        if let Some(key) = &self.provider.api_key {
            req = req.bearer_auth(key);
        }
        let mut resp = req
            .send()
            .await
            .with_context(|| format!("GET {url} failed"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = capped_error_body(&mut resp).await;
            anyhow::bail!("list_models: HTTP {status} — {body}");
        }
        let body: ModelsResponse = capped_json_body(&mut resp)
            .await
            .context("list_models: failed to decode response JSON")?;
        Ok(distinct_model_ids(body.data.into_iter().map(|m| m.id)))
    }

    /// `POST {base_url}/chat/completions` with `stream: true`. Returns the
    /// raw SSE stream for the caller to consume via `SseStream::next_event`.
    pub async fn stream_chat(&self, model: &str, messages: Vec<ChatMessage>) -> Result<SseStream> {
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
        // Only ask for `usage` on a non-private (billable cloud) endpoint — a
        // local/private call is $0 in the ledger and never consults usage, so
        // there's no reason to send the field there (and the more-likely-strict
        // self-hosted servers never see it). Mirrors the `tools` field's "omit
        // where not needed" precedent.
        let stream_options = (!self.provider.is_private()).then_some(StreamOptions {
            include_usage: true,
        });
        let body = ChatRequest {
            model,
            messages: &messages,
            stream: true,
            stream_options,
            tools,
        };
        let mut req = self.client.post(&url).json(&body);
        if let Some(key) = &self.provider.api_key {
            req = req.bearer_auth(key);
        }
        let mut resp = req
            .send()
            .await
            .with_context(|| format!("POST {url} failed"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = capped_error_body(&mut resp).await;
            anyhow::bail!("stream_chat: HTTP {status} — {body}");
        }
        Ok(SseStream::new(resp))
    }

    /// `POST {base_url}/chat/completions` with `stream: false`. Returns the
    /// full assistant text. Useful for short prompts (titles, routing TRM
    /// calls) where the streaming overhead isn't worth it.
    pub async fn complete(&self, model: &str, messages: Vec<ChatMessage>) -> Result<String> {
        let url = format!(
            "{}/chat/completions",
            self.provider.base_url.trim_end_matches('/')
        );
        let body = ChatRequest {
            model,
            messages: &messages,
            stream: false,
            stream_options: None,
            tools: None,
        };
        let mut req = self.client.post(&url).json(&body);
        if let Some(key) = &self.provider.api_key {
            req = req.bearer_auth(key);
        }
        let mut resp = req
            .send()
            .await
            .with_context(|| format!("POST {url} failed"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = capped_error_body(&mut resp).await;
            anyhow::bail!("complete: HTTP {status} — {body}");
        }
        let body: ChatResponse = capped_json_body(&mut resp)
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
