//! The stage-2 [`TokenProvider`] impl: keychain-backed, per-profile, with an
//! in-memory access-token cache.
//!
//! Reads the install-global GCP client and the profile's refresh token from
//! the OS keychain AT CALL TIME (so freshly pasted credentials or a
//! reconnect take effect without restart), refreshes through
//! [`oauth::refresh`], and keeps the short-lived access token only in memory
//! (never persisted — see the module trust posture in `email/mod.rs`).
//!
//! Failure honesty: [`RefreshError::NeedsReconnect`] must reach callers
//! UNMANGLED — the IPC layer maps it to the calm "reconnect your Gmail"
//! state, and the agent tools convert it into a clear "ask the user to
//! reconnect in Settings" tool error. Both match on
//! [`NEEDS_RECONNECT_MARKER`] rather than parsing prose.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::gmail::TokenProvider;
use super::oauth::{self, GcpClient, RefreshError, TokenEndpoint};
use super::BoxFuture;
use crate::secrets::ProviderSecretStore;

/// Stable substring carried by every NeedsReconnect-derived error message.
/// The IPC layer and tools match on this instead of prose so the mapping
/// survives copy edits.
pub const NEEDS_RECONNECT_MARKER: &str = "[gmail:needs_reconnect]";

/// Refresh this many seconds before the token actually expires, so a token
/// handed to a request can't die mid-flight.
const EXPIRY_MARGIN_SECS: u64 = 60;

/// Keychain-backed [`TokenProvider`] for one profile.
pub struct KeychainTokenProvider {
    profile: String,
    secrets: Arc<dyn ProviderSecretStore>,
    endpoint: Arc<dyn TokenEndpoint>,
    /// (access_token, hard expiry). Memory-only by design.
    cache: Mutex<Option<(String, Instant)>>,
}

impl KeychainTokenProvider {
    pub fn new(
        profile: &str,
        secrets: Arc<dyn ProviderSecretStore>,
        endpoint: Arc<dyn TokenEndpoint>,
    ) -> Self {
        Self {
            profile: profile.to_string(),
            secrets,
            endpoint,
            cache: Mutex::new(None),
        }
    }

    /// The pasted GCP client, or a setup-pointing error.
    fn load_client(&self) -> anyhow::Result<GcpClient> {
        let id = self
            .secrets
            .get(super::SECRET_GMAIL_CLIENT_ID)
            .map_err(anyhow::Error::msg)?;
        let secret = self
            .secrets
            .get(super::SECRET_GMAIL_CLIENT_SECRET)
            .map_err(anyhow::Error::msg)?;
        match (id, secret) {
            (Some(client_id), Some(client_secret)) => Ok(GcpClient { client_id, client_secret }),
            _ => anyhow::bail!(
                "no Google OAuth client is configured — finish the Gmail setup in Settings → Email"
            ),
        }
    }

    /// The profile's refresh token, or a connect-pointing error.
    fn load_refresh_token(&self) -> anyhow::Result<String> {
        self.secrets
            .get(&super::secret_gmail_refresh_token(&self.profile))
            .map_err(anyhow::Error::msg)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "the {} profile isn't connected to Gmail — connect it in Settings → Email",
                    self.profile
                )
            })
    }

    async fn refresh_now(&self) -> anyhow::Result<String> {
        let gcp = self.load_client()?;
        let stored = self.load_refresh_token()?;
        match oauth::refresh(self.endpoint.as_ref(), &gcp, &stored).await {
            Ok(tokens) => {
                // Google rarely rotates the refresh token; keep the stored one
                // when the response omits it (S1 contract note #4).
                if let Some(new_rt) = &tokens.refresh_token {
                    let _ = self
                        .secrets
                        .set(&super::secret_gmail_refresh_token(&self.profile), new_rt);
                }
                let expiry = Instant::now()
                    + Duration::from_secs(
                        tokens.expires_in_secs.saturating_sub(EXPIRY_MARGIN_SECS).max(30),
                    );
                *self.cache.lock() = Some((tokens.access_token.clone(), expiry));
                Ok(tokens.access_token)
            }
            Err(RefreshError::NeedsReconnect { .. }) => anyhow::bail!(
                "{NEEDS_RECONNECT_MARKER} Gmail needs to be reconnected for the {} profile \
                 (the stored authorization expired or was revoked — this is routine for a \
                 Testing-status Google client). Reconnect in Settings → Email.",
                self.profile
            ),
            Err(RefreshError::Misconfigured { detail }) => anyhow::bail!(
                "the Google OAuth client rejected the request ({detail}) — re-check the pasted \
                 client ID / secret in Settings → Email"
            ),
            Err(RefreshError::Transient { detail }) => {
                anyhow::bail!("couldn't reach Google to refresh the Gmail session: {detail}")
            }
        }
    }
}

impl TokenProvider for KeychainTokenProvider {
    fn access_token(&self, force_refresh: bool) -> BoxFuture<'_, anyhow::Result<String>> {
        Box::pin(async move {
            if !force_refresh {
                if let Some((token, expiry)) = self.cache.lock().clone() {
                    if Instant::now() < expiry {
                        return Ok(token);
                    }
                }
            }
            self.refresh_now().await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::MemoryProviderSecretStore;

    /// A scripted token endpoint: each call pops the next canned response.
    struct ScriptedEndpoint {
        responses: Mutex<Vec<Result<oauth::TokenSet, RefreshError>>>,
        calls: Mutex<u32>,
    }

    impl TokenEndpoint for ScriptedEndpoint {
        fn post_form<'a>(
            &'a self,
            _form: Vec<(String, String)>,
        ) -> BoxFuture<'a, anyhow::Result<(u16, String)>> {
            // The provider goes through oauth::refresh, which needs the raw
            // HTTP seam. Simpler for these tests: script at the HTTP level.
            let next = self.responses.lock().remove(0);
            *self.calls.lock() += 1;
            Box::pin(async move {
                match next {
                    Ok(ts) => Ok((
                        200,
                        format!(
                            r#"{{"access_token":"{}","expires_in":{}{}}}"#,
                            ts.access_token,
                            ts.expires_in_secs,
                            ts.refresh_token
                                .map(|r| format!(r#","refresh_token":"{r}""#))
                                .unwrap_or_default()
                        ),
                    )),
                    Err(RefreshError::NeedsReconnect { .. }) => {
                        Ok((400, r#"{"error":"invalid_grant"}"#.to_string()))
                    }
                    Err(RefreshError::Misconfigured { .. }) => {
                        Ok((401, r#"{"error":"invalid_client"}"#.to_string()))
                    }
                    Err(RefreshError::Transient { .. }) => Ok((503, "upstream sad".to_string())),
                }
            })
        }
    }

    fn seeded_store() -> Arc<MemoryProviderSecretStore> {
        let s = Arc::new(MemoryProviderSecretStore::default());
        s.set(crate::email::SECRET_GMAIL_CLIENT_ID, "x.apps.googleusercontent.com")
            .unwrap();
        s.set(crate::email::SECRET_GMAIL_CLIENT_SECRET, "shhh").unwrap();
        s.set(&crate::email::secret_gmail_refresh_token("personal"), "rt-1")
            .unwrap();
        s
    }

    fn ok_tokens(access: &str, rotate_to: Option<&str>) -> Result<oauth::TokenSet, RefreshError> {
        Ok(oauth::TokenSet {
            access_token: access.into(),
            refresh_token: rotate_to.map(String::from),
            expires_in_secs: 3599,
        })
    }

    #[tokio::test]
    async fn caches_until_forced_and_persists_a_rotated_refresh_token() {
        let store = seeded_store();
        let endpoint = Arc::new(ScriptedEndpoint {
            responses: Mutex::new(vec![ok_tokens("at-1", Some("rt-2")), ok_tokens("at-2", None)]),
            calls: Mutex::new(0),
        });
        let p = KeychainTokenProvider::new("personal", store.clone(), endpoint.clone());

        // First call refreshes; second serves the cache (no endpoint call).
        assert_eq!(p.access_token(false).await.unwrap(), "at-1");
        assert_eq!(p.access_token(false).await.unwrap(), "at-1");
        assert_eq!(*endpoint.calls.lock(), 1);
        // The rotated refresh token was persisted.
        assert_eq!(
            store
                .get(&crate::email::secret_gmail_refresh_token("personal"))
                .unwrap()
                .as_deref(),
            Some("rt-2")
        );
        // force_refresh bypasses the cache; the omitted refresh_token keeps rt-2.
        assert_eq!(p.access_token(true).await.unwrap(), "at-2");
        assert_eq!(
            store
                .get(&crate::email::secret_gmail_refresh_token("personal"))
                .unwrap()
                .as_deref(),
            Some("rt-2")
        );
    }

    #[tokio::test]
    async fn dead_grant_carries_the_reconnect_marker() {
        let store = seeded_store();
        let endpoint = Arc::new(ScriptedEndpoint {
            responses: Mutex::new(vec![Err(RefreshError::NeedsReconnect { detail: "invalid_grant".into() })]),
            calls: Mutex::new(0),
        });
        let p = KeychainTokenProvider::new("personal", store, endpoint);
        let err = p.access_token(false).await.unwrap_err().to_string();
        assert!(err.contains(NEEDS_RECONNECT_MARKER), "got: {err}");
    }

    #[tokio::test]
    async fn missing_client_and_missing_token_point_at_setup() {
        let empty = Arc::new(MemoryProviderSecretStore::default());
        let endpoint = Arc::new(ScriptedEndpoint {
            responses: Mutex::new(vec![]),
            calls: Mutex::new(0),
        });
        let p = KeychainTokenProvider::new("personal", empty.clone(), endpoint.clone());
        let err = p.access_token(false).await.unwrap_err().to_string();
        assert!(err.contains("Settings → Email"), "got: {err}");

        // Client present but profile unconnected → the connect message.
        empty
            .set(crate::email::SECRET_GMAIL_CLIENT_ID, "x.apps.googleusercontent.com")
            .unwrap();
        empty.set(crate::email::SECRET_GMAIL_CLIENT_SECRET, "shhh").unwrap();
        let err = p.access_token(false).await.unwrap_err().to_string();
        assert!(err.contains("isn't connected"), "got: {err}");
    }
}
