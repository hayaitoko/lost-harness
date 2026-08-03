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
///
/// Double duty: it is also the divisor in [`budget_check_and_reserve`]'s
/// per-item allowance (`remaining / MAX_CONCURRENT_HELPERS`) — the worst-case
/// number of items that can be mid-flight at once is exactly this bound, so
/// splitting the headroom this many ways guarantees a full concurrent burst
/// can all be admitted while their reservations still sum within the cap.
const MAX_CONCURRENT_HELPERS: usize = 4;

/// Floor on one work item's budget reservation (USD). Two jobs:
///
/// 1. **Saturation is reachable.** A pure `remaining / MAX_CONCURRENT_HELPERS`
///    slice decays geometrically — each admission reserves a quarter of an
///    ever-smaller remainder, the sum never reaches the cap, and admission
///    control never actually says no. With a floor, every admission consumes
///    at least `min(FLOOR, remaining)`, so a run of admissions genuinely
///    exhausts the headroom and the next helper halts.
/// 2. **A lone item keeps a meaningful allowance.** When one item is the only
///    thing running against nearly-spent headroom, its slice is clamped up to
///    this floor (never past the actual remainder) instead of a vanishing
///    sliver of a sliver.
///
/// $0.25 ≈ the cost of one modest frontier-model round: small enough that a
/// tight hobby cap still admits several concurrent helpers, large enough that
/// saturating a cap takes at most `cap / 0.25` concurrent admissions.
const MIN_ITEM_RESERVATION_USD: f64 = 0.25;

/// How long the poll loop sleeps between fully-drained ticks. Each tick
/// drains every profile's DUE queue completely before sleeping, so this only
/// bounds how often an empty (or momentarily-idle) queue is re-checked — it
/// is not a per-item latency.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Wall-clock ceiling for one helper run. A stalled model stream fails the item
/// (and frees its concurrency permit) rather than hanging forever.
const HELPER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(300);

/// Shared state for transactional budget reservations across concurrent helpers.
/// Keyed by profile name, then by WORK ITEM id; each leaf is the USD that one
/// in-flight item has reserved but not yet spent. Per-item keying is what lets
/// one helper's release (on any exit path) return exactly ITS allowance without
/// touching a concurrent sibling's still-live reservation.
/// The Mutex ensures atomic check-and-reserve (M-09), so concurrent helpers
/// cannot collectively exceed the configured cap.
type BudgetLock = Arc<tokio::sync::Mutex<HashMap<String, HashMap<String, f64>>>>;

/// Lock, check the budget against both on-disk spend and in-memory reservations,
/// and — if the profile is capped — reserve a bounded PER-ITEM allowance before
/// dispatch. The caller MUST reconcile after the run.
///
/// **The allowance formula**:
///
/// ```text
/// remaining = cap − (booked known spend + Σ live reservations)   // > 0 here
/// allowance = clamp(remaining / MAX_CONCURRENT_HELPERS,
///                   MIN_ITEM_RESERVATION_USD, remaining)
/// ```
///
/// Rationale: reserving the FULL remainder (the old behavior) meant whichever
/// helper won the lock first swallowed all headroom, so with a $100 cap and $1
/// spent, one delegate call reserved $99 and its three concurrent siblings were
/// terminally budget-failed despite the cap being untouched. Dividing by
/// [`MAX_CONCURRENT_HELPERS`] — the hard ceiling on how many items can be
/// mid-flight at once — sizes each slice so a full concurrent burst is all
/// admitted while the slices still sum within the remainder. The
/// [`MIN_ITEM_RESERVATION_USD`] floor keeps the slices from decaying
/// geometrically (so back-to-back admissions genuinely exhaust the headroom and
/// saturation halts the excess) and keeps a lone item's allowance meaningful;
/// the final `min(…, remaining)` clamp is the HARD INVARIANT: no admission ever
/// reserves past the actual remainder, so at every instant, under any
/// interleaving, `Σ live reservations + booked known spend ≤ cap`. A helper is
/// budget-halted ONLY when that sum has consumed the entire cap (`remaining ≤
/// 0` — the `budget::evaluate` Halt below); any positive unreserved headroom
/// admits it.
///
/// Returns `true` when the item should proceed; `false` when it was already
/// `finish_work_item`'d as `Failed` with a budget reason (caller MUST `return`).
///
/// **What this is, precisely** (M-09, half 1): an ATOMIC pre-dispatch
/// reservation. Every reservation is visible to every concurrent helper, so a
/// helper reading the same ledger sees its siblings' committed-but-unbooked
/// headroom and is admitted only against what is genuinely unclaimed. That
/// closes the concurrent check-then-spend race — several helpers can no longer
/// each observe the same under-cap ledger and collectively blow past the cap.
///
/// **What it is NOT**: a bound on what a SINGLE helper spends. Nothing here
/// re-reads the ledger once the helper is running. The other half of the
/// ceiling is the per-round unattended re-check in
/// `AgentLoop::process_message_inner`, which halts a running unattended loop as
/// soon as its BOOKED spend reaches the cap. Together the residual overrun is
/// one round's cost PER ADMITTED HELPER (the round that crosses the cap is
/// already paid for before it can be observed), not a whole `HELPER_DEADLINE`
/// window — bounded, not zero. A zero-overrun cap needs pre-call cost
/// reservation, which the provider APIs don't offer.
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
            let reserved: f64 = reservations
                .get(profile)
                .map(|per_item| per_item.values().sum())
                .unwrap_or(0.0);
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
            // Reserve this ITEM's bounded allowance (see the doc comment for the
            // formula) so concurrent helpers see a reduced — but not zeroed —
            // remainder. `evaluate` did not Halt, so `remaining > 0` here, and
            // the final `.min(remaining)` clamp upholds the invariant:
            // Σ reservations + known spend never passes the cap.
            if let Some(cap_val) = cap {
                let remaining = (cap_val - effective.known_cost_usd).max(0.0);
                let allowance = (remaining / MAX_CONCURRENT_HELPERS as f64)
                    .max(MIN_ITEM_RESERVATION_USD)
                    .min(remaining);
                reservations
                    .entry(profile.to_string())
                    .or_default()
                    .insert(item.id.clone(), allowance);
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

/// Release ONE work item's budget reservation (reconcile).
/// Called after the helper run completes (success, error, timeout — and, via
/// `supervise_one_item`, panic) so the next budget check sees actual on-disk
/// spend rather than the reservation. Keyed by item id so a finishing helper
/// returns exactly its own allowance and never touches a concurrent sibling's
/// still-live reservation; removing a key that is absent (item never reserved,
/// or already released) is a harmless no-op, so the panic-path double call is
/// safe.
async fn budget_reconcile(budget_lock: &BudgetLock, profile: &str, item_id: &str) {
    let mut reservations = budget_lock.lock().await;
    if let Some(per_item) = reservations.get_mut(profile) {
        per_item.remove(item_id);
        if per_item.is_empty() {
            reservations.remove(profile);
        }
    }
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
        // run_one_item panicked — release this item's budget reservation so it
        // doesn't leak and starve future helpers of that headroom forever.
        budget_reconcile(&budget_for_cleanup, &profile_for_reconcile, &item_id).await;
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
            budget_reconcile(&budget_lock, &profile, &item.id).await;
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
    // this item's bounded allowance TRANSACTIONALLY so concurrent helpers see
    // each other's committed-but-unbooked headroom. This bounds how many helpers
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

    // Reconcile: release this item's budget reservation. Actual per-round costs
    // have already been booked by process_message → record_usage during the run.
    budget_reconcile(&budget_lock, &profile, &item.id).await;

    // Resolve the run into (terminal state, result payload, error, and the
    // human-facing note posted into the parent). Success, failure, AND timeout
    // all post SOMETHING back — the user must never see "dispatched" then
    // silence (Wave 4.3c review fix).
    //
    // `zone` is the trust zone the helper's OWN turns were stamped with,
    // carried out of `run_subagent` (`SubagentRun::zone`). A failed or timed-out
    // run has no completed turn to speak for, so it carries `None` — rendered as
    // UNKNOWN, never as "local".
    #[allow(clippy::type_complexity)]
    let (state, result_json, error, note, zone): (
        WorkState,
        Option<String>,
        Option<String>,
        String,
        Option<crate::models::TrustZone>,
    ) = match outcome {
        Ok(Ok(run)) => (
            WorkState::Done,
            Some(run.text.clone()),
            None,
            format!("**[helper: {agent_name}]**\n\n{}", run.text),
            run.zone,
        ),
        Ok(Err(e)) => {
            let e = e.to_string();
            (
                WorkState::Failed,
                None,
                Some(e.clone()),
                format!("**[helper: {agent_name}] failed:** {e}"),
                None,
            )
        }
        Err(_elapsed) => (
            WorkState::Failed,
            None,
            Some("helper timed out".to_string()),
            format!("**[helper: {agent_name}] timed out** and was stopped."),
            None,
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
            // The trust zone the HELPER ran in — carried out of the run itself,
            // stamped on its own turns at send time by the same `is_cloud` the
            // gate was given.
            //
            // This used to be `model_manager().get_provider(&provider_id)`
            // evaluated HERE, after the run finished: a registry lookup, not a
            // fact about the turn. Edit the provider's base URL or delete it
            // while the helper is running and the note recorded whatever the
            // registry said afterwards — a cloud helper's output could be filed
            // as "local". That is precisely what stamping the zone on the row
            // (profile schema v12) exists to prevent, so the last live lookup
            // is gone. `None` renders as UNKNOWN, never as "local".
            endpoint_zone: zone.map(|z| z.as_str().to_string()),
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
        /// Completion tokens the `usage` chunk reports each round — what prices
        /// the round. 60_000 against `gpt-4o`'s $10.00/Mtok output = $0.60
        /// ([`ROUND_COST`]); the saturation test books cheaper rounds instead.
        completion_tokens: u64,
    }

    impl BillingStreamer {
        fn gated(gate: Arc<tokio::sync::Semaphore>) -> Self {
            Self {
                provider: cloud(),
                gate: Some(gate),
                keep_calling_tools: false,
                completion_tokens: 60_000,
            }
        }
        fn looping() -> Self {
            Self {
                provider: cloud(),
                gate: None,
                keep_calling_tools: true,
                completion_tokens: 60_000,
            }
        }
        /// Gated, with a chosen per-round completion-token count (i.e. a chosen
        /// per-round cost at `gpt-4o` pricing: 10_000 tokens = $0.10).
        fn gated_with_tokens(gate: Arc<tokio::sync::Semaphore>, completion_tokens: u64) -> Self {
            Self {
                provider: cloud(),
                gate: Some(gate),
                keep_calling_tools: false,
                completion_tokens,
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
            let completion_tokens = self.completion_tokens;
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
                    "usage": { "prompt_tokens": 0, "completion_tokens": completion_tokens }
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

    /// A streamer that emits a fenced tool call for its first `tool_rounds`
    /// rounds and then a plain "all done" — with a `usage` chunk EVERY round,
    /// so each round books a ledger row. Paired with an UNPRICED model id,
    /// every row is `cost_usd = NULL` (an unknown-cost call): the exact shape
    /// that used to terminally halt a capped unattended run at round 1.
    struct CountdownToolStreamer {
        provider: Provider,
        fences_remaining: std::sync::atomic::AtomicUsize,
    }

    impl CountdownToolStreamer {
        fn new(tool_rounds: usize) -> Self {
            Self {
                provider: cloud(),
                fences_remaining: std::sync::atomic::AtomicUsize::new(tool_rounds),
            }
        }
    }

    impl ModelStreamer for CountdownToolStreamer {
        fn provider(&self) -> &Provider {
            &self.provider
        }
        fn stream<'a>(
            &'a self,
            _m: &'a str,
            _msgs: Vec<ChatMessage>,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<SseStream>> + Send + 'a>>
        {
            // Decrement-if-positive: rounds with a fence remaining keep the
            // tool loop going; the round after the last fence answers plainly.
            let emit_fence = self
                .fences_remaining
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |n| n.checked_sub(1),
                )
                .is_ok();
            Box::pin(async move {
                let content = if emit_fence {
                    "working\n```tool\n{\"name\":\"no_such_tool\",\"arguments\":{}}\n```"
                } else {
                    "all done"
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

    /// M-09, half 1, REVISED (round-1 NEEDS-LUKAS #3) — four concurrent helpers
    /// racing the SAME capped ledger must ALL be admitted when the headroom
    /// covers a full burst: each item reserves a bounded per-item allowance
    /// (`remaining / MAX_CONCURRENT_HELPERS`, floored and clamped), never the
    /// whole remainder. The OLD semantics reserved the entire remaining
    /// headroom for whichever helper won the lock first — with $1.50 of
    /// headroom one delegate call swallowed all of it and its three concurrent
    /// siblings were terminally budget-failed despite the cap being untouched.
    /// The pinned test of that behavior
    /// (`budget_lock_prevents_four_concurrent_helpers_from_exceeding_the_cap`,
    /// exactly-3-of-4-halted) is superseded by this one; the halting half now
    /// lives in `reservation_saturation_halts_excess_helpers_…` below.
    ///
    /// The gate makes the race real rather than scheduler-dependent: every
    /// helper is parked in `stream()` until all four hold LIVE reservations at
    /// once, so the invariant assertion observes peak commitment, and each
    /// helper then books a REAL $0.10 round (10k tokens at `gpt-4o` output
    /// pricing) — the spend assertions are live, not decorative.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn four_concurrent_helpers_share_headroom_under_one_cap_and_all_run() {
        let (storage, root) = temp_storage();
        let storage = Arc::new(storage);
        let db = storage.open_profile("personal").unwrap();
        let budget_lock: BudgetLock = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        // Cap $2.00, already spent $0.50 → $1.50 of headroom. Slices as the
        // four admissions land: $0.375, $0.28125, $0.25, $0.25 (the last two
        // floor-clamped) — Σ = $1.15625, within the remainder, so ALL FOUR run.
        db.set_budget_cap(Some(2.0)).unwrap();
        seed_spend(&db, "seed", 0.50);

        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let agent = agent_with(
            Arc::new(BillingStreamer::gated_with_tokens(
                Arc::clone(&gate),
                10_000,
            )),
            Arc::clone(&storage),
        );
        let now = 5_000_000;

        // Create and claim 4 items at the same due time.
        let mut items = Vec::new();
        for i in 0..4 {
            let mut item = dispatch_item_model(now, "gpt-4o");
            item.id = format!("burst-{i}");
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

        // Wait until all four hold live reservations at once (each is then
        // parked in the gated stream with none of its own spend booked yet).
        let mut live = 0usize;
        for _ in 0..400 {
            live = budget_lock
                .lock()
                .await
                .get("personal")
                .map(|per_item| per_item.len())
                .unwrap_or(0);
            if live == 4 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(
            live, 4,
            "all four items must hold live reservations at once — none may be \
             budget-halted on the way in"
        );

        // THE HARD INVARIANT, observed at peak commitment: live reservations
        // plus booked known spend never pass the cap.
        {
            let reservations = budget_lock.lock().await;
            let reserved: f64 = reservations.get("personal").unwrap().values().sum();
            let since = budget::month_start_ts(chrono::Utc::now());
            let booked = db.usage_summary_since(since).unwrap().known_cost_usd;
            assert!(
                reserved + booked <= 2.0 + 1e-9,
                "Σ live reservations (${reserved:.5}) + booked spend (${booked:.2}) \
                 must stay within the $2.00 cap"
            );
        }

        // Open the gate: all four stream and book their rounds.
        gate.add_permits(4);
        for h in handles {
            let _ = h.await;
        }

        // NONE budget-halted; every item ran to completion.
        for item in &items {
            let done = db.get_work_item(&item.id).unwrap().expect("item exists");
            assert_eq!(
                done.state,
                WorkState::Done,
                "helper {} must run to completion (error: {:?}) — per-item \
                 reservations admit a full concurrent burst under ample headroom",
                item.id,
                done.error
            );
        }

        let since = budget::month_start_ts(chrono::Utc::now());
        let summary = db.usage_summary_since(since).unwrap();
        // Guard against an INERT test: all four helpers must actually have
        // billed PRICED rounds, else the assertions above pass vacuously.
        assert_eq!(
            summary.unknown_cost_calls, 0,
            "every booked call must be PRICED for the spend accounting to be live"
        );
        assert!(
            (summary.known_cost_usd - 0.90).abs() < 0.01,
            "all four helpers billed one $0.10 round on top of the seeded $0.50; \
             got ${:.2}",
            summary.known_cost_usd
        );
        assert!(
            budget_lock.lock().await.is_empty(),
            "every reservation is released once its run completes"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    /// The saturation half of the revised M-09: when live reservations
    /// genuinely exhaust the cap, the excess helpers DO halt — the per-item
    /// allowance loosens the old whole-remainder grab without unbounding
    /// admission. Six items race a $1.00 cap: the floor makes each admission
    /// consume $0.25, so exactly four are admitted (Σ = the full $1.00) and
    /// the other two are budget-failed while genuinely out of unreserved
    /// headroom — before anything is booked, so the reservations alone do the
    /// halting. Total booked spend never exceeds the cap, and every
    /// reservation is released at the end.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reservation_saturation_halts_excess_helpers_and_spend_stays_capped() {
        let (storage, root) = temp_storage();
        let storage = Arc::new(storage);
        let db = storage.open_profile("personal").unwrap();
        let budget_lock: BudgetLock = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        db.set_budget_cap(Some(1.0)).unwrap();

        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let agent = agent_with(
            Arc::new(BillingStreamer::gated_with_tokens(
                Arc::clone(&gate),
                10_000,
            )),
            Arc::clone(&storage),
        );
        let now = 6_000_000;

        let mut items = Vec::new();
        for i in 0..6 {
            let mut item = dispatch_item_model(now, "gpt-4o");
            item.id = format!("sat-{i}");
            db.insert_work_item(&item).unwrap();
            items.push(item);
        }
        let mut claimed = Vec::new();
        while let Ok(Some(item)) = db.claim_next_due_work(now) {
            claimed.push(item);
        }
        assert_eq!(claimed.len(), 6, "all 6 items must be claimable");

        let mut handles = Vec::new();
        for item in claimed {
            let a = Arc::clone(&agent);
            let d = Arc::clone(&db);
            let b = Arc::clone(&budget_lock);
            handles.push(tokio::spawn(async move {
                run_one_item(a, d, "personal".into(), item, b).await;
            }));
        }

        // The four winners park in the gated stream (they cannot book or
        // release yet), so the two losers can only have been halted by
        // exhausted RESERVATIONS — nothing is on the ledger.
        let mut halted = 0usize;
        for _ in 0..400 {
            halted = items.iter().filter(|i| budget_failed(&db, &i.id)).count();
            if halted == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(
            halted, 2,
            "exactly two of six helpers must be budget-halted once reservations \
             consume the whole cap"
        );
        {
            let reservations = budget_lock.lock().await;
            let reserved: f64 = reservations.get("personal").unwrap().values().sum();
            assert!(
                (reserved - 1.0).abs() < 1e-9,
                "the four admitted slices must sum to exactly the $1.00 cap — \
                 saturated, never overcommitted; got ${reserved:.5}"
            );
        }

        gate.add_permits(6);
        for h in handles {
            let _ = h.await;
        }

        let done = items
            .iter()
            .filter(|i| db.get_work_item(&i.id).unwrap().expect("item").state == WorkState::Done)
            .count();
        assert_eq!(done, 4, "the four admitted helpers all complete");

        let since = budget::month_start_ts(chrono::Utc::now());
        let summary = db.usage_summary_since(since).unwrap();
        assert_eq!(
            summary.unknown_cost_calls, 0,
            "every booked call must be PRICED for the cap assertion to be live"
        );
        assert!(
            summary.known_cost_usd <= 1.0,
            "total booked spend ${:.2} must never exceed the $1.00 cap",
            summary.known_cost_usd
        );
        assert!(
            (summary.known_cost_usd - 0.40).abs() < 0.01,
            "the four admitted helpers billed one $0.10 round each; got ${:.2}",
            summary.known_cost_usd
        );
        assert!(
            budget_lock.lock().await.is_empty(),
            "saturating reservations are all released after the runs finish"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    /// The admission arithmetic, pinned deterministically (no tasks, no
    /// streamers): a $1.00 cap admits exactly four items — floor-sized $0.25
    /// slices — with Σ reservations + known spend ≤ cap after EVERY grant; the
    /// fifth-onward items are refused and budget-failed; and releasing one
    /// item's reservation restores exactly its slice of headroom for the next
    /// admission.
    #[tokio::test]
    async fn per_item_reservations_never_overcommit_and_release_restores_headroom() {
        let (storage, root) = temp_storage();
        let storage = Arc::new(storage);
        let db = storage.open_profile("personal").unwrap();
        let budget_lock: BudgetLock = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        db.set_budget_cap(Some(1.0)).unwrap();

        let now = 7_000_000;
        for i in 0..7 {
            let mut item = dispatch_item_model(now, "gpt-4o");
            item.id = format!("adm-{i}");
            db.insert_work_item(&item).unwrap();
        }
        let mut claimed = Vec::new();
        while let Ok(Some(item)) = db.claim_next_due_work(now) {
            claimed.push(item);
        }
        assert_eq!(claimed.len(), 7);

        let mut admitted = Vec::new();
        for item in &claimed {
            let ok = budget_check_and_reserve(&budget_lock, &db, "personal", item).await;
            let reserved: f64 = budget_lock
                .lock()
                .await
                .get("personal")
                .map(|per_item| per_item.values().sum())
                .unwrap_or(0.0);
            assert!(
                reserved <= 1.0 + 1e-12,
                "after {}: Σ reservations ${reserved:.5} overcommits the $1.00 cap",
                item.id
            );
            if ok {
                admitted.push(item.id.clone());
            } else {
                assert!(
                    budget_failed(&db, &item.id),
                    "a refused item must be budget-failed"
                );
            }
        }
        let expected: Vec<String> = claimed[..4].iter().map(|i| i.id.clone()).collect();
        assert_eq!(
            admitted, expected,
            "a $1.00 cap admits exactly the first four floor-sized ($0.25) slices"
        );

        // Release one admitted item — its slice (exactly $0.25) is the ONLY
        // headroom returned, and the next admission fits it again. (Admission
        // consults the ledger + reservations, not the item's row state, so
        // re-checking a previously refused item is fine here.)
        budget_reconcile(&budget_lock, "personal", &admitted[1]).await;
        assert!(
            budget_check_and_reserve(&budget_lock, &db, "personal", &claimed[6]).await,
            "releasing one reservation restores exactly one slice of headroom"
        );
        let reserved: f64 = budget_lock
            .lock()
            .await
            .get("personal")
            .map(|per_item| per_item.values().sum())
            .unwrap_or(0.0);
        assert!(
            (reserved - 1.0).abs() < 1e-9,
            "back to saturated — never overcommitted; got ${reserved:.5}"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    /// A helper that PANICS mid-run must not leak its reservation: the
    /// supervisor's cleanup releases it BY ITEM ID, so the next admission sees
    /// the full remainder again rather than a permanently reduced cap. (The
    /// success / error / timeout paths all share `run_one_item`'s one
    /// unconditional reconcile; the panic path is the only separate release
    /// and is exercised here.)
    #[tokio::test]
    async fn a_panicked_helper_releases_its_budget_reservation() {
        let (storage, root) = temp_storage();
        let storage = Arc::new(storage);
        let db = storage.open_profile("personal").unwrap();
        let budget_lock: BudgetLock = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        db.set_budget_cap(Some(10.0)).unwrap();
        let agent = agent_with(Arc::new(PanicStreamer(cloud())), Arc::clone(&storage));

        let now = 8_000_000;
        let item = dispatch_item_model(now, "gpt-4o");
        let id = item.id.clone();
        db.insert_work_item(&item).unwrap();
        let claimed = db.claim_next_due_work(now).unwrap().expect("claimed");
        supervise_one_item(
            agent,
            Arc::clone(&db),
            "personal".into(),
            claimed,
            Arc::clone(&budget_lock),
        )
        .await;

        let done = db.get_work_item(&id).unwrap().expect("item");
        assert_eq!(
            done.state,
            WorkState::Failed,
            "the panicked helper must be Failed"
        );
        assert!(
            budget_lock.lock().await.is_empty(),
            "the panic path must release the item's reservation"
        );

        // Leak-proof beyond emptiness: a follow-up item's slice is computed
        // against the FULL $10 remainder — (10 − 0) / 4 = $2.50. A leaked
        // reservation would shrink it to (10 − 2.5) / 4 = $1.875.
        let mut item2 = dispatch_item_model(now + 1, "gpt-4o");
        item2.id = "after-panic".into();
        db.insert_work_item(&item2).unwrap();
        let claimed2 = db.claim_next_due_work(now + 1).unwrap().expect("claimed");
        assert!(budget_check_and_reserve(&budget_lock, &db, "personal", &claimed2).await);
        let slice = budget_lock.lock().await["personal"]["after-panic"];
        assert!(
            (slice - 2.5).abs() < 1e-9,
            "the follow-up slice must see the full remainder, got ${slice:.4}"
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

    // ── The unpriced-model policy: untracked spend warns, never halts ────────

    /// A capped profile on a model OUTSIDE the pricing table must complete a
    /// 3-round tool task. Every round books an unknown-cost row
    /// (`cost_usd = NULL`); the OLD per-round governor failed closed on
    /// `unknown_cost_calls > 0` and killed the run at round 1, so a capped
    /// multi-round task could never finish on most OpenRouter/unknown models.
    #[tokio::test]
    async fn a_capped_run_on_an_unpriced_model_completes_a_multi_round_tool_task() {
        let (storage, root) = temp_storage();
        let storage = Arc::new(storage);
        let db = storage.open_profile("personal").unwrap();
        db.set_budget_cap(Some(2.0)).unwrap();

        let agent = agent_with(
            Arc::new(CountdownToolStreamer::new(2)),
            Arc::clone(&storage),
        );
        let run = agent
            .run_subagent(
                "you are a helper",
                &[],
                "cloudco",
                "totally-unpriced-model",
                "personal",
                Binding::Public,
                "do the thing",
            )
            .await
            .expect("a capped run on an unpriced model must complete, not halt");
        assert!(
            run.text.contains("all done"),
            "the final round's answer comes back, got: {}",
            run.text
        );

        let since = budget::month_start_ts(chrono::Utc::now());
        let summary = db.usage_summary_since(since).unwrap();
        assert_eq!(
            summary.unknown_cost_calls, 3,
            "all 3 rounds (2 tool rounds + the final answer) must have booked \
             unknown-cost rows, else this test isn't exercising the unpriced path"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// A sink that records `budget_warning` calls, so the untracked-spend
    /// warning is observable (`run_subagent` hardwires the no-op HeadlessSink).
    #[derive(Default)]
    struct RecordingSink {
        warnings: std::sync::Mutex<Vec<String>>,
    }

    impl crate::agent::result_sink::ResultSink for RecordingSink {
        fn token(&self, _c: &str, _m: &str, _t: &str) {}
        fn local_reroute(&self, _c: &str, _r: &str, _f: &str, _t: &str, _b: bool) {}
        fn memory_event(&self, _c: &str, _k: &'static str, _n: usize) {}
        fn error(&self, _c: &str, _e: &str, _s: &'static str) {}
        fn budget_warning(&self, _conversation_id: &str, message: &str) {
            self.warnings.lock().unwrap().push(message.to_string());
        }
    }

    /// The warn half of the unpriced-model policy: the run proceeds, and the
    /// EXISTING `stream:budget_warning` path carries the "spend is untracked"
    /// notice — once per run, not once per round (rounds 1 AND 2 both observe
    /// `unknown_cost_calls > 0` here).
    #[tokio::test]
    async fn untracked_spend_on_a_capped_unattended_run_fires_the_budget_warning_once() {
        let (storage, root) = temp_storage();
        let storage = Arc::new(storage);
        let db = storage.open_profile("personal").unwrap();
        db.set_budget_cap(Some(2.0)).unwrap();
        let now = chrono::Utc::now().timestamp();
        db.create_conversation(&crate::storage::Conversation {
            id: "conv-unpriced".into(),
            name: "t".into(),
            pinned: false,
            binding: "public".into(),
            folder_id: None,
            color: None,
            created_at: now,
            updated_at: now,
        })
        .unwrap();

        // `agent_with` builds on `ToolDispatcher::empty()` — no approver wired,
        // so `is_attended()` is false and the per-round governor is live even
        // though process_message is driven directly (necessary to inject an
        // observable sink instead of run_subagent's hardwired HeadlessSink).
        let agent = agent_with(
            Arc::new(CountdownToolStreamer::new(2)),
            Arc::clone(&storage),
        );
        let recording = Arc::new(RecordingSink::default());
        let sink: Arc<dyn crate::agent::result_sink::ResultSink> =
            Arc::clone(&recording) as Arc<dyn crate::agent::result_sink::ResultSink>;
        agent
            .process_message(
                "do the thing".into(),
                "conv-unpriced".into(),
                Binding::Public,
                "cloudco".into(),
                "totally-unpriced-model".into(),
                "personal".into(),
                crate::hooks::SessionMode::Normal,
                &sink,
            )
            .await
            .expect("the capped unpriced run completes");

        let warnings = recording.warnings.lock().unwrap();
        assert_eq!(
            warnings.len(),
            1,
            "exactly one budget warning per run, got: {warnings:?}"
        );
        assert!(
            warnings[0].contains("untracked"),
            "the warning must say spend is untracked, got: {}",
            warnings[0]
        );
        assert!(
            warnings[0].contains("totally-unpriced-model"),
            "the warning must name the model, got: {}",
            warnings[0]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    // ── The helper note's trust zone is HISTORY, not a registry lookup ───────

    /// A streamer that rewrites the provider registry MID-RUN — the user edits
    /// the endpoint (or deletes and re-adds it) while a helper is working.
    ///
    /// It answers as the cloud endpoint the helper was dispatched to, but by the
    /// time the run returns, provider `cloudco` points at a loopback URL, i.e.
    /// `trust_zone()` now says Local. Any code that asks the registry AFTER the
    /// run records "local" for a turn that went to `api.cloudco.example`.
    struct RegistrySwappingStreamer {
        provider: Provider,
        mm: Arc<ModelManager>,
    }

    impl ModelStreamer for RegistrySwappingStreamer {
        fn provider(&self) -> &Provider {
            &self.provider
        }
        fn stream<'a>(
            &'a self,
            _m: &'a str,
            _msgs: Vec<ChatMessage>,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<SseStream>> + Send + 'a>>
        {
            let mm = Arc::clone(&self.mm);
            Box::pin(async move {
                // Same id, private base URL: exactly what an edit in
                // Settings → Models produces.
                mm.add_provider(Provider::new(
                    "cloudco",
                    "Cloud",
                    "http://127.0.0.1:11434/v1",
                    None,
                    ProviderKind::Local,
                ));
                let delta = serde_json::json!({ "choices": [{ "delta": { "content": "done" } }] })
                    .to_string();
                let chunks: Vec<Vec<u8>> = vec![
                    format!("data: {delta}\n").into_bytes(),
                    b"data: [DONE]\n".to_vec(),
                ];
                Ok(SseStream::from_byte_stream(tokio_stream::iter(
                    chunks.into_iter().map(Ok::<Vec<u8>, reqwest::Error>),
                )))
            })
        }
    }

    #[tokio::test]
    async fn the_helper_note_records_the_zone_the_run_actually_used() {
        // THE regression. The note posted back into the parent conversation used
        // to take its zone from `model_manager().get_provider(..)` evaluated
        // AFTER the run finished — the registry as it is NOW, not the endpoint
        // that served the turn. Repoint (or delete) the provider mid-run and a
        // helper that talked to a public cloud endpoint got filed as "local".
        //
        // The whole point of stamping the zone on the row (profile schema v12)
        // is that a past turn's trust zone is a fact about the past.
        let (storage, root) = temp_storage();
        let storage = Arc::new(storage);
        let db = storage.open_profile("personal").unwrap();

        let mm = Arc::new(ModelManager::new());
        mm.add_provider(cloud());
        let streamer = Arc::new(RegistrySwappingStreamer {
            provider: cloud(),
            mm: Arc::clone(&mm),
        });
        let agent = Arc::new(
            AgentLoop::new(
                PrivacyGate::new(Arc::new(RulesClassifier::new())),
                Arc::clone(&mm),
                Arc::clone(&storage),
                Arc::new(crate::tools::ToolDispatcher::empty()),
            )
            .with_model_streamer_override(streamer),
        );

        // The parent conversation the helper's outcome is posted into.
        let now = 1_000_000;
        let parent = "parent-conv".to_string();
        db.create_conversation(&crate::storage::Conversation {
            id: parent.clone(),
            name: "Parent".into(),
            pinned: false,
            binding: "public".into(),
            folder_id: None,
            color: None,
            created_at: now,
            updated_at: now,
        })
        .unwrap();

        let mut item = dispatch_item(now);
        item.target_conversation_id = Some(parent.clone());
        db.insert_work_item(&item).unwrap();
        let claimed = db.claim_next_due_work(now).unwrap().expect("claimed");

        let budget_lock: BudgetLock = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        run_one_item(
            agent,
            Arc::clone(&db),
            "personal".into(),
            claimed,
            budget_lock,
        )
        .await;

        // Sanity: the swap really happened, so a post-hoc lookup WOULD have
        // answered "local" — the test is exercising the live hazard, not a
        // hypothetical one.
        assert_eq!(
            mm.get_provider("cloudco").map(|p| p.trust_zone()),
            Some(crate::models::TrustZone::Local),
            "the registry must read Local by now for this test to mean anything"
        );

        let note = db
            .list_messages_by_conversation(&parent)
            .unwrap()
            .into_iter()
            .find(|m| m.routing_decision.as_deref() == Some("delegated"))
            .expect("the helper's outcome is posted into the parent conversation");
        assert_eq!(
            note.endpoint_zone.as_deref(),
            Some("cloud"),
            "the note must record the zone the helper actually ran in"
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
