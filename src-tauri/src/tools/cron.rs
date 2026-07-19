//! Cron-management tools (PLAN §8 M3 item 10) — the agent's control over this
//! profile's scheduled jobs. Split by risk, matching the house idiom
//! (`recall_memory`/`session_search` read = Safe; `remember`/`write_file`
//! mutate = Write):
//!
//! * [`ListCronJobsTool`] (`list_cron_jobs`) — read-only, `RiskClass::Safe` ⇒
//!   pre-trusted; lists this profile's jobs.
//! * [`ManageCronTool`] (`manage_cron`) — creates / enables / disables /
//!   deletes a job; `RiskClass::Dangerous`. Creating/removing *standing,
//!   autonomous, recurring* automation is a higher-blast-radius act than a
//!   local file edit: the job re-runs the agent unattended on a schedule with
//!   an (agent-authorable) prompt. So it must always be confirmed explicitly
//!   and can never earn a standing grant — `Dangerous` structurally forces
//!   Once-only Ask (Q8 matrix, invariant #8) and, crucially, keeps the
//!   `accept_edits` session mode (which blanket-approves `Write`) from ever
//!   silently creating a job.
//!
//! **Profile-scoped**: cron jobs live in the active profile's transcript DB
//! (`ctx.profile`), never crossing a profile boundary — the same store
//! `session_search` reads. No scheduler *runs* these yet (that is the
//! one-queue-model unification pass, Wave 4.4); this is the CRUD surface the
//! agent uses to express scheduling intent, validated so it can never persist
//! an unrunnable schedule.

use std::future::Future;
use std::pin::Pin;

use serde_json::json;

use crate::storage::{CronJob, Storage};
use crate::tools::{Capability, ExecCtx, RiskClass, Tool, ToolInput, ToolResult};

/// Cap the number of jobs a single `create` batch could imply — a defensive
/// bound so a runaway agent can't flood the table. One call creates one job;
/// this bounds the *total* jobs per profile.
const MAX_JOBS_PER_PROFILE: usize = 64;

/// Month name→number (JAN=1..DEC=12) — accepted in the month field, as
/// standard `crontab(5)` does. Matched case-insensitively.
const MONTH_NAMES: &[(&str, u32)] = &[
    ("JAN", 1), ("FEB", 2), ("MAR", 3), ("APR", 4), ("MAY", 5), ("JUN", 6),
    ("JUL", 7), ("AUG", 8), ("SEP", 9), ("OCT", 10), ("NOV", 11), ("DEC", 12),
];
/// Day-of-week name→number (SUN=0..SAT=6) — accepted in the day-of-week field.
const DAY_NAMES: &[(&str, u32)] = &[
    ("SUN", 0), ("MON", 1), ("TUE", 2), ("WED", 3), ("THU", 4), ("FRI", 5), ("SAT", 6),
];

/// Validate a cron `schedule` string so the agent can never persist an
/// unrunnable job. Accepts either a `@`-macro (`@daily`, `@hourly`, …) or a
/// standard 5-field expression `min hour day-of-month month day-of-week`, with
/// each field a comma-list of `*`, a number, a range `a-b`, or a step
/// (`*/n`, `a-b/n`, `a/n`), range-checked to the field's bounds. The month and
/// day-of-week fields also accept 3-letter names (`JAN`…`DEC`, `SUN`…`SAT`,
/// case-insensitive) including in ranges (`MON-FRI`). Returns a human-readable
/// reason on rejection.
pub(crate) fn validate_cron(schedule: &str) -> Result<(), String> {
    let s = schedule.trim();
    if s.is_empty() {
        return Err("schedule is empty".to_string());
    }
    // Named macros the common schedulers accept.
    if let Some(macro_name) = s.strip_prefix('@') {
        const MACROS: &[&str] = &[
            "yearly", "annually", "monthly", "weekly", "daily", "midnight", "hourly",
        ];
        return if MACROS.contains(&macro_name) {
            Ok(())
        } else {
            Err(format!(
                "unknown schedule macro \"@{macro_name}\" (allowed: @yearly @annually \
                 @monthly @weekly @daily @midnight @hourly)"
            ))
        };
    }

    let fields: Vec<&str> = s.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(format!(
            "cron schedule must have exactly 5 fields \
             (min hour day-of-month month day-of-week), got {}",
            fields.len()
        ));
    }
    // (lo, hi) inclusive bounds per field.
    const BOUNDS: [(u32, u32); 5] = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)];
    const LABELS: [&str; 5] = ["minute", "hour", "day-of-month", "month", "day-of-week"];
    // Name tables apply only to month (idx 3) and day-of-week (idx 4).
    let names: [&[(&str, u32)]; 5] = [&[], &[], &[], MONTH_NAMES, DAY_NAMES];
    for (i, field) in fields.iter().enumerate() {
        validate_cron_field(field, BOUNDS[i], LABELS[i], names[i])?;
    }
    Ok(())
}

/// Validate one comma-separated cron field against its `(lo, hi)` bounds.
/// `names` is a name→value table (month/day fields) or empty (numeric only).
fn validate_cron_field(
    field: &str,
    (lo, hi): (u32, u32),
    label: &str,
    names: &[(&str, u32)],
) -> Result<(), String> {
    if field.is_empty() {
        return Err(format!("{label} field is empty"));
    }
    for part in field.split(',') {
        // Split off an optional step (`*/5`, `1-10/2`, `5/15`).
        let (base, step) = match part.split_once('/') {
            Some((b, s)) => {
                let n: u32 = s
                    .parse()
                    .map_err(|_| format!("{label}: invalid step \"/{s}\""))?;
                if n == 0 {
                    return Err(format!("{label}: step must be > 0"));
                }
                (b, Some(n))
            }
            None => (part, None),
        };
        // The base is `*`, a single value/name, or a range.
        if base == "*" {
            continue;
        }
        match base.split_once('-') {
            Some((a, b)) => {
                let av = parse_cron_num(a, (lo, hi), label, names)?;
                let bv = parse_cron_num(b, (lo, hi), label, names)?;
                if av > bv {
                    return Err(format!("{label}: range {av}-{bv} is inverted"));
                }
            }
            None => {
                // A bare step with no range needs a start value (`5/15`); a
                // bare value with no step is just a value.
                parse_cron_num(base, (lo, hi), label, names)?;
                let _ = step; // step on a single value is accepted (e.g. `5/15`).
            }
        }
    }
    Ok(())
}

/// Parse a cron token as a number, or (for month/day fields) a 3-letter name,
/// then range-check it. Names are matched case-insensitively.
fn parse_cron_num(
    tok: &str,
    (lo, hi): (u32, u32),
    label: &str,
    names: &[(&str, u32)],
) -> Result<u32, String> {
    if let Ok(v) = tok.parse::<u32>() {
        if v < lo || v > hi {
            return Err(format!("{label}: {v} out of range {lo}-{hi}"));
        }
        return Ok(v);
    }
    // Not numeric — try a name (month/day fields only).
    if !names.is_empty() {
        let upper = tok.to_ascii_uppercase();
        if let Some((_, v)) = names.iter().find(|(n, _)| *n == upper) {
            return Ok(*v);
        }
    }
    Err(format!(
        "{label}: \"{tok}\" is not a number, name, `*`, range, or step"
    ))
}

// ── cron matcher (Wave 4.4 — "is this schedule due at this minute?") ─────────

/// The 5-field expansion of an `@macro`, or `None` for a plain expression.
fn macro_to_fields(macro_name: &str) -> Option<[&'static str; 5]> {
    Some(match macro_name {
        "yearly" | "annually" => ["0", "0", "1", "1", "*"],
        "monthly" => ["0", "0", "1", "*", "*"],
        "weekly" => ["0", "0", "*", "*", "0"],
        "daily" | "midnight" => ["0", "0", "*", "*", "*"],
        "hourly" => ["0", "*", "*", "*", "*"],
        _ => return None,
    })
}

/// Does a single cron `field` (a comma-list of `*` / value / range / step /
/// name) match `value`? Assumes a validated field (invalid parts are treated as
/// "no match", never a panic).
fn cron_field_matches(field: &str, value: u32, (lo, hi): (u32, u32), names: &[(&str, u32)]) -> bool {
    for part in field.split(',') {
        let (base, step) = match part.split_once('/') {
            Some((b, s)) => match s.parse::<u32>() {
                Ok(n) if n > 0 => (b, n),
                _ => continue,
            },
            None => (part, 1),
        };
        let (start, end) = if base == "*" {
            (lo, hi)
        } else if let Some((a, b)) = base.split_once('-') {
            match (
                parse_cron_num(a, (lo, hi), "", names),
                parse_cron_num(b, (lo, hi), "", names),
            ) {
                (Ok(a), Ok(b)) if a <= b => (a, b),
                _ => continue,
            }
        } else {
            match parse_cron_num(base, (lo, hi), "", names) {
                // A bare value with a step (`5/15`) runs from the value to `hi`;
                // a bare value alone matches only itself.
                Ok(v) => {
                    if part.contains('/') {
                        (v, hi)
                    } else {
                        (v, v)
                    }
                }
                Err(_) => continue,
            }
        };
        if value >= start && value <= end && (value - start) % step == 0 {
            return true;
        }
    }
    false
}

/// Is `schedule` due to fire at the given local wall-clock minute? Standard cron
/// semantics, including the day-of-month / day-of-week OR rule: when BOTH are
/// restricted (neither is `*`), the day matches if EITHER field matches. `dow`
/// value 0 and 7 both mean Sunday.
pub(crate) fn cron_due(schedule: &str, dt: chrono::DateTime<chrono::Local>) -> bool {
    use chrono::{Datelike, Timelike};
    let s = schedule.trim();
    let fields: Vec<&str> = if let Some(m) = s.strip_prefix('@') {
        match macro_to_fields(m) {
            Some(f) => f.to_vec(),
            None => return false,
        }
    } else {
        s.split_whitespace().collect()
    };
    if fields.len() != 5 {
        return false;
    }
    const BOUNDS: [(u32, u32); 5] = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)];
    if !cron_field_matches(fields[0], dt.minute(), BOUNDS[0], &[]) {
        return false;
    }
    if !cron_field_matches(fields[1], dt.hour(), BOUNDS[1], &[]) {
        return false;
    }
    if !cron_field_matches(fields[3], dt.month(), BOUNDS[3], MONTH_NAMES) {
        return false;
    }
    let dow = dt.weekday().num_days_from_sunday(); // 0=Sun .. 6=Sat
    let dom_restricted = fields[2] != "*";
    let dow_restricted = fields[4] != "*";
    let dom_match = cron_field_matches(fields[2], dt.day(), BOUNDS[2], &[]);
    let dow_match = cron_field_matches(fields[4], dow, BOUNDS[4], DAY_NAMES)
        || (dow == 0 && cron_field_matches(fields[4], 7, BOUNDS[4], DAY_NAMES));
    let day_ok = match (dom_restricted, dow_restricted) {
        (true, true) => dom_match || dow_match,
        (true, false) => dom_match,
        (false, true) => dow_match,
        (false, false) => true,
    };
    day_ok
}

fn job_json(j: &CronJob) -> serde_json::Value {
    json!({
        "id": j.id,
        "name": j.name,
        "prompt": j.prompt,
        "schedule": j.schedule,
        "enabled": j.enabled,
        "last_run_at": j.last_run_at,
        "last_status": j.last_status,
        "target_conversation_id": j.target_conversation_id,
    })
}

// ── list_cron_jobs (Safe, read-only) ─────────────────────────────────────────

/// Read-only listing of the active profile's scheduled jobs.
pub struct ListCronJobsTool {
    storage: Storage,
}

impl ListCronJobsTool {
    pub fn new(storage: Storage) -> Self {
        Self { storage }
    }
}

impl Tool for ListCronJobsTool {
    fn name(&self) -> &str {
        "list_cron_jobs"
    }

    fn description(&self) -> &str {
        "List this user's scheduled jobs (cron). No args. Returns each job's id, \
         name, prompt, schedule, and whether it's enabled."
    }

    fn requires(&self) -> &[Capability] {
        &[]
    }

    // risk() defaults to Safe (read-only, on-device) → pre-trusted.

    fn run<'a>(
        &'a self,
        _input: ToolInput,
        ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let db = match self.storage.open_profile(&ctx.profile) {
                Ok(d) => d,
                Err(e) => return ToolResult::Err(format!("list_cron_jobs failed: {e}")),
            };
            match db.list_cron_jobs() {
                Ok(jobs) => {
                    let arr: Vec<_> = jobs.iter().map(job_json).collect();
                    ToolResult::Ok(json!({ "jobs": arr }))
                }
                Err(e) => ToolResult::Err(format!("list_cron_jobs failed: {e}")),
            }
        })
    }
}

// ── manage_cron (Write) ──────────────────────────────────────────────────────

/// Mutating cron management: create / enable / disable / delete a job.
pub struct ManageCronTool {
    storage: Storage,
}

impl ManageCronTool {
    pub fn new(storage: Storage) -> Self {
        Self { storage }
    }
}

impl Tool for ManageCronTool {
    fn name(&self) -> &str {
        "manage_cron"
    }

    fn description(&self) -> &str {
        "Create, enable, disable, or delete a scheduled job for this user. \
         args: {\"action\":\"create\",\"name\":..,\"prompt\":..,\"schedule\":\"0 9 * * *\"} \
         | {\"action\":\"enable\"|\"disable\"|\"delete\",\"id\":..}. \
         `schedule` is a 5-field cron expression or an @macro (@daily, @hourly, …)."
    }

    fn requires(&self) -> &[Capability] {
        &[]
    }

    fn risk(&self) -> RiskClass {
        // Standing autonomous automation = high blast radius. `Dangerous` forces
        // Once-only Ask (no standing grant) AND stops `accept_edits` (which
        // blanket-approves Write) from silently creating/deleting jobs.
        RiskClass::Dangerous
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["create", "enable", "disable", "delete"] },
                "id": { "type": "string", "description": "job id (enable/disable/delete)" },
                "name": { "type": "string", "description": "job name (create)" },
                "prompt": { "type": "string", "description": "what the job should do (create)" },
                "schedule": { "type": "string", "description": "5-field cron or @macro (create)" },
                "target_conversation_id": { "type": "string", "description": "optional conversation to post into (create)" }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    fn run<'a>(
        &'a self,
        input: ToolInput,
        ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let action = match input.args.get("action").and_then(|v| v.as_str()) {
                Some(a) => a,
                None => {
                    return ToolResult::Err(
                        "manage_cron requires an \"action\" (create|enable|disable|delete)"
                            .to_string(),
                    )
                }
            };
            let db = match self.storage.open_profile(&ctx.profile) {
                Ok(d) => d,
                Err(e) => return ToolResult::Err(format!("manage_cron failed: {e}")),
            };

            match action {
                "create" => {
                    let name = match req_str(&input.args, "name") {
                        Ok(s) => s,
                        Err(e) => return ToolResult::Err(e),
                    };
                    let prompt = match req_str(&input.args, "prompt") {
                        Ok(s) => s,
                        Err(e) => return ToolResult::Err(e),
                    };
                    let schedule = match req_str(&input.args, "schedule") {
                        Ok(s) => s,
                        Err(e) => return ToolResult::Err(e),
                    };
                    if let Err(e) = validate_cron(&schedule) {
                        return ToolResult::Err(format!("invalid schedule: {e}"));
                    }
                    // Bound the table so a runaway agent can't flood it.
                    match db.list_cron_jobs() {
                        Ok(existing) if existing.len() >= MAX_JOBS_PER_PROFILE => {
                            return ToolResult::Err(format!(
                                "cannot create: this profile already has {} scheduled jobs (max {})",
                                existing.len(),
                                MAX_JOBS_PER_PROFILE
                            ));
                        }
                        Ok(_) => {}
                        Err(e) => return ToolResult::Err(format!("manage_cron failed: {e}")),
                    }
                    // An optional target conversation must exist in THIS profile.
                    let target = match input
                        .args
                        .get("target_conversation_id")
                        .and_then(|v| v.as_str())
                    {
                        Some(cid) if !cid.trim().is_empty() => {
                            let cid = cid.trim().to_string();
                            match db.get_conversation(&cid) {
                                Ok(Some(_)) => Some(cid),
                                Ok(None) => {
                                    return ToolResult::Err(format!(
                                        "target_conversation_id \"{cid}\" not found in this profile"
                                    ))
                                }
                                Err(e) => return ToolResult::Err(format!("manage_cron failed: {e}")),
                            }
                        }
                        _ => None,
                    };
                    let job = CronJob {
                        id: uuid::Uuid::new_v4().to_string(),
                        name,
                        prompt,
                        schedule,
                        enabled: true,
                        last_run_at: None,
                        last_status: None,
                        target_conversation_id: target,
                    };
                    match db.insert_cron_job(&job) {
                        Ok(()) => ToolResult::Ok(json!({
                            "action": "create",
                            "created": true,
                            "job": job_json(&job),
                        })),
                        Err(e) => ToolResult::Err(format!("manage_cron failed: {e}")),
                    }
                }
                "enable" | "disable" => {
                    let id = match req_str(&input.args, "id") {
                        Ok(s) => s,
                        Err(e) => return ToolResult::Err(e),
                    };
                    let enabled = action == "enable";
                    match db.set_cron_job_enabled(&id, enabled) {
                        Ok(true) => ToolResult::Ok(json!({
                            "action": action, "id": id, "enabled": enabled,
                        })),
                        Ok(false) => {
                            ToolResult::Err(format!("no scheduled job with id \"{id}\""))
                        }
                        Err(e) => ToolResult::Err(format!("manage_cron failed: {e}")),
                    }
                }
                "delete" => {
                    let id = match req_str(&input.args, "id") {
                        Ok(s) => s,
                        Err(e) => return ToolResult::Err(e),
                    };
                    match db.delete_cron_job(&id) {
                        Ok(true) => ToolResult::Ok(json!({
                            "action": "delete", "id": id, "deleted": true,
                        })),
                        Ok(false) => {
                            ToolResult::Err(format!("no scheduled job with id \"{id}\""))
                        }
                        Err(e) => ToolResult::Err(format!("manage_cron failed: {e}")),
                    }
                }
                other => ToolResult::Err(format!(
                    "unknown action \"{other}\" (create|enable|disable|delete)"
                )),
            }
        })
    }
}

/// Pull a required non-empty string arg, or a usage error naming the field.
fn req_str(args: &serde_json::Value, key: &str) -> Result<String, String> {
    match args.get(key).and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        _ => Err(format!("manage_cron requires a non-empty string \"{key}\" arg")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Conversation;
    use chrono::TimeZone;

    /// A local DateTime for a specific wall-clock (year, month, day, hour, min).
    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> chrono::DateTime<chrono::Local> {
        chrono::Local.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn cron_due_matches_minute_hour_and_wildcards() {
        // 2026-07-15 is a Wednesday.
        let wed_0930 = at(2026, 7, 15, 9, 30);
        assert!(cron_due("30 9 * * *", wed_0930), "exact minute+hour, wildcards");
        assert!(!cron_due("31 9 * * *", wed_0930), "wrong minute");
        assert!(!cron_due("30 10 * * *", wed_0930), "wrong hour");
        assert!(cron_due("* * * * *", wed_0930), "every minute");
        assert!(cron_due("@hourly", at(2026, 7, 15, 9, 0)), "@hourly fires at :00");
        assert!(!cron_due("@hourly", wed_0930), "@hourly doesn't fire at :30");
        assert!(cron_due("@daily", at(2026, 7, 15, 0, 0)), "@daily at midnight");
    }

    #[test]
    fn cron_due_handles_lists_ranges_steps_and_names() {
        assert!(cron_due("0,30 * * * *", at(2026, 7, 15, 9, 30)), "list");
        assert!(cron_due("*/15 * * * *", at(2026, 7, 15, 9, 45)), "step /15 at :45");
        assert!(!cron_due("*/15 * * * *", at(2026, 7, 15, 9, 46)), "step /15 not at :46");
        assert!(cron_due("0 9-17 * * *", at(2026, 7, 15, 14, 0)), "hour range");
        assert!(!cron_due("0 9-17 * * *", at(2026, 7, 15, 18, 0)), "outside hour range");
        assert!(cron_due("0 0 * JUL *", at(2026, 7, 1, 0, 0)), "month by name");
        assert!(cron_due("0 0 * * WED", at(2026, 7, 15, 0, 0)), "weekday by name (Wed)");
    }

    #[test]
    fn cron_due_dom_dow_or_semantics_and_sunday_0_or_7() {
        // When BOTH dom and dow are restricted, EITHER matching fires it.
        // 2026-07-15 is Wed the 15th. "1,15 of month OR Monday" → the 15th matches.
        assert!(cron_due("0 0 15 * MON", at(2026, 7, 15, 0, 0)), "dom matches (OR)");
        // 2026-07-13 is a Monday, not the 15th → dow matches (OR).
        assert!(cron_due("0 0 15 * MON", at(2026, 7, 13, 0, 0)), "dow matches (OR)");
        // 2026-07-14 (Tue, 14th) → neither → no fire.
        assert!(!cron_due("0 0 15 * MON", at(2026, 7, 14, 0, 0)), "neither → no fire");
        // Sunday: both 0 and 7 mean Sunday. 2026-07-19 is a Sunday.
        assert!(cron_due("0 0 * * 0", at(2026, 7, 19, 0, 0)), "dow 0 = Sunday");
        assert!(cron_due("0 0 * * 7", at(2026, 7, 19, 0, 0)), "dow 7 = Sunday");
    }

    fn temp_storage() -> (Storage, std::path::PathBuf) {
        let mut root = std::env::temp_dir();
        root.push(format!("lhp-cron-{}", uuid::Uuid::new_v4()));
        let storage = Storage::open(&root).unwrap();
        (storage, root)
    }

    fn ctx_for(profile: &str) -> ExecCtx {
        ExecCtx {
            profile: profile.into(),
            ..ExecCtx::default()
        }
    }

    #[test]
    fn cron_validation_accepts_valid_and_rejects_garbage() {
        // Valid forms.
        for ok in [
            "* * * * *",
            "0 9 * * *",
            "*/15 0-8 1,15 * 1-5",
            "0 0 1 1 0",
            "@daily",
            "@hourly",
            "5/15 * * * *",
            "0 9 * * MON-FRI",     // named weekday range (very common)
            "0 0 1 JAN *",         // named month
            "30 8 * * mon,wed,fri", // lowercase names in a list
            "0 12 * DEC SUN",      // named month + named day
        ] {
            assert!(validate_cron(ok).is_ok(), "should accept {ok:?}");
        }
        // Invalid forms.
        for bad in [
            "",
            "every day",       // prose
            "0 9 * *",         // 4 fields
            "0 9 * * * *",     // 6 fields
            "99 * * * *",      // minute out of range
            "* 24 * * *",      // hour out of range
            "* * 0 * *",       // day-of-month below 1
            "* * * 13 *",      // month out of range
            "10-5 * * * *",    // inverted range
            "*/0 * * * *",     // zero step
            "@weeklyish",      // unknown macro
            "0 9 * * FOO",     // bogus weekday name
            "0 9 * MON *",     // a weekday name in the MONTH field is not valid
            "MON * * * *",     // a name in the minute field is not valid
        ] {
            assert!(validate_cron(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn manage_cron_is_dangerous_so_accept_edits_cannot_silently_run_it() {
        // Regression for the review's HIGH finding: a mutating standing-automation
        // tool must NOT be Write (which accept_edits blanket-approves).
        let storage_dir = std::env::temp_dir().join(format!("lhp-cron-risk-{}", uuid::Uuid::new_v4()));
        let storage = Storage::open(&storage_dir).unwrap();
        let tool = ManageCronTool::new(storage);
        assert_eq!(tool.risk(), RiskClass::Dangerous);
        assert_eq!(ListCronJobsTool::new(Storage::open(&storage_dir).unwrap()).risk(), RiskClass::Safe);
        let _ = std::fs::remove_dir_all(storage_dir);
    }

    #[tokio::test]
    async fn create_list_toggle_delete_roundtrip_scoped_to_profile() {
        let (storage, root) = temp_storage();
        let create = ManageCronTool::new(storage.clone());
        let list = ListCronJobsTool::new(storage.clone());
        let ctx = ctx_for("personal");

        // Create a job.
        let id = match create
            .run(
                ToolInput::new(json!({
                    "action": "create",
                    "name": "Morning brief",
                    "prompt": "summarize overnight email",
                    "schedule": "0 7 * * *"
                })),
                &ctx,
            )
            .await
        {
            ToolResult::Ok(v) => {
                assert_eq!(v["created"], true);
                assert_eq!(v["job"]["enabled"], true);
                v["job"]["id"].as_str().unwrap().to_string()
            }
            ToolResult::Err(e) => panic!("create failed: {e}"),
        };

        // It lists in this profile.
        match list.run(ToolInput::empty(), &ctx).await {
            ToolResult::Ok(v) => {
                let jobs = v["jobs"].as_array().unwrap();
                assert_eq!(jobs.len(), 1);
                assert_eq!(jobs[0]["name"], "Morning brief");
            }
            ToolResult::Err(e) => panic!("list failed: {e}"),
        }

        // A DIFFERENT profile sees no jobs (profile-scoped).
        match list.run(ToolInput::empty(), &ctx_for("work")).await {
            ToolResult::Ok(v) => assert!(v["jobs"].as_array().unwrap().is_empty()),
            ToolResult::Err(e) => panic!("list failed: {e}"),
        }

        // Disable it.
        match create
            .run(ToolInput::new(json!({"action": "disable", "id": id})), &ctx)
            .await
        {
            ToolResult::Ok(v) => assert_eq!(v["enabled"], false),
            ToolResult::Err(e) => panic!("disable failed: {e}"),
        }
        match list.run(ToolInput::empty(), &ctx).await {
            ToolResult::Ok(v) => assert_eq!(v["jobs"][0]["enabled"], false),
            ToolResult::Err(e) => panic!("list failed: {e}"),
        }

        // Delete it.
        match create
            .run(ToolInput::new(json!({"action": "delete", "id": id})), &ctx)
            .await
        {
            ToolResult::Ok(v) => assert_eq!(v["deleted"], true),
            ToolResult::Err(e) => panic!("delete failed: {e}"),
        }
        match list.run(ToolInput::empty(), &ctx).await {
            ToolResult::Ok(v) => assert!(v["jobs"].as_array().unwrap().is_empty()),
            ToolResult::Err(e) => panic!("list failed: {e}"),
        }

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn create_rejects_bad_schedule_and_missing_args() {
        let (storage, root) = temp_storage();
        let create = ManageCronTool::new(storage.clone());
        let ctx = ctx_for("personal");

        // Bad schedule → rejected, nothing persisted.
        assert!(matches!(
            create
                .run(
                    ToolInput::new(json!({
                        "action": "create", "name": "x", "prompt": "y", "schedule": "soon"
                    })),
                    &ctx
                )
                .await,
            ToolResult::Err(_)
        ));
        // Missing name → usage error.
        assert!(matches!(
            create
                .run(
                    ToolInput::new(json!({
                        "action": "create", "prompt": "y", "schedule": "@daily"
                    })),
                    &ctx
                )
                .await,
            ToolResult::Err(_)
        ));
        // Nothing got created.
        let list = ListCronJobsTool::new(storage.clone());
        match list.run(ToolInput::empty(), &ctx).await {
            ToolResult::Ok(v) => assert!(v["jobs"].as_array().unwrap().is_empty()),
            ToolResult::Err(e) => panic!("list failed: {e}"),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn toggle_and_delete_report_unknown_id() {
        let (storage, root) = temp_storage();
        let create = ManageCronTool::new(storage.clone());
        let ctx = ctx_for("personal");
        for action in ["enable", "disable", "delete"] {
            assert!(matches!(
                create
                    .run(ToolInput::new(json!({"action": action, "id": "nope"})), &ctx)
                    .await,
                ToolResult::Err(_)
            ));
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn create_rejects_a_nonexistent_target_conversation() {
        let (storage, root) = temp_storage();
        let create = ManageCronTool::new(storage.clone());
        let ctx = ctx_for("personal");
        // Missing target conversation → error.
        assert!(matches!(
            create
                .run(
                    ToolInput::new(json!({
                        "action": "create", "name": "x", "prompt": "y",
                        "schedule": "@daily", "target_conversation_id": "ghost"
                    })),
                    &ctx
                )
                .await,
            ToolResult::Err(_)
        ));
        // With a real conversation it succeeds.
        let db = storage.open_profile("personal").unwrap();
        db.create_conversation(&Conversation {
            id: "c1".into(),
            name: "Chat".into(),
            pinned: false,
            binding: "auto".into(),
            folder_id: None,
            color: None,
            created_at: 1,
            updated_at: 1,
        })
        .unwrap();
        match create
            .run(
                ToolInput::new(json!({
                    "action": "create", "name": "x", "prompt": "y",
                    "schedule": "@daily", "target_conversation_id": "c1"
                })),
                &ctx,
            )
            .await
        {
            ToolResult::Ok(v) => assert_eq!(v["job"]["target_conversation_id"], "c1"),
            ToolResult::Err(e) => panic!("create with real target failed: {e}"),
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
