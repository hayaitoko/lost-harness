//! macOS Accessibility + Quartz implementation for the semantic `ui_*` tools.
//!
//! Accessibility is used only to re-resolve the app/role/label tuple and obtain
//! fresh bounds. Quartz synthesizes pointer and scroll events at those fresh
//! bounds. Text/key events use System Events after the target has been focused.
//! No coordinate is ever accepted from a model, and every failed resolution is
//! reported before an input event is posted.

use std::process::Command;
use std::thread;
use std::time::Duration;

use core_graphics::event::{
    CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, KeyCode, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;

use crate::tools::computer_backend::{ComputerBackend, CuError, ResolvedElement, UiNode, UiPoint};
use crate::tools::computer_use::{ActionTarget, ComputerAction};

/// The concrete backend registered by default on macOS builds.
pub struct MacOsComputerBackend;

impl MacOsComputerBackend {
    pub fn new() -> Self {
        Self
    }
}

const RESOLVE_SCRIPT: &str = r#"
on normalizedRole(rawRole)
    if rawRole is "AXButton" then return "button"
    if rawRole is "AXTextField" then return "textField"
    if rawRole is "AXTextArea" then return "textArea"
    if rawRole is "AXScrollArea" then return "scrollArea"
    if rawRole is "AXMenuItem" then return "menuItem"
    if rawRole is "AXMenuButton" then return "menuButton"
    if rawRole is "AXPopUpButton" then return "popUpButton"
    if rawRole is "AXCheckBox" then return "checkBox"
    if rawRole is "AXRadioButton" then return "radioButton"
    if rawRole is "AXStaticText" then return "staticText"
    if rawRole is "AXWindow" then return "window"
    if rawRole is "AXGroup" then return "group"
    if rawRole is "AXRow" then return "row"
    if rawRole is "AXCell" then return "cell"
    if rawRole is "AXLink" then return "link"
    if rawRole is "AXToolbar" then return "toolbar"
    return rawRole
end normalizedRole

on candidateLabel(theElement)
    try
        set elementName to name of theElement as text
        if elementName is not "" then return elementName
    end try
    try
        set elementDescription to description of theElement as text
        if elementDescription is not "" then return elementDescription
    end try
    try
        return title of theElement as text
    end try
    return ""
end candidateLabel

on locate(appName, wantedRole, wantedLabel)
    tell application "System Events"
        if not (exists process appName) then return missing value
        tell process appName
            set candidates to windows
            try
                set candidates to candidates & (entire contents of windows)
            end try
            repeat with candidate in candidates
                try
                    set actualRole to my normalizedRole(role of candidate as text)
                    set actualLabel to my candidateLabel(candidate)
                    if actualRole is wantedRole and actualLabel is wantedLabel then return candidate
                end try
            end repeat
        end tell
    end tell
    return missing value
end locate

on run argv
    set appName to item 1 of argv
    set wantedRole to item 2 of argv
    set wantedLabel to item 3 of argv
    set targetElement to my locate(appName, wantedRole, wantedLabel)
    if targetElement is missing value then return "NOT_FOUND"
    try
        set p to position of targetElement
        set s to size of targetElement
        return "FOUND" & tab & (item 1 of p as text) & tab & (item 2 of p as text) & tab & (item 1 of s as text) & tab & (item 2 of s as text)
    on error
        return "NOT_FOUND"
    end try
end run
"#;

const FRONTMOST_APP_SCRIPT: &str = r#"
tell application "System Events"
    set frontProcess to first process whose frontmost is true
    return name of frontProcess as text
end tell
"#;

const TYPE_SCRIPT: &str = r#"
on run argv
    tell application "System Events"
        keystroke (item 1 of argv)
    end tell
end run
"#;

const KEY_SCRIPT: &str = r#"
on run argv
    set keyKind to item 1 of argv
    set keyValue to item 2 of argv
    set useCommand to item 3 of argv is "1"
    set useControl to item 4 of argv is "1"
    set useOption to item 5 of argv is "1"
    set useShift to item 6 of argv is "1"
    set modifierList to {}
    if useCommand then set modifierList to modifierList & {command down}
    if useControl then set modifierList to modifierList & {control down}
    if useOption then set modifierList to modifierList & {option down}
    if useShift then set modifierList to modifierList & {shift down}
    tell application "System Events"
        if keyKind is "keycode" then
            key code (keyValue as integer) using modifierList
        else
            keystroke keyValue using modifierList
        end if
    end tell
end run
"#;

fn checked_arg<'a>(value: &'a str, what: &str) -> Result<&'a str, CuError> {
    if value.is_empty() || value.len() > 512 || value.contains('\0') {
        return Err(CuError::Failed(format!(
            "invalid {what} for macOS Accessibility"
        )));
    }
    Ok(value)
}

fn command_error(output: &std::process::Output) -> CuError {
    let raw = String::from_utf8_lossy(&output.stderr);
    let detail = raw.trim().chars().take(400).collect::<String>();
    let lower = detail.to_ascii_lowercase();
    if lower.contains("not allowed assistive")
        || lower.contains("accessibility")
        || lower.contains("not authorized")
        || lower.contains("permission")
    {
        CuError::PermissionDenied(
            "grant Lost Harness Accessibility access in System Settings > Privacy & Security > Accessibility"
                .into(),
        )
    } else {
        CuError::Failed(if detail.is_empty() {
            "macOS automation command failed".into()
        } else {
            detail
        })
    }
}

fn osascript(script: &str, args: &[&str]) -> Result<String, CuError> {
    let output = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .args(args)
        .output()
        .map_err(|e| CuError::Failed(format!("could not start macOS automation: {e}")))?;
    if !output.status.success() {
        return Err(command_error(&output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn event_source() -> Result<CGEventSource, CuError> {
    CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
        .map_err(|_| CuError::PermissionDenied("macOS did not permit input event creation".into()))
}

fn post_mouse(
    source: &CGEventSource,
    event_type: CGEventType,
    point: UiPoint,
) -> Result<(), CuError> {
    let event = CGEvent::new_mouse_event(
        source.clone(),
        event_type,
        CGPoint::new(point.x, point.y),
        CGMouseButton::Left,
    )
    .map_err(|_| CuError::Failed("could not create a macOS pointer event".into()))?;
    event.post(CGEventTapLocation::HID);
    Ok(())
}

fn click(point: UiPoint) -> Result<(), CuError> {
    let source = event_source()?;
    post_mouse(&source, CGEventType::LeftMouseDown, point)?;
    thread::sleep(Duration::from_millis(20));
    post_mouse(&source, CGEventType::LeftMouseUp, point)
}

fn drag(from: UiPoint, to: UiPoint) -> Result<(), CuError> {
    let source = event_source()?;
    post_mouse(&source, CGEventType::LeftMouseDown, from)?;
    thread::sleep(Duration::from_millis(20));
    post_mouse(&source, CGEventType::LeftMouseDragged, to)?;
    thread::sleep(Duration::from_millis(20));
    post_mouse(&source, CGEventType::LeftMouseUp, to)
}

fn scroll(point: UiPoint) -> Result<(), CuError> {
    let source = event_source()?;
    post_mouse(&source, CGEventType::MouseMoved, point)?;
    let event = CGEvent::new_scroll_event(source, ScrollEventUnit::LINE, 1, -3, 0, 0)
        .map_err(|_| CuError::Failed("could not create a macOS scroll event".into()))?;
    event.post(CGEventTapLocation::HID);
    Ok(())
}

#[derive(Debug)]
struct KeySpec {
    kind: &'static str,
    value: String,
    command: bool,
    control: bool,
    option: bool,
    shift: bool,
}

fn parse_key_spec(keys: &str) -> Result<KeySpec, CuError> {
    let mut command = false;
    let mut control = false;
    let mut option = false;
    let mut shift = false;
    let mut base = None;
    for segment in keys.split('+').map(str::trim).filter(|s| !s.is_empty()) {
        match segment.to_ascii_lowercase().as_str() {
            "cmd" | "command" => command = true,
            "ctrl" | "control" => control = true,
            "alt" | "option" => option = true,
            "shift" => shift = true,
            value if base.is_none() => base = Some(value.to_string()),
            _ => {
                return Err(CuError::Failed(
                    "a key chord has more than one base key".into(),
                ))
            }
        }
    }
    let base =
        base.ok_or_else(|| CuError::Failed("a key chord needs a base key, e.g. cmd+s".into()))?;
    let code = match base.as_str() {
        "enter" | "return" => Some(KeyCode::RETURN),
        "tab" => Some(KeyCode::TAB),
        "space" => Some(KeyCode::SPACE),
        "delete" | "backspace" => Some(KeyCode::DELETE),
        "escape" | "esc" => Some(KeyCode::ESCAPE),
        "left" | "leftarrow" => Some(KeyCode::LEFT_ARROW),
        "right" | "rightarrow" => Some(KeyCode::RIGHT_ARROW),
        "up" | "uparrow" => Some(KeyCode::UP_ARROW),
        "down" | "downarrow" => Some(KeyCode::DOWN_ARROW),
        _ => None,
    };
    let (kind, value) = if let Some(code) = code {
        ("keycode", code.to_string())
    } else if base.len() == 1 && base.bytes().all(|b| b.is_ascii_alphanumeric()) {
        ("text", base)
    } else {
        return Err(CuError::Failed(
            "unsupported key chord; use a letter/digit or enter, tab, escape, delete, or an arrow key"
                .into(),
        ));
    };
    Ok(KeySpec {
        kind,
        value,
        command,
        control,
        option,
        shift,
    })
}

impl ComputerBackend for MacOsComputerBackend {
    fn ui_tree(&self) -> Result<UiNode, CuError> {
        let app = osascript(FRONTMOST_APP_SCRIPT, &[])?;
        if app.is_empty() {
            return Err(CuError::NotFound);
        }
        // This endpoint is intentionally a conservative root node. Actions
        // always use `resolve`, which scans the live AX tree at actuation time.
        Ok(UiNode {
            app,
            role: "application".into(),
            label: "frontmost".into(),
            children: vec![],
        })
    }

    fn resolve(&self, locator: &ActionTarget) -> Result<Option<ResolvedElement>, CuError> {
        let app = checked_arg(&locator.app, "application")?;
        let role = checked_arg(&locator.role, "role")?;
        let label = checked_arg(&locator.label, "label")?;
        let response = osascript(RESOLVE_SCRIPT, &[app, role, label])?;
        if response == "NOT_FOUND" {
            return Ok(None);
        }
        let parts = response.split('\t').collect::<Vec<_>>();
        if parts.len() != 5 || parts[0] != "FOUND" {
            return Err(CuError::Failed(
                "macOS Accessibility returned an invalid element location".into(),
            ));
        }
        let number = |index: usize| {
            parts[index].replace(',', ".").parse::<f64>().map_err(|_| {
                CuError::Failed("macOS Accessibility returned non-numeric element bounds".into())
            })
        };
        let (x, y, width, height) = (number(1)?, number(2)?, number(3)?, number(4)?);
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return Ok(None);
        }
        Ok(Some(ResolvedElement {
            app: locator.app.clone(),
            role: locator.role.clone(),
            label: locator.label.clone(),
            center: Some(UiPoint {
                x: x + width / 2.0,
                y: y + height / 2.0,
            }),
        }))
    }

    fn synthesize(&self, action: &ComputerAction, elem: &ResolvedElement) -> Result<(), CuError> {
        let center = elem.center.ok_or_else(|| {
            CuError::Failed("resolved element did not have usable on-screen bounds".into())
        })?;
        match action {
            ComputerAction::Click { .. } => click(center),
            ComputerAction::Scroll { .. } => scroll(center),
            ComputerAction::Type { text, .. } => {
                checked_arg(text, "text")?;
                click(center)?;
                osascript(TYPE_SCRIPT, &[text]).map(|_| ())
            }
            ComputerAction::Key { keys, .. } => {
                let key = parse_key_spec(keys)?;
                click(center)?;
                osascript(
                    KEY_SCRIPT,
                    &[
                        key.kind,
                        &key.value,
                        if key.command { "1" } else { "0" },
                        if key.control { "1" } else { "0" },
                        if key.option { "1" } else { "0" },
                        if key.shift { "1" } else { "0" },
                    ],
                )
                .map(|_| ())
            }
            ComputerAction::Drag { from, to } => {
                let from = self.resolve(from)?.ok_or(CuError::NotFound)?;
                let to = self.resolve(to)?.ok_or(CuError::NotFound)?;
                let from_center = from.center.ok_or(CuError::NotFound)?;
                let to_center = to.center.ok_or(CuError::NotFound)?;
                drag(from_center, to_center)
            }
            ComputerAction::ReadUiTree
            | ComputerAction::CaptureScreen
            | ComputerAction::ReadClipboard => Err(CuError::Unavailable),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_chords_are_parsed_without_shell_interpolation() {
        let key = parse_key_spec("cmd+shift+s").unwrap();
        assert_eq!(key.kind, "text");
        assert_eq!(key.value, "s");
        assert!(key.command && key.shift);
        assert!(parse_key_spec("cmd+invalid key").is_err());
        assert!(parse_key_spec("cmd+q+z").is_err());
    }
}
