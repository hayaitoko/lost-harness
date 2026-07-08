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
use crate::trm::{Classifier, HeuristicClassifier, Label};

fn gate() -> PrivacyGate {
    PrivacyGate::new(Arc::new(HeuristicClassifier::new()))
}

#[test]
fn private_binding_blocks_cloud() {
    let g = gate();
    let d = g.check(&Binding::Private, "any text at all", true);
    match d {
        GateDecision::Block(msg) => assert!(msg.contains("Private binding")),
        other => panic!("expected Block, got {:?}", other),
    }
}

#[test]
fn private_binding_allows_local() {
    let g = gate();
    let d = g.check(&Binding::Private, "any text at all", false);
    assert_eq!(d, GateDecision::Allow);
}

#[test]
fn public_binding_allows_cloud() {
    let g = gate();
    let d = g.check(&Binding::Public, "any text at all", true);
    assert_eq!(d, GateDecision::Allow);
}

#[test]
fn public_binding_allows_local() {
    let g = gate();
    let d = g.check(&Binding::Public, "any text at all", false);
    assert_eq!(d, GateDecision::Allow);
}

#[test]
fn public_binding_overrides_pii() {
    let g = gate();
    // Even with an SSN, a Public binding bypasses the classifier.
    let d = g.check(&Binding::Public, "my SSN is 123-45-6789", true);
    assert_eq!(d, GateDecision::Allow);
}

#[test]
fn auto_private_text_routes_local_on_cloud() {
    let g = gate();
    let d = g.check(&Binding::Auto, "my SSN is 123-45-6789", true);
    assert_eq!(d, GateDecision::RouteLocal);
}

#[test]
fn auto_private_text_allowed_on_local() {
    let g = gate();
    let d = g.check(&Binding::Auto, "my SSN is 123-45-6789", false);
    assert_eq!(d, GateDecision::Allow);
}

#[test]
fn auto_clean_text_allowed_everywhere() {
    let g = gate();
    let d_cloud = g.check(&Binding::Auto, "what's the capital of france?", true);
    let d_local = g.check(&Binding::Auto, "what's the capital of france?", false);
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

    let d = g.check(&Binding::Auto, text, true);
    assert_eq!(d, GateDecision::RouteLocal);
}

#[test]
fn auto_uncertain_text_allowed_on_local() {
    let g = gate();
    let text = "I was diagnosed with the flu last week";
    let d = g.check(&Binding::Auto, text, false);
    assert_eq!(d, GateDecision::Allow);
}

#[test]
fn log_decision_emits_tracing_event() {
    // Smoke test: log_decision must not panic and must accept the standard
    // arguments. The actual storage wiring lands in a later milestone.
    let g = gate();
    g.log_decision(&GateDecision::Allow, "deadbeef", "conv-1");
    g.log_decision(
        &GateDecision::Block("x".into()),
        "deadbeef",
        "conv-1",
    );
    g.log_decision(&GateDecision::RouteLocal, "deadbeef", "conv-1");
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
    assert!(!is_private_endpoint("https://generativelanguage.googleapis.com/v1beta/openai"));
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
