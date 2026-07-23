//! `WorkQueueRunner` — Wave 4.3c/4.4: the background loop that actually
//! EXECUTES what `delegate` (and, later, other `work_items` producers) only
//! enqueues.
//!
//! This is the other half of the circular-dependency break described in
//! `tools::delegate`'s module docs: `delegate` cannot hold an `Arc<AgentLoop>`
//! (that would make `AgentLoop → ToolDispatcher → delegate → AgentLoop`), so
//! it only writes a `work_items` row. This module holds the `Arc<AgentLoop>`
//! and drains that queue instead — polling every profile, claiming due items
//! atomically (`ProfileDb::claim_next_due_work`, which flips `queued →
//! running` in one statement so two runners — or two ticks of this same
//! runner — can never claim the same row twice), and running each claimed
//! item to completion.
//!
//! Today the only claimable `WorkKind` a persona can produce is
//! `AgentDispatch` (the `delegate` tool). A `Cron` item can already be
//! enqueued via the `manage_cron` tool's CRUD surface, but nothing produces
//! one yet — if this runner ever claims a non-`AgentDispatch` item, it fails
//! it loudly (`WorkState::Failed`, "no runner for this work kind yet") rather
//! than leaving it stuck `running` forever or silently dropping it.

use std::sync::Arc;

use crate::agent::gate::Binding;
use crate::agent::loop_mod::AgentLoop;
use crate::queue::{WorkItem, WorkKind, WorkState};
use crate::storage::{Message, ProfileDb, Storage};

/// Max helper sub-agents allowed to run concurrently, across every profile.
/// A shared bound (not per-profile) so a burst of `delegate` calls can't spin
/// up unbounded concurrent model calls regardless of how they're spread
/// across profiles.
const MAX_CONCURRENT_HELPERS: usize = 4;

/// How long the poll loop sleeps between fully-drained ticks. Each tick
/// drains every profile's DUE queue completely before sleeping, so this only
/// bounds how often an empty (or momentarily-idle) queue is re-checked — it
/// is not a per-item latency.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Wall-clock ceiling for one helper run. A stalled model stream fails the item
/// (and frees its concurrency permit) rather than hanging forever.
const HELPER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(300);

/// Spawn the background work-queue runner. Fire-and-forget: runs for the
/// life of the process on the Tauri async runtime. Call once at boot, after
/// the `Arc<AgentLoop>` exists (`lib.rs`'s `setup` closure).
pub fn spawn_work_runner(agent_loop: Arc<AgentLoop>, storage: Arc<Storage>) {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_HELPERS));
    tauri::async_runtime::spawn(async move {
        loop {
            let profiles = storage.list_profile_names().unwrap_or_default();
            for profile in profiles {
                let Ok(db) = storage.open_profile(&profile) else {
                    continue;
                };
                let now = chrono::Utc::now().timestamp();
                // Wave 4.4: enqueue any cron jobs that are due this minute as
                // Cron work_items BEFORE draining, so they flow through the same
                // one queue as agent dispatch (exactly-once per minute via a
                // `cron:<id>@<minute>` claim_key + the last_run guard).
                schedule_due_cron_jobs(&db, now);
                // Drain every DUE item in this profile before moving to the
                // next — `claim_next_due_work` is atomic (UPDATE ... RETURNING
                // under the row lock), so this can never double-claim.
                while let Ok(Some(item)) = db.claim_next_due_work(now) {
                    let agent_loop = Arc::clone(&agent_loop);
                    let db_for_task = Arc::clone(&db);
                    let profile_for_task = profile.clone();
                    // Acquire the permit HERE (before spawning), so the drain
                    // loop itself backpressures once `MAX_CONCURRENT_HELPERS`
                    // helpers are already running — bounding concurrency is
                    // then a property of when the task starts, not just how
                    // many are spawned.
                    let Ok(permit) = Arc::clone(&semaphore).acquire_owned().await else {
                        // The semaphore is never `close()`d — unreachable in
                        // practice, but fail closed (stop draining this tick)
                        // rather than run unbounded.
                        break;
                    };
                    tauri::async_runtime::spawn(async move {
                        let _permit = permit; // held for the whole run
                        supervise_one_item(agent_loop, db_for_task, profile_for_task, item).await;
                    });
                }
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}

/// Supervise [`run_one_item`] (Wave 4.3c review fix): run it in an inner task so
/// a PANIC inside `run_subagent`/`process_message` still terminalizes the work
/// item (`Failed` / "helper panicked") instead of leaving it `running` until the
/// next boot's crash reconcile. Extracted from the drain loop so the panic path
/// is directly testable (B6).
async fn supervise_one_item(
    agent_loop: Arc<AgentLoop>,
    db: Arc<ProfileDb>,
    profile: String,
    item: WorkItem,
) {
    let item_id = item.id.clone();
    let db_supervise = Arc::clone(&db);
    let inner = tauri::async_runtime::spawn(run_one_item(agent_loop, db, profile, item));
    if inner.await.is_err() {
        let now = chrono::Utc::now().timestamp();
        let _ = db_supervise.finish_work_item(
            &item_id,
            WorkState::Failed,
            None,
            Some("helper panicked"),
            now,
        );
    }
}

/// Run one claimed item to completion (or failure) and finish it. Never
/// panics on a malformed payload or a failed helper run — every path ends in
/// a `finish_work_item` call, so a bad item can never sit `running` forever.
async fn run_one_item(agent_loop: Arc<AgentLoop>, db: Arc<ProfileDb>, profile: String, item: WorkItem) {
    match item.kind {
        WorkKind::AgentDispatch => {}
        WorkKind::Cron => {
            run_cron_item(&agent_loop, &db, &profile, &item).await;
            return;
        }
        WorkKind::ServerResult => {
            let now = chrono::Utc::now().timestamp();
            let _ = db.finish_work_item(
                &item.id,
                WorkState::Failed,
                None,
                Some("no runner for this work kind yet"),
                now,
            );
            return;
        }
    }

    let payload: serde_json::Value = match serde_json::from_str(&item.input_json) {
        Ok(v) => v,
        Err(e) => {
            let now = chrono::Utc::now().timestamp();
            let _ = db.finish_work_item(
                &item.id,
                WorkState::Failed,
                None,
                Some(&format!("malformed agent_dispatch payload: {e}")),
                now,
            );
            return;
        }
    };

    let agent_name = payload
        .get("agent_name")
        .and_then(|v| v.as_str())
        .unwrap_or("helper")
        .to_string();
    let system_prompt = payload
        .get("system_prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let tools_allowlist: Vec<String> = payload
        .get("tools_allowlist")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let provider_id = payload.get("provider").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let model = payload.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let task = payload.get("task").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let profile = payload.get("profile").and_then(|v| v.as_str()).unwrap_or("personal").to_string();
    let binding = parse_binding_lenient(payload.get("binding").and_then(|v| v.as_str()).unwrap_or("auto"));

    // Wave 4.3c review fix: a wall-clock deadline so a stalled model stream
    // can't hang a helper forever (and, at MAX_CONCURRENT_HELPERS stalls,
    // permanently starve every future helper of a semaphore permit).
    let run =
        agent_loop.run_subagent(&system_prompt, &tools_allowlist, &provider_id, &model, &profile, binding, &task);
    let outcome = tokio::time::timeout(HELPER_DEADLINE, run).await;
    let finished_at = chrono::Utc::now().timestamp();

    // Resolve the run into (terminal state, result payload, error, and the
    // human-facing note posted into the parent). Success, failure, AND timeout
    // all post SOMETHING back — the user must never see "dispatched" then
    // silence (Wave 4.3c review fix).
    let (state, result_json, error, note): (WorkState, Option<String>, Option<String>, String) =
        match outcome {
            Ok(Ok(text)) => (
                WorkState::Done,
                Some(text.clone()),
                None,
                format!("**[helper: {agent_name}]**\n\n{text}"),
            ),
            Ok(Err(e)) => {
                let e = e.to_string();
                (
                    WorkState::Failed,
                    None,
                    Some(e.clone()),
                    format!("**[helper: {agent_name}] failed:** {e}"),
                )
            }
            Err(_elapsed) => (
                WorkState::Failed,
                None,
                Some("helper timed out".to_string()),
                format!("**[helper: {agent_name}] timed out** and was stopped."),
            ),
        };

    // Lukas decision #2: the outcome lands in the PARENT conversation as a
    // labeled message. `routing_decision = "delegated"` marks it so the main
    // agent's history assembly guard-wraps it (it's model-generated content
    // re-entering a model — see loop_mod's history loop).
    if let Some(target_conversation_id) = item.target_conversation_id.clone() {
        let msg = Message {
            id: uuid::Uuid::new_v4().to_string(),
            conversation_id: target_conversation_id,
            role: "assistant".to_string(),
            content: note,
            model: Some(model.clone()),
            provider_id: Some(provider_id.clone()),
            routing_decision: Some("delegated".to_string()),
            thinking_content: None,
            error: None,
            aborted: false,
            created_at: finished_at,
        };
        if let Err(e) = db.add_message(&msg) {
            tracing::warn!(
                target: "lhp::work_runner",
                error = %e,
                work_item = %item.id,
                "failed to post the helper's outcome into the parent conversation"
            );
        }
    }
    let _ = db.finish_work_item(&item.id, state, result_json.as_deref(), error.as_deref(), finished_at);
}

/// Enqueue every enabled cron job that is DUE this minute as a `Cron`
/// work_item, so scheduled work flows through the same one queue as agent
/// dispatch. Exactly-once per minute: a `cron:<id>@<minute>` `claim_key` (the
/// partial-unique index rejects a duplicate) PLUS the `last_run_at` guard (the
/// poll runs every few seconds, but a job fires only once per matching minute).
fn schedule_due_cron_jobs(db: &ProfileDb, now: i64) {
    schedule_due_cron_jobs_at(db, now, chrono::Local::now())
}

/// Testable core of [`schedule_due_cron_jobs`] with the wall-clock injected.
fn schedule_due_cron_jobs_at(db: &ProfileDb, now: i64, local_now: chrono::DateTime<chrono::Local>) {
    let minute_start = now - now.rem_euclid(60);
    let jobs = match db.list_cron_jobs() {
        Ok(j) => j,
        Err(_) => return,
    };
    for job in jobs {
        if !job.enabled {
            continue;
        }
        if job.last_run_at.is_some_and(|t| t >= minute_start) {
            continue; // already fired this minute
        }
        if !crate::tools::cron::cron_due(&job.schedule, local_now) {
            continue;
        }
        let payload = serde_json::json!({ "prompt": job.prompt, "job_id": job.id }).to_string();
        let mut wi = WorkItem::queued(WorkKind::Cron, payload, now);
        wi.target_conversation_id = job.target_conversation_id.clone();
        wi.source_ref = Some(job.id.clone());
        wi.claim_key = Some(format!("cron:{}@{}", job.id, minute_start));
        if db.insert_work_item(&wi).unwrap_or(false) {
            // Mark it run THIS minute so the next few-second poll won't re-enqueue.
            let _ = db.record_cron_run(&job.id, "queued");
        }
    }
}

/// Execute one Cron work_item: run its prompt as an unattended, headless,
/// local-only turn (`AgentLoop::run_cron`), delivering into the job's target
/// conversation. Every path finishes the item; a bounded deadline mirrors the
/// helper path.
async fn run_cron_item(agent_loop: &AgentLoop, db: &ProfileDb, profile: &str, item: &WorkItem) {
    let payload: serde_json::Value =
        serde_json::from_str(&item.input_json).unwrap_or(serde_json::Value::Null);
    let prompt = payload.get("prompt").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let job_id = payload.get("job_id").and_then(|v| v.as_str()).map(str::to_string);
    let target = item.target_conversation_id.clone();
    let now = chrono::Utc::now().timestamp();

    if prompt.trim().is_empty() {
        let _ = db.finish_work_item(&item.id, WorkState::Failed, None, Some("cron: empty prompt"), now);
        if let Some(id) = &job_id {
            let _ = db.record_cron_run(id, "failed");
        }
        return;
    }

    let outcome =
        tokio::time::timeout(HELPER_DEADLINE, agent_loop.run_cron(&prompt, profile, target)).await;
    let finished_at = chrono::Utc::now().timestamp();
    let (state, result, error, status) = match outcome {
        Ok(Ok(text)) => (WorkState::Done, Some(text), None, "ok"),
        Ok(Err(e)) => (WorkState::Failed, None, Some(e.to_string()), "failed"),
        Err(_) => (WorkState::Failed, None, Some("cron: timed out".to_string()), "timed_out"),
    };
    let _ = db.finish_work_item(&item.id, state, result.as_deref(), error.as_deref(), finished_at);
    if let Some(id) = &job_id {
        let _ = db.record_cron_run(id, status);
    }
}

/// Parse the payload's `binding` string, defaulting to `Auto` for anything
/// unrecognized (including a missing/empty field) — never fails the run over
/// this, since a helper defaulting to the classifier's own per-message call
/// is always a safe choice.
fn parse_binding_lenient(s: &str) -> Binding {
    match s.to_ascii_lowercase().as_str() {
        "public" => Binding::Public,
        "private" => Binding::Private,
        _ => Binding::Auto,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::CronJob;
    use chrono::TimeZone;

    fn temp_storage() -> (Storage, std::path::PathBuf) {
        let mut root = std::env::temp_dir();
        root.push(format!("lhp-cronrun-{}", uuid::Uuid::new_v4()));
        (Storage::open(&root).unwrap(), root)
    }

    fn job(id: &str, schedule: &str, enabled: bool) -> CronJob {
        CronJob {
            id: id.into(),
            name: format!("job {id}"),
            prompt: "do the thing".into(),
            schedule: schedule.into(),
            enabled,
            last_run_at: None,
            last_status: None,
            target_conversation_id: None,
        }
    }

    #[test]
    fn schedule_enqueues_a_due_enabled_job_once_per_minute() {
        let (storage, root) = temp_storage();
        let db = storage.open_profile("personal").unwrap();
        db.insert_cron_job(&job("due", "30 9 * * *", true)).unwrap();
        db.insert_cron_job(&job("not-due", "0 0 * * *", true)).unwrap();
        db.insert_cron_job(&job("disabled", "30 9 * * *", false)).unwrap();

        // 2026-07-15 09:30 local → only "due" matches.
        let local = chrono::Local.with_ymd_and_hms(2026, 7, 15, 9, 30, 0).unwrap();
        let now = local.timestamp();
        schedule_due_cron_jobs_at(&db, now, local);

        // Exactly one Cron work_item enqueued, for the due job.
        let claimed = db.claim_next_due_work(now + 1).unwrap().expect("a cron item");
        assert_eq!(claimed.kind, WorkKind::Cron);
        assert_eq!(claimed.source_ref.as_deref(), Some("due"));
        let payload: serde_json::Value = serde_json::from_str(&claimed.input_json).unwrap();
        assert_eq!(payload["job_id"], "due");
        assert!(db.claim_next_due_work(now + 1).unwrap().is_none(), "only the due+enabled job enqueues");

        // Re-running the SAME minute does not enqueue again (last_run guard +
        // the cron:<id>@<minute> claim_key).
        schedule_due_cron_jobs_at(&db, now, local);
        assert!(db.claim_next_due_work(now + 1).unwrap().is_none(), "no double-fire within the minute");
        let _ = std::fs::remove_dir_all(root);
    }

    // ── B6: the untested work_runner safety-nets — the wall-clock deadline and
    // the panic supervisor (Wave 4.3c review fixes). ─────────────────────────

    use crate::agent::gate::PrivacyGate;
    use crate::agent::loop_mod::{AgentLoop, ModelStreamer};
    use crate::classifier::RulesClassifier;
    use crate::models::sse::SseStream;
    use crate::models::{ChatMessage, ModelManager, Provider, ProviderKind};
    use std::pin::Pin;

    /// A streamer whose stream() never resolves — makes run_subagent hang so the
    /// HELPER_DEADLINE timeout fires.
    struct StallStreamer(Provider);
    impl ModelStreamer for StallStreamer {
        fn provider(&self) -> &Provider {
            &self.0
        }
        fn stream<'a>(
            &'a self,
            _m: &'a str,
            _msgs: Vec<ChatMessage>,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<SseStream>> + Send + 'a>> {
            Box::pin(std::future::pending())
        }
    }

    /// A streamer that PANICS when polled — simulates an unexpected bug inside
    /// the helper run, so the panic supervisor must terminalize the item.
    struct PanicStreamer(Provider);
    impl ModelStreamer for PanicStreamer {
        fn provider(&self) -> &Provider {
            &self.0
        }
        fn stream<'a>(
            &'a self,
            _m: &'a str,
            _msgs: Vec<ChatMessage>,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<SseStream>> + Send + 'a>> {
            Box::pin(async { panic!("boom — a bug inside the helper run") })
        }
    }

    fn cloud() -> Provider {
        Provider::new("cloudco", "Cloud", "https://api.cloudco.example/v1", None, ProviderKind::Cloud)
    }

    fn agent_with(streamer: Arc<dyn ModelStreamer>, storage: Arc<Storage>) -> Arc<AgentLoop> {
        let mm = Arc::new(ModelManager::new());
        mm.add_provider(cloud());
        let gate = PrivacyGate::new(Arc::new(RulesClassifier::new()));
        Arc::new(
            AgentLoop::new(gate, mm, storage, Arc::new(crate::tools::ToolDispatcher::empty()))
                .with_model_streamer_override(streamer),
        )
    }

    fn dispatch_item(now: i64) -> WorkItem {
        let payload = serde_json::json!({
            "agent_name": "helper",
            "system_prompt": "you are a helper",
            "tools_allowlist": [],
            "provider": "cloudco",
            "model": "gpt-x",
            "task": "do the thing",
            "profile": "personal",
            "binding": "public"  // bypass the classifier → straight to the (fake) cloud stream
        })
        .to_string();
        WorkItem::queued(WorkKind::AgentDispatch, payload, now)
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_times_out_a_stalled_helper_and_fails_the_item() {
        let (storage, root) = temp_storage();
        let storage = Arc::new(storage);
        let db = storage.open_profile("personal").unwrap();
        let agent = agent_with(Arc::new(StallStreamer(cloud())), Arc::clone(&storage));

        let now = 1_000_000;
        let item = dispatch_item(now);
        let id = item.id.clone();
        db.insert_work_item(&item).unwrap();
        let claimed = db.claim_next_due_work(now).unwrap().expect("claimed");

        // run_one_item stalls at the fake stream; advance the virtual clock past
        // HELPER_DEADLINE (300s) and the timeout must finalize the item. Use
        // `tokio::spawn` (NOT tauri's spawn) so the task runs on THIS test's
        // paused runtime — otherwise the timeout would burn 300s of real time.
        let db2 = Arc::clone(&db);
        let handle = tokio::spawn(run_one_item(agent, db2, "personal".into(), claimed));
        tokio::task::yield_now().await; // let the spawned task reach its stalled await
        tokio::time::advance(HELPER_DEADLINE + std::time::Duration::from_secs(1)).await;
        handle.await.unwrap();

        let done = db.get_work_item(&id).unwrap().expect("item exists");
        assert_eq!(done.state, WorkState::Failed, "a stalled helper must be Failed, not stuck running");
        assert!(
            done.error.as_deref().unwrap_or("").to_lowercase().contains("timed out")
                || done.error.as_deref().unwrap_or("").to_lowercase().contains("timeout"),
            "the failure must name the timeout, got: {:?}",
            done.error
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn supervise_finalizes_a_panicked_helper_instead_of_leaving_it_running() {
        let (storage, root) = temp_storage();
        let storage = Arc::new(storage);
        let db = storage.open_profile("personal").unwrap();
        let agent = agent_with(Arc::new(PanicStreamer(cloud())), Arc::clone(&storage));

        let now = 2_000_000;
        let item = dispatch_item(now);
        let id = item.id.clone();
        db.insert_work_item(&item).unwrap();
        let claimed = db.claim_next_due_work(now).unwrap().expect("claimed");

        // supervise_one_item runs run_one_item in an inner task; the panic there
        // must be caught and the item finalized Failed (never left running).
        supervise_one_item(agent, Arc::clone(&db), "personal".into(), claimed).await;

        let done = db.get_work_item(&id).unwrap().expect("item exists");
        assert_eq!(done.state, WorkState::Failed, "a panicked helper must be Failed, not stuck running");
        assert!(
            done.error.as_deref().unwrap_or("").contains("panicked"),
            "the failure must name the panic, got: {:?}",
            done.error
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
