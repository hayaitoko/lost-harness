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
use crate::storage::schema::{GLOBAL_TABLES, PROFILE_TABLES, SCHEMA_VERSION};

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
// 5. schema_version is 1 after init (and after reopening)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn schema_version_is_one_after_init_global() {
    let db = GlobalDb::open_in_memory().unwrap();
    let v: i32 = db
        .raw()
        .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, SCHEMA_VERSION);
    assert_eq!(v, 1);
}

#[test]
fn schema_version_is_one_after_init_profile() {
    let db = ProfileDb::open_in_memory("personal").unwrap();
    let v: i32 = db
        .raw()
        .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, SCHEMA_VERSION);
    assert_eq!(v, 1);
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
    assert_eq!(v, SCHEMA_VERSION);
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

#[test]
fn storage_open_profile_rejects_path_traversal() {
    let dir = tempdir();
    let storage = Storage::open(&dir).unwrap();
    for bad in ["", "../escape", "with/slash", ".hidden", ".."] {
        let res = storage.open_profile(bad);
        assert!(res.is_err(), "expected error for bad profile name {bad:?}");
    }
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
        api_key_encrypted: Some(b"fake-encrypted-bytes".to_vec()),
        kind: "anthropic".into(),
        created_at: now,
    })
    .unwrap();
    let eps = g.list_endpoints().unwrap();
    assert_eq!(eps.len(), 1);
    assert_eq!(eps[0].name, "Anthropic");
    assert_eq!(
        eps[0].api_key_encrypted.as_deref(),
        Some(b"fake-encrypted-bytes".as_slice())
    );

    // Memory fact with tags (JSON array).
    g.insert_memory_fact(&MemoryFact {
        id: "fact-1".into(),
        content: "User's name is Lukas".into(),
        origin_profile: "personal".into(),
        tags: Some(r#"["identity","name"]"#.into()),
        created_at: now,
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

// ─────────────────────────────────────────────────────────────────────────────
// Extra: TRM log retention helper
// ─────────────────────────────────────────────────────────────────────────────

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
