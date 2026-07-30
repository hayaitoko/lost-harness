//! Storage layer — two-database architecture (global + per-profile).
//!
//! Spec §1 + §5. The agent always knows which profile it's operating in
//! (writes go to the active profile; reads can cross profiles on request).
//!
//! Layout on disk:
//! ```text
//! <base_path>/
//!   global.db
//!   profiles/
//!     <name>.db
//! ```
//!
//! Typical default base_path: `~/Documents/Lost-Harness/` (chosen at install
//! time per spec §2).

pub mod global;
pub mod migrations;
pub mod profile;
pub mod schema;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};

use anyhow::{Context, Result};
use parking_lot::Mutex;

pub use global::{GlobalDb, *};
pub use profile::{ProfileDb, *};

/// Register the `sqlite-vec` extension for every connection opened in this
/// process. `sqlite3_auto_extension` applies to all *future* connections, so
/// this must run before the first `Connection::open`; the `Once` makes it
/// safe (and cheap) to call from every DB-open path. After this, `vec0`
/// virtual tables + the KNN `MATCH` operator are available — the "by meaning"
/// half of memory search (the keyword half is bundled SQLite's FTS5).
pub(crate) fn ensure_sqlite_vec_registered() {
    static VEC_INIT: Once = Once::new();
    VEC_INIT.call_once(|| {
        // SAFETY: the standard sqlite-vec + rusqlite registration. The init
        // fn has the C extension-entry-point signature; `sqlite3_auto_extension`
        // stores it for use on every subsequent connection open.
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

/// Validate a profile name (B3, closing the 2026-07-18 whitespace/confusable
/// gap). A **strict ASCII allowlist** (`[A-Za-z0-9_-]`, ≤64 chars) rather than a
/// Unicode denylist: blocking homoglyphs/confusables one-by-one is an arms race
/// (Cyrillic а/е/о, zero-width joiners, combining marks, NFKC lookalikes…),
/// while allowlisting sidesteps the whole class AND every real name the app has
/// generated (`personal`/`work`/`school`/`developer`) passes. This also rejects
/// all whitespace-padding (`" work"`, `"work "`, `"wo\trk"`) — three confusable,
/// distinct `.db` files otherwise. `pub(crate)` so the `send_message` IPC
/// boundary enforces the same rule before it touches `args.profile`.
pub(crate) fn validate_profile_name(name: &str) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("invalid profile name: empty");
    }
    if name.len() > 64 {
        anyhow::bail!("invalid profile name: too long ({} chars)", name.len());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        anyhow::bail!(
            "invalid profile name: {name:?} (only ASCII letters, digits, '_' and '-' allowed)"
        );
    }
    // Defense-in-depth: the allowlist already excludes '.', but keep the
    // explicit traversal guard in case the allowlist is ever loosened.
    if name.starts_with('.') || name.contains("..") {
        anyhow::bail!("invalid profile name: {name:?}");
    }
    Ok(())
}

/// Top-level storage handle. Owns the global DB and a registry of
/// open per-profile DBs. Cheap to clone (`Arc` inside) so it can be
/// passed to the agent loop, IPC handlers, and TRM concurrently.
#[derive(Clone)]
pub struct Storage {
    inner: Arc<StorageInner>,
}

// `Storage` is genuinely `Send + Sync` with no manual impl needed: both
// `GlobalDb` and `ProfileDb` hold their `rusqlite::Connection` behind a
// `parking_lot::Mutex`, which makes each of them `Send + Sync` on its own
// (`Mutex<T>: Sync` when `T: Send`, and `Connection: Send`). Every field of
// `StorageInner` below is therefore `Send + Sync`, and so is `Arc<StorageInner>`.

struct StorageInner {
    /// Absolute path of the storage root (e.g. `~/Documents/Lost-Harness/`).
    base_path: PathBuf,
    global: Arc<GlobalDb>,
    /// Cached open profile DBs. Mutex is fine — connections are cheap to
    /// clone internally and the agent is single-threaded per profile.
    profiles: Mutex<std::collections::HashMap<String, Arc<ProfileDb>>>,
    /// Cached open memory DBs for WALLED profiles (§7). A walled profile's
    /// memory lives in its own physically-separate DB under `walled-memory/`,
    /// never in `global.db` — so toggling the wall back off can't retroactively
    /// spill what was written while private (the data was never in the shared
    /// pool). Reuses the `GlobalDb` shape so all memory methods work unchanged;
    /// the non-memory tables it also creates are simply unused.
    walled_memory: Mutex<std::collections::HashMap<String, Arc<GlobalDb>>>,
}

impl Storage {
    /// Open (or create) the storage tree at `base_path`.
    ///
    /// Creates `base_path/` and `base_path/profiles/` if missing, then
    /// opens `global.db` and runs migrations.
    pub fn open(base_path: &Path) -> Result<Self> {
        std::fs::create_dir_all(base_path)
            .with_context(|| format!("creating storage root {}", base_path.display()))?;
        let profiles_dir = base_path.join("profiles");
        std::fs::create_dir_all(&profiles_dir)
            .with_context(|| format!("creating profiles dir {}", profiles_dir.display()))?;

        let global_path = base_path.join("global.db");
        let global = GlobalDb::open(&global_path)
            .with_context(|| format!("opening global.db at {}", global_path.display()))?;

        Ok(Self {
            inner: Arc::new(StorageInner {
                base_path: base_path.to_path_buf(),
                global: Arc::new(global),
                profiles: Mutex::new(std::collections::HashMap::new()),
                walled_memory: Mutex::new(std::collections::HashMap::new()),
            }),
        })
    }

    /// Borrow the shared global DB.
    pub fn global(&self) -> &GlobalDb {
        &self.inner.global
    }

    /// The memory database a given profile's facts read from and write to
    /// (Wave 1.5 / §7). A **shared** profile (the default) uses the shared
    /// `global.db`; a **walled** profile uses its own physically-separate DB
    /// under `walled-memory/<name>.db`, which is opened + cached on first use.
    /// Every memory call site routes through here so the wall is enforced in
    /// one place — a walled profile never touches `global.db`'s memory, and
    /// vice versa.
    ///
    /// **Fail-safe direction for the wall (§7 invariant):** the two failure
    /// modes are handled differently on purpose.
    /// - `open_profile` returns `Err` — an invalid / degenerate name (path
    ///   traversal, empty, a `Default` `ExecCtx` in a tool test). This is never
    ///   a real walled profile — there's no island to protect, and no valid
    ///   file path to route to — so it uses the shared store.
    /// - the profile opens but its wall status is **unreadable** (a transient
    ///   SQLite busy/I-O error, a corrupt settings table): we **fail closed** —
    ///   propagate the `Err` rather than route a possibly-walled profile's
    ///   memory to the shared `global.db`. Every caller already skips the memory
    ///   op on `Err` (injection is dropped; a save/recall surfaces the error),
    ///   so a wall is never breached just because its status couldn't be read.
    ///   Defaulting the unreadable case to "shared" would be a privacy
    ///   loosening, which the invariant forbids.
    pub fn memory_db_for_profile(&self, profile: &str) -> Result<Arc<GlobalDb>> {
        // An unopenable profile (invalid/degenerate name) has no island to
        // protect and no valid path to route to → shared store.
        let db = match self.open_profile(profile) {
            Ok(db) => db,
            Err(_) => return Ok(self.inner.global.clone()),
        };
        // The profile opened; its wall status must be READ successfully. A read
        // error fails closed (propagates) — we never assume "not walled".
        let walled = db
            .memory_settings()
            .context("resolving memory store: reading the profile's wall status (failing closed)")?
            .walled;
        if !walled {
            return Ok(self.inner.global.clone());
        }
        // Fast path — already open.
        if let Some(existing) = self.inner.walled_memory.lock().get(profile) {
            return Ok(existing.clone());
        }
        // `open_profile` above already validated the name (rejects traversal),
        // so the file name is safe to build. Walled memory DBs live in their own
        // directory to avoid any collision with `profiles/<name>.db`.
        let dir = self.inner.base_path.join("walled-memory");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating walled-memory dir {}", dir.display()))?;
        let path = dir.join(format!("{profile}.db"));
        let db = Arc::new(
            GlobalDb::open(&path)
                .with_context(|| format!("opening walled memory DB at {}", path.display()))?,
        );
        self.inner
            .walled_memory
            .lock()
            .insert(profile.to_string(), db.clone());
        Ok(db)
    }

    /// Open (or return the cached open handle for) a per-profile DB.
    /// Profile name is the file stem of `profiles/<name>.db`. Special
    /// characters in the name should be sanitized by the caller.
    pub fn open_profile(&self, name: &str) -> Result<Arc<ProfileDb>> {
        // Strict validation (B3): rejects path traversal AND whitespace-padded /
        // confusable names — see `validate_profile_name`.
        validate_profile_name(name)?;
        // Canonicalize to lowercase (review finding): on a case-INSENSITIVE
        // filesystem — macOS APFS / Windows NTFS, exactly what we ship on —
        // `work.db` and `Work.db` are the SAME inode, so `"work"` and `"Work"`
        // must map to the SAME cache key too. Otherwise two "profiles" would be
        // cached as distinct handles over one physical DB and silently share
        // data — defeating the very isolation B3 protects (a walled profile's
        // "physically separate" store included). Folding here means the cache
        // key and the filename always agree.
        let name = name.to_ascii_lowercase();
        // Fast path — already open (under the canonical key).
        if let Some(existing) = self.inner.profiles.lock().get(&name) {
            return Ok(existing.clone());
        }
        let path = self
            .inner
            .base_path
            .join("profiles")
            .join(format!("{name}.db"));
        let db = ProfileDb::open(&path, &name)?;
        let arc = Arc::new(db);
        self.inner.profiles.lock().insert(name, arc.clone());
        Ok(arc)
    }

    /// Absolute path of the storage root.
    pub fn base_path(&self) -> &Path {
        &self.inner.base_path
    }

    /// List the names of profile DBs that exist on disk (e.g. for a profile
    /// picker in Settings). Does not open them.
    pub fn list_profile_names(&self) -> Result<Vec<String>> {
        let dir = self.inner.base_path.join("profiles");
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir)
            .with_context(|| format!("reading profiles dir {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("db") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    out.push(stem.to_string());
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Drop a cached profile DB handle. The next `open_profile` will reopen
    /// from disk. Use this to force a re-read after external writes.
    pub fn close_profile(&self, name: &str) -> bool {
        self.inner.profiles.lock().remove(name).is_some()
    }
}
