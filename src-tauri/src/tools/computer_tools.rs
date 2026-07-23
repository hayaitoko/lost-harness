//! C6 / M5 (logic half) — the `ui_*` act-tool skeletons over the
//! [`ComputerBackend`](crate::tools::computer_backend::ComputerBackend) seam.
//! Registered behind the `computer-use` cargo feature (compile-time gate) AND
//! only usable in an env granting [`Capability::ComputerUse`] (runtime gate) —
//! the design's "two independent absences must agree."
//!
//! Args are the SEMANTIC locator (`app`/`role`/`label`) — never pixels, never a
//! node id — so `ActionFingerprint::of(name, args)` (computed before the hook
//! chain) is already the stable semantic fingerprint the grant system pins.
//!
//! Risk mapping (m5 Revision v2, Fix 2): `ui_scroll` is `Safe` (reversible);
//! the actuating tools are `External` — a fingerprint-pinned Session grant per
//! semantic target via `resolve_grant`, with IRREVERSIBLE targets (Send/Delete/
//! Buy/…) enforced Once-only ON TOP by the `OnScreenActionHook`'s `covers_once`
//! floor. Deliberately NOT `Dangerous` (which would collapse every grant to
//! Once and break the consequential tier).
//!
//! `run()` re-resolves the locator against a FRESH snapshot immediately before
//! synthesis — the SECOND re-snapshot verify (the first is the hook's); a moved
//! or vanished target refuses, never a mis-click.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::json;

use crate::tools::computer_backend::ComputerBackend;
use crate::tools::computer_use::{ActionTarget, ComputerAction};
use crate::tools::{Capability, ExecCtx, RiskClass, Tool, ToolInput, ToolResult};

const CU_REQUIRES: &[Capability] = &[Capability::ComputerUse];

/// Parse a semantic target out of tool args (`app`/`role`/`label`, all
/// required non-empty strings). `prefix` distinguishes drag endpoints
/// (`from_app`… / `to_app`…).
fn parse_target(args: &serde_json::Value, prefix: &str) -> Result<ActionTarget, String> {
    let field = |k: &str| -> Result<String, String> {
        let key = if prefix.is_empty() { k.to_string() } else { format!("{prefix}_{k}") };
        match args.get(&key).and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
            _ => Err(format!("requires a non-empty string \"{key}\" arg (a semantic locator, never pixels)")),
        }
    };
    Ok(ActionTarget { app: field("app")?, role: field("role")?, label: field("label")? })
}

/// Build the [`ComputerAction`] a `ui_*` call describes. `None` for a non-ui
/// tool name or unparseable args — the hook treats that as "not mine".
pub(crate) fn parse_action(tool_name: &str, args: &serde_json::Value) -> Option<ComputerAction> {
    match tool_name {
        "ui_scroll" => Some(ComputerAction::Scroll { target: parse_target(args, "").ok()? }),
        "ui_click" => Some(ComputerAction::Click { target: parse_target(args, "").ok()? }),
        "ui_type" => Some(ComputerAction::Type {
            target: parse_target(args, "").ok()?,
            text: args.get("text")?.as_str()?.to_string(),
        }),
        "ui_key" => Some(ComputerAction::Key {
            target: parse_target(args, "").ok()?,
            keys: args.get("keys")?.as_str()?.to_string(),
        }),
        "ui_drag" => Some(ComputerAction::Drag {
            from: parse_target(args, "from").ok()?,
            to: parse_target(args, "to").ok()?,
        }),
        _ => None,
    }
}

/// The shared act-tool body: parse → fresh re-resolve → synthesize.
async fn act(
    backend: &Arc<dyn ComputerBackend>,
    tool_name: &str,
    args: &serde_json::Value,
) -> ToolResult {
    let Some(action) = parse_action(tool_name, args) else {
        return ToolResult::Err(format!(
            "{tool_name} requires a semantic locator: {{\"app\",\"role\",\"label\"}} (never pixel coordinates)"
        ));
    };
    // The primary locator to re-resolve (a drag re-resolves both endpoints).
    let targets: Vec<&ActionTarget> = match &action {
        ComputerAction::Scroll { target }
        | ComputerAction::Click { target }
        | ComputerAction::Type { target, .. }
        | ComputerAction::Key { target, .. } => vec![target],
        ComputerAction::Drag { from, to } => vec![from, to],
        _ => vec![],
    };
    let mut resolved = None;
    for t in &targets {
        match backend.resolve(t) {
            Some(r) => resolved = Some(r),
            None => {
                return ToolResult::Err(format!(
                    "target moved or vanished (\"{}\" {} in {}) — refusing to act on a stale position; re-read the screen first",
                    t.label, t.role, t.app
                ))
            }
        }
    }
    let Some(elem) = resolved else {
        return ToolResult::Err("no target to act on".to_string());
    };
    match backend.synthesize(&action, &elem) {
        Ok(()) => ToolResult::Ok(json!({
            "acted": tool_name,
            "app": elem.app,
            "role": elem.role,
            "label": elem.label,
        })),
        Err(e) => ToolResult::Err(e.to_string()),
    }
}

/// Declare one `ui_*` tool struct wrapping the shared body.
macro_rules! ui_tool {
    ($struct_name:ident, $tool_name:literal, $risk:expr, $desc:literal, $schema:expr) => {
        pub struct $struct_name {
            backend: Arc<dyn ComputerBackend>,
        }
        impl $struct_name {
            pub fn new(backend: Arc<dyn ComputerBackend>) -> Self {
                Self { backend }
            }
        }
        impl Tool for $struct_name {
            fn name(&self) -> &str {
                $tool_name
            }
            fn description(&self) -> &str {
                $desc
            }
            fn risk(&self) -> RiskClass {
                $risk
            }
            fn requires(&self) -> &[Capability] {
                CU_REQUIRES
            }
            fn schema(&self) -> serde_json::Value {
                $schema
            }
            fn run<'a>(
                &'a self,
                input: ToolInput,
                _ctx: &'a ExecCtx,
            ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
                Box::pin(async move { act(&self.backend, $tool_name, &input.args).await })
            }
        }
    };
}

fn locator_schema(extra: &[(&str, &str)]) -> serde_json::Value {
    let mut props = serde_json::Map::new();
    for (k, d) in [
        ("app", "the owning application, e.g. \"Mail\""),
        ("role", "the accessibility role, e.g. \"button\""),
        ("label", "the element's accessible label, e.g. \"Reply\""),
    ]
    .iter()
    .chain(extra)
    {
        props.insert((*k).to_string(), json!({"type": "string", "description": d}));
    }
    let required: Vec<&str> =
        ["app", "role", "label"].into_iter().chain(extra.iter().map(|(k, _)| *k)).collect();
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}

ui_tool!(
    UiScrollTool,
    "ui_scroll",
    RiskClass::Safe,
    "Scroll an on-screen element (reversible). args: {app, role, label} — a semantic locator, never pixels.",
    locator_schema(&[])
);
ui_tool!(
    UiClickTool,
    "ui_click",
    RiskClass::External,
    "Click an on-screen control. args: {app, role, label} — a semantic locator, never pixels.",
    locator_schema(&[])
);
ui_tool!(
    UiTypeTool,
    "ui_type",
    RiskClass::External,
    "Type text into an on-screen control. args: {app, role, label, text}.",
    locator_schema(&[("text", "the text to type")])
);
ui_tool!(
    UiKeyTool,
    "ui_key",
    RiskClass::External,
    "Press a key chord on an on-screen control. args: {app, role, label, keys}.",
    locator_schema(&[("keys", "the key chord, e.g. \"cmd+s\"")])
);

/// `ui_drag` has two locators, so it's hand-written rather than macro'd.
pub struct UiDragTool {
    backend: Arc<dyn ComputerBackend>,
}
impl UiDragTool {
    pub fn new(backend: Arc<dyn ComputerBackend>) -> Self {
        Self { backend }
    }
}
impl Tool for UiDragTool {
    fn name(&self) -> &str {
        "ui_drag"
    }
    fn description(&self) -> &str {
        "Drag one on-screen element onto another. args: {from_app, from_role, from_label, to_app, to_role, to_label}."
    }
    fn risk(&self) -> RiskClass {
        RiskClass::External
    }
    fn requires(&self) -> &[Capability] {
        CU_REQUIRES
    }
    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "from_app": {"type": "string"}, "from_role": {"type": "string"}, "from_label": {"type": "string"},
                "to_app": {"type": "string"}, "to_role": {"type": "string"}, "to_label": {"type": "string"}
            },
            "required": ["from_app", "from_role", "from_label", "to_app", "to_role", "to_label"],
            "additionalProperties": false
        })
    }
    fn run<'a>(
        &'a self,
        input: ToolInput,
        _ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move { act(&self.backend, "ui_drag", &input.args).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::computer_backend::MockComputerBackend;

    fn click_args(label: &str) -> serde_json::Value {
        json!({"app": "Mail", "role": "button", "label": label})
    }

    #[tokio::test]
    async fn ui_click_resolves_fresh_and_synthesizes() {
        let mock = Arc::new(MockComputerBackend::with_elements(vec![("Mail", "button", "Reply")]));
        let tool = UiClickTool::new(mock.clone() as Arc<dyn ComputerBackend>);
        let out = tool.run(ToolInput::new(click_args("Reply")), &ExecCtx::default()).await;
        match out {
            ToolResult::Ok(v) => assert_eq!(v["label"], "Reply"),
            other => panic!("expected an actuation, got {other:?}"),
        }
        assert_eq!(mock.synthesized.lock().len(), 1, "exactly one synthesis");
    }

    #[tokio::test]
    async fn a_vanished_target_refuses_never_misclicks() {
        let mock = Arc::new(MockComputerBackend::with_elements(vec![("Mail", "button", "Reply")]));
        mock.vanish_all();
        let tool = UiClickTool::new(mock.clone() as Arc<dyn ComputerBackend>);
        let out = tool.run(ToolInput::new(click_args("Reply")), &ExecCtx::default()).await;
        assert!(matches!(out, ToolResult::Err(ref e) if e.contains("moved or vanished")), "got {out:?}");
        assert!(mock.synthesized.lock().is_empty(), "NOTHING was actuated");
    }

    #[tokio::test]
    async fn pixel_args_are_rejected_semantic_locators_only() {
        let mock = Arc::new(MockComputerBackend::with_elements(vec![]));
        let tool = UiClickTool::new(mock as Arc<dyn ComputerBackend>);
        let out = tool.run(ToolInput::new(json!({"x": 100, "y": 250})), &ExecCtx::default()).await;
        assert!(matches!(out, ToolResult::Err(ref e) if e.contains("semantic locator")), "got {out:?}");
    }

    #[test]
    fn static_risks_match_the_design_matrix() {
        let mock = Arc::new(MockComputerBackend::with_elements(vec![]));
        let b = || mock.clone() as Arc<dyn ComputerBackend>;
        assert_eq!(UiScrollTool::new(b()).risk(), RiskClass::Safe);
        assert_eq!(UiClickTool::new(b()).risk(), RiskClass::External);
        assert_eq!(UiTypeTool::new(b()).risk(), RiskClass::External);
        assert_eq!(UiKeyTool::new(b()).risk(), RiskClass::External);
        assert_eq!(UiDragTool::new(b()).risk(), RiskClass::External);
        // Never Dangerous — that would collapse the consequential tier to Once.
    }

    #[test]
    fn parse_action_builds_the_semantic_action() {
        let a = parse_action("ui_click", &click_args("Send")).unwrap();
        assert!(matches!(a, ComputerAction::Click { ref target } if target.label == "Send"));
        assert!(parse_action("read_file", &json!({})).is_none(), "non-ui tools are not mine");
        let d = parse_action(
            "ui_drag",
            &json!({"from_app": "Finder", "from_role": "file", "from_label": "a.txt",
                    "to_app": "Finder", "to_role": "button", "to_label": "Trash"}),
        )
        .unwrap();
        assert!(matches!(d, ComputerAction::Drag { .. }));
    }
}
