//! Privacy gate + egress control tests.
//!
//! Covers:
//!   - Private binding + cloud → blocked
//!   - Public binding → always allowed
//!   - Auto + PII (SSN) + cloud → route local
//!   - Auto + clean text → allowed
//!   - Auto + borderline text → route local (conservative)
//!   - is_private_endpoint: real private ranges → true
//!   - is_private_endpoint: public hostnames / lookalike hostnames → false

use std::sync::Arc;

use crate::agent::egress::is_private_endpoint;
use crate::agent::gate::{Binding, GateDecision, PrivacyGate};
use crate::classifier::{Classifier, ClassifierConfig, HeuristicClassifier, Label};

fn gate() -> PrivacyGate {
    PrivacyGate::new(Arc::new(HeuristicClassifier::new()))
}

#[test]
fn private_binding_blocks_cloud() {
    let g = gate();
    let d = g.check(
        &Binding::Private,
        "any text at all",
        true,
        &ClassifierConfig::default(),
    );
    match d {
        GateDecision::Block(msg) => assert!(msg.contains("Private binding")),
        other => panic!("expected Block, got {:?}", other),
    }
}

#[test]
fn private_binding_allows_local() {
    let g = gate();
    let d = g.check(
        &Binding::Private,
        "any text at all",
        false,
        &ClassifierConfig::default(),
    );
    assert_eq!(d, GateDecision::Allow);
}

#[test]
fn public_binding_allows_cloud() {
    let g = gate();
    let d = g.check(
        &Binding::Public,
        "any text at all",
        true,
        &ClassifierConfig::default(),
    );
    assert_eq!(d, GateDecision::Allow);
}

#[test]
fn public_binding_allows_local() {
    let g = gate();
    let d = g.check(
        &Binding::Public,
        "any text at all",
        false,
        &ClassifierConfig::default(),
    );
    assert_eq!(d, GateDecision::Allow);
}

/// H-12 (renamed successor of `public_binding_overrides_pii`, whose assertion
/// — "an SSN under Public is silently allowed" — was the finding). A `Public`
/// binding is still an opt-in to cloud, but it is no longer a blanket bypass of
/// the un-tunable structured-secret floor: the gate asks for ONE confirmation.
#[test]
fn public_binding_no_longer_blanket_overrides_pii_it_asks_for_one_confirm() {
    let g = gate();
    match g.check(
        &Binding::Public,
        "my SSN is 123-45-6789",
        true,
        &ClassifierConfig::default(),
    ) {
        GateDecision::ConfirmRequired {
            fingerprint,
            reason,
        } => {
            assert_eq!(
                fingerprint.len(),
                64,
                "a sha256 fingerprint pinning this text"
            );
            assert!(!reason.is_empty(), "the UI needs something to show");
        }
        other => panic!("expected ConfirmRequired for Public+SSN, got {other:?}"),
    }
    // ...and it is NOT a hard block: benign Public content still sails through,
    // so the confirmation only costs the user something when it should.
    assert_eq!(
        g.check(
            &Binding::Public,
            "what's the capital of france?",
            true,
            &ClassifierConfig::default()
        ),
        GateDecision::Allow
    );
}

/// F1: the `Public` arm used to ignore `is_cloud_endpoint`, unlike `Auto` and
/// `Private` which both consult it — so a `Public`-bound send to a LOCAL
/// endpoint demanded a confirmation for content that never crossed the egress
/// boundary (and on the tool path was then hard-denied). The gate governs
/// EGRESS; make the three arms consistent.
#[test]
fn public_binding_asks_for_a_confirm_only_when_something_actually_egresses() {
    let g = gate();
    let cfg = ClassifierConfig::default();
    let text = "my SSN is 123-45-6789";

    // CONTROL: on a CLOUD endpoint the floor still demands one confirmation.
    assert!(
        matches!(
            g.check(&Binding::Public, text, true, &cfg),
            GateDecision::ConfirmRequired { .. }
        ),
        "control: cloud egress of a structured secret must still be confirmed"
    );

    // On a LOCAL endpoint nothing leaves the device → nothing to confirm.
    assert_eq!(
        g.check(&Binding::Public, text, false, &cfg),
        GateDecision::Allow,
        "an on-device send must not demand a cloud-egress confirmation"
    );

    // And the classification is still surfaced for the redaction UI.
    let (d, classification) = g.check_detailed(&Binding::Public, text, false, &cfg);
    assert_eq!(d, GateDecision::Allow);
    assert!(
        classification.is_some(),
        "Public always surfaces a classification, cloud or not"
    );
}

#[test]
fn a_local_public_send_does_not_spend_an_outstanding_cloud_confirmation() {
    // The confirmation the user gave is for the CLOUD send. A local send of the
    // same text in between must not burn it (which is what happened when the
    // Public arm consumed the grant before looking at the endpoint).
    let g = gate();
    let cfg = ClassifierConfig::default();
    let text = "my SSN is 123-45-6789";
    let fp = g.confirm_public_send(text);

    assert_eq!(
        g.check(&Binding::Public, text, false, &cfg),
        GateDecision::Allow
    );
    assert!(
        g.confirmations().holds(&fp),
        "a local send must leave the cloud confirmation on file"
    );
    // ...and it is still spendable on the cloud send it was granted for.
    assert_eq!(
        g.check(&Binding::Public, text, true, &cfg),
        GateDecision::Allow
    );
    assert!(!g.confirmations().holds(&fp), "the cloud send consumed it");
}

#[test]
fn auto_private_text_routes_local_on_cloud() {
    let g = gate();
    let d = g.check(
        &Binding::Auto,
        "my SSN is 123-45-6789",
        true,
        &ClassifierConfig::default(),
    );
    assert_eq!(d, GateDecision::RouteLocal);
}

#[test]
fn auto_private_text_allowed_on_local() {
    let g = gate();
    let d = g.check(
        &Binding::Auto,
        "my SSN is 123-45-6789",
        false,
        &ClassifierConfig::default(),
    );
    assert_eq!(d, GateDecision::Allow);
}

#[test]
fn auto_clean_text_allowed_everywhere() {
    let g = gate();
    let d_cloud = g.check(
        &Binding::Auto,
        "what's the capital of france?",
        true,
        &ClassifierConfig::default(),
    );
    let d_local = g.check(
        &Binding::Auto,
        "what's the capital of france?",
        false,
        &ClassifierConfig::default(),
    );
    assert_eq!(d_cloud, GateDecision::Allow);
    assert_eq!(d_local, GateDecision::Allow);
}

#[test]
fn auto_uncertain_text_routes_local_on_cloud() {
    let g = gate();
    // First-person + health term → Uncertain per heuristic.rs.
    let text = "I was diagnosed with the flu last week";
    // Sanity: the classifier should land on Uncertain.
    let c = HeuristicClassifier.classify(text);
    assert_eq!(c.label, Label::Uncertain);

    let d = g.check(&Binding::Auto, text, true, &ClassifierConfig::default());
    assert_eq!(d, GateDecision::RouteLocal);
}

#[test]
fn auto_uncertain_text_allowed_on_local() {
    let g = gate();
    let text = "I was diagnosed with the flu last week";
    let d = g.check(&Binding::Auto, text, false, &ClassifierConfig::default());
    assert_eq!(d, GateDecision::Allow);
}

#[test]
fn log_decision_emits_tracing_event() {
    // Smoke test: log_decision must not panic and must accept the standard
    // arguments. The actual storage wiring lands in a later milestone.
    let g = gate();
    g.log_decision(&GateDecision::Allow, "deadbeef", "conv-1");
    g.log_decision(&GateDecision::Block("x".into()), "deadbeef", "conv-1");
    g.log_decision(&GateDecision::RouteLocal, "deadbeef", "conv-1");
    g.log_decision(
        &GateDecision::ConfirmRequired {
            fingerprint: "ab".into(),
            reason: "x".into(),
        },
        "deadbeef",
        "conv-1",
    );
}

// --- per-profile thresholds actually change the egress decision ------------

/// A classifier that grades a fixed model score against whatever config it's
/// handed — lets us prove the profile's thresholds reach the gate and flip the
/// routing decision (the whole point of the strictness knob).
#[derive(Debug)]
struct ScoreClassifier(f32);
impl Classifier for ScoreClassifier {
    fn classify(&self, text: &str) -> crate::classifier::Classification {
        self.classify_with(text, &ClassifierConfig::default())
    }
    fn classify_with(
        &self,
        _text: &str,
        cfg: &ClassifierConfig,
    ) -> crate::classifier::Classification {
        let label = if self.0 >= cfg.tau_block {
            Label::Private
        } else if self.0 >= cfg.tau_band {
            Label::Uncertain
        } else {
            Label::Public
        };
        crate::classifier::Classification {
            label,
            confidence: self.0,
            raw_output: vec![self.0],
            spans: Vec::new(),
        }
    }
}

#[test]
fn strictness_config_flips_the_gate_egress_decision() {
    // A borderline model score of 0.03 on a cloud endpoint under Auto binding.
    // Under the DEFAULT config (tau_band 0.05) it's Public → Allow (goes cloud).
    // Under a STRICT profile config (strictness 100 → tau_band ≈ 0.005) it's
    // Uncertain → RouteLocal (kept on device). This is the behavior the whole
    // Round-1 strictness control exists to deliver — proof the knob works.
    let g = PrivacyGate::new(Arc::new(ScoreClassifier(0.03)));
    let default_cfg = ClassifierConfig::default();
    let strict_cfg = ClassifierConfig::from_ui(100, "medium");
    assert_eq!(
        g.check(&Binding::Auto, "borderline", true, &default_cfg),
        GateDecision::Allow,
        "default thresholds send borderline content to cloud"
    );
    assert_eq!(
        g.check(&Binding::Auto, "borderline", true, &strict_cfg),
        GateDecision::RouteLocal,
        "a strict profile keeps the same borderline content local"
    );
}

/// The test the review flagged as failing. Name kept deliberately — it is the
/// regression anchor named in the finding — but the contract it locks is the
/// H-12 one: `Public` no longer bypasses the classifier, so it now surfaces a
/// classification too. Only `Private` (an unconditional refusal that never needs
/// to look at content) returns `None`.
#[test]
fn check_detailed_surfaces_classification_only_for_auto() {
    let g = gate();
    let (_d, auto) = g.check_detailed(
        &Binding::Auto,
        "my SSN is 123-45-6789",
        true,
        &ClassifierConfig::default(),
    );
    assert!(auto.is_some(), "Auto must surface the classification");
    let (d, public) = g.check_detailed(
        &Binding::Public,
        "my SSN is 123-45-6789",
        true,
        &ClassifierConfig::default(),
    );
    let public =
        public.expect("H-12: Public now ALWAYS classifies, so the UI can explain the confirm");
    assert_eq!(
        public.label,
        Label::Private,
        "the surfaced classification must be the real one"
    );
    assert!(
        matches!(d, GateDecision::ConfirmRequired { .. }),
        "and the decision it accompanies is the one-send confirm, got {d:?}"
    );
    let (_d, private) = g.check_detailed(
        &Binding::Private,
        "anything",
        true,
        &ClassifierConfig::default(),
    );
    assert!(
        private.is_none(),
        "Private refuses cloud unconditionally — no classification needed"
    );
}

// --- C-01: degraded classifier ⇒ fail closed -------------------------------

/// A classifier that unconditionally reports `Public` at full confidence — i.e.
/// the most permissive thing a fallback could say. Any cloud egress the gate
/// permits under `Auto` while degraded is therefore permitted *on this
/// classifier's word alone*, which is exactly what C-01 says must not happen.
#[derive(Debug)]
struct AlwaysPublicClassifier;
impl Classifier for AlwaysPublicClassifier {
    fn classify(&self, _text: &str) -> crate::classifier::Classification {
        crate::classifier::Classification {
            label: Label::Public,
            confidence: 1.0,
            raw_output: vec![0.0],
            spans: Vec::new(),
        }
    }
}

#[test]
fn degraded_gate_fails_closed_on_a_fresh_install_where_the_model_never_loaded() {
    // The fresh-install shape: `EnsembleClassifier::load` failed (no models on
    // disk yet), so the app is running on a fallback.
    let healthy = PrivacyGate::new(Arc::new(AlwaysPublicClassifier));
    let degraded = PrivacyGate::new_degraded(Arc::new(AlwaysPublicClassifier));
    let cfg = ClassifierConfig::default();

    // CONTROL: with the trained classifier present, this same text+endpoint is
    // allowed to cloud. (Without this arm the test below could not fail.)
    assert_eq!(
        healthy.check(&Binding::Auto, "ship it", true, &cfg),
        GateDecision::Allow,
        "a healthy gate honours a Public label on a cloud endpoint"
    );
    // C-01: degraded ⇒ the fallback's "Public" is not evidence of cloud-safety.
    assert_eq!(
        degraded.check(&Binding::Auto, "ship it", true, &cfg),
        GateDecision::RouteLocal,
        "a degraded classifier must not authorise cloud egress"
    );
    assert!(degraded.degraded() && !healthy.degraded());
    assert!(
        degraded.degraded_reason().is_some(),
        "the UI banner needs a reason to show"
    );
}

#[test]
fn degraded_gate_still_allows_local_endpoints() {
    // Fail-closed is about EGRESS. A degraded classifier must not brick the app
    // for on-device endpoints, or the fail-closed path is unusable and users
    // will disable it.
    let degraded = PrivacyGate::new_degraded(Arc::new(AlwaysPublicClassifier));
    assert_eq!(
        degraded.check(
            &Binding::Auto,
            "ship it",
            false,
            &ClassifierConfig::default()
        ),
        GateDecision::Allow
    );
}

#[test]
fn degraded_gate_refuses_cloud_for_the_real_rules_fallback_too() {
    // The actual production fallback (`RulesClassifier`) on text it finds
    // nothing in. A rules MISS is no signal — it must not become a cloud pass.
    use crate::classifier::RulesClassifier;
    let cfg = ClassifierConfig::default();
    let clean = "please summarise the meeting notes";
    // The fallback really does call this Public (so the assertion below is about
    // the DEGRADED flag, not about the classifier disagreeing).
    assert_eq!(RulesClassifier::new().classify(clean).label, Label::Public);

    assert_eq!(
        PrivacyGate::new(Arc::new(RulesClassifier::new())).check(&Binding::Auto, clean, true, &cfg),
        GateDecision::Allow,
        "control: not degraded ⇒ cloud is fine"
    );
    assert_eq!(
        PrivacyGate::new_degraded(Arc::new(RulesClassifier::new())).check(
            &Binding::Auto,
            clean,
            true,
            &cfg
        ),
        GateDecision::RouteLocal,
        "cloud fallback: rules-only screening must not clear cloud egress"
    );
}

#[test]
fn the_degraded_flag_is_shared_state_not_a_copy() {
    // C-01's real complaint was that the flag had no observers. It is now an
    // `Arc<ClassifierHealth>`: `lib.rs` hands the SAME arc to the message gate,
    // the tool-hook gate, and `AppState` (for `get_classifier_health`). Prove
    // the sharing is live — flipping the arc changes what an already-built gate
    // (and a clone of it, which is what the tool chain / sub-agents hold) does.
    let health = crate::classifier::ClassifierHealth::healthy();
    let message_gate =
        PrivacyGate::with_health(Arc::new(AlwaysPublicClassifier), Arc::clone(&health));
    let tool_gate = message_gate.clone();
    let cfg = ClassifierConfig::default();

    assert_eq!(
        message_gate.check(&Binding::Auto, "hi", true, &cfg),
        GateDecision::Allow
    );
    assert_eq!(
        tool_gate.check(&Binding::Auto, "hi", true, &cfg),
        GateDecision::Allow
    );

    health.mark_degraded("models dir missing");

    assert!(
        message_gate.degraded() && tool_gate.degraded(),
        "both gates observe the flip"
    );
    assert_eq!(
        message_gate.check(&Binding::Auto, "hi", true, &cfg),
        GateDecision::RouteLocal
    );
    assert_eq!(
        tool_gate.check(&Binding::Auto, "hi", true, &cfg),
        GateDecision::RouteLocal,
        "the tool-hook gate must degrade with the message gate, not silently bypass"
    );
    assert_eq!(
        tool_gate.degraded_reason().as_deref(),
        Some("models dir missing")
    );
}

// --- H-12: the expiring one-send confirmation ------------------------------

#[test]
fn a_public_confirmation_authorises_exactly_one_send_then_re_prompts() {
    let g = gate();
    let cfg = ClassifierConfig::default();
    let text = "my SSN is 123-45-6789";

    // 1. First attempt → confirm required.
    assert!(matches!(
        g.check(&Binding::Public, text, true, &cfg),
        GateDecision::ConfirmRequired { .. }
    ));
    // 2. The user confirms (what `ipc::confirm_public_send` does).
    let fp = g.confirm_public_send(text);
    assert!(g.confirmations().holds(&fp), "the grant is on file");
    // 3. The retry goes through — and CONSUMES the grant.
    assert_eq!(
        g.check(&Binding::Public, text, true, &cfg),
        GateDecision::Allow
    );
    assert!(
        !g.confirmations().holds(&fp),
        "the grant was consumed by the send"
    );
    // 4. A second send of the same text re-prompts: one confirm, one send.
    assert!(
        matches!(
            g.check(&Binding::Public, text, true, &cfg),
            GateDecision::ConfirmRequired { .. }
        ),
        "a confirmation must not become a standing allow"
    );
}

#[test]
fn a_public_confirmation_pins_the_exact_text() {
    let g = gate();
    let cfg = ClassifierConfig::default();
    g.confirm_public_send("my SSN is 123-45-6789");
    // A DIFFERENT secret is a different fingerprint — the confirmation the user
    // gave for one message cannot be spent on another.
    assert!(matches!(
        g.check(&Binding::Public, "my SSN is 987-65-4321", true, &cfg),
        GateDecision::ConfirmRequired { .. }
    ));
}

#[test]
fn a_public_confirmation_expires() {
    use crate::agent::gate::PublicSendConfirmations;
    let cfg = ClassifierConfig::default();
    let text = "my SSN is 123-45-6789";

    // A generous TTL: the confirmation is honoured.
    let fresh =
        PrivacyGate::new(Arc::new(HeuristicClassifier::new())).with_confirmations(Arc::new(
            PublicSendConfirmations::with_ttl(std::time::Duration::from_secs(60)),
        ));
    fresh.confirm_public_send(text);
    assert_eq!(
        fresh.check(&Binding::Public, text, true, &cfg),
        GateDecision::Allow
    );

    // A TTL that lapses before the retry: the stale confirmation is refused.
    let stale =
        PrivacyGate::new(Arc::new(HeuristicClassifier::new())).with_confirmations(Arc::new(
            PublicSendConfirmations::with_ttl(std::time::Duration::from_millis(20)),
        ));
    let fp = stale.confirm_public_send(text);
    std::thread::sleep(std::time::Duration::from_millis(60));
    assert!(!stale.confirmations().holds(&fp), "the grant aged out");
    assert!(
        matches!(
            stale.check(&Binding::Public, text, true, &cfg),
            GateDecision::ConfirmRequired { .. }
        ),
        "an unused confirmation must expire, not sit around as an allow"
    );
}

/// A classifier that calls text `Private` at full confidence but reports NO
/// spans — the documented contract of `HeuristicClassifier` and the
/// `EnsembleClassifier` stub (`Classification::spans`: "Empty for classifiers
/// that only produce a coarse label").
#[derive(Debug)]
struct SpanlessPrivateClassifier;
impl Classifier for SpanlessPrivateClassifier {
    fn classify(&self, _text: &str) -> crate::classifier::Classification {
        crate::classifier::Classification {
            label: Label::Private,
            confidence: 1.0,
            raw_output: vec![1.0],
            spans: Vec::new(),
        }
    }
}

#[test]
fn the_public_floor_fires_even_when_the_classifier_reports_no_spans() {
    // The hole this closes: a spans-based floor check ("any span whose category
    // is Credential/PiiId/...") is vacuously false for every classifier that
    // returns an empty `spans` vec — which includes the heuristic fallback AND
    // the ensemble stub, i.e. exactly the configurations H-12 cares about. The
    // floor now calls `rules::detect` directly, so it is classifier-independent.
    let cfg = ClassifierConfig::default();
    let text = "my SSN is 123-45-6789";

    // Document the precondition: these classifiers really do report no spans.
    assert!(HeuristicClassifier.classify(text).spans.is_empty());
    assert!(SpanlessPrivateClassifier.classify(text).spans.is_empty());

    for (name, g) in [
        (
            "heuristic",
            PrivacyGate::new(Arc::new(HeuristicClassifier::new())),
        ),
        (
            "spanless",
            PrivacyGate::new(Arc::new(SpanlessPrivateClassifier)),
        ),
    ] {
        assert!(
            matches!(
                g.check(&Binding::Public, text, true, &cfg),
                GateDecision::ConfirmRequired { .. }
            ),
            "{name}: the floor must fire without relying on classifier spans"
        );
    }

    // The floor is the RULES layer, not the classifier's label: a classifier
    // screaming "Private" about benign text does NOT manufacture a confirm
    // under Public (that would make the prompt noise the user clicks through).
    assert_eq!(
        PrivacyGate::new(Arc::new(SpanlessPrivateClassifier)).check(
            &Binding::Public,
            "what's the capital of france?",
            true,
            &cfg
        ),
        GateDecision::Allow
    );
}

// --- is_private_endpoint ---------------------------------------------------

#[test]
fn private_endpoint_localhost() {
    assert!(is_private_endpoint("http://localhost:1234/v1"));
    assert!(is_private_endpoint("http://localhost/v1"));
}

#[test]
fn private_endpoint_loopback_v4() {
    assert!(is_private_endpoint("http://127.0.0.1:1234/v1"));
    assert!(is_private_endpoint("http://127.0.0.53:53"));
}

#[test]
fn private_endpoint_rfc1918() {
    assert!(is_private_endpoint("http://10.0.0.5:8080/v1"));
    assert!(is_private_endpoint("http://10.255.255.255:80"));
    assert!(is_private_endpoint("http://192.168.1.1:80"));
    assert!(is_private_endpoint("http://172.16.0.1:80"));
    assert!(is_private_endpoint("http://172.31.255.255:80"));
}

#[test]
fn private_endpoint_tailnet_cgnat() {
    // Tailscale 100.64.0.0/10 — Friday lives at 100.97.80.2.
    assert!(is_private_endpoint("http://100.97.80.2:8765"));
    assert!(is_private_endpoint("http://100.64.0.1:80"));
    assert!(is_private_endpoint("http://100.127.255.254:80"));
}

#[test]
fn private_endpoint_rfc1918_boundary() {
    // 172.15.x.x and 172.32.x.x are NOT in 172.16/12.
    assert!(!is_private_endpoint("http://172.15.0.1:80"));
    assert!(!is_private_endpoint("http://172.32.0.1:80"));
    // 100.63.x.x and 100.128.x.x are NOT in 100.64/10.
    assert!(!is_private_endpoint("http://100.63.255.255:80"));
    assert!(!is_private_endpoint("http://100.128.0.0:80"));
}

#[test]
fn private_endpoint_hostname_suffixes() {
    assert!(is_private_endpoint("http://tadashi.lan:8080/v1"));
    assert!(is_private_endpoint("http://nas.local:5000"));
    assert!(is_private_endpoint("http://server.internal:80"));
    assert!(is_private_endpoint("http://friday.tail.ts.net:8765"));
}

#[test]
fn private_endpoint_ipv6_loopback() {
    // url::Url::host_str returns the IPv6 without brackets.
    assert!(is_private_endpoint("http://[::1]:8080/v1"));
}

#[test]
fn public_endpoint_cloud_hostnames() {
    assert!(!is_private_endpoint("https://api.openai.com/v1"));
    assert!(!is_private_endpoint("https://api.anthropic.com/v1"));
    assert!(!is_private_endpoint("https://openrouter.ai/api/v1"));
    assert!(!is_private_endpoint(
        "https://generativelanguage.googleapis.com/v1beta/openai"
    ));
}

#[test]
fn public_endpoint_lookalike_hostnames() {
    // The hostname looks like a private range but is not a dotted-quad —
    // egress control must not be fooled. Spec §7 explicitly calls this out.
    assert!(!is_private_endpoint("https://10.evil.com/v1"));
    assert!(!is_private_endpoint("https://192.168.phishing.example/v1"));
    // Suffix-only is fine; the *whole* hostname needs to end in the suffix.
    assert!(!is_private_endpoint("https://evil.local.com/v1"));
}

#[test]
fn public_endpoint_public_ips() {
    assert!(!is_private_endpoint("http://8.8.8.8:80"));
    assert!(!is_private_endpoint("http://1.1.1.1:53"));
}

#[test]
fn public_endpoint_invalid_url() {
    // Garbage in → refuse (treat as public). We never want a typo in the
    // base URL to silently let traffic through.
    assert!(!is_private_endpoint("not a url"));
    assert!(!is_private_endpoint(""));
}
