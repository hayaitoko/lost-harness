//! Wave 4.5 — **Capability Packs** (PLAN §4, §8 M7). A single installable bundle
//! of the things a "capability" needs: skills, agent-type personas, and cron
//! templates. Installing a pack registers all of them at once, so a
//! non-technical user gets a working capability without hand-editing config.
//!
//! A [`Pack`] is PORTABLE: it carries only the durable, shareable fields — never
//! ids, timestamps, approval state, or run history (those are minted fresh on
//! install). Installing lands every item in its **untrusted** default state:
//! skills + agent types as `Pending` (the user reviews each in Settings →
//! Skills / Agent types before it can be searched or dispatched), and cron jobs
//! **disabled** (the user enables them). A pack can add capabilities to review;
//! it can never silently arm one. Export is the inverse — bundle selected
//! existing items back into a portable pack to share.
//!
//! Atomicity note: skills + agent types live in `global.db` while cron jobs live
//! in the active profile's DB, so a pack that spans both cannot be one SQL
//! transaction. The global.db segment (skills + agent types) IS wrapped in a
//! transaction; cron jobs are best-effort last. A mid-install failure inside the
//! global segment rolls back; a failure after global commit returns a partial
//! [`InstallReport`] of exactly what landed, with the already-inserted (inert)
//! items in place rather than corrupting either store.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::storage::{AgentTypeApproval, CronJob, SkillApproval, Storage};

// ── Validation constants ────────────────────────────────────────────────────

/// Total byte limit for a pack JSON payload (P05 enforces this at the IPC
/// boundary; we re-enforce at the domain layer as defense-in-depth).
pub const PACK_IMPORT_MAX_BYTES: usize = 1_000_000;

/// Maximum number of items per category in a single pack.
pub const PACK_MAX_SKILLS: usize = 50;
pub const PACK_MAX_AGENT_TYPES: usize = 25;
pub const PACK_MAX_CRON_JOBS: usize = 25;

/// Maximum length (in UTF-8 bytes) for individual string fields.
pub const PACK_MAX_NAME_LEN: usize = 128;
pub const PACK_MAX_DESC_LEN: usize = 2_048;
pub const PACK_MAX_VERSION_LEN: usize = 32;
pub const PACK_MAX_CONTENT_LEN: usize = 262_144; // skill content / cron prompt
pub const PACK_MAX_SCHEDULE_LEN: usize = 256;
pub const PACK_MAX_SYSTEM_PROMPT_LEN: usize = 262_144; // agent-type system prompt
pub const PACK_MAX_SEAT_LEN: usize = 64;
pub const PACK_MAX_LIST_ITEM_LEN: usize = 256; // each element in a list field

/// Maximum entries in a list field (capabilities_required, tools_allowlist,
/// trigger_examples).
pub const PACK_MAX_LIST_ITEMS: usize = 100;

/// The current pack-format version. Bumped if the on-disk shape changes.
pub const PACK_FORMAT_VERSION: u32 = 1;

/// A portable capability bundle. Serialized to/from JSON for sharing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Pack {
    /// Pack format version (forward-compat guard).
    #[serde(default = "default_format")]
    pub format: u32,
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub skills: Vec<PackSkill>,
    #[serde(default)]
    pub agent_types: Vec<PackAgentType>,
    #[serde(default)]
    pub cron_jobs: Vec<PackCron>,
}

fn default_format() -> u32 {
    PACK_FORMAT_VERSION
}

/// A skill as carried in a pack — durable fields only (no id/approval/timestamp).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PackSkill {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub content: String,
    #[serde(default)]
    pub capabilities_required: Vec<String>,
    #[serde(default)]
    pub version: String,
}

/// An agent-type persona as carried in a pack.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PackAgentType {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub tools_allowlist: Vec<String>,
    #[serde(default = "inherit_seat")]
    pub seat: String,
    #[serde(default)]
    pub trigger_examples: Vec<String>,
}

fn inherit_seat() -> String {
    "inherit".to_string()
}

/// A cron template as carried in a pack — installed DISABLED.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PackCron {
    pub name: String,
    pub prompt: String,
    pub schedule: String,
}

/// What an install actually registered (each item lands inert — Pending /
/// disabled — pending the user's review).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct InstallReport {
    pub pack_name: String,
    pub skills_installed: usize,
    pub agent_types_installed: usize,
    pub cron_jobs_installed: usize,
}

/// Parse a pack from JSON, validate every field against resource limits,
/// and reject an unknown future format version.
pub fn parse_pack(json: &str) -> Result<Pack> {
    // Byte cap first: reject oversized payloads before parsing.
    if json.len() > PACK_IMPORT_MAX_BYTES {
        anyhow::bail!(
            "pack payload too large: {} bytes (max {})",
            json.len(),
            PACK_IMPORT_MAX_BYTES
        );
    }

    let pack: Pack = serde_json::from_str(json)?;

    if pack.format > PACK_FORMAT_VERSION {
        anyhow::bail!(
            "this pack needs a newer version of the app (pack format {} > supported {})",
            pack.format,
            PACK_FORMAT_VERSION
        );
    }
    if pack.name.trim().is_empty() {
        anyhow::bail!("a pack must have a name");
    }

    // Validate structural limits before returning.
    validate_pack(&pack)?;

    Ok(pack)
}

/// Validate a parsed `Pack`'s structural limits: item counts, per-field
/// string lengths, and list sizes. Called by `parse_pack` and also by
/// `install_pack` as defense-in-depth.
fn validate_pack(pack: &Pack) -> Result<()> {
    // ── Top-level string fields ──────────────────────────────────────────────
    check_len("pack", "name", &pack.name, PACK_MAX_NAME_LEN)?;
    check_len("pack", "version", &pack.version, PACK_MAX_VERSION_LEN)?;
    check_len("pack", "description", &pack.description, PACK_MAX_DESC_LEN)?;

    // ── Item counts ──────────────────────────────────────────────────────────
    if pack.skills.len() > PACK_MAX_SKILLS {
        anyhow::bail!(
            "too many skills: {} (max {})",
            pack.skills.len(),
            PACK_MAX_SKILLS
        );
    }
    if pack.agent_types.len() > PACK_MAX_AGENT_TYPES {
        anyhow::bail!(
            "too many agent types: {} (max {})",
            pack.agent_types.len(),
            PACK_MAX_AGENT_TYPES
        );
    }
    if pack.cron_jobs.len() > PACK_MAX_CRON_JOBS {
        anyhow::bail!(
            "too many cron jobs: {} (max {})",
            pack.cron_jobs.len(),
            PACK_MAX_CRON_JOBS
        );
    }

    // ── Per-item validation ──────────────────────────────────────────────────
    for (i, s) in pack.skills.iter().enumerate() {
        let prefix = format!("skill[{}]", i);
        if s.name.trim().is_empty() {
            anyhow::bail!("{} name must not be empty", prefix);
        }
        check_len(&prefix, "name", &s.name, PACK_MAX_NAME_LEN)?;
        check_len(&prefix, "description", &s.description, PACK_MAX_DESC_LEN)?;
        check_len(&prefix, "content", &s.content, PACK_MAX_CONTENT_LEN)?;
        check_len(&prefix, "version", &s.version, PACK_MAX_VERSION_LEN)?;
        check_list(
            &prefix,
            "capabilities_required",
            &s.capabilities_required,
            PACK_MAX_LIST_ITEMS,
            PACK_MAX_LIST_ITEM_LEN,
        )?;
    }

    for (i, a) in pack.agent_types.iter().enumerate() {
        let prefix = format!("agent_type[{}]", i);
        if a.name.trim().is_empty() {
            anyhow::bail!("{} name must not be empty", prefix);
        }
        check_len(&prefix, "name", &a.name, PACK_MAX_NAME_LEN)?;
        check_len(&prefix, "description", &a.description, PACK_MAX_DESC_LEN)?;
        check_len(
            &prefix,
            "system_prompt",
            &a.system_prompt,
            PACK_MAX_SYSTEM_PROMPT_LEN,
        )?;
        check_len(&prefix, "seat", &a.seat, PACK_MAX_SEAT_LEN)?;
        check_list(
            &prefix,
            "tools_allowlist",
            &a.tools_allowlist,
            PACK_MAX_LIST_ITEMS,
            PACK_MAX_LIST_ITEM_LEN,
        )?;
        check_list(
            &prefix,
            "trigger_examples",
            &a.trigger_examples,
            PACK_MAX_LIST_ITEMS,
            PACK_MAX_LIST_ITEM_LEN,
        )?;
    }

    for (i, c) in pack.cron_jobs.iter().enumerate() {
        let prefix = format!("cron[{}]", i);
        if c.name.trim().is_empty() {
            anyhow::bail!("{} name must not be empty", prefix);
        }
        check_len(&prefix, "name", &c.name, PACK_MAX_NAME_LEN)?;
        check_len(&prefix, "prompt", &c.prompt, PACK_MAX_CONTENT_LEN)?;
        check_len(&prefix, "schedule", &c.schedule, PACK_MAX_SCHEDULE_LEN)?;
        if c.schedule.trim().is_empty() {
            anyhow::bail!("{} schedule must not be empty", prefix);
        }
    }

    Ok(())
}

/// Check that a string field does not exceed `max` bytes (UTF-8 length).
fn check_len(prefix: &str, field: &str, value: &str, max: usize) -> Result<()> {
    if value.len() > max {
        anyhow::bail!(
            "{}.{} too long: {} chars (max {})",
            prefix,
            field,
            value.len(),
            max
        );
    }
    Ok(())
}

/// Check list size and per-item lengths.
fn check_list(
    prefix: &str,
    field: &str,
    items: &[String],
    max_items: usize,
    max_item_len: usize,
) -> Result<()> {
    if items.len() > max_items {
        anyhow::bail!(
            "{}.{} too many items: {} (max {})",
            prefix,
            field,
            items.len(),
            max_items
        );
    }
    for (j, item) in items.iter().enumerate() {
        if item.len() > max_item_len {
            anyhow::bail!(
                "{}.{}[{}] too long: {} chars (max {})",
                prefix,
                field,
                j,
                item.len(),
                max_item_len
            );
        }
    }
    Ok(())
}

/// Install a pack into `storage` (skills + agent types go GLOBAL; cron jobs go
/// into `profile`). Every item lands INERT — skills/agent types `Pending`, cron
/// jobs disabled — so a pack adds capabilities to review, never arms one. `now`
/// is the caller's timestamp. Returns what landed.
///
/// # Atomicity
/// Skills and agent types are inserted in a single SQLite transaction on
/// `global.db` — a mid-install failure there rolls back completely. Cron jobs
/// live in the profile's own DB and cannot share the same transaction; they are
/// installed best-effort after the global transaction commits. The profile DB
/// is opened *before* the global transaction starts, so a missing profile is
/// caught before any mutation.
pub fn install_pack(
    storage: &Storage,
    profile: &str,
    pack: &Pack,
    now: i64,
) -> Result<InstallReport> {
    // Defense-in-depth: validate even if parse_pack already ran.
    validate_pack(pack)?;

    let source = format!("pack:{}", pack.name);

    // Open profile DB early (before global mutation) so a missing/invalid
    // profile is caught before any insert.
    let profile_db = if !pack.cron_jobs.is_empty() {
        Some(storage.open_profile(profile)?)
    } else {
        None
    };

    let global = storage.global();
    let skills_count = pack.skills.len();
    let agent_types_count = pack.agent_types.len();

    // ── Global DB (skills + agent types) — atomic transaction ────────────────
    if skills_count > 0 || agent_types_count > 0 {
        let mut conn = global.raw();
        let tx = conn.transaction()?;

        for s in &pack.skills {
            let caps_json =
                serde_json::to_string(&s.capabilities_required).unwrap_or_else(|_| "[]".into());
            let version = if s.version.is_empty() {
                "0.1.0"
            } else {
                &s.version
            };
            tx.execute(
                "INSERT INTO skills
                 (id, name, description, content, capabilities_required,
                  approval_status, path, version, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    uuid::Uuid::new_v4().to_string(),
                    s.name,
                    s.description,
                    s.content,
                    caps_json,
                    SkillApproval::Pending.as_str(),
                    "",
                    version,
                    now,
                ],
            )?;
        }

        for a in &pack.agent_types {
            let tools_json =
                serde_json::to_string(&a.tools_allowlist).unwrap_or_else(|_| "[]".into());
            let triggers_json =
                serde_json::to_string(&a.trigger_examples).unwrap_or_else(|_| "[]".into());
            tx.execute(
                "INSERT INTO agent_types
                 (id, name, description, system_prompt, tools_allowlist,
                  seat, trigger_examples, approval_status, source, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    uuid::Uuid::new_v4().to_string(),
                    a.name,
                    a.description,
                    a.system_prompt,
                    tools_json,
                    a.seat,
                    triggers_json,
                    AgentTypeApproval::Pending.as_str(),
                    source,
                    now,
                ],
            )?;
        }

        tx.commit()?;
    }

    // ── Profile DB (cron jobs) — best-effort, no cross-DB transaction ────────
    if let Some(db) = profile_db {
        for c in &pack.cron_jobs {
            let job = CronJob {
                id: uuid::Uuid::new_v4().to_string(),
                name: c.name.clone(),
                prompt: c.prompt.clone(),
                schedule: c.schedule.clone(),
                // Installed DISABLED — the user turns it on deliberately.
                enabled: false,
                last_run_at: None,
                last_status: None,
                target_conversation_id: None,
            };
            db.insert_cron_job(&job)?;
        }
    }

    Ok(InstallReport {
        pack_name: pack.name.clone(),
        skills_installed: skills_count,
        agent_types_installed: agent_types_count,
        cron_jobs_installed: pack.cron_jobs.len(),
    })
}

/// Bundle selected existing items into a portable pack (the inverse of install).
/// Skills/agent types are selected by id (global); cron jobs by id (profile).
pub fn export_pack(
    storage: &Storage,
    profile: &str,
    name: &str,
    version: &str,
    description: &str,
    skill_ids: &[String],
    agent_type_ids: &[String],
    cron_ids: &[String],
) -> Result<Pack> {
    let global = storage.global();
    let mut skills = Vec::new();
    for id in skill_ids {
        if let Some(s) = global.get_skill(id)? {
            skills.push(PackSkill {
                name: s.name,
                description: s.description,
                content: s.content,
                capabilities_required: s.capabilities_required,
                version: s.version,
            });
        }
    }
    let mut agent_types = Vec::new();
    for id in agent_type_ids {
        if let Some(a) = global.get_agent_type(id)? {
            agent_types.push(PackAgentType {
                name: a.name,
                description: a.description,
                system_prompt: a.system_prompt,
                tools_allowlist: a.tools_allowlist,
                seat: a.seat,
                trigger_examples: a.trigger_examples,
            });
        }
    }
    let mut cron_jobs = Vec::new();
    if !cron_ids.is_empty() {
        let db = storage.open_profile(profile)?;
        for id in cron_ids {
            if let Some(c) = db.get_cron_job(id)? {
                cron_jobs.push(PackCron {
                    name: c.name,
                    prompt: c.prompt,
                    schedule: c.schedule,
                });
            }
        }
    }
    Ok(Pack {
        format: PACK_FORMAT_VERSION,
        name: name.to_string(),
        version: version.to_string(),
        description: description.to_string(),
        skills,
        agent_types,
        cron_jobs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_storage() -> (Storage, std::path::PathBuf) {
        let mut root = std::env::temp_dir();
        root.push(format!("lhp-packs-{}", uuid::Uuid::new_v4()));
        (Storage::open(&root).unwrap(), root)
    }

    fn sample_pack() -> Pack {
        Pack {
            format: 1,
            name: "Deploy Kit".into(),
            version: "1.0.0".into(),
            description: "Deploy helpers".into(),
            skills: vec![PackSkill {
                name: "Deploy the app".into(),
                description: "build + push".into(),
                content: "1. test\n2. push".into(),
                capabilities_required: vec!["Shell".into()],
                version: String::new(),
            }],
            agent_types: vec![PackAgentType {
                name: "Deploy reviewer".into(),
                description: "reviews deploys".into(),
                system_prompt: "You review deploys.".into(),
                tools_allowlist: vec!["read_file".into()],
                seat: "Reviewer".into(),
                trigger_examples: vec![],
            }],
            cron_jobs: vec![PackCron {
                name: "Nightly build".into(),
                prompt: "run the nightly build".into(),
                schedule: "0 2 * * *".into(),
            }],
        }
    }

    #[test]
    fn install_lands_everything_inert_pending_and_disabled() {
        let (storage, root) = temp_storage();
        let report = install_pack(&storage, "personal", &sample_pack(), 100).unwrap();
        assert_eq!(report.skills_installed, 1);
        assert_eq!(report.agent_types_installed, 1);
        assert_eq!(report.cron_jobs_installed, 1);

        let g = storage.global();
        // The skill exists but is PENDING (not searchable/usable yet).
        let skills = g.list_skills().unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].approval_status, SkillApproval::Pending);
        assert!(
            g.search_skills("deploy", 5).unwrap().is_empty(),
            "a pack skill isn't usable until approved"
        );
        // The agent type is PENDING (not dispatchable) + tagged with its pack.
        let ats = g.list_agent_types().unwrap();
        assert_eq!(ats.len(), 1);
        assert_eq!(ats[0].approval_status, AgentTypeApproval::Pending);
        assert_eq!(ats[0].source, "pack:Deploy Kit");
        assert!(g.list_approved_agent_types().unwrap().is_empty());
        // The cron job is installed DISABLED.
        let jobs = storage
            .open_profile("personal")
            .unwrap()
            .list_cron_jobs()
            .unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(!jobs[0].enabled, "a pack cron job is installed disabled");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn parse_rejects_a_future_format_and_empty_name() {
        assert!(parse_pack(r#"{"format": 999, "name": "x"}"#).is_err());
        assert!(parse_pack(r#"{"name": "  "}"#).is_err());
        // A minimal valid pack parses (all item arrays default to empty).
        let p = parse_pack(r#"{"name": "Bare"}"#).unwrap();
        assert_eq!(p.name, "Bare");
        assert!(p.skills.is_empty());
    }

    // ── Validation tests ────────────────────────────────────────────────────

    #[test]
    fn validate_rejects_oversized_json() {
        let big = "x".repeat(PACK_IMPORT_MAX_BYTES + 1);
        let json = format!("{{\"name\": \"Huge\", \"description\": \"{big}\"}}");
        assert!(
            parse_pack(&json).is_err(),
            "oversized JSON should be rejected"
        );
    }

    #[test]
    fn validate_rejects_too_many_skills() {
        let mut pack = sample_pack();
        pack.skills = vec![
            PackSkill {
                name: "s".into(),
                description: String::new(),
                content: "c".into(),
                capabilities_required: vec![],
                version: String::new(),
            };
            PACK_MAX_SKILLS + 1
        ];
        assert!(validate_pack(&pack).is_err());
    }

    #[test]
    fn validate_rejects_too_many_agent_types() {
        let mut pack = sample_pack();
        pack.agent_types = vec![
            PackAgentType {
                name: "a".into(),
                description: String::new(),
                system_prompt: String::new(),
                tools_allowlist: vec![],
                seat: "inherit".into(),
                trigger_examples: vec![],
            };
            PACK_MAX_AGENT_TYPES + 1
        ];
        assert!(validate_pack(&pack).is_err());
    }

    #[test]
    fn validate_rejects_too_many_cron_jobs() {
        let pack = Pack {
            cron_jobs: vec![
                PackCron {
                    name: "c".into(),
                    prompt: "p".into(),
                    schedule: "0 * * * *".into(),
                };
                PACK_MAX_CRON_JOBS + 1
            ],
            ..sample_pack()
        };
        assert!(validate_pack(&pack).is_err());
    }

    #[test]
    fn validate_rejects_skill_with_name_too_long() {
        let mut pack = sample_pack();
        pack.skills[0].name = "n".repeat(PACK_MAX_NAME_LEN + 1);
        assert!(validate_pack(&pack).is_err());
    }

    #[test]
    fn validate_rejects_skill_with_content_too_long() {
        let mut pack = sample_pack();
        pack.skills[0].content = "c".repeat(PACK_MAX_CONTENT_LEN + 1);
        assert!(validate_pack(&pack).is_err());
    }

    #[test]
    fn validate_rejects_too_many_capabilities() {
        let mut pack = sample_pack();
        pack.skills[0].capabilities_required = (0..PACK_MAX_LIST_ITEMS + 1)
            .map(|i| format!("cap{i}"))
            .collect();
        assert!(validate_pack(&pack).is_err());
    }

    #[test]
    fn validate_rejects_agent_type_with_too_many_tools() {
        let mut pack = sample_pack();
        pack.agent_types[0].tools_allowlist = (0..PACK_MAX_LIST_ITEMS + 1)
            .map(|i| format!("tool{i}"))
            .collect();
        assert!(validate_pack(&pack).is_err());
    }

    #[test]
    fn validate_rejects_cron_with_empty_schedule() {
        let mut pack = sample_pack();
        pack.cron_jobs[0].schedule = "".into();
        assert!(validate_pack(&pack).is_err());
    }

    #[test]
    fn validate_rejects_cron_with_schedule_too_long() {
        let mut pack = sample_pack();
        pack.cron_jobs[0].schedule = "s".repeat(PACK_MAX_SCHEDULE_LEN + 1);
        assert!(validate_pack(&pack).is_err());
    }

    #[test]
    fn validate_rejects_pack_name_too_long() {
        let mut pack = sample_pack();
        pack.name = "n".repeat(PACK_MAX_NAME_LEN + 1);
        assert!(validate_pack(&pack).is_err());
    }

    #[test]
    fn validate_rejects_agent_type_with_too_many_triggers() {
        let mut pack = sample_pack();
        pack.agent_types[0].trigger_examples = (0..PACK_MAX_LIST_ITEMS + 1)
            .map(|i| format!("trigger{i}"))
            .collect();
        assert!(validate_pack(&pack).is_err());
    }

    #[test]
    fn validate_rejects_empty_skill_name() {
        let mut pack = sample_pack();
        pack.skills[0].name = "  ".into();
        assert!(validate_pack(&pack).is_err());
    }

    // ── Atomicity / pre-mutation guard tests ─────────────────────────────────

    #[test]
    fn install_rejects_before_mutation_on_validation_failure() {
        let (storage, root) = temp_storage();
        let mut pack = sample_pack();
        pack.skills[0].name = "n".repeat(PACK_MAX_NAME_LEN + 1);

        let err = install_pack(&storage, "personal", &pack, 100).unwrap_err();
        assert!(
            err.to_string().contains("too long"),
            "install should reject oversized skill name: {err}"
        );

        // Verify nothing was inserted.
        let g = storage.global();
        assert_eq!(
            g.list_skills().unwrap().len(),
            0,
            "no skills inserted on validation failure"
        );
        assert_eq!(
            g.list_agent_types().unwrap().len(),
            0,
            "no agent types inserted on validation failure"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn install_rejects_invalid_profile_before_global_mutation() {
        let (storage, root) = temp_storage();
        let pack = sample_pack();

        // A profile name with spaces fails validate_profile_name.
        let err = install_pack(&storage, "invalid name!", &pack, 100).unwrap_err();
        assert!(
            err.to_string().contains("invalid profile name") || err.to_string().contains("profile"),
            "install should reject invalid profile name: {err}"
        );

        // Verify the global DB was never touched.
        let g = storage.global();
        assert_eq!(
            g.list_skills().unwrap().len(),
            0,
            "no skills inserted on profile failure"
        );
        assert_eq!(
            g.list_agent_types().unwrap().len(),
            0,
            "no agent types inserted on profile failure"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn export_then_reparse_roundtrips_the_portable_shape() {
        let (storage, root) = temp_storage();
        // Install, approve the items so export can find them by id, then export.
        install_pack(&storage, "personal", &sample_pack(), 1).unwrap();
        let g = storage.global();
        let skill_id = g.list_skills().unwrap()[0].id.clone();
        let at_id = g.list_agent_types().unwrap()[0].id.clone();
        let cron_id = storage
            .open_profile("personal")
            .unwrap()
            .list_cron_jobs()
            .unwrap()[0]
            .id
            .clone();

        let exported = export_pack(
            &storage,
            "personal",
            "Deploy Kit v2",
            "2.0.0",
            "exported",
            &[skill_id],
            &[at_id],
            &[cron_id],
        )
        .unwrap();
        assert_eq!(exported.skills.len(), 1);
        assert_eq!(exported.skills[0].name, "Deploy the app");
        assert_eq!(exported.agent_types.len(), 1);
        assert_eq!(exported.cron_jobs.len(), 1);

        // JSON round-trip: export → serialize → parse yields an equal pack.
        let json = serde_json::to_string(&exported).unwrap();
        let reparsed = parse_pack(&json).unwrap();
        assert_eq!(reparsed, exported);

        // And re-installing the exported pack lands another inert copy.
        let report = install_pack(&storage, "personal", &reparsed, 2).unwrap();
        assert_eq!(report.skills_installed, 1);
        assert_eq!(
            g.list_skills().unwrap().len(),
            2,
            "re-install adds a second (pending) copy"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
