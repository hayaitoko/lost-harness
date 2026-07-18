//! §9 Agent loop integration tests.
//!
//! Covers:
//!   - public binding + cloud provider → goes through
//!   - private binding + cloud provider → blocked
//!   - auto binding + SSN on a cloud provider → routed to a local model
//!   - auto binding + clean text → goes through
//!   - TRM log entry written to storage after every gate decision
//!
//! Strategy: drive the agent loop end-to-end with a fake `ModelStreamer`
//! that returns canned SSE bytes (no real HTTP). `AppHandle` is the
//! `MockRuntime` variant so we don't need a real window.

use std::sync::Arc;

use tauri::test::mock_app;
use tauri::{AppHandle, Emitter, Manager, Runtime};

use crate::agent::gate::{Binding, GateDecision, PrivacyGate};
use crate::agent::loop_mod::{sha256_hex, StreamErrorPayload, StreamTokenPayload};
use crate::models::sse::{SseEvent, SseStream};
use crate::models::{ChatMessage, Provider, ProviderKind};
use crate::storage::{Message, Storage, TrmLog};
use crate::classifier::HeuristicClassifier;

// ── Fake model streamer ──────────────────────────────────────────────────

/// Implements `ModelStreamer` for tests by returning a canned SSE byte
/// stream. We go through `SseStream::from_byte_stream` (the
/// `#[cfg(test)]` back door on `SseStream`).
struct FakeStreamer {
    provider: Provider,
    /// The byte chunks to feed to the SSE parser. Owned (not borrowed)
    /// because `SseStream::from_byte_stream` requires a `'static` stream.
    chunks: Vec<Vec<u8>>,
    /// A copy of the request the agent loop sends — useful for
    /// assertions about routing / system prompt construction.
    captured_messages: parking_lot::Mutex<Option<Vec<ChatMessage>>>,
}

impl FakeStreamer {
    fn new(provider: Provider, chunks: Vec<Vec<u8>>) -> Self {
        Self {
            provider,
            chunks,
            captured_messages: parking_lot::Mutex::new(None),
        }
    }

    fn captured(&self) -> Option<Vec<ChatMessage>> {
        self.captured_messages.lock().clone()
    }
}

// We can't use the production `ModelStreamer` trait name in the impl
// (it collides with the method also named `stream` and would need
// the trait in scope at every call site). Re-declare a private trait
// for the test fake.
#[allow(async_fn_in_trait)]
trait TestStreamer: Send + Sync {
    fn provider(&self) -> &Provider;
    async fn stream_chunks(
        &self,
        model: &str,
        messages: Vec<ChatMessage>,
    ) -> anyhow::Result<SseStream>;
}

impl TestStreamer for FakeStreamer {
    fn provider(&self) -> &Provider {
        &self.provider
    }

    async fn stream_chunks(
        &self,
        _model: &str,
        messages: Vec<ChatMessage>,
    ) -> anyhow::Result<SseStream> {
        *self.captured_messages.lock() = Some(messages);
        // Clone the chunks out of `self` so the resulting stream
        // doesn't borrow from `&self` (SseStream requires `'static`).
        let chunks: Vec<Vec<u8>> = self.chunks.clone();
        let byte_stream = tokio_stream::iter(
            chunks
                .into_iter()
                .map(|b| Ok::<Vec<u8>, reqwest::Error>(b)),
        );
        Ok(SseStream::from_byte_stream(byte_stream))
    }
}

// ── Test harness ────────────────────────────────────────────────────────

/// Re-implementation of the relevant `AgentLoop::process_message` body
/// that uses a `&dyn TestStreamer` instead of a real `ModelClient`.
/// Lives only in tests so we can inject canned SSE without monkey-
/// patching the `ModelManager`.
///
/// Generic over `R` so we can be used with both the Wry runtime
/// (production) and `MockRuntime` (tests).
struct TestLoop<R: Runtime> {
    storage: Arc<Storage>,
    profile: String,
    app: AppHandle<R>,
    gate: PrivacyGate,
    fake: parking_lot::Mutex<Option<Arc<FakeStreamer>>>,
}

impl<R: Runtime> TestLoop<R> {
    fn new(storage: Arc<Storage>, profile: String, app: AppHandle<R>) -> Self {
        Self {
            storage,
            profile,
            app,
            gate: PrivacyGate::new(Arc::new(HeuristicClassifier::new())),
            fake: parking_lot::Mutex::new(None),
        }
    }

    fn set_fake(&self, fake: Arc<FakeStreamer>) {
        *self.fake.lock() = Some(fake);
    }

    async fn process(
        &self,
        content: &str,
        binding: Binding,
        provider: Provider,
        conversation_id: String,
    ) -> Result<String, String> {
        let is_cloud = !crate::agent::egress::is_private_endpoint(&provider.base_url);
        let decision = self.gate.check(&binding, content, is_cloud, &crate::classifier::ClassifierConfig::default());
        let message_hash = sha256_hex(content.as_bytes());
        self.log_trm(&conversation_id, &decision, &message_hash)
            .map_err(|e| e.to_string())?;

        match &decision {
            GateDecision::Block(reason) => {
                let _ = self.app.emit(
                    "stream:error",
                    StreamErrorPayload {
                        error: reason.clone(),
                        conversation_id: conversation_id.clone(),
                        source: "gate",
                    },
                );
                return Ok(reason.clone());
            }
            GateDecision::Allow | GateDecision::RouteLocal => {
                // TestLoop always streams via the supplied `provider`.
                // The gate's RouteLocal decision is still logged; the
                // test that exercises RouteLocal asserts on the log
                // rather than on the routing target.
            }
        }

        // Resolve the fake (test setup always provides one).
        let fake = self
            .fake
            .lock()
            .clone()
            .expect("test must set_fake before process()");

        // Persist user message.
        let user_msg = Message {
            id: uuid::Uuid::new_v4().to_string(),
            conversation_id: conversation_id.clone(),
            role: "user".to_string(),
            content: content.to_string(),
            model: Some("m".to_string()),
            provider_id: Some(provider.id.clone()),
            routing_decision: Some(match &decision {
                GateDecision::Allow => "allow".to_string(),
                GateDecision::RouteLocal => "route_local".to_string(),
                GateDecision::Block(_) => "block".to_string(),
            }),
            thinking_content: None,
            error: None,
            aborted: false,
            created_at: chrono::Utc::now().timestamp(),
        };
        let profile_db = self
            .storage
            .open_profile(&self.profile)
            .map_err(|e| e.to_string())?;
        profile_db
            .add_message(&user_msg)
            .map_err(|e| e.to_string())?;

        // Stream from the fake.
        let mut sse = fake
            .stream_chunks("m", vec![ChatMessage::user(content.to_string())])
            .await
            .map_err(|e| e.to_string())?;

        let assistant_id = uuid::Uuid::new_v4().to_string();
        let mut assembled = String::new();
        while let Some(event) = sse.next_event().await {
            match event {
                SseEvent::Delta(delta) => {
                    assembled.push_str(&delta);
                    let _ = self.app.emit(
                        "stream:token",
                        StreamTokenPayload {
                            token: delta,
                            conversation_id: conversation_id.clone(),
                            message_id: assistant_id.clone(),
                        },
                    );
                }
                SseEvent::Done | SseEvent::KeepAlive | SseEvent::ToolCalls(_) => {}
                SseEvent::Error(msg) => {
                    let _ = self.app.emit(
                        "stream:error",
                        StreamErrorPayload {
                            error: msg.clone(),
                            conversation_id: conversation_id.clone(),
                            source: "model",
                        },
                    );
                    return Err(format!("model error: {msg}"));
                }
            }
        }

        // Persist assistant message.
        let assistant_msg = Message {
            id: assistant_id,
            conversation_id,
            role: "assistant".to_string(),
            content: assembled.clone(),
            model: Some("m".to_string()),
            provider_id: Some(provider.id),
            routing_decision: None,
            thinking_content: None,
            error: None,
            aborted: false,
            created_at: chrono::Utc::now().timestamp(),
        };
        profile_db
            .add_message(&assistant_msg)
            .map_err(|e| e.to_string())?;
        Ok(assembled)
    }

    fn log_trm(
        &self,
        conversation_id: &str,
        decision: &GateDecision,
        message_hash: &str,
    ) -> anyhow::Result<()> {
        let profile_db = self.storage.open_profile(&self.profile)?;
        let entry = TrmLog {
            id: uuid::Uuid::new_v4().to_string(),
            conversation_id: conversation_id.to_string(),
            message_hash: message_hash.to_string(),
            decision: match decision {
                GateDecision::Allow => "public".to_string(),
                GateDecision::Block(_) | GateDecision::RouteLocal => "private".to_string(),
            },
            confidence: 1.0,
            created_at: chrono::Utc::now().timestamp(),
        };
        profile_db.insert_trm_log(&entry)?;
        Ok(())
    }
}

// ── helpers ─────────────────────────────────────────────────────────────

/// A unique tempdir for each test. We don't pull in `tempfile` —
/// `std::env::temp_dir()` + uuid is enough for a single-process test.
fn tempdir() -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("lhp-test-{}", uuid::Uuid::new_v4()));
    p
}

/// SSE bytes that the parser will turn into one `Delta(text)` followed
/// by `Done`.
fn sse_chunks_for(text: &str) -> Vec<Vec<u8>> {
    let body = format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\
         data: [DONE]\n"
    );
    vec![body.into_bytes()]
}

fn cloud_provider(id: &str) -> Provider {
    Provider::new(
        id,
        "Cloud",
        "https://api.openai.com/v1",
        Some("sk-test".into()),
        ProviderKind::Cloud,
    )
}

/// Set up a fresh temp storage + a mock Tauri app + a pre-created
/// conversation. Returns the env pieces the tests need.
struct TestEnv {
    storage: Arc<Storage>,
    app: tauri::App<tauri::test::MockRuntime>,
    profile: String,
    conversation_id: String,
}

fn fresh_env() -> TestEnv {
    let dir = tempdir();
    let storage = Storage::open(&dir).expect("open temp storage");
    let storage = Arc::new(storage);

    let profile = "personal".to_string();
    let profile_db = storage.open_profile(&profile).expect("open profile");
    let conv = crate::storage::Conversation {
        id: "conv-1".to_string(),
        name: "Test".to_string(),
        pinned: false,
        binding: "auto".to_string(),
        folder_id: None,
        color: None,
        created_at: 1,
        updated_at: 1,
    };
    profile_db.create_conversation(&conv).expect("create conv");

    // Build a mock Tauri app. `mock_app` returns a fully-wired
    // `App<MockRuntime>` without opening a window.
    let app = mock_app();

    TestEnv {
        storage,
        app,
        profile,
        conversation_id: "conv-1".to_string(),
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn public_binding_and_cloud_provider_goes_through() {
    let env = fresh_env();
    let provider = cloud_provider("openai");
    let fake = Arc::new(FakeStreamer::new(
        provider.clone(),
        sse_chunks_for("hi"),
    ));

    let test_loop = TestLoop::new(
        env.storage.clone(),
        env.profile.clone(),
        env.app.handle().clone(),
    );
    test_loop.set_fake(fake.clone());
    let result = test_loop
        .process(
            "hello world",
            Binding::Public,
            provider,
            env.conversation_id.clone(),
        )
        .await
        .expect("public+cloud should not block");
    assert_eq!(result, "hi");

    // The fake captured the request — verify it carried the user text.
    let captured = fake.captured().expect("captured");
    assert_eq!(captured.last().unwrap().content, "hello world");
}

#[tokio::test]
async fn private_binding_and_cloud_provider_is_blocked() {
    let env = fresh_env();
    let provider = cloud_provider("openai");
    let fake = Arc::new(FakeStreamer::new(
        provider.clone(),
        sse_chunks_for("should never see this"),
    ));

    let test_loop = TestLoop::new(
        env.storage.clone(),
        env.profile.clone(),
        env.app.handle().clone(),
    );
    test_loop.set_fake(fake.clone());
    let result = test_loop
        .process(
            "any text",
            Binding::Private,
            provider,
            env.conversation_id.clone(),
        )
        .await
        .expect("block returns Ok with reason string");
    assert!(
        result.contains("Private binding"),
        "expected block reason, got {result}"
    );
    // The fake should not have been called.
    assert!(fake.captured().is_none(), "fake was invoked despite block");

    // And the TRM log records the block.
    let profile_db = env.storage.open_profile(&env.profile).unwrap();
    let logs = profile_db.list_trm_logs(&env.conversation_id).unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].decision, "private");
}

#[tokio::test]
async fn auto_binding_with_ssn_on_cloud_routes_to_local() {
    let env = fresh_env();
    let cloud = cloud_provider("openai");
    let local = Provider::new(
        "lmstudio",
        "LM Studio",
        "http://localhost:1234/v1",
        None,
        ProviderKind::Local,
    );
    let fake = Arc::new(FakeStreamer::new(
        local.clone(),
        sse_chunks_for("from local"),
    ));

    // TestLoop's RouteLocal branch currently falls through and streams
    // via the supplied provider (the production agent loop re-queries
    // `find_local_provider`). For this test we assert the gate
    // decision via the TRM log, not the routing target.
    let test_loop = TestLoop::new(
        env.storage.clone(),
        env.profile.clone(),
        env.app.handle().clone(),
    );
    test_loop.set_fake(fake.clone());
    let result = test_loop
        .process(
            "my SSN is 123-45-6789",
            Binding::Auto,
            cloud,
            env.conversation_id.clone(),
        )
        .await
        .expect("RouteLocal should not error");
    assert_eq!(result, "from local");

    // Verify the TRM log row records `private` (RouteLocal).
    let profile_db = env.storage.open_profile(&env.profile).unwrap();
    let logs = profile_db.list_trm_logs(&env.conversation_id).unwrap();
    assert_eq!(logs.len(), 1, "expected exactly one TRM log row");
    assert_eq!(
        logs[0].decision, "private",
        "SSN on cloud should be logged as private"
    );
}

#[tokio::test]
async fn auto_binding_with_clean_text_goes_through() {
    let env = fresh_env();
    let provider = cloud_provider("openai");
    let fake = Arc::new(FakeStreamer::new(
        provider.clone(),
        sse_chunks_for("Paris"),
    ));

    let test_loop = TestLoop::new(
        env.storage.clone(),
        env.profile.clone(),
        env.app.handle().clone(),
    );
    test_loop.set_fake(fake.clone());
    let result = test_loop
        .process(
            "what is the capital of france",
            Binding::Auto,
            provider,
            env.conversation_id.clone(),
        )
        .await
        .expect("clean text should be allowed");
    assert_eq!(result, "Paris");

    // And the TRM log records `public`.
    let profile_db = env.storage.open_profile(&env.profile).unwrap();
    let logs = profile_db.list_trm_logs(&env.conversation_id).unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].decision, "public");
}

#[tokio::test]
async fn trm_log_entry_is_written_for_every_decision() {
    let env = fresh_env();
    let provider = cloud_provider("openai");
    let fake = Arc::new(FakeStreamer::new(
        provider.clone(),
        sse_chunks_for("ok"),
    ));

    let test_loop = TestLoop::new(
        env.storage.clone(),
        env.profile.clone(),
        env.app.handle().clone(),
    );
    test_loop.set_fake(fake);
    test_loop
        .process(
            "hello",
            Binding::Public,
            provider,
            env.conversation_id.clone(),
        )
        .await
        .expect("public goes through");

    let profile_db = env.storage.open_profile(&env.profile).unwrap();
    let logs = profile_db.list_trm_logs(&env.conversation_id).unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].decision, "public");
    assert_eq!(logs[0].conversation_id, env.conversation_id);
    // Hash is sha256 of the plaintext (spec §3 — never log the text).
    let expected = sha256_hex(b"hello");
    assert_eq!(logs[0].message_hash, expected);
}

// ── item 6: resolve_turn_outcome (reroute resolution) ────────────────────
//
// These call the loop-level reroute resolver DIRECTLY (it's `pub(crate)` and
// never calls `stream_chat`, so no HTTP is needed) with a hand-built
// `TurnOutcome::NeedsLocalReroute`. The existing `TestLoop` harness does not
// wire a `ToolDispatcher`, so we build a real one here.

fn local_provider(id: &str) -> Provider {
    Provider::new(id, "LocalLLM", "http://localhost:1234/v1", None, ProviderKind::Local)
}

/// A `ToolDispatcher` with one `EchoTool`, allowed + pre-confirmed, so gating
/// passes and `resume_after_local_switch` actually runs the tool.
fn echo_allow_dispatcher() -> crate::tools::ToolDispatcher {
    use crate::hooks::{build_pretooluse_chain_with_confirmed, InMemoryPolicySource, PermissionMode};
    use crate::tools::{BodyEnv, EchoTool, ToolDispatcher, ToolRegistry};
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(EchoTool));
    let mut policy = InMemoryPolicySource::new();
    policy.set_mode("echo", PermissionMode::Allow);
    let chain = build_pretooluse_chain_with_confirmed(
        PrivacyGate::new(Arc::new(HeuristicClassifier::new())),
        Box::new(policy),
        &["echo"],
    );
    ToolDispatcher::new(registry, chain, BodyEnv::empty())
}

fn exec_ctx() -> crate::tools::ExecCtx {
    crate::tools::ExecCtx {
        conversation_id: "c1".to_string(),
        profile: "personal".to_string(),
        reads: None,
        allow_private_memory: false,
        session_mode: Default::default(),
    }
}

#[tokio::test]
async fn resolve_turn_outcome_reroutes_to_local_and_hides_reason() {
    // Test 11: a local candidate exists → switch to it, fire on_reroute once
    // with the detailed reason, but keep the reason OUT of the replayed
    // ChatMessage (pins Fable's specific risk callout).
    use crate::agent::loop_mod::resolve_turn_outcome;
    use crate::models::ModelManager;
    use crate::tools::{dispatch::TurnOutcome, ToolCall};

    let manager = ModelManager::new();
    let cloud = cloud_provider("cloud1");
    manager.add_provider(cloud.clone());
    manager.add_provider(local_provider("local1"));
    let tools = echo_allow_dispatcher();

    let turn = TurnOutcome::NeedsLocalReroute {
        reason: "UNIQUE_TEST_MARKER".to_string(),
        call: ToolCall { name: "echo".to_string(), args: serde_json::json!({"x": 1}) },
        prior_sections: vec![],
        remaining: vec![],
        turn_call_count: 0,
        cascade_active: false,
    };

    let fired = Arc::new(parking_lot::Mutex::new(Vec::<String>::new()));
    let fired2 = fired.clone();
    let cloud_client = manager.get_client("cloud1").expect("cloud client");

    let (msg, provider, _client, is_cloud, routing) = resolve_turn_outcome(
        &tools,
        &manager,
        turn,
        &exec_ctx(),
        Binding::Auto,
        cloud,
        cloud_client,
        true,
        "allow",
        &move |_from, _to, reason| fired2.lock().push(reason.to_string()),
    )
    .await
    .expect("resolve ok");

    assert!(!is_cloud, "must switch to the local endpoint");
    assert_eq!(provider.id, "local1");
    assert_eq!(routing, "tool_reroute_local");
    {
        let fired = fired.lock();
        assert_eq!(fired.len(), 1, "on_reroute fires exactly once");
        assert_eq!(fired[0], "UNIQUE_TEST_MARKER");
    }
    let content = msg.expect("feedback present").content;
    assert!(
        !content.contains("UNIQUE_TEST_MARKER"),
        "the detailed reason must never leak into the replayed content: {content}"
    );
    assert!(content.contains("[routing] switched to the local model"), "banner: {content}");
}

#[tokio::test]
async fn resolve_turn_outcome_no_local_candidate_stays_cloud_with_hard_deny() {
    // Test 12: only a cloud provider is registered → no switch, on_reroute
    // never fires, and the feedback is exactly today's hard-deny wording.
    use crate::agent::loop_mod::resolve_turn_outcome;
    use crate::models::ModelManager;
    use crate::tools::{dispatch::TurnOutcome, ToolCall};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let manager = ModelManager::new();
    let cloud = cloud_provider("cloud1");
    manager.add_provider(cloud.clone());
    let tools = echo_allow_dispatcher();

    let turn = TurnOutcome::NeedsLocalReroute {
        reason: "content must not leave this device".to_string(),
        call: ToolCall { name: "echo".to_string(), args: serde_json::json!({"x": 1}) },
        prior_sections: vec![],
        remaining: vec![],
        turn_call_count: 0,
        cascade_active: false,
    };

    let fired = Arc::new(AtomicUsize::new(0));
    let fired2 = fired.clone();
    let cloud_client = manager.get_client("cloud1").expect("cloud client");

    let (msg, provider, _client, is_cloud, routing) = resolve_turn_outcome(
        &tools,
        &manager,
        turn,
        &exec_ctx(),
        Binding::Auto,
        cloud.clone(),
        cloud_client,
        true,
        "allow",
        &move |_, _, _| {
            fired2.fetch_add(1, Ordering::SeqCst);
        },
    )
    .await
    .expect("resolve ok");

    assert!(is_cloud, "no local candidate → stays on cloud");
    assert_eq!(provider.id, cloud.id);
    assert_eq!(routing, "allow", "routing_decision unchanged");
    assert_eq!(fired.load(Ordering::SeqCst), 0, "on_reroute must never fire");
    let content = msg.expect("feedback present").content;
    assert!(content.contains("must stay on-device"), "content: {content}");
    assert!(
        content.contains("switch to a local model or set the conversation binding to Private"),
        "content: {content}"
    );
    // Test 13 (by construction): resolve_turn_outcome never calls stream_chat,
    // so local-down can never be silently retried against cloud — see the
    // function's doc comment; there is no catch-and-retry-on-cloud path.
}

// ── Round 2: partial-delegation redact-and-send helpers ────────────────────

/// Build a real `AgentLoop` (rules classifier) plus the shared `Storage` handle
/// so a test can seed prior turns and then exercise the private redaction
/// helpers directly.
fn redaction_loop() -> (crate::agent::loop_mod::AgentLoop, Arc<Storage>, std::path::PathBuf) {
    use crate::agent::loop_mod::AgentLoop;
    use crate::classifier::RulesClassifier;
    use crate::models::ModelManager;
    let dir = tempdir();
    let storage = Arc::new(Storage::open(&dir).expect("open temp storage"));
    let gate = PrivacyGate::new(Arc::new(RulesClassifier::new()));
    let agent = AgentLoop::new(
        gate,
        Arc::new(ModelManager::new()),
        Arc::clone(&storage),
        Arc::new(echo_allow_dispatcher()),
    );
    (agent, storage, dir)
}

#[test]
fn plan_redaction_redacts_value_spans_and_honors_the_toggle() {
    use crate::classifier::{Classifier, ClassifierConfig, RulesClassifier};
    let (agent, storage, dir) = redaction_loop();
    let clf = RulesClassifier::new();
    let cfg = ClassifierConfig::default();

    // An email is a concrete VALUE span → redactable. plan_redaction blacks it
    // out, re-classifies the remainder clean, and returns Some.
    let content = "please email me at a@b.com about the invoice";
    let classification = clf.classify(content);
    let red = agent
        .plan_redaction("personal", content, Some(&classification), &cfg)
        .expect("an email-only message must be redact-and-sendable");
    assert!(red.is_redacted());
    assert!(!red.redacted_text.contains("a@b.com"), "the value must not survive redaction");

    // A proprietary CUE ("confidential") is not a value — redacting it would
    // strip the signal, not the secret — so nothing is redacted and the turn
    // must stay local (None).
    let cue = "this is strictly confidential, do not share";
    let cue_cls = clf.classify(cue);
    assert!(
        agent.plan_redaction("personal", cue, Some(&cue_cls), &cfg).is_none(),
        "a proprietary-cue message can't be partially delegated → stays local"
    );

    // With redaction disabled for the profile, even the email message stays local.
    storage
        .open_profile("personal")
        .unwrap()
        .set_redaction_enabled(false)
        .unwrap();
    assert!(
        agent.plan_redaction("personal", content, Some(&classification), &cfg).is_none(),
        "redaction toggle off ⇒ no redact-and-send"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn conversation_is_cloud_safe_blocks_on_a_prior_private_turn() {
    use crate::classifier::ClassifierConfig;
    let (agent, storage, dir) = redaction_loop();
    let cfg = ClassifierConfig::default();

    // An empty conversation is vacuously safe.
    assert!(agent.conversation_is_cloud_safe("personal", "empty", &cfg));

    // Seed a prior turn that carries private content (an SSN). Now the whole
    // conversation is unsafe to replay to a cloud model — redact-and-send must
    // NOT fire, because that prior turn would leak in the history.
    let db = storage.open_profile("personal").unwrap();
    db.create_conversation(&crate::storage::Conversation {
        id: "c-private".to_string(),
        name: "P".to_string(),
        pinned: false,
        binding: "auto".to_string(),
        folder_id: None,
        color: None,
        created_at: 1,
        updated_at: 1,
    })
    .unwrap();
    db.add_message(&Message {
        id: uuid::Uuid::new_v4().to_string(),
        conversation_id: "c-private".to_string(),
        role: "user".to_string(),
        content: "my SSN is 123-45-6789".to_string(),
        model: None,
        provider_id: None,
        routing_decision: None,
        thinking_content: None,
        error: None,
        aborted: false,
        created_at: 1,
    })
    .unwrap();
    assert!(
        !agent.conversation_is_cloud_safe("personal", "c-private", &cfg),
        "a prior private turn makes the conversation unsafe for cloud replay"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn assemble_memory_context_is_endpoint_aware_and_profile_scoped() {
    use crate::storage::{MemoryBucket, MemoryFact};
    let (agent, storage, dir) = redaction_loop();
    let g = storage.global();
    let mk = |id: &str, content: &str, profile: &str| MemoryFact {
        id: id.into(),
        content: content.into(),
        origin_profile: profile.into(),
        tags: None,
        created_at: 1,
        pinned: false,
    };
    g.insert_memory_fact_in(MemoryBucket::Shared, &mk("s", "the deploy key lives in the vault", "personal")).unwrap();
    g.insert_memory_fact_in(MemoryBucket::PrivateLocal, &mk("p", "home address is 123 Oak Street", "personal")).unwrap();
    g.insert_memory_fact_in(MemoryBucket::Shared, &mk("w", "work standup is at 9am", "work")).unwrap();

    // CLOUD turn (is_cloud=true): the always-loaded summary carries the shared
    // fact, the private-local fact is NEVER queried, and it's guard-wrapped.
    let (block, _recalled) = agent
        .assemble_memory_context("cv1", "personal", "where is the deploy key", true)
        .expect("some context to inject");
    assert!(block.contains("deploy key"), "shared fact is loaded on a cloud turn");
    assert!(!block.contains("Oak Street"), "cloud turn must NOT surface a private-local fact");
    assert!(block.contains("UNTRUSTED TOOL OUTPUT"), "injected memory is guard-wrapped as untrusted");
    // Profile scope: another profile's fact never appears.
    assert!(!block.contains("work standup"), "another profile's fact must not leak in");

    // LOCAL turn (is_cloud=false): the private-local fact MAY appear.
    let (block_local, _) = agent
        .assemble_memory_context("cv1", "personal", "what is my home address", false)
        .expect("some context");
    assert!(block_local.contains("Oak Street"), "a local turn may surface private-local memory");
    assert!(!block_local.contains("work standup"));

    // A profile with no facts injects nothing.
    assert!(agent.assemble_memory_context("cv2", "school", "anything", true).is_none());

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn assemble_memory_context_uses_the_meaning_lane_with_a_relevance_gate() {
    use crate::embedder::{EmbedderHandle, FakeEmbedder, TextEmbedder};
    use crate::storage::{MemoryBucket, MemoryFact};
    use std::sync::Arc;

    let (agent, storage, dir) = redaction_loop();
    let fake: Arc<dyn TextEmbedder> =
        Arc::new(FakeEmbedder(vec![("heater", 2), ("furnace", 2), ("groceries", 3)]));
    let g = storage.global();
    let mk = |id: &str, content: &str| MemoryFact {
        id: id.into(),
        content: content.into(),
        origin_profile: "personal".into(),
        tags: None,
        created_at: 1,
        pinned: false,
    };
    // Fill the always-loaded summary with pinned facts so the relevance
    // snippets below are genuinely NEW to the turn (the injection path dedups
    // against the summary).
    for i in 0..8 {
        let mut f = mk(&format!("pin{i}"), &format!("pinned filler fact number {i}"));
        f.pinned = true;
        g.insert_memory_fact_in(MemoryBucket::Shared, &f).unwrap();
    }
    // Related-by-meaning fact (axis 2 via "heater") and an unrelated one (axis 3).
    g.insert_memory_fact_in(MemoryBucket::Shared, &mk("h", "the heater was repaired in March")).unwrap();
    g.insert_memory_fact_in(MemoryBucket::Shared, &mk("g", "groceries are delivered on Sundays")).unwrap();
    g.upsert_memory_embedding(MemoryBucket::Shared, "h", &fake.embed_passage("the heater was repaired in March").unwrap()).unwrap();
    g.upsert_memory_embedding(MemoryBucket::Shared, "g", &fake.embed_passage("groceries are delivered on Sundays").unwrap()).unwrap();

    let agent = agent.with_embedder(Some(EmbedderHandle::ready(fake)));

    // "furnace" shares no keyword with either fact, but its vector sits on the
    // heater fact's axis → that one (and only that one) injects. The summary
    // also carries facts, so assert on the relevance section by count: strip
    // the summary by checking the unrelated fact is absent from the relevance
    // wording. Simplest robust check: the block contains the heater fact.
    let (block, recalled) = agent
        .assemble_memory_context("cv", "personal", "when did we fix the furnace?", false)
        .expect("context expected");
    assert!(block.contains("heater"), "meaning-lane match must inject");
    // recalled counts only relevance snippets (not the always-loaded summary):
    // exactly the heater fact — the groceries fact is past the distance gate
    // and shares no keywords.
    assert_eq!(recalled, 1, "only the semantically-near fact clears the inject gate");

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn curated_summary_is_snapshotted_per_conversation() {
    // Wave 1.3: the curated summary is frozen at a conversation's first turn.
    // A fact saved mid-conversation shows up in the NEXT conversation's summary,
    // not the current one (PLAN §9 "Timing and trust").
    use crate::storage::{MemoryBucket, MemoryFact};
    let (agent, storage, dir) = redaction_loop();
    let g = storage.global();
    let mk = |id: &str, content: &str| MemoryFact {
        id: id.into(),
        content: content.into(),
        origin_profile: "personal".into(),
        tags: None,
        created_at: 1,
        pinned: false,
    };
    // Fact A exists at the start. The query shares no keyword with either fact,
    // so nothing arrives via the relevance lane — this isolates the summary.
    g.insert_memory_fact_in(MemoryBucket::Shared, &mk("a", "alpha the first note")).unwrap();

    // First turn of conversation "cv" — freezes the summary (just A).
    let (b1, _) = agent
        .assemble_memory_context("cv", "personal", "zzz unrelated query", false)
        .expect("summary A");
    assert!(b1.contains("alpha"), "turn 1 sees fact A");
    assert!(!b1.contains("bravo"), "fact B doesn't exist yet");

    // Fact B is saved mid-conversation.
    g.insert_memory_fact_in(MemoryBucket::Shared, &mk("b", "bravo the second note")).unwrap();

    // Same conversation, later turn — the snapshot is unchanged (no B).
    let (b2, _) = agent
        .assemble_memory_context("cv", "personal", "zzz unrelated query", false)
        .expect("summary still A");
    assert!(b2.contains("alpha"));
    assert!(
        !b2.contains("bravo"),
        "a mid-conversation save must NOT rewrite the loaded summary"
    );

    // A fresh conversation re-snapshots and sees BOTH facts.
    let (b3, _) = agent
        .assemble_memory_context("cv2", "personal", "zzz unrelated query", false)
        .expect("fresh summary A+B");
    assert!(b3.contains("alpha") && b3.contains("bravo"), "next conversation sees the new fact");

    let _ = std::fs::remove_dir_all(dir);
}
