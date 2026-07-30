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
//!   - A line longer than `MAX_LINE_LENGTH` → `SseEvent::Error` and the stream
//!     terminates. It is never skipped: one frame can carry the entire turn.
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
    /// Native tool-call deltas from `choices[0].delta.tool_calls` (Q1). Each
    /// fragment carries the call slot it belongs to (`index`) plus whatever
    /// pieces this chunk streamed (the name arrives once; `arguments` arrives
    /// as string fragments to be concatenated per slot by the caller).
    ToolCalls(Vec<ToolCallFragment>),
    /// The `usage` totals from the final chunk (OpenAI sends this when the
    /// request set `stream_options.include_usage`). Feeds the usage ledger's
    /// cost accounting (Wave 3.2). Absent on providers that don't report usage —
    /// the ledger then records an unknown ("flying blind") cost.
    Usage {
        prompt_tokens: u32,
        completion_tokens: u32,
    },
}

/// One streamed piece of a native tool call (OpenAI `delta.tool_calls[i]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallFragment {
    pub index: usize,
    pub name: Option<String>,
    pub arguments: String,
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

/// Maximum length of a single SSE line (including the `data:` prefix).
///
/// A line beyond this is a hard, surfaced FAILURE (`SseEvent::Error`, stream
/// terminated) — never a silent skip. One SSE frame can legitimately carry an
/// entire assistant turn (LiteLLM-style fake streaming, a buffering reverse
/// proxy, several local OpenAI-shim servers) or a whole tool-call argument, so
/// dropping an over-cap line as a keep-alive persists an empty reply and
/// dispatches no tool call, with nothing the user can see. Loud failure only.
///
/// The value sits deliberately below `MAX_BUFFER_SIZE`: the buffer cap is what
/// actually bounds our memory use, and it is checked before bytes are appended,
/// so a line cap at or above it could never be reached. 256 KiB admits a
/// realistic single-frame completion (16 KiB did not — it is roughly 4k tokens
/// of text) while keeping the parse of any one line bounded (M-01).
pub(crate) const MAX_LINE_LENGTH: usize = 256 * 1024; // 256 KiB

/// Maximum total bytes the SSE buffer can accumulate before we refuse to
/// append more data. Prevents OOM from a provider that streams an
/// unbounded preamble (M-01).
const MAX_BUFFER_SIZE: usize = 512 * 1024; // 512 KiB

impl SseStream {
    /// Wrap a reqwest `bytes_stream()` so the caller can `await` chunks of
    /// the SSE body. We map each `Bytes` chunk into a `Vec<u8>` so the
    /// stream's item type doesn't require the `bytes` crate in our direct
    /// deps. We take ownership because we pin the stream internally and
    /// need a `'static` boxed stream to poll it.
    pub fn new(response: reqwest::Response) -> Self {
        // Map each `Bytes` chunk into a `Vec<u8>` so the stream's item
        // type doesn't require the `bytes` crate in our direct deps.
        let mapped = response.bytes_stream().map(|res| res.map(|b| b.to_vec()));
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
                    // Enforce a cumulative buffer cap so a provider that
                    // streams an unbounded invalid preamble cannot OOM us
                    // (M-01).
                    if self.buffer.len() + bytes.len() > MAX_BUFFER_SIZE {
                        self.finished = true;
                        return Some(SseEvent::Error(
                            "SSE buffer exceeded maximum size (512 KiB)".into(),
                        ));
                    }
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
        // Enforce the per-line length cap (M-01). An over-cap line is a
        // TERMINAL, SURFACED failure — never a silent skip: one frame can carry
        // the entire assistant turn (single-frame `message.content`) or a whole
        // tool call's `arguments`, so discarding it as a keep-alive persisted an
        // empty assistant message and dispatched no tool call, with no error the
        // user could see. Name the cap and the observed size so the diagnostic
        // is actionable (HI3).
        if line.len() > MAX_LINE_LENGTH {
            self.finished = true;
            return Some(SseEvent::Error(format!(
                "SSE line of {} bytes exceeds the {}-byte per-line cap ({} KiB) — \
                 the provider sent one frame larger than this parser accepts, \
                 so the reply was not delivered",
                line.len(),
                MAX_LINE_LENGTH,
                MAX_LINE_LENGTH / 1024
            )));
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
        // Native tool-call deltas (Q1). Content and tool_calls never share a
        // chunk in practice; content wins above if a server ever mixes them
        // (the tool fragments of such a chunk would be re-sent by no server
        // we target — accepted limitation, logged at the assembler).
        let fragments = parsed.tool_call_fragments();
        if !fragments.is_empty() {
            return SseEvent::ToolCalls(fragments);
        }
        // The final usage chunk (empty `choices`, a `usage` object) — surface
        // the token totals for the cost ledger. Pulled leniently: a token count
        // that isn't a non-negative integer is treated as absent (0), so a
        // malformed usage yields no event (→ unknown cost) rather than poisoning
        // the line. Clamped to u32 range.
        if let Some(u) = &parsed.usage {
            let field = |k: &str| -> u32 {
                u.get(k)
                    .and_then(|v| v.as_u64())
                    .map(|n| n.min(u32::MAX as u64) as u32)
                    .unwrap_or(0)
            };
            let prompt_tokens = field("prompt_tokens");
            let completion_tokens = field("completion_tokens");
            if prompt_tokens > 0 || completion_tokens > 0 {
                return SseEvent::Usage {
                    prompt_tokens,
                    completion_tokens,
                };
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
    /// Token usage totals (final chunk, when `stream_options.include_usage` was
    /// requested). Kept as a permissive `Value` — NOT a typed struct — so a
    /// provider that ships a malformed `usage` (e.g. string or float token
    /// counts, or usage riding on a content chunk) can NEVER fail the whole
    /// line's parse and drop co-located `delta.content`. Tokens are pulled
    /// leniently at the decode site; anything non-integer is treated as absent.
    #[serde(default)]
    usage: Option<serde_json::Value>,
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
    #[serde(default)]
    tool_calls: Option<Vec<SseToolCallDelta>>,
}

/// Wire shape of one `delta.tool_calls[i]` entry (OpenAI streaming).
#[derive(Debug, Deserialize)]
struct SseToolCallDelta {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    function: Option<SseToolCallFunction>,
}

#[derive(Debug, Deserialize)]
struct SseToolCallFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
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

    /// Extract native tool-call fragments from `choices[0].delta.tool_calls`
    /// (falling back to `message.tool_calls` for non-streaming-shaped
    /// providers). Missing `index` defaults to slot 0 (single-call servers).
    fn tool_call_fragments(&self) -> Vec<ToolCallFragment> {
        let Some(choice) = self.choices.first() else {
            return Vec::new();
        };
        let deltas = choice
            .delta
            .as_ref()
            .and_then(|d| d.tool_calls.as_ref())
            .or_else(|| choice.message.as_ref().and_then(|m| m.tool_calls.as_ref()));
        let Some(deltas) = deltas else {
            return Vec::new();
        };
        deltas
            .iter()
            .map(|d| ToolCallFragment {
                index: d.index.unwrap_or(0),
                name: d.function.as_ref().and_then(|f| f.name.clone()),
                arguments: d
                    .function
                    .as_ref()
                    .and_then(|f| f.arguments.clone())
                    .unwrap_or_default(),
            })
            .collect()
    }
}
