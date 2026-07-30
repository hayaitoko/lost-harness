//! Span redaction for partial delegation (PLAN §11).
//!
//! When a message is private only because of concrete PII *values* (an email,
//! an SSN, a card number, a credential), we can black those spans out and send
//! the safe remainder to the cloud — then rehydrate the model's reply locally.
//! The sensitive bytes never leave the device; the model only ever sees
//! `[REDACTED:…]` placeholders.
//!
//! **Safety boundary — only concrete VALUES are redacted.** Categories like
//! `PROPRIETARY` are matched by *cue words* ("confidential", "under NDA"), not
//! by the sensitive content itself. Redacting the cue would strip the very
//! signal that made the message private while leaving the proprietary content
//! in place — the opposite of protection. So [`is_redactable_value`] excludes
//! them, and the caller re-runs the FULL classifier on the redacted text: any
//! remaining privacy signal (a proprietary cue, a model-detected semantic
//! disclosure, or a PII value the redaction happened to miss) keeps the message
//! non-Public and forces it local. Redaction can only ever *propose* a safe
//! remainder; the re-classify pass is what proves it.
//!
//! Byte offsets (`Span::start_byte`/`end_byte`) are used for slicing — they are
//! the only offsets safe to index a Rust `&str` with.

use super::rules::{RuleCategory, Span};

/// Whether a category names a concrete sensitive *value* (safe to black out)
/// rather than a contextual *cue* (redacting which would remove the signal, not
/// the secret). Only value categories are redacted for partial delegation.
pub fn is_redactable_value(cat: RuleCategory) -> bool {
    matches!(
        cat,
        RuleCategory::PiiContact
            | RuleCategory::PiiId
            | RuleCategory::Financial
            | RuleCategory::Credential
    )
}

/// One black-out: the placeholder that went to the model, and the original text
/// it stands for (kept on-device for rehydrating the reply).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replacement {
    pub placeholder: String,
    pub original: String,
}

/// The result of redacting a message: the text safe to send, plus the map back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redaction {
    /// The message with every redactable-value span replaced by a placeholder.
    pub redacted_text: String,
    /// Placeholder → original, in redaction order. Empty ⇒ nothing was redacted
    /// (the caller should NOT treat the message as delegatable).
    pub replacements: Vec<Replacement>,
}

impl Redaction {
    /// True when at least one span was redacted.
    pub fn is_redacted(&self) -> bool {
        !self.replacements.is_empty()
    }
}

/// Black out every redactable-value span in `text`, replacing each with an
/// unforgeable placeholder. Non-value categories (see [`is_redactable_value`])
/// are ignored.
///
/// **Overlapping spans are UNIONED, never dropped.** The classifier can emit
/// overlapping matches whose ranges only partially coincide (an `idish` span
/// abutting a `token`, an `email` vs its obfuscated variant). An earlier
/// "skip the later span" approach leaked the *tail* of a staggered overlap
/// (bytes past the first span's end were never redacted) — a real cloud-egress
/// leak. This merges every cluster of overlapping value-spans into one
/// contiguous interval and blacks out the whole union, so no sensitive byte can
/// slip between two overlapping matches.
///
/// `nonce` (a per-turn random token from the caller) is embedded in every
/// placeholder so untrusted tool/web content pulled into the conversation can't
/// forge a `[REDACTED:…]` string that [`rehydrate`] would blindly expand into
/// the user's real value.
pub fn redact(text: &str, spans: &[Span], nonce: &str) -> Redaction {
    // Value spans only, on valid char boundaries, sorted by start.
    let mut chosen: Vec<&Span> = spans
        .iter()
        .filter(|s| is_redactable_value(s.category) && s.end_byte > s.start_byte)
        .filter(|s| {
            s.end_byte <= text.len()
                && text.is_char_boundary(s.start_byte)
                && text.is_char_boundary(s.end_byte)
        })
        .collect();
    chosen.sort_by_key(|s| s.start_byte);

    // Merge overlapping spans into disjoint intervals covering the UNION. Two
    // spans overlap when the later one starts strictly before the running end;
    // adjacent (end == start) spans stay separate. A merged interval keeps the
    // category of the span that opened it (a display label only — every byte in
    // the interval is redacted regardless).
    let mut intervals: Vec<(usize, usize, RuleCategory)> = Vec::new();
    for s in chosen {
        match intervals.last_mut() {
            Some(last) if s.start_byte < last.1 => {
                last.1 = last.1.max(s.end_byte); // extend the union
            }
            _ => intervals.push((s.start_byte, s.end_byte, s.category)),
        }
    }

    let mut out = String::with_capacity(text.len());
    let mut replacements: Vec<Replacement> = Vec::new();
    let mut cursor = 0usize;
    for (i, (start, end, category)) in intervals.iter().enumerate() {
        out.push_str(&text[cursor..*start]);
        // Placeholder is unique per redaction (index) and unforgeable (nonce).
        let placeholder = format!("[REDACTED:{}#{}-{}]", category.as_str(), nonce, i + 1);
        out.push_str(&placeholder);
        replacements.push(Replacement {
            placeholder,
            original: text[*start..*end].to_string(),
        });
        cursor = *end;
    }
    out.push_str(&text[cursor..]);

    Redaction {
        redacted_text: out,
        replacements,
    }
}

/// Restore the original values in a model reply by swapping each placeholder
/// back. The originals never left the device; this just un-masks them in the
/// text the user sees. A placeholder the model didn't echo simply isn't found.
pub fn rehydrate(reply: &str, replacements: &[Replacement]) -> String {
    let mut out = reply.to_string();
    for r in replacements {
        if out.contains(&r.placeholder) {
            out = out.replace(&r.placeholder, &r.original);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(text: &str, start: usize, end: usize, cat: RuleCategory) -> Span {
        Span {
            text: text.into(),
            start_byte: start,
            end_byte: end,
            start_char: start,
            end_char: end,
            category: cat,
            rule: "test",
        }
    }

    // Fixed nonce for deterministic assertions (production passes a random one).
    const N: &str = "t";

    #[test]
    fn redacts_value_spans_and_maps_back() {
        let text = "email me at a@b.com about it";
        let s = span("a@b.com", 12, 19, RuleCategory::PiiContact);
        let r = redact(text, &[s], N);
        assert!(r.is_redacted());
        assert_eq!(
            r.redacted_text,
            "email me at [REDACTED:PII_CONTACT#t-1] about it"
        );
        assert!(
            !r.redacted_text.contains("a@b.com"),
            "value must not survive in the sent text"
        );
        // The model echoes the placeholder; rehydration restores the original.
        let reply = "Sure, I'll email [REDACTED:PII_CONTACT#t-1] shortly.";
        assert_eq!(
            rehydrate(reply, &r.replacements),
            "Sure, I'll email a@b.com shortly."
        );
    }

    #[test]
    fn does_not_redact_proprietary_cues() {
        // "confidential" is a cue, not a value — redacting it would strip the
        // signal, not the secret. Left untouched so the re-classify keeps it local.
        let text = "this is confidential info about the deal";
        let s = span("confidential", 8, 20, RuleCategory::Proprietary);
        let r = redact(text, &[s], N);
        assert!(!r.is_redacted(), "proprietary cues are never redacted");
        assert_eq!(r.redacted_text, text);
    }

    #[test]
    fn multiple_spans_get_distinct_placeholders() {
        let text = "ssn 123-45-6789 card 4111111111111111";
        let spans = [
            span("123-45-6789", 4, 15, RuleCategory::PiiId),
            span("4111111111111111", 21, 37, RuleCategory::Financial),
        ];
        let r = redact(text, &spans, N);
        assert_eq!(r.replacements.len(), 2);
        assert_eq!(
            r.redacted_text,
            "ssn [REDACTED:PII_ID#t-1] card [REDACTED:FINANCIAL#t-2]"
        );
        assert!(!r.redacted_text.contains("123-45-6789"));
        assert!(!r.redacted_text.contains("4111111111111111"));
    }

    #[test]
    fn nested_overlapping_spans_merge_to_one() {
        // A span fully inside another → one placeholder covering the outer range.
        let text = "contact a@b.com now";
        let spans = [
            span("a@b.com", 8, 15, RuleCategory::PiiContact),
            span("b.com", 10, 15, RuleCategory::PiiContact),
        ];
        let r = redact(text, &spans, N);
        assert_eq!(r.replacements.len(), 1);
        assert_eq!(r.redacted_text, "contact [REDACTED:PII_CONTACT#t-1] now");
    }

    #[test]
    fn staggered_overlap_redacts_the_union_no_tail_leak() {
        // REGRESSION (security review CRITICAL): span A [0,8) and span B [4,12)
        // stagger — B starts inside A but ends 4 bytes later. The old "skip the
        // later span" logic dropped B entirely and leaked "CCCC" (B's tail) to
        // the wire. The union merge must black out the WHOLE [0,12).
        let text = "AAAABBBBCCCC";
        let spans = [
            span("AAAABBBB", 0, 8, RuleCategory::Financial),
            span("BBBBCCCC", 4, 12, RuleCategory::PiiId),
        ];
        let r = redact(text, &spans, N);
        assert_eq!(
            r.replacements.len(),
            1,
            "the overlapping cluster is one union"
        );
        assert_eq!(r.redacted_text, "[REDACTED:FINANCIAL#t-1]");
        assert!(
            !r.redacted_text.contains("CCCC"),
            "no byte of the staggered span may survive"
        );
        assert!(!r.redacted_text.contains("BBBB"));
    }

    #[test]
    fn adjacent_but_non_overlapping_spans_stay_separate() {
        // end == start is NOT an overlap → two placeholders (nothing between leaks).
        let text = "aaaabbbb";
        let spans = [
            span("aaaa", 0, 4, RuleCategory::PiiId),
            span("bbbb", 4, 8, RuleCategory::Financial),
        ];
        let r = redact(text, &spans, N);
        assert_eq!(r.replacements.len(), 2);
        assert_eq!(
            r.redacted_text,
            "[REDACTED:PII_ID#t-1][REDACTED:FINANCIAL#t-2]"
        );
    }

    #[test]
    fn multibyte_text_slices_on_char_boundaries() {
        // A multibyte prefix must not shift the redaction (byte offsets).
        let text = "café → mail a@b.com";
        let start = text.find("a@b.com").unwrap();
        let s = span("a@b.com", start, start + 7, RuleCategory::PiiContact);
        let r = redact(text, &[s], N);
        assert_eq!(r.redacted_text, "café → mail [REDACTED:PII_CONTACT#t-1]");
        assert!(r.redacted_text.starts_with("café → mail"));
    }

    #[test]
    fn nonce_makes_placeholders_unforgeable() {
        // Two different nonces produce different placeholders for the same span,
        // so untrusted content can't guess the token rehydrate() will expand.
        let text = "mail a@b.com";
        let s = span("a@b.com", 5, 12, RuleCategory::PiiContact);
        let a = redact(text, std::slice::from_ref(&s), "aaaa");
        let b = redact(text, &[s], "bbbb");
        assert_ne!(a.replacements[0].placeholder, b.replacements[0].placeholder);
        // A forged placeholder with the wrong nonce is not rehydrated.
        assert_eq!(
            rehydrate("see [REDACTED:PII_CONTACT#zzzz-1]", &a.replacements),
            "see [REDACTED:PII_CONTACT#zzzz-1]"
        );
    }

    #[test]
    fn no_spans_is_a_noop() {
        let text = "nothing sensitive here";
        let r = redact(text, &[], N);
        assert!(!r.is_redacted());
        assert_eq!(r.redacted_text, text);
        assert_eq!(rehydrate("a reply", &r.replacements), "a reply");
    }

    #[test]
    fn out_of_range_or_unaligned_spans_are_ignored() {
        // A span past the end, or not on a char boundary, is dropped (defensive).
        // In "café …", 'é' occupies bytes 3–4, so byte 4 is mid-character.
        let text = "café a@b.com";
        let unaligned = span("x", 4, 5, RuleCategory::PiiContact); // starts mid-'é'
        let past = span("x", 50, 60, RuleCategory::PiiId);
        let r = redact(text, &[unaligned, past], N);
        assert!(!r.is_redacted());
        assert_eq!(r.redacted_text, text);
    }
}
