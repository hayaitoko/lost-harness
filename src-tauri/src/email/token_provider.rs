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
//! reconnect in Settings" tool error. It travels as the typed
//! [`NeedsReconnect`] error, which is what
//! [`crate::email::connection_state`] downcasts to; the prose is for humans.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::gmail::TokenProvider;
use super::oauth::{self, GcpClient, RefreshError, TokenEndpoint};
use super::BoxFuture;
use crate::secrets::ProviderSecretStore;

/// The stored authorization for a profile is dead (expired or revoked) and
/// only a reconnect can fix it.
///
/// A TYPE, not a marker in prose. This used to be a `[gmail:needs_reconnect]`
/// substring that state-changing code matched on, which meant any text that
/// reached an error message — including an excerpt of an untrusted HTTP
/// response body — could assert the state. Callers that add context with
/// `.context(…)` keep the payload downcastable; callers that rebuild an error
/// from `format!("{e}")` do not, so they must not (see `gmail::execute`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeedsReconnect {
    pub profile: String,
}

impl std::fmt::Display for NeedsReconnect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Gmail needs to be reconnected for the {} profile (the stored authorization expired \
             or was revoked — this is routine for a Testing-status Google client). Reconnect in \
             Settings → Email.",
            self.profile
        )
    }
}

impl std::error::Error for NeedsReconnect {}

/// Text shapes that used to BE state decisions. Nothing parses them any more,
/// but untrusted text (an HTTP body excerpt) is still echoed into error
/// messages, so it is neutralised on the way in: a future reader of these
/// strings cannot be fooled by a body that writes them itself.
const STATE_MARKER_PREFIXES: [&str; 2] = ["[gmail:", "[google:"];

/// Neutralise marker-shaped sequences in untrusted text, keeping the text
/// readable (the bracket becomes a paren) rather than silently deleting it —
/// a truncated excerpt hides what the server actually said.
pub fn scrub_state_markers(text: &str) -> String {
    let mut out = text.to_string();
    for prefix in STATE_MARKER_PREFIXES {
        if out.contains(prefix) {
            out = out.replace(prefix, &prefix.replacen('[', "(", 1));
        }
    }
    out
}

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
    /// Async single-flight lock: only one concurrent refresh per provider at
    /// a time. Held across the network call + keychain write in `refresh_now`.
    refresh_lock: tokio::sync::Mutex<()>,
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
            refresh_lock: tokio::sync::Mutex::new(()),
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
            (Some(client_id), Some(client_secret)) => Ok(GcpClient {
                client_id,
                client_secret,
            }),
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
                    self.secrets
                        .set(&super::secret_gmail_refresh_token(&self.profile), new_rt)
                        .map_err(|e| {
                            anyhow::anyhow!(
                                "failed to persist rotated refresh token for profile {}: {e}",
                                self.profile
                            )
                        })?;
                }
                let expiry = Instant::now()
                    + Duration::from_secs(
                        tokens
                            .expires_in_secs
                            .saturating_sub(EXPIRY_MARGIN_SECS)
                            .max(30),
                    );
                *self.cache.lock() = Some((tokens.access_token.clone(), expiry));
                Ok(tokens.access_token)
            }
            Err(RefreshError::NeedsReconnect { .. }) => Err(anyhow::Error::new(NeedsReconnect {
                profile: self.profile.clone(),
            })),
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
            // Fast path: cache hit with parking_lot (no await, no lock contention).
            if !force_refresh {
                if let Some((token, expiry)) = self.cache.lock().clone() {
                    if Instant::now() < expiry {
                        return Ok(token);
                    }
                }
            }
            // Single-flight: only one task enters the refresh path; others
            // await the result.  The tokio mutex guard is held across the
            // network call + keychain write in `refresh_now`.
            let _guard = self.refresh_lock.lock().await;
            // Double-check: another task may have refreshed the cache while
            // we waited for the lock.
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
        s.set(
            crate::email::SECRET_GMAIL_CLIENT_ID,
            "x.apps.googleusercontent.com",
        )
        .unwrap();
        s.set(crate::email::SECRET_GMAIL_CLIENT_SECRET, "shhh")
            .unwrap();
        s.set(
            &crate::email::secret_gmail_refresh_token("personal"),
            "rt-1",
        )
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
            responses: Mutex::new(vec![
                ok_tokens("at-1", Some("rt-2")),
                ok_tokens("at-2", None),
            ]),
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

    /// A dead grant reaches callers as the TYPED failure (what the banner
    /// decision reads) AND as prose that tells the user what to do.
    #[tokio::test]
    async fn dead_grant_carries_the_typed_needs_reconnect() {
        let store = seeded_store();
        let endpoint = Arc::new(ScriptedEndpoint {
            responses: Mutex::new(vec![Err(RefreshError::NeedsReconnect {
                detail: "invalid_grant".into(),
            })]),
            calls: Mutex::new(0),
        });
        let p = KeychainTokenProvider::new("personal", store, endpoint);
        let err = p.access_token(false).await.unwrap_err();
        assert_eq!(
            err.downcast_ref::<NeedsReconnect>(),
            Some(&NeedsReconnect {
                profile: "personal".into()
            }),
            "got: {err}"
        );
        assert!(err.to_string().contains("Reconnect in Settings"), "{err}");
    }

    /// Untrusted text can no longer wear the shape of a state marker. The
    /// text stays readable — a scrub that deleted it would hide what the
    /// server said.
    #[test]
    fn marker_shaped_text_is_neutralised_not_dropped() {
        let scrubbed = scrub_state_markers("[gmail:needs_reconnect] and [google:api_not_enabled]!");
        assert_eq!(
            scrubbed,
            "(gmail:needs_reconnect] and (google:api_not_enabled]!"
        );
        assert_eq!(scrub_state_markers("ordinary text"), "ordinary text");
    }

    /// A token endpoint that takes its time and counts how often it was hit —
    /// the only way to see whether racing callers collapsed into one refresh.
    struct SlowCountingEndpoint {
        calls: std::sync::atomic::AtomicU32,
        delay: Duration,
    }

    impl TokenEndpoint for SlowCountingEndpoint {
        fn post_form(
            &self,
            _form: Vec<(String, String)>,
        ) -> BoxFuture<'_, anyhow::Result<(u16, String)>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let delay = self.delay;
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                Ok((
                    200,
                    r#"{"access_token":"at-single","expires_in":3599}"#.to_string(),
                ))
            })
        }
    }

    /// The finding: without a single-flight gate, every concurrent tool call
    /// that found a cold/expired cache fired its own refresh — N token
    /// requests for one expiry, each one a chance to race the keychain write.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_refreshes_collapse_into_one_token_request() {
        let store = seeded_store();
        let endpoint = Arc::new(SlowCountingEndpoint {
            calls: std::sync::atomic::AtomicU32::new(0),
            delay: Duration::from_millis(80),
        });
        let provider = Arc::new(KeychainTokenProvider::new(
            "personal",
            store,
            endpoint.clone(),
        ));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let p = Arc::clone(&provider);
            handles.push(tokio::spawn(async move { p.access_token(false).await }));
        }
        for h in handles {
            assert_eq!(h.await.unwrap().unwrap(), "at-single");
        }
        assert_eq!(
            endpoint.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "8 racing callers must produce exactly one token request"
        );
    }

    /// A store that reads fine but refuses to write the refresh token —
    /// models a locked or denied OS keychain at the exact moment Google hands
    /// back a ROTATED refresh token.
    struct WriteFailStore {
        inner: MemoryProviderSecretStore,
    }

    impl ProviderSecretStore for WriteFailStore {
        fn get(&self, id: &str) -> Result<Option<String>, String> {
            self.inner.get(id)
        }
        fn set(&self, id: &str, secret: &str) -> Result<(), String> {
            if id == crate::email::secret_gmail_refresh_token("personal") {
                return Err("keychain is locked".to_string());
            }
            self.inner.set(id, secret)
        }
        fn delete(&self, id: &str) -> Result<(), String> {
            self.inner.delete(id)
        }
    }

    /// The finding: the rotated-token write used to be `let _ = ...`. If it
    /// failed, the OLD refresh token stayed on disk while Google had already
    /// invalidated it — a silent, delayed disconnect. It must surface as an
    /// error, and it must NOT leave a usable access token cached (which would
    /// hide the breakage until the token expired).
    #[tokio::test]
    async fn a_failed_keychain_write_surfaces_and_does_not_cache_the_token() {
        let inner = MemoryProviderSecretStore::default();
        inner
            .set(
                crate::email::SECRET_GMAIL_CLIENT_ID,
                "x.apps.googleusercontent.com",
            )
            .unwrap();
        inner
            .set(crate::email::SECRET_GMAIL_CLIENT_SECRET, "shhh")
            .unwrap();
        inner
            .set(
                &crate::email::secret_gmail_refresh_token("personal"),
                "rt-1",
            )
            .unwrap();
        let store = Arc::new(WriteFailStore { inner });
        let endpoint = Arc::new(ScriptedEndpoint {
            responses: Mutex::new(vec![
                ok_tokens("at-1", Some("rt-2")),
                ok_tokens("at-2", Some("rt-3")),
            ]),
            calls: Mutex::new(0),
        });
        let p = KeychainTokenProvider::new("personal", store.clone(), endpoint.clone());

        let err = p.access_token(false).await.unwrap_err().to_string();
        assert!(
            err.contains("failed to persist rotated refresh token"),
            "got: {err}"
        );
        assert!(
            err.contains("keychain is locked"),
            "the store's own reason survives: {err}"
        );
        // The old token is still what's on disk — we did not pretend otherwise.
        assert_eq!(
            store
                .get(&crate::email::secret_gmail_refresh_token("personal"))
                .unwrap()
                .as_deref(),
            Some("rt-1")
        );
        // Nothing was cached, so the next call retries rather than serving an
        // access token whose refresh token we failed to record.
        let err2 = p.access_token(false).await.unwrap_err().to_string();
        assert!(
            err2.contains("failed to persist rotated refresh token"),
            "got: {err2}"
        );
        assert_eq!(
            *endpoint.calls.lock(),
            2,
            "the failure was retried, not cached over"
        );
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
            .set(
                crate::email::SECRET_GMAIL_CLIENT_ID,
                "x.apps.googleusercontent.com",
            )
            .unwrap();
        empty
            .set(crate::email::SECRET_GMAIL_CLIENT_SECRET, "shhh")
            .unwrap();
        let err = p.access_token(false).await.unwrap_err().to_string();
        assert!(err.contains("isn't connected"), "got: {err}");
    }
}
