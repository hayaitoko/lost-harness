//! Deterministic context compaction (Wave 3.3, PLAN §9 "pre-compaction flush"
//! + §3 "cache-shaped prompt assembly"). A pure function that shrinks the
//! **model-facing** chat history to fit a char budget — the stored transcript
//! is never touched (compaction takes a borrowed slice and returns owned
//! values; SQLite keeps every message, "whatever gets trimmed is still in the
//! archive").
//!
//! Design invariants (tests below lock them):
//! - **Deterministic**: a pure function of the input slice + the two size
//!   knobs. No model call, no `chrono`, no RNG — same history → same result.
//! - **Prefix-stable**: the leading run of `system` messages (the tool catalog
//!   + the frozen curated-summary block) is ALWAYS kept byte-identical, so the
//!   KV/prompt-cache prefix is reused across a conversation's turns.
//! - **Whole messages only**: never slices a message's content — that would cut
//!   through a redaction placeholder or a guard-wrap frame. It drops entire
//!   older messages from the middle and keeps the most recent `keep_recent`.
//!   Consequently the budget is a TARGET, not a hard cap: the protected recent
//!   tail (and any single oversized message in it) is kept whole even if that
//!   pushes the send over budget. The caller raises `keep_recent` to pin the
//!   current user turn forward, so the actual question is never trimmed.
//! - **3.5-ready**: the dropped middle is returned as `trimmed` (oldest-first)
//!   — the exact set Wave 3.5's pre-compaction flush sweeps for durable facts
//!   BEFORE they leave the wire. 3.3 itself does not consume it.
//!
//! Char budget, not tokens: there is no tokenizer in the build. A conservative
//! ~4-chars-per-token proxy over Unicode scalar counts is the deterministic,
//! platform-stable estimate.

use crate::models::ChatMessage;

/// Model-facing history size budget, in characters. ~6k tokens at a
/// conservative 4 chars/token — comfortable headroom under an 8k local
/// context window. A per-profile / per-model override can thread a different
/// `budget_chars` into [`compact_history`] later without changing its shape.
pub(crate) const COMPACT_BUDGET_CHARS: usize = 24_000;

/// Floor on how many of the most-recent messages are protected from trimming.
/// The CALLER raises the effective `keep_recent` above this to cover the
/// current user turn forward (see `stream_to_provider`), so the question and
/// the tool loop's own appends are always retained; this constant only sets the
/// minimum recent context kept for an ordinary (non-tool-loop) turn. An
/// oversized protected message is kept WHOLE (budget is a target, not a cap).
pub(crate) const KEEP_RECENT_MESSAGES: usize = 8;

/// Per-message overhead added to the content length to approximate role/framing
/// tokens the wire adds around each message.
const PER_MSG_OVERHEAD: usize = 8;

/// The outcome of a compaction pass over a model-facing history.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Compaction {
    /// The (possibly shrunk) history to actually send to the model.
    pub sent: Vec<ChatMessage>,
    /// The dropped middle messages, oldest-first — the signal Wave 3.5's
    /// pre-compaction flush consumes. Empty when nothing was trimmed.
    pub trimmed: Vec<ChatMessage>,
}

/// Estimated size (chars + framing) of one message.
fn msg_cost(m: &ChatMessage) -> usize {
    m.content.chars().count() + PER_MSG_OVERHEAD
}

/// Estimated total size of a slice of messages.
pub(crate) fn estimate_chars(msgs: &[ChatMessage]) -> usize {
    msgs.iter().map(msg_cost).sum()
}

/// The single deterministic elision marker inserted where the middle was cut.
fn marker_message(dropped: usize) -> ChatMessage {
    ChatMessage::system(format!(
        "[{dropped} earlier message(s) were omitted to fit the context window; \
         the full conversation is preserved in this chat's history.]"
    ))
}

/// Compact `history` to fit `budget_chars`, keeping the leading system-message
/// prefix and the most-recent `keep_recent` messages, dropping the oldest
/// middle messages (whole) and inserting one marker. Returns the history to
/// send plus the dropped middle (the 3.5 signal). Pure + deterministic.
pub(crate) fn compact_history(
    history: &[ChatMessage],
    budget_chars: usize,
    keep_recent: usize,
) -> Compaction {
    // Under budget ⇒ send byte-for-byte, pay nothing.
    if estimate_chars(history) <= budget_chars {
        return Compaction {
            sent: history.to_vec(),
            trimmed: Vec::new(),
        };
    }

    // The stable prefix: the leading contiguous run of system messages (tool
    // catalog + curated-summary block). Always kept, byte-identical.
    let prefix_len = history.iter().take_while(|m| m.role == "system").count();
    let (prefix, body) = history.split_at(prefix_len);

    // Never drop the last `keep_recent` body messages.
    let protected_start = body.len().saturating_sub(keep_recent);

    // Suffix costs: cost of body[i..] for each i (so the fit check is O(1)).
    let mut suffix_cost = vec![0usize; body.len() + 1];
    for i in (0..body.len()).rev() {
        suffix_cost[i] = suffix_cost[i + 1] + msg_cost(&body[i]);
    }
    let prefix_cost = estimate_chars(prefix);

    // Smallest cut in 0..=protected_start whose kept remainder (+ prefix +
    // marker) fits. If none fits, drop everything droppable (cut =
    // protected_start) and accept an over-budget send (recent turns are never
    // trimmed).
    let mut cut = protected_start;
    for candidate in 0..=protected_start {
        let marker_cost = if candidate == 0 {
            0
        } else {
            msg_cost(&marker_message(candidate))
        };
        if prefix_cost + marker_cost + suffix_cost[candidate] <= budget_chars {
            cut = candidate;
            break;
        }
    }

    // Nothing droppable actually helps (or nothing droppable at all) ⇒ send
    // as-is, over budget, rather than a misleading marker with no trim.
    if cut == 0 {
        return Compaction {
            sent: history.to_vec(),
            trimmed: Vec::new(),
        };
    }

    let trimmed = body[..cut].to_vec();
    let mut sent = Vec::with_capacity(prefix.len() + 1 + (body.len() - cut));
    sent.extend_from_slice(prefix);
    sent.push(marker_message(cut));
    sent.extend_from_slice(&body[cut..]);
    Compaction { sent, trimmed }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sys(s: &str) -> ChatMessage {
        ChatMessage::system(s)
    }
    fn user(s: &str) -> ChatMessage {
        ChatMessage::user(s)
    }
    fn asst(s: &str) -> ChatMessage {
        ChatMessage::assistant(s)
    }
    /// A message whose content is `n` chars (so cost = n + overhead).
    fn sized(role: &str, n: usize) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: "x".repeat(n),
        }
    }

    #[test]
    fn estimate_counts_scalars_plus_overhead() {
        assert_eq!(msg_cost(&user("")), PER_MSG_OVERHEAD);
        assert_eq!(msg_cost(&user("abcd")), 4 + PER_MSG_OVERHEAD);
        // Multi-byte char counts as one scalar, not its byte length.
        assert_eq!(msg_cost(&user("é")), 1 + PER_MSG_OVERHEAD);
    }

    #[test]
    fn under_budget_is_identity() {
        let h = vec![sys("catalog"), sys("summary"), user("hi"), asst("hello")];
        let c = compact_history(&h, 10_000, 8);
        assert_eq!(c.sent, h, "under budget ⇒ byte-for-byte");
        assert!(c.trimmed.is_empty());
    }

    #[test]
    fn exactly_at_budget_is_not_trimmed() {
        let h = vec![sized("system", 100), sized("user", 100)];
        let budget = estimate_chars(&h); // exactly at budget
        let c = compact_history(&h, budget, 8);
        assert!(
            c.trimmed.is_empty(),
            "strict > boundary: == budget is a no-op"
        );
        assert_eq!(c.sent, h);
    }

    #[test]
    fn over_budget_drops_oldest_middle_keeps_prefix_and_recent_with_one_marker() {
        // Prefix (2 system) + 10 user turns of 100 chars each. keep_recent=3.
        let mut h = vec![sys("catalog"), sys("summary")];
        for i in 0..10 {
            h.push(sized("user", 100).with_marker(i));
        }
        // Budget only fits prefix + a couple of turns → forces trimming.
        let c = compact_history(&h, 600, 3);
        // Prefix preserved byte-identical.
        assert_eq!(&c.sent[0], &h[0]);
        assert_eq!(&c.sent[1], &h[1]);
        // Exactly one marker, right after the prefix.
        assert_eq!(c.sent[2].role, "system");
        assert!(c.sent[2].content.contains("omitted"));
        // The last 3 turns are always kept (tail protection).
        let last3: Vec<_> = h[h.len() - 3..].to_vec();
        let sent_tail: Vec<_> = c.sent[c.sent.len() - 3..].to_vec();
        assert_eq!(
            sent_tail, last3,
            "the most-recent keep_recent turns survive"
        );
        // trimmed = the dropped middle, oldest-first, disjoint from sent.
        assert!(!c.trimmed.is_empty());
        assert_eq!(c.trimmed[0], h[2], "oldest dropped first");
        // Result is within budget (a marker replaced the dropped middle).
        assert!(estimate_chars(&c.sent) <= 600 + msg_cost(&marker_message(c.trimmed.len())));
    }

    #[test]
    fn dropped_plus_kept_equals_original_body_no_reorder_no_loss() {
        let mut h = vec![sys("s")];
        for i in 0..6 {
            h.push(sized("user", 50).with_marker(i));
        }
        let c = compact_history(&h, 100, 2);
        // Reconstruct body order: trimmed ++ (sent without prefix and marker).
        let kept_body: Vec<_> = c
            .sent
            .iter()
            .filter(|m| !(m.role == "system"))
            .cloned()
            .collect();
        let mut rebuilt = c.trimmed.clone();
        rebuilt.extend(kept_body);
        let original_body: Vec<_> = h[1..].to_vec();
        assert_eq!(
            rebuilt, original_body,
            "no message lost, duplicated, or reordered"
        );
    }

    #[test]
    fn single_oversized_recent_turn_is_kept_whole_never_sliced() {
        // A single huge recent turn bigger than the budget must be kept intact.
        let h = vec![sys("s"), sized("user", 100_000)];
        let c = compact_history(&h, 1_000, 8);
        assert!(
            c.trimmed.is_empty(),
            "recent turn is protected, nothing to drop"
        );
        // Content never sliced.
        assert_eq!(c.sent, h);
        assert_eq!(c.sent[1].content.chars().count(), 100_000);
    }

    #[test]
    fn no_droppable_middle_returns_identity_not_a_bare_marker() {
        // Over budget but body.len() <= keep_recent ⇒ nothing to drop.
        let h = vec![sys("s"), sized("user", 50_000), sized("assistant", 50_000)];
        let c = compact_history(&h, 1_000, 8);
        assert!(c.trimmed.is_empty());
        assert!(
            !c.sent.iter().any(|m| m.content.contains("omitted")),
            "no misleading marker"
        );
        assert_eq!(c.sent, h);
    }

    #[test]
    fn a_large_keep_recent_pins_the_current_turn_through_a_deep_tool_loop() {
        // Regression for the review finding: the current user turn must survive
        // however many messages the tool loop appends after it. The caller sets
        // keep_recent = len - pinned_from to cover the question forward.
        let mut h = vec![sys("catalog"), sys("summary")];
        for i in 0..8 {
            h.push(sized("user", 300).with_marker(i)); // old prior turns
        }
        let question = user("THE ACTUAL QUESTION");
        let pinned_from = h.len(); // index of the current turn
        h.push(question.clone());
        // Simulate a deep tool loop appending 12 large messages after the turn.
        for i in 0..12 {
            h.push(sized("assistant", 2_000).with_marker(i));
            h.push(sized("user", 2_000).with_marker(i)); // big tool feedback
        }
        let keep_recent = KEEP_RECENT_MESSAGES.max(h.len() - pinned_from);
        let c = compact_history(&h, 24_000, keep_recent);
        // The question is never trimmed, whatever the budget pressure.
        assert!(
            c.sent.iter().any(|m| m.content == "THE ACTUAL QUESTION"),
            "the pinned current turn must survive a deep tool loop"
        );
        assert!(
            !c.trimmed.iter().any(|m| m.content == "THE ACTUAL QUESTION"),
            "the question must never be in the trimmed set"
        );
        // Older prior turns before the question DID get trimmed (budget pressure).
        assert!(!c.trimmed.is_empty(), "older prior turns are still trimmed");
    }

    #[test]
    fn is_deterministic() {
        let mut h = vec![sys("catalog"), sys("summary")];
        for i in 0..20 {
            h.push(sized("user", 200).with_marker(i));
            h.push(sized("assistant", 200).with_marker(i));
        }
        let a = compact_history(&h, 3_000, 4);
        let b = compact_history(&h, 3_000, 4);
        assert_eq!(a, b, "same input ⇒ byte-identical output (no clock/rng)");
    }

    #[test]
    fn prefix_of_only_leading_system_messages() {
        // A system message that appears AFTER a user turn is body, not prefix —
        // so it can be trimmed like any other middle message.
        let mut h = vec![sys("catalog"), sys("summary"), user("first")];
        h.push(sys("a stray mid-history system note")); // body, not prefix
        for i in 0..10 {
            h.push(sized("user", 200).with_marker(i));
        }
        let c = compact_history(&h, 800, 2);
        // Only the first two system messages are the guaranteed prefix.
        assert_eq!(c.sent[0].content, "catalog");
        assert_eq!(c.sent[1].content, "summary");
        // The marker sits right after the 2-message prefix.
        assert!(c.sent[2].content.contains("omitted"));
    }

    // Tiny test helper: tag a sized message so equal-length messages stay
    // distinguishable in order assertions.
    trait WithMarker {
        fn with_marker(self, i: usize) -> Self;
    }
    impl WithMarker for ChatMessage {
        fn with_marker(mut self, i: usize) -> Self {
            self.content = format!("{i}:{}", self.content);
            self
        }
    }
}
