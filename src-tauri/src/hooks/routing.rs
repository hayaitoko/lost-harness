//! Hard "must-not-leave-this-host" routing enforcement. Spec §2/§6 of
//! `docs/PLAN.md`: *"A registry-level guarantee that a PII-flagged request
//! literally cannot fail over to a cloud model under pressure — today's
//! routing is a strong default, not a hard rule."* `docs/PLAN.md` §8 M3
//! (folded into item 3, "added to the privacy filter here, while it's
//! already being touched").
//!
//! `PrivacyFilterHook` annotates `EventContext::routing` when the §7 gate
//! returns `RouteLocal` (see `hooks::privacy_filter`); this module is the
//! *only* place allowed to turn that annotation into an actual endpoint
//! choice. When `routing` is `LocalRequired` and no local candidate
//! exists, `enforce_local_routing` returns a named `LocalRoutingViolation`
//! — it never falls back to picking the first (possibly cloud) candidate,
//! which is exactly the silent-fallthrough failure mode this exists to
//! close.
//!
//! "Local" here uses the same definition `AgentLoop::find_local_provider`
//! already uses in production (`Provider::is_local() && Provider::is_private()`)
//! so this can't quietly diverge from what the loop actually calls.

use crate::hooks::RoutingRequirement;
use crate::models::Provider;

/// Returned when a `LocalRequired` routing requirement cannot be
/// satisfied by any candidate endpoint. Deliberately a distinct,
/// named error type (not a generic `anyhow::Error` string) so callers —
/// and tests — can match on it instead of pattern-matching error text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalRoutingViolation {
    pub reason: String,
}

impl std::fmt::Display for LocalRoutingViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "local-only routing violation: {}", self.reason)
    }
}

impl std::error::Error for LocalRoutingViolation {}

/// Pick the endpoint `candidates` that satisfies `routing`.
///
/// - `Unconstrained` — returns the first candidate (existing "pick
///   whatever's configured" behavior), or a violation if there are none
///   at all.
/// - `LocalRequired` — returns the first candidate that is both
///   `ProviderKind::Local` and resolves to a private/loopback `base_url`.
///   If none exists, returns `Err` — this is the loud failure; it is
///   structurally impossible for this branch to return a cloud provider.
pub fn enforce_local_routing<'a>(
    routing: &RoutingRequirement,
    candidates: &'a [Provider],
) -> Result<&'a Provider, LocalRoutingViolation> {
    match routing {
        RoutingRequirement::Unconstrained => {
            candidates.first().ok_or_else(|| LocalRoutingViolation {
                reason: "no endpoint candidates available".to_string(),
            })
        }
        RoutingRequirement::LocalRequired { reason } => candidates
            .iter()
            .find(|p| p.is_local() && p.is_private())
            .ok_or_else(|| LocalRoutingViolation {
                reason: format!(
                    "request is local_required ({reason}) but no local provider is \
                     available — refusing to fail over to a cloud endpoint"
                ),
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ProviderKind;

    fn cloud_provider() -> Provider {
        Provider::new(
            "openai",
            "OpenAI",
            "https://api.openai.com/v1",
            Some("sk-test".to_string()),
            ProviderKind::Cloud,
        )
    }

    fn local_provider() -> Provider {
        Provider::new(
            "local-llama",
            "Local Llama",
            "http://localhost:11434",
            None,
            ProviderKind::Local,
        )
    }

    fn local_required(reason: &str) -> RoutingRequirement {
        RoutingRequirement::LocalRequired {
            reason: reason.to_string(),
        }
    }

    #[test]
    fn local_required_with_only_cloud_candidates_fails_loud() {
        let candidates = vec![cloud_provider()];
        let result = enforce_local_routing(&local_required("PII detected"), &candidates);
        match result {
            Err(violation) => {
                assert!(violation.reason.contains("local_required"));
                assert!(violation.reason.contains("no local provider"));
            }
            Ok(p) => panic!(
                "expected a loud LocalRoutingViolation, but got a provider back: {}",
                p.id
            ),
        }
    }

    #[test]
    fn local_required_never_returns_a_cloud_provider() {
        // Even with a mix of candidates, if none qualifies as local the
        // call must still fail rather than degrade to the first (cloud)
        // candidate in the list.
        let candidates = vec![cloud_provider()];
        let result = enforce_local_routing(&local_required("uncertain content"), &candidates);
        assert!(result.is_err(), "must never silently return a cloud provider");
    }

    #[test]
    fn local_required_with_a_local_candidate_succeeds() {
        let candidates = vec![cloud_provider(), local_provider()];
        let result = enforce_local_routing(&local_required("PII detected"), &candidates)
            .expect("a local candidate exists and must be selected");
        assert_eq!(result.id, "local-llama");
    }

    #[test]
    fn unconstrained_returns_first_candidate() {
        let candidates = vec![cloud_provider(), local_provider()];
        let result = enforce_local_routing(&RoutingRequirement::Unconstrained, &candidates)
            .expect("candidates are non-empty");
        assert_eq!(result.id, "openai");
    }

    #[test]
    fn unconstrained_with_no_candidates_still_errors() {
        let candidates: Vec<Provider> = vec![];
        let result = enforce_local_routing(&RoutingRequirement::Unconstrained, &candidates);
        assert!(result.is_err());
    }

    #[test]
    fn local_required_with_no_candidates_at_all_fails_loud() {
        let candidates: Vec<Provider> = vec![];
        let result = enforce_local_routing(&local_required("PII detected"), &candidates);
        assert!(result.is_err());
    }
}
