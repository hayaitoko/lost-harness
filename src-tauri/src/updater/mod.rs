//! App self-update — round-2 spec item 3.
//!
//! ## What this is
//!
//! On launch the app asks `github.com/hayaitoko/lost-harness`'s **public**
//! releases for a `latest.json` manifest. If that manifest names a version
//! newer than the running one, a calm banner appears. Nothing is downloaded
//! and nothing is installed until the user clicks. There is no silent install
//! path in this module, by construction: `check` and `install` are separate
//! commands and only the user's click calls the second one.
//!
//! ## Why the whole thing is driven from Rust
//!
//! `tauri-plugin-updater` ships a JS API, and the obvious wiring would be to
//! grant the webview `updater:default` and call `check()` from Svelte. This
//! module deliberately does not do that, for three reasons:
//!
//! 1. **The launch check has to be Rust-side anyway.** It must not delay the
//!    window, and it must run whether or not a webview has finished booting.
//!    The toggle that governs it is therefore read from SQLite by Rust — the
//!    frontend's localStorage settings store is unreadable from here.
//! 2. **One egress path, not two.** The spec's acceptance criterion is that
//!    a disabled toggle produces *zero* update-related egress. If the webview
//!    could also call `plugin:updater|check` directly, the guarantee would
//!    depend on auditing every line of frontend code as well as this file.
//!    With the ACL grant withheld, the *only* way to reach the network for an
//!    update is [`check_now`], which is fourteen lines long and logged.
//! 3. **It matches the repo.** Every other feature here is a hand-rolled
//!    `#[tauri::command]`; the frontend has exactly one IPC seam
//!    (`src/lib/api/tauri.ts`). A plugin ACL grant would have been the first
//!    exception.
//!
//! ## The seam that makes "zero egress" testable
//!
//! [`run_launch_check`] takes the network call as a closure. The closure is
//! the only thing in the launch path that can touch a socket, so a test can
//! pass a counting fake and assert the counter is still zero when the toggle
//! is off or the build is a dev build. That is the proof, and it lives in
//! `tests.rs` next to this file.

use std::sync::Arc;

use serde::Serialize;
use tauri::{Emitter, Manager, Runtime};
use tauri_plugin_updater::UpdaterExt;

#[cfg(test)]
mod signature_tests;
#[cfg(test)]
mod tests;

/// Event emitted to the webview when a launch-time check found a newer
/// version. Payload is [`UpdateInfo`]. Mirrored in `src/lib/api/tauri.ts`.
pub const UPDATE_AVAILABLE_EVENT: &str = "update:available";

// ── The launch gate ─────────────────────────────────────────────────────────

/// Why a launch-time update check did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// `tauri::is_dev()` — a `cargo run` / `tauri dev` build. A dev build's
    /// version is whatever is in Cargo.toml and its bundle is not a signed
    /// `.app`, so "updating" it would replace a working checkout build with a
    /// release one. The spec says dev builds skip; this is that.
    DevBuild,
    /// The user turned the launch check off in Settings → About.
    ToggleOff,
}

/// The decision the launch path makes *before* anything can reach the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchDecision {
    /// Both gates passed — a version request to GitHub is permitted.
    Check,
    /// No update-related network request may be made.
    Skip(SkipReason),
}

/// Pure gate. Dev-build skip is checked first so that a developer who has the
/// toggle on still never sees a check, and so the reported reason is the
/// structural one rather than a user setting.
pub fn launch_check_decision(is_dev_build: bool, toggle_enabled: bool) -> LaunchDecision {
    if is_dev_build {
        LaunchDecision::Skip(SkipReason::DevBuild)
    } else if !toggle_enabled {
        LaunchDecision::Skip(SkipReason::ToggleOff)
    } else {
        LaunchDecision::Check
    }
}

/// What a launch-time check ended up doing.
#[derive(Debug, Clone, PartialEq)]
pub enum LaunchOutcome {
    /// The gate refused; `fetch` was never called.
    Skipped(SkipReason),
    /// A request was made and the running version is current.
    UpToDate,
    /// A request was made and a newer version was announced.
    Available(UpdateInfo),
    /// A request was made and failed (offline, DNS, 404, bad manifest…).
    /// Never surfaced as an error dialog: a failed update check is not a
    /// problem the user has to act on.
    Failed(String),
}

/// Run the launch-time update check behind the gate.
///
/// `fetch` is the *only* argument that can perform I/O. It is invoked exactly
/// once when the gate passes and **never** when it does not — that is the
/// zero-egress guarantee, and it is a structural property of this function
/// rather than a promise about a call site.
pub async fn run_launch_check<F, Fut>(
    is_dev_build: bool,
    toggle_enabled: bool,
    fetch: F,
) -> LaunchOutcome
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Option<UpdateInfo>, String>>,
{
    match launch_check_decision(is_dev_build, toggle_enabled) {
        LaunchDecision::Skip(reason) => {
            tracing::debug!(
                ?reason,
                "skipping the launch update check (no request made)"
            );
            LaunchOutcome::Skipped(reason)
        }
        LaunchDecision::Check => match fetch().await {
            Ok(Some(info)) => LaunchOutcome::Available(info),
            Ok(None) => LaunchOutcome::UpToDate,
            Err(e) => {
                tracing::info!(error = %e, "update check failed; staying quiet");
                LaunchOutcome::Failed(e)
            }
        },
    }
}

// ── Version comparison ──────────────────────────────────────────────────────

/// Is `candidate` a strictly newer semver than `current`?
///
/// `tauri-plugin-updater` already applies its own comparison before it hands
/// back an `Update`, so this is the app's **second** gate rather than the only
/// one. It exists because the manifest is remote input: a `latest.json` that
/// announces the running version — or an older one — must never produce an
/// "Update available" banner, whatever the plugin decides. An unparseable
/// version on either side is treated as "not newer" (refuse rather than guess).
pub fn is_strictly_newer(current: &str, candidate: &str) -> bool {
    let (Ok(cur), Ok(cand)) = (
        semver::Version::parse(current.trim_start_matches('v')),
        semver::Version::parse(candidate.trim_start_matches('v')),
    ) else {
        return false;
    };
    cand > cur
}

// ── Where an update may be downloaded from ──────────────────────────────────

/// The only host an update payload may be fetched from.
///
/// The manifest endpoint is pinned in `tauri.conf.json`, but the *download* URL
/// is not: `latest.json` carries `platforms.<target>.url` and the plugin
/// fetches whatever that says. So a manifest that was swapped, mirrored or
/// tampered with could point the download at any host on the internet. The
/// minisign check still protects INTEGRITY there — nothing unsigned installs
/// either way — but it says nothing about *where the app connected*, and "one
/// request to github.com" is a claim this app makes to the user in Settings.
///
/// This constant is what turns that claim into an enforced property.
pub const RELEASE_HOST: &str = "github.com";

/// The only path prefix under [`RELEASE_HOST`] a payload may come from —
/// i.e. this project's own release assets, not some other repository's.
///
/// Kept in step with the manifest endpoint in `tauri.conf.json` by
/// `constrained_host_matches_the_configured_manifest_endpoint` in `tests.rs`:
/// repoint the endpoint at a different repo without editing this, and that test
/// fails rather than the constraint silently blocking every real download.
pub const RELEASE_DOWNLOAD_PREFIX: &str = "/hayaitoko/lost-harness/releases/download/";

/// Is `raw` a URL this app is willing to download an update payload from?
///
/// Deliberately strict — scheme, credentials, host, port and path prefix all
/// have to be right:
///
/// * `https` only. A plaintext download of a signed payload would still verify,
///   but it is not what the app tells the user it does.
/// * No userinfo. `https://github.com@evil.example/...` has host `evil.example`
///   and would fail the host check anyway; rejecting it explicitly means the
///   refusal reads as intentional rather than incidental.
/// * The host is compared for **exact** equality, so `raw.github.com` and
///   `github.com.evil.example` are both refused.
/// * Default port only — `github.com:8443` is a different service.
/// * The path must be under this project's release assets, and must not try to
///   climb out of them (`url` normalises real `..` segments away at parse time;
///   the percent-encoded spelling is rejected here).
pub fn is_permitted_download_url(raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    if url.scheme() != "https" {
        return false;
    }
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    if url.host_str() != Some(RELEASE_HOST) {
        return false;
    }
    if url.port_or_known_default() != Some(443) {
        return false;
    }
    let path = url.path();
    if !path.starts_with(RELEASE_DOWNLOAD_PREFIX) {
        return false;
    }
    // `Url::parse` already collapsed literal `..` segments; this catches the
    // encoded spelling, which it leaves alone.
    if path.contains("%2e") || path.contains("%2E") {
        return false;
    }
    true
}

/// [`is_permitted_download_url`] as a refusal with a message.
///
/// Returned to the caller rather than logged-and-ignored: a manual check should
/// tell the user their manifest is pointing somewhere it should not, and an
/// install must stop rather than fetch.
pub fn ensure_permitted_download_url(raw: &str) -> Result<(), String> {
    if is_permitted_download_url(raw) {
        return Ok(());
    }
    tracing::error!(
        url = %raw,
        expected_host = RELEASE_HOST,
        "refusing an update whose download URL is not this project's GitHub release asset"
    );
    Err(format!(
        "This update's download link points somewhere unexpected, so it was refused. \
         Updates may only be downloaded from https://{RELEASE_HOST}{RELEASE_DOWNLOAD_PREFIX}…"
    ))
}

// ── Payloads ────────────────────────────────────────────────────────────────

/// What the banner and the About pane show. Deliberately small: the version
/// strings, the release notes if the manifest carried any, and the date.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UpdateInfo {
    /// The announced (newer) version, e.g. `"0.1.1"`.
    pub version: String,
    /// The version currently running.
    pub current_version: String,
    /// Release notes from the manifest's `notes` field, if present.
    pub notes: Option<String>,
    /// Publish date from the manifest's `pub_date`, if present.
    pub date: Option<String>,
}

/// Result of a manual "Check for updates" click.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum ManualCheckResult {
    /// A newer version is available.
    Available(UpdateInfo),
    /// The running version is current.
    UpToDate { current_version: String },
}

// ── The pending update ──────────────────────────────────────────────────────

/// Holds the `Update` handle produced by the most recent successful check, so
/// that clicking "Install" does not have to hit the network a second time to
/// re-discover it.
///
/// Managed separately from `AppState` because it is the only piece of app
/// state whose lifetime is "between a check and the install the user clicked".
#[derive(Default)]
pub struct PendingUpdate {
    inner: parking_lot::Mutex<Option<Arc<tauri_plugin_updater::Update>>>,
    /// Serializes installs. Two concurrent `download_and_install` calls would
    /// both unpack over the running `.app` bundle; the UI already disables its
    /// button while one is in flight, but the bundle is not something to
    /// protect with a UI convention alone.
    install_lock: tokio::sync::Mutex<()>,
}

impl PendingUpdate {
    pub fn store(&self, update: tauri_plugin_updater::Update) {
        *self.inner.lock() = Some(Arc::new(update));
    }

    /// Clone the staged handle **without** clearing the slot.
    ///
    /// Deliberately not a `take`: a download that fails verification must leave
    /// the offer intact, or the banner's "Try again" would come back with "no
    /// update is staged" instead of retrying. The slot is cleared by
    /// [`PendingUpdate::clear`] once an install has actually succeeded.
    pub fn peek(&self) -> Option<Arc<tauri_plugin_updater::Update>> {
        self.inner.lock().clone()
    }

    /// Drop the staged update — called after a successful install, so the same
    /// payload can't be installed twice.
    pub fn clear(&self) {
        *self.inner.lock() = None;
    }

    /// Held for the duration of an install.
    pub async fn install_guard(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.install_lock.lock().await
    }
}

// ── The one place update egress happens ─────────────────────────────────────

/// Ask the configured endpoint whether a newer version exists.
///
/// **This function is the app's entire update-related network surface.** It is
/// reached from exactly two places: the gated launch check, and the user
/// clicking "Check for updates". Both log here, at the call site, the same way
/// every other background egress in this codebase does (see
/// `models/hf_search.rs`) — there is no reader UI for the audit tables and
/// `tool_audit` is conversation-scoped, so a fabricated audit row would be a
/// worse record than an honest log line.
///
/// Sends: a plain GET for `latest.json`, anonymous, no headers of ours, no app
/// data. Receives: a version string, a download URL and a signature.
///
/// The manifest endpoint is pinned in `tauri.conf.json`; the download URL it
/// names is remote input, so it is checked here against
/// [`is_permitted_download_url`] before an update is ever offered. Between the
/// two, every update-related connection this app opens is to
/// `https://github.com/…` — which is what Settings → About tells the user.
/// (GitHub itself answers an asset download with a redirect to its own object
/// CDN; that hop is GitHub's, not a host this manifest chose.)
pub async fn check_now<R: Runtime>(
    app: &tauri::AppHandle<R>,
) -> Result<Option<UpdateInfo>, String> {
    let current_version = app.package_info().version.to_string();

    tracing::info!(
        endpoint = "github.com/hayaitoko/lost-harness/releases",
        current_version = %current_version,
        "egress: requesting the update manifest (version only; no app data is sent)"
    );

    let updater = app.updater().map_err(|e| e.to_string())?;
    let found = updater.check().await.map_err(|e| e.to_string())?;

    let Some(update) = found else {
        return Ok(None);
    };

    // Second gate — see `is_strictly_newer`. A manifest that announces the
    // running version (or older) is dropped here rather than shown.
    if !is_strictly_newer(&current_version, &update.version) {
        tracing::warn!(
            announced = %update.version,
            current = %current_version,
            "update manifest announced a version that is not newer; ignoring"
        );
        return Ok(None);
    }

    // Third gate — where the bytes would come from. `is_strictly_newer` said
    // the manifest announced something worth offering; this says the payload
    // may only be fetched from this project's own GitHub release assets. A
    // refusal here is an error rather than "up to date": the user asked a
    // question and the honest answer is that the manifest is pointing
    // somewhere it should not, not that they are current.
    ensure_permitted_download_url(update.download_url.as_str())?;

    let info = UpdateInfo {
        version: update.version.clone(),
        current_version,
        notes: update.body.clone(),
        date: update.date.map(|d| d.to_string()),
    };

    // `try_state` rather than `state`: the latter panics when the type was
    // never managed, and a wiring mistake in `lib.rs::run` should surface as a
    // refused check, not as a panic inside a spawned launch task.
    app.try_state::<PendingUpdate>()
        .ok_or_else(|| "update state is not initialised".to_string())?
        .store(update);
    Ok(Some(info))
}

/// Spawn the launch-time check. Returns immediately — the window is never
/// waiting on this. Emits [`UPDATE_AVAILABLE_EVENT`] if (and only if) a newer
/// version was announced.
pub fn spawn_launch_check<R: Runtime>(app: tauri::AppHandle<R>, toggle_enabled: bool) {
    tauri::async_runtime::spawn(async move {
        let handle = app.clone();
        let outcome =
            run_launch_check(tauri::is_dev(), toggle_enabled, || check_now(&handle)).await;

        match outcome {
            LaunchOutcome::Available(info) => {
                tracing::info!(version = %info.version, "update available");
                if let Err(e) = app.emit(UPDATE_AVAILABLE_EVENT, info) {
                    tracing::warn!(error = %e, "couldn't tell the UI about the update");
                }
            }
            LaunchOutcome::UpToDate => tracing::info!("already on the latest version"),
            LaunchOutcome::Skipped(_) | LaunchOutcome::Failed(_) => {}
        }
    });
}
