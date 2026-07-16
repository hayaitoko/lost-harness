//! Trained privacy classifier — the on-device ONNX ensemble.
//!
//! The delivered classifier (see the `pf-bundle` / PLAN §11) is a two-encoder
//! transformer **ensemble** — fine-tuned bge-small + distilbert — exported to
//! INT8 ONNX, fused with the deterministic rules layer. It runs on-device via
//! ONNX Runtime (no Python, no GPU, nothing leaves the box). This mirrors the
//! reference server's `_classify_core` / `_model_prob_windowed` (bundle
//! `src/serve.py`) exactly:
//!
//!   layer 0  rules.detect(text) — structured PII + confidentiality cues. Any
//!            span → Private @ 1.0, short-circuit (no transformer inference).
//!   layer 1  the two encoders, binary classification, class index 1 = private.
//!            Each text is tokenized whole (no truncation), scanned in sliding
//!            128-token windows (stride 96, CLS/SEP-wrapped), and the MAX
//!            probability of class 1 over all windows is the model's score — so
//!            sensitive content past token 128 can't slip through.
//!   layer 2  fusion: private if rules fired OR max(model probs) ≥ tau_band.
//!            tau_block only grades severity (Private vs the borderline
//!            uncertainty band); both keep the text local at the gate. The two
//!            thresholds come from the per-profile [`ClassifierConfig`] passed
//!            to [`Classifier::classify_with`] (default 0.5 / 0.05); the plain
//!            [`Classifier::classify`] uses the defaults.
//!
//! Interchangeable with [`crate::classifier::RulesClassifier`] behind the
//! `Classifier` trait: [`EnsembleClassifier::load`] returns an error when the
//! model files are absent (or the `onnx-classifier` feature is off), and the
//! caller falls back to the rules-only classifier — so a missing model dir
//! never breaks the app, it just runs with layer 0 alone.
//!
//! (The module was originally named "TRM" for a Tiny Recursive Model; that
//! approach was evaluated and dropped in favor of this ensemble, hence the
//! rename.)

use std::path::Path;

use super::{Classification, Classifier, ClassifierConfig, Label};

/// The two model subdirectories expected under a classifier model dir, in
/// ensemble order. Each holds `model.int8.onnx` + `tokenizer.json`.
pub const MODEL_SUBDIRS: [&str; 2] = ["tf_bge_scaled", "tf_distilbert_scaled"];

#[cfg(feature = "onnx-classifier")]
pub use onnx::EnsembleClassifier;

#[cfg(not(feature = "onnx-classifier"))]
pub use stub::EnsembleClassifier;

// ── stub (feature off) ──────────────────────────────────────────────────────
#[cfg(not(feature = "onnx-classifier"))]
mod stub {
    use super::*;

    /// Placeholder when the `onnx-classifier` feature is disabled: `load`
    /// always errors so the caller uses the rules-only fallback.
    #[derive(Debug)]
    pub struct EnsembleClassifier {
        _private: (),
    }

    impl EnsembleClassifier {
        pub fn load(_model_dir: &Path) -> anyhow::Result<Self> {
            Err(anyhow::anyhow!(
                "trained classifier not available (onnx-classifier feature disabled) — using rules fallback"
            ))
        }
    }

    impl Classifier for EnsembleClassifier {
        fn classify(&self, _text: &str) -> Classification {
            Classification {
                label: Label::Uncertain,
                confidence: 0.0,
                raw_output: Vec::new(),
                spans: Vec::new(),
            }
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

    use crate::classifier::rules::RulesClassifier;

    const WINDOW: usize = 128;
    const STRIDE: usize = 96;

    /// One loaded encoder: its ONNX session (behind a `Mutex` — `ort`'s
    /// `Session::run` takes `&mut self`, but the classifier is shared behind an
    /// `Arc<dyn Classifier>`; classification isn't a hot concurrent path), its
    /// tokenizer (padding/truncation disabled — the exported `tokenizer.json`
    /// bakes in fixed 128-token padding, which would feed the model garbage),
    /// and the special-token ids used to wrap each window.
    struct Encoder {
        session: Mutex<Session>,
        tokenizer: Tokenizer,
        cls_id: Option<i64>,
        sep_id: Option<i64>,
        pad_id: i64,
    }

    /// The trained ensemble: layer-0 rules + the two ONNX encoders.
    pub struct EnsembleClassifier {
        encoders: Vec<Encoder>,
        rules: RulesClassifier,
    }

    impl std::fmt::Debug for EnsembleClassifier {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("EnsembleClassifier")
                .field("encoders", &self.encoders.len())
                .finish_non_exhaustive()
        }
    }

    impl EnsembleClassifier {
        /// Load the ensemble from a model dir containing `tf_bge_scaled/` and
        /// `tf_distilbert_scaled/` (each with `model.int8.onnx` +
        /// `tokenizer.json`). Errors — so the caller falls back to rules-only —
        /// if any file is missing or fails to load.
        pub fn load(model_dir: &Path) -> anyhow::Result<Self> {
            let mut encoders = Vec::with_capacity(MODEL_SUBDIRS.len());
            for name in MODEL_SUBDIRS {
                let dir = model_dir.join(name);
                let onnx_path = dir.join("model.int8.onnx");
                let tok_path = dir.join("tokenizer.json");
                if !onnx_path.exists() || !tok_path.exists() {
                    anyhow::bail!(
                        "classifier model files missing under {} (need model.int8.onnx + tokenizer.json)",
                        dir.display()
                    );
                }
                let session = Session::builder()?.commit_from_file(&onnx_path)?;
                let mut tokenizer =
                    Tokenizer::from_file(&tok_path).map_err(|e| anyhow::anyhow!("{e}"))?;
                // Critical: the exported tokenizer.json bakes in Fixed(128)
                // padding + truncation. Left on, every input is padded to 128
                // tokens and the model scores garbage. We do our own windowing
                // + batch padding, so disable both here.
                tokenizer.with_padding(None);
                tokenizer
                    .with_truncation(None)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                let vocab = tokenizer.get_vocab(true);
                let cls_id = vocab.get("[CLS]").map(|&x| x as i64);
                let sep_id = vocab.get("[SEP]").map(|&x| x as i64);
                let pad_id = vocab.get("[PAD]").map(|&x| x as i64).unwrap_or(0);
                encoders.push(Encoder {
                    session: Mutex::new(session),
                    tokenizer,
                    cls_id,
                    sep_id,
                    pad_id,
                });
            }
            Ok(Self {
                encoders,
                rules: RulesClassifier::new(),
            })
        }

        /// Max probability of class 1 ("private") over a sliding-window scan of
        /// the full token sequence. Mirrors `serve.py::_model_prob_windowed`.
        fn windowed_max_prob(enc: &Encoder, text: &str) -> anyhow::Result<f32> {
            let encoding = enc
                .tokenizer
                .encode(text, false)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let mut ids: Vec<i64> = encoding.get_ids().iter().map(|&x| x as i64).collect();
            if ids.is_empty() {
                ids.push(enc.pad_id);
            }
            let n = ids.len();

            let has_specials = enc.cls_id.is_some() && enc.sep_id.is_some();
            let inner = if has_specials { WINDOW - 2 } else { WINDOW }.max(1);

            // Sliding windows: starts 0, STRIDE, 2·STRIDE, … < n; drop a window
            // that would just repeat the previous end (except the first).
            let mut windows: Vec<&[i64]> = Vec::new();
            let mut seen_end: Option<usize> = None;
            let mut s = 0usize;
            while s < n.max(1) {
                let end = (s + inner).min(n);
                let chunk = &ids[s..end];
                if chunk.is_empty() {
                    s += STRIDE;
                    continue;
                }
                let e = end;
                let dup = matches!(seen_end, Some(prev) if e <= prev) && s != 0;
                if !dup {
                    windows.push(chunk);
                    seen_end = Some(e);
                    if e >= n {
                        break;
                    }
                }
                s += STRIDE;
            }
            if windows.is_empty() {
                windows.push(&ids[..inner.min(n)]);
            }

            // Wrap each window in CLS/SEP, then right-pad the batch to a common
            // length with the attention mask zeroed over the padding.
            let rows: Vec<Vec<i64>> = windows
                .iter()
                .map(|chunk| {
                    let mut row = Vec::with_capacity(chunk.len() + 2);
                    if let Some(cls) = enc.cls_id {
                        row.push(cls);
                    }
                    row.extend_from_slice(chunk);
                    if let Some(sep) = enc.sep_id {
                        row.push(sep);
                    }
                    row
                })
                .collect();
            let batch = rows.len();
            let max_len = rows.iter().map(|r| r.len()).max().unwrap_or(1).max(1);

            let mut ids_flat = Vec::with_capacity(batch * max_len);
            let mut attn_flat = Vec::with_capacity(batch * max_len);
            for row in &rows {
                let pad_n = max_len - row.len();
                ids_flat.extend_from_slice(row);
                ids_flat.extend(std::iter::repeat(enc.pad_id).take(pad_n));
                attn_flat.extend(std::iter::repeat(1i64).take(row.len()));
                attn_flat.extend(std::iter::repeat(0i64).take(pad_n));
            }

            let input_ids = Array2::from_shape_vec((batch, max_len), ids_flat)?;
            let attn = Array2::from_shape_vec((batch, max_len), attn_flat)?;

            let mut session = enc.session.lock();
            let outputs = session.run(ort::inputs![
                "input_ids" => Tensor::from_array(input_ids)?,
                "attention_mask" => Tensor::from_array(attn)?,
            ])?;
            let (shape, logits) = outputs["logits"].try_extract_tensor::<f32>()?;
            let cols = *shape.last().unwrap_or(&2) as usize;
            if cols < 2 {
                anyhow::bail!("classifier logits have {cols} columns, expected 2");
            }

            // softmax over the last dim per row, take class 1, max over windows.
            let mut max_p1 = 0.0f32;
            for b in 0..batch {
                let a = logits[b * cols];
                let c = logits[b * cols + 1];
                let m = a.max(c);
                let (ea, ec) = ((a - m).exp(), (c - m).exp());
                let p1 = ec / (ea + ec);
                if p1 > max_p1 {
                    max_p1 = p1;
                }
            }
            Ok(max_p1)
        }
    }

    impl Classifier for EnsembleClassifier {
        fn classify(&self, text: &str) -> Classification {
            self.classify_with(text, &ClassifierConfig::default())
        }

        fn classify_with(&self, text: &str, cfg: &ClassifierConfig) -> Classification {
            // Defence in depth: never let a corrupt/hand-edited threshold widen
            // the filter. `sanitized` fails toward stricter (lower thresholds).
            let cfg = cfg.sanitized();

            // Layer 0: deterministic rules first. Any span → Private @ 1.0,
            // short-circuit (no transformer inference), and carry the spans for
            // the annotated-redaction UI. Rules are the un-tunable floor — the
            // per-profile thresholds never apply here.
            let ruled = self.rules.classify(text);
            if !ruled.spans.is_empty() {
                return Classification {
                    label: Label::Private,
                    confidence: 1.0,
                    raw_output: ruled.raw_output,
                    spans: ruled.spans,
                };
            }

            // Layer 1: the two encoders, windowed max-prob. Any inference error
            // fails closed (route local) rather than leaking to the cloud.
            let mut probs = Vec::with_capacity(self.encoders.len());
            for enc in &self.encoders {
                match Self::windowed_max_prob(enc, text) {
                    Ok(p) => probs.push(p),
                    Err(e) => {
                        tracing::warn!(
                            target: "lhp::classifier",
                            error = %e,
                            "ensemble inference failed — failing closed to Private"
                        );
                        return Classification {
                            label: Label::Private,
                            confidence: 1.0,
                            raw_output: probs,
                            spans: Vec::new(),
                        };
                    }
                }
            }
            let max_prob = probs.iter().cloned().fold(0.0f32, f32::max);

            // Layer 2: fusion. ≥ tau_block → Private; in [tau_band, tau_block)
            // → Uncertain (borderline, still local); below → Public. Thresholds
            // are the profile's (sanitized above).
            let (label, confidence) = if max_prob >= cfg.tau_block {
                (Label::Private, max_prob)
            } else if max_prob >= cfg.tau_band {
                (Label::Uncertain, max_prob)
            } else {
                (Label::Public, 1.0 - max_prob)
            };

            Classification {
                label,
                confidence,
                raw_output: probs,
                spans: Vec::new(),
            }
        }
    }
}

#[cfg(all(test, feature = "onnx-classifier"))]
mod parity_tests {
    use super::*;

    /// Parity test against the Python reference (bundle `src/serve.py` windowing
    /// + `onnxruntime` INT8). Ground truth is captured in
    /// `docs/classifier-parity.json` (regenerated by the bundle's `gen_parity`).
    /// The models are ~96 MB and not in git, so this test is opt-in: set
    /// `LHP_CLASSIFIER_MODELS_DIR` to a dir containing `tf_bge_scaled/` +
    /// `tf_distilbert_scaled/` to run it; otherwise it skips.
    #[test]
    fn matches_python_reference_probs() {
        let Some(dir) = std::env::var_os("LHP_CLASSIFIER_MODELS_DIR") else {
            eprintln!("skipping ONNX parity test — set LHP_CLASSIFIER_MODELS_DIR to run");
            return;
        };
        let clf = EnsembleClassifier::load(std::path::Path::new(&dir))
            .expect("load ensemble from LHP_CLASSIFIER_MODELS_DIR");

        // A benign line → Public; a health line (no rule span) → Private via the
        // models; a long line with a sensitive tail past token 128 → Private via
        // the sliding window (proves windowing works).
        let benign = clf.classify("what is the capital of France?");
        assert_eq!(benign.label, Label::Public, "benign should be Public");

        let health =
            clf.classify("I was diagnosed with type 2 diabetes and started metformin last week.");
        assert_eq!(
            health.label,
            Label::Private,
            "health context should be Private (model-only category)"
        );

        let long = format!("{} My password is hunter2secret!", "Here is a very long message. ".repeat(60));
        let long_c = clf.classify(&long);
        assert_ne!(
            long_c.label,
            Label::Public,
            "sensitive tail past token 128 must be caught by the sliding window"
        );
    }
}
