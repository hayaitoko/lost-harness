//! `PermissionHook` — tri-state per-tool mode (`allow`/`ask`/`deny`) plus
//! pattern rules (e.g. `(shell_exec, "git commit:*", allow)`). Spec
//! `docs/tooling-and-skills.md` §3.1 "Permission granularity" / §10,
//! `docs/PLAN.md` §8 M3 item 3.
//!
//! Resolution order (spec §10 step 3.5): pattern rules first (deny > ask >
//! allow, most-specific wins among matches), falling back to the whole-tool
//! mode, falling back to `Continue` (unset) so `FirstUseConfirmHook`
//! downstream can do its "ask once, remember" thing.
//!
//! Backed by a pluggable `PolicySource` trait so a future SQLite-backed
//! `tool_rules`/`tool_profile_permissions` implementation can drop in
//! without touching `PermissionHook` itself — the SQLite schema for those
//! tables is explicitly *not* built in this milestone (PLAN.md M4), so
//! `InMemoryPolicySource` is the only implementation today.

use std::collections::HashMap;
use std::sync::Arc;

use crate::hooks::approval::{ActionFingerprint, ApprovalLedger};
use crate::hooks::{EventContext, GatingHook, HookEvent, HookResult};
use crate::storage::Storage;

// ── PermissionMode ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionMode {
    Allow,
    Ask,
    Deny,
}

impl PermissionMode {
    /// Higher wins when two matching rules are equally specific.
    fn priority(self) -> u8 {
        match self {
            PermissionMode::Deny => 2,
            PermissionMode::Ask => 1,
            PermissionMode::Allow => 0,
        }
    }
}

// ── ToolRule ─────────────────────────────────────────────────────────────

/// A pattern-scoped rule: `(tool_name, pattern, action)`. `pattern` is
/// matched against `EventContext::command_text` via a small glob matcher
/// supporting `*` as a wildcard (e.g. `"git commit:*"`, `"rm -rf:*"`) —
/// deliberately simple since these are user/profile-authored strings, not
/// full regex.
#[derive(Debug, Clone)]
pub struct ToolRule {
    pub tool_name: String,
    pub pattern: String,
    pub action: PermissionMode,
}

impl ToolRule {
    pub fn new(
        tool_name: impl Into<String>,
        pattern: impl Into<String>,
        action: PermissionMode,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            pattern: pattern.into(),
            action,
        }
    }

    /// Number of non-wildcard characters — the specificity heuristic used
    /// to pick a winner among multiple matching rules (more literal
    /// characters = more specific).
    fn specificity(&self) -> usize {
        self.pattern.chars().filter(|c| *c != '*').count()
    }
}

/// Minimal glob matcher: `*` matches any run of characters (including
/// none); everything else must match literally. Good enough for patterns
/// like `"git commit:*"` or `"rm -rf:*"` without pulling in a regex/glob
/// crate for a handful of profile-authored rules.
fn glob_match(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == text;
    }

    let starts_with_wild = pattern.starts_with('*');
    let ends_with_wild = pattern.ends_with('*');
    let segments: Vec<&str> = pattern.split('*').filter(|s| !s.is_empty()).collect();

    if segments.is_empty() {
        // Pattern is just "*" (or "**", ...) — matches anything.
        return true;
    }

    let mut pos = 0usize;
    for (i, seg) in segments.iter().enumerate() {
        let is_first = i == 0;
        let is_last = i == segments.len() - 1;

        if is_first && !starts_with_wild {
            if !text[pos..].starts_with(seg) {
                return false;
            }
            pos += seg.len();
        } else if is_last && !ends_with_wild {
            if !text[pos..].ends_with(seg) {
                return false;
            }
            // No need to advance pos — this is the final check.
        } else {
            match text[pos..].find(seg) {
                Some(idx) => pos += idx + seg.len(),
                None => return false,
            }
        }
    }
    true
}

// ── PolicySource ─────────────────────────────────────────────────────────

/// Where `PermissionHook` gets its configuration from. `mode_for` returns
/// `None` when the tool has no configured whole-tool mode at all — that's
/// the signal to fall through to `FirstUseConfirmHook` rather than an
/// implicit `Ask`.
pub trait PolicySource: Send + Sync {
    /// The whole-tool default mode, or `None` to fall through to
    /// `FirstUseConfirmHook`. Profile-blind: the default is risk-derived and
    /// the same for every profile (a *persisted* whole-tool policy is
    /// expressed as a `(tool, "*", …)` pattern rule instead, so it flows
    /// through `rules_for` and respects per-profile isolation).
    fn mode_for(&self, tool_name: &str) -> Option<PermissionMode>;

    /// Pattern rules for this tool in this `profile`. Profile-scoped so a
    /// persisted rule written in one profile (`SqlitePolicySource`) never
    /// resolves in another. In-memory sources ignore `profile`.
    fn rules_for(&self, tool_name: &str, profile: &str) -> Vec<ToolRule>;
}

/// A simple in-memory `PolicySource`. Stands in for the future
/// SQLite-backed `tool_profile_permissions`/`tool_rules` tables (PLAN.md
/// M4) — same trait contract, no migration needed when that lands.
#[derive(Debug, Clone, Default)]
pub struct InMemoryPolicySource {
    modes: HashMap<String, PermissionMode>,
    rules: Vec<ToolRule>,
}

impl InMemoryPolicySource {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_mode(&mut self, tool_name: impl Into<String>, mode: PermissionMode) -> &mut Self {
        self.modes.insert(tool_name.into(), mode);
        self
    }

    pub fn add_rule(
        &mut self,
        tool_name: impl Into<String>,
        pattern: impl Into<String>,
        action: PermissionMode,
    ) -> &mut Self {
        self.rules.push(ToolRule::new(tool_name, pattern, action));
        self
    }
}

impl PolicySource for InMemoryPolicySource {
    fn mode_for(&self, tool_name: &str) -> Option<PermissionMode> {
        self.modes.get(tool_name).copied()
    }

    fn rules_for(&self, tool_name: &str, _profile: &str) -> Vec<ToolRule> {
        self.rules
            .iter()
            .filter(|r| r.tool_name == tool_name)
            .cloned()
            .collect()
    }
}

// ── SqlitePolicySource ─────────────────────────────────────────────────────

/// A per-profile `PolicySource` backed by the SQLite `tool_rules` table (Q8
/// persisted `Always` grants). Read **live** on each gating pass — a freshly
/// persisted rule is visible on the next same-session call and a Settings
/// revoke takes effect immediately, no restart. One indexed lookup per pass;
/// consistent with the item-5 audit write already on the dispatch path (rides
/// the same `ProfileDb` `unsafe impl Sync` single-in-flight deferral).
/// `mode_for` is always `None` — whole-tool defaults are risk-derived and live
/// in-memory; a persisted whole-tool policy is a `(tool, "*", …)` pattern rule.
pub struct SqlitePolicySource {
    storage: Storage,
}

impl SqlitePolicySource {
    pub fn new(storage: Storage) -> Self {
        Self { storage }
    }
}

impl PolicySource for SqlitePolicySource {
    fn mode_for(&self, _tool_name: &str) -> Option<PermissionMode> {
        None
    }

    fn rules_for(&self, tool_name: &str, profile: &str) -> Vec<ToolRule> {
        // An empty profile (the EventContext default, tests, any path that
        // didn't set one) has no per-profile DB — return nothing and fall
        // through to the in-memory defaults, i.e. exactly the pre-Q8 behavior.
        if profile.is_empty() {
            return Vec::new();
        }
        let db = match self.storage.open_profile(profile) {
            Ok(db) => db,
            Err(e) => {
                tracing::warn!(profile, error = %e, "tool_rules: open_profile failed; no persisted rules this pass");
                return Vec::new();
            }
        };
        let rows = match db.list_tool_rules_for(tool_name) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(tool = tool_name, error = %e, "tool_rules: query failed; no persisted rules this pass");
                return Vec::new();
            }
        };
        rows.into_iter()
            .filter_map(|r| {
                let action = match r.action.as_str() {
                    "allow" => PermissionMode::Allow,
                    "ask" => PermissionMode::Ask,
                    "deny" => PermissionMode::Deny,
                    // A malformed action must NEVER silently widen access —
                    // drop the rule (falls through to the default mode) + flag.
                    other => {
                        tracing::warn!(tool = tool_name, action = other, "tool_rules: skipping rule with unknown action");
                        return None;
                    }
                };
                Some(ToolRule::new(r.tool_name, r.pattern, action))
            })
            .collect()
    }
}

// ── LayeredPolicySource ────────────────────────────────────────────────────

/// Composes an in-memory `defaults` source (the risk-derived whole-tool modes
/// built at boot, plus any static rules) with a `persisted` per-profile source
/// (`SqlitePolicySource`). `mode_for` comes from the defaults; `rules_for` is
/// `persisted ⧺ defaults`, so a dialog/Settings-authored rule participates in
/// the SAME deny>ask>allow / most-specific-wins resolution as a static one —
/// `PermissionHook::resolve` is unchanged.
pub struct LayeredPolicySource {
    defaults: Box<dyn PolicySource>,
    persisted: Box<dyn PolicySource>,
}

impl LayeredPolicySource {
    pub fn new(defaults: Box<dyn PolicySource>, persisted: Box<dyn PolicySource>) -> Self {
        Self { defaults, persisted }
    }
}

impl PolicySource for LayeredPolicySource {
    fn mode_for(&self, tool_name: &str) -> Option<PermissionMode> {
        self.defaults.mode_for(tool_name)
    }

    fn rules_for(&self, tool_name: &str, profile: &str) -> Vec<ToolRule> {
        let mut out = self.persisted.rules_for(tool_name, profile);
        out.extend(self.defaults.rules_for(tool_name, profile));
        out
    }
}

// ── PermissionHook ───────────────────────────────────────────────────────

pub struct PermissionHook {
    policy: Box<dyn PolicySource>,
    /// Interactive-approval grants recorded by `ToolDispatcher`. A grant that
    /// covers this call turns a policy `Ask` into `Continue` on the re-run.
    /// Defaults to an empty, standalone ledger (so `Ask` behaves as a plain
    /// `Ask` where no interactive approver is wired).
    ledger: Arc<ApprovalLedger>,
}

impl PermissionHook {
    pub fn new(policy: Box<dyn PolicySource>) -> Self {
        Self {
            policy,
            ledger: Arc::new(ApprovalLedger::new()),
        }
    }

    /// Share the dispatcher's approval ledger so recorded grants are visible
    /// here (see `crate::hooks::build_pretooluse_chain_full`).
    pub fn with_ledger(mut self, ledger: Arc<ApprovalLedger>) -> Self {
        self.ledger = ledger;
        self
    }

    /// Resolve the effective mode for this call: most-specific matching
    /// pattern rule wins (deny > ask > allow tiebreak among equally
    /// specific matches), else the whole-tool mode, else `None`.
    fn resolve(&self, ctx: &EventContext) -> Option<PermissionMode> {
        let rules = self.policy.rules_for(&ctx.tool_name, &ctx.profile);
        let matching = rules
            .iter()
            .filter(|r| glob_match(&r.pattern, &ctx.command_text));

        let best = matching.max_by_key(|r| (r.specificity(), r.action.priority()));
        if let Some(rule) = best {
            return Some(rule.action);
        }
        self.policy.mode_for(&ctx.tool_name)
    }
}

impl GatingHook for PermissionHook {
    fn name(&self) -> &str {
        "permission"
    }

    fn on_event(&self, ctx: &mut EventContext) -> HookResult {
        if ctx.event != HookEvent::PreToolUse {
            return HookResult::Continue;
        }

        match self.resolve(ctx) {
            Some(PermissionMode::Allow) => HookResult::Continue,
            Some(PermissionMode::Deny) => HookResult::Deny(format!(
                "denied by permission policy for tool '{}'",
                ctx.tool_name
            )),
            Some(PermissionMode::Ask) => {
                // A prior interactive approval may already cover this exact
                // action (or this whole tool). If so, don't ask again.
                let fp = ActionFingerprint::from_ctx(ctx);
                if self.ledger.covers(&ctx.tool_name, &fp) {
                    HookResult::Continue
                } else {
                    HookResult::Ask(format!("tool '{}' requires confirmation", ctx.tool_name))
                }
            }
            // Unconfigured — defer to FirstUseConfirmHook.
            None => HookResult::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_match_prefix_wildcard() {
        assert!(glob_match("git commit:*", "git commit:-m fix"));
        assert!(!glob_match("git commit:*", "git push:origin"));
    }

    #[test]
    fn glob_match_exact() {
        assert!(glob_match("git status", "git status"));
        assert!(!glob_match("git status", "git status --short"));
    }

    #[test]
    fn glob_match_bare_star_matches_anything() {
        assert!(glob_match("*", "anything at all"));
    }

    #[test]
    fn whole_tool_deny_denies() {
        let mut policy = InMemoryPolicySource::new();
        policy.set_mode("shell_exec", PermissionMode::Deny);
        let hook = PermissionHook::new(Box::new(policy));
        let mut ctx = EventContext::pre_tool_use("shell_exec").with_command_text("ls -la");
        match hook.on_event(&mut ctx) {
            HookResult::Deny(_) => {}
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn whole_tool_allow_allows() {
        let mut policy = InMemoryPolicySource::new();
        policy.set_mode("shell_exec", PermissionMode::Allow);
        let hook = PermissionHook::new(Box::new(policy));
        let mut ctx = EventContext::pre_tool_use("shell_exec").with_command_text("ls -la");
        assert_eq!(hook.on_event(&mut ctx), HookResult::Continue);
    }

    #[test]
    fn unconfigured_tool_falls_through_as_continue() {
        let policy = InMemoryPolicySource::new();
        let hook = PermissionHook::new(Box::new(policy));
        let mut ctx = EventContext::pre_tool_use("unknown_tool").with_command_text("whatever");
        assert_eq!(
            hook.on_event(&mut ctx),
            HookResult::Continue,
            "unconfigured tools must fall through to FirstUseConfirmHook, not implicitly Ask"
        );
    }

    #[test]
    fn pattern_rule_overrides_whole_tool_mode() {
        let mut policy = InMemoryPolicySource::new();
        policy.set_mode("shell_exec", PermissionMode::Ask);
        policy.add_rule("shell_exec", "git commit:*", PermissionMode::Allow);
        let hook = PermissionHook::new(Box::new(policy));

        let mut allowed = EventContext::pre_tool_use("shell_exec")
            .with_command_text("git commit:-m fix typo");
        assert_eq!(hook.on_event(&mut allowed), HookResult::Continue);

        // Doesn't match the pattern → falls back to the whole-tool Ask.
        let mut asked =
            EventContext::pre_tool_use("shell_exec").with_command_text("git push:origin");
        match hook.on_event(&mut asked) {
            HookResult::Ask(_) => {}
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[test]
    fn most_specific_matching_rule_wins() {
        let mut policy = InMemoryPolicySource::new();
        // Broad allow, narrower deny — the narrower/more-specific one
        // (more literal characters) must win regardless of registration
        // order.
        policy.add_rule("shell_exec", "git *", PermissionMode::Allow);
        policy.add_rule("shell_exec", "git push --force*", PermissionMode::Deny);
        let hook = PermissionHook::new(Box::new(policy));

        let mut ctx = EventContext::pre_tool_use("shell_exec")
            .with_command_text("git push --force origin main");
        match hook.on_event(&mut ctx) {
            HookResult::Deny(_) => {}
            other => panic!("expected Deny (most specific rule), got {other:?}"),
        }

        let mut ctx2 =
            EventContext::pre_tool_use("shell_exec").with_command_text("git status");
        assert_eq!(hook.on_event(&mut ctx2), HookResult::Continue);
    }

    #[test]
    fn deny_wins_tiebreak_among_equally_specific_matches() {
        let mut policy = InMemoryPolicySource::new();
        // Two rules with identical specificity (same literal char count)
        // that both match the same text — deny must win the tiebreak.
        policy.add_rule("shell_exec", "rm -rf:*", PermissionMode::Deny);
        policy.add_rule("shell_exec", "rm -rf:*", PermissionMode::Allow);
        let hook = PermissionHook::new(Box::new(policy));
        let mut ctx =
            EventContext::pre_tool_use("shell_exec").with_command_text("rm -rf:/tmp/x");
        match hook.on_event(&mut ctx) {
            HookResult::Deny(_) => {}
            other => panic!("expected Deny to win the tiebreak, got {other:?}"),
        }
    }

    #[test]
    fn non_pretooluse_event_is_ignored() {
        let mut policy = InMemoryPolicySource::new();
        policy.set_mode("shell_exec", PermissionMode::Deny);
        let hook = PermissionHook::new(Box::new(policy));
        let mut ctx = EventContext::pre_tool_use("shell_exec").with_command_text("ls");
        ctx.event = HookEvent::PostToolUse;
        assert_eq!(hook.on_event(&mut ctx), HookResult::Continue);
    }

    #[test]
    fn ask_becomes_continue_when_the_ledger_covers_the_action() {
        use crate::hooks::approval::{GrantScope, GrantTarget};
        use crate::tools::ToolInput;

        let mut policy = InMemoryPolicySource::new();
        policy.set_mode("write_file", PermissionMode::Ask);
        let ledger = Arc::new(ApprovalLedger::new());
        let hook = PermissionHook::new(Box::new(policy)).with_ledger(Arc::clone(&ledger));

        let mut ctx = EventContext::pre_tool_use("write_file")
            .with_input(ToolInput::new(serde_json::json!({"path": "a.txt"})));

        // No grant yet → Ask.
        match hook.on_event(&mut ctx) {
            HookResult::Ask(_) => {}
            other => panic!("expected Ask before any grant, got {other:?}"),
        }

        // Grant exactly this action → Continue on the re-run.
        let fp = ActionFingerprint::from_ctx(&ctx);
        ledger.grant(GrantTarget::Fingerprint(fp), GrantScope::Session);
        assert_eq!(hook.on_event(&mut ctx), HookResult::Continue);

        // A different action (different args) is NOT covered — no drift.
        let mut other = EventContext::pre_tool_use("write_file")
            .with_input(ToolInput::new(serde_json::json!({"path": "b.txt"})));
        match hook.on_event(&mut other) {
            HookResult::Ask(_) => {}
            o => panic!("a grant must not drift to a different action, got {o:?}"),
        }
    }

    // ── SqlitePolicySource / LayeredPolicySource (Q8 persisted rules) ────────

    /// A throwaway on-disk `Storage` for the persisted-rule tests.
    fn temp_storage() -> (crate::storage::Storage, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("lhp-perm-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        let storage = crate::storage::Storage::open(&path).unwrap();
        (storage, path)
    }

    fn persist_rule(storage: &Storage, profile: &str, tool: &str, pattern: &str, action: &str) {
        storage
            .open_profile(profile)
            .unwrap()
            .add_tool_rule(&crate::storage::ToolRuleRow {
                id: format!("{tool}-{pattern}-{action}"),
                tool_name: tool.into(),
                pattern: pattern.into(),
                action: action.into(),
                created_at: 1,
            })
            .unwrap();
    }

    #[test]
    fn sqlite_source_resolves_only_the_calls_profile() {
        let (storage, dir) = temp_storage();
        persist_rule(&storage, "personal", "write_file", "*", "allow");
        let src = SqlitePolicySource::new(storage);

        // Resolves in the profile it was written to…
        let personal = src.rules_for("write_file", "personal");
        assert_eq!(personal.len(), 1);
        assert_eq!(personal[0].action, PermissionMode::Allow);

        // …never leaks into another profile, and an empty profile is inert.
        assert!(src.rules_for("write_file", "work").is_empty());
        assert!(src.rules_for("write_file", "").is_empty());
        // A different tool is unaffected.
        assert!(src.rules_for("read_file", "personal").is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sqlite_source_skips_a_malformed_action() {
        // A row with an unknown action must be dropped (fall through to the
        // default), never silently widen access.
        let (storage, dir) = temp_storage();
        persist_rule(&storage, "personal", "write_file", "*", "yolo");
        let src = SqlitePolicySource::new(storage);
        assert!(
            src.rules_for("write_file", "personal").is_empty(),
            "an unknown action must not become a rule"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn layered_source_composes_persisted_over_defaults() {
        let (storage, dir) = temp_storage();
        persist_rule(&storage, "personal", "write_file", "notes/*", "allow");
        let mut defaults = InMemoryPolicySource::new();
        defaults.set_mode("write_file", PermissionMode::Ask);
        defaults.add_rule("write_file", "static/*", PermissionMode::Deny);

        let layered = LayeredPolicySource::new(
            Box::new(defaults),
            Box::new(SqlitePolicySource::new(storage)),
        );

        // mode_for comes from the defaults; rules_for merges both sources.
        assert_eq!(layered.mode_for("write_file"), Some(PermissionMode::Ask));
        let rules = layered.rules_for("write_file", "personal");
        assert_eq!(rules.len(), 2, "persisted + default rules both present");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn permission_hook_honors_a_persisted_allow_rule_per_profile() {
        // The end-to-end read path: a persisted (write_file, "*", allow) rule
        // in `personal` turns the whole-tool Ask default into Continue — but
        // ONLY when the call runs under `personal`.
        let (storage, dir) = temp_storage();
        persist_rule(&storage, "personal", "write_file", "*", "allow");
        let mut defaults = InMemoryPolicySource::new();
        defaults.set_mode("write_file", PermissionMode::Ask);
        let layered = LayeredPolicySource::new(
            Box::new(defaults),
            Box::new(SqlitePolicySource::new(storage)),
        );
        let hook = PermissionHook::new(Box::new(layered));

        // Under `personal` → the persisted allow-rule wins → Continue.
        let mut in_personal = EventContext::pre_tool_use("write_file")
            .with_command_text("write_file {}")
            .with_profile("personal");
        assert_eq!(hook.on_event(&mut in_personal), HookResult::Continue);

        // Under `work` (isolation) → no rule → the whole-tool Ask default.
        let mut in_work = EventContext::pre_tool_use("write_file")
            .with_command_text("write_file {}")
            .with_profile("work");
        match hook.on_event(&mut in_work) {
            HookResult::Ask(_) => {}
            o => panic!("a personal-profile rule must not apply in work, got {o:?}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
