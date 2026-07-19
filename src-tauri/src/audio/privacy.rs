//! Wave 5.2 / M6 — the **audio-egress gate**: the NEW privacy invariant voice
//! introduces (PLAN §6 "the audio-specific privacy check"). Cloud STT/TTS is a
//! NEW egress boundary the text privacy gate never saw — the model's reply was
//! cleared for the *model* endpoint, not for a *cloud speech* service. This gate
//! re-vets speech egress at that boundary, REUSING `agent::gate::PrivacyGate`
//! (the same primitive the tool chain's `PrivacyFilterHook` reuses) — a new
//! boundary, not a forked classifier.
//!
//! **Structural invariant — audio egress ≤ text egress.** TTS is checked against
//! the CUMULATIVE reply prefix (a superset of every streamed chunk); the caller
//! LATCHES (once withheld, the rest of the turn stays local). The final prefix
//! equals the whole reply — exactly what the text gate would see — so a
//! `Private`-bound conversation is *exactly as local spoken as typed*, and
//! nothing reaches a cloud voice service that wouldn't reach a cloud model.
//!
//! Local STT/TTS (the default) is a no-op `Allow` — the offline case pays
//! nothing. This module is PURE POLICY (no audio I/O), so it's fully unit-tested;
//! the native STT/TTS backends that consume its verdict are the on-hardware work.

use std::sync::Arc;

use crate::agent::gate::{Binding, GateDecision, PrivacyGate};
use crate::classifier::{Classifier, ClassifierConfig};

/// Whether a piece of speech (a TTS reply prefix, or an STT transcript) may be
/// sent to a CLOUD speech service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioEgressDecision {
    /// Cleared for a cloud STT/TTS service.
    Allow,
    /// Must NOT go to the cloud — route to a LOCAL engine (or, if none, drop the
    /// audio channel for this turn). The caller emits a non-silent
    /// `voice:privacy_withheld` event (mirrors `stream:local_reroute`).
    Withhold,
}

/// Re-vets speech egress at the cloud STT/TTS boundary. Wraps a `PrivacyGate`
/// (same classifier as the §7 text gate).
pub struct AudioEgressGate {
    gate: PrivacyGate,
}

impl AudioEgressGate {
    pub fn new(classifier: Arc<dyn Classifier>) -> Self {
        Self { gate: PrivacyGate::new(classifier) }
    }

    /// Decide whether the reply text SO FAR (`cumulative_prefix` — a superset of
    /// every chunk streamed to TTS this turn) may be spoken by a CLOUD TTS.
    /// `is_cloud_tts == false` (local TTS, the default) is always `Allow`. The
    /// caller must LATCH: once this returns `Withhold`, keep the rest of the turn
    /// local (the prefix only grows, so re-checking each chunk is monotone).
    pub fn tts_egress(
        &self,
        cumulative_prefix: &str,
        binding: &Binding,
        is_cloud_tts: bool,
        cfg: &ClassifierConfig,
    ) -> AudioEgressDecision {
        if !is_cloud_tts {
            return AudioEgressDecision::Allow; // local TTS never crosses an egress boundary
        }
        match self.gate.check_detailed(binding, cumulative_prefix, true, cfg).0 {
            GateDecision::Allow => {
                // `Public` binding bypasses the tunable classifier in the text
                // gate (the user chose cloud for TEXT). The un-tunable FLOOR
                // still applies at the AUDIO boundary: if it flags structured
                // secrets/PII, withhold (the design raises one confirm; the
                // conservative primitive answer is Withhold-pending-confirm).
                if *binding == Binding::Public
                    && !crate::classifier::rules::detect(cumulative_prefix).is_empty()
                {
                    AudioEgressDecision::Withhold
                } else {
                    AudioEgressDecision::Allow
                }
            }
            // Block / RouteLocal — the same verdict the text gate reached; a
            // cloud voice service must not see it either.
            _ => AudioEgressDecision::Withhold,
        }
    }

    /// The same boundary for raw microphone audio bound for a CLOUD STT: the only
    /// pre-cloud-STT text we can gate is the transcript, under the identical
    /// policy. (Local STT is the default → `Allow`.)
    pub fn stt_egress(
        &self,
        transcript: &str,
        binding: &Binding,
        is_cloud_stt: bool,
        cfg: &ClassifierConfig,
    ) -> AudioEgressDecision {
        self.tts_egress(transcript, binding, is_cloud_stt, cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classifier::RulesClassifier;

    fn gate() -> AudioEgressGate {
        AudioEgressGate::new(Arc::new(RulesClassifier::new()))
    }
    fn cfg() -> ClassifierConfig {
        ClassifierConfig::default()
    }

    #[test]
    fn local_tts_never_egresses() {
        // Even blatantly sensitive text is fine for a LOCAL engine.
        assert_eq!(
            gate().tts_egress("my SSN is 123-45-6789", &Binding::Auto, false, &cfg()),
            AudioEgressDecision::Allow
        );
    }

    #[test]
    fn cloud_tts_withholds_sensitive_and_allows_benign() {
        let g = gate();
        // Benign reply → cloud TTS ok.
        assert_eq!(
            g.tts_egress("The weather looks clear today.", &Binding::Auto, true, &cfg()),
            AudioEgressDecision::Allow
        );
        // Sensitive reply (structured PII the floor catches) → withheld from cloud.
        assert_eq!(
            g.tts_egress("your account number is 4111 1111 1111 1111", &Binding::Auto, true, &cfg()),
            AudioEgressDecision::Withhold
        );
    }

    #[test]
    fn private_binding_is_as_local_spoken_as_typed() {
        // A Private conversation withholds from cloud TTS regardless of content
        // (same as the text gate routes it local).
        assert_eq!(
            gate().tts_egress("hello there", &Binding::Private, true, &cfg()),
            AudioEgressDecision::Withhold
        );
    }

    #[test]
    fn public_binding_still_applies_the_floor_to_audio() {
        let g = gate();
        // Public → the text classifier is bypassed, but the un-tunable floor
        // still withholds a structured secret from a CLOUD voice service.
        assert_eq!(
            g.tts_egress("the api key is sk-live-abcdef0123456789abcdef", &Binding::Public, true, &cfg()),
            AudioEgressDecision::Withhold
        );
        // A benign Public reply is allowed to cloud TTS.
        assert_eq!(
            g.tts_egress("sounds good, see you then", &Binding::Public, true, &cfg()),
            AudioEgressDecision::Allow
        );
    }

    #[test]
    fn cumulative_prefix_is_monotone_withhold_latches_conceptually() {
        // As the reply grows and crosses into sensitive territory, the verdict
        // flips to Withhold — and the final prefix (the whole reply) is what the
        // text gate would see, so audio egress ≤ text egress.
        let g = gate();
        assert_eq!(g.tts_egress("Here is the info:", &Binding::Auto, true, &cfg()), AudioEgressDecision::Allow);
        assert_eq!(
            g.tts_egress("Here is the info: SSN 123-45-6789", &Binding::Auto, true, &cfg()),
            AudioEgressDecision::Withhold,
            "once the growing prefix hits sensitive content, the caller latches local"
        );
    }
}
