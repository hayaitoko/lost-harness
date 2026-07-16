//! Privacy classifier — the interface the privacy filter classifies text
//! through (Private / Public / Uncertain).
//!
//! The trait and logging schema keep the classifiers interchangeable.
//! `RulesClassifier` (rules.rs — layer 0: structured PII + confidentiality
//! cues) is the always-available fallback; `HeuristicClassifier` is the older
//! conservative regex/keyword variant; `EnsembleClassifier` (engine.rs) is the
//! trained model — a bge-small + distilbert INT8 ONNX ensemble fused with the
//! rules layer, wired via ONNX Runtime (PLAN §11). All are interchangeable
//! behind the `Classifier` trait, so the gate never changes when the active
//! classifier does. `EnsembleClassifier::load` errors (→ rules fallback) when
//! its models aren't installed, so the app runs with or without them.

pub mod engine;
pub mod heuristic;
pub mod rules;

pub use engine::EnsembleClassifier;
pub use heuristic::HeuristicClassifier;
pub use rules::{RuleCategory, RulesClassifier, Span};

// `Classifier` (the trait) is declared `pub` in this module below and is
// reachable as `crate::classifier::Classifier` without re-export.

/// Privacy classification of a single piece of text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Label {
    /// Hard PII detected — must not leave the device.
    Private,
    /// No sensitivity signals — safe to send to a cloud endpoint.
    Public,
    /// Detector was unsure; spec Risk 4 says "when uncertain, route to private".
    Uncertain,
}

/// Result of classifying a single message.
#[derive(Debug, Clone)]
pub struct Classification {
    pub label: Label,
    /// 0.0..=1.0. Hard regex matches (SSN, Luhn-valid card) report 1.0;
    /// weaker heuristic matches report 0.7..=0.9.
    pub confidence: f32,
    /// Optional debug payload — for the heuristic classifier this is a vector
    /// of per-detector scores; for the trained ensemble it will be the
    /// per-model private-probabilities.
    pub raw_output: Vec<f32>,
    /// Exact spans backing the decision — populated by [`rules::RulesClassifier`]
    /// (and, once wired, the ensemble's fused rules layer) for the
    /// annotated-redaction UI. Empty for classifiers that only produce a
    /// coarse label (`HeuristicClassifier`, the `EnsembleClassifier` stub).
    pub spans: Vec<rules::Span>,
}

/// Anything that can decide whether a message is private, public, or uncertain.
///
/// Implementors: [`HeuristicClassifier`] (regex/keyword, ships now) and
/// [`EnsembleClassifier`] (trained model, ships when the model is delivered).
pub trait Classifier: Send + Sync {
    fn classify(&self, text: &str) -> Classification;
}
