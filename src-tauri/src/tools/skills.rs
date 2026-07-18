//! Wave 4.1 — the skills system as tools (PLAN §10; `tooling-and-skills.md`).
//! A skill is a reusable playbook. Two tools give the agent access to them,
//! riding the existing registry + gate chain like any other tool:
//!
//! * [`SearchSkillsTool`] (`search_skills`, Safe → pre-trusted) — find a
//!   relevant APPROVED skill and load its body (progressive disclosure: the
//!   catalog shows the tool, a search returns name/description + the body to
//!   follow). Read-only; the returned body is guard-wrapped by `run_turn` as
//!   untrusted content (a poisoned skill can't forge an instruction).
//! * [`SaveSkillTool`] (`save_skill`, **Dangerous** → always an explicit,
//!   content-showing Once prompt) — save a new playbook. That prompt IS the
//!   on-screen review; on approval the skill is stored `Approved` and becomes
//!   searchable (globally). It's `Dangerous`, NOT `Write`, on purpose: a saved
//!   skill is standing, cross-profile, persistent, and auto-loaded into future
//!   turns — as high-blast as a cron job — so it must never be minted without a
//!   human seeing the content. `Write` would let the `accept_edits` mode
//!   blanket-approve it with no review (matching the `cron.rs` precedent).
//!   (Autonomous drafting → `Pending` is Wave 4.2.)
//!
//! A skill can never exceed its profile's permissions: whatever tools its body
//! tells the agent to drive (write_file, fetch, …) each re-gate independently
//! (PLAN §10 "why autonomous is still safe"). The `capabilities_required` a
//! skill declares are validated here so a future skill-as-Tool wrapper's
//! `requires()` is well-formed.

use std::future::Future;
use std::pin::Pin;

use serde_json::json;

use crate::storage::{Skill, SkillApproval, Storage};
use crate::tools::{Capability, ExecCtx, RiskClass, Tool, ToolInput, ToolResult};

/// Cap a saved skill body — a playbook, not a novel.
const MAX_SKILL_CONTENT: usize = 20_000;
const MAX_SKILL_NAME: usize = 120;
const MAX_SKILL_DESCRIPTION: usize = 400;
/// How many matches `search_skills` returns.
const SEARCH_LIMIT: usize = 5;

/// The capability names a skill may declare (mirrors `tools::Capability`). A
/// skill that names an unknown capability is rejected at save — so a future
/// skill-as-Tool's `requires()` is always parseable.
const KNOWN_CAPABILITIES: &[&str] = &[
    "Filesystem",
    "Network",
    "Shell",
    "Display",
    "Audio",
    "ComputerUse",
    "Email",
    "Calendar",
    "WebResearch",
    "LongCompute",
];

// ── search_skills (Safe, read-only) ──────────────────────────────────────────

pub struct SearchSkillsTool {
    storage: Storage,
}

impl SearchSkillsTool {
    pub fn new(storage: Storage) -> Self {
        Self { storage }
    }
}

impl Tool for SearchSkillsTool {
    fn name(&self) -> &str {
        "search_skills"
    }

    fn description(&self) -> &str {
        "Search your saved skills (reusable playbooks) for one relevant to the \
         current task, and load it. args: {\"query\": \"what you're trying to do\"}. \
         Returns matching skills' name, description, and body — follow the body's \
         steps. Only your own approved skills are returned."
    }

    fn requires(&self) -> &[Capability] {
        &[]
    }

    // risk() defaults to Safe (read-only) → pre-trusted.

    fn run<'a>(
        &'a self,
        input: ToolInput,
        _ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let query = match input.args.get("query").and_then(|v| v.as_str()) {
                Some(q) if !q.trim().is_empty() => q.trim().to_string(),
                _ => {
                    return ToolResult::Err(
                        "search_skills requires a non-empty string \"query\" arg".to_string(),
                    )
                }
            };
            match self.storage.global().search_skills(&query, SEARCH_LIMIT) {
                Ok(skills) => {
                    let matches: Vec<_> = skills
                        .iter()
                        .map(|s| {
                            json!({
                                "name": s.name,
                                "description": s.description,
                                "body": s.content,
                            })
                        })
                        .collect();
                    ToolResult::Ok(json!({ "query": query, "matches": matches }))
                }
                Err(e) => ToolResult::Err(format!("search_skills failed: {e}")),
            }
        })
    }
}

// ── save_skill (Write) ───────────────────────────────────────────────────────

pub struct SaveSkillTool {
    storage: Storage,
}

impl SaveSkillTool {
    pub fn new(storage: Storage) -> Self {
        Self { storage }
    }
}

impl Tool for SaveSkillTool {
    fn name(&self) -> &str {
        "save_skill"
    }

    fn description(&self) -> &str {
        "Save a reusable playbook (skill) for future tasks. \
         args: {\"name\":.., \"description\":.., \"content\": \"the steps\", \
         \"capabilities_required\":[\"Filesystem\"]?}. Requires your approval; once \
         saved it's searchable via search_skills."
    }

    fn requires(&self) -> &[Capability] {
        &[]
    }

    fn risk(&self) -> RiskClass {
        // Dangerous, NOT Write: a saved skill is a standing, cross-profile,
        // persistent, auto-loaded playbook. Dangerous forces an always-shown
        // Once prompt (the content review) that `accept_edits` can't blanket-
        // approve — so `approved` always means a human saw it (cron.rs precedent).
        RiskClass::Dangerous
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "description": { "type": "string" },
                "content": { "type": "string", "description": "the playbook steps" },
                "capabilities_required": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "capabilities the playbook needs, e.g. [\"Filesystem\"]"
                }
            },
            "required": ["name", "content"],
            "additionalProperties": false
        })
    }

    fn run<'a>(
        &'a self,
        input: ToolInput,
        _ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            // ── lint ──
            let name = match req_str(&input.args, "name") {
                Ok(s) => s,
                Err(e) => return ToolResult::Err(e),
            };
            if name.chars().count() > MAX_SKILL_NAME {
                return ToolResult::Err(format!("skill name too long (max {MAX_SKILL_NAME} chars)"));
            }
            let content = match req_str(&input.args, "content") {
                Ok(s) => s,
                Err(e) => return ToolResult::Err(e),
            };
            if content.chars().count() > MAX_SKILL_CONTENT {
                return ToolResult::Err(format!(
                    "skill content too long (max {MAX_SKILL_CONTENT} chars)"
                ));
            }
            let description = input
                .args
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if description.chars().count() > MAX_SKILL_DESCRIPTION {
                return ToolResult::Err(format!(
                    "skill description too long (max {MAX_SKILL_DESCRIPTION} chars)"
                ));
            }
            // Validate declared capabilities against the known set.
            let mut capabilities_required = Vec::new();
            if let Some(arr) = input.args.get("capabilities_required").and_then(|v| v.as_array()) {
                for c in arr {
                    let Some(c) = c.as_str() else {
                        return ToolResult::Err(
                            "capabilities_required must be an array of strings".to_string(),
                        );
                    };
                    if !KNOWN_CAPABILITIES.contains(&c) {
                        return ToolResult::Err(format!(
                            "unknown capability \"{c}\" (allowed: {})",
                            KNOWN_CAPABILITIES.join(", ")
                        ));
                    }
                    capabilities_required.push(c.to_string());
                }
            }

            let skill = Skill {
                id: uuid::Uuid::new_v4().to_string(),
                name: name.clone(),
                description,
                content,
                capabilities_required,
                // Reached only after the Dangerous Once-prompt (which showed the
                // content) was approved — so an agent-saved skill is trusted
                // because a human reviewed it. Autonomous drafts that skip the
                // prompt land `Pending` — Wave 4.2.
                approval_status: SkillApproval::Approved,
                path: String::new(),
                version: "0.1.0".to_string(),
                created_at: chrono::Utc::now().timestamp(),
            };
            match self.storage.global().insert_skill(&skill) {
                Ok(()) => ToolResult::Ok(json!({ "saved": true, "name": name })),
                Err(e) => ToolResult::Err(format!("save_skill failed: {e}")),
            }
        })
    }
}

fn req_str(args: &serde_json::Value, key: &str) -> Result<String, String> {
    match args.get(key).and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        _ => Err(format!("save_skill requires a non-empty string \"{key}\" arg")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_storage() -> (Storage, std::path::PathBuf) {
        let mut root = std::env::temp_dir();
        root.push(format!("lhp-skills-{}", uuid::Uuid::new_v4()));
        (Storage::open(&root).unwrap(), root)
    }

    #[tokio::test]
    async fn save_then_search_roundtrip() {
        let (storage, root) = temp_storage();
        let save = SaveSkillTool::new(storage.clone());
        let search = SearchSkillsTool::new(storage.clone());
        let ctx = ExecCtx::default();

        match save
            .run(
                ToolInput::new(json!({
                    "name": "Deploy the app",
                    "description": "How to build and deploy",
                    "content": "1. run tests\n2. build\n3. push to the droplet",
                    "capabilities_required": ["Shell"]
                })),
                &ctx,
            )
            .await
        {
            ToolResult::Ok(v) => assert_eq!(v["saved"], true),
            ToolResult::Err(e) => panic!("save failed: {e}"),
        }

        // An approved skill is searchable by a content/description keyword.
        match search.run(ToolInput::new(json!({ "query": "deploy" })), &ctx).await {
            ToolResult::Ok(v) => {
                let m = v["matches"].as_array().unwrap();
                assert_eq!(m.len(), 1);
                assert_eq!(m[0]["name"], "Deploy the app");
                assert!(m[0]["body"].as_str().unwrap().contains("push to the droplet"));
            }
            ToolResult::Err(e) => panic!("search failed: {e}"),
        }
        // A non-matching query returns nothing.
        match search.run(ToolInput::new(json!({ "query": "zzz nonsense" })), &ctx).await {
            ToolResult::Ok(v) => assert!(v["matches"].as_array().unwrap().is_empty()),
            ToolResult::Err(e) => panic!("search failed: {e}"),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn save_rejects_bad_input() {
        let (storage, root) = temp_storage();
        let save = SaveSkillTool::new(storage.clone());
        let ctx = ExecCtx::default();
        // Missing content.
        assert!(matches!(
            save.run(ToolInput::new(json!({ "name": "x" })), &ctx).await,
            ToolResult::Err(_)
        ));
        // Unknown capability.
        assert!(matches!(
            save.run(
                ToolInput::new(json!({ "name": "x", "content": "y", "capabilities_required": ["Telepathy"] })),
                &ctx
            )
            .await,
            ToolResult::Err(_)
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn save_skill_is_dangerous_so_accept_edits_cant_mint_it_unreviewed() {
        // Regression for the review's HIGH finding: save_skill must NOT be Write
        // (which accept_edits blanket-approves) — a skill is standing +
        // cross-profile + persistent, so it must always be reviewed.
        let (storage, root) = temp_storage();
        assert_eq!(SaveSkillTool::new(storage.clone()).risk(), RiskClass::Dangerous);
        assert_eq!(SearchSkillsTool::new(storage).risk(), RiskClass::Safe);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn search_never_returns_a_pending_skill() {
        let (storage, root) = temp_storage();
        // Insert a PENDING skill directly (as an autonomous 4.2 draft would).
        storage
            .global()
            .insert_skill(&Skill {
                id: "p".into(),
                name: "Pending playbook".into(),
                description: "unreviewed".into(),
                content: "secret deploy steps".into(),
                capabilities_required: vec![],
                approval_status: SkillApproval::Pending,
                path: String::new(),
                version: "0.1.0".into(),
                created_at: 1,
            })
            .unwrap();
        let search = SearchSkillsTool::new(storage.clone());
        match search
            .run(ToolInput::new(json!({ "query": "deploy" })), &ExecCtx::default())
            .await
        {
            ToolResult::Ok(v) => assert!(
                v["matches"].as_array().unwrap().is_empty(),
                "an unapproved skill must never be loadable"
            ),
            ToolResult::Err(e) => panic!("search failed: {e}"),
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
