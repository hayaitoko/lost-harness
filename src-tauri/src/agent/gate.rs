//! §7 Privacy Gate — route determination + enforcement at the model-call
//! boundary. Spec §12 / §7.
//!
//! ```text
//!                 ┌──────────────────┐
//!   binding ───▶  │  PrivacyGate     │ ───▶ GateDecision
//!   text    ───▶  │   ├─ classifier  │       (Allow | Block | RouteLocal)
//!   cloud?  ───▶  │   └─ endpoint?   │
//!                 └──────────────────┘
//! ```
//!
//! The gate is the single point that decides whether a message may go to a
//! cloud endpoint. Three bindings:
//!
//!  - `Auto`    — the classifier (trained model, or heuristic fallback) labels the text.
//!                 Private + cloud → `RouteLocal`. Public → `Allow`.
//!                 Uncertain + cloud → `RouteLocal` (spec Risk 4:
//!                 "when uncertain, route to private tree").
//!  - `Public`  — the user explicitly opted in. Always `Allow`.
//!  - `Private` — the user explicitly opted in. Cloud endpoints are
//!                 always `Block`ed.
//!
//! The `GateDecision::RouteLocal` outcome is enforced by `is_private_endpoint`
//! in `egress.rs` at the actual HTTP boundary.

use std::sync::Arc;

use crate::classifier::{Classification, Classifier, ClassifierConfig};

/// Per-conversation routing binding (spec §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Binding {
    /// The classifier (or fallback) decides per-message.
    Auto,
    /// User override: every message may go to any endpoint.
    Public,
    /// User override: every message must stay on a local / private endpoint.
    Private,
}

/// The gate's decision for a given message + binding + endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// Send it.
    Allow,
    /// Refuse to send. The string is a user-safe explanation.
    Block(String),
    /// The gate's classifier said "private" or "uncertain"; the caller must
    /// re-route to a local model / private endpoint instead of failing.
    RouteLocal,
}

/// The §7 privacy gate. Owns a `Box<dyn Classifier>` so the trained classifier
/// and the heuristic fallback are interchangeable at the call site.
pub struct PrivacyGate {
    classifier: Arc<dyn Classifier>,
}

impl std::fmt::Debug for PrivacyGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrivacyGate").finish_non_exhaustive()
    }
}

impl PrivacyGate {
    /// Build a gate around the given classifier (trained or heuristic).
    pub fn new(classifier: Arc<dyn Classifier>) -> Self {
        Self { classifier }
    }

    /// Decide whether `text` may be sent to a cloud endpoint under `binding`.
    ///
    /// `is_cloud_endpoint` is the upstream-supplied signal for "this base URL
    /// points off-device". `cfg` carries the active profile's classifier
    /// thresholds (only consulted on the `Auto` binding, where the classifier
    /// runs). `check` doesn't resolve the URL itself — it just applies the
    /// binding/label policy. The actual egress control (allowing or refusing the
    /// HTTP call) lives in `agent::egress::is_private_endpoint`.
    pub fn check(
        &self,
        binding: &Binding,
        text: &str,
        is_cloud_endpoint: bool,
        cfg: &ClassifierConfig,
    ) -> GateDecision {
        self.check_detailed(binding, text, is_cloud_endpoint, cfg).0
    }

    /// Like [`check`], but also returns the `Classification` the `Auto` binding
    /// computed (so a caller can see the detected spans — e.g. for redact-and-
    /// send). `None` for `Public`/`Private` bindings, which bypass the
    /// classifier. The classification is computed at most once.
    pub fn check_detailed(
        &self,
        binding: &Binding,
        text: &str,
        is_cloud_endpoint: bool,
        cfg: &ClassifierConfig,
    ) -> (GateDecision, Option<Classification>) {
        match binding {
            // User explicitly chose cloud for this conversation — bypass the
            // classifier entirely.
            Binding::Public => (GateDecision::Allow, None),

            // User explicitly chose private. Cloud endpoints are never OK
            // regardless of content. The block message names the binding so
            // the UI can surface a useful "switch binding to Public to send"
            // prompt.
            Binding::Private => {
                let d = if is_cloud_endpoint {
                    GateDecision::Block("Private binding blocks cloud egress".to_string())
                } else {
                    GateDecision::Allow
                };
                (d, None)
            }

            // The classifier (or fallback) decides. The endpoint's cloud-ness only
            // matters when the label is Private or Uncertain — Public text
            // is always safe to send.
            Binding::Auto => {
                let classification = self.classifier.classify_with(text, cfg);
                let d = match classification.label {
                    crate::classifier::Label::Public => GateDecision::Allow,
                    crate::classifier::Label::Private | crate::classifier::Label::Uncertain => {
                        if is_cloud_endpoint {
                            GateDecision::RouteLocal
                        } else {
                            GateDecision::Allow
                        }
                    }
                };
                (d, Some(classification))
            }
        }
    }

    /// Record the gate's decision. Spec §3 defines the `trm_logs` schema; we
    /// log the *hash* of the text, never the text itself.
    ///
    /// For now this is `tracing::info` — the storage layer wiring lands when
    /// the storage module's M1 surface is finalized.
    pub fn log_decision(
        &self,
        decision: &GateDecision,
        text_hash: &str,
        conversation_id: &str,
    ) {
        let decision_str = match decision {
            GateDecision::Allow => "allow",
            GateDecision::Block(_) => "block",
            GateDecision::RouteLocal => "route_local",
        };
        tracing::info!(
            target: "lhp::classifier",
            decision = decision_str,
            text_hash = %text_hash,
            conversation_id = %conversation_id,
            "privacy_gate.decision"
        );
    }
}
