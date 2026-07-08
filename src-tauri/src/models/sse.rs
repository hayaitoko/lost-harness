//! Incremental SSE parser for OpenAI-compatible chat-completion streams.
//!
//! Port of `shared/sse.mjs` from the Electron app. The wire format is:
//!
//! ```text
//! data: {"choices":[{"delta":{"content":"hello"}}]}
//! data: {"choices":[{"delta":{"content":" world"},"finish_reason":"stop"}]}
//! data: [DONE]
//! ```
//!
//! Streams are split across arbitrary byte boundaries (a single JSON payload
//! may straddle two `bytes::Bytes` chunks), so we maintain a UTF-8 buffer and
//! drain complete `\n`-terminated lines from it on every `next_event` call.
//!
//! Robustness rules (mirroring the JS version):
//!   - Skip comment lines starting with `:`.
//!   - Skip empty `data:` lines (keep-alives).
//!   - Skip lines whose payload is not valid JSON (treat as keep-alive).
//!   - `[DONE]` sentinel → `SseEvent::Done`, then `None`.
//!   - `json.error` → `SseEvent::Error(msg)`. Stream continues after; the
//!     caller decides whether to terminate.
//!   - Pull `choices[0].delta.content` (falling back to `message.content` /
//!     `text` for non-OpenAI endpoints). Skip empty deltas.

use anyhow::Result;
use serde::Deserialize;
use std::pin::Pin;
use tokio_stream::{Stream, StreamExt};

/// One decoded event from the SSE stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseEvent {
    /// A content delta from `choices[0].delta.content`.
    Delta(String),
    /// The server sent `data: [DONE]`. The stream will return `None` next.
    Done,
    /// A no-op line (`: keep-alive`, empty `data:`, malformed JSON). The
    /// caller does not need to do anything with this — it's emitted so tests
    /// can verify the parser's behaviour, and so the caller's debug logging
    /// can show when the server is heartbeating.
    KeepAlive,
    /// The payload contained an `error` field. The string is the provider's
    /// error message when available.
    Error(String),
}

/// Incremental SSE stream parser. Construct via `SseStream::new` and call
/// `next_event` in a loop. The stream returns `None` on EOF or after a
/// `[DONE]` sentinel.
pub struct SseStream {
    /// We work in `Vec<u8>` instead of `bytes::Bytes` because the `bytes`
    /// crate is not in this crate's direct dependency set (it's only
    /// transitive via `reqwest`). The conversion happens once per chunk on
    /// the boundary; the parser itself never needs `Bytes` semantics.
    inner: Pin<Box<dyn Stream<Item = Result<Vec<u8>, reqwest::Error>> + Send>>,
    /// UTF-8 text assembled from byte chunks. Drained one line at a time.
    buffer: String,
    /// Set once we emit `SseEvent::Done`. The next call returns `None`.
    finished: bool,
}

impl SseStream {
    /// Wrap a reqwest `bytes_stream()` so the caller can `await` chunks of
    /// the SSE body. We map each `Bytes` chunk into a `Vec<u8>` so the
    /// stream's item type doesn't require the `bytes` crate in our direct
    /// deps. We take ownership because we pin the stream internally and
    /// need a `'static` boxed stream to poll it.
    pub fn new(response: reqwest::Response) -> Self {
        // Map each `Bytes` chunk into a `Vec<u8>` so the stream's item
        // type doesn't require the `bytes` crate in our direct deps.
        let mapped = response
            .bytes_stream()
            .map(|res| res.map(|b| b.to_vec()));
        Self {
            inner: Box::pin(mapped),
            buffer: String::new(),
            finished: false,
        }
    }

    /// Test-only constructor that lets us feed arbitrary byte chunks
    /// without standing up a real HTTP server. `reqwest::Response::fake`
    /// isn't stable, so we expose a small back door used only by `tests.rs`.
    #[cfg(test)]
    pub(crate) fn from_byte_stream<S>(stream: S) -> Self
    where
        S: Stream<Item = Result<Vec<u8>, reqwest::Error>> + Send + 'static,
    {
        Self {
            inner: Box::pin(stream),
            buffer: String::new(),
            finished: false,
        }
    }

    /// Pull the next event from the stream. Returns `None` on EOF or after
    /// a `Done` event.
    pub async fn next_event(&mut self) -> Option<SseEvent> {
        if self.finished {
            return None;
        }

        loop {
            // Drain every complete line in the buffer, returning the first
            // non-keepalive one. Keep-alives (`:`, empty `data:`, malformed
            // JSON) are silently discarded; they never reach the caller.
            while let Some(event) = self.pop_line() {
                if !matches!(event, SseEvent::KeepAlive) {
                    return Some(event);
                }
            }

            // Buffer is empty of complete lines. Pull more bytes.
            match self.inner.next().await {
                Some(Ok(bytes)) => {
                    // It's a protocol violation to send invalid UTF-8 mid-stream,
                    // but `from_utf8_lossy` keeps us resilient against buggy
                    // providers (some inject raw bytes for heartbeats).
                    self.buffer.push_str(&String::from_utf8_lossy(&bytes));
                }
                Some(Err(e)) => {
                    // Transport error — surface to caller and terminate.
                    self.finished = true;
                    return Some(SseEvent::Error(format!("stream transport error: {e}")));
                }
                None => {
                    // EOF. Flush any remaining buffer that doesn't end in \n.
                    self.finished = true;
                    if self.buffer.is_empty() {
                        return None;
                    }
                    let event = self.parse_line(&self.buffer.clone());
                    self.buffer.clear();
                    if matches!(event, SseEvent::KeepAlive) {
                        return None;
                    }
                    return Some(event);
                }
            }
        }
    }

    /// Attempt to extract one event from the front of `self.buffer`. Returns
    /// `None` if no complete line is available yet. The caller is
    /// responsible for skipping `KeepAlive` results.
    fn pop_line(&mut self) -> Option<SseEvent> {
        let newline = self.buffer.find('\n')?;
        let mut line: String = self.buffer.drain(..=newline).collect();
        // Strip the trailing \n (drain(..=) includes it).
        line.pop();
        // Strip optional \r for CRLF streams.
        if line.ends_with('\r') {
            line.pop();
        }
        Some(self.parse_line(&line))
    }

    /// Classify a single SSE line into an `SseEvent`. Empty / comment /
    /// empty-data / malformed-JSON lines all return `KeepAlive`.
    fn parse_line(&self, line: &str) -> SseEvent {
        // Comment line (SSE spec).
        if line.starts_with(':') {
            return SseEvent::KeepAlive;
        }
        // Must be a `data:` line.
        let Some(payload) = line.strip_prefix("data:") else {
            return SseEvent::KeepAlive;
        };
        let payload = payload.trim();
        if payload.is_empty() {
            return SseEvent::KeepAlive;
        }
        if payload == "[DONE]" {
            return SseEvent::Done;
        }
        let parsed: SsePayload = match serde_json::from_str(payload) {
            Ok(p) => p,
            Err(_) => return SseEvent::KeepAlive, // malformed line — never crash
        };
        if let Some(msg) = parsed.error_message() {
            return SseEvent::Error(msg);
        }
        let delta = parsed.first_delta();
        if let Some(text) = delta {
            if !text.is_empty() {
                return SseEvent::Delta(text);
            }
        }
        // No content and no error — keep-alive-like, e.g. a `role` chunk.
        SseEvent::KeepAlive
    }
}

// ── Wire format ────────────────────────────────────────────────────────────
//
// We only deserialize the bits we need. Anything we don't recognise is
// ignored, which makes us forward-compatible with new OpenAI fields and
// tolerant of non-OpenAI providers (Ollama, OpenRouter, Together, etc.) that
// add their own extensions.

#[derive(Debug, Deserialize)]
struct SsePayload {
    #[serde(default)]
    choices: Vec<SseChoice>,
    /// OpenAI's error envelope: `{ "error": { "message": "..." } }`.
    /// Some providers use a string instead.
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct SseChoice {
    #[serde(default)]
    delta: Option<SseMessage>,
    #[serde(default)]
    message: Option<SseMessage>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SseMessage {
    #[serde(default)]
    content: Option<String>,
}

impl SsePayload {
    fn error_message(&self) -> Option<String> {
        let v = self.error.as_ref()?;
        let msg = match v {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Object(map) => map
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("Provider error")
                .to_string(),
            _ => "Provider error".to_string(),
        };
        Some(msg)
    }

    /// Returns the first non-empty content delta we can find, or `None`.
    /// Tries `choices[0].delta.content` (OpenAI streaming), then
    /// `choices[0].message.content` (some non-streaming-shaped providers),
    /// then `choices[0].text` (legacy completions).
    fn first_delta(&self) -> Option<String> {
        let choice = self.choices.first()?;
        if let Some(d) = &choice.delta {
            if let Some(c) = &d.content {
                if !c.is_empty() {
                    return Some(c.clone());
                }
            }
        }
        if let Some(m) = &choice.message {
            if let Some(c) = &m.content {
                if !c.is_empty() {
                    return Some(c.clone());
                }
            }
        }
        choice.text.as_ref().filter(|s| !s.is_empty()).cloned()
    }
}
