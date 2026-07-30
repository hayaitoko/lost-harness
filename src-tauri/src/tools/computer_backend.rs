//! C6 / M5 (logic half) — the **ComputerBackend seam**: the trait the `ui_*`
//! act tools and the [`crate::hooks::OnScreenActionHook`] drive, plus the mock
//! that makes the whole slice `cargo test --lib`-provable. macOS supplies a
//! production Accessibility-backed implementation in
//! [`crate::platform::macos`]. Other platforms deliberately use
//! [`UnavailableBackend`] until they have an equivalent native implementation.
//!
//! Locators are SEMANTIC (`app`/`role`/`label` — see
//! [`crate::tools::computer_use::ActionTarget`]), never pixels and never an
//! opaque node id: the fingerprint that gates an action is computed from the
//! tool args BEFORE the hook chain runs, so the args must BE the stable
//! semantic identity (m5 design Revision v2, Fix 1).

use crate::tools::computer_use::ActionTarget;

/// One node of the (foreground-app-scoped) accessibility tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiNode {
    pub app: String,
    pub role: String,
    pub label: String,
    pub children: Vec<UiNode>,
}

/// A locator re-resolved against a FRESH snapshot — what synthesis acts on.
/// No geometry here: bounds/HiDPI math is the native backend's private concern.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedElement {
    pub app: String,
    pub role: String,
    pub label: String,
    /// Geometry is discovered privately by the native backend, never accepted
    /// from tool arguments or exposed as an action fingerprint.
    pub center: Option<UiPoint>,
}

/// A native screen-space point. It exists only after Accessibility has freshly
/// resolved a semantic target; callers can never provide one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UiPoint {
    pub x: f64,
    pub y: f64,
}

/// Why a backend call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CuError {
    /// The locator matched nothing on a fresh snapshot (moved/vanished).
    NotFound,
    /// The OS denied the accessibility/input permission.
    PermissionDenied(String),
    /// No native backend is available for this platform/build.
    Unavailable,
    /// The accessibility service rejected an otherwise well-formed action.
    Failed(String),
}

impl std::fmt::Display for CuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CuError::NotFound => write!(
                f,
                "target not found on a fresh snapshot (moved or vanished)"
            ),
            CuError::PermissionDenied(why) => write!(f, "OS permission denied: {why}"),
            CuError::Unavailable => write!(
                f,
                "no native computer-use backend is available on this platform"
            ),
            CuError::Failed(why) => write!(f, "computer-use action failed: {why}"),
        }
    }
}

/// The backend seam. `resolve` re-finds a semantic locator against a FRESH
/// snapshot (the moved-target gate); `synthesize` actuates a resolved element.
/// Object-safe so the tools/hook hold `Arc<dyn ComputerBackend>`.
pub trait ComputerBackend: Send + Sync {
    /// A fresh accessibility-tree snapshot (foreground app scoped).
    fn ui_tree(&self) -> Result<UiNode, CuError>;
    /// Re-find `locator` by its semantic tuple (app, role, label) against a
    /// fresh snapshot. `Ok(None)` = moved/vanished — the caller refuses, never
    /// mis-clicks a stale position. An `Err` preserves permission/backend
    /// failures instead of misreporting them as a vanished target.
    fn resolve(&self, locator: &ActionTarget) -> Result<Option<ResolvedElement>, CuError>;
    /// Actuate: click/type/key/drag/scroll on a JUST-resolved element.
    fn synthesize(
        &self,
        action: &crate::tools::computer_use::ComputerAction,
        elem: &ResolvedElement,
    ) -> Result<(), CuError>;
}

/// The honest fallback on platforms without a native backend.
pub struct UnavailableBackend;

impl ComputerBackend for UnavailableBackend {
    fn ui_tree(&self) -> Result<UiNode, CuError> {
        Err(CuError::Unavailable)
    }
    fn resolve(&self, _locator: &ActionTarget) -> Result<Option<ResolvedElement>, CuError> {
        Err(CuError::Unavailable)
    }
    fn synthesize(
        &self,
        _action: &crate::tools::computer_use::ComputerAction,
        _elem: &ResolvedElement,
    ) -> Result<(), CuError> {
        Err(CuError::Unavailable)
    }
}

/// A scriptable mock for tests: a fixed flat element list + a "vanish" toggle
/// (simulating a target disappearing between the hook's re-resolve and the
/// tool's own re-resolve — the double-re-snapshot gate), plus a synthesis log
/// so tests assert exactly what was actuated.
#[cfg(test)]
pub struct MockComputerBackend {
    pub elements: parking_lot::Mutex<Vec<ResolvedElement>>,
    pub vanished: std::sync::atomic::AtomicBool,
    pub synthesized: parking_lot::Mutex<Vec<String>>,
}

#[cfg(test)]
impl MockComputerBackend {
    pub fn with_elements(elements: Vec<(&str, &str, &str)>) -> Self {
        Self {
            elements: parking_lot::Mutex::new(
                elements
                    .into_iter()
                    .map(|(app, role, label)| ResolvedElement {
                        app: app.into(),
                        role: role.into(),
                        label: label.into(),
                        center: None,
                    })
                    .collect(),
            ),
            vanished: std::sync::atomic::AtomicBool::new(false),
            synthesized: parking_lot::Mutex::new(Vec::new()),
        }
    }
    /// Simulate every target vanishing (a window closed, the app quit).
    pub fn vanish_all(&self) {
        self.vanished
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
impl ComputerBackend for MockComputerBackend {
    fn ui_tree(&self) -> Result<UiNode, CuError> {
        Ok(UiNode {
            app: "mock".into(),
            role: "window".into(),
            label: "root".into(),
            children: Vec::new(),
        })
    }
    fn resolve(&self, locator: &ActionTarget) -> Result<Option<ResolvedElement>, CuError> {
        if self.vanished.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(None);
        }
        Ok(self
            .elements
            .lock()
            .iter()
            .find(|e| e.app == locator.app && e.role == locator.role && e.label == locator.label)
            .cloned())
    }
    fn synthesize(
        &self,
        action: &crate::tools::computer_use::ComputerAction,
        elem: &ResolvedElement,
    ) -> Result<(), CuError> {
        self.synthesized.lock().push(format!(
            "{action:?} on {}/{}/{}",
            elem.app, elem.role, elem.label
        ));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_resolves_by_semantic_tuple_and_vanish_makes_targets_disappear() {
        let mock = MockComputerBackend::with_elements(vec![("Mail", "button", "Reply")]);
        let target = ActionTarget {
            app: "Mail".into(),
            role: "button".into(),
            label: "Reply".into(),
        };
        assert!(
            mock.resolve(&target).unwrap().is_some(),
            "present target resolves"
        );
        let missing = ActionTarget {
            app: "Mail".into(),
            role: "button".into(),
            label: "Nope".into(),
        };
        assert!(
            mock.resolve(&missing).unwrap().is_none(),
            "absent target doesn't"
        );
        mock.vanish_all();
        assert!(
            mock.resolve(&target).unwrap().is_none(),
            "a vanished target stops resolving"
        );
    }

    #[test]
    fn unavailable_backend_refuses_everything_loudly() {
        let b = UnavailableBackend;
        assert_eq!(b.ui_tree().unwrap_err(), CuError::Unavailable);
        let t = ActionTarget {
            app: "X".into(),
            role: "button".into(),
            label: "Y".into(),
        };
        assert_eq!(b.resolve(&t).unwrap_err(), CuError::Unavailable);
    }
}
