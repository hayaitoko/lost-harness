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

/// Whether a piece of speech (a TTS reply prefix, or raw STT audio) may be sent
/// to a CLOUD speech service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioEgressDecision {
    /// Cleared for a cloud STT/TTS service.
    Allow,
    /// Must NOT go to the cloud — route to a LOCAL engine (or, if none, drop the
    /// audio channel for this turn). The caller emits a non-silent
    /// `voice:privacy_withheld` event (mirrors `stream:local_reroute`).
    Withhold,
    /// B9: a `Public`-bound TTS reply hit the un-tunable floor. The design
    /// mandates ONE confirm via the approval spine before withholding, not an
    /// automatic block. Carries the [`ActionFingerprint`](crate::hooks::approval::ActionFingerprint)
    /// pinning THIS exact `cumulative_prefix` — the caller asks an
    /// `ApprovalPrompter` under `RiskClass::External` and, on approve, records a
    /// `Once`+this-fingerprint grant; [`AudioEgressGate::resolve_confirm`]
    /// re-resolves it to `Allow` only via `ApprovalLedger::covers_once` (a
    /// standing Session/Tool grant can never satisfy it).
    ConfirmRequired(String),
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
                // secrets/PII, the design raises ONE confirm via the approval
                // spine (B9) rather than silently blocking — a spoken secret is
                // a deliberate, human-visible act, not an automatic egress.
                if *binding == Binding::Public
                    && !crate::classifier::rules::detect(cumulative_prefix).is_empty()
                {
                    let fp = crate::hooks::approval::ActionFingerprint::of(
                        "tts_cloud_egress",
                        &serde_json::json!({ "text": cumulative_prefix }),
                    );
                    AudioEgressDecision::ConfirmRequired(fp)
                } else {
                    AudioEgressDecision::Allow
                }
            }
            // Block / RouteLocal — the same verdict the text gate reached; a
            // cloud voice service must not see it either.
            _ => AudioEgressDecision::Withhold,
        }
    }

    /// B9: the pre-cloud-STT decision — **content-free**. Unlike TTS (where we
    /// gate the model's own reply TEXT), raw microphone audio CANNOT be
    /// classified before it's transcribed, so the earlier "classify the
    /// transcript" delegation was wrong (there is no transcript yet at the point
    /// this decision must be made). The decision is BINDING-based: cloud STT is
    /// permitted only when the user explicitly chose cloud for this conversation
    /// (`Public`); under `Auto`/`Private` the raw audio must be transcribed by a
    /// LOCAL engine (`Withhold`) — we never ship unvetted audio to a cloud
    /// service on a guess about its contents. Local STT (the default) is `Allow`.
    pub fn stt_egress(&self, binding: &Binding, is_cloud_stt: bool) -> AudioEgressDecision {
        if !is_cloud_stt {
            return AudioEgressDecision::Allow; // local STT never crosses an egress boundary
        }
        match binding {
            // The user explicitly opted this conversation into cloud.
            Binding::Public => AudioEgressDecision::Allow,
            // Content-unknown (pre-transcription) → keep it on a local engine.
            Binding::Auto | Binding::Private => AudioEgressDecision::Withhold,
        }
    }

    /// B9: re-resolve a [`AudioEgressDecision::ConfirmRequired`] AFTER the caller
    /// has run the one-confirm round-trip (asked an `ApprovalPrompter` under
    /// `RiskClass::External` and, on approve, recorded
    /// `ledger.grant(GrantTarget::Fingerprint(fp), GrantScope::Once)`). Returns
    /// `Allow` iff the ledger holds a fresh `Once`+this-fingerprint grant
    /// ([`ApprovalLedger::covers_once`]) — a standing `Session`/`Tool` grant can
    /// never satisfy this floor. `Allow`/`Withhold` pass through unchanged.
    pub fn resolve_confirm(
        decision: AudioEgressDecision,
        ledger: &crate::hooks::approval::ApprovalLedger,
    ) -> AudioEgressDecision {
        match decision {
            AudioEgressDecision::ConfirmRequired(fp) => {
                // `take_once` CONSUMES the grant atomically — a confirm authorizes
                // EXACTLY one cloud-TTS egress of this exact text. A second
                // resolve of the same fingerprint (a retry, a re-render) finds no
                // grant and re-withholds until the user confirms again (review
                // finding: the old `covers_once` check left the grant standing).
                if ledger.take_once(&fp) {
                    AudioEgressDecision::Allow
                } else {
                    AudioEgressDecision::Withhold
                }
            }
            other => other,
        }
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
    fn public_binding_raises_one_confirm_for_a_floor_hit_on_audio() {
        let g = gate();
        // B9: Public bypasses the tunable text classifier, but the un-tunable
        // floor on a structured secret at the CLOUD voice boundary raises ONE
        // confirm (carrying a fingerprint of this exact text), not a silent block.
        match g.tts_egress("the api key is sk-live-abcdef0123456789abcdef", &Binding::Public, true, &cfg()) {
            AudioEgressDecision::ConfirmRequired(fp) => assert_eq!(fp.len(), 64, "a sha256 fingerprint"),
            other => panic!("expected ConfirmRequired, got {other:?}"),
        }
        // A benign Public reply is allowed to cloud TTS with no confirm.
        assert_eq!(
            g.tts_egress("sounds good, see you then", &Binding::Public, true, &cfg()),
            AudioEgressDecision::Allow
        );
    }

    #[test]
    fn stt_egress_is_content_free_and_binding_based() {
        // B9: the pre-transcription STT decision can't depend on content (there
        // is no transcript yet) — it's binding-based.
        let g = gate();
        // Local STT never egresses, whatever the binding.
        assert_eq!(g.stt_egress(&Binding::Auto, false), AudioEgressDecision::Allow);
        assert_eq!(g.stt_egress(&Binding::Private, false), AudioEgressDecision::Allow);
        // Cloud STT: only Public (explicit opt-in); Auto/Private keep it local.
        assert_eq!(g.stt_egress(&Binding::Public, true), AudioEgressDecision::Allow);
        assert_eq!(g.stt_egress(&Binding::Auto, true), AudioEgressDecision::Withhold);
        assert_eq!(g.stt_egress(&Binding::Private, true), AudioEgressDecision::Withhold);
    }

    #[test]
    fn one_confirm_round_trip_flips_confirm_required_to_allow() {
        use crate::hooks::approval::{ApprovalLedger, GrantScope, GrantTarget};
        let g = gate();
        let secret = "the api key is sk-live-abcdef0123456789abcdef";
        let decision = g.tts_egress(secret, &Binding::Public, true, &cfg());
        let fp = match &decision {
            AudioEgressDecision::ConfirmRequired(fp) => fp.clone(),
            other => panic!("expected ConfirmRequired, got {other:?}"),
        };

        let ledger = ApprovalLedger::new();
        // No grant yet → the confirm is unresolved → Withhold.
        assert_eq!(
            AudioEgressGate::resolve_confirm(decision.clone(), &ledger),
            AudioEgressDecision::Withhold
        );
        // The user approves ONE confirm for this exact text.
        ledger.grant(GrantTarget::Fingerprint(fp.clone()), GrantScope::Once);
        assert_eq!(AudioEgressGate::resolve_confirm(decision.clone(), &ledger), AudioEgressDecision::Allow);
        // ...and it's SINGLE-USE: the grant is consumed, so a second egress of
        // the identical text re-withholds until the user confirms again.
        assert_eq!(
            AudioEgressGate::resolve_confirm(decision, &ledger),
            AudioEgressDecision::Withhold,
            "a Once confirm authorizes exactly one cloud-TTS egress"
        );

        // A Session/Tool grant can NEVER satisfy this floor (covers_once semantics).
        let ledger2 = ApprovalLedger::new();
        ledger2.grant(GrantTarget::Fingerprint(fp.clone()), GrantScope::Session);
        let d2 = g.tts_egress(secret, &Binding::Public, true, &cfg());
        assert_eq!(AudioEgressGate::resolve_confirm(d2, &ledger2), AudioEgressDecision::Withhold);

        // The fingerprint pins THIS text — a different reply yields a different fp.
        if let AudioEgressDecision::ConfirmRequired(fp2) =
            g.tts_egress("api key is sk-live-9999999999999999999999", &Binding::Public, true, &cfg())
        {
            assert_ne!(fp, fp2, "the confirm fingerprint pins the exact reply text");
        }
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
