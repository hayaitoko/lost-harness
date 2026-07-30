//! Tests for the self-update gate.
//!
//! The load-bearing one is `toggle_off_makes_no_request`: the spec's acceptance
//! criterion is that a disabled launch check produces *zero* update-related
//! egress, and the way that is proven here is that the only I/O-capable
//! argument `run_launch_check` takes — the fetch closure — is never called.
//! An assertion on a counter that stayed at zero is a real proof precisely
//! because the closure is the sole path to a socket in that function.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn info(version: &str) -> UpdateInfo {
    UpdateInfo {
        version: version.to_string(),
        current_version: "0.1.0".to_string(),
        notes: None,
        date: None,
    }
}

// ── The gate ────────────────────────────────────────────────────────────────

#[test]
fn gate_permits_a_check_only_for_a_release_build_with_the_toggle_on() {
    assert_eq!(launch_check_decision(false, true), LaunchDecision::Check);
    assert_eq!(
        launch_check_decision(false, false),
        LaunchDecision::Skip(SkipReason::ToggleOff)
    );
    assert_eq!(
        launch_check_decision(true, true),
        LaunchDecision::Skip(SkipReason::DevBuild)
    );
    // A dev build reports the structural reason even with the toggle off, so a
    // developer is never told "you turned it off" when the build would have
    // skipped regardless.
    assert_eq!(
        launch_check_decision(true, false),
        LaunchDecision::Skip(SkipReason::DevBuild)
    );
}

#[tokio::test]
async fn toggle_off_makes_no_request() {
    let calls = AtomicUsize::new(0);

    let outcome = run_launch_check(false, false, || async {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(Some(info("9.9.9")))
    })
    .await;

    assert_eq!(outcome, LaunchOutcome::Skipped(SkipReason::ToggleOff));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the launch check must not even ATTEMPT a request when the toggle is off"
    );
}

#[tokio::test]
async fn dev_build_makes_no_request_even_with_the_toggle_on() {
    let calls = AtomicUsize::new(0);

    let outcome = run_launch_check(true, true, || async {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(Some(info("9.9.9")))
    })
    .await;

    assert_eq!(outcome, LaunchOutcome::Skipped(SkipReason::DevBuild));
    assert_eq!(calls.load(Ordering::SeqCst), 0, "dev builds skip the check");
}

#[tokio::test]
async fn release_build_with_the_toggle_on_makes_exactly_one_request() {
    let calls = AtomicUsize::new(0);

    let outcome = run_launch_check(false, true, || async {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(Some(info("0.1.1")))
    })
    .await;

    assert_eq!(outcome, LaunchOutcome::Available(info("0.1.1")));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_up_to_date_check_reports_up_to_date() {
    let outcome = run_launch_check(false, true, || async { Ok(None) }).await;
    assert_eq!(outcome, LaunchOutcome::UpToDate);
}

#[tokio::test]
async fn a_failed_check_is_reported_not_panicked() {
    // Offline, DNS failure, 404, malformed manifest — all land here, and all
    // must stay quiet rather than become a startup error dialog.
    let outcome = run_launch_check(false, true, || async { Err("offline".to_string()) }).await;
    assert_eq!(outcome, LaunchOutcome::Failed("offline".to_string()));
}

// ── Version comparison ──────────────────────────────────────────────────────

#[test]
fn only_a_strictly_newer_version_is_an_update() {
    assert!(is_strictly_newer("0.1.0", "0.1.1"));
    assert!(is_strictly_newer("0.1.0", "0.2.0"));
    assert!(is_strictly_newer("0.9.9", "1.0.0"));
    assert!(is_strictly_newer("1.0.0-beta.1", "1.0.0"));

    // Same version is not an update.
    assert!(!is_strictly_newer("0.1.0", "0.1.0"));
    // A DOWNGRADE announced by a manifest must never produce a banner.
    assert!(!is_strictly_newer("0.2.0", "0.1.9"));
    assert!(!is_strictly_newer("1.0.0", "1.0.0-rc.1"));
}

#[test]
fn a_leading_v_is_tolerated_on_either_side() {
    // Tags are `vX.Y.Z`; the manifest carries a bare semver. Accept both so a
    // tag string pasted into a manifest doesn't silently disable updates.
    assert!(is_strictly_newer("0.1.0", "v0.1.1"));
    assert!(is_strictly_newer("v0.1.0", "0.1.1"));
    assert!(!is_strictly_newer("v0.1.1", "v0.1.0"));
}

#[test]
fn an_unparseable_version_is_never_newer() {
    // Refuse rather than guess: a manifest with a junk version string must not
    // be able to talk the app into offering an "update".
    assert!(!is_strictly_newer("0.1.0", "not-a-version"));
    assert!(!is_strictly_newer("0.1.0", ""));
    assert!(!is_strictly_newer("also-junk", "0.9.9"));
    assert!(!is_strictly_newer("0.1.0", "0.1"));
}

// ── The pending-update slot ─────────────────────────────────────────────────

#[test]
fn nothing_is_pending_before_a_check() {
    let pending = PendingUpdate::default();
    assert!(
        pending.take().is_none(),
        "install must refuse when no check has run"
    );
}

// ── Payload shape (the frontend deserializes these) ─────────────────────────

#[test]
fn manual_check_result_serializes_with_a_status_tag() {
    let available = serde_json::to_value(ManualCheckResult::Available(info("0.1.1"))).unwrap();
    assert_eq!(available["status"], "available");
    assert_eq!(available["version"], "0.1.1");
    assert_eq!(available["current_version"], "0.1.0");

    let current = serde_json::to_value(ManualCheckResult::UpToDate {
        current_version: "0.1.0".into(),
    })
    .unwrap();
    assert_eq!(current["status"], "up_to_date");
    assert_eq!(current["current_version"], "0.1.0");
}
