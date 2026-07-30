//! Cloud model pricing → real cost for the usage ledger (Wave 3.2, PLAN §3).
//!
//! The honesty rule (PLAN §3): we NEVER guess a cost. A cloud call's dollar
//! cost is computed only when BOTH the endpoint reported token `usage` AND this
//! model has a known price here. Anything else stays `None` in the ledger — a
//! visible "flying blind" flag, not a fabricated number. Local calls are $0 and
//! never consult this table.
//!
//! Prices are USD per 1,000,000 tokens (input, output). The table is a small,
//! auditable set of well-known models keyed by a normalized substring match, so
//! provider prefixes (`openai/gpt-4o`, `anthropic/claude-…`, an OpenRouter slug)
//! resolve to the same entry. It is intentionally conservative: an unrecognized
//! model returns `None` rather than a nearest-guess. Prices drift, so this is a
//! floor for "we can at least show a number," not a billing source of truth.

/// (input $/Mtok, output $/Mtok) for a model whose id CONTAINS `needle`
/// (checked case-insensitively, first match wins — order most-specific first).
const PRICES: &[(&str, f64, f64)] = &[
    // OpenAI
    ("gpt-4o-mini", 0.15, 0.60),
    ("gpt-4o", 2.50, 10.00),
    ("gpt-4.1-mini", 0.40, 1.60),
    ("gpt-4.1", 2.00, 8.00),
    ("gpt-4-turbo", 10.00, 30.00),
    ("gpt-3.5-turbo", 0.50, 1.50),
    ("o3-mini", 1.10, 4.40),
    // Anthropic Claude
    ("claude-3-5-haiku", 0.80, 4.00),
    ("claude-3-haiku", 0.25, 1.25),
    ("claude-3-5-sonnet", 3.00, 15.00),
    ("claude-3-7-sonnet", 3.00, 15.00),
    ("claude-3-opus", 15.00, 75.00),
    // Google Gemini
    ("gemini-1.5-flash", 0.075, 0.30),
    ("gemini-1.5-pro", 1.25, 5.00),
];

/// The USD cost of a cloud call, or `None` if the model isn't priced here
/// (→ the ledger flags it "unknown", never a guess). `model` is matched
/// case-insensitively against known substrings, so a provider prefix
/// (`openai/…`, `anthropic/…`) still resolves.
pub fn cost_usd(model: &str, prompt_tokens: u32, completion_tokens: u32) -> Option<f64> {
    let m = model.to_ascii_lowercase();
    let (in_per_m, out_per_m) = PRICES
        .iter()
        .find(|(needle, _, _)| m.contains(needle))
        .map(|(_, i, o)| (*i, *o))?;
    let cost = (prompt_tokens as f64 / 1_000_000.0) * in_per_m
        + (completion_tokens as f64 / 1_000_000.0) * out_per_m;
    Some(cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_model_computes_real_cost() {
        // gpt-4o: $2.50/Mtok in, $10/Mtok out. 1000 in + 500 out.
        let c = cost_usd("gpt-4o", 1000, 500).unwrap();
        let expected = (1000.0 / 1e6) * 2.50 + (500.0 / 1e6) * 10.00;
        assert!(
            (c - expected).abs() < 1e-12,
            "cost = {c}, expected {expected}"
        );
    }

    #[test]
    fn provider_prefixed_and_cased_ids_resolve() {
        assert!(cost_usd("openai/GPT-4o", 100, 100).is_some());
        assert!(cost_usd("anthropic/claude-3-5-sonnet-20241022", 100, 100).is_some());
    }

    #[test]
    fn more_specific_variant_wins_over_general() {
        // "gpt-4o-mini" must NOT resolve to the "gpt-4o" price (it's listed
        // first / more specific), else the cheaper mini would be overcharged.
        let mini = cost_usd("gpt-4o-mini", 1_000_000, 0).unwrap();
        assert!((mini - 0.15).abs() < 1e-9, "mini input price = {mini}");
        let full = cost_usd("gpt-4o", 1_000_000, 0).unwrap();
        assert!((full - 2.50).abs() < 1e-9);
    }

    #[test]
    fn unknown_model_is_none_never_a_guess() {
        assert_eq!(cost_usd("some-local-llama-7b", 5000, 5000), None);
        assert_eq!(cost_usd("qwen3.6-35b-a3b", 5000, 5000), None);
    }
}
