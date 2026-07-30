//! Layer 0: deterministic, high-recall detectors for structured secrets +
//! confidentiality cues. Direct port of the delivered `privacy-filter/src/rules.py`
//! (v3) to Rust.
//!
//! Anything matched here is a hard signal with zero ML uncertainty — this
//! layer never returns [`Label::Uncertain`]; either it finds a span (Private)
//! or it doesn't (Public — subject to the supplementary soft-signal check in
//! [`RulesClassifier`], see below). Tuned for recall on purpose: a false
//! alarm just routes the message local; a miss is a leak. This mirrors the
//! Python docstring's stated philosophy verbatim and is a *different* bias
//! than `heuristic.rs`'s original "prefer low false-positive" design goal —
//! that tradeoff is intentional; see PLAN §11 / the rules.rs port notes.
//!
//! What this layer does NOT attempt (left to the future ONNX ensemble,
//! `engine::EnsembleClassifier` — "layer 1"): PII_NAME, HEALTH, LOCATION,
//! PII_ORG, PERSONAL_CONTEXT. Those require semantic judgment a regex can't
//! make safely. [`RulesClassifier`] below temporarily borrows `heuristic.rs`'s
//! two orphaned soft detectors (health, home address) as a stopgap for two of
//! those five categories until the ensemble ships.
//!
//! ## Differences from the Python source (`re` vs `regex` crate)
//!
//! - **No lookaround.** Rust's `regex` crate (RE2-derived, no backtracking —
//!   this also means none of these patterns can ReDoS, a correctness win the
//!   Python original doesn't have) does not support `(?<!...)`/`(?!...)`.
//!   The phone pattern used `(?<!\d)...(?!\d)` to avoid matching inside a
//!   longer digit run; ported here as a manual boundary check on the
//!   characters immediately before/after each match ([`is_isolated_run`]).
//! - **Named capture groups, inline `(?i)`, `\b`, `\d`, `\s` Unicode
//!   semantics** are all directly supported by the `regex` crate with
//!   identical syntax — every other pattern is a verbatim 1:1 port.
//! - **Unicode digit folding.** Python's `unicodedata.digit()` reads the
//!   Unicode Character Database's per-codepoint numeric value, covering every
//!   decimal-digit script. Rust's `std` has no equivalent lookup, and pulling
//!   in a crate (`unicode-normalization` NFKC) only covers compatibility
//!   folding (e.g. fullwidth), not distinct native-digit scripts (Arabic-Indic,
//!   Devanagari, ...). [`DIGIT_BLOCKS`] is a small hand-rolled table of the
//!   most realistic decimal-digit blocks (every Unicode `Nd` block is a
//!   guaranteed contiguous run of 10 codepoints, so `value = cp - block_start`)
//!   — not the full ~66-block Unicode set. Extend the table if a missed
//!   script matters later. Zero new crates required.
//! - **Char offsets vs byte offsets.** `regex` on `&str` returns *byte*
//!   offsets; Python `re` on `str` returns *codepoint* offsets. [`Span`]
//!   carries both so callers can pick: byte offsets are needed to slice the
//!   original Rust `&str` safely; char offsets are the closest parity with
//!   the Python original and with what a spec might call "char offset". Note
//!   neither is what a JS/TS frontend's `String.slice()` uses (UTF-16 code
//!   units) — if the redaction UI slices displayed text by index, that's a
//!   third offset space and needs its own conversion at the IPC boundary.
//!   `Span::text` (the exact matched substring) is included so a first-cut
//!   UI can do a text-based redaction and sidestep the offset-space question
//!   entirely.
//!
//! No `entropy` detector exists in the delivered `rules.py` — despite the
//! phrase "high-entropy secrets", the closest things are `_LABELED_SECRET`
//! (structural: `password is <value>`) and `_IDISH` (character-class shaped,
//! not an actual Shannon-entropy calculation). This port stays faithful to
//! what was delivered; a true entropy-threshold detector would be new scope,
//! not a port, and isn't added here.

use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;

use super::{Classification, Classifier, Label};

// ── category ────────────────────────────────────────────────────────────

/// Category label a deterministic rule can assign. Strings match
/// `prompt.py::CATEGORIES` exactly (the subset a regex can respons­ibly
/// claim) so logs/UI/the future ensemble output share one vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuleCategory {
    PiiContact,
    PiiId,
    Financial,
    Credential,
    Proprietary,
}

impl RuleCategory {
    pub const fn as_str(self) -> &'static str {
        match self {
            RuleCategory::PiiContact => "PII_CONTACT",
            RuleCategory::PiiId => "PII_ID",
            RuleCategory::Financial => "FINANCIAL",
            RuleCategory::Credential => "CREDENTIAL",
            RuleCategory::Proprietary => "PROPRIETARY",
        }
    }
}

/// One deterministic hit: exact span (both byte and char offsets, see the
/// module docs) + category + which specific rule fired.
///
/// `rule` is more granular than the Python original (which always set
/// `"rule" == category`) — e.g. `"token_stripe"` vs `"token_aws"` instead of
/// a blanket `"CREDENTIAL"` — for a more useful audit log / redaction UI.
/// Deliberate, non-breaking improvement over the source; documented here so
/// it isn't mistaken for a divergence bug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_char: usize,
    pub end_char: usize,
    pub category: RuleCategory,
    pub rule: &'static str,
}

// ── confidentiality cues ───────────────────────────────────────────────

/// Verbatim port of `rules.py::CONFIDENTIALITY_CUES`.
const CONFIDENTIALITY_CUES: &[&str] = &[
    r"\bconfidential(?:ity)?\b",
    r"\b(?:strictly|highly)\s+confidential\b",
    r"\bprivileged\s+and\s+confidential\b",
    r"\binternal\s+(?:only|use(?:\s+only)?)\b",
    r"\bfor\s+internal\s+use\b",
    r"\bkeep\s+(?:it\s+|this\s+)?internal\b",
    r"\bkeep\s+(?:this\s+)?under\s+wraps\b",
    r"\bnot\s+for\s+(?:external|public|distribution|external\s+eyes)\b",
    r"\bdo\s*n'?o?t\s+share\b",
    r"\bdon'?t\s+share\b",
    r"\bdo\s*n'?o?t\s+(?:forward|distribute|disclose|screenshot)\b",
    r"\bcan'?t\s+leave\s+the\s+company\b",
    r"\bembargo(?:ed)?\b",
    r"\bunder\s+embargo\b",
    r"\bunreleased\b",
    r"\bnot\s+(?:yet\s+)?(?:public|announced)\b",
    r"\bpre-?(?:announcement|release|launch)\b",
    r"\bstealth[-\s]?mode\b",
    r"\bcode[-\s]?named?\b",
    r"\bstill\s+under\s+wraps\b",
    r"\bunder\s+nda\b",
    r"\bnda[-\s]?covered\b",
    r"\bcovered\s+by\s+(?:our\s+)?nda\b",
    r"\bproprietary\b",
    r"\btrade\s+secret\b",
    r"\bboard\s+only\b",
    r"\b(?:leadership|exec(?:utive)?s?)\s+only\b",
    r"\bdeal\s+team\s+only\b",
];

/// Common decimal-digit (`Nd`) block starts. Every Unicode `Nd` block is a
/// guaranteed contiguous run of 10 codepoints (UAX #44), so digit value is
/// always `codepoint - block_start`. Not exhaustive (~18 of ~66 blocks) —
/// covers the realistic obfuscation surface (fullwidth, Arabic-Indic,
/// Devanagari, ...) for a personal single-user tool. Extend if needed.
const DIGIT_BLOCKS: &[u32] = &[
    0x0660, // Arabic-Indic
    0x06F0, // Extended Arabic-Indic (Persian)
    0x07C0, // NKo
    0x0966, // Devanagari
    0x09E6, // Bengali
    0x0A66, // Gurmukhi
    0x0AE6, // Gujarati
    0x0B66, // Oriya
    0x0BE6, // Tamil
    0x0C66, // Telugu
    0x0CE6, // Kannada
    0x0D66, // Malayalam
    0x0E50, // Thai
    0x0ED0, // Lao
    0x0F20, // Tibetan
    0x1040, // Myanmar
    0xFF10, // Fullwidth
];

fn unicode_digit_value(c: char) -> Option<u8> {
    let cp = c as u32;
    for &start in DIGIT_BLOCKS {
        if cp >= start && cp < start + 10 {
            return Some((cp - start) as u8);
        }
    }
    None
}

/// Fold every recognized Unicode decimal digit to its ASCII equivalent,
/// leaving every other char untouched. Char-count preserving (1 char in, 1
/// char out) — mirrors `rules.py::_fold_unicode_digits` — but *not*
/// byte-length preserving, since a folded multi-byte digit becomes a
/// single-byte ASCII digit. Callers must remap by char index, not byte
/// index, when translating matches on the folded string back onto the
/// original (see [`detect`]'s spaced-digit branch).
fn fold_unicode_digits(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_digit() {
                c
            } else if let Some(v) = unicode_digit_value(c) {
                (b'0' + v) as char
            } else {
                c
            }
        })
        .collect()
}

/// Byte offset of the start of each char, plus a trailing sentinel of
/// `s.len()`. `char_byte_offsets(s)[k]` is the byte offset of char index
/// `k`; used to map a char-index range back to a byte range for slicing.
fn char_byte_offsets(s: &str) -> Vec<usize> {
    let mut v: Vec<usize> = s.char_indices().map(|(i, _)| i).collect();
    v.push(s.len());
    v
}

// ── Luhn + digit-run classification ────────────────────────────────────

/// Standard mod-10 Luhn checksum. `digits` must be ASCII `0`-`9` only.
fn luhn_ok(digits: &str) -> bool {
    if digits.len() < 13 {
        return false;
    }
    let mut total: u32 = 0;
    for (i, c) in digits.chars().rev().enumerate() {
        let mut n = c.to_digit(10).unwrap_or(0);
        if i % 2 == 1 {
            n *= 2;
            if n > 9 {
                n -= 9;
            }
        }
        total += n;
    }
    total % 10 == 0
}

/// `rules.py::_classify_digit_run` — what a bare N-digit run probably is.
fn classify_digit_run(digits: &str) -> Option<RuleCategory> {
    match digits.len() {
        9 => Some(RuleCategory::PiiId),            // SSN / EIN / routing-ish
        10 | 11 => Some(RuleCategory::PiiContact), // phone
        13..=19 if luhn_ok(digits) => Some(RuleCategory::Financial), // card
        _ => None,
    }
}

const LETTER_SWAP: &[(char, char)] = &[
    ('o', '0'),
    ('O', '0'),
    ('l', '1'),
    ('I', '1'),
    ('i', '1'),
    ('B', '8'),
    ('S', '5'),
    ('s', '5'),
    ('Z', '2'),
    ('z', '2'),
];

fn letter_swap(c: char) -> char {
    LETTER_SWAP
        .iter()
        .find(|(k, _)| *k == c)
        .map(|(_, v)| *v)
        .unwrap_or(c)
}

// ── compiled patterns (built once) ─────────────────────────────────────

struct Patterns {
    email: Regex,
    ssn: Regex,
    /// Lookaround-free version of `rules.py::_PHONE`; isolation from a
    /// longer digit run is enforced by [`is_isolated_run`] after matching.
    phone: Regex,
    ipv4: Regex,
    tokens: Vec<Regex>,
    labeled_secret: Regex,
    card_candidate: Regex,
    cues: Vec<Regex>,
    spelled_run: Regex,
    spaced_digits: Regex,
    email_obf: Regex,
    idish: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        email: Regex::new(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b").unwrap(),
        ssn: Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").unwrap(),
        phone: Regex::new(
            r"(?:\+?1[\s.-]?)?(?:\(\d{3}\)|\d{3})[\s.-]?\d{3}[\s.-]?\d{4}",
        )
        .unwrap(),
        ipv4: Regex::new(
            r"\b(?:(?:25[0-5]|2[0-4]\d|1?\d?\d)\.){3}(?:25[0-5]|2[0-4]\d|1?\d?\d)\b",
        )
        .unwrap(),
        tokens: vec![
            Regex::new(r"\bsk-[A-Za-z0-9_-]{12,}\b").unwrap(),
            Regex::new(r"\bgh[pousr]_[A-Za-z0-9]{16,}\b").unwrap(),
            Regex::new(r"\bAKIA[0-9A-Z]{16}\b").unwrap(),
            Regex::new(r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b").unwrap(),
            Regex::new(r"\bAIza[0-9A-Za-z_-]{20,}\b").unwrap(),
            Regex::new(r"\bsk_live_[A-Za-z0-9]{16,}\b").unwrap(),
        ],
        labeled_secret: Regex::new(
            r"(?i)\b(?:password|passwd|pwd|api[\s_-]?key|secret|token|passphrase|seed\s?phrase)\b\s*(?:is|are|=|:)\s*(?P<val>\S+)",
        )
        .unwrap(),
        card_candidate: Regex::new(r"\b(?:\d[ -]?){13,19}\b").unwrap(),
        cues: CONFIDENTIALITY_CUES
            .iter()
            .map(|p| Regex::new(&format!("(?i){p}")).unwrap())
            .collect(),
        spelled_run: Regex::new(
            r"(?i)(?:\b(?:zero|oh|one|two|three|four|five|six|seven|eight|nine)\b[\s.,\-]*){5,}",
        )
        .unwrap(),
        spaced_digits: Regex::new(r"(?:\d[\s._\-]*){9,}").unwrap(),
        email_obf: Regex::new(
            r"(?i)[A-Za-z0-9._%+\-]+\s*(?:@|\[\s*at\s*\]|\(\s*at\s*\))\s*[A-Za-z0-9.\-]+\s*(?:\.|\[\s*dot\s*\]|\(\s*dot\s*\))\s*[A-Za-z]{2,}",
        )
        .unwrap(),
        idish: Regex::new(r"[0-9OlIiBSZoblisz][0-9OlIiBSZoblisz\-]{6,}").unwrap(),
    })
}

/// Port of the `(?<!\d)...(?!\d)` lookaround on `rules.py::_PHONE`: the char
/// immediately before/after the match (if any) must not itself be a digit,
/// so this doesn't match inside a longer digit run. Uses `is_ascii_digit`
/// rather than a full Unicode-digit check (Python's `\d` is Unicode-aware
/// even in a lookbehind) — a narrow fidelity gap, low severity: a phone
/// number directly abutting a non-ASCII digit is a rare adversarial case.
fn is_isolated_run(text: &str, start: usize, end: usize) -> bool {
    let before_ok = text[..start]
        .chars()
        .next_back()
        .map_or(true, |c| !c.is_ascii_digit());
    let after_ok = text[end..]
        .chars()
        .next()
        .map_or(true, |c| !c.is_ascii_digit());
    before_ok && after_ok
}

// ── detect ──────────────────────────────────────────────────────────────

type RawSpan = (usize, usize, RuleCategory, &'static str);

/// Return every private span [`detect`] finds, in `text`, as exact
/// byte+char offsets + category + rule id. Deterministic, pure, safe to run
/// on every send. Mirrors `rules.py::detect`.
pub fn detect(text: &str) -> Vec<Span> {
    let p = patterns();
    let mut raw: Vec<RawSpan> = Vec::new();

    for m in p.email.find_iter(text) {
        raw.push((m.start(), m.end(), RuleCategory::PiiContact, "email"));
    }
    for m in p.ssn.find_iter(text) {
        raw.push((m.start(), m.end(), RuleCategory::PiiId, "ssn"));
    }
    for m in p.phone.find_iter(text) {
        if is_isolated_run(text, m.start(), m.end()) {
            raw.push((m.start(), m.end(), RuleCategory::PiiContact, "phone"));
        }
    }
    for m in p.ipv4.find_iter(text) {
        raw.push((m.start(), m.end(), RuleCategory::PiiContact, "ipv4"));
    }
    for tok in &p.tokens {
        for m in tok.find_iter(text) {
            raw.push((m.start(), m.end(), RuleCategory::Credential, "token"));
        }
    }
    for caps in p.labeled_secret.captures_iter(text) {
        if let Some(val) = caps.name("val") {
            raw.push((
                val.start(),
                val.end(),
                RuleCategory::Credential,
                "labeled_secret",
            ));
        }
    }
    for m in p.card_candidate.find_iter(text) {
        let digits: String = m.as_str().chars().filter(|c| c.is_ascii_digit()).collect();
        if luhn_ok(&digits) {
            raw.push((m.start(), m.end(), RuleCategory::Financial, "luhn_card"));
        }
    }
    for cue in &p.cues {
        for m in cue.find_iter(text) {
            raw.push((
                m.start(),
                m.end(),
                RuleCategory::Proprietary,
                "confidentiality_cue",
            ));
        }
    }
    for m in p.spelled_run.find_iter(text) {
        raw.push((m.start(), m.end(), RuleCategory::PiiId, "spelled_digits"));
    }

    // Obfuscated digit runs: fold unicode digits to ASCII first, then match.
    // The folded string has the same CHAR count as `text` but a different
    // BYTE length, so matches on it must be remapped via char index, not
    // byte index, back onto `text` (see fold_unicode_digits' doc comment).
    let folded = fold_unicode_digits(text);
    let orig_char_bytes = char_byte_offsets(text);
    for m in p.spaced_digits.find_iter(&folded) {
        let digits: String = m.as_str().chars().filter(|c| c.is_ascii_digit()).collect();
        if let Some(cat) = classify_digit_run(&digits) {
            let start_char = folded[..m.start()].chars().count();
            let end_char = folded[..m.end()].chars().count();
            if let (Some(&sb), Some(&eb)) = (
                orig_char_bytes.get(start_char),
                orig_char_bytes.get(end_char),
            ) {
                raw.push((sb, eb, cat, "obfuscated_digits"));
            }
        }
    }

    for m in p.idish.find_iter(text) {
        let tok = m.as_str();
        if tok.chars().filter(|c| c.is_ascii_digit()).count() < 2 {
            continue;
        }
        let swapped: String = tok.chars().map(letter_swap).collect();
        let digits: String = swapped.chars().filter(|c| c.is_ascii_digit()).collect();
        if let Some(cat) = classify_digit_run(&digits) {
            raw.push((m.start(), m.end(), cat, "idish"));
        }
    }
    for m in p.email_obf.find_iter(text) {
        raw.push((
            m.start(),
            m.end(),
            RuleCategory::PiiContact,
            "email_obfuscated",
        ));
    }

    // De-dup EXACT (start,end) duplicates only (e.g. two detectors firing on
    // the identical substring) — overlapping-but-differently-bounded spans
    // (e.g. a structured email match vs. an obfuscated-email match with
    // different bounds) both survive, same as rules.py.
    raw.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    raw.retain(|s| seen.insert((s.0, s.1)));

    raw.into_iter()
        .map(|(sb, eb, category, rule)| Span {
            text: text[sb..eb].to_string(),
            start_char: text[..sb].chars().count(),
            end_char: text[..eb].chars().count(),
            start_byte: sb,
            end_byte: eb,
            category,
            rule,
        })
        .collect()
}

// ── Classifier trait adapter ───────────────────────────────────────────

/// Deterministic layer-0 classifier: wraps [`detect`] for drop-in use
/// wherever a `Classifier` is expected (the §7 [`crate::agent::gate::PrivacyGate`]
/// and the hook chain's `PrivacyFilterHook`).
///
/// Always [`Label::Private`] at confidence 1.0 when [`detect`] finds
/// anything (no ambiguity — that's the whole point of layer 0). As a
/// stopgap for the two soft categories `rules.py` structurally can't cover
/// (HEALTH, LOCATION) until the ONNX ensemble ships, this also runs
/// `heuristic::detect_health_info` / `heuristic::detect_home_address` and
/// reports [`Label::Uncertain`] if only those fire. Everything else in
/// `heuristic.rs` (SSN, Luhn card, API key, email+password, phone+context,
/// financial-account) is superseded by [`detect`], which is a strict
/// superset with span-level output.
#[derive(Debug, Default, Clone, Copy)]
pub struct RulesClassifier;

impl RulesClassifier {
    pub fn new() -> Self {
        Self
    }
}

impl Classifier for RulesClassifier {
    fn classify(&self, text: &str) -> Classification {
        let spans = detect(text);
        if !spans.is_empty() {
            let raw_output = vec![spans.len() as f32];
            return Classification {
                label: Label::Private,
                confidence: 1.0,
                raw_output,
                spans,
            };
        }

        // Temporary stopgap for HEALTH / LOCATION — see doc comment above.
        if super::heuristic::detect_health_info(text).is_some()
            || super::heuristic::detect_home_address(text).is_some()
        {
            return Classification {
                label: Label::Uncertain,
                confidence: 0.8,
                raw_output: Vec::new(),
                spans: Vec::new(),
            };
        }

        // C-01: a rules MISS (nothing matched, no soft signal) was previously
        // reported as Public@confidence-1.0, misleading callers into treating
        // the fallback as authoritative. With no signal either way, confidence
        // should be 0.0 — the caller must not treat a rules-only non-match as
        // certainty that content is safe for cloud egress.
        Classification {
            label: Label::Public,
            confidence: 0.0,
            raw_output: Vec::new(),
            spans: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cats(text: &str) -> Vec<(&'static str, String)> {
        detect(text)
            .into_iter()
            .map(|s| (s.category.as_str(), s.text))
            .collect()
    }

    // Direct translation of rules.py's __main__ smoke tests.

    #[test]
    fn email() {
        assert_eq!(
            cats("email me at a.b@x.com"),
            vec![("PII_CONTACT", "a.b@x.com".into())]
        );
    }

    #[test]
    fn ssn() {
        assert_eq!(
            cats("ssn 512-88-1029"),
            vec![("PII_ID", "512-88-1029".into())]
        );
    }

    #[test]
    fn phone() {
        assert_eq!(
            cats("call 217-555-0142"),
            vec![("PII_CONTACT", "217-555-0142".into())]
        );
    }

    #[test]
    fn token() {
        let r = cats("key sk-live-9a8b7c6d5e4f0011");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "CREDENTIAL");
    }

    #[test]
    fn luhn_card() {
        let r = cats("card 4111 1111 1111 1111");
        assert_eq!(r, vec![("FINANCIAL", "4111 1111 1111 1111".into())]);
    }

    #[test]
    fn labeled_secret() {
        let r = cats("the wifi password is hunter2Secure");
        assert_eq!(r, vec![("CREDENTIAL", "hunter2Secure".into())]);
    }

    #[test]
    fn ip_address() {
        assert_eq!(
            cats("my ip is 73.42.19.8"),
            vec![("PII_CONTACT", "73.42.19.8".into())]
        );
    }

    #[test]
    fn confidentiality_cues_without_pii() {
        let r = cats(
            "Our unreleased product Project Cardinal launches in March. Internal only, do not share.",
        );
        assert!(r.iter().all(|(c, _)| *c == "PROPRIETARY"));
        assert!(r.len() >= 2, "expected multiple cue hits, got {r:?}");
    }

    #[test]
    fn spelled_out_digits() {
        let r = cats("acct spelled out: four two nine one seven three six zero five");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "PII_ID");
    }

    #[test]
    fn spaced_digit_run() {
        let r = cats("routing 3 3 9 8 8 2 1 0 4 please");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "PII_ID"); // 9 digits
    }

    #[test]
    fn obfuscated_email() {
        let r = cats("reach me at j.mora [at] gmail [dot] com");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "PII_CONTACT");
    }

    #[test]
    fn normal_sentence_is_clean() {
        assert_eq!(cats("just a normal sentence about cookies"), vec![]);
    }

    #[test]
    fn stack_trace_is_not_flagged_by_rules_layer() {
        // rules.py itself does NOT special-case code/stack-traces (that's
        // prompt.py's rubric rule 5, enforced by the future ML layer) — a
        // plain error message with no PII-shaped text produces no spans.
        assert_eq!(
            cats("Debug this: TypeError: cannot read property 'map' of undefined"),
            vec![]
        );
    }

    #[test]
    fn fullwidth_digit_ssn_is_folded_and_caught() {
        // U+FF15 etc. (fullwidth 5-1-2-8-8-1-0-2-9) folds to ASCII, then the
        // spaced-digit path classifies the resulting 9-digit run as PII_ID.
        let fullwidth = "５１２８８１０２９";
        let r = cats(&format!("id: {fullwidth}"));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "PII_ID");
        // start/end char offsets must point back at the fullwidth text
        // in the ORIGINAL string, not the (byte-shorter) folded one.
        let spans = detect(&format!("id: {fullwidth}"));
        assert_eq!(spans[0].text, fullwidth);
    }

    #[test]
    fn dedup_keeps_exact_duplicates_out() {
        // A single SSN-shaped hit shouldn't appear twice even though it
        // could in principle satisfy more than one detector.
        let spans = detect("ssn 512-88-1029 ssn 512-88-1029");
        let unique: HashSet<_> = spans.iter().map(|s| (s.start_byte, s.end_byte)).collect();
        assert_eq!(spans.len(), unique.len());
    }

    #[test]
    fn phone_not_matched_inside_longer_digit_run() {
        // Emulates the (?<!\d)...(?!\d) lookaround: a 10-digit chunk
        // embedded in a longer bare digit run should not fire as PII_CONTACT
        // via the phone pattern (it may still be caught by the digit-run
        // path with a different category, which is fine / matches Python).
        let r = detect("order 12345678901234567890");
        assert!(
            !r.iter().any(|s| s.rule == "phone"),
            "phone rule should not fire inside a longer digit run: {r:?}"
        );
    }

    #[test]
    fn classifier_trait_private_on_hard_hit() {
        let c = RulesClassifier.classify("my ssn is 512-88-1029");
        assert_eq!(c.label, Label::Private);
        assert_eq!(c.confidence, 1.0);
        assert_eq!(c.spans.len(), 1);
    }

    #[test]
    fn classifier_trait_uncertain_on_soft_health_signal() {
        let c = RulesClassifier.classify("I was diagnosed with the flu last week");
        assert_eq!(c.label, Label::Uncertain);
        assert!(c.spans.is_empty());
    }

    #[test]
    fn classifier_trait_public_on_clean_text() {
        let c = RulesClassifier.classify("what's the capital of france?");
        assert_eq!(c.label, Label::Public);
    }
}
