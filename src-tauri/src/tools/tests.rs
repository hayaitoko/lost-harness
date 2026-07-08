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

#[test]
fn no_requirements_tool_available_everywhere() {
    let r = registry();
    let empty_env = BodyEnv::empty();
    let names: Vec<&str> = r
        .available_tools(&empty_env)
        .into_iter()
        .map(|t| t.name())
        .collect();
    assert!(names.contains(&"echo"), "echo has no requirements, should always be available");
    assert!(!names.contains(&"screenshot"));
    assert!(!names.contains(&"sync_file"));
}

#[test]
fn display_tool_hidden_on_no_display_env() {
    let r = registry();
    // Headless-server-shaped env: no Display capability.
    let headless = BodyEnv::headless_server_default();
    let names: Vec<&str> = r
        .available_tools(&headless)
        .into_iter()
        .map(|t| t.name())
        .collect();
    assert!(
        !names.contains(&"screenshot"),
        "a Display-requiring tool must be hidden on a no-Display env, got {names:?}"
    );
    // Filesystem+Network tool should still be available server-side.
    assert!(names.contains(&"sync_file"));
    assert!(names.contains(&"echo"));
}

#[test]
fn display_tool_available_on_app_env() {
    let r = registry();
    let app_env = BodyEnv::app_default();
    let names: Vec<&str> = r
        .available_tools(&app_env)
        .into_iter()
        .map(|t| t.name())
        .collect();
    assert!(names.contains(&"screenshot"));
    assert!(names.contains(&"sync_file"));
    assert!(names.contains(&"echo"));
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
