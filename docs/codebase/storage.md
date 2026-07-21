# Storage

- **Purpose** — Owns all persistent state for the app: a multi-database SQLite
  architecture (one `global.db` shared across profiles, one
  `profiles/<name>.db` per profile, and — when a profile opts into the §7
  privacy wall — its own `walled-memory/<name>.db`), a per-database migration
  runner with **independently versioned** global/profile schemas, and the
  `sqlite-vec` + FTS5 registration that backs a real, **live** hybrid
  (keyword + meaning) memory search. Every other subsystem (agent loop, IPC
  layer, classifier audit log, tool dispatcher) reads and writes through this
  module rather than touching SQLite directly.

- **Files**
  - `src-tauri/src/storage/mod.rs` (236 lines) — `Storage` top-level handle,
    `ensure_sqlite_vec_registered()`, profile-DB open/cache/list, and
    `memory_db_for_profile` — the §7 walled-vs-shared memory router.
  - `src-tauri/src/storage/global.rs` (1470 lines) — `GlobalDb`: row types +
    CRUD for `user_facts`, `endpoints`, `model_catalog`, `memory_facts` /
    `memory_facts_private`, `memory_vectors` / `memory_vectors_private`,
    `skills`, `agent_types`, `app_settings` — plus the hybrid memory-search
    implementation (FTS5 keyword lane, sqlite-vec cosine-distance meaning
    lane, Reciprocal Rank Fusion).
  - `src-tauri/src/storage/profile.rs` (1755 lines) — `ProfileDb`: row types +
    CRUD for `conversations`, `messages`, `email_accounts`/`email_messages`,
    `calendar_events`, `tasks`, `cron_jobs`, `trm_logs`, `folders`,
    `tag_definitions`/`session_tags`, `tool_audit`, `tool_rules`,
    `classifier_settings`, `memory_settings`, `usage_events`, `work_items`,
    `seat_bindings`, `sandbox_config`.
  - `src-tauri/src/storage/schema.rs` (417 lines) — `GLOBAL_SCHEMA_VERSION`,
    `PROFILE_SCHEMA_VERSION` (tracked **independently**), `GLOBAL_TABLES` /
    `PROFILE_TABLES` (name lists the test-suite's table-existence sweep
    walks), and the base `GLOBAL_SCHEMA_SQL` / `PROFILE_SCHEMA_SQL`
    `CREATE TABLE` blobs. **This is only half the schema** — see "The
    dual-definition migration convention" below before assuming this file
    alone tells you a table's current columns.
  - `src-tauri/src/storage/migrations.rs` (490 lines) — `Migration` struct,
    `GLOBAL_MIGRATIONS` (7 entries) / `PROFILE_MIGRATIONS` (10 entries),
    `migrate_global()` / `migrate_profile()`, the `run_migrations()` engine
    (versioned, transactional, idempotent, one `schema_version` table per DB).
  - `src-tauri/src/storage/tests.rs` (1538 lines, `#[cfg(test)] mod tests`
    wired in from `mod.rs:22-23`) — integration tests over in-memory +
    tempdir-backed DBs, including the walled-memory and hybrid-search suites.

- **Schema versions (verified 2026-07-21, HEAD `ca54251`)**
  `GLOBAL_SCHEMA_VERSION: i32 = 7` (`schema.rs:12`); `PROFILE_SCHEMA_VERSION:
  i32 = 10` (`schema.rs:21`). These are two independent constants, not one
  shared version — the doc comment at `schema.rs:14-20` states this
  explicitly: a change that only touches per-profile tables (e.g. adding
  `tool_audit`, `tool_rules`, `memory_settings`) bumps `PROFILE_SCHEMA_VERSION`
  alone. `migrate_global`/`migrate_profile` each `debug_assert_eq!` their own
  migrations array's last version against their own constant
  (`migrations.rs:386-390`, `398-401`) — the two assertions are independent of
  each other, so there is no cross-DB entanglement to trip.

- **The dual-definition migration convention** — this is the single most
  important thing to understand before touching schema. Two different things
  can happen when a table/column is added:
  1. **A new column on an existing table** (`ALTER TABLE ... ADD COLUMN`) has
     no `IF NOT EXISTS` in SQLite, so it is added **only** in the migration
     that introduces it, never in `schema.rs`'s base blob. A fresh DB gets the
     column by running v1 (creates the table without it) and then that later
     migration (adds it) **in the same pass** — matching an *existing* DB's
     upgrade path exactly. Examples: `memory_facts.pinned` (global v2,
     `migrations.rs:52`), `endpoints.supports_native_tools` (global v4,
     `migrations.rs:134`), `skills.description`/`capabilities_required`/
     `approval_status`/`path`/`version`/`embedding` (global v5,
     `migrations.rs:148-153`), `model_catalog.sha256`/`status` (global v7,
     `migrations.rs:189-190`), `classifier_settings.redaction_enabled`
     (profile v5, `migrations.rs:273-274`).
  2. **A brand-new table** is typically **dual-defined**: its
     `CREATE TABLE IF NOT EXISTS` is written into `GLOBAL_SCHEMA_SQL` /
     `PROFILE_SCHEMA_SQL` (so a fresh install gets it at v1) **and** repeated
     verbatim inside its own numbered `Migration` (so an existing DB upgrades)
     — the numbered migration is then a no-op on a fresh install and a real
     `CREATE TABLE` on an upgrade. Examples: `agent_types` (global v6,
     `migrations.rs:155-178`, dual-defined with `schema.rs:117-128`);
     `tool_audit`/`tool_rules`/`classifier_settings`/`memory_settings`/
     `usage_events`/`work_items`/`seat_bindings`/`sandbox_config` (profile
     v2–v10, each dual-defined with its `PROFILE_SCHEMA_SQL` counterpart).
  Net effect: **`schema.rs` alone will NOT tell you a table's true current
  shape** — `pinned`, `supports_native_tools`, the `skills` metadata columns,
  and `model_catalog.sha256`/`status` all live only in `migrations.rs`. Read
  both files together, or trust `GLOBAL_TABLES`/`PROFILE_TABLES` (names only)
  plus the actual `CREATE TABLE`/`ALTER TABLE` statements across both files.

- **Key types / traits / functions**
  - `Storage` — `mod.rs:58` — clonable (`Arc`-backed) top-level handle.
    `Storage::open(base_path: &Path) -> Result<Self>` (`mod.rs:89`) creates
    `<base>/` and `<base>/profiles/`, opens `global.db`, runs migrations.
    `global()` (`mod.rs:111`), `open_profile(name)` (`mod.rs:178`),
    `memory_db_for_profile(profile)` (`mod.rs:137`, the §7 wall router — see
    below), `list_profile_names()` (`mod.rs:213`), `close_profile(name)`
    (`mod.rs:233`, evicts the cache entry to force a disk reopen). **`Storage`
    is genuinely `Send + Sync` with no manual/unsafe impl** — see the next
    bullet.
  - **The `Connection` now lives behind `parking_lot::Mutex`, not a bare
    field.** `GlobalDb { conn: parking_lot::Mutex<Connection> }`
    (`global.rs:417-419`) and `ProfileDb { conn: parking_lot::Mutex<Connection>,
    pub name: String }` (`profile.rs:223-228`). Every method locks around its
    access; `raw()` (`global.rs:445-447`, `profile.rs:270-272`) returns a
    `MutexGuard` — **the mutex is not reentrant**, so a caller holding a
    `raw()` guard must not call another locking method on the same handle
    (documented at both `raw()` sites). This replaced **all four**
    `unsafe impl Send + Sync` blocks that used to exist on `Storage` and
    `ProfileDb` (commit `ff64b3a`, 2026-07-18) — see Gotchas for why the old
    approach was actually unsound.
  - `ensure_sqlite_vec_registered()` — `mod.rs:40-52` — `pub(crate)`,
    `Once`-guarded, registers `sqlite_vec::sqlite3_vec_init` via
    `sqlite3_auto_extension` before the *first* `Connection::open` in the
    process. Called at the top of `GlobalDb::open`/`open_in_memory`
    (`global.rs:424`, `434`) and `ProfileDb::open`/`open_in_memory`
    (`profile.rs:238`, `256`).
  - `GlobalDb` (`global.rs:417`) — `open(path)` (`global.rs:423`),
    `open_in_memory()` (`global.rs:432`, test-only). Notable CRUD groups:
    `user_facts` (`global.rs:451-506`); `endpoints` incl. the Q1
    `supports_native_tools` flag (`global.rs:510-574`); `model_catalog` incl.
    the M8 `sha256`/`status` trust-anchor columns and `set_model_status`
    (`global.rs:578-637`); `skills` with an `approval_status` trust gate
    (`global.rs:1200-1294`); `agent_types` (Wave 4.3 personas), also gated by
    `approval_status`, plus `ensure_builtin_agent_types` — an idempotent
    `INSERT OR IGNORE` seed of two built-ins (`global.rs:1298-1397`);
    `app_settings` incl. the `skill_reflect_enabled` toggle
    (`global.rs:1401-1457`).
  - **Memory: `MemoryBucket`** (`global.rs:88-116`) — `Shared` vs
    `PrivateLocal`. Each variant maps to **three physically separate
    tables** (`table()`, `fts_table()`, `vectors_table()`): `Shared` →
    `memory_facts`/`memory_facts_fts`/`memory_vectors`; `PrivateLocal` →
    `memory_facts_private`/`memory_facts_private_fts`/
    `memory_vectors_private`. This is a **table split, not a filtered view**
    — see Invariants.
  - **Memory: keyword lane** — `fts_match_expr` (`global.rs:157-182`) turns a
    raw query into a quoted, stopword-filtered, OR'd FTS5 `MATCH` expression
    (injection-safe, and skips English function words so an OR-recall doesn't
    fire on every fact). `search_memory`/`search_memory_scoped`/
    `search_memory_for_recall` (`global.rs:866-918`) run `bm25()`-ranked FTS5
    queries via `search_bucket` (`global.rs:945-988`), gated on
    `allow_private` for the private-local bucket.
  - **Memory: meaning lane** — `semantic_search_bucket`
    (`global.rs:1073-1116`) runs `vec_distance_cosine(v.embedding, ?1)` as a
    **scalar SQL function directly against the `memory_vectors`/
    `memory_vectors_private` BLOB column** (not through a `vec0` virtual
    table — see Gotchas), with a `length(embedding) = ?` guard to skip any
    row whose blob isn't this embedder's dimension, gated at a `max_dist`
    cosine-distance threshold. `upsert_memory_embedding`/
    `facts_missing_embedding` (`global.rs:1120-1167`) write/backfill vectors.
  - **Memory: hybrid fusion** — `search_memory_scoped_hybrid`/
    `search_memory_for_recall_hybrid` (`global.rs:999-1066`) run the keyword
    and (when a query vector is supplied) semantic lanes together and merge
    them with `rrf_fuse` (`global.rs:190-214`, Reciprocal Rank Fusion,
    `1/(60+rank)` per list, summed, deduped by fact id). `None` for the
    embedder degrades cleanly to keyword-only.
  - `ProfileDb` (`profile.rs:223`) — `open(path, name)` (`profile.rs:237`),
    `open_in_memory(name)` (`profile.rs:255`, test-only), `name()`
    (`profile.rs:274`). Notable CRUD groups: `conversations`/`messages`
    (`profile.rs:280-505`, `list_messages_by_conversation` sorts
    `created_at ASC, rowid ASC` — see Gotchas); `search_messages`
    (`profile.rs:432-465`, the `session_search` tool's backing, case-
    insensitive substring over `user`/`assistant` turns only);
    `email_accounts`/`email_messages`, `calendar_events`, `tasks`,
    `cron_jobs` (largely unchanged CRUD, `profile.rs:655-918`); `usage_events`
    — `record_usage`/`usage_summary` (`profile.rs:924-961`, the Wave 3.2 cost
    ledger: `cost_usd: None` = unknown/"flying blind", never guessed);
    `work_items` — the Wave 4.4 one-queue substrate: `insert_work_item`
    (`profile.rs:969-995`, `INSERT OR IGNORE` on `claim_key` for exactly-once
    dedup), `claim_next_due_work` (`profile.rs:1001-1020`, one atomic
    `UPDATE ... WHERE id = (SELECT ... LIMIT 1) RETURNING ...` so two runners
    can never claim the same row), `finish_work_item` (`profile.rs:1026-1043`,
    guarded by both `WorkState::can_transition_to` and a SQL
    `WHERE state='running'` predicate — a terminal item can't be re-finished),
    `terminalize_orphaned_work` (`profile.rs:1048-1055`, boot-time crash
    recovery: fails any item left `running`, never silently re-runs a
    mutating action); `trm_logs` — `insert_trm_log`/`list_trm_logs`
    (`profile.rs:1074-1110`), `purge_trm_logs_older_than` (`profile.rs:1114-
    1120`, the 7-day retention helper — still no caller, see Gotchas);
    `tool_audit` — append-only, insert + list only (`profile.rs:1134-1191`);
    `tool_rules` — persisted `Always` grants, upsert on `(tool_name, pattern)`
    (`profile.rs:1203-1263`); `classifier_settings` — thresholds +
    `redaction_enabled`, always `sanitize`d toward *stricter*, never leakier
    (`profile.rs:1274-1414`); `memory_settings` — `semantic_search_enabled` +
    `walled`, single row id=1, defaults preserve pre-Wave-1 behavior
    (`profile.rs:1422-1459`, `MemorySettings::default` at `profile.rs:1551-
    1558`); `seat_bindings` — Wave 3.1 per-profile model seats, seat names
    `.trim()`med on every read/write (`profile.rs:1465-1516`); `sandbox_config`
    — single JSON row, M7 Tier-K per-profile network ceiling
    (`profile.rs:1339-1376`).
  - `migrate_global`/`migrate_profile` (`migrations.rs:383`, `395`) — both
    delegate to `run_migrations()` (`migrations.rs:406-453`): sets
    `PRAGMA foreign_keys = ON`, bootstraps `schema_version`, then for every
    `Migration` with `version > current` runs its SQL in one
    `conn.unchecked_transaction()` and records the version — each migration's
    failure leaves earlier ones' committed state untouched.

- **Data flow / how it fits**
  - `AppState` (`src-tauri/src/ipc/mod.rs:56`, `pub storage: Arc<Storage>`)
    is constructed once at app start. **Note the doc has moved**: `AppState`
    used to live in `agent/loop_mod.rs`; it is now defined in `ipc/mod.rs` and
    the module doc there (`ipc/mod.rs:17-24`) is the current explanation of
    why holding `Arc<Storage>` directly (no outer `Mutex`) at that boundary is
    sound — because `GlobalDb`/`ProfileDb` are each internally synchronized.
  - IPC commands call `state.storage.global()` / `state.storage.open_profile(name)`
    directly; the agent loop calls `profile_db.insert_trm_log(&entry)` after
    every classifier decision on a message (now at `agent/loop_mod.rs:1636`
    — the file has grown to 1825 lines since the last doc pass, so line
    numbers here drift fast; grep `insert_trm_log` to confirm before citing
    it elsewhere).
  - **Memory is LIVE, not a placeholder** — the previous version of this doc
    was wrong on this point. `Storage::memory_db_for_profile` is called from
    three real call sites: `tools/memory.rs` (the `recall_memory` tool, e.g.
    `tools/memory.rs:155`, `310`), `agent/memory_flush.rs:257` (the Wave 3.5
    background fact-flush/embed pipeline), and `agent/loop_mod.rs:833/909`
    (the per-turn curated-summary + relevance-gated auto-injection). The
    on-device embedder (`src-tauri/src/embedder.rs`, bge-small-en-v1.5 INT8
    ONNX, `EMBED_DIM = 384`) loads lazily behind `AppState.embedder:
    Option<Arc<EmbedderHandle>>` (`ipc/mod.rs:70-73`) only when a profile's
    `memory_settings().semantic_search_enabled` is true; every hybrid-search
    call site degrades to keyword-only automatically when the embedder isn't
    loaded (passes `None` for the query vector).
  - **Walled-vs-shared memory (§7) is IMPLEMENTED**, not just decided — the
    previous doc's claim that it was undesigned is stale.
    `Storage::memory_db_for_profile(profile)` (`mod.rs:137-173`) is the single
    routing choke point: it opens `profile` via `open_profile`, reads
    `memory_settings().walled`; `false` → returns the shared `global()`
    handle; `true` → opens/caches `walled-memory/<profile>.db` (its own
    `GlobalDb`-shaped file — the same schema, so all memory methods work
    unchanged; the non-memory tables it also creates are simply unused).
    **Fail-safe direction is asymmetric on purpose** (`mod.rs:123-136`
    doc comment): an *invalid/degenerate* profile name (`open_profile` errors)
    routes to shared — there's no real island to protect. But if the profile
    opens fine and its wall status itself is **unreadable** (a transient
    SQLite error, a corrupt settings table), the call **fails closed**
    (propagates `Err`) rather than defaulting to shared — every caller
    already skips the memory op on `Err`, so a wall can never be breached
    just because its status couldn't be read.
  - `GlobalDb::open`/`ProfileDb::open` both call
    `ensure_sqlite_vec_registered()` before `Connection::open`, then run
    their respective `migrate_*` before returning — callers never see an
    unmigrated connection.

- **Invariants (do NOT break)**
  - **Physical separation is the privacy boundary, at two grains.** At the
    profile grain: `global.db` holds only legitimately cross-profile data
    (endpoints, model catalog, shared memory tagged by `origin_profile`,
    skills, agent types, app settings); conversation-shaped data
    (messages, folders, tags, email, calendar, tasks, cron, `trm_logs`) stays
    in the per-profile DB. At the memory-sensitivity grain (PLAN §9):
    `memory_facts_private`/`memory_vectors_private` are **different tables**
    from `memory_facts`/`memory_vectors`, not a filtered view of one table —
    stated explicitly at `migrations.rs:38-44` as a leak-prevention design
    choice. A cloud-bound context assembly that only ever queries the
    `Shared` bucket structurally cannot see private-local data; don't
    "simplify" this into one table with a sensitivity column.
  - **`open_profile`'s name denylist is the only thing stopping a profile
    name from escaping `profiles/`** (`mod.rs:184-191`: rejects empty,
    contains `/`, contains `\`, contains `..`, or starts with `.`). Any new
    caller that constructs a profile DB path some other way must apply the
    same check. Covered by `storage_open_profile_rejects_path_traversal`
    (`tests.rs:729`).
  - **`trm_logs` is still the classifier-decision audit trail** — every
    private/public routing decision should be recorded via `insert_trm_log`
    (now called at `agent/loop_mod.rs:1636`). This remains a stated product
    invariant (PLAN §3/§4) — don't remove or bypass the call when touching
    classifier or message-send code.
  - **Migrations are append-only, and the dual-definition convention is not
    optional decoration.** Never edit `GLOBAL_SCHEMA_SQL`/`PROFILE_SCHEMA_SQL`
    or an existing `Migration`'s SQL once it has shipped — add a new
    `Migration { version: N+1, ... }`. A brand-new table should normally be
    dual-defined (in the base SQL blob **and** its own migration) from the
    day it ships, per the `agent_types`/`tool_audit`/etc. pattern above — a
    new column can only ever go in the migration (`ALTER TABLE` has no
    `IF NOT EXISTS`).
  - **`ensure_sqlite_vec_registered()` must run before the first
    `Connection::open` in the process** — `Once`-guarded specifically so
    every DB-open path calls it first (`global.rs:424,434`;
    `profile.rs:238,256`). Skipping it on a new connection-opening path makes
    `vec0`/`vec_distance_cosine` fail with "no such module"/"no such
    function".
  - **Foreign keys are ON but only per-connection** (`run_migrations` issues
    `PRAGMA foreign_keys = ON` at `migrations.rs:407`) — a raw
    `rusqlite::Connection` opened outside `GlobalDb::open`/`ProfileDb::open`
    won't have cascades (`memory_vectors.fact_id`, `messages.conversation_id`,
    `conversations.folder_id`, `session_tags.*`, `trm_logs.conversation_id`,
    `memory_vectors_private.fact_id`) enforced.

- **Gotchas / watch-items**
  - **`open_profile` does NOT trim the name — a known, unfixed gap.** The
    denylist (`mod.rs:184-191`) checks `is_empty()`/`contains('/')`/
    `contains('\\')`/`contains("..")`/`starts_with('.')` — it never calls
    `.trim()`. A caller-supplied `" personal"` or `"personal "` passes every
    check, is a **different** `HashMap` cache key, and resolves to a
    **different** file (`profiles/ personal.db` vs `profiles/personal .db`
    vs `profiles/personal.db`) than the intended profile — three silently
    confusable, non-sharing profiles from what a user would consider one
    typo. Contrast with `seat_bindings`, where `set_seat_binding`/
    `get_seat_binding`/`delete_seat_binding` (`profile.rs:1465-1516`) *do*
    `.trim()` the seat name — the same hygiene was never applied to profile
    names here. **Flagged as a follow-up, not yet fixed** — anyone touching
    profile creation/switching should close this (trim before the denylist
    check, or reject names with leading/trailing whitespace outright).
  - **`memory_vectors`/`memory_vectors_private` stay plain `BLOB` columns —
    sqlite-vec is used as a scalar function, not a `vec0` virtual table.**
    `semantic_search_bucket` (`global.rs:1073-1116`) computes
    `vec_distance_cosine(embedding, ?)` in an ordinary `SELECT` against the
    BLOB column; there is no `CREATE VIRTUAL TABLE ... USING vec0(...)`
    backing these tables. This is a legitimate, simpler way to use
    sqlite-vec for a modest row count (no shadow-table sync to maintain) —
    but it means "no `vec0` table exists" is **not** evidence the meaning
    lane is unwired, which the pre-2026-07-21 version of this doc got wrong.
    The separate `sqlite_vec_extension_loads_and_does_knn` test
    (`tests.rs:1318`) *does* create its own scratch `vec0` table — that is a
    pure extension-load smoke test, unrelated to how production actually
    queries `memory_vectors`. Don't conflate the two when reasoning about
    what's proven.
  - **`purge_trm_logs_older_than` still has no caller outside tests.** The
    7-day TRM-log retention policy (`profile.rs:1112-1120`, citing spec §3)
    remains a helper method only — a background task to invoke it on a
    schedule has still not been built.
  - **`api_key_encrypted` is still not actually encrypted.** `endpoints`'s
    `api_key_encrypted BLOB` column (`schema.rs:69`) just stores whatever
    bytes the caller passes (`insert_endpoint`/`update_endpoint`,
    `global.rs:510-566`); `hydrate_providers_from_storage`
    (`src-tauri/src/lib.rs:304-332`) reads it back with a bare
    `String::from_utf8`, and the comment at `lib.rs:321-323` still says
    "encryption is M4+ work." Don't assume API keys are protected at rest.
  - **`ProfileDb::name` is public and set once at open time**
    (`profile.rs:227`, `pub name: String`) — not re-derived from the file
    path on access. A `.db` file renamed on disk out-of-band leaves an
    already-open cached handle reporting the old name until
    `Storage::close_profile` + reopen.
  - **`list_messages_by_conversation` deliberately sorts by
    `created_at ASC, rowid ASC`, not by `id`** (`profile.rs:407-424`, comment
    explains: `created_at` is second-granularity and `id` is a random UUID,
    so two same-second messages would otherwise sort randomly; `rowid`
    preserves true insertion order). A future move to a `WITHOUT ROWID`
    `messages` table would silently break this.
  - **`update_message` uses `COALESCE(?, column)` per field**
    (`profile.rs:470-496`) — passing `None` preserves the existing value, so
    this method **cannot** intentionally null out a field (e.g. clearing
    `error` after a retry succeeds); a caller needing that must write a
    dedicated statement.
  - **`trm_logs` keeps its old name on purpose.** The `trm/` module was
    renamed to `classifier/` (2026-07-09) after the "Tiny-Recursive-Model"
    approach was dropped, but the `trm_logs` table/`TrmLog` struct name was
    kept because renaming a persisted table needs a migration nobody judged
    worth it yet. Don't "fix" the name without adding one — and expect
    classifier code to import `crate::storage::TrmLog`.
  - **Why the `Mutex<Connection>` fix landed (context for future concurrency
    work):** the prior `unsafe impl Send + Sync` rested on "every DB access
    is serialized through a `Mutex<Storage>` at the AppState boundary" — but
    the Wave 3.5 memory-flush (`agent/memory_flush.rs`) and Wave 4.2
    skill-reflection (`agent/skill_reflect.rs`) background tasks hold their
    own `Arc<Storage>` and call `storage.global()`/`memory_db_for_profile()`
    **concurrently** with the main loop, bypassing that boundary entirely —
    a real, exploitable soundness hole (two `&self` calls racing
    `rusqlite::Connection`'s non-atomic borrow flag is Rust-level UB even
    though bundled SQLite is C-thread-safe). Commit `ff64b3a` fixed it by
    moving the `Connection` behind `parking_lot::Mutex` in both DB types and
    removing all four `unsafe impl` blocks. If you add another background
    task that holds its own `Arc<Storage>`, this is now safe by construction
    — but keep the "don't call a second locking method while holding a
    `raw()` guard" rule in mind (non-reentrant mutex; `crash_recovery`'s
    `reconcile_profile_db` had to be rewritten around exactly this when the
    fix landed).

- **How to extend**
  - **New global-scope table:** add its `CREATE TABLE IF NOT EXISTS` to
    `GLOBAL_SCHEMA_SQL` **and** its name to `GLOBAL_TABLES` in `schema.rs`;
    bump `GLOBAL_SCHEMA_VERSION`; add a matching `Migration` to
    `GLOBAL_MIGRATIONS` whose SQL is the *same* `CREATE TABLE IF NOT EXISTS`
    text (the `agent_types` v6 pattern, `migrations.rs:155-178`) so it's a
    no-op on fresh installs and a real upgrade on existing DBs. Add a row
    type + typed CRUD to `global.rs`; add coverage in `tests.rs` (the
    table-existence sweep picks up `GLOBAL_TABLES` automatically; add a
    dedicated CRUD round-trip test too).
  - **New per-profile table:** same pattern in `PROFILE_SCHEMA_SQL` /
    `PROFILE_TABLES` / `PROFILE_MIGRATIONS` / `profile.rs`, bumping
    `PROFILE_SCHEMA_VERSION` instead.
  - **New column on an existing table:** this one is migration-only — write
    an `ALTER TABLE ... ADD COLUMN` in a new numbered `Migration` and do
    **not** touch the base `CREATE TABLE` in `schema.rs` (it has no
    `IF NOT EXISTS` semantics to reconcile against; see the `pinned`/
    `supports_native_tools`/`sha256`+`status` precedent).
  - **Extending memory sensitivity buckets:** `MemoryBucket`
    (`global.rs:88-116`) is the single switch point — a new bucket needs all
    three of `table()`/`fts_table()`/`vectors_table()` kept in sync, plus its
    own physically-separate tables (never a shared table with a
    discriminator column) to preserve the leak-prevention property.
  - **Changing an existing schema/behavior:** migrations are append-only —
    any breaking change (renamed column, changed type, dropped table) needs
    a new numbered migration doing the transformation in SQL, not an edit to
    an already-shipped `CREATE TABLE`/`Migration`.

- **Tests** — All in `src-tauri/src/storage/tests.rs` (`#[cfg(test)] mod
  tests` in `mod.rs:22-23`), plus a small migration-runner suite inside
  `migrations.rs:455-490`. Representative tests (2026-07-21, HEAD
  `ca54251`): `agent_types_crud_and_builtin_seed` (`tests.rs:79`),
  `model_catalog_carries_sha256_and_status` (`tests.rs:159`),
  `seat_bindings_crud_roundtrip` (`tests.rs:183`),
  `storage_open_profile_rejects_path_traversal` (`tests.rs:729`),
  `walled_profile_memory_is_physically_separate_and_survives_toggle_back`
  (`tests.rs:773`), `memory_routing_fails_closed_when_wall_status_is_unreadable`
  (`tests.rs:844`), `memory_search_keyword_and_bucket_isolation`
  (`tests.rs:963`), `memory_fts_stays_in_sync_on_delete` (`tests.rs:1081`),
  `sqlite_vec_extension_loads_and_does_knn` (`tests.rs:1318`),
  `semantic_lane_finds_meaning_matches_without_keyword_overlap`
  (`tests.rs:1358`), `semantic_lane_respects_private_wall_profile_scope_and_dim_guard`
  (`tests.rs:1405`), `hybrid_fuses_keyword_and_semantic_lanes`
  (`tests.rs:1483`), `facts_missing_embedding_backfill_worklist`
  (`tests.rs:1522`). Run with:
  ```
  cd src-tauri && cargo test storage::
  ```
  Full crate: `cargo test --lib` from `src-tauri/` — 542 tests passing as of
  2026-07-21 (HEAD `ca54251`).
