//! Boot reconciliation sweep for the `install_pack` atomicity residual
//! (fix4/pack-reconcile — PRODUCT DECISION, Lukas, 2026-08-03).
//!
//! `install_pack` (see the parent module's `# Atomicity` note) cannot put
//! skills + agent types (`global.db`) and cron jobs (the profile DB) in one
//! SQLite transaction — they are two separate files. Both transactions are
//! held open at once and the profile's cron transaction commits FIRST,
//! `global.db` LAST, so a crash in the one-`commit()`-wide window between
//! them can leave cron jobs durable with no matching skill/agent-type rows in
//! `global.db` — orphaned, disabled cron jobs nothing will ever review or
//! enable.
//!
//! The decision is to keep that transaction design exactly as it is — it is
//! correct, and closing the window would need a distributed commit protocol
//! across two SQLite files, out of scope here — and instead run this sweep
//! at every boot to clean up the artifact it can leave behind.
//!
//! # What makes this possible
//! Before this change there was no way to tell which pack installed a given
//! row at all, so nothing could be correlated back to `global.db`.
//! Migrations global-v10 and profile-v13 (`storage::migrations`) add that
//! provenance: `install_pack` mints one UUID (`install_id`) per call and
//! stamps it, as `pack_install_id`, on every row it inserts in EITHER
//! database — plus `cron_jobs.pack_expected_global_rows`, the count of
//! skill+agent-type rows that SAME call was writing to `global.db` (0 for a
//! cron-only pack, which never even opens a global transaction and so has no
//! atomicity window at all).
//!
//! # Round 2: row counts alone are NOT proof (read this before touching it)
//! An earlier version of this sweep deleted a cron job whenever its pack's
//! `global.db` rows were entirely absent, reasoning that `install_pack`'s
//! global transaction is atomic so "absent" could only mean "never
//! committed". Adversarial review found the flaw: "absent" is EQUALLY what a
//! perfectly healthy install looks like after the user deletes its skill or
//! agent type by hand in Settings (`GlobalDb::delete_skill` /
//! `delete_agent_type` — ordinary, unconditional deletes with zero
//! relationship to `pack_install_id`). A row count, taken later, cannot tell
//! those two histories apart — so nothing derived only from `global.db`'s
//! CURRENT state can ever be sufficient here, no matter how the row-state
//! checks below are tightened.
//!
//! The fix is to stop inferring intent from state and record it directly.
//! Profile v14 adds `pack_install_pending`: `install_pack` writes a row keyed
//! by `install_id` in the SAME transaction as the cron-job inserts (so it is
//! exactly as durable as they are — see [`super`]'s module doc), and clears
//! it in a separate write immediately after its global transaction commits.
//! A row STILL being there is the one fact that only a genuine crash between
//! those two commits can produce; a later, unrelated deletion never
//! recreates it.
//!
//! # The deletion rule
//! For each distinct `pack_install_id` found on candidate cron jobs
//! (`ProfileDb::pack_cron_orphan_candidates` — pack-installed AND
//! `pack_expected_global_rows > 0`; a cron-only pack has nothing to
//! reconcile and is filtered out there, not here):
//!
//! | pending marker | global.db rows | meaning                                   | action                                  |
//! |-----------------|-----------------|--------------------------------------------|------------------------------------------|
//! | absent          | present         | healthy install                            | untouched                               |
//! | absent          | absent          | healthy install, rows deleted LATER by hand | untouched — **this is the round-2 fix** |
//! | present         | present         | crash between the global commit and the marker's own clear | self-heal: clear the marker, delete nothing |
//! | present         | absent          | crash between the cron commit and the global commit — the ACTUAL artifact | candidate for deletion                  |
//!
//! Only the last row is even a candidate; on it, the existing row-level
//! guardrail still applies as defense in depth: the job is removed if and
//! only if it is ALSO still DISABLED (`enabled = false` — the state
//! `install_pack` always leaves a fresh row in) and has NEVER fired
//! (`last_run_at IS NULL`). A row that fails either one LOOKS orphaned by the
//! table above but proves a human (or the cron runner) already interacted
//! with it since — the sweep cannot rule out a second, independent
//! explanation from here, so it is left in place and reported as
//! [`SkippedOrphan`] (logged at `warn`) instead of removed. Deleting
//! user-touched state on an inference is worse than leaving an inert,
//! already-disabled row around.

use anyhow::{Context, Result};

use crate::storage::{GlobalDb, ProfileDb, Storage};

/// One cron job the sweep deleted — the crash artifact itself.
#[derive(Debug, Clone, PartialEq)]
pub struct RemovedOrphan {
    pub profile: String,
    pub cron_job_id: String,
    pub cron_job_name: String,
    pub pack_install_id: String,
}

/// One orphan-SHAPED cron job the sweep found but did NOT delete, and why —
/// see the module doc's deletion-rule table and its row-level guardrail.
#[derive(Debug, Clone, PartialEq)]
pub struct SkippedOrphan {
    pub profile: String,
    pub cron_job_id: String,
    pub cron_job_name: String,
    pub pack_install_id: String,
    pub reason: &'static str,
}

/// Summary of one boot-pass run across every profile.
#[derive(Debug, Default, Clone)]
pub struct ReconcileReport {
    pub profiles_scanned: usize,
    pub removed: Vec<RemovedOrphan>,
    pub skipped: Vec<SkippedOrphan>,
    /// `(profile_name, error)` for a profile that failed to open or failed to
    /// reconcile — the pass never aborts the rest of the sweep on one.
    pub profile_errors: Vec<(String, String)>,
}

/// Reconcile ONE already-open profile against `global.db`. Exposed at this
/// granularity so tests can drive it directly with in-memory DBs, no
/// `Storage`/tempdir needed — same shape as
/// `agent::crash_recovery::reconcile_profile_db`.
pub(crate) fn reconcile_profile(
    global: &GlobalDb,
    db: &ProfileDb,
    profile: &str,
) -> Result<(Vec<RemovedOrphan>, Vec<SkippedOrphan>)> {
    let candidates = db
        .pack_cron_orphan_candidates()
        .context("pack-reconcile: listing candidate cron jobs")?;

    let mut removed = Vec::new();
    let mut skipped = Vec::new();
    // Memoize per pack_install_id: a single pack install can register several
    // cron jobs, and every one of them would otherwise re-run the same
    // global.db + marker lookups.
    let mut has_global_rows: std::collections::HashMap<String, bool> =
        std::collections::HashMap::new();
    let mut is_pending: std::collections::HashMap<String, bool> = std::collections::HashMap::new();

    for c in candidates {
        if !has_global_rows.contains_key(&c.pack_install_id) {
            let rows = global
                .count_pack_install_rows(&c.pack_install_id)
                .context("pack-reconcile: checking global.db for the pack's rows")?;
            let pending = db
                .pack_install_is_pending(&c.pack_install_id)
                .context("pack-reconcile: checking the pack-install-pending marker")?;
            // pending + global rows PRESENT: a crash between the global
            // commit and the marker's own clear (see `install_pack`) — the
            // install is fine, only that tiny cleanup write didn't land.
            // Self-heal it here; this was never an orphan.
            if pending && rows > 0 {
                db.clear_pack_install_pending(&c.pack_install_id)
                    .context("pack-reconcile: self-healing a stuck pending marker")?;
            }
            has_global_rows.insert(c.pack_install_id.clone(), rows > 0);
            is_pending.insert(c.pack_install_id.clone(), pending);
        }
        let has_rows = has_global_rows[&c.pack_install_id];
        let pending = is_pending[&c.pack_install_id];

        if has_rows {
            // This install's global transaction committed — healthy, untouched.
            continue;
        }
        if !pending {
            // No global rows AND no pending marker: this install's global
            // transaction committed just fine — the marker was cleared right
            // after — and the rows are gone NOW for a completely unrelated
            // reason (the user deleted the skill/agent type by hand in
            // Settings). Ordinary, supported behavior; never infer a crash
            // from it. THIS is the check the round-2 fix adds — see the
            // module doc's table.
            continue;
        }

        // Only rows that reach here are PROVEN crash artifacts (pending +
        // absent). The row-level guardrail still applies as defense in
        // depth: only a job PROVABLY untouched since install — never
        // enabled, never run — is removed.
        if !c.enabled && c.last_run_at.is_none() {
            db.delete_cron_job(&c.id)
                .context("pack-reconcile: deleting orphaned cron job")?;
            // The marker's job is done along with the row it was protecting;
            // idempotent, so redundant calls for sibling jobs of the same
            // install are harmless no-ops.
            db.clear_pack_install_pending(&c.pack_install_id)
                .context("pack-reconcile: clearing the marker for a deleted orphan")?;
            removed.push(RemovedOrphan {
                profile: profile.to_string(),
                cron_job_id: c.id,
                cron_job_name: c.name,
                pack_install_id: c.pack_install_id,
            });
        } else {
            let reason = if c.enabled {
                "orphaned pack cron job was enabled by the user; left in place"
            } else {
                "orphaned pack cron job has already run at least once; left in place"
            };
            skipped.push(SkippedOrphan {
                profile: profile.to_string(),
                cron_job_id: c.id,
                cron_job_name: c.name,
                pack_install_id: c.pack_install_id,
                reason,
            });
        }
    }
    // GLM review LOW-13: mop up any stranded markers whose cron jobs are gone
    // (an orphan was already deleted above, or the user cleaned up in Settings).
    if let Err(e) = db.clear_stranded_pending_markers() {
        tracing::warn!(profile = %profile, error = %e, "pack-reconcile: could not GC stranded markers");
    }
    Ok((removed, skipped))
}

/// Run the sweep across every profile (wired at boot in `lib.rs`, alongside
/// `agent::crash_recovery::run_boot_pass` and
/// `models::runner::sweep_local_model_integrity_at_boot`). Best-effort per
/// profile — a failure opening or reconciling one profile is logged and
/// skipped, never brick boot.
pub fn run_boot_pass(storage: &Storage) -> Result<ReconcileReport> {
    let mut report = ReconcileReport::default();
    let names = storage
        .list_profile_names()
        .context("pack-reconcile: listing profiles")?;
    let global = storage.global();
    for name in names {
        report.profiles_scanned += 1;
        let db = match storage.open_profile(&name) {
            Ok(db) => db,
            Err(e) => {
                tracing::error!(
                    profile = %name,
                    error = %e,
                    "pack-reconcile: could not open profile; skipping"
                );
                report.profile_errors.push((name, e.to_string()));
                continue;
            }
        };
        match reconcile_profile(global, &db, &name) {
            Ok((removed, skipped)) => {
                report.removed.extend(removed);
                report.skipped.extend(skipped);
            }
            Err(e) => {
                tracing::error!(
                    profile = %name,
                    error = %e,
                    "pack-reconcile: reconciliation failed; skipping profile"
                );
                report.profile_errors.push((name, e.to_string()));
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packs::{install_pack, Pack, PackAgentType, PackCron, PackSkill};
    use crate::storage::Storage;

    fn temp_storage() -> (Storage, std::path::PathBuf) {
        let mut root = std::env::temp_dir();
        root.push(format!("lhp-pack-reconcile-{}", uuid::Uuid::new_v4()));
        (Storage::open(&root).unwrap(), root)
    }

    fn pack_with_everything(name: &str) -> Pack {
        Pack {
            format: 1,
            name: name.to_string(),
            version: "1.0.0".into(),
            description: "spans both databases".into(),
            skills: vec![PackSkill {
                name: "A skill".into(),
                description: String::new(),
                content: "do the thing".into(),
                capabilities_required: vec![],
                version: String::new(),
            }],
            agent_types: vec![PackAgentType {
                name: "A reviewer".into(),
                description: String::new(),
                system_prompt: "review it".into(),
                tools_allowlist: vec![],
                seat: "inherit".into(),
                trigger_examples: vec![],
            }],
            cron_jobs: vec![PackCron {
                name: "A cron job".into(),
                prompt: "run the thing".into(),
                schedule: "0 2 * * *".into(),
            }],
        }
    }

    /// Simulate the exact crash window `install_pack`'s `# Atomicity` note
    /// describes: the cron transaction (INCLUDING the pending marker it
    /// writes in the SAME transaction — see `install_pack`) commits, the
    /// global one never does. Returns the install id the crashed install
    /// used, so a test can assert on it directly.
    fn simulate_crash_after_cron_commit(
        storage: &Storage,
        profile: &str,
        pack: &Pack,
        now: i64,
    ) -> String {
        let install_id = uuid::Uuid::new_v4().to_string();
        let expected_global_rows = (pack.skills.len() + pack.agent_types.len()) as i64;
        let db = storage.open_profile(profile).unwrap();
        {
            let mut conn = db.raw();
            let tx = conn.transaction().unwrap();
            // Same transaction as the cron inserts, exactly like `install_pack`
            // — this is the fact a real crash right after this commit leaves
            // behind on purpose: proof the global half had not landed yet.
            if expected_global_rows > 0 {
                tx.execute(
                    "INSERT INTO pack_install_pending (pack_install_id, created_at) VALUES (?1, ?2)",
                    rusqlite::params![install_id, now],
                )
                .unwrap();
            }
            for c in &pack.cron_jobs {
                tx.execute(
                    "INSERT INTO cron_jobs
                     (id, name, prompt, schedule, enabled, last_run_at,
                      last_status, target_conversation_id, pack_install_id,
                      pack_expected_global_rows)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    rusqlite::params![
                        uuid::Uuid::new_v4().to_string(),
                        c.name,
                        c.prompt,
                        c.schedule,
                        0_i64,
                        None::<i64>,
                        None::<String>,
                        None::<String>,
                        install_id,
                        expected_global_rows,
                    ],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        // The global transaction is deliberately never opened at all — the
        // crash killed the process before step 1 of `install_pack`'s
        // `# Atomicity` sequence had a chance to run, matching the state a
        // real crash right after the cron commit (step 3) leaves behind: no
        // global row was ever written for this pack.
        install_id
    }

    /// Simulate a crash AFTER the global transaction commits but BEFORE
    /// `install_pack`'s own follow-up write clears the pending marker: a
    /// REAL, fully successful install (both commits landed — skills, agent
    /// types, cron jobs all present and correct), with the marker manually
    /// re-inserted afterward to stand in for that missing last write.
    fn simulate_crash_after_global_commit_before_marker_clear(
        storage: &Storage,
        profile: &str,
        pack: &Pack,
        now: i64,
    ) -> String {
        install_pack(storage, profile, pack, now).unwrap();
        let db = storage.open_profile(profile).unwrap();
        let install_id: String = db
            .raw()
            .query_row(
                "SELECT pack_install_id FROM cron_jobs WHERE pack_install_id IS NOT NULL \
                 ORDER BY rowid DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        db.raw()
            .execute(
                "INSERT INTO pack_install_pending (pack_install_id, created_at) VALUES (?1, ?2)",
                rusqlite::params![install_id, now],
            )
            .unwrap();
        install_id
    }

    #[test]
    fn sweep_removes_a_crash_orphaned_cron_job() {
        let (storage, root) = temp_storage();
        let pack = pack_with_everything("Deploy Kit");
        let install_id = simulate_crash_after_cron_commit(&storage, "personal", &pack, 100);

        let db = storage.open_profile("personal").unwrap();
        assert_eq!(
            db.list_cron_jobs().unwrap().len(),
            1,
            "the orphan exists before the sweep"
        );

        let report = run_boot_pass(&storage).unwrap();
        assert_eq!(
            report.removed.len(),
            1,
            "exactly the orphan is removed: {:?}",
            report
        );
        assert_eq!(report.removed[0].pack_install_id, install_id);
        assert!(report.skipped.is_empty());
        assert!(
            db.list_cron_jobs().unwrap().is_empty(),
            "the orphan is gone"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sweep_does_not_touch_a_healthy_packs_cron_jobs() {
        let (storage, root) = temp_storage();
        let pack = pack_with_everything("Deploy Kit");
        // A REAL install — both transactions commit, nothing crashes.
        install_pack(&storage, "personal", &pack, 100).unwrap();

        let report = run_boot_pass(&storage).unwrap();
        assert!(
            report.removed.is_empty(),
            "a healthy pack's jobs must never be touched: {:?}",
            report
        );
        assert!(report.skipped.is_empty());

        let db = storage.open_profile("personal").unwrap();
        assert_eq!(
            db.list_cron_jobs().unwrap().len(),
            1,
            "the healthy job survives the sweep"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sweep_does_not_touch_a_user_created_cron_job() {
        let (storage, root) = temp_storage();
        let db = storage.open_profile("personal").unwrap();
        db.insert_cron_job(&crate::storage::CronJob {
            id: uuid::Uuid::new_v4().to_string(),
            name: "My own reminder".into(),
            prompt: "remind me".into(),
            schedule: "0 9 * * *".into(),
            enabled: false,
            last_run_at: None,
            last_status: None,
            target_conversation_id: None,
        })
        .unwrap();

        let report = run_boot_pass(&storage).unwrap();
        assert!(
            report.removed.is_empty(),
            "a user-created job has no pack_install_id and must never be swept"
        );
        assert!(report.skipped.is_empty());
        assert_eq!(db.list_cron_jobs().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sweep_does_not_touch_an_orphan_the_user_enabled() {
        let (storage, root) = temp_storage();
        let pack = pack_with_everything("Deploy Kit");
        simulate_crash_after_cron_commit(&storage, "personal", &pack, 100);

        let db = storage.open_profile("personal").unwrap();
        let job = db.list_cron_jobs().unwrap().into_iter().next().unwrap();
        // The user reviewed it in Settings and turned it on before the next
        // boot — that is a deliberate action on state that merely LOOKS
        // orphaned; the sweep must not delete it out from under them.
        db.set_cron_job_enabled(&job.id, true).unwrap();

        let report = run_boot_pass(&storage).unwrap();
        assert!(
            report.removed.is_empty(),
            "an enabled job must never be deleted: {:?}",
            report
        );
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].cron_job_id, job.id);
        assert_eq!(db.list_cron_jobs().unwrap().len(), 1, "left in place");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sweep_does_not_touch_an_orphan_that_has_run() {
        let (storage, root) = temp_storage();
        let pack = pack_with_everything("Deploy Kit");
        simulate_crash_after_cron_commit(&storage, "personal", &pack, 100);

        let db = storage.open_profile("personal").unwrap();
        let job = db.list_cron_jobs().unwrap().into_iter().next().unwrap();
        // Still disabled, but it fired at least once — `last_run_at` is set.
        // That is evidence someone/something interacted with this job; the
        // conservative rule requires BOTH disabled AND never-run.
        db.record_cron_run(&job.id, "ok").unwrap();

        let report = run_boot_pass(&storage).unwrap();
        assert!(
            report.removed.is_empty(),
            "a job that has run must never be deleted: {:?}",
            report
        );
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(db.list_cron_jobs().unwrap().len(), 1, "left in place");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sweep_does_not_touch_a_cron_only_packs_jobs() {
        // A pack with ZERO skills/agent types never opens a global
        // transaction in `install_pack` — there is no atomicity window, so
        // `pack_expected_global_rows` is stamped 0 and the sweep must treat
        // these jobs as healthy even though global.db never has a matching row.
        let (storage, root) = temp_storage();
        let pack = Pack {
            format: 1,
            name: "Cron Only".into(),
            version: "1.0.0".into(),
            description: String::new(),
            skills: vec![],
            agent_types: vec![],
            cron_jobs: vec![PackCron {
                name: "Just a cron job".into(),
                prompt: "run it".into(),
                schedule: "0 3 * * *".into(),
            }],
        };
        install_pack(&storage, "personal", &pack, 100).unwrap();

        let report = run_boot_pass(&storage).unwrap();
        assert!(
            report.removed.is_empty(),
            "a cron-only pack has no atomicity window to reconcile: {:?}",
            report
        );
        assert!(report.skipped.is_empty());

        let db = storage.open_profile("personal").unwrap();
        assert_eq!(
            db.list_cron_jobs().unwrap().len(),
            1,
            "the cron-only pack's job survives"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sweep_is_idempotent_across_two_boots() {
        let (storage, root) = temp_storage();
        let pack = pack_with_everything("Deploy Kit");
        simulate_crash_after_cron_commit(&storage, "personal", &pack, 100);

        let first = run_boot_pass(&storage).unwrap();
        assert_eq!(first.removed.len(), 1);

        // Second boot: nothing left to remove, and re-running must not error
        // or touch anything else (e.g. a healthy job installed in between).
        let healthy_pack = pack_with_everything("Second Pack");
        install_pack(&storage, "personal", &healthy_pack, 200).unwrap();

        let second = run_boot_pass(&storage).unwrap();
        assert!(
            second.removed.is_empty(),
            "nothing left to remove on the second boot: {:?}",
            second
        );
        assert!(second.skipped.is_empty());

        let db = storage.open_profile("personal").unwrap();
        assert_eq!(
            db.list_cron_jobs().unwrap().len(),
            1,
            "only the healthy pack's job remains"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    // ── round 2: the adversarial-review fix ─────────────────────────────────

    #[test]
    fn sweep_does_not_touch_a_healthy_install_whose_skill_was_later_deleted_by_hand() {
        // THE BUG the round-1 sweep had: a healthy install (both transactions
        // commit fine) whose skill + agent type the user deletes by hand in
        // Settings, months later, via the real unconditional delete methods —
        // no relationship to `pack_install_id` at all. That leaves the cron
        // job pack-installed + disabled + never-run + zero matching global
        // rows: identical, by row count alone, to a genuine crash artifact.
        // The pending marker is what tells them apart — it was cleared right
        // after the original install succeeded, so it can never come back.
        let (storage, root) = temp_storage();
        let pack = pack_with_everything("Deploy Kit");
        install_pack(&storage, "personal", &pack, 100).unwrap();

        let g = storage.global();
        let skill_id = g.list_skills().unwrap()[0].id.clone();
        let at_id = g.list_agent_types().unwrap()[0].id.clone();
        assert!(
            g.delete_skill(&skill_id).unwrap(),
            "the skill existed to delete"
        );
        assert!(
            g.delete_agent_type(&at_id).unwrap(),
            "the agent type existed to delete"
        );
        assert_eq!(g.list_skills().unwrap().len(), 0);
        assert_eq!(g.list_agent_types().unwrap().len(), 0);

        let db = storage.open_profile("personal").unwrap();
        assert_eq!(
            db.list_cron_jobs().unwrap().len(),
            1,
            "the cron job is untouched by the Settings deletes"
        );

        let report = run_boot_pass(&storage).unwrap();
        assert!(
            report.removed.is_empty(),
            "a later, unrelated deletion must NEVER be mistaken for a crash: {:?}",
            report
        );
        assert!(
            report.skipped.is_empty(),
            "this isn't even orphan-SHAPED to the sweep — no pending marker, so it's \
             not a candidate at all: {:?}",
            report
        );
        assert_eq!(
            db.list_cron_jobs().unwrap().len(),
            1,
            "the cron job survives — this is the data-loss bug the marker fixes"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sweep_clears_a_marker_stuck_pending_after_a_successful_global_commit() {
        // Crash between the global commit landing and `install_pack`'s own
        // follow-up write to clear the marker: the install is completely
        // healthy (skills, agent types, cron job — all present and correct),
        // only that last tiny cleanup write never ran. The sweep must self-heal
        // by clearing the marker and touch NOTHING else.
        let (storage, root) = temp_storage();
        let pack = pack_with_everything("Deploy Kit");
        let install_id = simulate_crash_after_global_commit_before_marker_clear(
            &storage, "personal", &pack, 100,
        );

        let db = storage.open_profile("personal").unwrap();
        assert!(
            db.pack_install_is_pending(&install_id).unwrap(),
            "the marker is stuck pending before the sweep runs"
        );

        let report = run_boot_pass(&storage).unwrap();
        assert!(
            report.removed.is_empty(),
            "global rows are present — nothing here is an orphan: {:?}",
            report
        );
        assert!(report.skipped.is_empty());

        assert!(
            !db.pack_install_is_pending(&install_id).unwrap(),
            "the sweep must self-heal the stuck marker"
        );
        assert_eq!(
            db.list_cron_jobs().unwrap().len(),
            1,
            "the healthy job is untouched"
        );
        let g = storage.global();
        assert_eq!(
            g.list_skills().unwrap().len(),
            1,
            "the healthy skill is untouched"
        );
        assert_eq!(
            g.list_agent_types().unwrap().len(),
            1,
            "the healthy agent type is untouched"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn round_2_scenarios_are_idempotent_across_two_boots() {
        let (storage, root) = temp_storage();

        // Scenario 1: a healthy install later hand-edited in Settings.
        let edited_pack = pack_with_everything("Edited Later");
        install_pack(&storage, "personal", &edited_pack, 100).unwrap();
        let g = storage.global();
        let edited_skill_id = g
            .list_skills()
            .unwrap()
            .iter()
            .find(|s| s.name == "A skill")
            .unwrap()
            .id
            .clone();
        g.delete_skill(&edited_skill_id).unwrap();

        // Scenario 2: a genuine crash artifact.
        let crashed_pack = pack_with_everything("Crashed");
        let crashed_install_id =
            simulate_crash_after_cron_commit(&storage, "personal", &crashed_pack, 200);

        // Scenario 3: a marker stuck pending after a successful commit.
        let stuck_pack = pack_with_everything("Stuck Marker");
        let stuck_install_id = simulate_crash_after_global_commit_before_marker_clear(
            &storage,
            "personal",
            &stuck_pack,
            300,
        );

        let db = storage.open_profile("personal").unwrap();
        assert_eq!(
            db.list_cron_jobs().unwrap().len(),
            3,
            "all three jobs exist pre-sweep"
        );

        let first = run_boot_pass(&storage).unwrap();
        assert_eq!(
            first.removed.len(),
            1,
            "only the genuine crash artifact is removed on the first boot: {:?}",
            first
        );
        assert_eq!(first.removed[0].pack_install_id, crashed_install_id);
        assert!(!db.pack_install_is_pending(&stuck_install_id).unwrap());

        let jobs_after_first = db.list_cron_jobs().unwrap();
        assert_eq!(
            jobs_after_first.len(),
            2,
            "the edited-later job and the stuck-marker job both survive"
        );

        let second = run_boot_pass(&storage).unwrap();
        assert!(
            second.removed.is_empty(),
            "nothing left to remove on the second boot: {:?}",
            second
        );
        assert!(second.skipped.is_empty());
        assert_eq!(
            db.list_cron_jobs().unwrap().len(),
            2,
            "the second boot is a pure no-op"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
