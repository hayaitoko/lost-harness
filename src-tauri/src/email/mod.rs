//! Email (Gmail) — stage 1 of the email round: the OAuth + REST plumbing,
//! NOT yet toolized (no IPC commands, no `Tool` registrations — stage 2 does
//! that wiring; stage 3 adds the Settings UI that pastes the GCP client).
//!
//! ## Trust posture (read before wiring anything)
//!
//! - **Per-PROFILE connection.** A Gmail account is connected to exactly one
//!   profile; nothing here is app-global except the user's own GCP OAuth
//!   client (id + secret), which is one pasted credential pair for the whole
//!   install. The refresh token — the durable credential — is keyed by
//!   profile (see the key builders below), so profiles never share a mailbox.
//! - **Tokens live ONLY in the OS keychain**, via the existing
//!   [`crate::secrets::ProviderSecretStore`] trait (same store, new account
//!   keys — the constants/builders below are the single source of truth for
//!   those key strings). Access tokens are short-lived and held in memory
//!   only; they are never persisted anywhere. Nothing in this module logs or
//!   `Debug`-prints a token (see the redacted `Debug` impls in `oauth.rs`).
//! - **Reads are off-box egress.** `list_messages`/`get_message` send the
//!   user's queries to Google and pull mail content onto the box. When stage 2
//!   toolizes them they are `RiskClass::External` — they must surface their
//!   destination and route through the approval spine like `fetch_url` does.
//!   Mail content is untrusted input (indirect-prompt-injection carrier);
//!   the dispatcher's guard-wrap covers it like any tool result.
//! - **Send is irreversible.** An email cannot be unsent. The send tool is
//!   `RiskClass::Dangerous` at dispatch, and the C2 durability journal covers
//!   the dispatch itself (a crash mid-send must not silently re-send).
//!
//! ## The Google Testing-mode caveat (LOAD-BEARING for the UX)
//!
//! The user pastes their OWN GCP OAuth client. An unverified GCP app in
//! **Testing** publishing status gets refresh tokens that Google expires after
//! ~7 days (and a consent-screen "unverified app" interstitial). That means
//! [`oauth::RefreshError::NeedsReconnect`] is a NORMAL, expected state — the
//! UI must render it as "reconnect your Gmail" (a calm re-auth button), never
//! as an error toast or a broken account. Revocation from the user's Google
//! security page surfaces identically, which is correct: same remedy.
//!
//! ## Layout
//!
//! - [`oauth`] — the installed-app authorization-code flow with PKCE (RFC
//!   8252 loopback redirect + RFC 7636 S256), token exchange and refresh,
//!   with the token-endpoint HTTP behind the [`oauth::TokenEndpoint`] seam.
//! - [`gmail`] — a minimal Gmail REST v1 client behind the [`gmail::GmailApi`]
//!   trait; transport behind [`gmail::GmailHttp`], authorization behind
//!   [`gmail::TokenProvider`] (stage 2 implements it over the keychain +
//!   [`oauth::refresh`]); pure fixture-tested parsers for everything else.
//! - [`api_error`] — the pure classifier for non-2xx Google REST responses.
//!   `oauth` classifies token-REFRESH failures; this classifies API-CALL
//!   failures, which is a different question with different remedies (a
//!   disabled Cloud API is not fixable by reconnecting). Its verdict is a
//!   typed value on the error, never a marker inside the message.
//! - [`connection_state`] — the per-profile recovery state both the screens
//!   and the agent tools record into, decided by DOWNCASTING the failures the
//!   two classifiers produce.

pub mod api_error;
pub mod calendar;
pub mod connection_state;
pub mod gmail;
pub mod google;
pub mod oauth;
pub mod tasks;
pub mod token_provider;

use std::future::Future;
use std::pin::Pin;

/// The object-safe boxed-future shape every async seam in this module uses
/// (same pattern as `models::runner::HealthCheck` / `tools::fetch`).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ---------------------------------------------------------------------------
// ProviderSecretStore account keys — the single source of truth.
//
// Stage 2 stores/reads ONLY through these; never inline the strings. The GCP
// client pair is install-global (one pasted OAuth client), the refresh token
// and connected-address are per-profile.
// ---------------------------------------------------------------------------

/// Keychain account key for the pasted GCP OAuth client id (install-global).
pub const SECRET_GMAIL_CLIENT_ID: &str = "gmail:client_id";

/// Keychain account key for the pasted GCP OAuth client secret
/// (install-global). "Secret" is nominal for an installed app — it is not a
/// proof of client identity (RFC 8252 §8.5) — but we store it like one anyway.
pub const SECRET_GMAIL_CLIENT_SECRET: &str = "gmail:client_secret";

/// Keychain account key for a profile's Gmail refresh token — the durable
/// credential. Deleting this key IS disconnecting the account.
pub fn secret_gmail_refresh_token(profile: &str) -> String {
    format!("gmail:{profile}:refresh_token")
}

/// Keychain account key for the email address a profile is connected as, so
/// the UI can show "connected as x@gmail.com" without a network call. Not a
/// secret, but kept beside the token so disconnect cleanup is one sweep.
pub fn secret_gmail_account_email(profile: &str) -> String {
    format!("gmail:{profile}:account_email")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_keys_are_stable_and_profile_scoped() {
        // These strings are a persistence contract (they name keychain rows on
        // user machines) — changing them orphans stored credentials. Lock them.
        assert_eq!(SECRET_GMAIL_CLIENT_ID, "gmail:client_id");
        assert_eq!(SECRET_GMAIL_CLIENT_SECRET, "gmail:client_secret");
        assert_eq!(
            secret_gmail_refresh_token("work"),
            "gmail:work:refresh_token"
        );
        assert_eq!(
            secret_gmail_account_email("work"),
            "gmail:work:account_email"
        );
        assert_ne!(
            secret_gmail_refresh_token("work"),
            secret_gmail_refresh_token("home"),
            "profiles never share a mailbox credential"
        );
    }
}
