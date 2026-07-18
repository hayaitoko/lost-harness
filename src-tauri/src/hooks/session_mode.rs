//! Session-wide permission modes (Q11) — `normal` / `plan` / `accept_edits`.
//!
//! A *session mode* is a user-chosen posture for a conversation that changes
//! how tool calls are gated. Crucially, it is **bounded by Q8's grant×risk
//! matrix**: a mode can never widen `External`/`Dangerous`, and it can never
//! bypass a non-overridable floor.
//!
//! Enforcement is a single [`SessionModeHook`] placed in the PreToolUse chain
//! **after** the floors (`SandboxHook`'s hardline danger denylist and
//! `ProtectedPathHook`) and **before** `PermissionHook`. That position is what
//! makes the bound structural:
//! - A danger-floor or protected-path call has already short-circuited
//!   (`Deny`/`Ask`) before this hook runs, so no mode can loosen it.
//! - `accept_edits` only ever auto-approves `Write` risk; `External`/`Dangerous`
//!   fall straight through to normal gating (`PermissionHook` + the Q8 matrix in
//!   `resolve_grant`), so they can't be widened.
//! - `plan` only ever *denies* (it's read-only), which is always the safe
//!   direction.

use crate::hooks::{EventContext, GatingHook, HookResult};
use crate::tools::RiskClass;

/// A conversation's permission posture. `Normal` is the default and reproduces
/// the pre-mode behavior exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    /// Normal gating — the default; behaves exactly as if modes didn't exist.
    #[default]
    Normal,
    /// Plan / read-only: the agent may read and reason but make **no changes**.
    /// Any tool with risk above `Safe` is denied with an explanatory message so
    /// the model can plan and report instead of mutating.
    Plan,
    /// Accept-edits: auto-approve `Write`-risk tools (no prompt) to cut friction
    /// on local edits — but **never** `External`/`Dangerous`, which still gate
    /// normally. This deliberately spends safety margin, and only for local
    /// edits, per Q11's bound.
    AcceptEdits,
}

impl SessionMode {
    /// Lowercase stable discriminant for the IPC/UI surface.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionMode::Normal => "normal",
            SessionMode::Plan => "plan",
            SessionMode::AcceptEdits => "accept_edits",
        }
    }

    /// Parse a mode from the frontend, defaulting to `Normal` (the safe
    /// posture) for anything unrecognized — an unknown value never silently
    /// unlocks `accept_edits`.
    pub fn from_str_lenient(s: &str) -> Self {
        match s {
            "plan" => SessionMode::Plan,
            "accept_edits" | "accept-edits" => SessionMode::AcceptEdits,
            _ => SessionMode::Normal,
        }
    }
}

/// The gating hook that applies the session mode. See the module docs for why
/// its chain position makes the Q8-matrix bound structural.
pub struct SessionModeHook;

impl GatingHook for SessionModeHook {
    fn name(&self) -> &str {
        "session_mode"
    }

    fn on_event(&self, ctx: &mut EventContext) -> HookResult {
        match ctx.session_mode {
            SessionMode::Normal => HookResult::Continue,
            SessionMode::Plan => {
                // Read-only: allow Safe reads, deny anything that could change
                // state or reach off-box. This only ever restricts.
                if ctx.risk == RiskClass::Safe {
                    HookResult::Continue
                } else {
                    HookResult::Deny(format!(
                        "plan mode is read-only — \"{}\" would make a change (risk: {}). \
                         Switch off plan mode to let it run.",
                        ctx.tool_name,
                        ctx.risk.as_str()
                    ))
                }
            }
            SessionMode::AcceptEdits => {
                if ctx.risk == RiskClass::Write {
                    // Auto-approve local edits. Setting `policy_allowed` also
                    // satisfies the downstream `FirstUseConfirmHook` — the same
                    // channel Q8's "always allow" uses — so this is a genuine
                    // zero-prompt approval, not just a first-hook opinion.
                    // This runs AFTER Sandbox + ProtectedPath, so a danger-floor
                    // or protected-path write already short-circuited and never
                    // reaches here. An explicit deny-rule in `PermissionHook`
                    // (which runs next) still wins — accept-edits doesn't
                    // override a user's deliberate "never".
                    ctx.policy_allowed = true;
                    HookResult::Allow
                } else {
                    // Safe reads need no help; External/Dangerous fall through to
                    // normal gating — accept-edits NEVER widens beyond Write.
                    HookResult::Continue
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::EventContext;
    use crate::tools::RiskClass;

    fn ev(tool: &str, risk: RiskClass, mode: SessionMode) -> EventContext {
        EventContext::pre_tool_use(tool)
            .with_risk(risk)
            .with_session_mode(mode)
    }

    #[test]
    fn normal_mode_is_a_noop_for_every_risk() {
        for risk in [
            RiskClass::Safe,
            RiskClass::Write,
            RiskClass::External,
            RiskClass::Dangerous,
        ] {
            let mut c = ev("t", risk, SessionMode::Normal);
            assert_eq!(SessionModeHook.on_event(&mut c), HookResult::Continue);
        }
    }

    #[test]
    fn plan_mode_allows_reads_and_denies_every_mutation() {
        let mut safe = ev("read_file", RiskClass::Safe, SessionMode::Plan);
        assert_eq!(SessionModeHook.on_event(&mut safe), HookResult::Continue);

        for risk in [RiskClass::Write, RiskClass::External, RiskClass::Dangerous] {
            let mut c = ev("mutate", risk, SessionMode::Plan);
            assert!(
                matches!(SessionModeHook.on_event(&mut c), HookResult::Deny(_)),
                "plan mode must deny {risk:?}"
            );
        }
    }

    #[test]
    fn accept_edits_auto_approves_write_but_never_external_or_dangerous() {
        // Write → Allow + policy_allowed (zero-prompt).
        let mut w = ev("write_file", RiskClass::Write, SessionMode::AcceptEdits);
        assert_eq!(SessionModeHook.on_event(&mut w), HookResult::Allow);
        assert!(w.policy_allowed, "accept-edits must satisfy first-use for a Write");

        // External / Dangerous → Continue (fall through to the matrix), and it
        // must NOT set policy_allowed (that would bypass first-use for them).
        for risk in [RiskClass::External, RiskClass::Dangerous] {
            let mut c = ev("reach_out", risk, SessionMode::AcceptEdits);
            assert_eq!(
                SessionModeHook.on_event(&mut c),
                HookResult::Continue,
                "accept-edits must not touch {risk:?}"
            );
            assert!(
                !c.policy_allowed,
                "accept-edits must NEVER pre-authorize {risk:?} — that would widen the matrix"
            );
        }
    }

    #[test]
    fn round_trips_through_str() {
        for m in [SessionMode::Normal, SessionMode::Plan, SessionMode::AcceptEdits] {
            assert_eq!(SessionMode::from_str_lenient(m.as_str()), m);
        }
        // Unknown → Normal (never silently accept-edits).
        assert_eq!(SessionMode::from_str_lenient("garbage"), SessionMode::Normal);
        assert_eq!(SessionMode::from_str_lenient("accept-edits"), SessionMode::AcceptEdits);
    }
}
