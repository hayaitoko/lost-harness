//! Lost Harness — IPC layer (frontend ↔ Rust core)
//!
//! M1 stub: defines the Tauri command surface the Svelte frontend calls and
//! the event surface the Rust core emits. Implementations are intentionally
//! canned — they return placeholder data and emit a few fake tokens — so the
//! frontend can be developed and exercised end-to-end before the agent loop
//! and TRM are wired up.
//!
//! Conventions:
//! - Commands are sync or `async` functions marked with `#[tauri::command]`.
//! - Every command that may fail returns `Result<T, String>` (frontend sees
//!   the rejection as a JS error). Until we have real error types, we use
//!   `String` to keep the stub small.
//! - Events emitted to the frontend follow the naming scheme
//!   `<domain>:<action>` (e.g. `stream:token`). Payloads are serde-derived
//!   structs that the TS bridge mirrors.
//! - The frontend bridge is `src/lib/api/tauri.ts`. Any new command or event
//!   here must be reflected there.

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::time::{sleep, Duration};
use uuid::Uuid;

// ── Response types ──────────────────────────────────────────────────────────

/// Returned by `send_message` once the (stub) assistant response is complete.
/// Mirrors what the real M1 agent loop will return when the model finishes a
/// turn. Streaming tokens arrive separately via the `stream:token` event.
#[derive(Debug, Clone, Serialize)]
pub struct SendMessageResponse {
    /// Server-generated id for the assistant message.
    pub message_id: String,
    /// Final, fully-assembled assistant text. The frontend renders this after
    /// the stream:token events finish so it has the canonical content even if
    /// it missed any token (e.g. page was hidden).
    pub content: String,
    /// The conversation this message belongs to (echoed for convenience).
    pub conversation_id: String,
    /// Which profile the message was handled under. Real M1 will route
    /// through TRM; for now we just stamp "personal".
    pub profile: String,
    /// Epoch milliseconds the response was finalized. Frontend can use this
    /// for ordering if messages from multiple streams interleave.
    pub completed_at: i64,
}

/// Payload of the `stream:token` event. Emitted once per token chunk during
/// a streaming response. The frontend appends `token` to the in-progress
/// assistant message identified by `message_id` in the conversation
/// `conversation_id`.
#[derive(Debug, Clone, Serialize)]
struct StreamTokenPayload {
    token: String,
    conversation_id: String,
    message_id: String,
}

// ── Commands ────────────────────────────────────────────────────────────────

/// Returns the app version string. Real version will come from
/// `tauri::Builder::context().package_info().version` (or a build-script
/// generated constant). For M1 we hardcode the milestone tag.
#[tauri::command]
pub fn get_app_version() -> String {
    "0.1.0-m0".to_string()
}

/// Returns the id of the currently active profile. Real implementation will
/// read from the KV config store (sled/redb) and watch for external changes
/// (e.g. CLI override). For M1 we return the default.
#[tauri::command]
pub fn get_active_profile() -> String {
    "personal".to_string()
}

/// Lists the profile ids known to the app. Matches the four-profile design
/// from the spec: personal / work / school / developer.
#[tauri::command]
pub fn list_profiles() -> Vec<String> {
    vec![
        "personal".to_string(),
        "work".to_string(),
        "school".to_string(),
        "developer".to_string(),
    ]
}

/// Stubs the agent loop. Generates a message id, sleeps 500ms, then emits a
/// handful of fake tokens via the `stream:token` event before returning the
/// final `SendMessageResponse`.
///
/// Real M1 will:
///   1. Push the user message into the conversation store (storage module).
///   2. Run the message through TRM for sensitivity classification.
///   3. Route to the bound model (binding cycle) and stream tokens back.
///   4. Persist the assistant message once the model signals completion.
#[tauri::command]
pub async fn send_message(
    app: AppHandle,
    content: String,
    conversation_id: String,
) -> Result<SendMessageResponse, String> {
    let message_id = Uuid::new_v4().to_string();
    tracing::info!(
        conversation_id = %conversation_id,
        message_id = %message_id,
        content_len = content.len(),
        "send_message received"
    );

    // Pretend "thinking" delay. The frontend can show a spinner during this
    // window — useful for tuning perceived latency once the real agent lands.
    sleep(Duration::from_millis(500)).await;

    // Build a canned reply that demonstrates streaming: split into a few
    // token-sized chunks. Using the user's content as a prefix gives a
    // "useful-looking" response for the stub. A real model will replace this
    // with the actual generation.
    let reply_body = format!(
        "Echo: \"{}\". This is the M1 stub reply — the real agent loop, TRM \
         classification, and model streaming land in subsequent milestones.",
        content
    );
    let tokens: Vec<String> = chunk_reply(&reply_body);

    for token in &tokens {
        // Emit one stream:token event per chunk. Pay attention to the
        // payload shape — `src/lib/api/tauri.ts` mirrors it.
        let payload = StreamTokenPayload {
            token: token.clone(),
            conversation_id: conversation_id.clone(),
            message_id: message_id.clone(),
        };
        if let Err(e) = app.emit("stream:token", payload) {
            // Don't abort the whole response on a single emit failure — log
            // and continue. Real impl will want retry/backoff for the
            // webview channel.
            tracing::warn!(error = %e, "failed to emit stream:token");
        }
        // ~30ms between tokens → ~33 tokens/sec, close to a comfortable
        // human reading rate. Adjust in the real impl based on model speed.
        sleep(Duration::from_millis(30)).await;
    }

    let completed_at = chrono::Utc::now().timestamp_millis();

    Ok(SendMessageResponse {
        message_id,
        content: reply_body,
        conversation_id,
        profile: "personal".to_string(),
        completed_at,
    })
}

/// Emits a single `stream:token` event. Exposed for tests and for the future
/// "external" producer path (e.g. resuming a stream after a tool call). Not
/// used by the frontend's `sendMessage` flow (that drives its own
/// `stream:token` emissions from inside `send_message`).
#[tauri::command]
pub fn stream_token(app: AppHandle, token: String, conversation_id: String, message_id: String) {
    let payload = StreamTokenPayload {
        token,
        conversation_id,
        message_id,
    };
    if let Err(e) = app.emit("stream:token", payload) {
        tracing::warn!(error = %e, "stream_token command failed to emit");
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Splits a response into token-sized chunks that look plausible to the
/// frontend's streaming renderer. Real tokenization is model-specific; this
/// approximation groups ~3-6 word chunks.
fn chunk_reply(text: &str) -> Vec<String> {
    // Naive word-grouping: emit groups of 3-4 words, preserving the original
    // whitespace. Frontend appends verbatim, so spaces matter.
    let words: Vec<&str> = text.split_inclusive(' ').collect();
    let mut out = Vec::with_capacity(words.len() / 3 + 1);
    let mut buf = String::new();
    let mut count = 0;
    for w in words {
        buf.push_str(w);
        count += 1;
        if count >= 3 {
            out.push(std::mem::take(&mut buf));
            count = 0;
        }
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_version_is_m0_tag() {
        assert_eq!(get_app_version(), "0.1.0-m0");
    }

    #[test]
    fn list_profiles_has_four_entries() {
        let p = list_profiles();
        assert_eq!(p.len(), 4);
        assert!(p.contains(&"personal".to_string()));
        assert!(p.contains(&"work".to_string()));
        assert!(p.contains(&"school".to_string()));
        assert!(p.contains(&"developer".to_string()));
    }

    #[test]
    fn active_profile_defaults_to_personal() {
        assert_eq!(get_active_profile(), "personal");
    }

    #[test]
    fn chunk_reply_emits_groups_of_three() {
        let chunks = chunk_reply("alpha beta gamma delta epsilon zeta eta");
        // 7 words → first 3, next 3, last 1
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[2], "eta");
    }

    #[test]
    fn chunk_reply_preserves_text_when_reassembled() {
        let original = "Hello world this is a test of chunking behavior.";
        let reassembled: String = chunk_reply(original).concat();
        assert_eq!(reassembled, original);
    }
}
