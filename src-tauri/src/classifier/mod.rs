//! Privacy classifier — the interface the privacy filter classifies text
//! through (Private / Public / Uncertain).
//!
//! The trait and logging schema are defined now so the trained model drops
//! in cleanly. `HeuristicClassifier` is the conservative regex/keyword
//! fallback that ships today; `EnsembleClassifier` is the stub for the real
//! trained model (a bge-small + distilbert ONNX ensemble fused with rules —
//! see engine.rs / PLAN §11). The two are interchangeable behind the
//! `Classifier` trait, so the real model drops in without touching the
//! privacy filter itself.

pub mod engine;
pub mod heuristic;

pub use engine::EnsembleClassifier;
pub use heuristic::HeuristicClassifier;

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
}

/// Anything that can decide whether a message is private, public, or uncertain.
///
/// Implementors: [`HeuristicClassifier`] (regex/keyword, ships now) and
/// [`EnsembleClassifier`] (trained model, ships when the model is delivered).
pub trait Classifier: Send + Sync {
    fn classify(&self, text: &str) -> Classification;
}
