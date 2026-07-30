//! Tool registry tests: capability filtering, multi-capability `requires`,
//! empty-requirements tools, and lookup by name.

use super::*;

fn registry() -> ToolRegistry {
    let mut r = ToolRegistry::new();
    r.register(Box::new(EchoTool));
    r.register(Box::new(ScreenshotTool));
    r.register(Box::new(SyncFileTool));
    r
}

fn allow(names: &[&str]) -> std::collections::HashSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

// ── Wave 4.3b — bounded toolbelt intersection (a SECURITY boundary) ───────────

#[test]
fn restricted_to_excludes_every_tool_not_in_the_allowlist() {
    // A persona allowed only "echo" cannot SEE or LOOK UP the others — they're
    // physically absent from the sub-registry, so neither the catalog
    // (available_tools) nor a direct dispatch (get) can reach them.
    let sub = registry().restricted_to(&allow(&["echo"]));
    assert!(sub.get("echo").is_some());
    assert!(
        sub.get("screenshot").is_none(),
        "a tool outside the belt is not lookupable"
    );
    assert!(sub.get("sync_file").is_none());
    let names: Vec<String> = sub
        .available_tools(&BodyEnv::app_default())
        .into_iter()
        .map(|t| t.name().to_string())
        .collect();
    assert_eq!(names, vec!["echo"], "only the allowed tool is listable");
}

#[test]
fn restricted_to_is_an_intersection_never_a_widening() {
    // An allowlist naming a tool that isn't registered yields nothing for it —
    // a persona can never gain a capability the parent body doesn't have.
    let sub = registry().restricted_to(&allow(&["echo", "shell_exec", "ghost_tool"]));
    assert!(sub.get("echo").is_some());
    assert!(
        sub.get("shell_exec").is_none(),
        "not registered → not granted"
    );
    assert!(sub.get("ghost_tool").is_none());
    assert_eq!(
        sub.len(),
        1,
        "only the intersection with the registered set survives"
    );
}

#[test]
fn restricted_to_still_applies_the_env_capability_filter() {
    // Being in the allowlist is necessary but not sufficient: a tool still needs
    // its capabilities satisfied by the environment. sync_file (Filesystem+
    // Network) is allowed but an env with neither can't offer it.
    let sub = registry().restricted_to(&allow(&["echo", "sync_file"]));
    let bare = BodyEnv::empty();
    let names: Vec<String> = sub
        .available_tools(&bare)
        .into_iter()
        .map(|t| t.name().to_string())
        .collect();
    assert_eq!(
        names,
        vec!["echo"],
        "sync_file is allowed but ungranted by this env"
    );
    // With the capabilities present, the allowed tool becomes available.
    let full = BodyEnv::new([Capability::Filesystem, Capability::Network]);
    let names2: Vec<String> = sub
        .available_tools(&full)
        .into_iter()
        .map(|t| t.name().to_string())
        .collect();
    assert!(names2.iter().any(|n| n == "sync_file") && names2.iter().any(|n| n == "echo"));
}

#[test]
fn restricted_to_empty_allowlist_yields_an_empty_belt() {
    let sub = registry().restricted_to(&allow(&[]));
    assert_eq!(sub.len(), 0);
    assert!(sub.available_tools(&BodyEnv::app_default()).is_empty());
}

#[test]
fn no_requirements_tool_available_everywhere() {
    let r = registry();
    let empty_env = BodyEnv::empty();
    let names: Vec<String> = r
        .available_tools(&empty_env)
        .into_iter()
        .map(|t| t.name().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n == "echo"),
        "echo has no requirements, should always be available"
    );
    assert!(!names.iter().any(|n| n == "screenshot"));
    assert!(!names.iter().any(|n| n == "sync_file"));
}

#[test]
fn display_tool_hidden_on_no_display_env() {
    let r = registry();
    // Headless-server-shaped env: no Display capability.
    let headless = BodyEnv::headless_server_default();
    let names: Vec<String> = r
        .available_tools(&headless)
        .into_iter()
        .map(|t| t.name().to_string())
        .collect();
    assert!(
        !names.iter().any(|n| n == "screenshot"),
        "a Display-requiring tool must be hidden on a no-Display env, got {names:?}"
    );
    // Filesystem+Network tool should still be available server-side.
    assert!(names.iter().any(|n| n == "sync_file"));
    assert!(names.iter().any(|n| n == "echo"));
}

#[test]
fn display_tool_available_on_app_env() {
    let r = registry();
    let app_env = BodyEnv::app_default();
    let names: Vec<String> = r
        .available_tools(&app_env)
        .into_iter()
        .map(|t| t.name().to_string())
        .collect();
    assert!(names.iter().any(|n| n == "screenshot"));
    assert!(names.iter().any(|n| n == "sync_file"));
    assert!(names.iter().any(|n| n == "echo"));
}

#[test]
fn multi_capability_requires_is_set_intersection_not_any_of() {
    // SyncFileTool needs BOTH Filesystem and Network. An env with only one
    // of the two must not be enough.
    let only_fs = BodyEnv::new([Capability::Filesystem]);
    let only_net = BodyEnv::new([Capability::Network]);
    let both = BodyEnv::new([Capability::Filesystem, Capability::Network]);

    let tool = SyncFileTool;
    assert!(!tool.available(&only_fs));
    assert!(!tool.available(&only_net));
    assert!(tool.available(&both));
}

#[test]
fn get_by_name_finds_registered_tool_regardless_of_env() {
    let r = registry();
    assert!(r.get("screenshot").is_some());
    assert!(r.get("nonexistent_tool").is_none());
}

#[test]
fn registry_len_and_is_empty() {
    let empty = ToolRegistry::new();
    assert!(empty.is_empty());
    assert_eq!(empty.len(), 0);

    let r = registry();
    assert!(!r.is_empty());
    assert_eq!(r.len(), 3);
}

#[tokio::test]
async fn echo_tool_run_returns_input_args() {
    let tool = EchoTool;
    let ctx = ExecCtx::default();
    let input = ToolInput::new(serde_json::json!({"hello": "world"}));
    let result = tool.run(input, &ctx).await;
    match result {
        ToolResult::Ok(v) => assert_eq!(v, serde_json::json!({"hello": "world"})),
        ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
    }
}

#[test]
fn body_env_has_and_has_all() {
    let env = BodyEnv::new([Capability::Filesystem, Capability::Network]);
    assert!(env.has(Capability::Filesystem));
    assert!(!env.has(Capability::Display));
    assert!(env.has_all(&[Capability::Filesystem, Capability::Network]));
    assert!(!env.has_all(&[Capability::Filesystem, Capability::Display]));
}
