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
//! transaction. Install is therefore best-effort-sequential and returns an
//! [`InstallReport`] of exactly what landed; a mid-install failure leaves the
//! already-inserted (inert) items in place rather than corrupting either store.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::storage::{
    AgentType, AgentTypeApproval, CronJob, Skill, SkillApproval, Storage,
};

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

/// Parse a pack from JSON, rejecting an unknown future format version.
pub fn parse_pack(json: &str) -> Result<Pack> {
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
    Ok(pack)
}

/// Install a pack into `storage` (skills + agent types go GLOBAL; cron jobs go
/// into `profile`). Every item lands INERT — skills/agent types `Pending`, cron
/// jobs disabled — so a pack adds capabilities to review, never arms one. `now`
/// is the caller's timestamp. Returns what landed.
pub fn install_pack(
    storage: &Storage,
    profile: &str,
    pack: &Pack,
    now: i64,
) -> Result<InstallReport> {
    let source = format!("pack:{}", pack.name);
    let global = storage.global();
    let mut report = InstallReport {
        pack_name: pack.name.clone(),
        ..Default::default()
    };

    for s in &pack.skills {
        let skill = Skill {
            id: uuid::Uuid::new_v4().to_string(),
            name: s.name.clone(),
            description: s.description.clone(),
            content: s.content.clone(),
            capabilities_required: s.capabilities_required.clone(),
            // Untrusted until the user reviews it in Settings → Skills.
            approval_status: SkillApproval::Pending,
            path: String::new(),
            version: if s.version.is_empty() { "0.1.0".into() } else { s.version.clone() },
            created_at: now,
        };
        global.insert_skill(&skill)?;
        report.skills_installed += 1;
    }

    for a in &pack.agent_types {
        let at = AgentType {
            id: uuid::Uuid::new_v4().to_string(),
            name: a.name.clone(),
            description: a.description.clone(),
            system_prompt: a.system_prompt.clone(),
            tools_allowlist: a.tools_allowlist.clone(),
            seat: a.seat.clone(),
            trigger_examples: a.trigger_examples.clone(),
            // Untrusted until the user approves it in Settings → Agent types.
            approval_status: AgentTypeApproval::Pending,
            source: source.clone(),
            created_at: now,
        };
        global.insert_agent_type(&at)?;
        report.agent_types_installed += 1;
    }

    if !pack.cron_jobs.is_empty() {
        let db = storage.open_profile(profile)?;
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
            report.cron_jobs_installed += 1;
        }
    }

    Ok(report)
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
                cron_jobs.push(PackCron { name: c.name, prompt: c.prompt, schedule: c.schedule });
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
        assert!(g.search_skills("deploy", 5).unwrap().is_empty(), "a pack skill isn't usable until approved");
        // The agent type is PENDING (not dispatchable) + tagged with its pack.
        let ats = g.list_agent_types().unwrap();
        assert_eq!(ats.len(), 1);
        assert_eq!(ats[0].approval_status, AgentTypeApproval::Pending);
        assert_eq!(ats[0].source, "pack:Deploy Kit");
        assert!(g.list_approved_agent_types().unwrap().is_empty());
        // The cron job is installed DISABLED.
        let jobs = storage.open_profile("personal").unwrap().list_cron_jobs().unwrap();
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

    #[test]
    fn export_then_reparse_roundtrips_the_portable_shape() {
        let (storage, root) = temp_storage();
        // Install, approve the items so export can find them by id, then export.
        install_pack(&storage, "personal", &sample_pack(), 1).unwrap();
        let g = storage.global();
        let skill_id = g.list_skills().unwrap()[0].id.clone();
        let at_id = g.list_agent_types().unwrap()[0].id.clone();
        let cron_id = storage.open_profile("personal").unwrap().list_cron_jobs().unwrap()[0].id.clone();

        let exported = export_pack(
            &storage, "personal", "Deploy Kit v2", "2.0.0", "exported",
            &[skill_id], &[at_id], &[cron_id],
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
        assert_eq!(g.list_skills().unwrap().len(), 2, "re-install adds a second (pending) copy");
        let _ = std::fs::remove_dir_all(root);
    }
}
