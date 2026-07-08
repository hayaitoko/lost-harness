//! Model manager tests.
//!
//! Covers:
//!   - `Provider::is_local` / `is_private` for representative endpoints
//!   - `SseStream` parser: split chunks, `[DONE]`, keep-alives, errors,
//!     non-OpenAI shapes (delta vs message vs text), UTF-8 resilience
//!   - `ModelManager` add / remove / lookup / list_models_for (mocked)
//!
//! No real HTTP — the SSE tests use `reqwest::Response::fake` (or build
//! streams from byte chunks), and the manager tests don't touch the network.

use tokio_stream;

use super::client::ChatMessage;
use super::manager::ModelManager;
use super::provider::{Provider, ProviderKind};
use super::sse::{SseEvent, SseStream};

// ── Provider ───────────────────────────────────────────────────────────────

#[test]
fn provider_is_local_matches_kind() {
    let local = Provider::new("lmstudio", "LM Studio", "http://localhost:1234/v1", None, ProviderKind::Local);
    let cloud = Provider::new("openai", "OpenAI", "https://api.openai.com/v1", Some("sk-".into()), ProviderKind::Cloud);
    let custom_public = Provider::new("my-proxy", "My Proxy", "https://api.example.com/v1", None, ProviderKind::Custom);
    let custom_private = Provider::new("tadashi", "Tadashi", "http://10.0.0.5:8080/v1", None, ProviderKind::Custom);

    assert!(local.is_local());
    assert!(!cloud.is_local());
    assert!(!custom_public.is_local());
    assert!(!custom_private.is_local());
}

#[test]
fn provider_is_private_uses_egress_check() {
    let localhost = Provider::new("p1", "p1", "http://localhost:1234/v1", None, ProviderKind::Local);
    let tailnet = Provider::new("p2", "p2", "http://100.97.80.2:8765/v1", None, ProviderKind::Custom);
    let rfc1918 = Provider::new("p3", "p3", "http://192.168.1.10:11434/v1", None, ProviderKind::Local);
    let openai = Provider::new("p4", "p4", "https://api.openai.com/v1", Some("sk".into()), ProviderKind::Cloud);
    let lookalike = Provider::new("p5", "p5", "https://10.evil.com/v1", None, ProviderKind::Custom);
    let m_dns = Provider::new("p6", "p6", "http://macbook.local:1234/v1", None, ProviderKind::Local);
    let unparseable = Provider::new("p7", "p7", "not a url", None, ProviderKind::Cloud);

    assert!(localhost.is_private());
    assert!(tailnet.is_private(), "Tailscale CGNAT 100.64.0.0/10");
    assert!(rfc1918.is_private(), "RFC 1918 192.168.0.0/16");
    assert!(!openai.is_private());
    assert!(!lookalike.is_private(), "host is a public name, not a dotted-quad");
    assert!(m_dns.is_private(), ".local mDNS suffix");
    assert!(!unparseable.is_private(), "unparseable URL is treated as public (refuse)");
}

#[test]
fn provider_serde_roundtrip() {
    let p = Provider::new("openai", "OpenAI", "https://api.openai.com/v1", Some("sk-abc".into()), ProviderKind::Cloud);
    let json = serde_json::to_string(&p).unwrap();
    let back: Provider = serde_json::from_str(&json).unwrap();
    assert_eq!(back.id, "openai");
    assert_eq!(back.api_key.as_deref(), Some("sk-abc"));
    assert_eq!(back.kind, ProviderKind::Cloud);
}

#[test]
fn chat_message_constructors() {
    assert_eq!(ChatMessage::user("hi").role, "user");
    assert_eq!(ChatMessage::system("be terse").role, "system");
    assert_eq!(ChatMessage::assistant("ok").role, "assistant");
}

// ── SSE parser ─────────────────────────────────────────────────────────────

/// Build an `SseStream` from a sequence of byte chunks. The bytes are
/// `reqwest::Response::bytes_stream()` output, so this matches the
/// production wire path.
fn stream_from_chunks(chunks: Vec<Vec<u8>>) -> SseStream {
    // `reqwest::Response::fake` isn't part of the stable public API, so we
    // go through the `#[cfg(test)]` `from_byte_stream` constructor. The
    // error type is `reqwest::Error` because that's what `SseStream` polls
    // for in production — we never produce one in the synthetic test
    // stream, so it doesn't matter how the value is constructed.
    SseStream::from_byte_stream(tokio_stream::iter(
        chunks
            .into_iter()
            .map(|b| Ok::<Vec<u8>, reqwest::Error>(b)),
    ))
}

async fn collect(mut s: SseStream) -> Vec<SseEvent> {
    let mut out = Vec::new();
    while let Some(ev) = s.next_event().await {
        out.push(ev);
    }
    out
}

#[tokio::test]
async fn sse_parses_complete_lines() {
    let body = b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\
                 data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\
                 data: [DONE]\n";
    let events = collect(stream_from_chunks(vec![body.to_vec()])).await;
    assert_eq!(
        events,
        vec![
            SseEvent::Delta("hello".to_string()),
            SseEvent::Delta(" world".to_string()),
            SseEvent::Done,
        ]
    );
}

#[tokio::test]
async fn sse_handles_split_chunks() {
    // Same payload as above, but split mid-line and mid-payload to make
    // sure the buffer doesn't lose data across chunk boundaries.
    let full = b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\
                data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\
                data: [DONE]\n";
    // Split into tiny pieces.
    let chunks: Vec<Vec<u8>> = full
        .chunks(7)
        .map(|c| c.to_vec())
        .collect();
    let events = collect(stream_from_chunks(chunks)).await;
    assert_eq!(
        events,
        vec![
            SseEvent::Delta("hello".into()),
            SseEvent::Delta(" world".into()),
            SseEvent::Done,
        ]
    );
}

#[tokio::test]
async fn sse_handles_crlf_line_endings() {
    let body = b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\r\n\
                 data: [DONE]\r\n";
    let events = collect(stream_from_chunks(vec![body.to_vec()])).await;
    assert_eq!(
        events,
        vec![SseEvent::Delta("x".into()), SseEvent::Done]
    );
}

#[tokio::test]
async fn sse_skips_keepalives_and_comments() {
    let body = b": keep-alive comment\n\
                 \n\
                 data:\n\
                 data: not-json {{ broken\n\
                 data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\
                 data: [DONE]\n";
    let events = collect(stream_from_chunks(vec![body.to_vec()])).await;
    // We don't emit KeepAlive for the actually-discharged lines; the parser
    // swallows them silently. The only events the caller sees are the
    // content delta and the Done sentinel.
    assert_eq!(
        events,
        vec![SseEvent::Delta("ok".into()), SseEvent::Done]
    );
}

#[tokio::test]
async fn sse_emits_error_payload() {
    let body = b"data: {\"error\":{\"message\":\"rate limited\"}}\n\
                 data: [DONE]\n";
    let events = collect(stream_from_chunks(vec![body.to_vec()])).await;
    assert_eq!(
        events,
        vec![
            SseEvent::Error("rate limited".to_string()),
            SseEvent::Done,
        ]
    );
}

#[tokio::test]
async fn sse_falls_back_to_message_and_text_fields() {
    // Some non-OpenAI providers put content in `message.content` or `text`
    // rather than `delta.content`. The parser should still pick it up.
    let body = b"data: {\"choices\":[{\"message\":{\"content\":\"from message\"}}]}\n\
                 data: {\"choices\":[{\"text\":\"from text\"}]}\n\
                 data: [DONE]\n";
    let events = collect(stream_from_chunks(vec![body.to_vec()])).await;
    assert_eq!(
        events,
        vec![
            SseEvent::Delta("from message".into()),
            SseEvent::Delta("from text".into()),
            SseEvent::Done,
        ]
    );
}

#[tokio::test]
async fn sse_handles_role_only_chunk() {
    // The first chunk from OpenAI is often `{"choices":[{"delta":{"role":"assistant"}}]}`.
    // No content yet — the parser should swallow it as a keep-alive and keep
    // reading.
    let body = b"data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\
                 data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\
                 data: [DONE]\n";
    let events = collect(stream_from_chunks(vec![body.to_vec()])).await;
    assert_eq!(
        events,
        vec![SseEvent::Delta("hi".into()), SseEvent::Done]
    );
}

#[tokio::test]
async fn sse_returns_none_after_eof() {
    let body = b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n";
    let events = collect(stream_from_chunks(vec![body.to_vec()])).await;
    // No [DONE] sentinel, but the stream ended. The parser should emit the
    // delta and then return None on the next call.
    assert_eq!(events, vec![SseEvent::Delta("x".into())]);
}

#[tokio::test]
async fn sse_handles_invalid_utf8_gracefully() {
    // Buggy providers sometimes inject raw bytes. `from_utf8_lossy` should
    // keep us running instead of erroring out.
    let mut body = b"data: {\"choices\":[{\"delta\":{\"content\":\"hel".to_vec();
    body.extend_from_slice(&[0xFF, 0xFE]); // invalid UTF-8
    body.extend_from_slice(b"lo\"}}]}\ndata: [DONE]\n");
    let events = collect(stream_from_chunks(vec![body])).await;
    // We don't assert on the exact replacement character (libstd is allowed
    // to change it); we just assert the parser produced a delta and Done.
    assert!(matches!(events.first(), Some(SseEvent::Delta(_))));
    assert_eq!(events.last(), Some(&SseEvent::Done));
}

// ── ModelManager ───────────────────────────────────────────────────────────

#[test]
fn manager_add_remove_and_list() {
    let m = ModelManager::new();
    assert!(m.list_providers().is_empty());

    let p1 = Provider::new("openai", "OpenAI", "https://api.openai.com/v1", Some("sk".into()), ProviderKind::Cloud);
    let p2 = Provider::new("lmstudio", "LM Studio", "http://localhost:1234/v1", None, ProviderKind::Local);
    m.add_provider(p1);
    m.add_provider(p2);
    assert_eq!(m.list_providers().len(), 2);

    // Lookup by id.
    assert!(m.get_provider("openai").is_some());
    assert!(m.get_provider("nope").is_none());

    // Replace existing by id.
    let replacement = Provider::new("openai", "OpenAI (new key)", "https://api.openai.com/v1", Some("sk2".into()), ProviderKind::Cloud);
    m.add_provider(replacement);
    assert_eq!(m.list_providers().len(), 2, "replace, not append");
    assert_eq!(m.get_provider("openai").unwrap().api_key.as_deref(), Some("sk2"));

    m.remove_provider("openai");
    assert_eq!(m.list_providers().len(), 1);
    assert!(m.get_provider("openai").is_none());

    // Removing an unknown id is a no-op.
    m.remove_provider("nope");
    assert_eq!(m.list_providers().len(), 1);
}

#[test]
fn manager_get_client_returns_clone() {
    let m = ModelManager::new();
    let p = Provider::new("lmstudio", "LM Studio", "http://localhost:1234/v1", None, ProviderKind::Local);
    m.add_provider(p);

    let c1 = m.get_client("lmstudio");
    let c2 = m.get_client("lmstudio");
    assert!(c1.is_some());
    assert!(c2.is_some());
    assert_eq!(c1.unwrap().provider().id, "lmstudio");
    assert_eq!(c2.unwrap().provider().id, "lmstudio");

    // Unknown id → None (no client built).
    assert!(m.get_client("missing").is_none());
}

#[tokio::test]
async fn manager_list_models_for_unknown_id_errors() {
    let m = ModelManager::new();
    let res = m.list_models_for("missing").await;
    assert!(res.is_err());
    assert!(res.unwrap_err().to_string().contains("unknown provider"));
}
