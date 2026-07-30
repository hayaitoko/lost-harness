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

use std::collections::HashMap;
use std::sync::Arc;

use crate::agent::gate::Binding;
use crate::agent::loop_mod::AgentLoop;
use crate::hooks::budget::{self, BudgetVerdict};
use crate::queue::{WorkItem, WorkKind, WorkState};
use crate::storage::{Message, ProfileDb, Storage, UsageSummary};

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

/// Shared state for transactional budget reservations across concurrent helpers.
/// Keyed by profile name, value is USD reserved but not yet spent.
/// The Mutex ensures atomic check-and-reserve (M-09), so concurrent helpers
/// cannot collectively exceed the configured cap.
type BudgetLock = Arc<tokio::sync::Mutex<HashMap<String, f64>>>;

/// Lock, check the budget against both on-disk spend and in-memory reservations,
/// and — if the profile is capped — reserve the full remaining budget before
/// dispatch. The caller MUST reconcile after the run.
///
/// Returns `true` when the item should proceed; `false` when it was already
/// `finish_work_item`'d as `Failed` with a budget reason (caller MUST `return`).
///
/// **What this is, precisely** (M-09, half 1): an ATOMIC pre-dispatch
/// reservation. The reservation is visible to every concurrent helper, so a
/// second helper reading the same ledger sees the first's
/// committed-but-unbooked headroom and halts. That closes the concurrent
/// check-then-spend race — several helpers can no longer each observe the same
/// under-cap ledger and collectively blow past the cap.
///
/// **What it is NOT**: a bound on what a SINGLE helper spends. Nothing here
/// re-reads the ledger once the helper is running. The other half of the
/// ceiling is the per-round unattended re-check in
/// `AgentLoop::process_message_inner`, which halts a running unattended loop as
/// soon as its BOOKED spend reaches the cap. Together the residual overrun is
/// one round's cost (the round that crosses the cap is already paid for before
/// it can be observed), not a whole `HELPER_DEADLINE` window — bounded, not
/// zero. A zero-overrun cap needs pre-call cost reservation, which the provider
/// APIs don't offer.
async fn budget_check_and_reserve(
    budget_lock: &BudgetLock,
    db: &ProfileDb,
    profile: &str,
    item: &WorkItem,
) -> bool {
    let mut reservations = budget_lock.lock().await;
    let since = budget::month_start_ts(chrono::Utc::now());
    match (db.budget_cap(), db.usage_summary_since(since)) {
        (Ok(cap), Ok(sum)) => {
            let reserved = reservations.get(profile).copied().unwrap_or(0.0);
            let effective = UsageSummary {
                total_calls: sum.total_calls,
                known_cost_usd: sum.known_cost_usd + reserved,
                unknown_cost_calls: sum.unknown_cost_calls,
            };
            if let BudgetVerdict::Halt(reason) = budget::evaluate(cap, &effective, false) {
                let now = chrono::Utc::now().timestamp();
                let _ = db.finish_work_item(
                    &item.id,
                    WorkState::Failed,
                    None,
                    Some(&format!("budget: {reason}")),
                    now,
                );
                return false;
            }
            // Reserve the remaining budget so concurrent helpers see a reduced cap.
            if let Some(cap_val) = cap {
                let remaining = (cap_val - effective.known_cost_usd).max(0.0);
                reservations.insert(profile.to_string(), reserved + remaining);
            }
            true
        }
        _ => {
            let now = chrono::Utc::now().timestamp();
            let _ = db.finish_work_item(
                &item.id,
                WorkState::Failed,
                None,
                Some("budget: budget check unavailable — halting to fail closed"),
                now,
            );
            false
        }
    }
}

/// Release the per-profile budget reservation (reconcile).
/// Called after the helper run completes (success, error, or timeout) so the
/// next budget check sees actual on-disk spend rather than the reservation.
async fn budget_reconcile(budget_lock: &BudgetLock, profile: &str) {
    let mut reservations = budget_lock.lock().await;
    reservations.remove(profile);
}

/// Spawn the background work-queue runner. Fire-and-forget: runs for the
/// life of the process on the Tauri async runtime. Call once at boot, after
/// the `Arc<AgentLoop>` exists (`lib.rs`'s `setup` closure).
pub fn spawn_work_runner(agent_loop: Arc<AgentLoop>, storage: Arc<Storage>) {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_HELPERS));
    let budget_lock: BudgetLock = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
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
                    let budget_for_task = Arc::clone(&budget_lock);
                    tauri::async_runtime::spawn(async move {
                        let _permit = permit; // held for the whole run
                        supervise_one_item(
                            agent_loop,
                            db_for_task,
                            profile_for_task,
                            item,
                            budget_for_task,
                        )
                        .await;
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
    budget_lock: BudgetLock,
) {
    let item_id = item.id.clone();
    let db_supervise = Arc::clone(&db);
    let profile_for_reconcile = profile.clone();
    let budget_for_cleanup = Arc::clone(&budget_lock);
    let inner =
        tauri::async_runtime::spawn(run_one_item(agent_loop, db, profile, item, budget_lock));
    if inner.await.is_err() {
        // run_one_item panicked — release the budget reservation so it
        // doesn't leak and block future helpers for this profile.
        budget_reconcile(&budget_for_cleanup, &profile_for_reconcile).await;
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
async fn run_one_item(
    agent_loop: Arc<AgentLoop>,
    db: Arc<ProfileDb>,
    profile: String,
    item: WorkItem,
    budget_lock: BudgetLock,
) {
    match item.kind {
        WorkKind::AgentDispatch => {}
        WorkKind::Cron => {
            // Parse job_id early for cron failure recording.
            let cron_payload: serde_json::Value =
                serde_json::from_str(&item.input_json).unwrap_or(serde_json::Value::Null);
            let cron_job_id = cron_payload
                .get("job_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            // Budget check + reservation before running the cron turn.
            if !budget_check_and_reserve(&budget_lock, &db, &profile, &item).await {
                if let Some(ref id) = cron_job_id {
                    let _ = db.record_cron_run(id, "failed");
                }
                return;
            }
            run_cron_item(&agent_loop, &db, &profile, &item).await;
            budget_reconcile(&budget_lock, &profile).await;
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
        WorkKind::MutatingAction => {
            // C2: journal rows are dispatcher-driven, never runner-claimable —
            // `claim_next_due_work` excludes them at the SQL level; this arm is
            // the defensive net if one is ever handed here anyway.
            let now = chrono::Utc::now().timestamp();
            let _ = db.finish_work_item(
                &item.id,
                WorkState::Failed,
                None,
                Some("mutating_action journal rows are not runner-claimable"),
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
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let provider_id = payload
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let model = payload
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let task = payload
        .get("task")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // The profile named INSIDE the payload. It must NOT shadow the queue
    // profile (`profile`): the queue profile is what `db` was opened for, what
    // the budget ledger below is read from, and — critically — what
    // `supervise_one_item`'s panic-path `budget_reconcile` releases. Keying the
    // reservation off a differing payload string would leak a PERMANENT
    // reservation on a panic and wedge the queue profile forever. Production
    // always writes the two identically (`delegate` stamps `ctx.profile` into
    // the payload AND inserts into that same profile's DB), so a mismatch means
    // a hand-crafted or corrupted row: log it and keep the reservation keyed to
    // the queue profile.
    let payload_profile = payload
        .get("profile")
        .and_then(|v| v.as_str())
        .unwrap_or("personal")
        .to_string();
    if payload_profile != profile {
        tracing::warn!(
            target: "lhp::work_runner",
            work_item = %item.id,
            queue_profile = %profile,
            payload_profile = %payload_profile,
            "agent_dispatch payload names a different profile than the queue it was \
             claimed from; the budget reservation stays keyed to the queue profile"
        );
    }
    let binding = parse_binding_lenient(
        payload
            .get("binding")
            .and_then(|v| v.as_str())
            .unwrap_or("auto"),
    );

    // C1 / M-09, part 1 of 2 — the PRE-DISPATCH gate. Lock, check, and reserve
    // the remaining budget TRANSACTIONALLY so concurrent helpers see each
    // other's committed-but-unbooked headroom. This bounds how many helpers
    // START; it does NOT bound how much one helper spends once running — part 2
    // is the per-round re-check inside `AgentLoop::process_message_inner`, which
    // halts an unattended run whose booked spend has reached the cap.
    // Keyed on the QUEUE profile so it pairs with the panic-path reconcile.
    if !budget_check_and_reserve(&budget_lock, &db, &profile, &item).await {
        return;
    }

    // Wave 4.3c review fix: a wall-clock deadline so a stalled model stream
    // can't hang a helper forever (and, at MAX_CONCURRENT_HELPERS stalls,
    // permanently starve every future helper of a semaphore permit).
    let run = agent_loop.run_subagent(
        &system_prompt,
        &tools_allowlist,
        &provider_id,
        &model,
        &payload_profile,
        binding,
        &task,
    );
    let outcome = tokio::time::timeout(HELPER_DEADLINE, run).await;
    let finished_at = chrono::Utc::now().timestamp();

    // Reconcile: release the budget reservation. Actual per-round costs have
    // already been booked by process_message → record_usage during the run.
    budget_reconcile(&budget_lock, &profile).await;

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
            // The trust zone the HELPER ran in, read off the endpoint it used
            // while that run is still the present. `None` (→ rendered as
            // UNKNOWN, never as "local") if the provider went away mid-run.
            endpoint_zone: agent_loop
                .model_manager()
                .get_provider(&provider_id)
                .map(|p| p.trust_zone().as_str().to_string()),
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
    let _ = db.finish_work_item(
        &item.id,
        state,
        result_json.as_deref(),
        error.as_deref(),
        finished_at,
    );
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
    let prompt = payload
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let job_id = payload
        .get("job_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let target = item.target_conversation_id.clone();
    let now = chrono::Utc::now().timestamp();

    // Budget check is handled by the caller (run_one_item) via the lock-guarded
    // budget_check_and_reserve + budget_reconcile pair.

    if prompt.trim().is_empty() {
        let _ = db.finish_work_item(
            &item.id,
            WorkState::Failed,
            None,
            Some("cron: empty prompt"),
            now,
        );
        if let Some(id) = &job_id {
            let _ = db.record_cron_run(id, "failed");
        }
        return;
    }

    let outcome = tokio::time::timeout(
        HELPER_DEADLINE,
        agent_loop.run_cron(&prompt, profile, target),
    )
    .await;
    let finished_at = chrono::Utc::now().timestamp();
    let (state, result, error, status) = match outcome {
        Ok(Ok(text)) => (WorkState::Done, Some(text), None, "ok"),
        Ok(Err(e)) => (WorkState::Failed, None, Some(e.to_string()), "failed"),
        Err(_) => (
            WorkState::Failed,
            None,
            Some("cron: timed out".to_string()),
            "timed_out",
        ),
    };
    let _ = db.finish_work_item(
        &item.id,
        state,
        result.as_deref(),
        error.as_deref(),
        finished_at,
    );
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
        db.insert_cron_job(&job("not-due", "0 0 * * *", true))
            .unwrap();
        db.insert_cron_job(&job("disabled", "30 9 * * *", false))
            .unwrap();

        // 2026-07-15 09:30 local → only "due" matches.
        let local = chrono::Local
            .with_ymd_and_hms(2026, 7, 15, 9, 30, 0)
            .unwrap();
        let now = local.timestamp();
        schedule_due_cron_jobs_at(&db, now, local);

        // Exactly one Cron work_item enqueued, for the due job.
        let claimed = db
            .claim_next_due_work(now + 1)
            .unwrap()
            .expect("a cron item");
        assert_eq!(claimed.kind, WorkKind::Cron);
        assert_eq!(claimed.source_ref.as_deref(), Some("due"));
        let payload: serde_json::Value = serde_json::from_str(&claimed.input_json).unwrap();
        assert_eq!(payload["job_id"], "due");
        assert!(
            db.claim_next_due_work(now + 1).unwrap().is_none(),
            "only the due+enabled job enqueues"
        );

        // Re-running the SAME minute does not enqueue again (last_run guard +
        // the cron:<id>@<minute> claim_key).
        schedule_due_cron_jobs_at(&db, now, local);
        assert!(
            db.claim_next_due_work(now + 1).unwrap().is_none(),
            "no double-fire within the minute"
        );
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
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<SseStream>> + Send + 'a>>
        {
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
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<SseStream>> + Send + 'a>>
        {
            Box::pin(async { panic!("boom — a bug inside the helper run") })
        }
    }

    /// A streamer that returns a COMPLETE, canned SSE stream carrying a `usage`
    /// chunk — so the REAL loop prices it and books a REAL cost to the ledger on
    /// every round. 60_000 completion tokens against `gpt-4o` ($10.00/Mtok out,
    /// `models::pricing`) is exactly $0.60 per round.
    ///
    /// This is the difference between a live test and an inert one: `StallStreamer`
    /// never completes a round, so `record_usage` never fires and spend can't move.
    struct BillingStreamer {
        provider: Provider,
        /// When set, `stream()` parks here until the test opens the gate. That is
        /// what makes the concurrency test deterministic: every helper is
        /// guaranteed to have finished its budget check BEFORE any of them books
        /// a cent, so the race M-09 describes is genuinely exercised rather than
        /// accidentally serialized by the scheduler.
        gate: Option<Arc<tokio::sync::Semaphore>>,
        /// Emit a fenced tool call in the reply, so the tool loop takes another
        /// round (and books another $0.60) instead of stopping at one.
        keep_calling_tools: bool,
    }

    impl BillingStreamer {
        fn gated(gate: Arc<tokio::sync::Semaphore>) -> Self {
            Self {
                provider: cloud(),
                gate: Some(gate),
                keep_calling_tools: false,
            }
        }
        fn looping() -> Self {
            Self {
                provider: cloud(),
                gate: None,
                keep_calling_tools: true,
            }
        }
    }

    impl ModelStreamer for BillingStreamer {
        fn provider(&self) -> &Provider {
            &self.provider
        }
        fn stream<'a>(
            &'a self,
            _m: &'a str,
            _msgs: Vec<ChatMessage>,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<SseStream>> + Send + 'a>>
        {
            let gate = self.gate.clone();
            let keep_calling_tools = self.keep_calling_tools;
            Box::pin(async move {
                if let Some(g) = gate {
                    // Park until the test opens the gate. A Semaphore (not a
                    // Notify) so an already-opened gate can't be missed.
                    let _permit = g.acquire().await;
                }
                let content = if keep_calling_tools {
                    "working\n```tool\n{\"name\":\"no_such_tool\",\"arguments\":{}}\n```"
                } else {
                    "done"
                };
                let delta = serde_json::json!({ "choices": [{ "delta": { "content": content } }] })
                    .to_string();
                let usage = serde_json::json!({
                    "choices": [],
                    "usage": { "prompt_tokens": 0, "completion_tokens": 60_000 }
                })
                .to_string();
                let chunks: Vec<Vec<u8>> = vec![
                    format!("data: {delta}\n").into_bytes(),
                    format!("data: {usage}\n").into_bytes(),
                    b"data: [DONE]\n".to_vec(),
                ];
                Ok(SseStream::from_byte_stream(tokio_stream::iter(
                    chunks.into_iter().map(Ok::<Vec<u8>, reqwest::Error>),
                )))
            })
        }
    }

    /// The per-round cost `BillingStreamer` books (see its doc comment).
    const ROUND_COST: f64 = 0.60;

    fn cloud() -> Provider {
        Provider::new(
            "cloudco",
            "Cloud",
            "https://api.cloudco.example/v1",
            None,
            ProviderKind::Cloud,
        )
    }

    fn agent_with(streamer: Arc<dyn ModelStreamer>, storage: Arc<Storage>) -> Arc<AgentLoop> {
        let mm = Arc::new(ModelManager::new());
        mm.add_provider(cloud());
        let gate = PrivacyGate::new(Arc::new(RulesClassifier::new()));
        Arc::new(
            AgentLoop::new(
                gate,
                mm,
                storage,
                Arc::new(crate::tools::ToolDispatcher::empty()),
            )
            .with_model_streamer_override(streamer),
        )
    }

    fn dispatch_item(now: i64) -> WorkItem {
        dispatch_item_model(now, "gpt-x")
    }

    /// `model` is load-bearing for the budget tests: only a model in
    /// `models::pricing::PRICES` yields a KNOWN cost. An unpriced id (`gpt-x`)
    /// books `cost_usd = NULL`, which the governor treats as fail-closed
    /// "unknown" rather than a dollar figure.
    fn dispatch_item_model(now: i64, model: &str) -> WorkItem {
        let payload = serde_json::json!({
            "agent_name": "helper",
            "system_prompt": "you are a helper",
            "tools_allowlist": [],
            "provider": "cloudco",
            "model": model,
            "task": "do the thing",
            "profile": "personal",
            "binding": "public"  // bypass the classifier → straight to the (fake) cloud stream
        })
        .to_string();
        WorkItem::queued(WorkKind::AgentDispatch, payload, now)
    }

    /// Seed the ledger with `spent` USD of KNOWN cloud cost.
    fn seed_spend(db: &ProfileDb, id: &str, spent: f64) {
        db.record_usage(&crate::storage::UsageEvent {
            id: id.into(),
            conversation_id: None,
            model: "gpt-4o".into(),
            provider_id: Some("cloud".into()),
            provider_kind: "cloud".into(),
            cost_usd: Some(spent),
            created_at: chrono::Utc::now().timestamp(),
        })
        .unwrap();
    }

    /// True when this work item ended `Failed` with a budget reason.
    fn budget_failed(db: &ProfileDb, id: &str) -> bool {
        db.get_work_item(id)
            .ok()
            .flatten()
            .map(|wi| {
                wi.state == WorkState::Failed
                    && wi.error.as_deref().unwrap_or("").contains("budget")
            })
            .unwrap_or(false)
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
        let budget_lock: BudgetLock = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let bl2 = Arc::clone(&budget_lock);
        let handle = tokio::spawn(run_one_item(agent, db2, "personal".into(), claimed, bl2));
        tokio::task::yield_now().await; // let the spawned task reach its stalled await
        tokio::time::advance(HELPER_DEADLINE + std::time::Duration::from_secs(1)).await;
        handle.await.unwrap();

        let done = db.get_work_item(&id).unwrap().expect("item exists");
        assert_eq!(
            done.state,
            WorkState::Failed,
            "a stalled helper must be Failed, not stuck running"
        );
        assert!(
            done.error
                .as_deref()
                .unwrap_or("")
                .to_lowercase()
                .contains("timed out")
                || done
                    .error
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains("timeout"),
            "the failure must name the timeout, got: {:?}",
            done.error
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn budget_governor_halts_an_unattended_helper_over_the_cap() {
        // C1: an over-budget unattended helper is Failed BEFORE the model call
        // fires (the StallStreamer would hang if reached — proving the halt
        // short-circuits before streaming).
        let (storage, root) = temp_storage();
        let storage = Arc::new(storage);
        let db = storage.open_profile("personal").unwrap();
        db.set_budget_cap(Some(1.0)).unwrap();
        db.record_usage(&crate::storage::UsageEvent {
            id: "u1".into(),
            conversation_id: None,
            model: "gpt".into(),
            provider_id: Some("cloud".into()),
            provider_kind: "cloud".into(),
            cost_usd: Some(5.0), // $5 spent vs a $1 cap → over
            created_at: chrono::Utc::now().timestamp(),
        })
        .unwrap();
        let agent = agent_with(Arc::new(StallStreamer(cloud())), Arc::clone(&storage));
        let now = 3_000_000;
        let item = dispatch_item(now);
        let id = item.id.clone();
        db.insert_work_item(&item).unwrap();
        let claimed = db.claim_next_due_work(now).unwrap().expect("claimed");
        run_one_item(
            agent,
            Arc::clone(&db),
            "personal".into(),
            claimed,
            Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        )
        .await;
        let done = db.get_work_item(&id).unwrap().expect("item");
        assert_eq!(
            done.state,
            WorkState::Failed,
            "over-budget unattended work must halt"
        );
        assert!(
            done.error.as_deref().unwrap_or("").contains("budget"),
            "the failure names the budget, got: {:?}",
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
        supervise_one_item(
            agent,
            Arc::clone(&db),
            "personal".into(),
            claimed,
            Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        )
        .await;

        let done = db.get_work_item(&id).unwrap().expect("item exists");
        assert_eq!(
            done.state,
            WorkState::Failed,
            "a panicked helper must be Failed, not stuck running"
        );
        assert!(
            done.error.as_deref().unwrap_or("").contains("panicked"),
            "the failure must name the panic, got: {:?}",
            done.error
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// M-09, half 1 — four concurrent helpers racing the SAME ledger must not
    /// collectively exceed the cap. Atomic check-and-reserve means only one can
    /// hold the remaining headroom, so the other three halt before spending.
    ///
    /// This test BOOKS REAL COST (`BillingStreamer`, $0.60/round) — the spend
    /// assertion is live, not decorative. Verified by mutation: deleting the
    /// `reservations.insert(...)` in `budget_check_and_reserve` makes it fail
    /// (see review-fixes/progress/P15.md for the recorded failure output).
    ///
    /// The gate is what makes the race real rather than scheduler-dependent:
    /// every helper is parked in `stream()` until all four have finished their
    /// budget check, so no helper's spend is on disk while another is checking.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn budget_lock_prevents_four_concurrent_helpers_from_exceeding_the_cap() {
        let (storage, root) = temp_storage();
        let storage = Arc::new(storage);
        let db = storage.open_profile("personal").unwrap();
        let budget_lock: BudgetLock = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        // Cap $2.00, already spent $0.50 → $1.50 of headroom. One helper's round
        // costs $0.60, so ONE fits ($1.10 total) and FOUR do not ($2.90).
        db.set_budget_cap(Some(2.0)).unwrap();
        seed_spend(&db, "seed", 0.50);

        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let agent = agent_with(
            Arc::new(BillingStreamer::gated(Arc::clone(&gate))),
            Arc::clone(&storage),
        );
        let now = 5_000_000;

        // Create and claim 4 items at the same due time.
        let mut items = Vec::new();
        for i in 0..4 {
            let mut item = dispatch_item_model(now, "gpt-4o");
            item.id = format!("race-{i}");
            db.insert_work_item(&item).unwrap();
            items.push(item);
        }
        let mut claimed = Vec::new();
        while let Ok(Some(item)) = db.claim_next_due_work(now) {
            claimed.push(item);
        }
        assert_eq!(claimed.len(), 4, "all 4 items must be claimable");

        // Spawn all 4 concurrently against the shared budget_lock.
        let mut handles = Vec::new();
        for item in claimed {
            let a = Arc::clone(&agent);
            let d = Arc::clone(&db);
            let b = Arc::clone(&budget_lock);
            handles.push(tokio::spawn(async move {
                run_one_item(a, d, "personal".into(), item, b).await;
            }));
        }

        // Bounded wait for the three losers to be halted. Whoever wins the lock
        // is parked in the gated stream with $0 booked, so this is the window in
        // which the reservation — and nothing else — is doing the work. If the
        // reservation is removed nobody halts, the poll simply times out, and
        // the spend assertions below catch the overspend.
        for _ in 0..400 {
            if items.iter().filter(|i| budget_failed(&db, &i.id)).count() == 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // Open the gate: every helper still running now streams and books cost.
        gate.add_permits(4);

        for h in handles {
            let _ = h.await;
        }

        let since = budget::month_start_ts(chrono::Utc::now());
        let summary = db.usage_summary_since(since).unwrap();

        // Guard against an INERT test: the surviving helper must actually have
        // billed. If the streamer silently books nothing, spend stays at the
        // seeded $0.50 and every cap assertion below passes vacuously.
        assert_eq!(
            summary.unknown_cost_calls, 0,
            "every booked call must be PRICED, else the governor's fail-closed \
             branch (not the reservation) would be what halts the losers"
        );
        // The headline invariant. (Mutation-checked: without the reservation
        // this reads $2.90.)
        assert!(
            summary.known_cost_usd <= 2.0,
            "total spend ${:.2} exceeded the $2.00 cap across 4 concurrent helpers",
            summary.known_cost_usd
        );
        assert!(
            (summary.known_cost_usd - (0.50 + ROUND_COST)).abs() < 0.01,
            "expected exactly one helper to bill one ${:.2} round on top of the \
             seeded $0.50; got ${:.2}",
            ROUND_COST,
            summary.known_cost_usd
        );

        // `budget::evaluate` halts at `known_cost_usd >= cap`, and one helper
        // reserves ALL remaining headroom — so it is exactly 3 of 4, not "some".
        let halted = items.iter().filter(|i| budget_failed(&db, &i.id)).count();
        assert_eq!(
            halted, 3,
            "exactly 3 of 4 helpers must be budget-halted (one holds the reservation)"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    /// M-09, half 2 — the MID-RUN ceiling. The pre-dispatch reservation bounds
    /// how many helpers start; this bounds how much ONE running helper spends.
    ///
    /// Drives `run_subagent` directly (so no pre-dispatch check is involved) with
    /// a streamer that emits a tool fence every round, making the loop keep going
    /// and keep billing $0.60/round:
    ///
    ///   round 0: sees $0.50  → runs → $1.10
    ///   round 1: sees $1.10  → runs → $1.70
    ///   round 2: sees $1.70  → runs → $2.30
    ///   round 3: sees $2.30 ≥ $2.00 cap → HALT
    ///
    /// Without the per-round re-check the loop runs all 7 permitted rounds and
    /// books $0.50 + 7×$0.60 = $4.70 against a $2.00 cap, and returns Ok.
    #[tokio::test]
    async fn unattended_run_is_halted_mid_loop_once_booked_spend_reaches_the_cap() {
        let (storage, root) = temp_storage();
        let storage = Arc::new(storage);
        let db = storage.open_profile("personal").unwrap();
        db.set_budget_cap(Some(2.0)).unwrap();
        seed_spend(&db, "seed", 0.50);

        let agent = agent_with(Arc::new(BillingStreamer::looping()), Arc::clone(&storage));
        let err = agent
            .run_subagent(
                "you are a helper",
                &[],
                "cloudco",
                "gpt-4o",
                "personal",
                Binding::Public,
                "do the thing",
            )
            .await
            .expect_err("an unattended run over the cap must stop with an error");
        assert!(
            err.to_string().contains("budget"),
            "the failure must name the budget, got: {err}"
        );

        let since = budget::month_start_ts(chrono::Utc::now());
        let summary = db.usage_summary_since(since).unwrap();
        // Live-test guard: the loop must actually have billed several rounds.
        assert!(
            summary.known_cost_usd > 0.50 + ROUND_COST,
            "the helper must have run more than one billed round before the \
             mid-run check could fire; got ${:.2}",
            summary.known_cost_usd
        );
        // The residual overrun is ONE round (the round that crosses the cap is
        // already paid for by the time the next check can see it) — so the bound
        // is cap + one round, NOT the unchecked $4.70.
        assert!(
            summary.known_cost_usd <= 2.0 + ROUND_COST,
            "spend ${:.2} must stop within one round's cost of the $2.00 cap",
            summary.known_cost_usd
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
