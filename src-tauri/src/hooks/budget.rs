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
//! **Fail-closed on unknown cost** (the honesty invariant `usage_summary`
//! already encodes): a cloud call whose price we couldn't determine is booked
//! `cost_usd = NULL` and counted in `unknown_cost_calls` — never guessed. The
//! governor treats "we don't know what we've spent" as *possibly already over*,
//! so it never lets "unknown" quietly mean "assume it's fine."

use crate::storage::UsageSummary;

/// What the governor decides for a model call about to happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetVerdict {
    /// Under the cap (or uncapped) — proceed.
    Ok,
    /// Over the cap (or spend is unknowable) but a HUMAN is present — proceed,
    /// but surface a non-blocking warning. Attended chat is never hard-blocked.
    Warn(String),
    /// Over the cap (or spend is unknowable) on UNATTENDED work — stop before
    /// the model call fires. The caller finalizes the work item `Failed` with
    /// this reason.
    Halt(String),
}

/// The pure governor decision. `cap = None` → uncapped → always [`BudgetVerdict::Ok`].
/// Otherwise "capped" is reached when known spend has met/exceeded the cap OR
/// any spend is unknown-flagged (fail-closed). A capped verdict is a `Warn` when
/// `attended`, a `Halt` when not.
pub fn evaluate(cap: Option<f64>, summary: &UsageSummary, attended: bool) -> BudgetVerdict {
    let Some(cap) = cap else {
        return BudgetVerdict::Ok;
    };
    let over_known = summary.known_cost_usd >= cap;
    let unknown = summary.unknown_cost_calls > 0;
    if !over_known && !unknown {
        return BudgetVerdict::Ok;
    }
    let reason = if unknown && !over_known {
        format!(
            "spend can't be verified against the ${:.2} cap ({} call(s) have an unknown cost) — \
             treating the budget as possibly exceeded (fail-closed)",
            cap, summary.unknown_cost_calls
        )
    } else {
        format!(
            "this profile has reached its ${:.2} spend cap (${:.2} known this period)",
            cap, summary.known_cost_usd
        )
    };
    if attended {
        BudgetVerdict::Warn(reason)
    } else {
        BudgetVerdict::Halt(reason)
    }
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
    fn unknown_cost_fails_closed_even_under_the_known_cap() {
        // Known spend is well under the cap, but a call has unknown cost →
        // capped branch (possibly already over).
        assert!(matches!(
            evaluate(Some(50.0), &summary(1.0, 1), false),
            BudgetVerdict::Halt(_)
        ));
        // ...and attended still only WARNS, never halts the human.
        assert!(matches!(
            evaluate(Some(50.0), &summary(1.0, 1), true),
            BudgetVerdict::Warn(_)
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
