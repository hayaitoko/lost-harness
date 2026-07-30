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
//!  - `Public`  — the user explicitly opted in. Always runs detection
//!                 (H-12); non-sensitive content passes, while a hit on the
//!                 **un-tunable rules floor** yields `ConfirmRequired` — ONE
//!                 send the user must authorise explicitly, which then expires.
//!                 Like `Auto` and `Private`, this arm is about EGRESS: on a
//!                 non-cloud endpoint nothing leaves the device, so there is
//!                 nothing to confirm and the send is `Allow`ed.
//!  - `Private` — the user explicitly opted in. Cloud endpoints are
//!                 always `Block`ed.
//!
//! The `GateDecision::RouteLocal` outcome is enforced by `is_private_endpoint`
//! in `egress.rs` at the actual HTTP boundary.
//!
//! ## C-01 — fail closed when the trained classifier is missing
//!
//! The gate holds a shared [`crate::classifier::ClassifierHealth`]. When the
//! trained ONNX ensemble did not load (`degraded`), `Auto` + a cloud endpoint is
//! `RouteLocal` **regardless of the fallback's label** — a rules-only miss is no
//! evidence that content is cloud-safe. The same `Arc` is read by
//! `ipc::get_classifier_health` so the UI can tell the user screening is reduced.
//!
//! ## H-12 — the expiring one-send confirmation
//!
//! `Public` used to be a blanket bypass. It now runs the deterministic rules
//! floor on every message. A floor hit returns
//! [`GateDecision::ConfirmRequired`], carrying a fingerprint that pins *this
//! exact text*. The frontend asks the user, calls `confirm_public_send`, and
//! re-sends; [`PublicSendConfirmations`] then authorises **exactly one** send of
//! that text and drops the grant. Grants also time out
//! ([`PublicSendConfirmations::DEFAULT_TTL`]), so an un-used confirmation cannot
//! sit around as a standing allow. This mirrors the audio boundary's
//! `AudioEgressGate::resolve_confirm` (`Once`-scope, single-use) rather than
//! inventing a second policy.
//!
//! The banner → `confirm_public_send` → re-send round trip is the MESSAGE
//! path's affordance. The TOOL path has no such banner, so
//! `hooks::privacy_filter` resolves the same `ConfirmRequired` through the
//! approval spine instead (`HookResult::Ask` → the approval dialog → a ledger
//! grant pinned to that exact tool+args). Two surfaces, one verdict — see that
//! module's docs for why it does not consume from this store.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::classifier::{Classification, Classifier, ClassifierConfig, ClassifierHealth};

/// Per-conversation routing binding (spec §12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Binding {
    /// The classifier (or fallback) decides per-message.
    #[default]
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
    /// H-12: `Public` binding, but the un-tunable rules floor found a structured
    /// secret / PII in the text. NOT a block and NOT an allow — the user must
    /// authorise this ONE send. The caller surfaces `reason`, and on the user's
    /// "send anyway" records the grant via
    /// [`PrivacyGate::confirm_public_send`] (or `ipc::confirm_public_send`) and
    /// retries. The grant is single-use and time-limited, so it can never
    /// degrade into a persistent allow.
    ConfirmRequired {
        /// Pins THIS exact text (sha256 over a domain-separated canonical form).
        fingerprint: String,
        /// User-safe explanation for the confirmation prompt.
        reason: String,
    },
}

/// H-12: the store of outstanding one-send confirmations, keyed by the
/// fingerprint of the exact text the user authorised.
///
/// Two properties make this a *confirmation* rather than an *allow-list*:
///  1. **Single use** — [`take`](Self::take) removes the grant under the same
///     lock acquisition that checks it, so two concurrent sends of identical
///     text cannot both consume one confirmation.
///  2. **Expiring** — a grant older than the TTL is refused (and swept), so a
///     confirmation the user never followed through on does not linger.
#[derive(Debug)]
pub struct PublicSendConfirmations {
    granted: Mutex<HashMap<String, Instant>>,
    ttl: Duration,
}

impl Default for PublicSendConfirmations {
    fn default() -> Self {
        Self::with_ttl(Self::DEFAULT_TTL)
    }
}

impl PublicSendConfirmations {
    /// How long a confirmation stays usable. Long enough for the round trip
    /// (banner → click → re-send), short enough that it isn't a standing allow.
    pub const DEFAULT_TTL: Duration = Duration::from_secs(120);

    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            granted: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Record the user's authorisation for one send of `fingerprint`.
    pub fn grant(&self, fingerprint: &str) {
        let mut g = match self.granted.lock() {
            Ok(g) => g,
            // A poisoned lock must not become an implicit allow; drop the grant.
            Err(_) => return,
        };
        let ttl = self.ttl;
        g.retain(|_, at| at.elapsed() < ttl);
        g.insert(fingerprint.to_string(), Instant::now());
    }

    /// Atomically check-and-consume. `true` iff a **fresh** grant existed; it is
    /// gone afterwards either way (an expired grant is swept, not honoured).
    pub fn take(&self, fingerprint: &str) -> bool {
        let mut g = match self.granted.lock() {
            Ok(g) => g,
            Err(_) => return false, // fail closed
        };
        match g.remove(fingerprint) {
            Some(at) => at.elapsed() < self.ttl,
            None => false,
        }
    }

    /// Test/introspection helper: is a fresh grant present (without consuming)?
    pub fn holds(&self, fingerprint: &str) -> bool {
        self.granted
            .lock()
            .map(|g| g.get(fingerprint).is_some_and(|at| at.elapsed() < self.ttl))
            .unwrap_or(false)
    }
}

/// The §7 privacy gate. Owns a `Box<dyn Classifier>` so the trained classifier
/// and the heuristic fallback are interchangeable at the call site. `Clone` is
/// cheap (an `Arc` bump) — a Wave-4.3 delegated sub-agent runs as a fresh
/// `AgentLoop` built with a clone of the parent's gate (same classifier).
///
/// C-01: when the trained ONNX classifier is unavailable (`health.is_degraded()`),
/// the gate fails closed — cloud egress under `Auto` binding is always kept
/// local. `health` is a shared `Arc`, so the IPC layer / UI banner observe the
/// same flag the gate enforces on.
///
/// **There is exactly one gate.** `lib.rs` constructs it once (the message-egress
/// gate) and hands `clone()`s to `AppState` and to the tool-hook chain, because
/// `clone()` is the only construction that shares BOTH `health` and `confirms`.
/// Constructing a second gate for another boundary re-opens the two bugs this
/// type has already had: a never-degraded tool path (C-01) and a confirmation
/// store the IPC command can grant into but the tool path can't read (H-12).
#[derive(Clone)]
pub struct PrivacyGate {
    classifier: Arc<dyn Classifier>,
    /// Shared, observable classifier health. When degraded, only the rules
    /// fallback is screening and cloud egress under `Auto` is refused (C-01).
    health: Arc<ClassifierHealth>,
    /// H-12: outstanding one-send confirmations.
    ///
    /// **Sharing is by `clone()` only.** `clone()` shares this `Arc`, so a
    /// delegated sub-agent's gate and `AppState.gate` consume from the same
    /// store `ipc::confirm_public_send` grants into. [`Self::with_health`] (and
    /// therefore [`Self::new`] / [`Self::new_degraded`]) allocates a FRESH,
    /// EMPTY store — a gate built that way cannot see another gate's grants.
    /// That was a live bug: `lib.rs` built the tool-hook gate with
    /// `with_health`, so an IPC-granted confirmation was unreachable from the
    /// tool path. `lib.rs` now derives the tool-hook gate from the message gate
    /// with `clone()`, which is the only construction that shares both this
    /// store and `health`. Use [`Self::with_confirmations`] to share
    /// deliberately when a clone isn't possible.
    confirms: Arc<PublicSendConfirmations>,
}

impl std::fmt::Debug for PrivacyGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrivacyGate").finish_non_exhaustive()
    }
}

impl PrivacyGate {
    /// Build a gate around the given classifier (trained or heuristic).
    ///
    /// The gate starts in normal (non-degraded) mode. Use [`new_degraded`] when
    /// the trained ONNX classifier is unavailable (C-01).
    pub fn new(classifier: Arc<dyn Classifier>) -> Self {
        Self::with_health(classifier, ClassifierHealth::healthy())
    }

    /// Build a gate around `classifier` sharing an existing health flag, so a
    /// degraded classifier degrades every gate built from it (C-01).
    ///
    /// This shares `health` and NOT the confirmation store — the new gate starts
    /// with an empty [`PublicSendConfirmations`]. `lib.rs` therefore calls this
    /// exactly ONCE (the message-egress gate) and derives the tool-hook gate
    /// from it with `clone()`; see the `confirms` field docs.
    pub fn with_health(classifier: Arc<dyn Classifier>, health: Arc<ClassifierHealth>) -> Self {
        Self {
            classifier,
            health,
            confirms: Arc::new(PublicSendConfirmations::default()),
        }
    }

    /// Build a gate in degraded mode — the trained ONNX classifier is
    /// unavailable and the rules-only fallback is active. Cloud egress under
    /// `Auto` binding is kept local (fail-closed, C-01).
    pub fn new_degraded(classifier: Arc<dyn Classifier>) -> Self {
        Self::with_health(
            classifier,
            ClassifierHealth::degraded_with("trained classifier unavailable"),
        )
    }

    /// Replace the confirmation store (tests use a short TTL to prove expiry).
    pub fn with_confirmations(mut self, confirms: Arc<PublicSendConfirmations>) -> Self {
        self.confirms = confirms;
        self
    }

    /// Whether the gate is operating in degraded mode (trained classifier
    /// unavailable). The frontend surfaces this as a warning banner via
    /// `ipc::get_classifier_health`.
    pub fn degraded(&self) -> bool {
        self.health.is_degraded()
    }

    /// Why the classifier is degraded, if it is.
    pub fn degraded_reason(&self) -> Option<String> {
        self.health.reason()
    }

    /// The shared health flag (so `AppState` / IPC can report it).
    pub fn health(&self) -> &Arc<ClassifierHealth> {
        &self.health
    }

    /// The shared one-send confirmation store.
    pub fn confirmations(&self) -> &Arc<PublicSendConfirmations> {
        &self.confirms
    }

    /// The fingerprint that pins one exact piece of text for a `Public`-binding
    /// confirmation. Domain-separated from tool-action fingerprints so a tool
    /// approval can never be replayed as a message-send confirmation.
    pub fn public_send_fingerprint(text: &str) -> String {
        crate::hooks::approval::ActionFingerprint::of(
            "privacy_gate.public_send",
            &serde_json::json!({ "text": text }),
        )
    }

    /// Record the user's "send this once anyway" for `text`. Called by
    /// `ipc::confirm_public_send` after the confirmation banner is accepted.
    /// Returns the fingerprint the grant was filed under.
    pub fn confirm_public_send(&self, text: &str) -> String {
        let fp = Self::public_send_fingerprint(text);
        self.confirms.grant(&fp);
        fp
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
    /// send). `None` for `Private` binding (which bypasses the classifier).
    /// `Some` for `Auto` (always classifies) and `Public` (H-12: always runs
    /// detection to surface sensitive content). The classification is computed
    /// at most once.
    pub fn check_detailed(
        &self,
        binding: &Binding,
        text: &str,
        is_cloud_endpoint: bool,
        cfg: &ClassifierConfig,
    ) -> (GateDecision, Option<Classification>) {
        match binding {
            // H-12: always run detection on Public binding and surface the
            // classification so callers can warn the user. A hit on the
            // un-tunable rules floor requires ONE explicit confirmation instead
            // of silently allowing (see `floor_hit` for why the check does not
            // read `classification.spans`).
            Binding::Public => {
                let classification = self.classifier.classify_with(text, cfg);
                // The gate governs EGRESS. `Auto` and `Private` both consult
                // `is_cloud_endpoint`; this arm used to ignore it, so a
                // `Public`-bound send to a LOCAL endpoint still demanded a
                // confirmation for content that never left the device (and, on
                // the tool path, was then hard-denied). Nothing crosses the
                // boundary on a non-cloud endpoint, so there is nothing to
                // confirm — allow it, exactly like `Private` does. The
                // classification is still surfaced so the redaction UI can show
                // what was detected.
                let d = if !is_cloud_endpoint {
                    GateDecision::Allow
                } else if floor_hit(text) {
                    let fingerprint = Self::public_send_fingerprint(text);
                    // A confirmation the user already gave for THIS text
                    // authorises exactly one send, and is consumed here.
                    if self.confirms.take(&fingerprint) {
                        GateDecision::Allow
                    } else {
                        GateDecision::ConfirmRequired {
                            fingerprint,
                            reason: "This message contains a secret or personal identifier. \
                                     Confirm to send it to the cloud provider this once."
                                .to_string(),
                        }
                    }
                } else {
                    GateDecision::Allow
                };
                (d, Some(classification))
            }

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
            // C-01: when the trained classifier is unavailable (degraded mode),
            // cloud egress is blocked regardless of the fallback label — the
            // gate fails closed.
            Binding::Auto => {
                let classification = self.classifier.classify_with(text, cfg);
                let d = if self.health.is_degraded() && is_cloud_endpoint {
                    GateDecision::RouteLocal
                } else {
                    match classification.label {
                        crate::classifier::Label::Public => GateDecision::Allow,
                        crate::classifier::Label::Private | crate::classifier::Label::Uncertain => {
                            if is_cloud_endpoint {
                                GateDecision::RouteLocal
                            } else {
                                GateDecision::Allow
                            }
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
    pub fn log_decision(&self, decision: &GateDecision, text_hash: &str, conversation_id: &str) {
        let decision_str = match decision {
            GateDecision::Allow => "allow",
            GateDecision::Block(_) => "block",
            GateDecision::RouteLocal => "route_local",
            GateDecision::ConfirmRequired { .. } => "confirm_required",
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

/// H-12: does `text` hit the **un-tunable structured-secret floor**?
///
/// This deliberately runs [`crate::classifier::rules::detect`] directly instead
/// of inspecting `classification.spans`. The spans route was the hole the review
/// flagged: `HeuristicClassifier` (and the `EnsembleClassifier` stub) return an
/// EMPTY `spans` vec by contract — see `Classification::spans` — so a
/// spans-based floor check silently never fired whenever those classifiers were
/// active, which is exactly the fresh-install / degraded configuration H-12 is
/// about. Calling the rules layer directly makes the floor **classifier-
/// independent**: it fires identically under the ensemble, the rules classifier,
/// and the heuristic. `AudioEgressGate::tts_egress` already reaches for
/// `rules::detect` at the voice boundary for the same reason.
///
/// Only *structured* categories count. `RuleCategory::Proprietary` (the
/// "confidential" / "do not share" keyword cues) is deliberately EXCLUDED here:
/// under an explicit `Public` binding, prompting on the mere word
/// "confidential" would train the user to click through confirmations, which
/// destroys the value of the prompt. The stricter audio boundary keeps the cues.
fn floor_hit(text: &str) -> bool {
    use crate::classifier::rules::RuleCategory;
    crate::classifier::rules::detect(text).iter().any(|s| {
        matches!(
            s.category,
            RuleCategory::Credential
                | RuleCategory::PiiId
                | RuleCategory::PiiContact
                | RuleCategory::Financial
        )
    })
}
