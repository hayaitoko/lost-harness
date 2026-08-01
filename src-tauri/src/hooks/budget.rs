//! C1 — the **budget governor**: a per-profile spend cap that HALTS unattended
//! work (cron / delegate helpers / any headless dispatch) while attended chat
//! only WARNS — the human in front of the app is never hard-blocked mid-thought.
//!
//! This is not a `GatingHook`: a `PreToolUse` hook only fires on TOOL dispatch,
//! but a plain chat turn spends money without ever calling a tool, and the
//! `ToolDispatcher` holds no storage handle to read the ledger. The cost is
//! booked at the MODEL-call layer (`agent::loop_mod` / `agent::work_runner`),
//! which is where the check belongs — both already hold `Arc<Storage>`/
//! `Arc<ProfileDb>`. This module is the PURE decision the caller consults there.
//!
//! **Unknown cost warns, never halts** (the honesty invariant `usage_summary`
//! encodes, applied without lying in either direction): a cloud call whose
//! price we couldn't determine is booked `cost_usd = NULL` and counted in
//! `unknown_cost_calls` — never guessed. The cap is enforced against KNOWN
//! spend only; unknown-cost calls surface a [`BudgetVerdict::Warn`] so "we're
//! flying blind on this model" is visible, but they never terminally halt a
//! run. (The previous fail-closed Halt meant any model absent from the small
//! pricing table — most OpenRouter ids, every niche model — killed a capped
//! unattended run after its first round, so multi-round tool tasks could never
//! finish. Product decision: an unpriced model is a visibility gap, not a
//! reason to break delegation/cron outright.)

use crate::storage::UsageSummary;

/// What the governor decides for a model call about to happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetVerdict {
    /// Under the cap (or uncapped) — proceed.
    Ok,
    /// Proceed, but surface a non-blocking warning. Two ways here: (1) KNOWN
    /// spend is over the cap but a HUMAN is present (attended chat is never
    /// hard-blocked mid-thought); (2) some spend is untracked (unpriced model)
    /// while known spend is still under the cap — attended or not, that is a
    /// visibility warning, never a terminal stop.
    Warn(String),
    /// KNOWN spend has reached/exceeded the cap on UNATTENDED work — stop
    /// before the model call fires. The caller finalizes the work item
    /// `Failed` with this reason. This is the ONLY halting condition; an
    /// unknown-cost call alone never halts (see the module docs).
    Halt(String),
}

/// The pure governor decision. `cap = None` → uncapped → always [`BudgetVerdict::Ok`].
/// KNOWN spend meeting/exceeding the cap is a `Warn` when `attended`, a `Halt`
/// when not. Unknown-cost calls with known spend still under the cap are a
/// `Warn` regardless of attendance — the cap is enforced against known spend
/// only, and untracked spend is surfaced, never a reason to kill a run.
pub fn evaluate(cap: Option<f64>, summary: &UsageSummary, attended: bool) -> BudgetVerdict {
    let Some(cap) = cap else {
        return BudgetVerdict::Ok;
    };
    if summary.known_cost_usd >= cap {
        let reason = format!(
            "this profile has reached its ${:.2} spend cap (${:.2} known this period)",
            cap, summary.known_cost_usd
        );
        return if attended {
            BudgetVerdict::Warn(reason)
        } else {
            BudgetVerdict::Halt(reason)
        };
    }
    if summary.unknown_cost_calls > 0 {
        // A model outside the pricing table books `cost_usd = NULL`. The old
        // behavior halted an unattended run here (fail-closed), which killed
        // every capped multi-round task on an unpriced model after round 0 —
        // see the module docs for the policy decision. Warn-only, all callers.
        return BudgetVerdict::Warn(format!(
            "spend is untracked for {} call(s) this period (model not in the pricing table) — \
             the ${:.2} cap is enforced against the ${:.2} of known spend only",
            summary.unknown_cost_calls, cap, summary.known_cost_usd
        ));
    }
    BudgetVerdict::Ok
}

/// The window boundary the governor sizes spend against: the start (Unix
/// seconds) of the CURRENT calendar month in UTC. (A monthly cap is the useful
/// product behavior vs. an all-time total; flagged as a design choice.) Takes
/// `now` explicitly so it's pure/testable.
pub fn month_start_ts(now: chrono::DateTime<chrono::Utc>) -> i64 {
    use chrono::{Datelike, TimeZone, Utc};
    Utc.with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .single()
        .map(|d| d.timestamp())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(known: f64, unknown: usize) -> UsageSummary {
        UsageSummary {
            total_calls: 1,
            known_cost_usd: known,
            unknown_cost_calls: unknown,
        }
    }

    #[test]
    fn uncapped_is_always_ok() {
        assert_eq!(
            evaluate(None, &summary(9999.0, 5), false),
            BudgetVerdict::Ok
        );
        assert_eq!(evaluate(None, &summary(9999.0, 5), true), BudgetVerdict::Ok);
    }

    #[test]
    fn under_cap_with_known_cost_is_ok() {
        assert_eq!(
            evaluate(Some(10.0), &summary(3.0, 0), false),
            BudgetVerdict::Ok
        );
        assert_eq!(
            evaluate(Some(10.0), &summary(3.0, 0), true),
            BudgetVerdict::Ok
        );
    }

    #[test]
    fn over_cap_warns_when_attended_halts_when_not() {
        assert!(matches!(
            evaluate(Some(5.0), &summary(6.0, 0), true),
            BudgetVerdict::Warn(_)
        ));
        assert!(matches!(
            evaluate(Some(5.0), &summary(6.0, 0), false),
            BudgetVerdict::Halt(_)
        ));
        // Exactly at the cap counts as reached.
        assert!(matches!(
            evaluate(Some(5.0), &summary(5.0, 0), false),
            BudgetVerdict::Halt(_)
        ));
    }

    #[test]
    fn unknown_cost_warns_but_never_halts_under_the_known_cap() {
        // Known spend is under the cap but a call has unknown cost (unpriced
        // model). Policy: this is a visibility WARNING, never a terminal halt
        // — the old fail-closed Halt killed every capped multi-round run on a
        // model outside the pricing table after its first round.
        let unattended = evaluate(Some(50.0), &summary(1.0, 1), false);
        match unattended {
            BudgetVerdict::Warn(reason) => assert!(
                reason.contains("untracked"),
                "the warning must say spend is untracked, got: {reason}"
            ),
            other => panic!("unattended unknown-cost must WARN, got {other:?}"),
        }
        // Attended warns too (same visibility signal).
        assert!(matches!(
            evaluate(Some(50.0), &summary(1.0, 1), true),
            BudgetVerdict::Warn(_)
        ));
    }

    #[test]
    fn known_spend_over_the_cap_still_halts_even_with_unknown_calls_present() {
        // The warn-only unknown policy must NOT weaken real cap enforcement:
        // once KNOWN spend reaches the cap, unattended work halts regardless
        // of any unknown-cost calls alongside it.
        assert!(matches!(
            evaluate(Some(5.0), &summary(6.0, 3), false),
            BudgetVerdict::Halt(_)
        ));
    }

    #[test]
    fn month_start_is_the_first_of_the_month_at_midnight_utc() {
        use chrono::{TimeZone, Utc};
        let mid_month = Utc.with_ymd_and_hms(2026, 7, 22, 14, 30, 0).unwrap();
        let start = month_start_ts(mid_month);
        assert_eq!(
            start,
            Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0)
                .unwrap()
                .timestamp()
        );
    }
}
