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

// ── The download host ───────────────────────────────────────────────────────
//
// The manifest endpoint is pinned in tauri.conf.json, but `platforms.*.url` is
// remote input. Without these, "updates come from github.com" is a claim about
// the manifest only, and a swapped manifest could send the (still
// signature-checked) download anywhere.

#[test]
fn the_real_release_asset_url_shape_is_permitted() {
    // Exactly what `.github/workflows/build.yml`'s `Stage the updater artifacts`
    // step writes into latest.json. If this fails, tagged releases stop being
    // installable — which is why the workflow now asserts the same thing.
    assert!(is_permitted_download_url(
        "https://github.com/hayaitoko/lost-harness/releases/download/v0.1.1/Lost-Harness_0.1.1_aarch64.app.tar.gz"
    ));
}

#[test]
fn another_host_is_refused_however_plausible() {
    for url in [
        // The bare swap.
        "https://evil.example/lost-harness.app.tar.gz",
        // A lookalike registrable domain.
        "https://github.com.evil.example/hayaitoko/lost-harness/releases/download/v0.1.1/x.app.tar.gz",
        // A subdomain of the real host is still not the real host.
        "https://raw.github.com/hayaitoko/lost-harness/releases/download/v0.1.1/x.app.tar.gz",
        "https://objects.githubusercontent.com/hayaitoko/lost-harness/releases/download/v0.1.1/x.app.tar.gz",
        // Userinfo that reads like the right host to a human skimming a log.
        "https://github.com@evil.example/hayaitoko/lost-harness/releases/download/v0.1.1/x.app.tar.gz",
        // Right host, wrong service.
        "https://github.com:8443/hayaitoko/lost-harness/releases/download/v0.1.1/x.app.tar.gz",
    ] {
        assert!(
            !is_permitted_download_url(url),
            "must refuse an off-host download: {url}"
        );
    }
}

#[test]
fn a_non_https_download_is_refused() {
    for url in [
        "http://github.com/hayaitoko/lost-harness/releases/download/v0.1.1/x.app.tar.gz",
        "file:///tmp/x.app.tar.gz",
        "ftp://github.com/hayaitoko/lost-harness/releases/download/v0.1.1/x.app.tar.gz",
        // Not a URL at all.
        "/hayaitoko/lost-harness/releases/download/v0.1.1/x.app.tar.gz",
        "",
    ] {
        assert!(!is_permitted_download_url(url), "must refuse: {url}");
    }
}

#[test]
fn the_right_host_but_the_wrong_repository_is_refused() {
    // github.com hosts everyone's releases. Being on the right host is not the
    // same as being this project's asset.
    for url in [
        "https://github.com/someone-else/lost-harness/releases/download/v9.9.9/x.app.tar.gz",
        "https://github.com/hayaitoko/some-other-repo/releases/download/v9.9.9/x.app.tar.gz",
        "https://github.com/hayaitoko/lost-harness/raw/main/x.app.tar.gz",
        "https://github.com/",
    ] {
        assert!(!is_permitted_download_url(url), "must refuse: {url}");
    }
}

#[test]
fn a_path_that_climbs_out_of_the_release_assets_is_refused() {
    // Both of these are refused by the PREFIX check, because `Url::parse`
    // collapses the climb before this function ever sees the path — and it does
    // that for the percent-encoded spelling too, which an earlier comment here
    // got backwards. `percent_encoded_dot_segments_are_collapsed_by_the_parser`
    // below pins the parser behaviour so the claim can't rot again.
    assert!(!is_permitted_download_url(
        "https://github.com/hayaitoko/lost-harness/releases/download/../../../../other/x.app.tar.gz"
    ));
    assert!(!is_permitted_download_url(
        "https://github.com/hayaitoko/lost-harness/releases/download/%2e%2e/%2e%2e/x.app.tar.gz"
    ));
}

#[test]
fn percent_encoded_dot_segments_are_collapsed_by_the_parser() {
    // The exact case from the security review. `url` decodes and collapses
    // `%2E%2E` at parse time — it is a "double-dot path segment" in the URL
    // standard, ASCII-case-insensitively, alongside `..`, `.%2e` and `%2e.`.
    // The path that reaches the prefix check has already lost `download/`.
    let parsed =
        url::Url::parse("https://github.com/hayaitoko/lost-harness/releases/download/%2E%2E/x")
            .expect("parses");
    assert_eq!(
        parsed.path(),
        "/hayaitoko/lost-harness/releases/x",
        "if this ever changes, the prefix check stops being what refuses a climb"
    );
    assert!(!is_permitted_download_url(parsed.as_str()));
    assert!(!is_permitted_download_url(
        "https://github.com/hayaitoko/lost-harness/releases/download/%2E%2E/x"
    ));
}

#[test]
fn an_escape_that_survives_parsing_cannot_smuggle_a_climb_past_the_prefix() {
    // These are the ones that matter: each still starts with
    // RELEASE_DOWNLOAD_PREFIX *after* parsing, so the prefix check passes them,
    // and the first four contain no `%2e` at all — the old
    // `path.contains("%2e")` guard let every one of them through.
    for raw in [
        // Encoded SLASH, literal dots. Decodes to `/../../../other/x.tar.gz`.
        "https://github.com/hayaitoko/lost-harness/releases/download/%2f..%2f..%2f..%2fother/x.tar.gz",
        // The same trick hidden behind a plausible-looking tag segment.
        "https://github.com/hayaitoko/lost-harness/releases/download/v1%2f..%2f..%2f..%2fother/x.tar.gz",
        // Encoded backslash.
        "https://github.com/hayaitoko/lost-harness/releases/download/v1/%5c..%5cevil.tar.gz",
        // Double-encoded dots: one decode pass yields `%2e%2e`. Refuse rather
        // than decode again and guess how many passes a fetcher makes.
        "https://github.com/hayaitoko/lost-harness/releases/download/%252e%252e/x.tar.gz",
        // Encoded slash AND encoded dots.
        "https://github.com/hayaitoko/lost-harness/releases/download/%2e%2e%2f%2e%2e/x.tar.gz",
        // A malformed escape can't be read the way a fetcher would read it.
        "https://github.com/hayaitoko/lost-harness/releases/download/v1/%zz.tar.gz",
        "https://github.com/hayaitoko/lost-harness/releases/download/v1/%2",
        // The release-asset directory itself is not a payload.
        "https://github.com/hayaitoko/lost-harness/releases/download/",
        // An empty segment is not an asset name.
        "https://github.com/hayaitoko/lost-harness/releases/download/v1//x.tar.gz",
    ] {
        let parsed = url::Url::parse(raw).expect("parses");
        assert!(
            parsed.path().starts_with(RELEASE_DOWNLOAD_PREFIX),
            "this case is only interesting while it still passes the prefix check: {raw}"
        );
        assert!(!is_permitted_download_url(raw), "must refuse: {raw}");
    }
}

#[test]
fn an_ordinary_asset_name_is_not_caught_by_the_segment_check() {
    // The hardening must not turn away real releases. Dots, dashes and
    // underscores are exactly what CI writes into `latest.json`.
    for raw in [
        "https://github.com/hayaitoko/lost-harness/releases/download/v0.1.1/Lost-Harness_0.1.1_aarch64.app.tar.gz",
        "https://github.com/hayaitoko/lost-harness/releases/download/v0.1.1/Lost-Harness_0.1.1_aarch64.app.tar.gz.sig",
        "https://github.com/hayaitoko/lost-harness/releases/download/v1.0.0-rc.1/x.app.tar.gz",
    ] {
        assert!(is_permitted_download_url(raw), "must permit: {raw}");
    }
}

#[test]
fn a_refusal_is_identifiable_to_the_frontend() {
    // `check_for_update` hands Svelte a bare String, so this refusal and "GitHub
    // is unreachable" arrive looking identical. `Settings.svelte` tells them
    // apart on this prefix (mirrored there as `UPDATE_DOWNLOAD_REFUSED`); if the
    // wording drifts, a security refusal starts rendering as a network blip.
    let err = ensure_permitted_download_url("https://evil.example/x.app.tar.gz")
        .expect_err("an off-host download must be refused");
    assert!(
        err.starts_with(DOWNLOAD_REFUSED_PREFIX),
        "the refusal must stay recognisable to the UI, got: {err}"
    );
}

#[test]
fn a_refusal_names_the_expected_host_and_is_an_error_not_a_shrug() {
    ensure_permitted_download_url(
        "https://github.com/hayaitoko/lost-harness/releases/download/v0.1.1/x.app.tar.gz",
    )
    .expect("the real shape must pass");

    let err = ensure_permitted_download_url("https://evil.example/x.app.tar.gz")
        .expect_err("an off-host download must be refused");
    assert!(
        err.contains(RELEASE_HOST),
        "the message should say where updates may come from, got: {err}"
    );
}

#[test]
fn constrained_host_matches_the_configured_manifest_endpoint() {
    // The tripwire. `RELEASE_HOST` / `RELEASE_DOWNLOAD_PREFIX` are hand-written
    // constants; the manifest endpoint lives in tauri.conf.json. If the repo is
    // ever renamed or moved and only one of the two is updated, every download
    // would be refused at runtime — silently, from the user's point of view.
    // Fail here instead.
    let conf: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json"),
        )
        .expect("tauri.conf.json"),
    )
    .expect("tauri.conf.json parses");

    let endpoint = conf["plugins"]["updater"]["endpoints"][0]
        .as_str()
        .expect("plugins.updater.endpoints[0] is configured");
    let endpoint = url::Url::parse(endpoint).expect("the configured endpoint is a URL");

    assert_eq!(
        endpoint.host_str(),
        Some(RELEASE_HOST),
        "the manifest endpoint and the permitted download host must be the same host"
    );

    // `.../<owner>/<repo>/releases/latest/download/latest.json` and
    // `.../<owner>/<repo>/releases/download/<tag>/<asset>` share the
    // `/<owner>/<repo>/releases/` stem.
    let stem = RELEASE_DOWNLOAD_PREFIX
        .strip_suffix("download/")
        .expect("the prefix ends with the release-asset segment");
    assert!(
        endpoint.path().starts_with(stem),
        "the manifest endpoint {} is not under {stem} — RELEASE_DOWNLOAD_PREFIX is stale",
        endpoint.path()
    );
}

// ── The pending-update slot ─────────────────────────────────────────────────

#[test]
fn nothing_is_pending_before_a_check() {
    let pending = PendingUpdate::default();
    assert!(
        pending.peek().is_none(),
        "install must refuse when no check has run"
    );
    // Clearing an empty slot is a no-op, not a panic.
    pending.clear();
    assert!(pending.peek().is_none());
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
