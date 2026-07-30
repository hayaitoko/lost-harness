//! Wave 5.1 / M5 — the computer-use **action model + reversibility classifier**:
//! the NON-native security core (per the m5 design's Revision v2). The native
//! backends (macOS AX/CGEvent/ScreenCaptureKit, Windows UIA/SendInput, Linux
//! AT-SPI/XTest) are the on-target build; what lands here is the part that
//! decides HOW HARD to gate an on-screen action — which is where the whole
//! "which pixel, reversible?" novelty lives, and it's pure logic, fully tested.
//!
//! Two ideas, both grounded in the real spine:
//! 1. **Actions are SEMANTIC, never pixels** ([`ActionTarget`] = app/role/label).
//!    So the existing `ActionFingerprint::of(name, args)` already hashes a stable
//!    semantic target — a click on a different control is a different fingerprint,
//!    with no `HookResult::Modify` and no fingerprint recompute.
//! 2. **Reversibility maps onto the EXISTING `RiskClass` matrix** — not a new
//!    parallel policy ([`risk_class`]): a reversible read/scroll is `Safe`
//!    (pre-trusted, no prompt); a consequential click is `External` (a
//!    fingerprint-pinned Session grant via `resolve_grant`); an irreversible
//!    click (Send/delete/buy/…) rides the existing `covers_once` floor so no
//!    standing grant can ever cover it (Once-only, human-present).

use crate::tools::{Capability, RiskClass};

/// A semantic locator for an on-screen element — NEVER pixel coordinates. Pixels
/// are computed inside the backend at synthesis time and re-verified against a
/// fresh snapshot; they never appear in a tool's args (so the fingerprint that
/// gates the action is a stable semantic target).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionTarget {
    /// The owning application (e.g. "Mail").
    pub app: String,
    /// The accessibility role (e.g. "button", "textField", "menuItem").
    pub role: String,
    /// The element's label / accessible name (e.g. "Send", "Reply").
    pub label: String,
}

/// A computer-use action the model can request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComputerAction {
    // ── Reversible (no discrete actuation) ──
    /// Read the on-screen accessibility tree.
    ReadUiTree,
    /// Capture the screen (routed LOCAL under Auto — the classifier can't label
    /// an image; see the design's Fix 3).
    CaptureScreen,
    /// Read the clipboard (guard-wrapped as untrusted content by the caller).
    ReadClipboard,
    /// Scroll — intrinsically reversible, no "sometimes irreversible" mode.
    Scroll {
        target: ActionTarget,
    },
    // ── Consequential/Irreversible (discrete actuation on a control) ──
    Click {
        target: ActionTarget,
    },
    Type {
        target: ActionTarget,
        text: String,
    },
    Key {
        target: ActionTarget,
        keys: String,
    },
    Drag {
        from: ActionTarget,
        to: ActionTarget,
    },
}

/// How reversible an action is — the axis the shell-command approval flow does
/// NOT capture ("which pixel, reversible?"). This is the M5-specific new axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reversibility {
    /// No side effect — pure reading / navigation.
    Reversible,
    /// Actuates a normal control; the DEFAULT for a click/keypress. Session-
    /// grantable per semantic target.
    Consequential,
    /// Actuates a control whose label is in the fail-safe verb set (Send, Delete,
    /// Buy, Pay, …) — Once-only, never coverable by a standing grant.
    Irreversible,
}

/// The fail-safe irreversible-verb set: if a click/keypress target's label
/// starts with (or equals) one of these, treat it as irreversible. Deliberately
/// OVER-broad (fail toward Once), case-insensitive, whole-word-ish.
const IRREVERSIBLE_VERBS: &[&str] = &[
    "send",
    "delete",
    "remove",
    "buy",
    "purchase",
    "pay",
    "order",
    "submit",
    "confirm",
    "post",
    "publish",
    "transfer",
    "withdraw",
    "trash",
    "erase",
    "discard",
    "empty trash",
    "move to trash",
    "sign out",
    "log out",
    "shut down",
    "restart",
    "unfriend",
    "unfollow",
    "block",
    "reply all",
];

fn label_is_irreversible(label: &str) -> bool {
    let l = label.trim().to_ascii_lowercase();
    IRREVERSIBLE_VERBS.iter().any(|v| {
        // whole-label match, or the label starts with the verb followed by a
        // word boundary (space/punct) — "Send", "Send Now", but not "Sender".
        l == *v
            || l.strip_prefix(v)
                .is_some_and(|rest| rest.starts_with(|c: char| !c.is_alphanumeric()))
    })
}

/// Classify an action's reversibility. Reads/scrolls are reversible; a discrete
/// actuation is irreversible when its target's label is in the fail-safe verb
/// set, else consequential.
pub fn reversibility(action: &ComputerAction) -> Reversibility {
    match action {
        ComputerAction::ReadUiTree
        | ComputerAction::CaptureScreen
        | ComputerAction::ReadClipboard
        | ComputerAction::Scroll { .. } => Reversibility::Reversible,
        ComputerAction::Click { target }
        | ComputerAction::Type { target, .. }
        | ComputerAction::Key { target, .. } => {
            if label_is_irreversible(&target.label) {
                Reversibility::Irreversible
            } else {
                Reversibility::Consequential
            }
        }
        // A drag is irreversible if EITHER endpoint is an irreversible target
        // (dropping onto Trash), else consequential.
        ComputerAction::Drag { from, to } => {
            if label_is_irreversible(&from.label) || label_is_irreversible(&to.label) {
                Reversibility::Irreversible
            } else {
                Reversibility::Consequential
            }
        }
    }
}

/// The static `RiskClass` an action's tool carries — mapping reversibility onto
/// the REAL matrix (design Fix 2), not a parallel policy:
/// - Reversible → `Safe` (pre-trusted; `lib.rs` whole-tool Allow → no prompt).
/// - Consequential/Irreversible → `External` (reaches beyond the machine; a
///   `resolve_grant(External, Session)` yields a fingerprint-pinned — never
///   whole-tool — session grant). Irreversibility is then enforced ON TOP by the
///   `covers_once` floor (`OnScreenActionHook`, the on-target slice), NOT by
///   bumping to `Dangerous` (which would forbid the per-target session grant the
///   consequential tier needs).
pub fn risk_class(action: &ComputerAction) -> RiskClass {
    match reversibility(action) {
        Reversibility::Reversible => RiskClass::Safe,
        Reversibility::Consequential | Reversibility::Irreversible => RiskClass::External,
    }
}

/// Every computer-use action needs the `ComputerUse` capability (absent on a
/// headless body → the tools are simply unavailable there).
pub fn required_capability() -> Capability {
    Capability::ComputerUse
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(app: &str, role: &str, label: &str) -> ActionTarget {
        ActionTarget {
            app: app.into(),
            role: role.into(),
            label: label.into(),
        }
    }
    fn click(label: &str) -> ComputerAction {
        ComputerAction::Click {
            target: target("Mail", "button", label),
        }
    }

    #[test]
    fn reads_and_scroll_are_reversible_safe() {
        for a in [
            ComputerAction::ReadUiTree,
            ComputerAction::CaptureScreen,
            ComputerAction::ReadClipboard,
            ComputerAction::Scroll {
                target: target("Mail", "scrollArea", "list"),
            },
        ] {
            assert_eq!(reversibility(&a), Reversibility::Reversible);
            assert_eq!(
                risk_class(&a),
                RiskClass::Safe,
                "reversible reads are Safe → no prompt"
            );
        }
    }

    #[test]
    fn a_normal_click_is_consequential_external() {
        let a = click("Reply");
        assert_eq!(reversibility(&a), Reversibility::Consequential);
        assert_eq!(
            risk_class(&a),
            RiskClass::External,
            "consequential → External (session-grantable per target)"
        );
    }

    #[test]
    fn an_irreversible_verb_click_is_irreversible() {
        for label in [
            "Send",
            "Send Now",
            "Delete",
            "Buy",
            "Pay",
            "Submit",
            "Move to Trash",
            "reply all",
        ] {
            assert_eq!(
                reversibility(&click(label)),
                Reversibility::Irreversible,
                "\"{label}\" is a fail-safe irreversible verb"
            );
        }
        // NOT a false match: "Sender" starts with "send" but isn't the verb.
        assert_eq!(
            reversibility(&click("Sender")),
            Reversibility::Consequential
        );
        assert_eq!(reversibility(&click("Reply")), Reversibility::Consequential);
    }

    #[test]
    fn drag_onto_trash_is_irreversible() {
        let a = ComputerAction::Drag {
            from: target("Finder", "file", "report.txt"),
            to: target("Finder", "button", "Trash"),
        };
        assert_eq!(reversibility(&a), Reversibility::Irreversible);
        // A benign drag is consequential.
        let benign = ComputerAction::Drag {
            from: target("App", "item", "A"),
            to: target("App", "list", "B"),
        };
        assert_eq!(reversibility(&benign), Reversibility::Consequential);
    }
}
