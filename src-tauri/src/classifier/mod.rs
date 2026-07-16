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

/// Per-profile, runtime-tunable fusion thresholds for the trained ensemble
/// (PLAN §11 "a dedicated classifier settings page"). These grade the model's
/// windowed private-probability into Private / Uncertain / Public:
///
///   max_prob ≥ `tau_block`               ⇒ Private
///   `tau_band` ≤ max_prob < `tau_block`  ⇒ Uncertain (borderline, stays local)
///   max_prob < `tau_band`                ⇒ Public
///
/// The deterministic **rules layer ignores these** — a structured-PII hit is
/// always Private regardless of any threshold (the floor can't be tuned away).
/// `Default` mirrors the reference server's env defaults (`PF_TAU_BLOCK` /
/// `PF_TAU_BAND`), so a profile with no saved settings behaves exactly as
/// before this was configurable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClassifierConfig {
    /// max_prob at/above which the model calls a message Private. In (0, 1].
    pub tau_block: f32,
    /// max_prob at/above which a message is at least Uncertain. In (0, tau_block].
    pub tau_band: f32,
}

impl Default for ClassifierConfig {
    fn default() -> Self {
        Self {
            tau_block: Self::DEFAULT_TAU_BLOCK,
            tau_band: Self::DEFAULT_TAU_BAND,
        }
    }
}

impl ClassifierConfig {
    // ── UI mapping (strictness/band ⇄ thresholds) ────────────────────────────
    // The settings page speaks *strictness* (0–100) and a coarse *uncertainty
    // band*, not raw probabilities. These are the single source of truth for
    // that translation (the frontend never computes thresholds itself).
    //
    // WHICH threshold each control drives matters: at the gate, `Private` and
    // `Uncertain` route IDENTICALLY (both stay local; spec Risk 4). So the only
    // threshold that changes what actually reaches the cloud is **tau_band**
    // (the Public / not-Public line). That's the egress dial, so it's what
    // "Detection strictness" drives. **tau_block** only splits the kept-local
    // zone into Private vs Uncertain — a labeling/severity distinction shown in
    // the "why" sidebar — so it's what the "uncertainty band" width drives.

    const DEFAULT_TAU_BLOCK: f32 = 0.5;
    const DEFAULT_TAU_BAND: f32 = 0.05;
    /// Reachable `tau_band` range (egress line). Lower = more paranoid (more
    /// content is at-least-Uncertain and kept local). The default (0.05) sits at
    /// strictness 50 via the log map below; the ends are one order of magnitude
    /// out either way.
    const TAU_BAND_MIN: f32 = 0.005; // strictness 100 (most paranoid)
    const TAU_BAND_MAX: f32 = 0.5; // strictness 0 (most permissive)
    /// Uncertain-zone width, expressed as a FRACTION of the headroom between
    /// `tau_band` and `TAU_BLOCK_MAX`, by band label. A fraction (rather than an
    /// absolute width) keeps `tau_block` from ever hitting the ceiling, so the
    /// band label always round-trips regardless of `tau_band`. Wider ⇒ higher
    /// `tau_block` ⇒ more borderline content labeled Uncertain (both stay local).
    const BAND_FRAC_NARROW: f32 = 0.25;
    const BAND_FRAC_MEDIUM: f32 = 0.5; // default: 0.05 + 0.5·(0.95−0.05) = 0.5 = historical tau_block
    const BAND_FRAC_WIDE: f32 = 0.75;
    /// Hard ceiling on `tau_block` (keep a sliver of "definitely private" room).
    const TAU_BLOCK_MAX: f32 = 0.95;

    /// Build a config from the UI's strictness slider (0–100, clamped) and the
    /// uncertainty band label.
    ///
    /// **Higher strictness ⇒ lower `tau_band` ⇒ more content kept on-device**
    /// (the egress-relevant dial). The map is logarithmic so the historical
    /// default `tau_band = 0.05` lands at the slider midpoint (strictness 50),
    /// with the paranoid/permissive ends an order of magnitude either side.
    /// A wider band ⇒ higher `tau_block` ⇒ more borderline content is labeled
    /// Uncertain rather than Private (a sidebar/severity distinction; both still
    /// stay local). Unknown band → medium.
    pub fn from_ui(strictness: u8, band: &str) -> Self {
        let s = strictness.min(100) as f32;
        // tau_band = DEFAULT * 10^((50 - s)/50): s=50→0.05, s=0→0.5, s=100→0.005.
        let tau_band =
            (Self::DEFAULT_TAU_BAND * 10f32.powf((50.0 - s) / 50.0)).clamp(Self::TAU_BAND_MIN, Self::TAU_BAND_MAX);
        let frac = match band {
            "narrow" => Self::BAND_FRAC_NARROW,
            "wide" => Self::BAND_FRAC_WIDE,
            _ => Self::BAND_FRAC_MEDIUM,
        };
        let tau_block = tau_band + frac * (Self::TAU_BLOCK_MAX - tau_band);
        Self {
            tau_block,
            tau_band,
        }
        .sanitized()
    }

    /// Recover the UI (strictness, band) that best represents these thresholds —
    /// the inverse of [`from_ui`], for rendering the settings page from stored
    /// raw values. Stable for values this code wrote; approximate for
    /// hand-edited DBs (which `sanitized` has already bounded).
    pub fn to_ui(self) -> (u8, &'static str) {
        // Invert the log map: s = 50 - 50·log10(tau_band / DEFAULT).
        let strictness = if self.tau_band > 0.0 {
            (50.0 - 50.0 * (self.tau_band / Self::DEFAULT_TAU_BAND).log10())
                .clamp(0.0, 100.0)
                .round() as u8
        } else {
            100
        };
        // Nearest band by the headroom fraction ((tau_block − tau_band) /
        // (TAU_BLOCK_MAX − tau_band)) — the inverse of `from_ui`'s frac, and
        // invariant to tau_band so it round-trips at any strictness.
        let headroom = Self::TAU_BLOCK_MAX - self.tau_band;
        let frac = if headroom > 0.0 {
            (self.tau_block - self.tau_band) / headroom
        } else {
            Self::BAND_FRAC_MEDIUM
        };
        let bands = [
            ("narrow", Self::BAND_FRAC_NARROW),
            ("medium", Self::BAND_FRAC_MEDIUM),
            ("wide", Self::BAND_FRAC_WIDE),
        ];
        let band = bands
            .iter()
            .min_by(|a, b| {
                (frac - a.1)
                    .abs()
                    .partial_cmp(&(frac - b.1).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(name, _)| *name)
            .unwrap_or("medium");
        (strictness, band)
    }

    /// Clamp to the range the UI can actually produce. A value **outside** the
    /// reachable range (a corrupt or hand-edited `classifier_settings` row) is
    /// treated as garbage and reset to the default — so a bad row can never make
    /// the filter looser than the *most permissive setting a user could
    /// deliberately choose* (strictness 0, `tau_band = 0.5`). NaN/∞ → default.
    /// This is the single validation choke point; `from_ui` and the storage
    /// read/write paths all call it.
    ///
    /// (Note: within the reachable range, a low strictness IS legitimately more
    /// permissive than the default — that's a user choice, not a leak. The
    /// invariant this enforces is "never looser than strictness 0," not "never
    /// looser than the default.")
    pub fn sanitized(self) -> Self {
        let d = Self::default();
        let in_range = |v: f32, lo: f32, hi: f32, fallback: f32| {
            if v.is_finite() && v >= lo && v <= hi {
                v
            } else {
                fallback
            }
        };
        let tau_band = in_range(self.tau_band, Self::TAU_BAND_MIN, Self::TAU_BAND_MAX, d.tau_band);
        // tau_block must sit in [tau_band, TAU_BLOCK_MAX]; out of range → default,
        // then floored at tau_band so the ordering invariant always holds.
        let tau_block =
            in_range(self.tau_block, tau_band, Self::TAU_BLOCK_MAX, d.tau_block).max(tau_band);
        Self {
            tau_block,
            tau_band,
        }
    }
}

/// Anything that can decide whether a message is private, public, or uncertain.
///
/// Implementors: [`HeuristicClassifier`] (regex/keyword, ships now) and
/// [`EnsembleClassifier`] (trained model, ships when the model is delivered).
pub trait Classifier: Send + Sync {
    /// Classify with default thresholds. Kept as the primary method so existing
    /// callers and the rules-only classifiers are unaffected.
    fn classify(&self, text: &str) -> Classification;

    /// Classify with per-profile fusion thresholds. The default ignores `cfg`
    /// (the rules layer and the coarse-label classifiers don't use thresholds);
    /// only [`EnsembleClassifier`] overrides this to grade its model score
    /// against the profile's `tau_block` / `tau_band`.
    fn classify_with(&self, text: &str, _cfg: &ClassifierConfig) -> Classification {
        self.classify(text)
    }
}

#[cfg(test)]
mod config_tests {
    use super::ClassifierConfig;

    #[test]
    fn default_matches_historical_constants() {
        let d = ClassifierConfig::default();
        assert_eq!(d.tau_block, 0.5);
        assert_eq!(d.tau_band, 0.05);
    }

    #[test]
    fn strictness_drives_tau_band_the_egress_threshold() {
        // The egress-relevant threshold is tau_band. Higher strictness ⇒ LOWER
        // tau_band ⇒ more content is non-Public ⇒ more kept local.
        let permissive = ClassifierConfig::from_ui(0, "medium");
        let default_ish = ClassifierConfig::from_ui(50, "medium");
        let paranoid = ClassifierConfig::from_ui(100, "medium");
        assert!(
            permissive.tau_band > default_ish.tau_band && default_ish.tau_band > paranoid.tau_band,
            "higher strictness must LOWER tau_band (more paranoid)"
        );
        // The log map puts the historical default 0.05 at the slider midpoint.
        assert!((default_ish.tau_band - 0.05).abs() < 1e-4, "s=50 ⇒ tau_band≈0.05");
        assert!((permissive.tau_band - 0.5).abs() < 1e-4, "s=0 ⇒ tau_band≈0.5");
        assert!((paranoid.tau_band - 0.005).abs() < 1e-4, "s=100 ⇒ tau_band≈0.005");
    }

    #[test]
    fn band_drives_tau_block_not_tau_band() {
        // The band changes the Uncertain-zone width (tau_block), NOT the egress
        // line (tau_band) — which stays fixed for a given strictness.
        let narrow = ClassifierConfig::from_ui(50, "narrow");
        let medium = ClassifierConfig::from_ui(50, "medium");
        let wide = ClassifierConfig::from_ui(50, "wide");
        assert_eq!(narrow.tau_band, medium.tau_band, "band must not move the egress line");
        assert_eq!(wide.tau_band, medium.tau_band);
        assert!(
            narrow.tau_block < medium.tau_block && medium.tau_block < wide.tau_block,
            "wider band ⇒ higher tau_block (bigger Uncertain zone)"
        );
        // Default (medium) reproduces the historical tau_block 0.5.
        assert!((medium.tau_block - 0.5).abs() < 1e-4);
        // Unknown band falls back to medium.
        assert_eq!(ClassifierConfig::from_ui(50, "bogus").tau_block, medium.tau_block);
    }

    #[test]
    fn ui_round_trips_for_values_this_code_writes() {
        for s in [0u8, 25, 50, 72, 100] {
            for band in ["narrow", "medium", "wide"] {
                let cfg = ClassifierConfig::from_ui(s, band);
                let (rs, rb) = cfg.to_ui();
                assert_eq!(rs, s, "strictness round-trip for {s}/{band}");
                assert_eq!(rb, band, "band round-trip for {s}/{band}");
            }
        }
    }

    #[test]
    fn sanitize_rejects_out_of_range_and_never_looser_than_strictness_zero() {
        // The loosest a user could deliberately choose (strictness 0).
        let loosest = ClassifierConfig::from_ui(0, "wide");

        // NaN / negative → default (garbage, not a valid choice).
        assert_eq!(
            ClassifierConfig { tau_block: f32::NAN, tau_band: -1.0 }.sanitized(),
            ClassifierConfig::default()
        );

        // The review's leak repro: a hand-edited row with both thresholds ~1.0
        // (legal floats, order satisfied) used to slip through and make the
        // filter far leakier than any legit setting. It must now reset to
        // default, NOT pass through.
        let repro = ClassifierConfig { tau_block: 0.999, tau_band: 0.999 }.sanitized();
        assert_eq!(repro, ClassifierConfig::default(), "leaky corrupt row must reset to default");

        // The 0.4-band repro likewise: tau_band 0.4 is within the reachable
        // range (≈ strictness 9), so it stays; tau_block 2.0 is garbage → default,
        // floored at tau_band. The result is never looser than strictness 0.
        let partial = ClassifierConfig { tau_block: 2.0, tau_band: 0.4 }.sanitized();
        assert!(partial.tau_band <= loosest.tau_band, "tau_band never looser than strictness 0");
        assert!(partial.tau_block >= partial.tau_band, "ordering invariant holds");

        // Property: NO sanitized output is looser (higher tau_band) than the
        // loosest legitimate config, for a spread of garbage inputs.
        for (tb, tba) in [(2.0, 5.0), (0.999, 0.999), (-1.0, 0.7), (0.3, 0.8), (f32::INFINITY, 0.6)] {
            let s = ClassifierConfig { tau_block: tb, tau_band: tba }.sanitized();
            assert!(
                s.tau_band <= loosest.tau_band + 1e-6,
                "sanitized {tb},{tba} → tau_band {} exceeds loosest legit {}",
                s.tau_band, loosest.tau_band
            );
            assert!(s.tau_band > 0.0 && s.tau_block >= s.tau_band && s.tau_block <= 0.95);
        }
    }
}
