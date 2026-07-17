//! On-device text embedder — the "meaning fingerprint" engine for memory's
//! semantic-search lane (PLAN §9).
//!
//! Wraps a small sentence-embedding model (bge-small-en-v1.5, INT8 ONNX,
//! ~34 MB) run in-process via ONNX Runtime — the exact same runtime, install
//! pattern, and fallback shape as the privacy classifier
//! ([`crate::classifier::EnsembleClassifier`]): models live under
//! `<storage>/models/embedder/` (`model.int8.onnx` + `tokenizer.json`, not in
//! git, installed out-of-band), and a missing model dir never breaks the app —
//! memory search just runs keyword-only until the model is installed.
//!
//! **This is deliberately NOT the classifier's bge encoder.** That model was
//! fine-tuned into a binary private/public classification head; its hidden
//! states no longer make general-purpose sentence embeddings. This is the
//! stock retrieval-tuned bge-small-en-v1.5.
//!
//! bge specifics honored here (per the model card):
//! - **Query vs passage asymmetry** — a *query* is prefixed with
//!   "Represent this sentence for searching relevant passages: "; a *passage*
//!   (the saved fact) is embedded as-is. Hence the two trait methods.
//! - **CLS pooling** — the sentence vector is the `[CLS]` (first) token's
//!   last-hidden-state, L2-normalized. Normalized vectors make cosine distance
//!   the right comparison (sqlite-vec `vec_distance_cosine`).
//! - **512-token cap** — inputs are truncated (memory facts are short; a
//!   truncated tail on a pathological input is acceptable for retrieval).
//!
//! Everything here stays on-device: indexing memory never sends anything off
//! the box (PLAN §9 "the meaning fingerprint … is computed by a small local
//! model").

use std::path::Path;

/// The embedding dimension of bge-small-en-v1.5. Stored blobs are validated
/// against this (`4 * EMBED_DIM` bytes) before entering a distance query.
pub const EMBED_DIM: usize = 384;

/// Object-safe embedding interface. The real implementation is
/// [`OnnxEmbedder`]; tests substitute deterministic fakes.
pub trait TextEmbedder: Send + Sync {
    /// Embed a search query (bge applies its query prefix).
    fn embed_query(&self, text: &str) -> anyhow::Result<Vec<f32>>;
    /// Embed a stored fact / passage (no prefix).
    fn embed_passage(&self, text: &str) -> anyhow::Result<Vec<f32>>;
}

/// Serialize an f32 vector to the little-endian blob layout sqlite-vec
/// expects for `vec_distance_cosine`.
pub fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// Deterministic test embedder: each `(needle, axis)` pair maps any text
/// containing `needle` (case-insensitive) to the unit vector on that axis of
/// an 8-dim space; unmatched text lands on the last axis. Lets tests stage
/// "semantically related, zero keyword overlap" pairs by giving two different
/// phrasings the same axis.
#[cfg(test)]
pub struct FakeEmbedder(pub Vec<(&'static str, usize)>);

#[cfg(test)]
impl FakeEmbedder {
    fn embed(&self, text: &str) -> Vec<f32> {
        let lower = text.to_lowercase();
        let axis = self
            .0
            .iter()
            .find(|(needle, _)| lower.contains(&needle.to_lowercase()))
            .map(|&(_, a)| a.min(7))
            .unwrap_or(7);
        let mut v = vec![0.0f32; 8];
        v[axis] = 1.0;
        v
    }
}

#[cfg(test)]
impl TextEmbedder for FakeEmbedder {
    fn embed_query(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        Ok(self.embed(text))
    }
    fn embed_passage(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        Ok(self.embed(text))
    }
}

#[cfg(feature = "onnx-classifier")]
pub use onnx::OnnxEmbedder;

#[cfg(not(feature = "onnx-classifier"))]
pub use stub::OnnxEmbedder;

// ── stub (feature off) ──────────────────────────────────────────────────────
#[cfg(not(feature = "onnx-classifier"))]
mod stub {
    use super::*;

    /// Placeholder when the `onnx-classifier` feature (which carries the `ort`
    /// + `tokenizers` deps this module shares) is disabled: `load` always
    /// errors, so memory search runs keyword-only.
    #[derive(Debug)]
    pub struct OnnxEmbedder {
        _private: (),
    }

    impl OnnxEmbedder {
        pub fn load(_model_dir: &Path) -> anyhow::Result<Self> {
            Err(anyhow::anyhow!(
                "embedder not available (onnx-classifier feature disabled) — memory search is keyword-only"
            ))
        }
    }

    impl TextEmbedder for OnnxEmbedder {
        fn embed_query(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Err(anyhow::anyhow!("embedder feature disabled"))
        }
        fn embed_passage(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Err(anyhow::anyhow!("embedder feature disabled"))
        }
    }
}

// ── real implementation (feature on) ────────────────────────────────────────
#[cfg(feature = "onnx-classifier")]
mod onnx {
    use super::*;

    use ndarray::Array2;
    use ort::session::Session;
    use ort::value::Tensor;
    use parking_lot::Mutex;
    use tokenizers::Tokenizer;

    /// bge's recommended prefix for short retrieval queries (model card).
    const QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";
    /// BERT-family hard input cap.
    const MAX_TOKENS: usize = 512;

    /// The bge-small-en-v1.5 embedder: one ONNX session + tokenizer.
    /// `Session::run` takes `&mut self`, and the embedder is shared behind an
    /// `Arc<dyn TextEmbedder>`, hence the `Mutex` — same shape as the
    /// classifier's encoders; embedding isn't a hot concurrent path.
    pub struct OnnxEmbedder {
        session: Mutex<Session>,
        tokenizer: Tokenizer,
        /// Whether the exported graph declares a `token_type_ids` input (BERT
        /// exports usually do); fed zeros when present.
        wants_token_types: bool,
    }

    impl std::fmt::Debug for OnnxEmbedder {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("OnnxEmbedder").finish_non_exhaustive()
        }
    }

    impl OnnxEmbedder {
        /// Load from a dir containing `model.int8.onnx` + `tokenizer.json`.
        /// Errors — so the caller runs keyword-only — if anything is missing.
        pub fn load(model_dir: &Path) -> anyhow::Result<Self> {
            let onnx_path = model_dir.join("model.int8.onnx");
            let tok_path = model_dir.join("tokenizer.json");
            if !onnx_path.exists() || !tok_path.exists() {
                anyhow::bail!(
                    "embedder model files missing under {} (need model.int8.onnx + tokenizer.json)",
                    model_dir.display()
                );
            }
            let session = Session::builder()?.commit_from_file(&onnx_path)?;
            let wants_token_types = session
                .inputs
                .iter()
                .any(|i| i.name == "token_type_ids");
            let mut tokenizer =
                Tokenizer::from_file(&tok_path).map_err(|e| anyhow::anyhow!("{e}"))?;
            // Same gotcha as the classifier: an exported tokenizer.json can
            // bake in fixed padding. We embed one sequence at a time and do
            // our own truncation, so disable both.
            tokenizer.with_padding(None);
            tokenizer
                .with_truncation(None)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(Self {
                session: Mutex::new(session),
                tokenizer,
                wants_token_types,
            })
        }

        /// Tokenize (with special tokens, truncated to [`MAX_TOKENS`]), run the
        /// encoder, CLS-pool, L2-normalize.
        fn embed_raw(&self, text: &str) -> anyhow::Result<Vec<f32>> {
            let encoding = self
                .tokenizer
                .encode(text, true)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let mut ids: Vec<i64> = encoding.get_ids().iter().map(|&x| x as i64).collect();
            if ids.is_empty() {
                anyhow::bail!("empty tokenization");
            }
            ids.truncate(MAX_TOKENS);
            let len = ids.len();

            let input_ids = Array2::from_shape_vec((1, len), ids)?;
            let attn = Array2::from_shape_vec((1, len), vec![1i64; len])?;

            let mut session = self.session.lock();
            let outputs = if self.wants_token_types {
                let type_ids = Array2::from_shape_vec((1, len), vec![0i64; len])?;
                session.run(ort::inputs![
                    "input_ids" => Tensor::from_array(input_ids)?,
                    "attention_mask" => Tensor::from_array(attn)?,
                    "token_type_ids" => Tensor::from_array(type_ids)?,
                ])?
            } else {
                session.run(ort::inputs![
                    "input_ids" => Tensor::from_array(input_ids)?,
                    "attention_mask" => Tensor::from_array(attn)?,
                ])?
            };
            let (shape, hidden) = outputs["last_hidden_state"].try_extract_tensor::<f32>()?;
            let dim = *shape.last().unwrap_or(&0) as usize;
            if dim != EMBED_DIM {
                anyhow::bail!("embedder produced dim {dim}, expected {EMBED_DIM}");
            }
            // CLS pooling: the first token's vector of the (only) batch row.
            let mut v: Vec<f32> = hidden[..dim].to_vec();
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if !(norm.is_finite() && norm > 0.0) {
                anyhow::bail!("embedding norm is zero/non-finite");
            }
            for x in &mut v {
                *x /= norm;
            }
            Ok(v)
        }
    }

    impl TextEmbedder for OnnxEmbedder {
        fn embed_query(&self, text: &str) -> anyhow::Result<Vec<f32>> {
            self.embed_raw(&format!("{QUERY_PREFIX}{text}"))
        }
        fn embed_passage(&self, text: &str) -> anyhow::Result<Vec<f32>> {
            self.embed_raw(text)
        }
    }
}

#[cfg(all(test, feature = "onnx-classifier"))]
mod live_model_tests {
    use super::*;

    /// Sanity test on the real installed model. Opt-in like the classifier's
    /// parity test: set `LHP_EMBEDDER_MODELS_DIR` (the dir holding
    /// `model.int8.onnx` + `tokenizer.json`) to run; otherwise skips.
    #[test]
    fn related_text_is_nearer_than_unrelated() {
        let Some(dir) = std::env::var_os("LHP_EMBEDDER_MODELS_DIR") else {
            eprintln!("skipping embedder live test — set LHP_EMBEDDER_MODELS_DIR to run");
            return;
        };
        let emb = OnnxEmbedder::load(std::path::Path::new(&dir))
            .expect("load embedder from LHP_EMBEDDER_MODELS_DIR");

        let q = emb.embed_query("where do I keep the server login key?").unwrap();
        let related = emb
            .embed_passage("the deploy key for the homelab lives in the vault")
            .unwrap();
        let unrelated = emb
            .embed_passage("the standup meeting moved to 10am on Tuesdays")
            .unwrap();

        assert_eq!(q.len(), EMBED_DIM);
        let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm(&q) - 1.0).abs() < 1e-3, "query must be L2-normalized");

        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        let sim_related = dot(&q, &related);
        let sim_unrelated = dot(&q, &unrelated);
        assert!(
            sim_related > sim_unrelated,
            "semantically related fact must score nearer (related {sim_related:.3} vs unrelated {sim_unrelated:.3})"
        );
    }
}

#[cfg(all(test, feature = "onnx-classifier"))]
mod live_gate_calibration {
    use super::*;

    /// Prints real cosine DISTANCES for related/adjacent/unrelated pairs so the
    /// SEMANTIC_MAX_DIST_* gates can be sanity-checked against the live model
    /// (opt-in via LHP_EMBEDDER_MODELS_DIR; run with --nocapture to see them).
    #[test]
    fn print_distance_bands() {
        let Some(dir) = std::env::var_os("LHP_EMBEDDER_MODELS_DIR") else {
            return;
        };
        let emb = OnnxEmbedder::load(std::path::Path::new(&dir)).unwrap();
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        let q = emb.embed_query("when did we fix the furnace?").unwrap();
        for (label, passage) in [
            ("related    ", "the heater was repaired in March"),
            ("adjacent   ", "the air conditioner filter needs replacing"),
            ("unrelated-1", "groceries are delivered on Sundays"),
            ("unrelated-2", "the standup meeting moved to 10am"),
        ] {
            let p = emb.embed_passage(passage).unwrap();
            eprintln!("dist {} = {:.3}  ({passage})", label, 1.0 - dot(&q, &p));
        }
    }
}
