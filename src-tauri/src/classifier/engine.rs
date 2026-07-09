//! Trained privacy classifier — stub for the real model.
//!
//! The delivered classifier (see the `pf-bundle` / PLAN §11) is a two-encoder
//! transformer **ensemble** — fine-tuned bge-small + distilbert, max-vote —
//! exported to INT8 ONNX, fused with the deterministic rules layer. It runs
//! on-device via ONNX Runtime (no Python, no GPU). `EnsembleClassifier::load`
//! is the seam it drops into: it will load the ONNX encoders + tokenizers,
//! run the sliding-window inference, and fuse with the rules. Until it's
//! wired, `load` returns an error and the app falls back to
//! [`crate::classifier::HeuristicClassifier`].
//!
//! (The module was originally named "TRM" for a Tiny Recursive Model; that
//! approach was evaluated and dropped in favor of this ensemble, hence the
//! rename.)

use std::path::Path;

use super::{Classification, Classifier, Label};

/// The trained ensemble classifier. Will carry the loaded ONNX encoders +
/// tokenizers + fused rules once wired via ONNX Runtime.
///
/// For now this is an empty struct: `load` returns an error so callers fall
/// back to [`HeuristicClassifier`]. When integrated, this will hold the two
/// `ort` sessions (bge-small, distilbert), their tokenizers, and the rules
/// engine.
#[derive(Debug)]
pub struct EnsembleClassifier {
    // Fields land here once we wire ONNX Runtime (`ort`):
    //   bge:    ort::Session,
    //   distil: ort::Session,
    //   tokenizer, rules, thresholds, ...
    _private: (),
}

impl EnsembleClassifier {
    /// Load the trained classifier from a model directory (the exported ONNX
    /// encoders + tokenizers + thresholds).
    ///
    /// Currently always returns an error — the ONNX runner isn't wired yet.
    /// The error message is the contract callers check for: "trained
    /// classifier not available — using heuristic fallback".
    pub fn load(_model_dir: &Path) -> anyhow::Result<Self> {
        Err(anyhow::anyhow!(
            "trained classifier not available — using heuristic fallback"
        ))
    }
}

impl Classifier for EnsembleClassifier {
    /// Stub. A real implementation will run `super::rules::detect(text)`
    /// FIRST, unconditionally, as the deterministic layer-0 pre-filter: any
    /// span found there should short-circuit straight to `Label::Private` at
    /// confidence 1.0 (and populate `Classification::spans`) without paying
    /// for transformer inference. Only when layer 0 finds nothing should the
    /// two ONNX encoders (sliding 128-token windows, max private-probability)
    /// run, to cover the categories layer 0 structurally can't see
    /// (PII_NAME, HEALTH, LOCATION, PII_ORG, PERSONAL_CONTEXT). For now,
    /// return `Uncertain` at 0.0 so the gate conservatively routes to local —
    /// the same outcome a missing classifier would produce.
    fn classify(&self, _text: &str) -> Classification {
        Classification {
            label: Label::Uncertain,
            confidence: 0.0,
            raw_output: Vec::new(),
            spans: Vec::new(),
        }
    }
}
