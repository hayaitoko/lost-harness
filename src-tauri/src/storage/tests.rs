//! Storage layer integration tests.
//!
//! Covers the spec's required test cases:
//!   1. Open a fresh in-memory database, verify all tables exist
//!   2. Create a conversation, add messages, query them back
//!   3. Create a folder, assign a conversation to it, query by folder
//!   4. Create tag definitions, tag a conversation, query by tag
//!   5. Verify schema_version is 1 after init
//!
//! Plus a handful of extra coverage tests (CRUD round-trips, end-to-end
//! `Storage::open` with a tempdir) to catch regressions early.

use super::*;
use crate::storage::schema::{
    GLOBAL_SCHEMA_VERSION, GLOBAL_TABLES, PROFILE_SCHEMA_VERSION, PROFILE_TABLES,
};

// ─────────────────────────────────────────────────────────────────────────────
// 1. Fresh in-memory DB has every expected table
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn fresh_in_memory_global_has_all_tables() {
    let db = GlobalDb::open_in_memory().unwrap();
    let conn = db.raw();

    for table in GLOBAL_TABLES {
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type IN ('table', 'index') AND tbl_name = ?1",
                rusqlite::params![table],
                |r| r.get(0),
            )
            .unwrap_or(0);
        assert!(n >= 1, "expected table/index {table} in global.db");
    }
}

#[test]
fn fresh_in_memory_profile_has_all_tables() {
    let db = ProfileDb::open_in_memory("personal").unwrap();
    let conn = db.raw();

    for table in PROFILE_TABLES {
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type IN ('table', 'index') AND tbl_name = ?1",
                rusqlite::params![table],
                |r| r.get(0),
            )
            .unwrap_or(0);
        assert!(n >= 1, "expected table/index {table} in profile.db");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 5. schema_version lands at the expected version per store (global=1, profile=3)
//    (and survives reopening)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn schema_version_is_current_after_init_global() {
    // Global schema is now v8 (v2 memory buckets + FTS5; v3 private-bucket
    // vector table for the meaning lane; v4 endpoints.supports_native_tools;
    // v5 skills metadata columns — Wave 4.1; v6 agent_types — Wave 4.3;
    // v7 model_catalog.sha256+status — Wave 5.3 / M8; v8 mcp_servers — C3;
    // v9 mcp_servers.executable_path/_hash — H-07 binary pinning).
    let db = GlobalDb::open_in_memory().unwrap();
    let v: i32 = db
        .raw()
        .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, GLOBAL_SCHEMA_VERSION);
    assert_eq!(v, 9); // H-07 added the MCP executable pins (v9)
}

/// H-07 / migration v9: a FRESH database must end up with the pin columns.
/// This is the regression guard for the `ALTER TABLE ADD COLUMN` convention —
/// declaring these columns in GLOBAL_SCHEMA_SQL *as well* would make v9 fail
/// with "duplicate column name" on every new install.
#[test]
fn fresh_global_db_has_the_mcp_executable_pin_columns() {
    let db = GlobalDb::open_in_memory().unwrap();
    let cols: Vec<String> = db
        .raw()
        .prepare("PRAGMA table_info(mcp_servers)")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(cols.contains(&"executable_path".to_string()), "{cols:?}");
    assert!(cols.contains(&"executable_hash".to_string()), "{cols:?}");
}

/// H-07: the pins survive a persist -> read round-trip, and a row written
/// without them reads back as `None` (the pre-v9 shape bring-up refuses).
#[test]
fn mcp_server_executable_pins_round_trip() {
    use crate::storage::McpServerRow;
    let db = GlobalDb::open_in_memory().unwrap();
    let mut row = McpServerRow {
        id: "s1".into(),
        name: "pinned".into(),
        command: "sh".into(),
        args: vec![],
        tier: "local".into(),
        trusted_read_only: false,
        capabilities: vec![],
        enabled: true,
        created_at: 1,
        executable_path: Some("/bin/sh".into()),
        executable_hash: Some("deadbeef".into()),
    };
    db.insert_mcp_server(&row).unwrap();
    let back = db.get_mcp_server("s1").unwrap().expect("row persisted");
    assert_eq!(back.executable_path.as_deref(), Some("/bin/sh"));
    assert_eq!(back.executable_hash.as_deref(), Some("deadbeef"));

    row.id = "s2".into();
    row.name = "unpinned".into();
    row.executable_path = None;
    row.executable_hash = None;
    db.insert_mcp_server(&row).unwrap();
    let back2 = db.get_mcp_server("s2").unwrap().expect("row persisted");
    assert!(back2.executable_path.is_none());
    assert!(back2.executable_hash.is_none());
    assert_eq!(db.list_mcp_servers().unwrap().len(), 2);
}

#[test]
fn agent_types_crud_and_builtin_seed() {
    use crate::storage::{AgentType, AgentTypeApproval};
    let db = GlobalDb::open_in_memory().unwrap();
    assert!(db.list_agent_types().unwrap().is_empty());

    // Seeding is idempotent — two runs leave exactly the built-ins.
    db.ensure_builtin_agent_types(100).unwrap();
    let after_first = db.list_agent_types().unwrap();
    assert_eq!(after_first.len(), 2, "two built-ins seeded");
    db.ensure_builtin_agent_types(200).unwrap();
    assert_eq!(db.list_agent_types().unwrap().len(), 2, "re-seed is a no-op");
    // Built-ins are approved + source=builtin, with a non-empty allowlist.
    let reviewer = db.get_agent_type("builtin-code-reviewer").unwrap().unwrap();
    assert_eq!(reviewer.approval_status, AgentTypeApproval::Approved);
    assert_eq!(reviewer.source, "builtin");
    assert!(reviewer.tools_allowlist.contains(&"read_file".to_string()));
    assert_eq!(db.list_approved_agent_types().unwrap().len(), 2);

    // A user-authored type lands pending and is filtered from the approved set.
    db.insert_agent_type(&AgentType {
        id: "u1".into(),
        name: "My persona".into(),
        description: "d".into(),
        system_prompt: "sp".into(),
        tools_allowlist: vec!["read_file".into()],
        seat: "Coding".into(),
        trigger_examples: vec![],
        approval_status: AgentTypeApproval::Pending,
        source: "user".into(),
        created_at: 300,
    })
    .unwrap();
    assert_eq!(db.list_agent_types().unwrap().len(), 3);
    assert_eq!(db.list_approved_agent_types().unwrap().len(), 2, "pending is excluded");
    assert!(db.set_agent_type_approval("u1", AgentTypeApproval::Approved).unwrap());
    assert_eq!(db.list_approved_agent_types().unwrap().len(), 3);
    assert!(db.delete_agent_type("u1").unwrap());
    assert_eq!(db.list_agent_types().unwrap().len(), 2);
}

#[test]
fn schema_version_is_eleven_after_init_profile() {
    // Profile schema version is now 10 (v2 tool_audit, v3 tool_rules, v4
    // classifier_settings, v5 the classifier_settings.redaction_enabled column,
    // v6 memory_settings, v7 usage_events, v8 work_items, v9 seat_bindings,
    // v10 sandbox_config); global stays at its own version. Tracked independently.
    let db = ProfileDb::open_in_memory("personal").unwrap();
    let v: i32 = db
        .raw()
        .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, PROFILE_SCHEMA_VERSION);
    assert_eq!(v, 11); // C1 added budget_settings (v11)

    // sandbox_config round-trips (M7 Tier-K Slice 2): unset → None; set → Some.
    use crate::hooks::{SandboxConfig, SandboxNetworkConfig};
    assert!(db.get_sandbox_config().unwrap().is_none(), "no row → None (unconstrained default)");
    let cfg = SandboxConfig {
        enabled: true,
        auto_allow_if_sandboxed: false,
        excluded_commands: vec!["rm".into()],
        network: SandboxNetworkConfig {
            allowed_domains: vec!["example.com".into()],
            allow_localhost: false,
            allow_unix_sockets: vec![],
        },
    };
    db.set_sandbox_config(&cfg).unwrap();
    assert_eq!(db.get_sandbox_config().unwrap(), Some(cfg.clone()), "set → get round-trips exactly");
    // permits_shell_network: a non-empty allowlist lifts the ceiling.
    assert!(cfg.permits_shell_network());
    // A fully locked-down config denies shell network.
    let locked = SandboxConfig {
        network: SandboxNetworkConfig { allowed_domains: vec![], allow_localhost: false, allow_unix_sockets: vec![] },
        ..cfg
    };
    assert!(!locked.permits_shell_network(), "no localhost + no domains → shell network denied");
}

#[test]
fn model_catalog_carries_sha256_and_status() {
    use crate::storage::ModelEntry;
    let db = GlobalDb::open_in_memory().unwrap();
    db.insert_model(&ModelEntry {
        id: "qwen-3b".into(),
        name: "Qwen 3B".into(),
        path: "/models/qwen-3b.gguf".into(),
        size_bytes: 2_100_000_000,
        quantization: Some("Q4_K_M".into()),
        added_at: 1,
        sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
        status: "ready".into(),
    })
    .unwrap();
    let got = db.get_model("qwen-3b").unwrap().unwrap();
    assert_eq!(got.sha256.len(), 64);
    assert_eq!(got.status, "ready");
    // Quarantine flips the status (integrity re-check failed) without deleting.
    assert!(db.set_model_status("qwen-3b", "quarantined").unwrap());
    assert_eq!(db.get_model("qwen-3b").unwrap().unwrap().status, "quarantined");
    assert_eq!(db.list_models().unwrap().len(), 1);
}

#[test]
fn seat_bindings_crud_roundtrip() {
    let db = ProfileDb::open_in_memory("personal").unwrap();
    assert!(db.list_seat_bindings().unwrap().is_empty());
    assert!(db.get_seat_binding("Coding").unwrap().is_none());

    db.set_seat_binding("Coding", "lmstudio", "qwen3-14b").unwrap();
    db.set_seat_binding("Reviewer", "cloudco", "gpt-x").unwrap();
    // Upsert: re-binding replaces, doesn't duplicate.
    db.set_seat_binding("Coding", "lmstudio", "qwen3-30b").unwrap();

    let all = db.list_seat_bindings().unwrap();
    assert_eq!(all.len(), 2, "two distinct seats, Coding upserted not duplicated");
    let coding = db.get_seat_binding("  Coding  ").unwrap().unwrap(); // name trimmed
    assert_eq!(coding.model, "qwen3-30b");
    assert_eq!(coding.provider_id, "lmstudio");

    assert!(db.delete_seat_binding("Coding").unwrap());
    assert!(!db.delete_seat_binding("Coding").unwrap(), "second delete is a no-op");
    assert!(db.get_seat_binding("Coding").unwrap().is_none());
    assert_eq!(db.list_seat_bindings().unwrap().len(), 1);
}

#[test]
fn redaction_toggle_is_independent_of_thresholds() {
    // The ON CONFLICT upserts must keep thresholds and the redaction flag from
    // clobbering each other (they share one row).
    use crate::classifier::ClassifierConfig;
    let db = ProfileDb::open_in_memory("personal").unwrap();
    assert!(db.redaction_enabled().unwrap(), "default is redaction ON");

    // Setting thresholds must NOT flip redaction.
    let strict = ClassifierConfig::from_ui(90, "wide");
    db.set_classifier_config(&strict).unwrap();
    assert!(db.redaction_enabled().unwrap(), "threshold change preserved redaction");

    // Disabling redaction must NOT change thresholds.
    db.set_redaction_enabled(false).unwrap();
    assert!(!db.redaction_enabled().unwrap());
    let cfg = db.classifier_config().unwrap();
    assert!(
        (cfg.tau_band - strict.tau_band).abs() < 1e-6,
        "toggling redaction preserved thresholds"
    );

    // Another threshold change must NOT re-enable redaction.
    db.set_classifier_config(&ClassifierConfig::from_ui(10, "narrow")).unwrap();
    assert!(!db.redaction_enabled().unwrap(), "redaction stayed off across a threshold change");

    // Setting redaction on a fresh profile (no row yet) uses default thresholds.
    let fresh = ProfileDb::open_in_memory("work").unwrap();
    fresh.set_redaction_enabled(false).unwrap();
    assert!(!fresh.redaction_enabled().unwrap());
    assert_eq!(fresh.classifier_config().unwrap(), ClassifierConfig::default());

    // Reset clears both back to defaults (redaction on).
    db.reset_classifier_config().unwrap();
    assert!(db.redaction_enabled().unwrap());
}

#[test]
fn schema_version_survives_reopen_to_disk() {
    let dir = tempdir();
    let path = dir.join("global.db");
    {
        let _ = GlobalDb::open(&path).unwrap();
    }
    let reopened = GlobalDb::open(&path).unwrap();
    let v: i32 = reopened
        .raw()
        .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, GLOBAL_SCHEMA_VERSION);
}

// ─────────────────────────────────────────────────────────────────────────────
// 2. Conversation + messages round trip
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn create_conversation_add_messages_query_back() {
    let db = ProfileDb::open_in_memory("personal").unwrap();
    let now = chrono::Utc::now().timestamp();

    // Empty profile has no conversations.
    assert!(db.list_conversations().unwrap().is_empty());

    // Create conversation.
    let conv_id = "conv-001".to_string();
    db.create_conversation(&Conversation {
        id: conv_id.clone(),
        name: "Test chat".into(),
        pinned: false,
        binding: "auto".into(),
        folder_id: None,
        color: None,
        created_at: now,
        updated_at: now,
    })
    .unwrap();

    assert!(db.set_conversation_binding(&conv_id, "private").unwrap());
    let updated = db.get_conversation(&conv_id).unwrap().unwrap();
    assert_eq!(updated.binding, "private");
    assert_eq!(updated.name, "Test chat");
    assert!(!updated.pinned);

    // Add 3 messages: user, assistant (with thinking), user.
    let messages = vec![
        Message {
            id: "m-1".into(),
            conversation_id: conv_id.clone(),
            role: "user".into(),
            content: "Hello".into(),
            model: None,
            provider_id: None,
            routing_decision: None,
            thinking_content: None,
            error: None,
            aborted: false,
            created_at: now,
        },
        Message {
            id: "m-2".into(),
            conversation_id: conv_id.clone(),
            role: "assistant".into(),
            content: "Hi there.".into(),
            model: Some("claude-sonnet-4-5".into()),
            provider_id: Some("anthropic".into()),
            routing_decision: Some("public".into()),
            thinking_content: Some("The user said hello. I should greet them back.".into()),
            error: None,
            aborted: false,
            created_at: now + 1,
        },
        Message {
            id: "m-3".into(),
            conversation_id: conv_id.clone(),
            role: "user".into(),
            content: "What can you do?".into(),
            model: None,
            provider_id: None,
            routing_decision: None,
            thinking_content: None,
            error: None,
            aborted: false,
            created_at: now + 2,
        },
    ];
    for m in &messages {
        db.add_message(m).unwrap();
    }

    // Query back by conversation.
    let got = db.list_messages_by_conversation(&conv_id).unwrap();
    assert_eq!(got.len(), 3);
    assert_eq!(got[0].id, "m-1");
    assert_eq!(got[1].id, "m-2");
    assert_eq!(got[1].role, "assistant");
    assert_eq!(got[1].model.as_deref(), Some("claude-sonnet-4-5"));
    assert_eq!(
        got[1].thinking_content.as_deref(),
        Some("The user said hello. I should greet them back.")
    );
    assert_eq!(got[2].id, "m-3");

    // Update a message: mark it aborted.
    let updated = db
        .update_message("m-2", None, None, None, Some(true))
        .unwrap();
    assert!(updated);
    let reloaded = db.get_message("m-2").unwrap().unwrap();
    assert!(reloaded.aborted, "aborted flag should be set");
    // Other fields preserved.
    assert_eq!(
        reloaded.thinking_content.as_deref(),
        Some("The user said hello. I should greet them back.")
    );

    // Delete a message.
    assert!(db.delete_message("m-3").unwrap());
    let got = db.list_messages_by_conversation(&conv_id).unwrap();
    assert_eq!(got.len(), 2);
}

// ─────────────────────────────────────────────────────────────────────────────
// 3. Folder assignment + query by folder
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn create_folder_assign_conversation_query_by_folder() {
    let db = ProfileDb::open_in_memory("work").unwrap();
    let now = chrono::Utc::now().timestamp();

    let folder_id = "fld-projects".to_string();
    db.create_folder(&Folder {
        id: folder_id.clone(),
        name: "Projects".into(),
        color: Some("#ff8800".into()),
        created_at: now,
    })
    .unwrap();

    // Two conversations: one in the folder, one not.
    let in_folder = "c-1".to_string();
    let other = "c-2".to_string();
    db.create_conversation(&Conversation {
        id: in_folder.clone(),
        name: "Project Alpha".into(),
        pinned: false,
        binding: "auto".into(),
        folder_id: Some(folder_id.clone()),
        color: None,
        created_at: now,
        updated_at: now,
    })
    .unwrap();
    db.create_conversation(&Conversation {
        id: other.clone(),
        name: "Random thoughts".into(),
        pinned: false,
        binding: "auto".into(),
        folder_id: None,
        color: None,
        created_at: now + 1,
        updated_at: now + 1,
    })
    .unwrap();

    // Query by folder — only the one conversation.
    let in_folder_list = db.list_conversations_in_folder(&folder_id).unwrap();
    assert_eq!(in_folder_list.len(), 1);
    assert_eq!(in_folder_list[0].id, in_folder);
    assert_eq!(
        in_folder_list[0].folder_id.as_deref(),
        Some(folder_id.as_str())
    );

    // Total conversations still 2.
    assert_eq!(db.list_conversations().unwrap().len(), 2);

    // Move the second conversation into the folder, then re-query.
    db.update_conversation(
        &other,
        "Random thoughts",
        false,
        "auto",
        Some(&folder_id),
        None,
    )
    .unwrap();
    let in_folder_list = db.list_conversations_in_folder(&folder_id).unwrap();
    assert_eq!(in_folder_list.len(), 2);

    // Move it back out (folder_id = NULL).
    db.update_conversation(&other, "Random thoughts", false, "auto", None, None)
        .unwrap();
    assert_eq!(
        db.list_conversations_in_folder(&folder_id).unwrap().len(),
        1
    );

    // Delete the folder — the conversation in it should fall back to NULL
    // (ON DELETE SET NULL on conversations.folder_id).
    assert!(db.delete_folder(&folder_id).unwrap());
    let dropped = db.get_conversation(&in_folder).unwrap().unwrap();
    assert!(
        dropped.folder_id.is_none(),
        "folder_id should be null after delete"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 4. Tag definitions + tag a conversation + query by tag
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn create_tag_definitions_tag_conversation_query_by_tag() {
    let db = ProfileDb::open_in_memory("personal").unwrap();
    let now = chrono::Utc::now().timestamp();

    // Create two tag definitions.
    let important = "tag-important".to_string();
    let fun = "tag-fun".to_string();
    db.create_tag(&TagDefinition {
        id: important.clone(),
        label: "Important".into(),
        color: Some("#ff0000".into()),
        created_at: now,
    })
    .unwrap();
    db.create_tag(&TagDefinition {
        id: fun.clone(),
        label: "Fun".into(),
        color: Some("#00ff00".into()),
        created_at: now,
    })
    .unwrap();

    // Three conversations, tagged variably.
    let convs = ["c-a", "c-b", "c-c"];
    for (i, cid) in convs.iter().enumerate() {
        db.create_conversation(&Conversation {
            id: cid.to_string(),
            name: format!("Chat {cid}"),
            pinned: false,
            binding: "auto".into(),
            folder_id: None,
            color: None,
            created_at: now + i as i64,
            updated_at: now + i as i64,
        })
        .unwrap();
    }
    db.tag_conversation("c-a", &important).unwrap();
    db.tag_conversation("c-b", &important).unwrap();
    db.tag_conversation("c-b", &fun).unwrap();
    db.tag_conversation("c-c", &fun).unwrap();

    // "Important" tag → c-a + c-b
    let tagged_important = db.list_conversations_with_tag(&important).unwrap();
    let ids: Vec<&str> = tagged_important.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(tagged_important.len(), 2);
    assert!(ids.contains(&"c-a"));
    assert!(ids.contains(&"c-b"));

    // "Fun" tag → c-b + c-c
    let tagged_fun = db.list_conversations_with_tag(&fun).unwrap();
    let ids: Vec<&str> = tagged_fun.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(tagged_fun.len(), 2);
    assert!(ids.contains(&"c-b"));
    assert!(ids.contains(&"c-c"));

    // Reverse direction: tags for a single conversation.
    let tags_for_b = db.list_tags_for_conversation("c-b").unwrap();
    let labels: Vec<&str> = tags_for_b.iter().map(|t| t.label.as_str()).collect();
    assert_eq!(tags_for_b.len(), 2);
    assert!(labels.contains(&"Important"));
    assert!(labels.contains(&"Fun"));

    // Idempotent tagging.
    db.tag_conversation("c-a", &important).unwrap();
    let still_important = db.list_conversations_with_tag(&important).unwrap();
    assert_eq!(still_important.len(), 2);

    // Untag one, verify it disappears.
    assert!(db.untag_conversation("c-b", &fun).unwrap());
    let tags_for_b = db.list_tags_for_conversation("c-b").unwrap();
    assert_eq!(tags_for_b.len(), 1);
    assert_eq!(tags_for_b[0].label, "Important");

    // Delete a tag definition — all session_tags for it should cascade away.
    assert!(db.delete_tag(&fun).unwrap());
    let tagged_fun = db.list_conversations_with_tag(&fun).unwrap();
    assert!(tagged_fun.is_empty());
}

// ─────────────────────────────────────────────────────────────────────────────
// Extra: end-to-end Storage::open with a real tempdir
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn storage_open_creates_dirs_and_persists() {
    let dir = tempdir();

    let storage = Storage::open(&dir).unwrap();
    assert!(dir.exists(), "base dir should be created");
    assert!(
        dir.join("global.db").exists(),
        "global.db should be created"
    );
    assert!(
        dir.join("profiles").exists(),
        "profiles dir should be created"
    );

    // Write a fact into global.
    storage.global().set_user_fact("name", "Lukas").unwrap();
    let fact = storage.global().get_user_fact("name").unwrap().unwrap();
    assert_eq!(fact.value, "Lukas");

    // Open a profile, write a conversation, reopen, read it back.
    let profile = storage.open_profile("personal").unwrap();
    let now = chrono::Utc::now().timestamp();
    profile
        .create_conversation(&Conversation {
            id: "c-test".into(),
            name: "Hello".into(),
            pinned: false,
            binding: "auto".into(),
            folder_id: None,
            color: None,
            created_at: now,
            updated_at: now,
        })
        .unwrap();

    // Drop the cached handle so we exercise the on-disk reopen path.
    storage.close_profile("personal");
    let profile2 = storage.open_profile("personal").unwrap();
    let convs = profile2.list_conversations().unwrap();
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].id, "c-test");

    // list_profile_names should pick it up.
    let names = storage.list_profile_names().unwrap();
    assert_eq!(names, vec!["personal".to_string()]);
}

// ─────────────────────────────────────────────────────────────────────────────
// tool_rules (Q8) — round-trip, upsert idempotency, delete, cross-profile
// isolation, and REAL on-disk durability (close_profile + reopen)
// ─────────────────────────────────────────────────────────────────────────────

fn rule(id: &str, tool: &str, pattern: &str, action: &str) -> ToolRuleRow {
    ToolRuleRow {
        id: id.into(),
        tool_name: tool.into(),
        pattern: pattern.into(),
        action: action.into(),
        created_at: 1_700_000_000,
    }
}

#[test]
fn tool_rules_round_trip_and_delete() {
    let db = ProfileDb::open_in_memory("personal").unwrap();
    assert!(db.list_tool_rules().unwrap().is_empty());

    db.add_tool_rule(&rule("r1", "write_file", "*", "allow")).unwrap();
    db.add_tool_rule(&rule("r2", "read_file", "secrets/*", "deny")).unwrap();

    let wf = db.list_tool_rules_for("write_file").unwrap();
    assert_eq!(wf.len(), 1);
    assert_eq!(wf[0].action, "allow");
    assert_eq!(db.list_tool_rules_for("read_file").unwrap().len(), 1);
    assert_eq!(db.list_tool_rules().unwrap().len(), 2);

    assert!(db.delete_tool_rule("r1").unwrap(), "deleting an existing rule returns true");
    assert!(!db.delete_tool_rule("r1").unwrap(), "deleting a gone rule returns false");
    assert!(db.list_tool_rules_for("write_file").unwrap().is_empty());
    assert_eq!(db.list_tool_rules().unwrap().len(), 1);
}

#[test]
fn classifier_config_defaults_when_unset_and_round_trips() {
    use crate::classifier::ClassifierConfig;
    let db = ProfileDb::open_in_memory("personal").unwrap();

    // No row → defaults, no error.
    assert_eq!(db.classifier_config().unwrap(), ClassifierConfig::default());

    // Persist a stricter config and read it back.
    let strict = ClassifierConfig::from_ui(90, "wide");
    db.set_classifier_config(&strict).unwrap();
    let got = db.classifier_config().unwrap();
    assert!((got.tau_block - strict.tau_block).abs() < 1e-6);
    assert!((got.tau_band - strict.tau_band).abs() < 1e-6);

    // Upsert (single row) overwrites, never piles up.
    let loose = ClassifierConfig::from_ui(10, "narrow");
    db.set_classifier_config(&loose).unwrap();
    let got2 = db.classifier_config().unwrap();
    assert!((got2.tau_block - loose.tau_block).abs() < 1e-6);

    // Reset → back to defaults; second reset returns false (nothing to remove).
    assert!(db.reset_classifier_config().unwrap());
    assert_eq!(db.classifier_config().unwrap(), ClassifierConfig::default());
    assert!(!db.reset_classifier_config().unwrap());
}

#[test]
fn classifier_config_read_sanitizes_a_corrupt_row() {
    // A hand-edited/corrupt row must never yield a leakier-than-valid config:
    // the read path sanitizes (fails toward stricter).
    use crate::classifier::ClassifierConfig;
    let db = ProfileDb::open_in_memory("personal").unwrap();
    db.raw()
        .execute(
            "INSERT OR REPLACE INTO classifier_settings (id, tau_block, tau_band, updated_at)
             VALUES (1, 9.0, -3.0, 0)",
            [],
        )
        .unwrap();
    let got = db.classifier_config().unwrap();
    // 9.0 is out of range → default tau_block; -3.0 → default tau_band.
    assert_eq!(got, ClassifierConfig::default());
}

#[test]
fn tool_rules_upsert_is_idempotent_on_tool_and_pattern() {
    // Re-adding the same (tool, pattern) updates the action instead of piling
    // a duplicate row (UNIQUE(tool_name, pattern) + INSERT OR REPLACE).
    let db = ProfileDb::open_in_memory("personal").unwrap();
    db.add_tool_rule(&rule("r1", "write_file", "*", "allow")).unwrap();
    db.add_tool_rule(&rule("r2", "write_file", "*", "deny")).unwrap();
    let rows = db.list_tool_rules_for("write_file").unwrap();
    assert_eq!(rows.len(), 1, "same (tool, pattern) must not duplicate");
    assert_eq!(rows[0].action, "deny", "re-add updates the action");
}

#[test]
fn tool_rules_are_isolated_per_profile() {
    // The whole point of per-profile placement: a rule written to `work` must
    // NOT appear in `personal` (physical separation via separate DB files).
    let dir = tempdir();
    let storage = Storage::open(&dir).unwrap();

    storage
        .open_profile("work")
        .unwrap()
        .add_tool_rule(&rule("r1", "shell_exec", "kubectl *", "allow"))
        .unwrap();

    assert_eq!(
        storage.open_profile("work").unwrap().list_tool_rules().unwrap().len(),
        1
    );
    assert!(
        storage
            .open_profile("personal")
            .unwrap()
            .list_tool_rules()
            .unwrap()
            .is_empty(),
        "a rule in `work` must never leak into `personal`"
    );
}

#[test]
fn tool_rules_survive_a_real_disk_reopen() {
    // Genuine durability: write to a temp-FILE DB, drop the cached handle
    // (close_profile), reopen from disk, and confirm the rule is still there.
    // `open_in_memory` can't test this (the DB dies with the connection), and
    // `open_profile` returns a memoized Arc unless close_profile is called.
    let dir = tempdir();
    let storage = Storage::open(&dir).unwrap();

    storage
        .open_profile("personal")
        .unwrap()
        .add_tool_rule(&rule("r1", "write_file", "notes/*", "allow"))
        .unwrap();

    assert!(storage.close_profile("personal"), "handle should have been cached");
    let reopened = storage.open_profile("personal").unwrap();
    let rows = reopened.list_tool_rules_for("write_file").unwrap();
    assert_eq!(rows.len(), 1, "the rule must survive a real on-disk reopen");
    assert_eq!(rows[0].pattern, "notes/*");
    assert_eq!(rows[0].action, "allow");
}

#[test]
fn storage_open_profile_rejects_path_traversal() {
    let dir = tempdir();
    let storage = Storage::open(&dir).unwrap();
    for bad in ["", "../escape", "with/slash", ".hidden", ".."] {
        let res = storage.open_profile(bad);
        assert!(res.is_err(), "expected error for bad profile name {bad:?}");
    }
}

#[test]
fn storage_open_profile_rejects_whitespace_padding_and_confusables() {
    // B3 (2026-07-18 gap): whitespace-padded and confusable names are three
    // distinct, confusable `.db` files. The ASCII allowlist rejects them all.
    let dir = tempdir();
    let storage = Storage::open(&dir).unwrap();
    for bad in [
        " work",        // leading space
        "work ",        // trailing space
        "wo\trk",       // internal tab
        "wo rk",        // internal space
        "work\n",       // trailing newline
        "wоrk",         // Cyrillic 'о' homoglyph
        "wo\u{200b}rk", // zero-width space
        "café",         // non-ASCII (combining/accent)
        &"x".repeat(65),// too long
    ] {
        assert!(storage.open_profile(bad).is_err(), "must reject {bad:?}");
    }
    // The four real names the app generates still open fine.
    for good in ["personal", "work", "school", "developer", "my-profile_2"] {
        assert!(storage.open_profile(good).is_ok(), "must accept {good:?}");
    }
}

#[test]
fn storage_open_profile_case_folds_to_prevent_filesystem_aliasing() {
    // Review finding: on a case-INSENSITIVE filesystem (macOS/Windows, what we
    // ship on) `work.db` and `Work.db` are the SAME inode, so `"work"` and
    // `"Work"` must resolve to the SAME cached handle — otherwise two "profiles"
    // would silently share one physical DB and defeat isolation.
    let dir = tempdir();
    let storage = Storage::open(&dir).unwrap();
    let lower = storage.open_profile("work").unwrap();
    let upper = storage.open_profile("Work").unwrap();
    assert!(
        std::sync::Arc::ptr_eq(&lower, &upper),
        "case-variant names must resolve to the SAME profile handle, not two aliases"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Wave 1 — per-profile memory settings + walled-memory physical separation
// ─────────────────────────────────────────────────────────────────────────────

fn walled_fact(id: &str, content: &str, profile: &str) -> MemoryFact {
    MemoryFact {
        id: id.into(),
        content: content.into(),
        origin_profile: profile.into(),
        tags: None,
        created_at: 1,
        pinned: false,
    }
}

#[test]
fn memory_settings_round_trip_and_default() {
    let dir = tempdir();
    let storage = Storage::open(&dir).unwrap();
    let db = storage.open_profile("personal").unwrap();

    // Default (no row): semantic on, not walled.
    let d = db.memory_settings().unwrap();
    assert!(d.semantic_search_enabled && !d.walled, "defaults: semantic on, shared");

    db.set_memory_settings(&MemorySettings {
        semantic_search_enabled: false,
        walled: true,
    })
    .unwrap();
    let s = db.memory_settings().unwrap();
    assert!(!s.semantic_search_enabled && s.walled, "round-trips both flags");
}

#[test]
fn walled_profile_memory_is_physically_separate_and_survives_toggle_back() {
    // The §7 / Wave 1.5 guarantee: a walled profile's facts live in its own DB,
    // never global.db — and toggling the wall back OFF can't retroactively spill
    // what was written while walled (it was never in the shared pool).
    let dir = tempdir();
    let storage = Storage::open(&dir).unwrap();

    // A shared (default) profile routes to global.db.
    let shared = storage.memory_db_for_profile("personal").unwrap();
    shared
        .insert_memory_fact_in(MemoryBucket::Shared, &walled_fact("s1", "alpha personal reminder", "personal"))
        .unwrap();
    assert_eq!(
        storage.global().search_memory("alpha personal reminder", true, 10).unwrap().len(),
        1,
        "a shared profile's writes land in global.db"
    );

    // Wall the `work` profile, then write to its memory store.
    storage
        .open_profile("work")
        .unwrap()
        .set_memory_settings(&MemorySettings { semantic_search_enabled: true, walled: true })
        .unwrap();
    let walled = storage.memory_db_for_profile("work").unwrap();
    walled
        .insert_memory_fact_in(MemoryBucket::Shared, &walled_fact("w1", "bravo confidential dossier", "work"))
        .unwrap();

    // The fact is in the walled DB...
    assert_eq!(
        walled.search_memory("bravo confidential dossier", true, 10).unwrap().len(),
        1,
        "the walled write is readable from the walled DB"
    );
    // ...and NEVER in global.db.
    assert!(
        storage.global().search_memory("bravo confidential dossier", true, 10).unwrap().is_empty(),
        "a walled profile's fact must never enter global.db"
    );
    // The walled memory DB is a separate physical file.
    assert!(
        dir.join("walled-memory").join("work.db").exists(),
        "walled memory lives in its own file under walled-memory/"
    );

    // Toggle the wall back OFF.
    storage
        .open_profile("work")
        .unwrap()
        .set_memory_settings(&MemorySettings { semantic_search_enabled: true, walled: false })
        .unwrap();

    // The walled-era fact STILL isn't in global — the wall survived the toggle.
    assert!(
        storage.global().search_memory("bravo confidential dossier", true, 10).unwrap().is_empty(),
        "toggling the wall off must not retroactively spill walled data into global"
    );
    // And routing now goes to the shared store: a fresh write lands in global.
    let mem_now = storage.memory_db_for_profile("work").unwrap();
    mem_now
        .insert_memory_fact_in(MemoryBucket::Shared, &walled_fact("w2", "gamma unwalled entry", "work"))
        .unwrap();
    assert_eq!(
        storage.global().search_memory("gamma unwalled entry", true, 10).unwrap().len(),
        1,
        "after un-walling, writes route to the shared global store"
    );
}

#[test]
fn memory_routing_fails_closed_when_wall_status_is_unreadable() {
    // §7 fail-safe (review finding): if a profile OPENS but its wall status
    // can't be read (transient SQLite error / corrupt settings table), routing
    // must FAIL CLOSED — never silently fall back to the shared global.db, which
    // would leak a possibly-walled profile's memory. Simulate the unreadable
    // status by dropping the settings table on the (cached) connection.
    let dir = tempdir();
    let storage = Storage::open(&dir).unwrap();

    // A profile that opens fine and IS walled routes to its own DB.
    let db = storage.open_profile("locked").unwrap();
    db.set_memory_settings(&MemorySettings { semantic_search_enabled: true, walled: true })
        .unwrap();
    assert!(storage.memory_db_for_profile("locked").is_ok(), "readable walled status resolves");

    // Now make the wall status unreadable (drop the table on the same cached conn).
    db.raw().execute_batch("DROP TABLE memory_settings").unwrap();
    assert!(
        storage.memory_db_for_profile("locked").is_err(),
        "an unreadable wall status must fail closed, never route to the shared store"
    );

    // A genuinely invalid/degenerate profile name (never a real walled profile,
    // and open_profile itself rejects it) still resolves to the shared store —
    // that path is a different, safe case (no island exists to protect).
    let shared = storage.memory_db_for_profile("../evil").unwrap();
    assert!(
        std::sync::Arc::ptr_eq(&shared, &storage.memory_db_for_profile("also-fresh-shared").unwrap()),
        "a degenerate name uses the one shared global store"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Extra: global CRUD round trip
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn global_endpoints_and_memory_round_trip() {
    let g = GlobalDb::open_in_memory().unwrap();
    let now = chrono::Utc::now().timestamp();

    g.insert_endpoint(&Endpoint {
        id: "ep-anthropic".into(),
        name: "Anthropic".into(),
        base_url: "https://api.anthropic.com".into(),
        api_key_marker: Some(b"legacy-plaintext-bytes".to_vec()),
        kind: "anthropic".into(),
        created_at: now,
        supports_native_tools: true,
    })
    .unwrap();
    let eps = g.list_endpoints().unwrap();
    assert_eq!(eps.len(), 1);
    assert_eq!(eps[0].name, "Anthropic");
    assert!(eps[0].supports_native_tools, "the native-tools flag must round-trip through storage");
    assert_eq!(
        eps[0].api_key_marker.as_deref(),
        Some(b"legacy-plaintext-bytes".as_slice())
    );

    // Memory fact with tags (JSON array).
    g.insert_memory_fact(&MemoryFact {
        id: "fact-1".into(),
        content: "User's name is Lukas".into(),
        origin_profile: "personal".into(),
        tags: Some(r#"["identity","name"]"#.into()),
        created_at: now,
        pinned: false,
    })
    .unwrap();

    let all = g.list_memory_facts().unwrap();
    assert_eq!(all.len(), 1);

    let personal_facts = g.list_memory_facts_by_profile("personal").unwrap();
    assert_eq!(personal_facts.len(), 1);

    let work_facts = g.list_memory_facts_by_profile("work").unwrap();
    assert!(work_facts.is_empty());

    // Vector attached to the fact.
    g.insert_memory_vector(&MemoryVector {
        id: 0, // ignored (autoincrement)
        fact_id: "fact-1".into(),
        embedding: vec![1.0_f32, 0.5, 0.25]
            .into_iter()
            .flat_map(|f| f.to_le_bytes())
            .collect(),
    })
    .unwrap();
    let vecs = g.list_vectors_for_fact("fact-1").unwrap();
    assert_eq!(vecs.len(), 1);
    assert_eq!(vecs[0].embedding.len(), 12);

    // Cascade: deleting the fact removes the vector.
    assert!(g.delete_memory_fact("fact-1").unwrap());
    let vecs = g.list_vectors_for_fact("fact-1").unwrap();
    assert!(
        vecs.is_empty(),
        "vectors should cascade-delete with the fact"
    );
}

#[test]
fn active_profile_defaults_to_none_and_round_trips() {
    let g = GlobalDb::open_in_memory().unwrap();

    // Fresh db: nothing stored yet. `get_active_profile` maps this None to
    // "personal", but the storage layer reports the honest absence.
    assert_eq!(g.active_profile(), None, "a fresh db has no stored active profile");

    // Persist a choice and read it back.
    g.set_active_profile("work").unwrap();
    assert_eq!(g.active_profile().as_deref(), Some("work"));

    // A second write overwrites (single app_settings row, not append).
    g.set_active_profile("school").unwrap();
    assert_eq!(g.active_profile().as_deref(), Some("school"));
}

// ─────────────────────────────────────────────────────────────────────────────
// Memory system (PLAN §9): sensitivity buckets + FTS5 search + curated summary
// ─────────────────────────────────────────────────────────────────────────────

fn mem_fact(id: &str, content: &str, profile: &str, pinned: bool) -> MemoryFact {
    MemoryFact {
        id: id.into(),
        content: content.into(),
        origin_profile: profile.into(),
        tags: None,
        created_at: chrono::Utc::now().timestamp(),
        pinned,
    }
}

#[test]
fn memory_search_keyword_and_bucket_isolation() {
    let g = GlobalDb::open_in_memory().unwrap();

    g.insert_memory_fact_in(
        MemoryBucket::Shared,
        &mem_fact("s1", "Lukas prefers concise, direct replies", "personal", false),
    )
    .unwrap();
    g.insert_memory_fact_in(
        MemoryBucket::Shared,
        &mem_fact("s2", "The payments service is written in Rust", "work", false),
    )
    .unwrap();
    g.insert_memory_fact_in(
        MemoryBucket::PrivateLocal,
        &mem_fact("p1", "Home address is 123 Oak Street", "personal", false),
    )
    .unwrap();

    // Keyword hit in the shared store on a cloud-bound search.
    let cloud = g.search_memory("Rust payments", false, 10).unwrap();
    assert_eq!(cloud.len(), 1);
    assert_eq!(cloud[0].fact.id, "s2");
    assert_eq!(cloud[0].bucket, MemoryBucket::Shared);

    // A private-only term returns NOTHING on a cloud search — the private table
    // is never even queried (the structural guarantee, PLAN §9).
    let cloud_private = g.search_memory("Oak Street address", false, 10).unwrap();
    assert!(
        cloud_private.is_empty(),
        "cloud-bound search must never surface a private-local fact"
    );

    // A local search (allow_private=true) can reach it.
    let local = g.search_memory("Oak Street address", true, 10).unwrap();
    assert_eq!(local.len(), 1);
    assert_eq!(local[0].fact.id, "p1");
    assert_eq!(local[0].bucket, MemoryBucket::PrivateLocal);

    // Non-matching query → empty; punctuation-only query → empty (no FTS syntax
    // injection, no error).
    assert!(g.search_memory("kangaroo", true, 10).unwrap().is_empty());
    assert!(g.search_memory("   \"*(", true, 10).unwrap().is_empty());
}

#[test]
fn search_memory_scoped_restricts_to_one_profile() {
    // The automatic-injection search must not surface another profile's facts.
    let g = GlobalDb::open_in_memory().unwrap();
    g.insert_memory_fact_in(
        MemoryBucket::Shared,
        &mem_fact("s1", "the deploy runbook lives in the wiki", "personal", false),
    )
    .unwrap();
    g.insert_memory_fact_in(
        MemoryBucket::Shared,
        &mem_fact("s2", "the deploy pipeline runs on Fridays", "work", false),
    )
    .unwrap();

    // Unscoped search sees both profiles' shared facts (recall_memory behavior).
    assert_eq!(g.search_memory("deploy", false, 10).unwrap().len(), 2);
    // Scoped search sees only the named profile's facts.
    let personal = g.search_memory_scoped("deploy", "personal", false, 10).unwrap();
    assert_eq!(personal.len(), 1);
    assert_eq!(personal[0].fact.id, "s1");
    let work = g.search_memory_scoped("deploy", "work", false, 10).unwrap();
    assert_eq!(work.len(), 1);
    assert_eq!(work[0].fact.id, "s2");

    // Scoped search still honors the private wall on a cloud-bound (false) call.
    g.insert_memory_fact_in(
        MemoryBucket::PrivateLocal,
        &mem_fact("p1", "deploy key is 123 Oak Street vault", "personal", false),
    )
    .unwrap();
    assert_eq!(
        g.search_memory_scoped("deploy", "personal", false, 10).unwrap().len(),
        1,
        "cloud-bound scoped search must not surface the private-local fact"
    );
    assert_eq!(
        g.search_memory_scoped("deploy", "personal", true, 10).unwrap().len(),
        2,
        "local scoped search reaches the private-local fact"
    );
}

#[test]
fn curated_summary_pins_first_and_gates_private() {
    let g = GlobalDb::open_in_memory().unwrap();
    g.insert_memory_fact_in(MemoryBucket::Shared, &mem_fact("a", "oldest note", "personal", false))
        .unwrap();
    g.insert_memory_fact_in(MemoryBucket::Shared, &mem_fact("b", "pinned note", "personal", true))
        .unwrap();
    g.insert_memory_fact_in(
        MemoryBucket::PrivateLocal,
        &mem_fact("c", "private note", "personal", false),
    )
    .unwrap();

    // Cloud-bound summary: private excluded, pinned first.
    let cloud = g.curated_summary("personal", false, 10).unwrap();
    assert_eq!(cloud.len(), 2, "private fact excluded from a cloud summary");
    assert_eq!(cloud[0].id, "b", "pinned fact comes first");
    assert!(cloud.iter().all(|f| f.id != "c"));

    // Local summary includes the private fact.
    let local = g.curated_summary("personal", true, 10).unwrap();
    assert_eq!(local.len(), 3);

    // Pinning is toggleable and reflected in the ordering.
    assert!(g.set_memory_pinned("a", true).unwrap());
    let after = g.curated_summary("personal", false, 10).unwrap();
    assert!(after[0].pinned && after[1].pinned, "both pinned now sort first");
}

#[test]
fn memory_fts_stays_in_sync_on_delete() {
    let g = GlobalDb::open_in_memory().unwrap();
    g.insert_memory_fact_in(
        MemoryBucket::Shared,
        &mem_fact("x", "quantum entanglement notes", "work", false),
    )
    .unwrap();
    assert_eq!(g.search_memory("quantum", false, 10).unwrap().len(), 1);
    assert!(g.delete_memory_fact("x").unwrap());
    assert!(
        g.search_memory("quantum", false, 10).unwrap().is_empty(),
        "FTS index must drop the row when the fact is deleted"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Extra: tool_audit round trip (item 5, Fable Q9)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn tool_audit_row_is_persisted_and_queryable() {
    let p = ProfileDb::open_in_memory("personal").unwrap();
    let now = chrono::Utc::now().timestamp();

    // Empty profile has no rows.
    let empty = p.list_tool_audit("conv-1").unwrap();
    assert!(empty.is_empty(), "fresh profile must have zero audit rows");

    // Insert a row shaped like a sandbox-denial observation.
    let row = ToolAuditRow {
        id: 0, // ignored; AUTOINCREMENT
        ts: now,
        conversation_id: "conv-1".to_string(),
        tool_name: "shell_exec".to_string(),
        canonical_args: "shell_exec {\"cmd\":\"rm -rf /\"}".to_string(),
        fingerprint: "deadbeef".to_string(),
        risk: "Write".to_string(),
        outcome: "denied".to_string(),
        gate_by: Some("sandbox".to_string()),
        grant_used: None,
        decision: None,
        endpoint_kind: Some("local".to_string()),
        duration_ms: Some(2),
    };
    p.add_tool_audit(&row).unwrap();

    // Query it back — all fields round-trip exactly.
    let got = p.list_tool_audit("conv-1").unwrap();
    assert_eq!(got.len(), 1, "expected exactly one audit row");
    assert!(got[0].id > 0, "AUTOINCREMENT id must be set on read");
    assert_eq!(got[0].ts, now);
    assert_eq!(got[0].conversation_id, "conv-1");
    assert_eq!(got[0].tool_name, "shell_exec");
    assert_eq!(got[0].canonical_args, "shell_exec {\"cmd\":\"rm -rf /\"}");
    assert_eq!(got[0].fingerprint, "deadbeef");
    assert_eq!(got[0].risk, "Write");
    assert_eq!(got[0].outcome, "denied");
    assert_eq!(got[0].gate_by.as_deref(), Some("sandbox"));
    assert!(got[0].grant_used.is_none());
    assert!(got[0].decision.is_none());
    assert_eq!(got[0].endpoint_kind.as_deref(), Some("local"));
    assert_eq!(got[0].duration_ms, Some(2));
}

#[test]
fn tool_audit_filtered_to_conversation() {
    let p = ProfileDb::open_in_memory("personal").unwrap();
    let now = chrono::Utc::now().timestamp();

    for conv in ["conv-a", "conv-b"] {
        p.add_tool_audit(&ToolAuditRow {
            id: 0,
            ts: now,
            conversation_id: conv.to_string(),
            tool_name: "echo".to_string(),
            canonical_args: "echo {}".to_string(),
            fingerprint: format!("fp-{conv}"),
            risk: "Safe".to_string(),
            outcome: "ok".to_string(),
            gate_by: None,
            grant_used: Some("pre-trusted".to_string()),
            decision: None,
            endpoint_kind: Some("local".to_string()),
            duration_ms: Some(1),
        })
        .unwrap();
    }
    // Add a second row for conv-a so we can verify ordering by id ASC.
    p.add_tool_audit(&ToolAuditRow {
        id: 0,
        ts: now,
        conversation_id: "conv-a".to_string(),
        tool_name: "echo".to_string(),
        canonical_args: "echo {}".to_string(),
        fingerprint: "fp-conv-a-2".to_string(),
        risk: "Safe".to_string(),
        outcome: "ok".to_string(),
        gate_by: None,
        grant_used: Some("pre-trusted".to_string()),
        decision: None,
        endpoint_kind: Some("local".to_string()),
        duration_ms: Some(1),
    })
    .unwrap();

    let a = p.list_tool_audit("conv-a").unwrap();
    let b = p.list_tool_audit("conv-b").unwrap();
    assert_eq!(a.len(), 2);
    assert_eq!(b.len(), 1);
    // Both conv-a rows are for the same conversation, both fingerprints
    // are the only thing distinguishing them in the row data.
    assert_eq!(a[0].fingerprint, "fp-conv-a");
    assert_eq!(a[1].fingerprint, "fp-conv-a-2");
    assert_eq!(b[0].fingerprint, "fp-conv-b");
}

#[test]
fn tool_audit_appends_only_no_update_path() {
    // The audit table is intentionally append-only at the API surface:
    // there is no `update_tool_audit` or `delete_tool_audit`. This test
    // exists to make that contract visible — if a future contributor
    // adds one, they'll have to acknowledge breaking the audit invariant
    // (a Settings "Activity" pane is read-only by design).
    let p = ProfileDb::open_in_memory("personal").unwrap();
    let now = chrono::Utc::now().timestamp();
    p.add_tool_audit(&ToolAuditRow {
        id: 0,
        ts: now,
        conversation_id: "conv".to_string(),
        tool_name: "echo".to_string(),
        canonical_args: "echo {}".to_string(),
        fingerprint: "fp".to_string(),
        risk: "Safe".to_string(),
        outcome: "ok".to_string(),
        gate_by: None,
        grant_used: None,
        decision: None,
        endpoint_kind: Some("local".to_string()),
        duration_ms: None,
    })
    .unwrap();
    // The whole point: there is no .update / .delete method on
    // ToolAuditRow; we only see add/list. Just confirm we can re-read.
    let rows = p.list_tool_audit("conv").unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn retention_purges_only_expired_terminal_and_audit_rows() {
    let p = ProfileDb::open_in_memory("personal").unwrap();
    let now = 2_000_000_000_i64;

    // Direct SQL setup keeps the retention test focused on age/state policy.
    {
        let conn = p.raw();
        for (id, state, finished_at) in [
            ("old-done", "done", Some(now - 31 * 86_400)),
            ("old-failed", "failed", Some(now - 31 * 86_400)),
            ("old-running", "running", None),
            ("recent-done", "done", Some(now - 29 * 86_400)),
        ] {
            conn.execute(
                "INSERT INTO work_items
                 (id, kind, state, input_json, attempts, created_at, finished_at)
                 VALUES (?1, 'agent_dispatch', ?2, '{}', 0, ?3, ?4)",
                rusqlite::params![id, state, now - 40 * 86_400, finished_at],
            )
            .unwrap();
        }
        for (conversation, ts) in [
            ("old", now - 91 * 86_400),
            ("recent", now - 89 * 86_400),
        ] {
            conn.execute(
                "INSERT INTO tool_audit
                 (ts, conversation_id, tool_name, canonical_args, fingerprint, risk, outcome)
                 VALUES (?1, ?2, 'echo', '[redacted]', 'fp', 'Safe', 'ok')",
                rusqlite::params![ts, conversation],
            )
            .unwrap();
        }
    }

    assert_eq!(p.purge_terminal_work_items_older_than(now - 30 * 86_400).unwrap(), 2);
    assert!(p.get_work_item("old-done").unwrap().is_none());
    assert!(p.get_work_item("old-failed").unwrap().is_none());
    assert!(p.get_work_item("old-running").unwrap().is_some());
    assert!(p.get_work_item("recent-done").unwrap().is_some());

    assert_eq!(p.purge_tool_audit_older_than(now - 90 * 86_400).unwrap(), 1);
    assert!(p.list_tool_audit("old").unwrap().is_empty());
    assert_eq!(p.list_tool_audit("recent").unwrap().len(), 1);
}

#[test]
fn trm_log_purge_keeps_recent_drops_old() {
    let p = ProfileDb::open_in_memory("personal").unwrap();
    let now = chrono::Utc::now().timestamp();
    p.create_conversation(&Conversation {
        id: "c".into(),
        name: "x".into(),
        pinned: false,
        binding: "auto".into(),
        folder_id: None,
        color: None,
        created_at: now,
        updated_at: now,
    })
    .unwrap();

    // Insert one old log and one new log.
    p.insert_trm_log(&TrmLog {
        id: "old".into(),
        conversation_id: "c".into(),
        message_hash: "h-old".into(),
        decision: "private".into(),
        confidence: 0.9,
        created_at: now - 10 * 86_400, // 10 days ago
    })
    .unwrap();
    p.insert_trm_log(&TrmLog {
        id: "new".into(),
        conversation_id: "c".into(),
        message_hash: "h-new".into(),
        decision: "public".into(),
        confidence: 0.2,
        created_at: now,
    })
    .unwrap();

    // Purge anything older than 7 days.
    let cutoff = now - 7 * 86_400;
    let n = p.purge_trm_logs_older_than(cutoff).unwrap();
    assert_eq!(n, 1);

    let remaining: Vec<TrmLog> = p
        .list_trm_logs("c")
        .unwrap()
        .into_iter()
        .filter(|l| l.id == "old" || l.id == "new")
        .collect();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, "new");
}

// ─────────────────────────────────────────────────────────────────────────────
// Helper: minimal tempdir impl (avoid pulling in a crate just for tests)
// ─────────────────────────────────────────────────────────────────────────────

/// Returns a fresh empty directory under the OS temp root. Auto-deletes on
/// drop, so tests can just `let dir = tempdir();` and forget about cleanup.
struct TempDir(PathBuf);

fn tempdir() -> TempDir {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("lhp-storage-test-{pid}-{n}"));
    std::fs::create_dir_all(&path).expect("create tempdir");
    TempDir(path)
}

impl std::ops::Deref for TempDir {
    type Target = PathBuf;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// sqlite-vec extension smoke test — proves the "by meaning" search engine
// actually loads and does nearest-neighbour matching on this toolchain.
// Opening any DB registers the extension (ensure_sqlite_vec_registered), so
// the vec0 virtual table + KNN MATCH must work end-to-end here.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn sqlite_vec_extension_loads_and_does_knn() {
    let db = GlobalDb::open_in_memory().unwrap();
    let conn = db.raw();

    // vec0 virtual table = the semantic index. If the extension didn't
    // register, this CREATE fails with "no such module: vec0".
    conn.execute_batch(
        "CREATE VIRTUAL TABLE vec_smoke USING vec0(embedding float[4]);
         INSERT INTO vec_smoke(rowid, embedding) VALUES
           (1, '[1.0, 2.0, 3.0, 4.0]'),
           (2, '[9.0, 9.0, 9.0, 9.0]'),
           (3, '[1.1, 2.1, 3.1, 4.1]');",
    )
    .expect("vec0 module must be available (sqlite-vec registered)");

    // KNN: the two nearest vectors to [1,2,3,4] must be rows 1 then 3,
    // never row 9-9-9-9. Proves the distance/MATCH operator works.
    let nearest: Vec<i64> = conn
        .prepare("SELECT rowid FROM vec_smoke WHERE embedding MATCH ?1 ORDER BY distance LIMIT 2")
        .unwrap()
        .query_map(rusqlite::params!["[1.0, 2.0, 3.0, 4.0]"], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<i64>>>()
        .expect("KNN query must run");

    assert_eq!(nearest, vec![1, 3], "nearest-match ordering must be 1 then 3");
}

// ─────────────────────────────────────────────────────────────────────────────
// Hybrid memory search — the sqlite-vec meaning lane (PLAN §9)
// ─────────────────────────────────────────────────────────────────────────────

/// A unit vector on `axis` of an 8-dim space (mirrors the FakeEmbedder's shape).
fn axis_vec(axis: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; 8];
    v[axis] = 1.0;
    v
}

#[test]
fn semantic_lane_finds_meaning_matches_without_keyword_overlap() {
    let g = GlobalDb::open_in_memory().unwrap();
    g.insert_memory_fact_in(
        MemoryBucket::Shared,
        &mem_fact("a", "the vault holds the door code", "personal", false),
    )
    .unwrap();
    g.insert_memory_fact_in(
        MemoryBucket::Shared,
        &mem_fact("b", "the standup moved to Tuesdays", "personal", false),
    )
    .unwrap();
    g.upsert_memory_embedding(MemoryBucket::Shared, "a", &axis_vec(0)).unwrap();
    g.upsert_memory_embedding(MemoryBucket::Shared, "b", &axis_vec(1)).unwrap();

    // The query shares ZERO tokens with fact "a" — keyword-only finds nothing —
    // but its vector sits on fact "a"'s axis (distance 0), so the meaning lane
    // surfaces it. Fact "b" is at distance 1.0, past the gate.
    let hits = g
        .search_memory_scoped_hybrid(
            "where is the passcode kept",
            Some(&axis_vec(0)),
            "personal",
            false,
            SEMANTIC_MAX_DIST_INJECT,
            3,
        )
        .unwrap();
    assert_eq!(hits.len(), 1, "only the near-by-meaning fact clears the gate");
    assert_eq!(hits[0].fact.id, "a");

    // Without a query vector (no embedder installed) the same query finds
    // nothing — graceful keyword-only degradation.
    let none = g
        .search_memory_scoped_hybrid(
            "where is the passcode kept",
            None,
            "personal",
            false,
            SEMANTIC_MAX_DIST_INJECT,
            3,
        )
        .unwrap();
    assert!(none.is_empty(), "keyword-only lane has no match for this phrasing");
}

#[test]
fn semantic_lane_respects_private_wall_profile_scope_and_dim_guard() {
    let g = GlobalDb::open_in_memory().unwrap();
    g.insert_memory_fact_in(
        MemoryBucket::PrivateLocal,
        &mem_fact("p", "my insulin dose is 12 units nightly", "personal", false),
    )
    .unwrap();
    g.upsert_memory_embedding(MemoryBucket::PrivateLocal, "p", &axis_vec(2)).unwrap();

    // Cloud turn (allow_private=false): even an exact vector match must not
    // surface the private fact — the private vector table is never queried.
    let cloud = g
        .search_memory_for_recall_hybrid(
            "zzz nothing keyword",
            Some(&axis_vec(2)),
            "personal",
            false,
            SEMANTIC_MAX_DIST_RECALL,
            5,
        )
        .unwrap();
    assert!(cloud.is_empty(), "cloud turn must never see the private vector index");

    // Local turn, same profile: found.
    let local = g
        .search_memory_for_recall_hybrid(
            "zzz nothing keyword",
            Some(&axis_vec(2)),
            "personal",
            true,
            SEMANTIC_MAX_DIST_RECALL,
            5,
        )
        .unwrap();
    assert_eq!(local.len(), 1);
    assert_eq!(local[0].fact.id, "p");
    assert_eq!(local[0].bucket, MemoryBucket::PrivateLocal);

    // Local turn, DIFFERENT profile: the private-local bucket never crosses
    // the profile boundary.
    let other = g
        .search_memory_for_recall_hybrid(
            "zzz nothing keyword",
            Some(&axis_vec(2)),
            "work",
            true,
            SEMANTIC_MAX_DIST_RECALL,
            5,
        )
        .unwrap();
    assert!(other.is_empty(), "private-local must not cross the profile boundary");

    // Dimension guard: a stale 3-dim blob must be skipped, not error the query.
    g.insert_memory_fact_in(
        MemoryBucket::Shared,
        &mem_fact("stale", "an old fact with a stale vector", "personal", false),
    )
    .unwrap();
    g.insert_memory_vector(&MemoryVector {
        id: 0,
        fact_id: "stale".into(),
        embedding: vec![1.0f32, 0.0, 0.0].into_iter().flat_map(|f| f.to_le_bytes()).collect(),
    })
    .unwrap();
    let ok = g
        .search_memory_for_recall_hybrid(
            "zzz nothing keyword",
            Some(&axis_vec(0)),
            "personal",
            false,
            SEMANTIC_MAX_DIST_RECALL,
            5,
        )
        .unwrap();
    assert!(ok.is_empty(), "a wrong-dimension row is skipped, never an error");
}

#[test]
fn hybrid_fuses_keyword_and_semantic_lanes() {
    let g = GlobalDb::open_in_memory().unwrap();
    // "both": keyword AND meaning match. "kw": keyword only. "sem": meaning only.
    g.insert_memory_fact_in(
        MemoryBucket::Shared,
        &mem_fact("both", "the deploy key lives in the vault", "personal", false),
    )
    .unwrap();
    g.insert_memory_fact_in(
        MemoryBucket::Shared,
        &mem_fact("kw", "a deploy checklist for fridays", "personal", false),
    )
    .unwrap();
    g.insert_memory_fact_in(
        MemoryBucket::Shared,
        &mem_fact("sem", "credentials are stored in the password manager", "personal", false),
    )
    .unwrap();
    g.upsert_memory_embedding(MemoryBucket::Shared, "both", &axis_vec(0)).unwrap();
    g.upsert_memory_embedding(MemoryBucket::Shared, "kw", &axis_vec(1)).unwrap();
    g.upsert_memory_embedding(MemoryBucket::Shared, "sem", &axis_vec(0)).unwrap();

    let hits = g
        .search_memory_scoped_hybrid(
            "deploy key",
            Some(&axis_vec(0)),
            "personal",
            false,
            SEMANTIC_MAX_DIST_INJECT,
            3,
        )
        .unwrap();
    let ids: Vec<&str> = hits.iter().map(|h| h.fact.id.as_str()).collect();
    assert!(ids.contains(&"both") && ids.contains(&"kw") && ids.contains(&"sem"),
        "hybrid must union both lanes, got {ids:?}");
    assert_eq!(ids[0], "both", "a fact matching BOTH lanes must rank first (RRF)");
}

#[test]
fn facts_missing_embedding_backfill_worklist() {
    let g = GlobalDb::open_in_memory().unwrap();
    g.insert_memory_fact_in(MemoryBucket::Shared, &mem_fact("e1", "embedded fact", "p", false)).unwrap();
    g.insert_memory_fact_in(MemoryBucket::Shared, &mem_fact("e2", "not yet embedded", "p", false)).unwrap();
    g.upsert_memory_embedding(MemoryBucket::Shared, "e1", &axis_vec(0)).unwrap();

    let missing = g.facts_missing_embedding(MemoryBucket::Shared, 10).unwrap();
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].id, "e2");

    g.upsert_memory_embedding(MemoryBucket::Shared, "e2", &axis_vec(1)).unwrap();
    assert!(g.facts_missing_embedding(MemoryBucket::Shared, 10).unwrap().is_empty());

    // Upsert replaces: re-embedding e1 leaves exactly one vector row.
    g.upsert_memory_embedding(MemoryBucket::Shared, "e1", &axis_vec(3)).unwrap();
    assert_eq!(g.list_vectors_for_fact("e1").unwrap().len(), 1, "upsert must replace, not stack");
}
