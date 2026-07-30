//! Heuristic PII / sensitivity classifier.
//!
//! Direct port of the Electron app's `src/privacy/classifier.js` so the two
//! trees agree on what "Auto" binding should treat as private until the real
//! trained classifier is delivered.
//!
//! Design goals (from the original file):
//!  - Pure and synchronous — safe to call on every send.
//!  - Conservative — prefer a few solid, low-false-positive detectors over
//!    broad pattern matching that would flag everyday text.
//!
//! Each detector returns `Option<&'static str>` — a human-readable reason if
//! it matched, or `None`. `classify` runs them in order, caps the reason list
//! at 5, and maps the result to a [`Classification`].

use std::sync::OnceLock;

use regex::Regex;

use super::{Classification, Classifier, Label};

/// Max reasons attached to a single classification. Mirrors the Electron app.
const MAX_REASONS: usize = 5;

/// Confidence assigned to a "hard" regex match (SSN, Luhn-valid card, etc.).
const HARD_CONFIDENCE: f32 = 1.0;
/// Confidence assigned to a "soft" heuristic match (health terms, addresses…).
const SOFT_CONFIDENCE: f32 = 0.8;

/// A single detector. Returns a human-readable reason if it matched, else `None`.
type Detector = fn(&str) -> Option<&'static str>;

/// Heuristic privacy classifier. Cheap enough to run inline on every send.
#[derive(Debug, Default, Clone, Copy)]
pub struct HeuristicClassifier;

impl HeuristicClassifier {
    pub fn new() -> Self {
        Self
    }
}

impl Classifier for HeuristicClassifier {
    fn classify(&self, text: &str) -> Classification {
        let empty = text.is_empty();
        let detectors: [Detector; 8] = [
            detect_ssn,
            detect_credit_card,
            detect_api_key_secret,
            detect_email_password,
            detect_phone_number,
            detect_health_info,
            detect_financial_account,
            detect_home_address,
        ];

        let mut reasons: Vec<&'static str> = Vec::new();
        for detect in detectors {
            if let Some(reason) = detect(text) {
                reasons.push(reason);
                if reasons.len() >= MAX_REASONS {
                    break;
                }
            }
        }

        // Empty text → trivially public, no signal.
        if empty {
            return Classification {
                label: Label::Public,
                confidence: 1.0,
                raw_output: vec![0.0; 8],
                spans: Vec::new(),
            };
        }

        // raw_output: 1.0 per matched detector, in detector order. Useful for
        // debugging which rule fired; the gate only consults `label`.
        let mut raw_output = vec![0.0_f32; 8];
        for (i, detect) in detectors.iter().enumerate() {
            if detect(text).is_some() {
                raw_output[i] = 1.0;
            }
        }

        // Map the result. The Electron classifier is binary (sensitive / not);
        // we have a tri-state, so we use Uncertain for soft matches that
        // require *both* halves of a conjunctive rule to fire (e.g. phone
        // number with first-person context, health term with first-person
        // context). Hard matches — SSN, Luhn-valid card, private-key block,
        // account number, home address, raw API key — are Private.
        if reasons.is_empty() {
            return Classification {
                label: Label::Public,
                confidence: 1.0,
                raw_output,
                spans: Vec::new(),
            };
        }

        // Hard rules fire Private at 1.0. Conjunctive (soft) rules fire
        // Uncertain at 0.8 — the gate routes Uncertain to local anyway, so
        // the safety outcome is identical, but the audit log can tell the
        // difference between a definite PII hit and a "looks like it might
        // be" hit.
        let is_hard = raw_output[0] == 1.0
            || raw_output[1] == 1.0
            || raw_output[2] == 1.0
            || raw_output[6] == 1.0
            || raw_output[7] == 1.0;

        if is_hard {
            Classification {
                label: Label::Private,
                confidence: HARD_CONFIDENCE,
                raw_output,
                spans: Vec::new(),
            }
        } else {
            Classification {
                label: Label::Uncertain,
                confidence: SOFT_CONFIDENCE,
                raw_output,
                spans: Vec::new(),
            }
        }
    }
}

// --- Individual detectors ----------------------------------------------------
// Each detector returns a human-readable reason string, or None if it didn't
// match. Keeping them as small pure functions makes them independently
// testable and easy to extend once the trained classifier lands.

/// SSN: 3-2-4 with dashes.
fn detect_ssn(text: &str) -> Option<&'static str> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").unwrap());
    if re.is_match(text) {
        Some("Looks like a Social Security number")
    } else {
        None
    }
}

/// 13-19 digit sequence, optionally grouped with spaces or dashes; must
/// pass a Luhn checksum. Standard mod-10.
fn detect_credit_card(text: &str) -> Option<&'static str> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\b(?:\d[ -]?){12,18}\d\b").unwrap());
    for mat in re.find_iter(text) {
        let digits: String = mat
            .as_str()
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect();
        if (13..=19).contains(&digits.len()) && luhn_check(&digits) {
            return Some("Contains a number that passes a credit-card checksum");
        }
    }
    None
}

/// Luhn checksum — standard mod-10 algorithm used to validate card numbers.
fn luhn_check(digits: &str) -> bool {
    let mut sum: u32 = 0;
    let mut alternate = false;
    for c in digits.chars().rev() {
        let mut n = c.to_digit(10).unwrap_or(0);
        if alternate {
            n *= 2;
            if n > 9 {
                n -= 9;
            }
        }
        sum += n;
        alternate = !alternate;
    }
    sum % 10 == 0
}

/// API keys, tokens, secrets.
fn detect_api_key_secret(text: &str) -> Option<&'static str> {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        vec![
            Regex::new(r"\bsk-[A-Za-z0-9]{16,}\b").unwrap(),
            Regex::new(r"\bAKIA[0-9A-Z]{16}\b").unwrap(),
            Regex::new(r"\bghp_[A-Za-z0-9]{20,}\b").unwrap(),
            Regex::new(r"-----BEGIN [^-]*PRIVATE KEY-----").unwrap(),
            // Case-insensitive: (?i) — `secret=xxx`, `api_key: xxx`, etc.
            Regex::new(r"(?i)(?:api[_-]?key|secret|token|password)\s*[:=]\s*\S{8,}").unwrap(),
        ]
    });
    if patterns.iter().any(|re| re.is_match(text)) {
        Some("Looks like an API key, token, or secret")
    } else {
        None
    }
}

/// Email + password on the same message — the combo is what makes it risky.
fn detect_email_password(text: &str) -> Option<&'static str> {
    static EMAIL: OnceLock<Regex> = OnceLock::new();
    static PASSWORD: OnceLock<Regex> = OnceLock::new();
    let email_re = EMAIL.get_or_init(|| Regex::new(r"\b[\w.+-]+@[\w-]+\.[\w.-]+\b").unwrap());
    let password_re = PASSWORD.get_or_init(|| Regex::new(r"(?i)\bpassword\b").unwrap());
    if email_re.is_match(text) && password_re.is_match(text) {
        Some("Mentions an email address alongside a password")
    } else {
        None
    }
}

/// Phone-like number, but only if first-person context is on the same message.
fn detect_phone_number(text: &str) -> Option<&'static str> {
    static PHONE: OnceLock<Regex> = OnceLock::new();
    static CTX: OnceLock<Regex> = OnceLock::new();
    let phone_re = PHONE.get_or_init(|| Regex::new(r"\b(?:\+?\d[\d -]{7,}\d)\b").unwrap());
    let ctx_re = CTX.get_or_init(|| {
        Regex::new(r"(?i)\b(my number|call me|text me|reach me at|my phone)\b").unwrap()
    });
    if phone_re.is_match(text) && ctx_re.is_match(text) {
        Some("Looks like a personal phone number")
    } else {
        None
    }
}

/// Health term + first-person context on the same message.
///
/// `pub(crate)`: reused directly by [`super::rules::RulesClassifier`] as a
/// stopgap for the HEALTH category, which the deterministic rules layer
/// (`rules.py`/`rules.rs`) intentionally doesn't attempt — see that module's
/// docs. Drop this back to private once the ONNX ensemble covers HEALTH.
pub(crate) fn detect_health_info(text: &str) -> Option<&'static str> {
    static HEALTH: OnceLock<Regex> = OnceLock::new();
    static FIRST: OnceLock<Regex> = OnceLock::new();
    let health_re = HEALTH.get_or_init(|| {
        Regex::new(r"(?i)\b(diagnos\w*|prescri\w*|medication|therapy|symptom\w*)\b").unwrap()
    });
    let first_re = FIRST.get_or_init(|| Regex::new(r"(?i)\b(my|i've|i had|i was|i'm)\b").unwrap());
    if health_re.is_match(text) && first_re.is_match(text) {
        Some("Mentions personal health information")
    } else {
        None
    }
}

/// "Account number" / "account #" followed by 6+ digits.
fn detect_financial_account(text: &str) -> Option<&'static str> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re =
        RE.get_or_init(|| Regex::new(r"(?i)\baccount\s*(?:#|number)\s*:?\s*\d{6,}\b").unwrap());
    if re.is_match(text) {
        Some("Contains a financial account number")
    } else {
        None
    }
}

/// "<number> <word> <street|st|ave|...>" — coarse, but rare false positives.
///
/// `pub(crate)`: reused directly by [`super::rules::RulesClassifier`] as a
/// stopgap for the LOCATION category — see [`detect_health_info`]'s doc
/// comment for why.
pub(crate) fn detect_home_address(text: &str) -> Option<&'static str> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)\b\d+\s+\w+\s+(street|st|ave|avenue|road|rd|drive|dr|lane|ln|blvd)\b")
            .unwrap()
    });
    if re.is_match(text) {
        Some("Looks like a home address")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(t: &str) -> Classification {
        HeuristicClassifier.classify(t)
    }

    #[test]
    fn empty_is_public() {
        let c = classify("");
        assert_eq!(c.label, Label::Public);
        assert_eq!(c.confidence, 1.0);
    }

    #[test]
    fn ssn_is_private() {
        let c = classify("My SSN is 123-45-6789");
        assert_eq!(c.label, Label::Private);
        assert_eq!(c.confidence, 1.0);
    }

    #[test]
    fn luhn_valid_card_is_private() {
        // 4111111111111111 is the standard Luhn-valid test number.
        let c = classify("card: 4111 1111 1111 1111");
        assert_eq!(c.label, Label::Private);
    }

    #[test]
    fn luhn_invalid_13_digit_run_is_public() {
        let c = classify("order # 1234567890123");
        assert_eq!(c.label, Label::Public);
    }

    #[test]
    fn openai_key_is_private() {
        let c = classify("use sk-abcdefghijklmnopqrstuvwxyz123456 for the test");
        assert_eq!(c.label, Label::Private);
    }

    #[test]
    fn aws_access_key_is_private() {
        let c = classify("AKIAIOSFODNN7EXAMPLE");
        assert_eq!(c.label, Label::Private);
    }

    #[test]
    fn private_key_block_is_private() {
        let c = classify("-----BEGIN RSA PRIVATE KEY-----");
        assert_eq!(c.label, Label::Private);
    }

    #[test]
    fn email_plus_password_is_uncertain() {
        let c = classify("login: alice@example.com password: hunter2");
        assert_eq!(c.label, Label::Uncertain);
    }

    #[test]
    fn phone_with_context_is_uncertain() {
        let c = classify("call me at 555-123-4567");
        assert_eq!(c.label, Label::Uncertain);
    }

    #[test]
    fn phone_without_context_is_public() {
        let c = classify("the support line is 555-123-4567");
        assert_eq!(c.label, Label::Public);
    }

    #[test]
    fn health_with_first_person_is_uncertain() {
        let c = classify("I was diagnosed with the flu last week");
        assert_eq!(c.label, Label::Uncertain);
    }

    #[test]
    fn home_address_is_private() {
        let c = classify("ship to 123 Maple Street");
        assert_eq!(c.label, Label::Private);
    }

    #[test]
    fn financial_account_is_private() {
        let c = classify("account number: 987654321");
        assert_eq!(c.label, Label::Private);
    }

    #[test]
    fn clean_text_is_public() {
        let c = classify("what's the capital of france?");
        assert_eq!(c.label, Label::Public);
    }

    #[test]
    fn raw_output_has_one_entry_per_detector() {
        let c = classify("My SSN is 123-45-6789");
        assert_eq!(c.raw_output.len(), 8);
    }
}
