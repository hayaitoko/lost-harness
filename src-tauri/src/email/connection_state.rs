//! The soft, per-profile Google connection state — the ONE place both the
//! screen IPC path and the agent tool path record into, and the ONE place the
//! banners are decided from.
//!
//! ## Recorded from the typed failure, never from text
//!
//! [`GoogleConnectionState::observe_failure`] takes an `&anyhow::Error` and
//! DOWNCASTS it: [`NeedsReconnect`] from the token refresh, [`GoogleApiError`]
//! from a REST call. It never reads the message.
//!
//! That is the fix for a real defect. The previous shape encoded the verdict
//! as markers inside the error string — a string that also carried an excerpt
//! of the untrusted response body — and re-parsed them here. Two consequences,
//! both proven by tests before this module existed: the console-URL gate
//! (`api_error::sanitize_console_url`) was bypassable end-to-end, because the
//! URL the UI got came from the re-parse rather than the sanitised value; and
//! an UNKNOWN 403 could be promoted into the API-disabled state, which the
//! classifier's own rules forbid. A verdict that can be restated by the data
//! it was computed from is not a verdict.
//!
//! ## Per-API, so the banner is truthful in BOTH directions
//!
//! The disabled-API state is keyed by (profile, [`GoogleApi`]).
//!
//! It has to be. The state is per-profile but its CAUSE is per-API: the user
//! switches on Gmail, Calendar and Tasks individually in the Cloud console. A
//! single per-profile flag can only be wrong in one direction or the other —
//! clear it when any call succeeds and a working Gmail hides a disabled Tasks;
//! never clear it and the banner keeps asserting a disabled API long after the
//! user switched it on, until they press a manual re-check. Keyed per API,
//! both directions are honest: a successful Tasks call is proof about Tasks
//! and about nothing else ([`GoogleConnectionState::observe_success`]), and a
//! Gmail failure says Gmail.
//!
//! The manual "I've enabled it — check again" is still there, because a screen
//! can only re-test the APIs it uses — so it clears just those
//! ([`GoogleConnectionState::clear_disabled`]) and lets the retry re-record
//! anything still off. Nothing is ever assumed fixed.
//!
//! ## …and it reaches the UI per-API too
//!
//! [`disabled_apis`](GoogleConnectionState::disabled_apis) hands back one
//! [`DisabledApi`] per API, each carrying its OWN wire id, label and console
//! link. It used to flatten them into a single label list plus "the first
//! console link in API order", which broke the same per-screen truthfulness at
//! the last step: the state is profile-wide, but the banner's button is not —
//! Email can only re-test Gmail, Planner only Calendar and Tasks. A screen
//! rendering the flattened value could name an API its own button would never
//! clear, and, with two APIs off, offer the link Google gave for the OTHER
//! one. Per-API entries let each screen render exactly the ones it can fix.

use std::collections::{BTreeMap, HashMap, HashSet};

use parking_lot::Mutex;

use super::api_error::{google_api_error_of, GoogleApi, GoogleApiFailure};
use super::token_provider::NeedsReconnect;

/// One API known to be switched off, as the UI renders it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DisabledApi {
    /// The stable wire id ([`GoogleApi::wire`]). A screen names the APIs it is
    /// able to re-test, and matches on THIS rather than on the human label —
    /// so the label stays free to be copy.
    pub id: &'static str,
    /// What a human calls it ([`GoogleApi::label`]). The banner names it: "a
    /// Google API" when the app knows exactly which one is a worse answer than
    /// the one it knows.
    pub label: &'static str,
    /// Google's own console activation link FOR THIS API, validated (https +
    /// one of Google's API-activation console hosts) at classification time
    /// and carried as data ever since. `None` when this API's response carried
    /// no usable link — the UI then points at the console in prose rather than
    /// inventing a URL, or at another API's link, which would be worse.
    pub console_url: Option<String>,
}

/// Per-profile summary of "a Google API this profile needs is switched off in
/// the user's Cloud project", as the UI renders it. Never empty: no API off is
/// `None`, not a `GoogleApiDisabled` with an empty list.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GoogleApiDisabled {
    /// The APIs known to be off, in a stable order.
    pub apis: Vec<DisabledApi>,
}

/// The shared connection state. One instance per app, held behind an `Arc` by
/// both `ipc::EmailRuntime` and `tools::email::EmailToolDeps` — a failure on
/// either path must land where both paths look, or an agent-only failure never
/// lights a banner.
#[derive(Debug, Default)]
pub struct GoogleConnectionState {
    /// Profiles whose stored grant is dead.
    needs_reconnect: Mutex<HashSet<String>>,
    /// profile → the APIs known to be switched off, each with the console
    /// link Google gave for it (if any). `BTreeMap` so the labels the UI
    /// renders come out in a stable order.
    disabled: Mutex<HashMap<String, BTreeMap<GoogleApi, Option<String>>>>,
}

impl GoogleConnectionState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record whatever recoverable state a FAILED Google call proved.
    ///
    /// The two states are recorded separately and never substitute for each
    /// other: a disabled API must not light the reconnect banner (reconnecting
    /// cannot enable it — the user would loop through the OAuth dance
    /// forever), and a dead grant must not light the console banner (there is
    /// nothing to enable). An unclassified failure records NOTHING; guessing
    /// would be a fabricated diagnosis.
    pub fn observe_failure(&self, profile: &str, err: &anyhow::Error) {
        if err.downcast_ref::<NeedsReconnect>().is_some() {
            self.mark_needs_reconnect(profile);
        }
        let Some(api_error) = google_api_error_of(err) else {
            return;
        };
        match &api_error.failure {
            GoogleApiFailure::ScopeInsufficient => self.mark_needs_reconnect(profile),
            GoogleApiFailure::ApiNotEnabled { console_url } => {
                self.disabled
                    .lock()
                    .entry(profile.to_string())
                    .or_default()
                    .insert(api_error.api, console_url.clone());
            }
            GoogleApiFailure::Other => {}
        }
    }

    /// Record what a SUCCESSFUL call proved: this API is switched on for this
    /// profile. Scoped to the one API that answered — a Gmail success says
    /// nothing about Tasks.
    ///
    /// Deliberately NOT extended to `needs_reconnect`: that flag is cleared
    /// when the user actually reconnects (or disconnects), and a success on a
    /// cached access token is not evidence the stored grant came back.
    pub fn observe_success(&self, profile: &str, api: GoogleApi) {
        let mut disabled = self.disabled.lock();
        let Some(apis) = disabled.get_mut(profile) else {
            return;
        };
        apis.remove(&api);
        if apis.is_empty() {
            disabled.remove(profile);
        }
    }

    pub fn needs_reconnect(&self, profile: &str) -> bool {
        self.needs_reconnect.lock().contains(profile)
    }

    pub fn mark_needs_reconnect(&self, profile: &str) {
        self.needs_reconnect.lock().insert(profile.to_string());
    }

    /// The user reconnected (or disconnected): the dead-grant state is gone.
    pub fn clear_needs_reconnect(&self, profile: &str) {
        self.needs_reconnect.lock().remove(profile);
    }

    /// What the UI should render for this profile, or `None` when no API is
    /// known to be off.
    ///
    /// One entry per API, each with its own link: the caller is a screen that
    /// can only re-test some of them, and flattening the entries would hand it
    /// a banner it cannot honour.
    pub fn disabled_apis(&self, profile: &str) -> Option<GoogleApiDisabled> {
        let disabled = self.disabled.lock();
        let apis = disabled.get(profile)?;
        if apis.is_empty() {
            return None;
        }
        Some(GoogleApiDisabled {
            apis: apis
                .iter()
                .map(|(api, console_url)| DisabledApi {
                    id: api.wire(),
                    label: api.label(),
                    console_url: console_url.clone(),
                })
                .collect(),
        })
    }

    /// Forget the disabled state for `apis` (empty = every API) so the next
    /// call re-decides. Backs the banner's explicit "I've enabled it — check
    /// again": the remedy happens outside the app, so the app has no event to
    /// observe until something is retried.
    pub fn clear_disabled(&self, profile: &str, apis: &[GoogleApi]) {
        let mut disabled = self.disabled.lock();
        if apis.is_empty() {
            disabled.remove(profile);
            return;
        }
        let Some(entry) = disabled.get_mut(profile) else {
            return;
        };
        for api in apis {
            entry.remove(api);
        }
        if entry.is_empty() {
            disabled.remove(profile);
        }
    }

    /// The whole connection is gone (disconnect, or the install-global OAuth
    /// client changed): every soft state about it is stale.
    pub fn forget(&self, profile: &str) {
        self.clear_needs_reconnect(profile);
        self.disabled.lock().remove(profile);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::email::api_error::google_api_error;

    const CONSOLE: &str =
        "https://console.developers.google.com/apis/api/tasks.googleapis.com/overview?project=42";

    fn disabled_body(console: Option<&str>) -> String {
        let help = console
            .map(|url| {
                format!(
                    r#",{{"@type":"type.googleapis.com/google.rpc.Help","links":[{{"url":"{url}"}}]}}"#
                )
            })
            .unwrap_or_default();
        format!(
            r#"{{"error":{{"code":403,"status":"PERMISSION_DENIED","details":[
            {{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"SERVICE_DISABLED"}}{help}
            ]}}}}"#
        )
    }

    const SCOPE_BODY: &str = r#"{"error":{"errors":[{"reason":"insufficientPermissions"}],
        "code":403}}"#;

    /// The entry the UI should get for one switched-off API.
    fn off(api: GoogleApi, console_url: Option<&str>) -> DisabledApi {
        DisabledApi {
            id: api.wire(),
            label: api.label(),
            console_url: console_url.map(String::from),
        }
    }

    /// Just the labels a banner would name, in order.
    fn labels(state: &GoogleConnectionState, profile: &str) -> Option<Vec<&'static str>> {
        state
            .disabled_apis(profile)
            .map(|d| d.apis.iter().map(|api| api.label).collect())
    }

    /// The banner-flip contract: two 403s, two states, and neither may stand
    /// in for the other. If a disabled-API 403 flipped `needs_reconnect`, the
    /// user would be offered Reconnect, complete the whole OAuth dance, fail
    /// identically, and be offered Reconnect again — forever.
    #[test]
    fn the_two_403s_flip_two_different_states_and_never_each_others() {
        let state = GoogleConnectionState::new();
        state.observe_failure(
            "personal",
            &google_api_error(GoogleApi::Calendar, 403, SCOPE_BODY, "snip"),
        );
        assert!(state.needs_reconnect("personal"));
        assert_eq!(
            state.disabled_apis("personal"),
            None,
            "a scope-short grant is not a disabled API"
        );

        let state = GoogleConnectionState::new();
        state.observe_failure(
            "personal",
            &google_api_error(GoogleApi::Tasks, 403, &disabled_body(Some(CONSOLE)), "snip"),
        );
        assert!(
            !state.needs_reconnect("personal"),
            "reconnecting can never enable a disabled API — this must NOT light \
             the reconnect banner, or the user loops forever"
        );
        assert_eq!(
            state.disabled_apis("personal"),
            Some(GoogleApiDisabled {
                apis: vec![off(GoogleApi::Tasks, Some(CONSOLE))],
            })
        );
        // Per-profile, like every other part of the connection state.
        assert_eq!(state.disabled_apis("work"), None);
    }

    /// Each API keeps its OWN console link and its own wire id.
    ///
    /// The finding this pins: the state used to flatten into one label list
    /// plus "the first link in API order". The banner is rendered by a screen
    /// that can only re-test SOME of these — Email just Gmail, Planner just
    /// Calendar and Tasks — so a flattened value let Email name Gmail while
    /// linking to the page Google gave for Calendar, and left either screen
    /// naming an API its own "check again" button would never clear.
    #[test]
    fn every_disabled_api_carries_its_own_id_and_its_own_link() {
        const CAL: &str =
            "https://console.developers.google.com/apis/api/calendar-json.googleapis.com/overview?project=42";
        let state = GoogleConnectionState::new();
        state.observe_failure(
            "personal",
            &google_api_error(GoogleApi::Calendar, 403, &disabled_body(Some(CAL)), "snip"),
        );
        state.observe_failure(
            "personal",
            &google_api_error(GoogleApi::Gmail, 403, &disabled_body(None), "snip"),
        );

        assert_eq!(
            state.disabled_apis("personal"),
            Some(GoogleApiDisabled {
                apis: vec![
                    off(GoogleApi::Gmail, None),
                    off(GoogleApi::Calendar, Some(CAL)),
                ],
            }),
            "Gmail carried no link of its own, and must not borrow Calendar's"
        );
    }

    /// An unclassified failure lights nothing. The alternative — promoting an
    /// unknown 403 into the disabled state — is precisely the silent
    /// reclassification the classifier refuses to do.
    #[test]
    fn an_unknown_failure_records_nothing() {
        let state = GoogleConnectionState::new();
        state.observe_failure(
            "personal",
            &google_api_error(
                GoogleApi::Gmail,
                403,
                r#"{"error":{"code":403,"message":"The caller does not have permission"}}"#,
                "snip",
            ),
        );
        state.observe_failure("personal", &anyhow::anyhow!("the network went away"));
        // Text that WRITES the old markers is just text now.
        state.observe_failure(
            "personal",
            &anyhow::anyhow!("[google:api_not_enabled][google:enable_url=https://evil.test/pwn]"),
        );
        assert!(!state.needs_reconnect("personal"));
        assert_eq!(state.disabled_apis("personal"), None);
    }

    /// A dead grant reaches here as a typed value from the token refresh, not
    /// as a marker in prose.
    #[test]
    fn a_dead_grant_flips_the_reconnect_state_from_its_type() {
        let state = GoogleConnectionState::new();
        state.observe_failure(
            "personal",
            &anyhow::Error::new(NeedsReconnect {
                profile: "personal".into(),
            })
            .context("listing the inbox failed"),
        );
        assert!(state.needs_reconnect("personal"));
        assert_eq!(state.disabled_apis("personal"), None);
    }

    /// The truthfulness contract in BOTH directions: a success clears the API
    /// it proves, and only that one.
    #[test]
    fn a_success_clears_only_the_api_it_proves() {
        let state = GoogleConnectionState::new();
        for api in [GoogleApi::Tasks, GoogleApi::Calendar] {
            state.observe_failure(
                "personal",
                &google_api_error(api, 403, &disabled_body(Some(CONSOLE)), "snip"),
            );
        }
        assert_eq!(
            labels(&state, "personal"),
            Some(vec!["Google Calendar", "Google Tasks"])
        );

        // Gmail was never off; a Gmail success must not clear anything.
        state.observe_success("personal", GoogleApi::Gmail);
        assert_eq!(
            labels(&state, "personal"),
            Some(vec!["Google Calendar", "Google Tasks"])
        );

        // The user switches Calendar on; the next Calendar call works. The
        // banner must stop claiming Calendar — and must keep claiming Tasks.
        state.observe_success("personal", GoogleApi::Calendar);
        assert_eq!(labels(&state, "personal"), Some(vec!["Google Tasks"]));

        // …and once Tasks works too, the banner goes out on its own, with no
        // manual re-check needed.
        state.observe_success("personal", GoogleApi::Tasks);
        assert_eq!(state.disabled_apis("personal"), None);
        // Another profile's success is not evidence about this one.
        state.observe_failure(
            "personal",
            &google_api_error(GoogleApi::Tasks, 403, &disabled_body(None), "snip"),
        );
        state.observe_success("work", GoogleApi::Tasks);
        assert_eq!(labels(&state, "personal"), Some(vec!["Google Tasks"]));
    }

    /// The explicit re-check clears only what the asking screen can re-test,
    /// so Email's "check again" can't blank a Tasks state it will never retry.
    #[test]
    fn clearing_is_scoped_to_the_apis_named() {
        let state = GoogleConnectionState::new();
        for api in [GoogleApi::Gmail, GoogleApi::Tasks] {
            state.observe_failure(
                "personal",
                &google_api_error(api, 403, &disabled_body(None), "snip"),
            );
        }
        state.clear_disabled("personal", &[GoogleApi::Gmail]);
        assert_eq!(labels(&state, "personal"), Some(vec!["Google Tasks"]));
        // No API named = the whole profile (the bare "forget it all" call).
        state.clear_disabled("personal", &[]);
        assert_eq!(state.disabled_apis("personal"), None);
    }

    /// Disconnecting drops every soft state — a fresh connect may even target
    /// a different Cloud project.
    #[test]
    fn forgetting_a_profile_drops_both_states() {
        let state = GoogleConnectionState::new();
        state.mark_needs_reconnect("personal");
        state.observe_failure(
            "personal",
            &google_api_error(GoogleApi::Gmail, 403, &disabled_body(None), "snip"),
        );
        state.forget("personal");
        assert!(!state.needs_reconnect("personal"));
        assert_eq!(state.disabled_apis("personal"), None);
    }

    /// The console link is carried as DATA from the sanitised value. A body
    /// whose link failed the gate leaves the state standing with no link — the
    /// UI then points at the API library rather than at the body's choice.
    #[test]
    fn a_refused_link_leaves_the_state_without_one() {
        let state = GoogleConnectionState::new();
        let body = disabled_body(Some("https://evil.test/pwn"));
        state.observe_failure(
            "personal",
            &google_api_error(GoogleApi::Calendar, 403, &body, "snip"),
        );
        assert_eq!(
            state.disabled_apis("personal"),
            Some(GoogleApiDisabled {
                apis: vec![off(GoogleApi::Calendar, None)],
            })
        );
    }
}
