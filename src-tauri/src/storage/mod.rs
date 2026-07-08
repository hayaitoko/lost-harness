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
use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::Mutex;

pub use global::{GlobalDb, *};
pub use profile::{ProfileDb, *};

/// Top-level storage handle. Owns the global DB and a registry of
/// open per-profile DBs. Cheap to clone (`Arc` inside) so it can be
/// passed to the agent loop, IPC handlers, and TRM concurrently.
#[derive(Clone)]
pub struct Storage {
    inner: Arc<StorageInner>,
}

// SAFETY (M1): `Storage` is logically single-writer per profile (the
// agent loop and IPC commands are serialized through a `Mutex<Storage>`
// at the AppState boundary — see `ipc::AppState`). The internal
// `rusqlite::Connection` is `!Sync` due to its use of `RefCell`, but
// `Mutex<Storage>` guarantees no two threads touch it concurrently, so
// the manual `Send + Sync` impls below are sound for the M1 usage.
// If a future milestone introduces truly concurrent access paths,
// replace this with proper `parking_lot::Mutex<Connection>` inside
// `GlobalDb` / `ProfileDb` and remove the manual impls.
unsafe impl Send for Storage {}
unsafe impl Sync for Storage {}

struct StorageInner {
    /// Absolute path of the storage root (e.g. `~/Documents/Lost-Harness/`).
    base_path: PathBuf,
    global: GlobalDb,
    /// Cached open profile DBs. Mutex is fine — connections are cheap to
    /// clone internally and the agent is single-threaded per profile.
    profiles: Mutex<std::collections::HashMap<String, Arc<ProfileDb>>>,
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
                global,
                profiles: Mutex::new(std::collections::HashMap::new()),
            }),
        })
    }

    /// Borrow the global DB.
    pub fn global(&self) -> &GlobalDb {
        &self.inner.global
    }

    /// Open (or return the cached open handle for) a per-profile DB.
    /// Profile name is the file stem of `profiles/<name>.db`. Special
    /// characters in the name should be sanitized by the caller.
    pub fn open_profile(&self, name: &str) -> Result<Arc<ProfileDb>> {
        // Fast path — already open.
        if let Some(existing) = self.inner.profiles.lock().get(name) {
            return Ok(existing.clone());
        }
        // Sanitize: caller is responsible, but refuse path traversal.
        if name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || name.contains("..")
            || name.starts_with('.')
        {
            anyhow::bail!("invalid profile name: {name:?}");
        }
        let path = self
            .inner
            .base_path
            .join("profiles")
            .join(format!("{name}.db"));
        let db = ProfileDb::open(&path, name)?;
        let arc = Arc::new(db);
        self.inner
            .profiles
            .lock()
            .insert(name.to_string(), arc.clone());
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
