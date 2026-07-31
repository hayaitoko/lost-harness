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
//! close. An `Unconstrained` requirement is refused for the same reason:
//! see the invariant on `enforce_local_routing`.
//!
//! "Local" here means `Provider::is_local() && Provider::is_private()`, and
//! nothing outside this module re-implements that test — `find_local_provider`,
//! `run_cron`, `LocalModelExtractor::local_provider` and
//! `LocalModelDrafter::local_provider` all call `enforce_local_routing`, so
//! there is exactly one definition in the tree and it cannot quietly diverge
//! from what the loop actually calls. `enforce_local_routing`'s own docs name
//! every call site and the (deliberately arbitrary, deliberately
//! trust-zone-safe) rule it uses to choose among several local endpoints. That
//! list is a claim about the code: `grep -rn 'is_local() && is_private()'` must
//! only ever find this module, the belt-and-suspenders re-checks in
//! `memory_flush` / `skill_reflect`, and doc comments.

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
/// - `LocalRequired` — returns the first candidate that is both
///   `ProviderKind::Local` and resolves to a private/loopback `base_url`.
///   If none exists, returns `Err` — this is the loud failure; it is
///   structurally impossible for this branch to return a cloud provider.
/// - `Unconstrained` — **also `Err`.** This function is a local-routing
///   *enforcer*, not an endpoint chooser. See the invariant below.
///
/// # The `LocalRequired` selection rule, stated plainly
///
/// **First candidate satisfying `is_local() && is_private()`, in the order
/// `candidates` arrives in.** In production `candidates` is
/// `ModelManager::list_providers()`, which the storage layer emits
/// `ORDER BY name` (`storage/global.rs`) — so in practice this is the
/// *alphabetically first local private endpoint*.
///
/// That is arbitrary, and it is written down here rather than left implicit:
/// with two local endpoints registered, which one serves a rerouted turn comes
/// down to their names. What it is **not** arbitrary about is the trust zone.
/// Every provider the predicate admits is private by base URL, so this can only
/// ever move a turn *toward* the private zone — which is the whole reason it is
/// an acceptable way to choose an endpoint, and `candidates.first()` was not.
///
/// # Invariant: this function never chooses an endpoint on its own
///
/// The `Unconstrained` arm used to return `candidates.first()` — "pick
/// whatever's configured". Nothing in production ever reached it (every call
/// site builds a `LocalRequired`), but it sat one careless call site away from
/// being live, and what it did is exactly what the endpoint-routing spec
/// forbids: serving a turn from a provider the user did not pick. "First
/// candidate" there meant *alphabetically first provider of any kind*, cloud
/// included — not a routing decision at all.
///
/// A turn's endpoint has exactly two legitimate sources:
///   1. the provider id the user explicitly selected, validated at the IPC
///      boundary and resolved by `ModelManager::get_provider`; or
///   2. this function, on a `LocalRequired` turn, where the privacy gate has
///      *overridden* that choice and the only acceptable answer is a local
///      endpoint.
///
/// There is no third source, and that is a claim about the code, so here is the
/// full list of what reaches source 2 — every place the backend picks an
/// endpoint the user did not name:
///   - `agent::loop_mod::AgentLoop::find_local_provider` — the §7 gate returned
///     `RouteLocal`, or a cloud send was refused because the conversation's
///     history isn't cloud-safe.
///   - `agent::loop_mod::resolve_turn_outcome` / `agent::loop_mod::lazy_start_then_retry`
///     — a tool result forced the rest of the turn local, including the retry
///     after the bundled sidecar is lazily started.
///   - `agent::loop_mod::AgentLoop::run_cron` — an unattended SCHEDULED job.
///     Nobody is watching it, so it is pinned to a local endpoint and a
///     `Private` binding by construction.
///   - `agent::memory_flush::LocalModelExtractor::local_provider` — not a
///     user turn at all, but it does send trimmed (possibly private) history to
///     a model, so it is held to the same rule.
///   - `agent::skill_reflect::LocalModelDrafter::local_provider` — likewise:
///     it hands a WHOLE prior conversation to a model to draft a skill from.
///
/// All but the second of those used to be hand-rolled
/// `find(|p| p.is_local() && p.is_private())` loops over `list_providers()`.
/// Same predicate, same order, same answer — but one more copy of "what counts
/// as local" each time, which is how two definitions drift apart and one of
/// them stops matching what the enforcer would allow. They now call this
/// function, so the list above is exhaustive by construction rather than by
/// good intentions — and it is checkable in one command:
/// `grep -rn 'enforce_local_routing(' src-tauri/src` finds exactly six call
/// expressions outside the `hooks` module itself, one in each of the six
/// functions named above (five entries; the second names two).
///
/// Keeping that claim true is a maintenance obligation, and two of the entries
/// above — the third (`run_cron`) and the last (the skill drafter) — are here
/// because it had already gone false: the scheduler (a live feature, running
/// unattended turns) and `LocalModelDrafter::local_provider` both hand-rolled
/// the predicate and appeared in no list. `models::runner::ensure_running` is
/// deliberately NOT an entry: it does not *select* among configured providers,
/// it CREATES the bundled sidecar's own `http://127.0.0.1:<port>` provider —
/// and every caller either routes the result back through this function
/// (`lazy_start_then_retry`) or re-derives the zone from the base URL rather
/// than assuming it (`process_message_inner`'s `local_is_cloud`).
///
/// Handing `Unconstrained` to an enforcer is a caller bug, so it is reported as
/// one rather than silently satisfied.
pub fn enforce_local_routing<'a>(
    routing: &RoutingRequirement,
    candidates: &'a [Provider],
) -> Result<&'a Provider, LocalRoutingViolation> {
    match routing {
        RoutingRequirement::Unconstrained => Err(LocalRoutingViolation {
            reason: "enforce_local_routing was called with an Unconstrained requirement — it \
                     enforces local-only routing, it does not choose endpoints. An unconstrained \
                     turn goes to the provider the user explicitly selected; picking the first \
                     configured candidate instead is the silent-fallback bug this module exists \
                     to prevent."
                .to_string(),
        }),
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

/// M5 Slice 2 — the **screenshot privacy invariant (SAFE DEFAULT)**. A turn that
/// carries an on-screen IMAGE (a screenshot) is forced
/// [`RoutingRequirement::LocalRequired`], UPGRADING any weaker requirement —
/// **regardless of binding.** The §7 classifier labels *text*; it cannot vet the
/// contents of a screenshot (which may show anything on the user's display — a
/// password, a bank page, someone else's messages). Critically, the user cannot
/// know what is on screen when a *later* turn captures it, so `Binding::Public`'s
/// text-level cloud opt-in does NOT extend to images (opting into sending the
/// text you just typed is not opting into whatever your screen shows). `Private`
/// is already local, so forcing local there is consistent.
///
/// Per the M5 design (Fix 3 + open question OQ-1): "`image_in_window ⇒
/// LocalRequired` today **regardless**" — screenshots are maximally private, and
/// only a FUTURE, explicit cloud-vision **consent toggle** (OQ-1, not yet decided
/// or built) may ever relax this. Until that toggle ships, this ignores the
/// binding for images. An already-`LocalRequired` base is preserved with its
/// original reason (e.g. PII in the text), so this only ever tightens routing.
pub fn routing_for_turn(base: RoutingRequirement, has_image: bool) -> RoutingRequirement {
    // Never downgrade an existing hard-local requirement (keep its reason).
    if base.is_local_required() {
        return base;
    }
    if has_image {
        return RoutingRequirement::LocalRequired {
            reason: "a screenshot can't be privacy-classified — kept local (maximally private)"
                .to_string(),
        };
    }
    base
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
        assert!(
            result.is_err(),
            "must never silently return a cloud provider"
        );
    }

    #[test]
    fn local_required_with_a_local_candidate_succeeds() {
        let candidates = vec![cloud_provider(), local_provider()];
        let result = enforce_local_routing(&local_required("PII detected"), &candidates)
            .expect("a local candidate exists and must be selected");
        assert_eq!(result.id, "local-llama");
    }

    #[test]
    fn local_required_picks_the_first_local_private_candidate_in_list_order() {
        // The selection rule, pinned so the doc can't drift from it: FIRST
        // candidate satisfying `is_local() && is_private()`, in the order given.
        // Production passes `list_providers()`, which storage emits
        // `ORDER BY name`, so this is "alphabetically first local private
        // endpoint" — arbitrary in WHICH local box it lands on, never in the
        // trust zone. `AgentLoop::find_local_provider` and the memory-flush
        // extractor both route through here, so this is the one rule.
        let first_alphabetically = Provider::new(
            "aardvark-cloud",
            "Aardvark",
            "https://api.aardvark.example/v1",
            Some("sk-test".to_string()),
            ProviderKind::Cloud,
        );
        let box_a = Provider::new(
            "box-a",
            "Box A",
            "http://10.0.0.5:8000/v1",
            None,
            ProviderKind::Local,
        );
        let box_b = Provider::new(
            "box-b",
            "Box B",
            "http://10.0.0.6:8000/v1",
            None,
            ProviderKind::Local,
        );
        let candidates = vec![first_alphabetically, box_a, box_b];

        let picked = enforce_local_routing(&local_required("PII detected"), &candidates)
            .expect("two local candidates exist");
        // The alphabetically-first provider overall is skipped because it is
        // cloud — the predicate, not the position, is what gates the answer.
        assert_eq!(picked.id, "box-a");
        assert!(picked.is_local() && picked.is_private());
    }

    #[test]
    fn local_required_skips_a_local_kind_provider_at_a_public_url() {
        // `kind` is a user-typed label with no enforcement power; the base URL
        // decides. A row someone tagged "Local" while pointing it at a public
        // host must not be able to satisfy a local-only requirement.
        let mislabelled = Provider::new(
            "mislabelled",
            "Definitely My Laptop",
            "https://api.openai.com/v1",
            Some("sk-test".to_string()),
            ProviderKind::Local,
        );
        let candidates = vec![mislabelled];
        assert!(enforce_local_routing(&local_required("PII detected"), &candidates).is_err());
    }

    #[test]
    fn unconstrained_never_picks_an_endpoint() {
        // REGRESSION (endpoint-routing spec): this arm used to return
        // `candidates.first()`. `list_providers()` is ordered `ORDER BY name`,
        // so "first" meant *alphabetically first* — with the stock presets,
        // "Anthropic". A turn must go to the provider the USER picked; an
        // enforcer handing back an arbitrary endpoint is the exact silent
        // fallback this module exists to prevent.
        let candidates = vec![cloud_provider(), local_provider()];
        let err = enforce_local_routing(&RoutingRequirement::Unconstrained, &candidates)
            .expect_err("Unconstrained must never yield an endpoint");
        assert!(
            err.reason.contains("does not choose endpoints"),
            "the refusal must name why, got: {}",
            err.reason
        );
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

    // ── M5 Slice 2: a screenshot forces local routing (safe default) ──────────

    #[test]
    fn a_screenshot_forces_local_regardless_of_binding() {
        // The safe default (M5 Fix 3): image_in_window ⇒ LocalRequired today,
        // regardless of binding — the text-routing `base` already baked the
        // binding in, so an image tightens it further no matter what.
        assert!(
            routing_for_turn(RoutingRequirement::Unconstrained, true).is_local_required(),
            "an image-bearing turn must be forced local"
        );
    }

    #[test]
    fn a_screenshot_over_a_public_cloud_opt_in_still_stays_local() {
        // REGRESSION (review): `Public` is a text-level cloud opt-in — it must
        // NOT extend to a screenshot the user can't see when it's captured.
        // Under Public the text gate yields `Unconstrained` (base), yet an image
        // must still force local until a cloud-vision consent toggle ships.
        let routing = routing_for_turn(RoutingRequirement::Unconstrained, true);
        assert!(
            routing.is_local_required(),
            "an image overrides a Public text opt-in"
        );
        let candidates = [cloud_provider()];
        let result = enforce_local_routing(&routing, &candidates);
        assert!(
            result.is_err(),
            "an unvetted screenshot must never fail over to cloud"
        );
    }

    #[test]
    fn no_image_is_left_unchanged() {
        // A text-only turn is unaffected by this invariant — its routing is
        // whatever the text gate already decided.
        assert!(!routing_for_turn(RoutingRequirement::Unconstrained, false).is_local_required());
    }

    #[test]
    fn an_existing_local_requirement_is_never_downgraded_and_keeps_its_reason() {
        // A turn already local-required (e.g. PII in the text) stays local with
        // its ORIGINAL reason — this only ever tightens, never relabels.
        let base = local_required("PII detected in the text");
        assert_eq!(
            routing_for_turn(base.clone(), false),
            base,
            "must never downgrade or relabel an existing local requirement"
        );
        // …and with an image too, the pre-existing reason wins over the generic one.
        assert_eq!(routing_for_turn(base.clone(), true), base);
    }
}
