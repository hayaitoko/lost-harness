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
                // Drain every DUE item in this profile before moving to the
                // next — `claim_next_due_work` is atomic (UPDATE ... RETURNING
                // under the row lock), so this can never double-claim.
                while let Ok(Some(item)) = db.claim_next_due_work(now) {
                    let agent_loop = Arc::clone(&agent_loop);
                    let db_for_task = Arc::clone(&db);
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
                        // Wave 4.3c review fix: supervise the run so a PANIC
                        // inside run_subagent/process_message still terminalizes
                        // the work item — otherwise it would sit `running` until
                        // the next boot's crash reconcile. Run in an inner task
                        // and, if its JoinHandle reports a panic, fail the item.
                        let item_id = item.id.clone();
                        let db_supervise = Arc::clone(&db_for_task);
                        let inner = tauri::async_runtime::spawn(run_one_item(agent_loop, db_for_task, item));
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
                    });
                }
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}

/// Run one claimed item to completion (or failure) and finish it. Never
/// panics on a malformed payload or a failed helper run — every path ends in
/// a `finish_work_item` call, so a bad item can never sit `running` forever.
async fn run_one_item(agent_loop: Arc<AgentLoop>, db: Arc<ProfileDb>, item: WorkItem) {
    if item.kind != WorkKind::AgentDispatch {
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
