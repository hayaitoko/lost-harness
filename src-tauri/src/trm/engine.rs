//! Real TRM (Tiny Recursive Model) engine — stub.
//!
//! Spec §3 defers the real implementation behind a trained model. The
//! `TrmEngine::load` signature is defined now so the trained model drops in
//! cleanly: it will accept a path to a GGUF file, load it via llama-cpp-2,
//! and run inference. Until the associate delivers the trained weights,
//! `load` returns an error and the rest of the app falls back to
//! [`crate::trm::HeuristicClassifier`].

use std::path::Path;

use super::{Classification, Classifier, Label};

/// Real TRM engine. Carries the loaded GGUF model + llama-cpp-2 context.
///
/// For now this is an empty struct: `load` returns an error so callers fall
/// back to [`HeuristicClassifier`]. When the trained weights land, this will
/// hold a `LlamaModel` and a `LlamaContext` (or whatever the llama-cpp-2
/// stable API ends up being).
#[derive(Debug)]
pub struct TrmEngine {
    // Fields land here once we wire llama-cpp-2:
    //   model:  LlamaModel,
    //   ctx:    LlamaContext,
    //   config: TrmConfig,
    _private: (),
}

impl TrmEngine {
    /// Load the trained TRM from a GGUF file.
    ///
    /// Currently always returns an error — there is no trained model yet.
    /// The error message is the contract callers check for: "TRM model not
    /// available — using heuristic fallback".
    pub fn load(_model_path: &Path) -> anyhow::Result<Self> {
        Err(anyhow::anyhow!(
            "TRM model not available — using heuristic fallback"
        ))
    }
}

impl Classifier for TrmEngine {
    /// Stub. A real implementation will tokenize `text`, run it through
    /// the recursive model, and map the logits to a [`Label`] + confidence.
    /// For now, return `Uncertain` at 0.0 so the gate conservatively routes
    /// to local — the same outcome a missing classifier would produce.
    fn classify(&self, _text: &str) -> Classification {
        Classification {
            label: Label::Uncertain,
            confidence: 0.0,
            raw_output: Vec::new(),
        }
    }
}
