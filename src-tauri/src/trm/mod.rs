//! TRM (Tiny Recursive Model) — privacy classification interface.
//!
//! Spec §3 defers the real implementation behind a trained model; the trait
//! and logging schema are defined now so the trained model drops in cleanly.
//! Until then, `HeuristicClassifier` provides a conservative regex/keyword
//! fallback. `TrmEngine` is a stub that will load a GGUF model via
//! llama-cpp-2 when the associate delivers the trained weights.

pub mod engine;
pub mod heuristic;

pub use engine::TrmEngine;
pub use heuristic::HeuristicClassifier;

// `Classifier` (the trait) is declared `pub` in this module below and is
// reachable as `crate::trm::Classifier` without re-export.

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
    /// of per-detector scores; for the trained TRM it will be the raw logits.
    pub raw_output: Vec<f32>,
}

/// Anything that can decide whether a message is private, public, or uncertain.
///
/// Implementors: [`HeuristicClassifier`] (regex/keyword, ships now) and
/// [`TrmEngine`] (trained model, ships when the model is delivered).
pub trait Classifier: Send + Sync {
    fn classify(&self, text: &str) -> Classification;
}
