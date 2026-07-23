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

/// Cap a saved skill body — a playbook, not a novel. `pub(crate)` so the Wave
/// 4.2 autonomous drafter enforces the SAME caps as an agent-driven `save_skill`.
pub(crate) const MAX_SKILL_CONTENT: usize = 20_000;
pub(crate) const MAX_SKILL_NAME: usize = 120;
pub(crate) const MAX_SKILL_DESCRIPTION: usize = 400;
/// How many matches `search_skills` returns.
const SEARCH_LIMIT: usize = 5;

/// The capability names a skill may declare (mirrors `tools::Capability`). A
/// skill that names an unknown capability is rejected at save — so a future
/// skill-as-Tool's `requires()` is always parseable. `pub(crate)` so the 4.2
/// drafter filters drafted capabilities against the same allow-list.
pub(crate) const KNOWN_CAPABILITIES: &[&str] = &[
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

// ── C4: skill-as-Tool — an APPROVED skill becomes a callable Tool ────────────

/// The tool name for a skill: `skill_<slug>`. The prefix guarantees a skill can
/// never shadow a built-in tool name; the slug (lowercase, `[a-z0-9_]`) keeps
/// the name stable for the fenced/native call dialects. Capped so the FULL name
/// fits the strictest native-transport function-name limit (64 chars — a cloud
/// endpoint 400s the whole tools array on an over-long name, which would brick
/// native tool calling for every tool, review finding #2). An all-symbols name
/// slugs to the explicit `unnamed` fallback, never a bare `skill_`.
pub fn skill_tool_name(skill_name: &str) -> String {
    let slug: String = skill_name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let trimmed = slug.trim_matches('_');
    let base = if trimmed.is_empty() { "unnamed" } else { trimmed };
    let capped: String = base.chars().take(58).collect(); // 64 - "skill_".len()
    format!("skill_{}", capped.trim_end_matches('_'))
}

/// C4: derive the wrapper's [`RiskClass`] from a skill's declared capabilities —
/// the "re-gate" rule: invoking a skill is gated NO WEAKER than the built-in
/// tool each declared capability maps to (`Shell` → `shell_exec` is Dangerous;
/// any egress capability → External like `fetch_url`; `Filesystem`/`ComputerUse`
/// → Write like the fs tools). Floor: `Write` — a standing playbook injecting
/// instructions into the loop is never silently pre-trusted like a Safe read,
/// even with an empty capability list (its content isn't statically analyzable).
pub(crate) fn risk_for_capabilities(caps: &[Capability]) -> RiskClass {
    fn rank(r: RiskClass) -> u8 {
        match r {
            RiskClass::Safe => 0,
            RiskClass::Write => 1,
            RiskClass::External => 2,
            RiskClass::Dangerous => 3,
        }
    }
    let mut max = RiskClass::Write; // the floor
    for c in caps {
        let r = match c {
            Capability::Shell => RiskClass::Dangerous,
            Capability::Network
            | Capability::Email
            | Capability::Calendar
            | Capability::WebResearch => RiskClass::External,
            // ComputerUse gates like the ui_* act tools (External) — "no weaker
            // than the built-in each capability maps to" (review finding #6).
            Capability::ComputerUse => RiskClass::External,
            Capability::Filesystem => RiskClass::Write,
            Capability::Display | Capability::Audio | Capability::LongCompute => RiskClass::Safe,
        };
        if rank(r) > rank(max) {
            max = r;
        }
    }
    max
}

/// C4: an APPROVED skill wrapped as a callable [`Tool`]. Invoking it loads the
/// skill's playbook body for the model to follow — the body itself is data (the
/// caller guard-wraps every tool result), and any actions it prescribes still
/// go through the full gating chain as ordinary tool calls. The wrapper
/// re-gates at dispatch time: `requires()` comes from the skill's declared
/// capabilities and `risk()` derives from them (see [`risk_for_capabilities`]),
/// and `run()` re-checks the CURRENT approval status from storage — a stale
/// in-memory registration can never serve a rejected/deleted skill's body.
pub struct SkillTool {
    skill_id: String,
    tool_name: String,
    description: String,
    requires: Vec<Capability>,
    risk: RiskClass,
    storage: std::sync::Arc<Storage>,
}

impl SkillTool {
    /// Build the wrapper for an APPROVED skill. Returns `None` when the skill
    /// isn't approved (never wrap an untrusted playbook) or when a declared
    /// capability string doesn't parse (fail closed — a gate we can't derive is
    /// a gate we refuse to guess).
    pub fn for_skill(skill: &Skill, storage: std::sync::Arc<Storage>) -> Option<SkillTool> {
        if skill.approval_status != SkillApproval::Approved {
            return None;
        }
        let mut requires = Vec::with_capacity(skill.capabilities_required.len());
        for s in &skill.capabilities_required {
            requires.push(Capability::from_capability_str(s)?);
        }
        let risk = risk_for_capabilities(&requires);
        Some(SkillTool {
            skill_id: skill.id.clone(),
            tool_name: skill_tool_name(&skill.name),
            description: format!("Apply the approved skill \"{}\": {}", skill.name, skill.description),
            requires,
            risk,
            storage,
        })
    }
}

impl Tool for SkillTool {
    fn name(&self) -> &str {
        &self.tool_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn risk(&self) -> RiskClass {
        self.risk
    }

    fn requires(&self) -> &[Capability] {
        &self.requires
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "What you want this playbook applied to (optional context)."
                }
            },
            "additionalProperties": false
        })
    }

    fn run<'a>(
        &'a self,
        _input: ToolInput,
        _ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            // Defense in depth: re-read the CURRENT approval from storage at
            // call time. A rejection/delete unregisters the wrapper, but a
            // stale Arc (a helper's snapshotted sub-registry, a race) must
            // still refuse — approval lives in the DB, not in this struct.
            match self.storage.global().get_skill(&self.skill_id) {
                Ok(Some(s)) if s.approval_status == SkillApproval::Approved => {
                    ToolResult::Ok(json!({
                        "skill": s.name,
                        "description": s.description,
                        "playbook": s.content,
                    }))
                }
                Ok(_) => ToolResult::Err(
                    "this skill is no longer approved — refusing to load its playbook".to_string(),
                ),
                Err(e) => ToolResult::Err(format!("couldn't verify skill approval: {e}")),
            }
        })
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

    // ── C4: skill-as-Tool ────────────────────────────────────────────────────

    fn approved_skill(id: &str, name: &str, caps: &[&str]) -> Skill {
        Skill {
            id: id.into(),
            name: name.into(),
            description: "a test playbook".into(),
            content: "1. do the thing\n2. verify it".into(),
            capabilities_required: caps.iter().map(|s| s.to_string()).collect(),
            approval_status: SkillApproval::Approved,
            path: String::new(),
            version: "1".into(),
            created_at: 1,
        }
    }

    #[test]
    fn skill_tool_name_is_prefixed_and_slugged() {
        assert_eq!(skill_tool_name("My Deploy Playbook"), "skill_my_deploy_playbook");
        assert_eq!(skill_tool_name("weird!!chars"), "skill_weird__chars");
        // The prefix structurally prevents shadowing any built-in tool name.
        assert!(skill_tool_name("shell_exec").starts_with("skill_"));
        // Review #2: the FULL name fits the strictest native function-name
        // limit (64) — one long-named skill must never brick the tools array.
        let long = skill_tool_name(&"very long skill name ".repeat(20));
        assert!(long.chars().count() <= 64, "got {} chars", long.chars().count());
        // Review #3 edge: an all-symbols name gets the explicit fallback.
        assert_eq!(skill_tool_name("!!!"), "skill_unnamed");
    }

    #[test]
    fn every_known_capability_string_parses_to_the_enum() {
        // Lock-step guard: a KNOWN_CAPABILITIES entry that doesn't parse would
        // silently drop a requires() gate — fail here instead.
        for s in KNOWN_CAPABILITIES {
            assert!(
                crate::tools::Capability::from_capability_str(s).is_some(),
                "KNOWN_CAPABILITIES entry {s:?} must parse to a Capability"
            );
        }
        assert!(crate::tools::Capability::from_capability_str("Nope").is_none());
    }

    #[test]
    fn skill_risk_derives_from_capabilities_with_a_write_floor() {
        use crate::tools::Capability as C;
        // The floor: even a no-capability playbook is never Safe/pre-trusted.
        assert_eq!(risk_for_capabilities(&[]), RiskClass::Write);
        assert_eq!(risk_for_capabilities(&[C::Display]), RiskClass::Write);
        assert_eq!(risk_for_capabilities(&[C::Filesystem]), RiskClass::Write);
        // Egress capabilities gate like fetch_url.
        assert_eq!(risk_for_capabilities(&[C::Network]), RiskClass::External);
        assert_eq!(risk_for_capabilities(&[C::WebResearch]), RiskClass::External);
        // Shell gates like shell_exec (Dangerous) and dominates.
        assert_eq!(risk_for_capabilities(&[C::Shell]), RiskClass::Dangerous);
        assert_eq!(risk_for_capabilities(&[C::Network, C::Shell]), RiskClass::Dangerous);
    }

    #[test]
    fn for_skill_wraps_only_approved_skills_and_fails_closed_on_bad_caps() {
        let (storage, root) = temp_storage();
        let storage = std::sync::Arc::new(storage);
        let mut s = approved_skill("s1", "Deploy It", &["Shell"]);
        let tool = SkillTool::for_skill(&s, std::sync::Arc::clone(&storage)).expect("approved wraps");
        assert_eq!(tool.name(), "skill_deploy_it");
        assert_eq!(tool.risk(), RiskClass::Dangerous, "Shell skill re-gates like shell_exec");

        s.approval_status = SkillApproval::Pending;
        assert!(SkillTool::for_skill(&s, std::sync::Arc::clone(&storage)).is_none(), "pending never wraps");
        s.approval_status = SkillApproval::Approved;
        s.capabilities_required = vec!["NotACapability".into()];
        assert!(SkillTool::for_skill(&s, storage).is_none(), "unparseable capability fails closed");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn skill_tool_run_rechecks_approval_from_storage_at_call_time() {
        let (storage, root) = temp_storage();
        let storage = std::sync::Arc::new(storage);
        let skill = approved_skill("s2", "Backup Routine", &[]);
        storage.global().insert_skill(&skill).unwrap();
        let tool = SkillTool::for_skill(&skill, std::sync::Arc::clone(&storage)).unwrap();

        // Approved → the playbook loads.
        let ok = tool.run(ToolInput::new(json!({})), &ExecCtx::default()).await;
        match ok {
            ToolResult::Ok(v) => assert!(v["playbook"].as_str().unwrap().contains("do the thing")),
            other => panic!("expected the playbook, got {other:?}"),
        }
        // Rejected AFTER registration → a stale wrapper must refuse (the DB is
        // the source of truth, not the captured struct).
        storage.global().set_skill_approval("s2", SkillApproval::Rejected).unwrap();
        let refused = tool.run(ToolInput::new(json!({})), &ExecCtx::default()).await;
        assert!(matches!(refused, ToolResult::Err(_)), "a revoked skill's body never loads");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn hot_register_and_unregister_round_trip_and_never_shadow() {
        use crate::tools::{EchoTool, ToolRegistry};
        let (storage, root) = temp_storage();
        let storage = std::sync::Arc::new(storage);
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoTool));

        let skill = approved_skill("s3", "Summarize Inbox", &[]);
        let tool = SkillTool::for_skill(&skill, std::sync::Arc::clone(&storage)).unwrap();
        assert!(registry.register_dynamic(Box::new(tool)), "hot-register succeeds");
        assert!(registry.get("skill_summarize_inbox").is_some(), "callable by name");
        assert!(registry.all_names().contains("skill_summarize_inbox"));

        // A dynamic tool can never shadow a static one (nor itself).
        struct FakeEcho;
        impl Tool for FakeEcho {
            fn name(&self) -> &str { "echo" }
            fn requires(&self) -> &[Capability] { &[] }
            fn run<'a>(&'a self, _i: ToolInput, _c: &'a ExecCtx)
                -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
                Box::pin(async { ToolResult::Err("shadow".into()) })
            }
        }
        assert!(!registry.register_dynamic(Box::new(FakeEcho)), "shadowing a built-in refused");

        // A helper's frozen belt SNAPSHOTS the dynamic set (no later widening).
        let belt: std::collections::HashSet<String> =
            ["skill_summarize_inbox".to_string()].into_iter().collect();
        let sub = registry.restricted_to(&belt);
        assert!(sub.get("skill_summarize_inbox").is_some(), "snapshot carries the skill");

        assert!(registry.unregister_dynamic("skill_summarize_inbox"));
        assert!(registry.get("skill_summarize_inbox").is_none(), "unregistered → not callable");
        assert!(!registry.unregister_dynamic("echo"), "static tools are untouchable");
        assert!(registry.get("echo").is_some());
        let _ = std::fs::remove_dir_all(root);
    }
}
