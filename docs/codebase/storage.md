# Storage

- **Purpose** — Owns all persistent state for the app: a two-database SQLite
  architecture (one `global.db` shared across profiles, one
  `profiles/<name>.db` per profile), the migration runner, and the
  `sqlite-vec` extension registration that backs semantic memory search.
  Every other subsystem (agent loop, IPC layer, classifier audit log) reads
  and writes through this module rather than touching SQLite directly.

- **Files**
  - `src-tauri/src/storage/mod.rs` — `Storage` top-level handle,
    `ensure_sqlite_vec_registered()`, profile-DB open/cache/list, the
    `unsafe impl Send + Sync for Storage`.
  - `src-tauri/src/storage/global.rs` — `GlobalDb`: row types + CRUD for
    `user_facts`, `endpoints`, `model_catalog`, `memory_facts`,
    `memory_vectors`, `skills`, `app_settings`.
  - `src-tauri/src/storage/profile.rs` — `ProfileDb`: row types + CRUD for
    `conversations`, `messages`, `folders`, `tag_definitions`/`session_tags`,
    `email_accounts`/`email_messages`, `calendar_events`, `tasks`,
    `cron_jobs`, `trm_logs`. Also has `unsafe impl Send + Sync for ProfileDb`.
  - `src-tauri/src/storage/schema.rs` — `SCHEMA_VERSION`, `GLOBAL_TABLES` /
    `PROFILE_TABLES` (name lists used by tests), and the raw
    `GLOBAL_SCHEMA_SQL` / `PROFILE_SCHEMA_SQL` `CREATE TABLE` blobs. This is
    the actual schema source of truth in code (PLAN.md §1/§5 is the design
    doc it was derived from).
  - `src-tauri/src/storage/migrations.rs` — `Migration` struct,
    `GLOBAL_MIGRATIONS` / `PROFILE_MIGRATIONS` arrays, `migrate_global()` /
    `migrate_profile()` entry points, the `run_migrations()` engine
    (versioned, transactional, idempotent).
  - `src-tauri/src/storage/tests.rs` (`#[cfg(test)] mod tests` from
    `mod.rs`) — integration tests over in-memory + tempdir-backed DBs.

- **Key types / traits / functions**
  - `Storage` — `src-tauri/src/storage/mod.rs:58` — clonable (`Arc`-backed)
    top-level handle. `Storage::open(base_path: &Path) -> Result<Self>`
    (`mod.rs:88`) creates `<base>/` and `<base>/profiles/`, opens
    `global.db`, runs migrations. `open_profile(&self, name: &str) ->
    Result<Arc<ProfileDb>>` (`mod.rs:116`) lazily opens/caches
    `profiles/<name>.db`, rejecting empty/`.`-prefixed/`..`/slash-containing
    names. `global()` (`mod.rs:109`), `list_profile_names()` (`mod.rs:151`),
    `close_profile(name)` (`mod.rs:171`, evicts the cache entry to force a
    disk reopen).
  - `ensure_sqlite_vec_registered()` — `mod.rs:40` — `pub(crate)`,
    `Once`-guarded, calls `rusqlite::ffi::sqlite3_auto_extension` with a
    transmuted `sqlite_vec::sqlite3_vec_init`. Called at the top of every
    `GlobalDb::open` / `open_in_memory` and `ProfileDb::open` /
    `open_in_memory`. Must run before the *first* `Connection::open` in the
    process for the `vec0` module to be available on all subsequent
    connections (both DBs, both real and in-memory).
  - `GlobalDb` — `global.rs:84`, single `rusqlite::Connection` field.
    `open(path) -> Result<Self>` (`global.rs:90`), `open_in_memory()`
    (`global.rs:100`, test-only), `raw() -> &Connection` (`global.rs:109`),
    plus typed CRUD per table (`set_user_fact`/`get_user_fact`/
    `list_user_facts`/`delete_user_fact`; `insert_endpoint`/`get_endpoint`/
    `list_endpoints`/`update_endpoint`/`delete_endpoint`; model_catalog,
    memory_facts, memory_vectors, skills, app_settings equivalents).
  - `ProfileDb` — `profile.rs:132`, `Connection` + `pub name: String`.
    `open(path, name) -> Result<Self>` (`profile.rs:150`),
    `open_in_memory(name)` (`profile.rs:168`, test-only), `raw()`
    (`profile.rs:178`), `name()` (`profile.rs:182`). Typed CRUD for every
    profile table, notably: `create_conversation`/`update_conversation`/
    `list_conversations_in_folder` (`profile.rs:188-270`); `add_message`/
    `list_messages_by_conversation` (`profile.rs:274-364`, ordered
    `created_at ASC, rowid ASC` — see Gotchas); `insert_trm_log`/
    `list_trm_logs`/`purge_trm_logs_older_than(cutoff: i64) -> Result<usize>`
    (`profile.rs:769-814`, the classifier-decision audit trail + its
    7-day-retention purge helper — nothing currently calls the purge helper
    automatically, see Gotchas).
  - `migrate_global(conn: &Connection) -> Result<()>` (`migrations.rs:42`) /
    `migrate_profile(conn: &Connection) -> Result<()>` (`migrations.rs:54`)
    — both delegate to `run_migrations()` (`migrations.rs:64`), which
    creates `schema_version` if missing, reads
    `MAX(version)`, and for every `Migration` with `version > current` runs
    its SQL in `conn.unchecked_transaction()`, records the version, commits.
    Each migration is independent — a later migration failing doesn't touch
    an earlier one's committed state.
  - `SCHEMA_VERSION: i32 = 1` (`schema.rs:11`) — currently only one
    migration exists for each DB (`GLOBAL_MIGRATIONS`/`PROFILE_MIGRATIONS`
    each hold exactly one `Migration { version: 1, ... }` in
    `migrations.rs:25-36`), and both `migrate_global`/`migrate_profile` end
    with a `debug_assert_eq!` that the last migration's version equals
    `SCHEMA_VERSION` — a debug-build tripwire if someone adds a migration
    and forgets to bump the constant.

- **Data flow / how it fits**
  - `AppState` (`src-tauri/src/agent/loop_mod.rs:58`, `pub storage:
    Arc<Storage>`) is constructed once at app start and handed to both the
    Tauri IPC command layer (`src-tauri/src/ipc/mod.rs`) and the agent loop
    (`src-tauri/src/agent/loop_mod.rs`).
  - IPC commands call `state.storage.global()` for global-scope reads/writes
    and `state.storage.open_profile(name)` to get/open the active profile's
    `ProfileDb`, then call typed methods on it directly — no query-building
    or SQL happens outside `storage/`.
  - The agent loop calls `profile_db.insert_trm_log(&entry)` after every
    classifier decision on a message (`agent/loop_mod.rs:445-467`) — this is
    the load-bearing privacy audit trail (see Invariants).
  - `GlobalDb::open` / `ProfileDb::open` both call
    `ensure_sqlite_vec_registered()` before `Connection::open`, then run
    their respective `migrate_*` function before returning — callers never
    see an unmigrated connection.
  - Nothing outside `storage/` currently touches `memory_vectors`,
    `MemoryVector`, or the `vec0`/FTS5 machinery directly (confirmed via
    repo-wide grep) — the embedding-generation and hybrid-search pipeline
    that would populate/query it is the not-yet-started "memory milestone"
    (see PLAN.md §9, HANDOFF.md).

- **Invariants (do NOT break)**
  - **Two-DB separation is the privacy boundary.** `global.db` holds only
    things that are legitimately cross-profile (endpoints, model catalog,
    shared memory facts tagged by `origin_profile`, skills, app settings).
    Anything conversation-shaped (messages, folders, tags, email, calendar,
    tasks, cron, the classifier audit log) lives in the per-profile DB and
    must stay there — mixing them would defeat the point of profiles.
  - **`open_profile` path-traversal guard is the only thing stopping a
    profile name from escaping `profiles/`** (`mod.rs:122-129`: rejects
    empty, contains `/`, contains `\`, contains `..`, or starts with `.`).
    Any caller that constructs a profile DB path some other way must apply
    the same check. Covered by
    `storage_open_profile_rejects_path_traversal` in `tests.rs:425`.
  - **`trm_logs` is the classifier-decision audit trail** — every
    private/public routing decision the privacy classifier makes should be
    recorded via `insert_trm_log`. This is a stated product invariant (PLAN
    §3/§4: "the privacy filter is load-bearing, not cosmetic" — HANDOFF.md
    line 191) — don't remove or bypass the `insert_trm_log` call in the
    agent loop when touching classifier or message-send code.
  - **Migrations are append-only.** Never edit `GLOBAL_SCHEMA_SQL` /
    `PROFILE_SCHEMA_SQL` or an existing `Migration` entry in place once it
    has shipped — add a new `Migration { version: N+1, ... }` instead
    (stated explicitly in the `migrations.rs:15-16` doc comment). Editing
    a shipped migration's SQL does nothing for users who already applied it
    (their `schema_version` row already covers that version) and will
    silently diverge fresh installs from upgraded installs.
  - **`ensure_sqlite_vec_registered()` must run before the first
    `Connection::open` in the process.** `sqlite3_auto_extension` registers
    the init function for *all future* connections process-wide, not
    per-connection — it's `Once`-guarded specifically so every DB-open path
    (`GlobalDb::open`, `GlobalDb::open_in_memory`, `ProfileDb::open`,
    `ProfileDb::open_in_memory`) calls it first. Don't add a new
    connection-opening path that skips this call, or `vec0` virtual tables
    silently fail with "no such module."
  - **Foreign keys are ON but only per-connection.** `run_migrations()`
    issues `PRAGMA foreign_keys = ON;` (`migrations.rs:65`) at the start of
    every migration run — this is a per-connection SQLite pragma, not
    persisted in the DB file. Any code path that opens a raw
    `rusqlite::Connection` to one of these files *outside* `GlobalDb::open`/
    `ProfileDb::open` (e.g. a maintenance script) will get FK enforcement
    off by default and must set the pragma itself, or cascade
    deletes (`ON DELETE CASCADE`/`SET NULL` used throughout — e.g.
    `memory_vectors.fact_id`, `messages.conversation_id`,
    `conversations.folder_id`, `session_tags.*`, `trm_logs.conversation_id`)
    silently won't fire.

- **Gotchas / watch-items**
  - **`unsafe impl Send + Sync` on both `Storage` (`mod.rs:71-72`) and
    `ProfileDb` (`profile.rs:145-146`) is a soundness claim, not a free
    lunch.** `rusqlite::Connection` is internally `RefCell`-backed and
    therefore `!Sync`. The safety comments state the actual invariant this
    relies on: `Storage`/`ProfileDb` are only ever touched from behind a
    serializing boundary — per `mod.rs:62-70`, "the agent loop and IPC
    commands are serialized through a `Mutex<Storage>` at the AppState
    boundary" — but the *actual* `AppState` code
    (`agent/loop_mod.rs:18-28`) says something subtly different: each
    top-level handle (`storage: Arc<Storage>`) is held directly with **no
    outer `Mutex`**, and it's Tauri's command-boundary serialization plus
    the agent loop's own internal `tokio::sync::Mutex` (for streaming) that
    provides the guarantee instead. **Read both comments before adding any
    genuinely concurrent access path to `Storage`/`ProfileDb`** (e.g. a
    background cron runner, a second window, a parallel embedding job) —
    the current soundness argument is "nothing runs two DB operations on
    the same connection at once," and that's easy to accidentally violate
    from a new async task. Both comments say the fix, if that happens, is
    to push a real `parking_lot::Mutex<Connection>` inside
    `GlobalDb`/`ProfileDb` and delete the manual impls.
  - **`memory_vectors` is a placeholder, not a working feature.** The table
    exists (`schema.rs:79-84`), `MemoryVector`/`insert_memory_vector`/
    `list_vectors_for_fact` exist (`global.rs:58-63`, `383-405`), and
    `sqlite-vec` is wired + smoke-tested (`tests.rs:596-623` creates its own
    scratch `vec0` virtual table, `vec_smoke` — it does **not** exercise
    `memory_vectors` itself, which is still a plain `BLOB` column, not a
    `vec0` virtual table). Nothing in the app actually generates embeddings
    or queries by similarity yet. FTS5 is mentioned only in a doc comment
    (`mod.rs:39`) — there is no FTS5 virtual table anywhere in `schema.rs`.
    Building the real hybrid (FTS5 keyword + sqlite-vec KNN) search is the
    unstarted "memory milestone" per HANDOFF.md.
  - **Walled-vs-shared memory (PLAN §7/§9) is decided but not implemented
    in this code.** The decision (2026-07-08, HANDOFF.md lines 100-109): a
    per-profile "keep this profile's memory private" toggle should make a
    walled profile use its **own separate memory database**, physically
    apart from `global.db`'s shared `memory_facts`/`memory_vectors`
    (default = shared, tagged by `origin_profile`). As of this read: (1)
    `PROFILE_SCHEMA_SQL` has **no** memory-related tables at all — a walled
    profile's memory DB doesn't exist as a concept in `profile.rs`/
    `schema.rs` yet; (2) there is no toggle/setting anywhere in `storage/`
    that branches memory writes between "shared" and "walled" paths; (3)
    the only place memory currently lives is `global.db`'s `memory_facts`
    (tagged `origin_profile: String`, a plain column, not yet
    toggle-aware) and `memory_vectors`. When implementing the memory
    milestone, the walled path most likely needs a **third kind of SQLite
    file** (a profile-scoped memory DB, distinct from both `global.db` and
    `profiles/<name>.db`) — that's new ground for `Storage`, not something
    `open_profile` already handles.
  - **`list_messages_by_conversation` deliberately sorts by `created_at
    ASC, rowid ASC`, not by `id`** (`profile.rs:312-320`, comment explains
    why: `created_at` is second-granularity and `id` is a random UUID, so
    two messages written in the same second would sort randomly by `id`;
    `rowid` preserves true insertion order). If you ever change `messages`
    to a table without an implicit rowid (e.g. `WITHOUT ROWID`), this
    ordering breaks silently.
  - **`update_message` uses `COALESCE(?, column)` per-field**
    (`profile.rs:341-347`) so passing `None` for a field preserves its
    existing value — but this also means **you cannot use this method to
    intentionally set a field back to `NULL`** (e.g. clearing `error` after
    a retry succeeds). A caller wanting to null out `error` would need a
    dedicated statement.
  - **`purge_trm_logs_older_than` exists but nothing calls it outside
    tests.** The 7-day TRM-log retention policy (doc comment,
    `profile.rs:806-807`, citing spec §3) is implemented as a helper method
    only — repo-wide grep found no caller in `agent/` or anywhere else. If
    retention is meant to actually happen, something (a cron-like
    background task) still needs to invoke this on a schedule.
  - **`trm_logs` table name is a deliberate historical mismatch.** Per
    HANDOFF.md line 115: the `trm/` Rust module was renamed to
    `classifier/` on 2026-07-09 after the "Tiny-Recursive-Model" approach
    was dropped, but the `trm_logs` table/struct name was **kept on
    purpose** because renaming a persisted table needs a migration that
    wasn't judged worth it. Don't "fix" the name without adding a proper
    migration (and don't be surprised the classifier code imports
    `crate::storage::TrmLog`).
  - **`SCHEMA_VERSION` is a single shared constant for both DBs**
    (`schema.rs:11`), even though `GLOBAL_MIGRATIONS` and
    `PROFILE_MIGRATIONS` are independent arrays. Today both happen to be at
    version 1. The moment global and profile schemas need to diverge in
    migration count, this shared constant plus the `debug_assert_eq!` in
    both `migrate_global`/`migrate_profile` will need to become two
    separate constants — as written, adding *only* a new global migration
    without a matching profile one will trip the profile-side
    `debug_assert_eq!` (debug builds only; release builds skip it silently).
  - **`api_key_encrypted` is a `BLOB` column but nothing in `storage/`
    encrypts it** — `insert_endpoint`/`update_endpoint` just store whatever
    bytes the caller passes (`global.rs:171-224`). Encryption/decryption is
    presumably a caller responsibility elsewhere (not part of this
    subsystem) — don't assume this module gives you encryption-at-rest for
    free.
  - **`ProfileDb::name` is public (`pub name: String`,
    `profile.rs:132-137`)** and is set once at `open`/`open_in_memory` time
    from the caller-supplied name — it is *not* re-derived from the file
    path on every access, so if a `.db` file is ever renamed on disk
    out-of-band, an already-open cached handle will keep reporting the old
    name until `Storage::close_profile` + reopen.

- **How to extend**
  - **New global-scope table:** add the `CREATE TABLE` to
    `GLOBAL_SCHEMA_SQL` and its name to `GLOBAL_TABLES` in `schema.rs`,
    bump `SCHEMA_VERSION` and add a new `Migration` entry to
    `GLOBAL_MIGRATIONS` in `migrations.rs` (do **not** edit the existing
    v1 migration's SQL — append a new one, even for a brand-new table, once
    v1 has shipped to any real user; if still pre-ship you may fold it into
    v1). Add a row type + typed CRUD methods to `global.rs` following the
    existing pattern (`params!`, `.optional()`, `query_map` +
    `collect::<rusqlite::Result<Vec<_>>>()`). Add coverage in `tests.rs`
    (table-existence loop already picks up anything in `GLOBAL_TABLES`
    automatically; add a dedicated CRUD round-trip test too).
  - **New per-profile table:** same pattern, but in `PROFILE_SCHEMA_SQL` /
    `PROFILE_TABLES` / `PROFILE_MIGRATIONS` / `profile.rs`.
  - **Walled per-profile memory DB (the next real piece of memory work):**
    will likely need a new `Storage` method parallel to `open_profile` —
    e.g. something like `open_profile_memory(name)` that opens/creates
    `profiles/<name>.memory.db` (or similar) with its own migration set —
    plus a settings flag (probably an `app_settings` key or a new
    per-profile column) that the memory-write path branches on. Read
    PLAN.md §7 and §9 in full before starting; the decision write-up
    specifies the shared-vs-walled semantics precisely (walled = full
    island, reads nothing shared, writes nothing back; a read-only
    variant is explicitly deferred past v1).
  - **Hybrid memory search (FTS5 + sqlite-vec):** FTS5 needs an actual
    `CREATE VIRTUAL TABLE ... USING fts5(...)` added to
    `GLOBAL_SCHEMA_SQL` (none exists yet — only a doc-comment mention).
    `memory_vectors.embedding` needs to move from a raw `BLOB` to an actual
    `vec0` virtual table (or a shadow `vec0` table kept in sync with
    `memory_vectors`) to get KNN `MATCH` queries — follow the pattern
    proven in `tests.rs:596-623`'s `vec_smoke` table.
  - **Changing an existing schema/behavior:** since migrations are
    append-only, any breaking change (renamed column, changed type,
    dropped table) needs a new numbered migration that does the
    transformation in SQL (e.g. `ALTER TABLE`, or create-new/copy/drop-old
    for SQLite's limited `ALTER TABLE`), not an edit to `schema.rs`'s
    existing `CREATE TABLE IF NOT EXISTS` blob (that blob only affects
    brand-new DBs going forward, once a matching migration row exists for
    a given DB, so the old blob and the accumulated migrations must stay
    consistent — this repo currently gets away with editing the v1 blob
    directly only because it hasn't shipped/diverged yet; that stops being
    safe after a real release).

- **Tests** — All in `src-tauri/src/storage/tests.rs`, wired in via `#[cfg(test)] mod tests;` in `mod.rs:22-23`, plus a small `#[cfg(test)] mod tests` block inside `migrations.rs:113-148` for migration-runner-specific checks (fresh-DB lands at `SCHEMA_VERSION`, reapplying is a no-op). Run with:
  ```
  cd src-tauri && cargo test storage::
  ```
  or narrower, e.g. `cargo test storage::tests::sqlite_vec_extension_loads_and_does_knn`. Notable tests: table-existence sweep over `GLOBAL_TABLES`/`PROFILE_TABLES` (`tests.rs:20-54`), conversation+message CRUD round trip (`tests.rs:101-199`), folder assignment incl. `ON DELETE SET NULL` cascade (`tests.rs:205-286`), tag assignment incl. `ON DELETE CASCADE` on `session_tags` (`tests.rs:292-371`), end-to-end `Storage::open` against a real tempdir with a drop-and-reopen profile cycle (`tests.rs:377-423`), profile-name path-traversal rejection (`tests.rs:425-433`), global endpoints/memory-facts/memory-vectors round trip incl. cascade delete (`tests.rs:439-501`), TRM log retention purge (`tests.rs:507-556`), and the `sqlite-vec` KNN smoke test (`tests.rs:596-623`). Tests use a hand-rolled `TempDir` helper (`tests.rs:562-587`) instead of pulling in the `tempfile` crate.
