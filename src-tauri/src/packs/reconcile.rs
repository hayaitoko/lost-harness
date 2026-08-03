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
//! # The deletion rule — read this before touching it
//! A cron job is removed if and only if ALL of:
//! 1. it was installed by a pack (`pack_install_id IS NOT NULL`);
//! 2. that install ALSO wrote to `global.db` (`pack_expected_global_rows >
//!    0`) — a cron-only pack has nothing to reconcile against, and is never a
//!    candidate (`ProfileDb::pack_cron_orphan_candidates` filters it out);
//! 3. NONE of that install's rows exist in `global.db`
//!    (`GlobalDb::count_pack_install_rows` == 0). `install_pack`'s global
//!    transaction is atomic, so "zero rows" can only mean "never
//!    committed" — never "partially committed" — which is exactly the crash
//!    this sweep targets;
//! 4. the job is still DISABLED (`enabled = false`) — the state
//!    `install_pack` always leaves a fresh cron row in;
//! 5. the job has NEVER fired (`last_run_at IS NULL`).
//!
//! (4) and (5) are the conservative guardrail. A row that satisfies (1)-(3)
//! but fails either one LOOKS orphaned to this sweep, but the fact that it is
//! enabled or has already run proves a human (or the cron runner) already
//! interacted with it — the sweep cannot tell from here whether that is a
//! false positive (its global rows were legitimately removed later, e.g. the
//! user rejected + deleted the skill by hand) or something else entirely.
//! Deleting user-touched state on an inference is worse than leaving an
//! inert, already-disabled row around, so those are left in place and
//! reported as [`SkippedOrphan`] instead of removed — logged at `warn`, never
//! silently dropped.

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
/// see the module doc's deletion rule, points (4)/(5).
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
    // global.db lookup.
    let mut global_has_rows: std::collections::HashMap<String, bool> =
        std::collections::HashMap::new();

    for c in candidates {
        let has_rows = match global_has_rows.get(&c.pack_install_id) {
            Some(v) => *v,
            None => {
                let n = global
                    .count_pack_install_rows(&c.pack_install_id)
                    .context("pack-reconcile: checking global.db for the pack's rows")?;
                let has_rows = n > 0;
                global_has_rows.insert(c.pack_install_id.clone(), has_rows);
                has_rows
            }
        };
        if has_rows {
            // This install's global transaction committed — healthy, untouched.
            continue;
        }

        // THE DELETION RULE (module doc): only a row PROVABLY the crash
        // artifact — never enabled, never run — is removed.
        if !c.enabled && c.last_run_at.is_none() {
            db.delete_cron_job(&c.id)
                .context("pack-reconcile: deleting orphaned cron job")?;
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
    /// describes: the cron transaction commits, the global one never does.
    /// Returns the install id the crashed install used, so a test can assert
    /// on it directly.
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
        // global row was ever written for this pack, but `now` is otherwise
        // unused here — accept it for symmetry with `install_pack`'s signature.
        let _ = now;
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
}
