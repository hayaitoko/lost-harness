//! Filesystem tools — the first real tools wired through the M3 spine. Spec
//! `docs/PLAN.md` §8 (M3 build order item 10: "file read/list/search").
//!
//! Tools:
//! - **read_file** (RiskClass::Safe) — read UTF-8 text
//! - **list_dir** (RiskClass::Safe) — list directory entries
//! - **search_files** (RiskClass::Safe) — substring search over names + contents
//! - **write_file** (RiskClass::Write) — create or overwrite
//! - **edit_file** (RiskClass::Write) — replace a unique substring
//! - **delete_file** (RiskClass::Write) — remove a single file (not a directory)
//!
//! ### Confinement
//! Every tool requires `Capability::Filesystem` and is confined to a single
//! **workspace root**: paths are relative, `..` is rejected, and the resolved
//! target must stay inside the root. This is defense-in-depth *below* the hook
//! chain — even before a call reaches the sandbox/permission gates, a tool
//! structurally cannot wander to `/etc/shadow`.
//!
//! ### Risk classes
//! - **Safe** (read_file, list_dir, search_files): no approval needed.
//! - **Write** (write_file, edit_file, delete_file): routes through the
//!   approval spine (`RiskClass::Write`). `write_file` and `edit_file` also
//!   enforce a read-before-write guard — an existing file must have been read in
//!   the current conversation before it can be overwritten or edited — so the
//!   model cannot clobber content it never saw. `delete_file` does NOT: removal
//!   is not a blind *edit*, and it is gated by approval like the other two.
//!
//! ### Confinement is descriptor-relative, not pathname-relative (M-04)
//! Checking a *pathname* and then operating on that same pathname is two
//! independent resolutions of one string. Anything that can create names in the
//! workspace (the agent itself, or a concurrently running `shell_exec`) can swap
//! a directory for a symlink in between, and the operation lands wherever the
//! link now points — the check said one thing, the use did another.
//!
//! So on unix the workspace root is opened **once**, and `rel` is then walked one
//! component at a time with `openat(2) | O_NOFOLLOW | O_CLOEXEC`. Each hop names
//! an inode rather than a string to be re-resolved, a component that is (or
//! becomes) a symlink fails the hop with `ELOOP` instead of redirecting it, and
//! the final `openat`/`renameat`/`unlinkat`/`fstatat` is relative to the pinned
//! parent descriptor. The thing checked IS the thing used, so there is no window
//! to race. See [`confined`] for the details; [`Target`]/[`DirTarget`] are the
//! handles the six tools use.
//!
//! Canonical pathnames survive in exactly one role: as the opaque key for the
//! read-before-write set (it must carry the on-disk casing, so a case-insensitive
//! filesystem doesn't falsely refuse a genuine read→write of one file). No byte
//! is read, written or unlinked by pathname.

use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;

use serde_json::json;

use crate::tools::{Capability, ExecCtx, RiskClass, Tool, ToolInput, ToolResult};

/// Cap on a single file read. Kept in lockstep with `MAX_WRITE_BYTES` so there
/// is no "writable but unreadable" dead zone: any file small enough to overwrite
/// is small enough to read in full, so the read-before-write guard can always be
/// satisfied. (Was 256 KiB, which trapped 256 KiB–1 MiB files — overwritable but
/// never readable, so the guard refused them forever. Flagged in review.)
const MAX_READ_BYTES: u64 = MAX_WRITE_BYTES as u64;

/// Bounds for `search_files`, so a deep or huge tree can't hang the agent.
const SEARCH_MAX_DEPTH: usize = 8;
const SEARCH_MAX_FILES_SCANNED: usize = 4000;
const SEARCH_MAX_RESULTS: usize = 50;
const SEARCH_MAX_FILE_BYTES: u64 = 256 * 1024;

// ── path safety ────────────────────────────────────────────────────────────

/// Split a workspace-relative path into its `Normal` components, rejecting
/// every form that could climb out of the workspace. `.` segments are dropped.
/// One place so the lexical rule can never drift between the pathname resolvers
/// and the descriptor-relative walk.
fn rel_components(rel: &str) -> Result<Vec<&std::ffi::OsStr>, String> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(format!(
            "path must be relative to the workspace, got: {rel}"
        ));
    }
    let mut out = Vec::new();
    for comp in rel_path.components() {
        match comp {
            Component::ParentDir => return Err("path may not contain '..'".to_string()),
            Component::Prefix(_) | Component::RootDir => {
                return Err("path may not be absolute or contain a drive prefix".to_string())
            }
            Component::CurDir => {}
            Component::Normal(s) => out.push(s),
        }
    }
    Ok(out)
}

/// Resolve `rel` against `root`, rejecting anything that could escape the
/// workspace. Requires the target to exist (uses `canonicalize`).
///
/// This is the LEXICAL + canonical-path check, and on unix it is no longer what
/// actually performs the I/O: it supplies the `..`/absolute rejection and the
/// canonical path used as the read-before-write set key (which must carry the
/// on-disk casing, so a case-insensitive FS doesn't falsely refuse a real
/// read→write of one file). The authoritative resolution is the
/// descriptor-relative walk in [`confined`], which cannot escape the root no
/// matter what this function concluded.
fn resolve_within(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel_path = Path::new(rel);
    rel_components(rel)?;
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("workspace root unavailable: {e}"))?;
    let canon = root
        .join(rel_path)
        .canonicalize()
        .map_err(|e| format!("cannot access '{rel}': {e}"))?;
    if !canon.starts_with(&canon_root) {
        return Err(format!("path escapes the workspace: {rel}"));
    }
    Ok(canon)
}

fn arg_str<'a>(input: &'a ToolInput, key: &str) -> Option<&'a str> {
    input.args.get(key).and_then(|v| v.as_str())
}

// ── profile → workspace root: the ONE normalizer ────────────────────────────

/// Where one profile's files live, relative to the shared workspace base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileScope {
    /// The EMPTY profile — the default/scratch `ExecCtx`. `open_profile` also
    /// rejects `""`, so it can never alias a real profile; it resolves to the
    /// shared base itself, which is the pre-Tier-P contract every
    /// non-profile-bound caller still depends on.
    SharedBase,
    /// A real profile: its own physically-separate `base/<name>` subtree.
    Subdir(String),
}

/// **THE** profile normalizer. Every profile→path decision funnels through
/// this one function, so the filesystem bucket and the DB bucket cannot drift.
///
/// The rule mirrors [`crate::storage::validate_profile_name`] +
/// `Storage::open_profile` byte-for-byte, and that equivalence is load-bearing:
/// - the same strict ASCII allowlist (`[A-Za-z0-9_-]`, ≤64 chars, no leading
///   `.`, no `..`). A name storage refuses to open a DB for must not be handed
///   a filesystem tree either.
/// - the same `to_ascii_lowercase` fold, because `open_profile` folds both its
///   cache key and `profiles/<name>.db`. On the case-INSENSITIVE filesystems we
///   ship on (APFS / NTFS) `Work` and `work` are one inode, so folding here is
///   what keeps one profile = one DB = one tree.
///
/// **Fail-closed.** Anything else is an `Err`, not a path. There is
/// deliberately no sentinel directory and no silent fall back to a real
/// location, so a hostile profile string can never be turned into a place on
/// disk. (An earlier cut returned `base/__invalid_profile__`, which converted a
/// validation failure into a genuine directory — exactly the wrong shape.)
pub fn profile_scope(profile: &str) -> Result<ProfileScope, String> {
    if profile.is_empty() {
        return Ok(ProfileScope::SharedBase);
    }
    if profile.len() > 64 {
        return Err(format!(
            "invalid profile name: too long ({} chars)",
            profile.len()
        ));
    }
    if !profile
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(format!(
            "invalid profile name {profile:?} (only ASCII letters, digits, '_' and '-' allowed)"
        ));
    }
    // Defence in depth: the allowlist already excludes '.', but keep the
    // explicit traversal guard in case it is ever loosened.
    if profile.starts_with('.') || profile.contains("..") {
        return Err(format!("invalid profile name {profile:?}"));
    }
    Ok(ProfileScope::Subdir(profile.to_ascii_lowercase()))
}

/// Fail-closed workspace root — what every fs tool uses. An invalid profile is
/// an error the tool hands back to the model; it never becomes a directory.
fn profile_workspace_root(base: &Path, profile: &str) -> Result<PathBuf, String> {
    Ok(match profile_scope(profile)? {
        ProfileScope::SharedBase => base.to_path_buf(),
        ProfileScope::Subdir(name) => base.join(name),
    })
}

/// Infallible *peek* at the same normalizer, for the callers that only need a
/// path to look at rather than a place to write: the `ProtectedPathHook`'s
/// resolve, the legacy-workspace migration, and the read-only Files IPC. On an
/// invalid profile it degenerates to `base`, which is safe for exactly those
/// callers (the hook then over-matches → an extra Ask, never a miss; the
/// migration's `dest == workspace_root` check short-circuits; the IPC
/// explicitly rejects `ws == ws_root`). Anything that WRITES must use
/// [`profile_workspace_root`] and fail closed instead.
pub fn profile_workspace_path(base: &std::path::Path, profile: &str) -> PathBuf {
    profile_workspace_root(base, profile).unwrap_or_else(|_| base.to_path_buf())
}

// ── async wrappers for sync resolvers (spawn_blocking) ─────────────

async fn resolve_within_async(root: PathBuf, rel: String) -> Result<PathBuf, String> {
    tokio::task::spawn_blocking(move || resolve_within(&root, &rel))
        .await
        .map_err(|e| format!("resolve task failed: {e}"))?
}

async fn resolve_within_new_async(root: PathBuf, rel: String) -> Result<PathBuf, String> {
    tokio::task::spawn_blocking(move || resolve_within_new(&root, &rel))
        .await
        .map_err(|e| format!("resolve task failed: {e}"))?
}

// ── descriptor-relative confinement ────────────────────────────────────────

/// What a directory entry is, decided WITHOUT following a symlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Dir,
    File,
    Symlink,
    Other,
}

impl EntryKind {
    fn as_str(self) -> &'static str {
        match self {
            EntryKind::Dir => "dir",
            EntryKind::File => "file",
            EntryKind::Symlink => "symlink",
            EntryKind::Other => "other",
        }
    }
}

/// **The** confinement layer (M-04). Every byte an fs tool reads, writes or
/// unlinks goes through here.
///
/// The old shape was check-then-use on a *pathname*: `canonicalize()` proved the
/// resolved target was inside the workspace, and then a second, independent
/// pathname resolution inside `open()` / `rename()` / `remove_file()` actually
/// went to disk. Those are two different resolutions of the same string, and
/// between them an attacker who can create names in the workspace (the agent
/// itself, or a concurrently-running `shell_exec`) can swap an intermediate
/// directory for a symlink — the check passes on the real directory, the use
/// lands wherever the symlink points.
///
/// Here the *pinned descriptor is the identity*. We open the workspace root
/// once, then walk `rel` one component at a time with `openat(2)` +
/// `O_NOFOLLOW`, so:
/// - each descriptor names a specific inode, not a string that gets re-resolved;
/// - a component that is (or becomes) a symlink fails the `openat` with `ELOOP`
///   instead of redirecting the walk;
/// - the final operation is `openat` / `renameat` / `unlinkat` / `fstatat`
///   **relative to the pinned parent descriptor**, so the object we checked is
///   the object we act on. There is no window to swap because there is no second
///   pathname resolution.
///
/// The workspace ROOT itself is opened by pathname and DOES follow symlinks —
/// deliberately: on macOS `std::env::temp_dir()` lives under `/var`, which is a
/// symlink to `/private/var`, and the user's own storage root may legitimately
/// sit behind one. What must never be followed is anything *below* the root, and
/// nothing below it is.
#[cfg(unix)]
mod confined {
    use super::{rel_components, EntryKind};
    use std::ffi::{CStr, CString, OsStr};
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    /// Flags for every directory descriptor we hold. `O_CLOEXEC` matters: a
    /// `shell_exec` subprocess must not inherit a live handle to a workspace
    /// directory (that handle would itself be an escape hatch).
    const DIR_FLAGS: libc::c_int =
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

    /// A workspace path resolved to a **pinned parent directory descriptor** plus
    /// the leaf's name. Every operation on the leaf is `*at()`-relative to
    /// `parent`.
    pub(super) struct Confined {
        pub parent: OwnedFd,
        pub leaf: CString,
    }

    fn cname(name: &OsStr) -> Result<CString, String> {
        CString::new(name.as_bytes()).map_err(|_| "path component contains a NUL byte".to_string())
    }

    fn last_err(what: &str) -> String {
        format!("{what}: {}", std::io::Error::last_os_error())
    }

    /// Mode for a file we create. `openat` is variadic and IGNORES the mode
    /// unless `O_CREAT` is set, but it must be PASSED whenever `O_CREAT` is set —
    /// omitting it leaves the kernel reading a garbage variadic slot, which
    /// produces a file with arbitrary permissions (often unreadable). `0o600` is
    /// then narrowed by the process umask, exactly as `std::fs::write`'s `0o666`
    /// would be, but private by default: workspace files are the agent's, not the
    /// world's.
    const CREATE_MODE: libc::mode_t = 0o600;

    /// `openat(2)`, returning an owned descriptor. `mode` MUST be `Some` iff
    /// `flags` contains `O_CREAT`.
    fn openat(
        dirfd: BorrowedFd<'_>,
        name: &CStr,
        flags: libc::c_int,
        mode: Option<libc::mode_t>,
    ) -> Result<OwnedFd, String> {
        debug_assert_eq!(
            flags & libc::O_CREAT != 0,
            mode.is_some(),
            "openat: a mode must be supplied for exactly the O_CREAT opens"
        );
        // SAFETY: `dirfd` is a live borrowed descriptor, `name` is a NUL-
        // terminated C string that outlives the call, the variadic mode argument
        // is supplied whenever `O_CREAT` asks for one, and the returned fd is
        // immediately wrapped in `OwnedFd` so it is closed exactly once.
        let fd = unsafe {
            match mode {
                Some(m) => libc::openat(dirfd.as_raw_fd(), name.as_ptr(), flags, m as libc::c_uint),
                None => libc::openat(dirfd.as_raw_fd(), name.as_ptr(), flags),
            }
        };
        if fd < 0 {
            return Err(last_err(&format!("open '{}'", name.to_string_lossy())));
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Open the workspace root. Symlinks in the ROOT's own path are followed on
    /// purpose (see the module doc); nothing below it ever is.
    fn open_root(root: &Path) -> Result<OwnedFd, String> {
        let dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(root)
            .map_err(|e| format!("workspace root unavailable: {e}"))?;
        Ok(OwnedFd::from(dir))
    }

    /// Walk every component of `rel` from the root, `O_NOFOLLOW` per hop.
    /// `keep_leaf = false` stops at the leaf's parent (the leaf is returned as a
    /// name to be used `*at()`-relative); `keep_leaf = true` descends into the
    /// leaf too, which is what `list_dir` / the search walk want.
    fn walk(root: &Path, rel: &str, keep_leaf: bool) -> Result<(OwnedFd, Option<CString>), String> {
        let comps = rel_components(rel)?;
        let mut dirfd = open_root(root)?;
        let split = if keep_leaf {
            comps.len()
        } else {
            comps.len().saturating_sub(1)
        };
        for comp in &comps[..split] {
            let name = cname(comp)?;
            dirfd = openat(dirfd.as_fd(), &name, DIR_FLAGS, None).map_err(|e| {
                format!(
                    "cannot descend into '{}' (a symlinked or missing path component is refused): {e}",
                    comp.to_string_lossy()
                )
            })?;
        }
        let leaf = if keep_leaf {
            None
        } else {
            Some(cname(
                comps
                    .last()
                    .copied()
                    .ok_or_else(|| format!("path has no filename: {rel}"))?,
            )?)
        };
        Ok((dirfd, leaf))
    }

    /// Resolve to (pinned parent descriptor, leaf name).
    pub(super) fn resolve(root: &Path, rel: &str) -> Result<Confined, String> {
        let (parent, leaf) = walk(root, rel, false)?;
        Ok(Confined {
            parent,
            leaf: leaf.expect("keep_leaf = false always yields a leaf"),
        })
    }

    /// Resolve all the way to a pinned DIRECTORY descriptor.
    pub(super) fn resolve_dir(root: &Path, rel: &str) -> Result<OwnedFd, String> {
        let (dirfd, _) = walk(root, rel, true)?;
        Ok(dirfd)
    }

    /// `fstatat(..., AT_SYMLINK_NOFOLLOW)` on the leaf, relative to its pinned
    /// parent — the race-free equivalent of an `lstat` on the pathname.
    pub(super) fn lstat_leaf(c: &Confined) -> Option<libc::stat> {
        // SAFETY: zeroed `stat` is a valid initial value for `fstatat` to fill;
        // `parent`/`leaf` are live for the call.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::fstatat(
                c.parent.as_raw_fd(),
                c.leaf.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc == 0 {
            Some(st)
        } else {
            None
        }
    }

    fn kind_of_mode(mode: libc::mode_t) -> EntryKind {
        match mode & libc::S_IFMT {
            libc::S_IFDIR => EntryKind::Dir,
            libc::S_IFREG => EntryKind::File,
            libc::S_IFLNK => EntryKind::Symlink,
            _ => EntryKind::Other,
        }
    }

    /// Open the leaf for reading, `O_NOFOLLOW`, relative to its pinned parent.
    pub(super) fn open_read(c: &Confined) -> Result<std::fs::File, String> {
        let fd = openat(
            c.parent.as_fd(),
            &c.leaf,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            None,
        )?;
        Ok(std::fs::File::from(fd))
    }

    /// Atomically replace the leaf: create a fresh temp entry in the SAME pinned
    /// directory with `O_CREAT | O_EXCL | O_NOFOLLOW`, write it, then
    /// `renameat` it over the leaf — both sides of the rename resolved against
    /// the same pinned descriptor, so neither can be redirected.
    pub(super) fn atomic_replace(c: &Confined, content: &str) -> Result<(), String> {
        use std::io::Write;
        let tmp_name = cname(OsStr::new(&format!(
            ".{}.tmp-{}",
            c.leaf.to_string_lossy(),
            uuid::Uuid::new_v4()
        )))?;
        let fd = openat(
            c.parent.as_fd(),
            &tmp_name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            Some(CREATE_MODE),
        )?;
        let mut file = std::fs::File::from(fd);
        // On ANY failure clean the temp entry up, so a failed write leaves the
        // workspace exactly as it was (no orphaned `.tmp` residue).
        if let Err(e) = file
            .write_all(content.as_bytes())
            .and_then(|()| file.sync_all())
        {
            drop(file);
            unlink_name(&c.parent, &tmp_name);
            return Err(format!("write temp file: {e}"));
        }
        drop(file);
        // SAFETY: both descriptors/names are live for the call.
        let rc = unsafe {
            libc::renameat(
                c.parent.as_raw_fd(),
                tmp_name.as_ptr(),
                c.parent.as_raw_fd(),
                c.leaf.as_ptr(),
            )
        };
        if rc != 0 {
            let err = last_err("finalize write");
            unlink_name(&c.parent, &tmp_name);
            return Err(err);
        }
        Ok(())
    }

    fn unlink_name(parent: &OwnedFd, name: &CStr) {
        // SAFETY: `parent` and `name` are live; failure is ignored on purpose
        // (best-effort cleanup).
        unsafe {
            libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0);
        }
    }

    /// `unlinkat` the leaf relative to its pinned parent.
    pub(super) fn unlink(c: &Confined) -> Result<(), String> {
        // SAFETY: `parent`/`leaf` are live for the call.
        let rc = unsafe { libc::unlinkat(c.parent.as_raw_fd(), c.leaf.as_ptr(), 0) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(())
    }

    /// Read the names + kinds of a pinned directory via `fdopendir`, so the
    /// listing comes from the descriptor and never from a re-walked pathname.
    pub(super) fn read_dir(
        dirfd: &OwnedFd,
    ) -> Result<Vec<(std::ffi::OsString, EntryKind)>, String> {
        // `fdopendir` takes OWNERSHIP of the descriptor it is handed, so give it
        // a dup and keep ours pinned for the caller's recursion.
        // SAFETY: `dirfd` is live; the dup is either handed to `fdopendir`
        // (which closes it via `closedir`) or closed here.
        let dup = unsafe { libc::dup(dirfd.as_raw_fd()) };
        if dup < 0 {
            return Err(last_err("dup directory descriptor"));
        }
        let dirp = unsafe { libc::fdopendir(dup) };
        if dirp.is_null() {
            let err = last_err("fdopendir");
            unsafe { libc::close(dup) };
            return Err(err);
        }
        let mut out = Vec::new();
        loop {
            // SAFETY: `dirp` is a valid open DIR*; the returned dirent is owned
            // by the DIR* and only read before the next `readdir` call.
            let ent = unsafe { libc::readdir(dirp) };
            if ent.is_null() {
                // A NULL return is end-of-directory (or, rarely, a read error —
                // treated the same: a short listing, never a wrong one).
                break;
            }
            let (name, d_type) = unsafe { (CStr::from_ptr((*ent).d_name.as_ptr()), (*ent).d_type) };
            let bytes = name.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            let kind = match d_type {
                libc::DT_DIR => EntryKind::Dir,
                libc::DT_REG => EntryKind::File,
                libc::DT_LNK => EntryKind::Symlink,
                // A filesystem that does not fill in `d_type` — ask the kernel,
                // still without following the link.
                _ => {
                    let mut st: libc::stat = unsafe { std::mem::zeroed() };
                    let rc = unsafe {
                        libc::fstatat(
                            dirfd.as_raw_fd(),
                            name.as_ptr(),
                            &mut st,
                            libc::AT_SYMLINK_NOFOLLOW,
                        )
                    };
                    if rc == 0 {
                        kind_of_mode(st.st_mode)
                    } else {
                        EntryKind::Other
                    }
                }
            };
            out.push((OsStr::from_bytes(bytes).to_os_string(), kind));
        }
        // SAFETY: `dirp` is valid and closed exactly once here (this also closes
        // the duped descriptor).
        unsafe { libc::closedir(dirp) };
        Ok(out)
    }

    /// Descend one already-listed subdirectory of a pinned directory, refusing
    /// to follow a symlink.
    pub(super) fn open_subdir(dirfd: &OwnedFd, name: &OsStr) -> Result<OwnedFd, String> {
        openat(dirfd.as_fd(), &cname(name)?, DIR_FLAGS, None)
    }

    /// Open one already-listed file of a pinned directory for reading, refusing
    /// to follow a symlink.
    pub(super) fn open_file_in(dirfd: &OwnedFd, name: &OsStr) -> Result<std::fs::File, String> {
        let fd = openat(
            dirfd.as_fd(),
            &cname(name)?,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            None,
        )?;
        Ok(std::fs::File::from(fd))
    }

    pub(super) fn leaf_kind(c: &Confined) -> Option<EntryKind> {
        lstat_leaf(c).map(|st| kind_of_mode(st.st_mode))
    }

    pub(super) fn leaf_size(c: &Confined) -> Option<u64> {
        lstat_leaf(c).map(|st| st.st_size as u64)
    }
}

// ── the tools' handle on a workspace target ─────────────────────────────────
//
// One API for the six tools; on unix it is backed by the descriptor-relative
// walk above. On other platforms (Windows has no `openat`) it falls back to the
// canonical-pathname resolution, which keeps the `..`/absolute/containment and
// symlink-leaf checks but not the pinned-descriptor guarantee. Everything we
// ship on today (macOS, Linux) takes the unix path.

/// A single file target: its parent directory, pinned, plus the leaf name.
struct Target {
    #[cfg(unix)]
    inner: confined::Confined,
    #[cfg(not(unix))]
    inner: PathBuf,
}

impl Target {
    /// Pin `rel`'s parent inside `ws`. The leaf itself need not exist.
    fn open(ws: &Path, rel: &str) -> Result<Self, String> {
        #[cfg(unix)]
        {
            Ok(Self {
                inner: confined::resolve(ws, rel)?,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                inner: resolve_within_new(ws, rel)?,
            })
        }
    }

    /// The leaf's kind, symlinks NOT followed. `None` = it does not exist.
    fn kind(&self) -> Option<EntryKind> {
        #[cfg(unix)]
        {
            confined::leaf_kind(&self.inner)
        }
        #[cfg(not(unix))]
        {
            std::fs::symlink_metadata(&self.inner).ok().map(|m| {
                let t = m.file_type();
                if t.is_dir() {
                    EntryKind::Dir
                } else if t.is_file() {
                    EntryKind::File
                } else if t.is_symlink() {
                    EntryKind::Symlink
                } else {
                    EntryKind::Other
                }
            })
        }
    }

    /// The leaf's size in bytes, without following a symlink.
    fn size(&self) -> Option<u64> {
        #[cfg(unix)]
        {
            confined::leaf_size(&self.inner)
        }
        #[cfg(not(unix))]
        {
            std::fs::symlink_metadata(&self.inner).ok().map(|m| m.len())
        }
    }

    /// Read the leaf as UTF-8 text, refusing to follow a symlink and refusing
    /// anything over `max_bytes`.
    fn read_to_string(&self, max_bytes: u64) -> Result<String, String> {
        use std::io::Read;
        match self.kind() {
            Some(EntryKind::File) => {}
            Some(EntryKind::Symlink) => {
                return Err("is a symlink — refusing to read through it".to_string())
            }
            Some(_) => return Err("is not a file".to_string()),
            None => return Err("no such file".to_string()),
        }
        if let Some(len) = self.size() {
            if len > max_bytes {
                return Err(format!(
                    "is {len} bytes, over the {max_bytes}-byte read limit"
                ));
            }
        }
        #[cfg(unix)]
        let mut file = confined::open_read(&self.inner)?;
        #[cfg(not(unix))]
        let mut file = std::fs::File::open(&self.inner).map_err(|e| e.to_string())?;
        let mut content = String::new();
        // `take(max + 1)` so a file that GREW past the cap between the stat and
        // the read is still bounded.
        file.by_ref()
            .take(max_bytes + 1)
            .read_to_string(&mut content)
            .map_err(|e| format!("{e} (not UTF-8 text?)"))?;
        if content.len() as u64 > max_bytes {
            return Err(format!("is over the {max_bytes}-byte read limit"));
        }
        Ok(content)
    }

    /// Does the leaf exist, and is it a directory? Decided without following a
    /// symlink, so a link *to* a directory is not mistaken for one.
    fn is_dir(&self) -> bool {
        self.kind() == Some(EntryKind::Dir)
    }

    /// Atomically replace the leaf's contents.
    fn atomic_replace(&self, content: &str) -> Result<(), String> {
        #[cfg(unix)]
        {
            confined::atomic_replace(&self.inner, content)
        }
        #[cfg(not(unix))]
        {
            atomic_write(&self.inner, content)
        }
    }

    /// Remove the leaf.
    fn unlink(&self) -> Result<(), String> {
        #[cfg(unix)]
        {
            confined::unlink(&self.inner)
        }
        #[cfg(not(unix))]
        {
            std::fs::remove_file(&self.inner).map_err(|e| e.to_string())
        }
    }
}

/// A directory target: pinned on unix, a canonical path elsewhere.
struct DirTarget {
    #[cfg(unix)]
    inner: std::os::fd::OwnedFd,
    #[cfg(not(unix))]
    inner: PathBuf,
}

impl DirTarget {
    fn open(ws: &Path, rel: &str) -> Result<Self, String> {
        #[cfg(unix)]
        {
            Ok(Self {
                inner: confined::resolve_dir(ws, rel)?,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                inner: resolve_within(ws, rel)?,
            })
        }
    }

    /// The directory's entries, with each kind decided without following a link.
    fn entries(&self) -> Result<Vec<(std::ffi::OsString, EntryKind)>, String> {
        #[cfg(unix)]
        {
            confined::read_dir(&self.inner)
        }
        #[cfg(not(unix))]
        {
            let mut out = Vec::new();
            for entry in std::fs::read_dir(&self.inner)
                .map_err(|e| e.to_string())?
                .flatten()
            {
                let kind = match entry.file_type() {
                    Ok(t) if t.is_dir() => EntryKind::Dir,
                    Ok(t) if t.is_file() => EntryKind::File,
                    Ok(t) if t.is_symlink() => EntryKind::Symlink,
                    _ => EntryKind::Other,
                };
                out.push((entry.file_name(), kind));
            }
            Ok(out)
        }
    }

    /// Descend into one of this directory's own entries, refusing a symlink.
    fn subdir(&self, name: &std::ffi::OsStr) -> Result<Self, String> {
        #[cfg(unix)]
        {
            Ok(Self {
                inner: confined::open_subdir(&self.inner, name)?,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                inner: self.inner.join(name),
            })
        }
    }

    /// Read one of this directory's own entries as text, refusing a symlink and
    /// anything over `max_bytes`.
    fn read_file(&self, name: &std::ffi::OsStr, max_bytes: u64) -> Result<String, String> {
        use std::io::Read;
        #[cfg(unix)]
        let mut file = confined::open_file_in(&self.inner, name)?;
        #[cfg(not(unix))]
        let mut file = std::fs::File::open(self.inner.join(name)).map_err(|e| e.to_string())?;
        let mut content = String::new();
        file.by_ref()
            .take(max_bytes + 1)
            .read_to_string(&mut content)
            .map_err(|e| e.to_string())?;
        if content.len() as u64 > max_bytes {
            return Err("over the read limit".to_string());
        }
        Ok(content)
    }
}

/// The filename that records the legacy-workspace migration already ran.
pub const LEGACY_MIGRATION_MARKER: &str = ".tierp-migrated";
/// The marker's CONTENT sentinel. Presence-only checks are unsafe (a legacy
/// file that happens to be named `.tierp-migrated` would spoof "already done"
/// and strand real data); we treat migration as done only when the marker holds
/// this exact magic, and we (re)write it ourselves.
const LEGACY_MIGRATION_MAGIC: &str = "lost-harness tier-p workspace migration v1\n";

fn migration_is_done(marker: &std::path::Path) -> bool {
    std::fs::read_to_string(marker)
        .map(|s| s == LEGACY_MIGRATION_MAGIC)
        .unwrap_or(false)
}

/// One-time, idempotent migration of the LEGACY shared workspace into the
/// DEFAULT profile's per-profile root (M7 design, Slice 1: "migrate the legacy
/// shared `workspace/` into the default profile's root, not deleted").
///
/// Before Tier-P every fs-tool write pooled at `<base>/workspace/*` regardless
/// of profile. After Tier-P the default profile ("personal") resolves to
/// `<base>/workspace/personal/`, so without this move a user's pre-upgrade files
/// would still be OUT of reach of every fs tool (the MEDIUM regression the first
/// review caught).
///
/// **Structural safety invariant (the key call — moving user data must NEVER
/// mis-attribute one profile's tree to another): this moves REGULAR FILES ONLY,
/// and NEVER moves a directory.** The rationale is decisive: after Tier-P the
/// workspace root only ever contains per-profile *directories* + this marker
/// (every tool write goes into a `workspace/<profile>/` subdir), so a loose
/// *file* at the root can ONLY be legacy pooled data — always safe to move. A
/// *directory* is inherently ambiguous — a live profile tree, an orphaned tree
/// whose DB desynced, or a legacy subdir — and is impossible to classify safely
/// by name alone (adversarial review disproved every name/known-list heuristic:
/// a lost/desynced profile DB, or a legacy file colliding with a profile name,
/// each defeated it). So we never move a directory; we leave it intact in place
/// and LOG it (benign: its data is untouched, just not surfaced under the default
/// profile — the human can move it deliberately). This makes profile
/// mis-attribution *structurally impossible* rather than heuristically unlikely.
///
/// Also: the marker is CONTENT-checked ([`LEGACY_MIGRATION_MAGIC`]), not just
/// presence-checked (a pre-existing same-named file can't spoof "done"); moves
/// via `rename` within the same `workspace/` subtree (same filesystem → no
/// `EXDEV`); never deletes; never clobbers an existing destination entry; moves
/// everything movable in one pass and returns the first error WITHOUT stamping
/// the marker (so a transient lock retries next boot). Called once at startup,
/// before any fs tool runs.
pub fn migrate_legacy_workspace(
    workspace_root: &std::path::Path,
    default_profile: &str,
) -> std::io::Result<()> {
    let marker = workspace_root.join(LEGACY_MIGRATION_MARKER);
    if migration_is_done(&marker) {
        return Ok(());
    }
    // Nothing on disk yet → stamp the marker so a file the user creates AFTER
    // upgrade (but before the first tool call) is never mistaken for legacy data.
    if !workspace_root.exists() {
        std::fs::create_dir_all(workspace_root)?;
        std::fs::write(&marker, LEGACY_MIGRATION_MAGIC)?;
        return Ok(());
    }
    let dest = profile_workspace_path(workspace_root, default_profile);
    // A degenerate default (empty/escaping → collapses to base) has no distinct
    // subtree to isolate into; just stamp and return rather than self-move.
    if dest == workspace_root {
        std::fs::write(&marker, LEGACY_MIGRATION_MAGIC)?;
        return Ok(());
    }
    let dest_name = dest.file_name();
    std::fs::create_dir_all(&dest)?;
    // Move everything movable in one pass; a single un-moveable entry (a lock, a
    // permission error) must NOT block migrating the others. Track the first
    // error and, if any, return it WITHOUT stamping the marker.
    let mut first_err: Option<std::io::Error> = None;
    let mut left_dirs: Vec<String> = Vec::new();
    let mut left_collisions: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(workspace_root)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == std::ffi::OsStr::new(LEGACY_MIGRATION_MARKER) {
            continue;
        }
        // Skip the destination profile dir itself (don't move it into itself).
        if dest_name == Some(name.as_os_str()) {
            continue;
        }
        // STRUCTURAL INVARIANT: only move REGULAR FILES. A directory (a live/
        // orphaned profile tree, or a legacy subdir) or a symlink is ambiguous —
        // never move it; leave it intact and record it for the log below.
        let is_regular_file = match entry.file_type() {
            Ok(ft) => ft.is_file(),
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
                continue;
            }
        };
        if !is_regular_file {
            if let Some(n) = name.to_str() {
                left_dirs.push(n.to_string());
            }
            continue;
        }
        let to = dest.join(&name);
        // Never clobber — if ANYTHING already sits at the destination (a partial
        // earlier run, or a user's own file), leave both in place. Use lstat
        // (`symlink_metadata`, no symlink follow), NOT `to.exists()`: the latter
        // FOLLOWS the link, so a DANGLING destination symlink (e.g. a profile
        // subfolder symlinked to a not-yet-mounted volume) would read as absent
        // and `rename` would silently replace/orphan it — a review-caught clobber.
        if std::fs::symlink_metadata(&to).is_ok() {
            if let Some(n) = name.to_str() {
                left_collisions.push(n.to_string());
            }
            continue;
        }
        if let Err(e) = std::fs::rename(entry.path(), &to) {
            if first_err.is_none() {
                first_err = Some(e);
            }
        }
    }
    // Surface (never silently drop) any legacy top-level directories/symlinks
    // left in place — their data is intact, just not under the default profile.
    if !left_dirs.is_empty() {
        tracing::warn!(
            entries = ?left_dirs,
            workspace = %workspace_root.display(),
            default_profile = %default_profile,
            "Tier-P migration left legacy top-level directories/symlinks IN PLACE (only loose \
             files are auto-moved into the default profile, so a profile tree can never be \
             mis-attributed); their data is intact — move them into the profile deliberately if needed"
        );
    }
    // Surface any legacy files NOT moved because the destination name was already
    // taken — also intact in place, but worth a trail (not a silent drop).
    if !left_collisions.is_empty() {
        tracing::warn!(
            entries = ?left_collisions,
            workspace = %workspace_root.display(),
            default_profile = %default_profile,
            "Tier-P migration left legacy files IN PLACE because a same-named entry already \
             exists in the default profile; both copies are intact — reconcile deliberately if needed"
        );
    }
    match first_err {
        Some(e) => Err(e),
        None => {
            std::fs::write(&marker, LEGACY_MIGRATION_MAGIC)?;
            Ok(())
        }
    }
}

// ── read_file ───────────────────────────────────────────────────────────────

/// Read a UTF-8 text file from the workspace.
pub struct ReadFileTool {
    root: PathBuf,
}

impl ReadFileTool {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a UTF-8 text file inside the workspace. args: {\"path\": \"relative/path.txt\"}"
    }

    fn requires(&self) -> &[Capability] {
        &[Capability::Filesystem]
    }

    fn run<'a>(
        &'a self,
        input: ToolInput,
        ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let Some(path) = arg_str(&input, "path") else {
                return ToolResult::Err("read_file requires a string \"path\" arg".to_string());
            };
            let ws = match profile_workspace_root(&self.root, &ctx.profile) {
                Ok(v) => v,
                Err(e) => return ToolResult::Err(e),
            };
            let _ = tokio::fs::create_dir_all(&ws).await;
            // The canonical path is ONLY the read-before-write set key (it must
            // carry the on-disk casing). The bytes come from the
            // descriptor-relative walk below, which is what enforces
            // confinement.
            let resolved = match resolve_within_async(ws.clone(), path.to_string()).await {
                Ok(p) => p,
                Err(e) => return ToolResult::Err(e),
            };
            let (ws2, path2) = (ws.clone(), path.to_string());
            let read = tokio::task::spawn_blocking(move || {
                Target::open(&ws2, &path2)?.read_to_string(MAX_READ_BYTES)
            })
            .await;
            let read = match read {
                Ok(v) => v,
                Err(e) => return ToolResult::Err(format!("read task failed: {e}")),
            };
            match read {
                Ok(content) => {
                    // Read-before-write: remember we've seen this file (by its
                    // canonical path) so a later write_file/edit_file in the
                    // same conversation is allowed to touch it. No-op unless the
                    // dispatcher wired a read-set into the context.
                    if let Some(reads) = &ctx.reads {
                        reads.record(&ctx.conversation_id, resolved.clone());
                    }
                    ToolResult::Ok(json!({
                        "path": path,
                        "bytes": content.len(),
                        "content": content,
                    }))
                }
                // `Target::read_to_string` already says *why* (missing, a
                // directory, a symlink, over the cap, not UTF-8).
                Err(e) => ToolResult::Err(format!("read '{path}': {e}")),
            }
        })
    }
}

// ── list_dir ─────────────────────────────────────────────────────────────────

/// List the entries of a directory in the workspace.
pub struct ListDirTool {
    root: PathBuf,
}

impl ListDirTool {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List entries of a directory in the workspace. args: {\"path\": \"subdir\"} (defaults to workspace root)"
    }

    fn requires(&self) -> &[Capability] {
        &[Capability::Filesystem]
    }

    fn run<'a>(
        &'a self,
        input: ToolInput,
        ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let path = arg_str(&input, "path").unwrap_or(".");
            let ws = match profile_workspace_root(&self.root, &ctx.profile) {
                Ok(v) => v,
                Err(e) => return ToolResult::Err(e),
            };
            let _ = tokio::fs::create_dir_all(&ws).await;
            // Pin the directory itself with the descriptor walk and list it from
            // that descriptor (`fdopendir`), so the listing describes the
            // directory we validated rather than whatever the pathname resolves
            // to by the time `read_dir` gets there.
            let (ws2, path2) = (ws.clone(), path.to_string());
            let listed = tokio::task::spawn_blocking(move || {
                let dir = DirTarget::open(&ws2, &path2)?;
                let mut entries = Vec::new();
                for (name, kind) in dir.entries()? {
                    entries.push(json!({
                        "name": name.to_string_lossy().into_owned(),
                        "kind": kind.as_str(),
                    }));
                }
                Ok::<_, String>(entries)
            })
            .await;
            let mut entries = match listed {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => return ToolResult::Err(format!("list '{path}': {e}")),
                Err(e) => return ToolResult::Err(format!("list task failed: {e}")),
            };
            entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            ToolResult::Ok(json!({ "path": path, "entries": entries }))
        })
    }
}

// ── search_files ─────────────────────────────────────────────────────────────

/// Substring-search filenames and text-file contents within the workspace.
pub struct SearchFilesTool {
    root: PathBuf,
}

impl SearchFilesTool {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl Tool for SearchFilesTool {
    fn name(&self) -> &str {
        "search_files"
    }

    fn description(&self) -> &str {
        "Case-insensitive substring search over filenames and text-file contents in the workspace. args: {\"query\": \"needle\"}"
    }

    fn requires(&self) -> &[Capability] {
        &[Capability::Filesystem]
    }

    fn run<'a>(
        &'a self,
        input: ToolInput,
        ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let Some(query) = arg_str(&input, "query") else {
                return ToolResult::Err("search_files requires a string \"query\" arg".to_string());
            };
            if query.is_empty() {
                return ToolResult::Err("search_files \"query\" must not be empty".to_string());
            }
            let ws = match profile_workspace_root(&self.root, &ctx.profile) {
                Ok(v) => v,
                Err(e) => return ToolResult::Err(e),
            };
            let _ = tokio::fs::create_dir_all(&ws).await;
            let needle = query.to_lowercase();
            // The whole tree walk is blocking filesystem work — keep it off the
            // async worker (M-21).
            let walked = tokio::task::spawn_blocking(move || {
                let root = DirTarget::open(&ws, ".")?;
                let mut matches = Vec::new();
                let mut scanned = 0usize;
                walk(&root, "", 0, &needle, &mut matches, &mut scanned);
                Ok::<_, String>((matches, scanned))
            })
            .await;
            let (matches, scanned) = match walked {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => return ToolResult::Err(format!("search: {e}")),
                Err(e) => return ToolResult::Err(format!("search task failed: {e}")),
            };
            let truncated =
                matches.len() >= SEARCH_MAX_RESULTS || scanned >= SEARCH_MAX_FILES_SCANNED;
            ToolResult::Ok(json!({
                "query": query,
                "matches": matches,
                "truncated": truncated,
            }))
        })
    }
}

/// Recursive, bounded workspace walk. Descends through pinned DIRECTORY
/// DESCRIPTORS (`dir`), never re-resolved pathnames, so a directory swapped for a
/// symlink mid-walk cannot redirect the search out of the workspace. `prefix` is
/// the display path relative to the workspace root ("" at the top), so results
/// never leak the absolute on-disk location.
///
/// Symlinks are skipped entirely rather than followed — matching the previous
/// behaviour (`read_dir`'s `file_type` is an lstat, so a link was neither `dir`
/// nor `file`), and now enforced by the descriptor walk itself.
fn walk(
    dir: &DirTarget,
    prefix: &str,
    depth: usize,
    needle: &str,
    matches: &mut Vec<serde_json::Value>,
    scanned: &mut usize,
) {
    if depth > SEARCH_MAX_DEPTH
        || matches.len() >= SEARCH_MAX_RESULTS
        || *scanned >= SEARCH_MAX_FILES_SCANNED
    {
        return;
    }
    let Ok(entries) = dir.entries() else {
        return;
    };
    for (name, kind) in entries {
        if matches.len() >= SEARCH_MAX_RESULTS || *scanned >= SEARCH_MAX_FILES_SCANNED {
            return;
        }
        let rel = if prefix.is_empty() {
            name.to_string_lossy().into_owned()
        } else {
            format!("{prefix}/{}", name.to_string_lossy())
        };
        match kind {
            EntryKind::Dir => {
                // `subdir` is an `openat` with `O_NOFOLLOW` off THIS descriptor:
                // if the entry stopped being the directory we just listed, the
                // descent fails instead of following the replacement.
                if let Ok(child) = dir.subdir(&name) {
                    walk(&child, &rel, depth + 1, needle, matches, scanned);
                }
                continue;
            }
            EntryKind::File => {}
            // Symlinks and non-regular entries (fifos, sockets, devices) are not
            // searched: opening them could block or escape.
            EntryKind::Symlink | EntryKind::Other => continue,
        }
        *scanned += 1;
        let name_hit = rel.to_lowercase().contains(needle);

        // Content match — only for reasonably-sized files we can read as text.
        // `read_file` caps the read itself, so an over-size or non-UTF-8 file
        // simply yields no content hit (same outcome as the old size pre-check,
        // without a second stat that could disagree with the read).
        let mut content_hit: Option<serde_json::Value> = None;
        if let Ok(text) = dir.read_file(&name, SEARCH_MAX_FILE_BYTES) {
            if let Some((lineno, line)) = text
                .lines()
                .enumerate()
                .find(|(_, l)| l.to_lowercase().contains(needle))
            {
                let snippet: String = line.trim().chars().take(200).collect();
                content_hit = Some(json!({ "line": lineno + 1, "snippet": snippet }));
            }
        }

        if name_hit || content_hit.is_some() {
            matches.push(json!({
                "path": rel,
                "name_match": name_hit,
                "content_match": content_hit,
            }));
        }
    }
}

// ── path safety for NEW paths ────────────────────────────────────────────────

/// Cap on a single write, so the agent can't dump an unbounded blob.
const MAX_WRITE_BYTES: usize = 1024 * 1024; // 1 MiB

/// Resolve `rel` against `root` for a target that may NOT exist yet (a file to
/// create/overwrite). Same `..`/absolute rejection as `resolve_within`, but
/// instead of canonicalizing the (possibly missing) target, it canonicalizes
/// the target's PARENT — which must exist and stay inside the workspace — then
/// re-attaches the final component. A symlinked parent still can't escape,
/// because the *parent's* canonical path is what's checked.
fn resolve_within_new(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(format!(
            "path must be relative to the workspace, got: {rel}"
        ));
    }
    for comp in rel_path.components() {
        match comp {
            Component::ParentDir => return Err("path may not contain '..'".to_string()),
            Component::Prefix(_) | Component::RootDir => {
                return Err("path may not be absolute or contain a drive prefix".to_string())
            }
            _ => {}
        }
    }
    let file_name = rel_path
        .file_name()
        .ok_or_else(|| format!("path has no filename: {rel}"))?;
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("workspace root unavailable: {e}"))?;
    // The parent directory must already exist (create it first with a dir
    // tool if needed) — we canonicalize it to resolve any symlinks before the
    // containment check.
    let parent = root.join(rel_path).parent().map(Path::to_path_buf);
    let canon_parent = match parent {
        Some(p) => p
            .canonicalize()
            .map_err(|e| format!("cannot access the parent directory of '{rel}': {e}"))?,
        None => canon_root.clone(),
    };
    if !canon_parent.starts_with(&canon_root) {
        return Err(format!("path escapes the workspace: {rel}"));
    }
    let resolved = canon_parent.join(file_name);
    // Refuse to write through/over a symlink. `rename` would replace the link
    // itself (silently orphaning whatever it pointed at) rather than write
    // through it, and following it could aim outside the workspace. If the
    // caller really means to replace it, they can delete the link first.
    // `symlink_metadata` is an lstat — it does NOT follow the link.
    if let Ok(meta) = std::fs::symlink_metadata(&resolved) {
        if meta.file_type().is_symlink() {
            return Err(format!(
                "'{rel}' is a symlink — refusing to write through it (delete it first to replace it)"
            ));
        }
    }
    Ok(resolved)
}

/// Best-effort canonicalization of a workspace-relative path against `root`,
/// following symlinks exactly the way the fs tools do before ever touching
/// disk: `resolve_within` when the target already exists, falling back to
/// `resolve_within_new`'s parent-canonicalization for a target that doesn't
/// exist yet (e.g. a fresh `write_file` under a symlinked alias dir).
/// Returns `None` on any validation failure (absolute, `..`, inaccessible
/// parent) — callers must treat `None` as "no additional signal," NOT as
/// "safe": each resolver's own checks are the actual guard against those
/// cases. This is a read-only peek used by `ProtectedPathHook` to see the
/// REAL on-disk target of a call before deciding whether to Ask; it does not
/// replace either resolver, it just re-exposes them to a caller outside this
/// module so the hook and the tool share one symlink-resolution algorithm
/// and can never drift (the drift between the hook's raw-text match and the
/// tools' `resolve_within*` is exactly what created the bypass this closes).
///
/// Note the two resolvers differ on an existing symlink *leaf*:
/// `resolve_within` follows it (reporting the real target), while
/// `resolve_within_new` would REFUSE to write through it. That only ever
/// yields an extra, harmless protected-path Ask for a call the tool would
/// reject anyway — never a false negative.
pub(crate) fn canonicalize_best_effort(root: &Path, rel: &str) -> Option<PathBuf> {
    resolve_within(root, rel)
        .or_else(|_| resolve_within_new(root, rel))
        .ok()
}

/// Write `content` to `target` atomically: write a temp file in the same
/// directory, then rename over the target (rename is atomic on one
/// filesystem, so a reader never sees a half-written file).
///
/// **Non-unix only.** On unix the equivalent runs against a pinned directory
/// descriptor ([`confined::atomic_replace`]) so neither the temp file nor the
/// rename target can be redirected by a swapped path component; this pathname
/// version is the fallback for platforms without `openat`.
#[cfg(not(unix))]
fn atomic_write(target: &Path, content: &str) -> Result<(), String> {
    let dir = target
        .parent()
        .ok_or_else(|| "target has no parent directory".to_string())?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "write".to_string()),
        uuid::Uuid::new_v4()
    ));
    // On ANY failure — the temp write itself (e.g. disk full) or the rename —
    // clean up the temp file, so a failed write leaves the workspace exactly
    // as it was (no orphaned `.tmp` residue for list_dir/search_files to find).
    if let Err(e) = std::fs::write(&tmp, content.as_bytes()) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("write temp file: {e}"));
    }
    std::fs::rename(&tmp, target).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("finalize write: {e}")
    })
}

// ── write_file ────────────────────────────────────────────────────────────────

/// Create or overwrite a UTF-8 text file in the workspace. State-changing:
/// routes through the approval spine (`RiskClass::Write`).
pub struct WriteFileTool {
    root: PathBuf,
}

impl WriteFileTool {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Create or overwrite a text file in the workspace. args: {\"path\": \"relative/path.txt\", \"content\": \"...\"}"
    }

    fn risk(&self) -> RiskClass {
        RiskClass::Write
    }

    fn requires(&self) -> &[Capability] {
        &[Capability::Filesystem]
    }

    fn run<'a>(
        &'a self,
        input: ToolInput,
        ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let Some(path) = arg_str(&input, "path").map(String::from) else {
                return ToolResult::Err("write_file requires a string \"path\" arg".to_string());
            };
            let Some(content) = arg_str(&input, "content").map(String::from) else {
                return ToolResult::Err("write_file requires a string \"content\" arg".to_string());
            };
            if content.len() > MAX_WRITE_BYTES {
                return ToolResult::Err(format!(
                    "content is {} bytes, over the {MAX_WRITE_BYTES}-byte write limit",
                    content.len()
                ));
            }
            let ws = match profile_workspace_root(&self.root, &ctx.profile) {
                Ok(v) => v,
                Err(e) => return ToolResult::Err(e),
            };
            let _ = tokio::fs::create_dir_all(&ws).await;
            // Lexical + canonical pre-check. It contributes the `..`/absolute
            // rejection, the "parent must exist and stay inside the workspace"
            // check, and — its only ongoing job — the canonical path used as the
            // read-before-write set key. It does NOT decide where the bytes go.
            let resolved = match resolve_within_new_async(ws.clone(), path.clone()).await {
                Ok(v) => v,
                Err(e) => return ToolResult::Err(e),
            };
            // Pin the parent directory. Everything below acts on `target`, i.e.
            // `*at()`-relative to that descriptor, so no pathname is resolved a
            // second time and there is no check→use window to race.
            let (ws2, path2) = (ws.clone(), path.clone());
            let target = match tokio::task::spawn_blocking(move || Target::open(&ws2, &path2)).await
            {
                Ok(Ok(t)) => t,
                Ok(Err(e)) => return ToolResult::Err(e),
                Err(e) => return ToolResult::Err(format!("resolve task failed: {e}")),
            };
            // One `fstatat(AT_SYMLINK_NOFOLLOW)` decides all three questions.
            let existing = target.kind();
            match existing {
                Some(EntryKind::Dir) => return ToolResult::Err(format!("'{path}' is a directory")),
                Some(EntryKind::Symlink) => {
                    return ToolResult::Err(format!(
                        "'{path}' is a symlink — refusing to write through it (delete it first to replace it)"
                    ))
                }
                _ => {}
            }
            let existed = existing.is_some();
            if existed {
                if let Some(reads) = &ctx.reads {
                    let key = tokio::fs::canonicalize(&resolved)
                        .await
                        .unwrap_or_else(|_| resolved.clone());
                    if !reads.contains(&ctx.conversation_id, &key) {
                        return ToolResult::Err(format!(
                            "refusing to write '{path}': read_file it first so you're not overwriting blind"
                        ));
                    }
                }
            }
            let content2 = content.clone();
            match tokio::task::spawn_blocking(move || target.atomic_replace(&content2)).await {
                Ok(Ok(())) => {
                    if let Some(reads) = &ctx.reads {
                        let key = tokio::fs::canonicalize(&resolved)
                            .await
                            .unwrap_or_else(|_| resolved.clone());
                        reads.record(&ctx.conversation_id, key);
                    }
                    ToolResult::Ok(
                        json!({"path": path, "bytes_written": content.len(), "created": !existed}),
                    )
                }
                Ok(Err(e)) => ToolResult::Err(format!("write '{path}': {e}")),
                Err(e) => ToolResult::Err(format!("write task failed: {e}")),
            }
        })
    }
}

// ── edit_file ─────────────────────────────────────────────────────────────────

/// Replace an exact substring in an existing workspace file. Requires the
/// match to be UNIQUE — 0 matches is an error (nothing to do), and >1 is an
/// error (ambiguous), so an edit can never silently hit the wrong spot.
pub struct EditFileTool {
    root: PathBuf,
}

impl EditFileTool {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Replace an exact, unique substring in an existing workspace file. args: {\"path\": \"file.txt\", \"old\": \"text to find (must occur exactly once)\", \"new\": \"replacement\"}"
    }

    fn risk(&self) -> RiskClass {
        RiskClass::Write
    }

    fn requires(&self) -> &[Capability] {
        &[Capability::Filesystem]
    }

    fn run<'a>(
        &'a self,
        input: ToolInput,
        ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let Some(path) = arg_str(&input, "path").map(String::from) else {
                return ToolResult::Err("edit_file requires a string \"path\" arg".to_string());
            };
            let Some(old) = arg_str(&input, "old").map(String::from) else {
                return ToolResult::Err("edit_file requires a string \"old\" arg".to_string());
            };
            let Some(new) = arg_str(&input, "new").map(String::from) else {
                return ToolResult::Err("edit_file requires a string \"new\" arg".to_string());
            };
            if old.is_empty() {
                return ToolResult::Err("edit_file \"old\" must not be empty".to_string());
            }
            let ws = match profile_workspace_root(&self.root, &ctx.profile) {
                Ok(v) => v,
                Err(e) => return ToolResult::Err(e),
            };
            let _ = tokio::fs::create_dir_all(&ws).await;
            let resolved = match resolve_within_async(ws.clone(), path.clone()).await {
                Ok(v) => v,
                Err(e) => return ToolResult::Err(e),
            };
            if let Some(reads) = &ctx.reads {
                if !reads.contains(&ctx.conversation_id, &resolved) {
                    return ToolResult::Err(format!(
                        "refusing to edit '{path}': read_file it first so you're not editing blind"
                    ));
                }
            }
            // Read-modify-write on ONE pinned parent descriptor: the file whose
            // content we matched against is, by construction, the file we then
            // replace. Doing it in a single blocking closure keeps that descriptor
            // alive across both halves and keeps the (blocking) file I/O off the
            // async worker (M-21).
            let (ws2, path2) = (ws.clone(), path.clone());
            let (old2, new2) = (old.clone(), new.clone());
            let edited = tokio::task::spawn_blocking(move || -> Result<String, String> {
                let target = Target::open(&ws2, &path2)?;
                // Same cap as `read_file`: a file the model cannot read in full is
                // one it cannot edit safely either.
                let content = target
                    .read_to_string(MAX_READ_BYTES)
                    .map_err(|e| format!("read '{path2}': {e}"))?;
                let count = content.matches(&old2).count();
                if count == 0 {
                    return Err(format!("edit_file: \"old\" not found in '{path2}'"));
                }
                if count > 1 {
                    return Err(format!(
                        "edit_file: \"old\" occurs {count} times in '{path2}' — make it unique (add surrounding context)"
                    ));
                }
                let updated = content.replacen(&old2, &new2, 1);
                if updated.len() > MAX_WRITE_BYTES {
                    return Err(format!(
                        "result is {} bytes, over the {MAX_WRITE_BYTES}-byte write limit",
                        updated.len()
                    ));
                }
                target
                    .atomic_replace(&updated)
                    .map_err(|e| format!("write '{path2}': {e}"))?;
                Ok(updated)
            })
            .await;
            match edited {
                Ok(Ok(updated)) => {
                    ToolResult::Ok(json!({"path": path, "replaced": 1, "bytes": updated.len()}))
                }
                Ok(Err(e)) => ToolResult::Err(e),
                Err(e) => ToolResult::Err(format!("edit write task failed: {e}")),
            }
        })
    }
}

// ── delete_file ─────────────────────────────────────────────────────────────

/// Delete a file (not a directory) inside the workspace.
pub struct DeleteFileTool {
    root: PathBuf,
}

impl DeleteFileTool {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl Tool for DeleteFileTool {
    fn name(&self) -> &str {
        "delete_file"
    }

    fn description(&self) -> &str {
        "Delete a file inside the workspace (not a directory). args: {\"path\": \"relative/path.txt\"}"
    }

    fn risk(&self) -> RiskClass {
        RiskClass::Write
    }

    fn requires(&self) -> &[Capability] {
        &[Capability::Filesystem]
    }

    fn run<'a>(
        &'a self,
        input: ToolInput,
        ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let Some(path) = arg_str(&input, "path").map(String::from) else {
                return ToolResult::Err("delete_file requires a string \"path\" arg".to_string());
            };
            let ws = match profile_workspace_root(&self.root, &ctx.profile) {
                Ok(v) => v,
                Err(e) => return ToolResult::Err(e),
            };
            let _ = tokio::fs::create_dir_all(&ws).await;
            // Pin the parent, then `unlinkat` the leaf off that descriptor. Note
            // this also fixes a real mis-targeting bug: the old code canonicalized
            // the pathname first, so deleting a symlink FOLLOWED it and removed
            // the link's target, leaving the link itself dangling. `unlinkat`
            // removes exactly the name that was asked for.
            let (ws2, path2) = (ws.clone(), path.clone());
            let deleted = tokio::task::spawn_blocking(move || -> Result<(), String> {
                let target = Target::open(&ws2, &path2)?;
                if target.is_dir() {
                    return Err(format!(
                        "'{path2}' is a directory — delete_file only removes files"
                    ));
                }
                target
                    .unlink()
                    .map_err(|e| format!("delete '{path2}': {e}"))
            })
            .await;
            match deleted {
                Ok(Ok(())) => ToolResult::Ok(json!({ "path": path, "deleted": true })),
                Ok(Err(e)) => ToolResult::Err(e),
                Err(e) => ToolResult::Err(format!("delete task failed: {e}")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ConversationReads;
    use std::sync::Arc;

    /// Build a throwaway workspace with a few files. Mirrors the tempdir
    /// pattern used across the crate's tests (no `tempfile` dependency).
    fn workspace() -> PathBuf {
        let mut root = std::env::temp_dir();
        root.push(format!("lhp-fs-tool-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("hello.txt"), "hello world\nsecond line\n").unwrap();
        std::fs::write(root.join("sub").join("notes.md"), "a NEEDLE lives here\n").unwrap();
        root
    }

    fn ctx() -> ExecCtx {
        ExecCtx::default()
    }

    /// A context WITH read-tracking wired, as the dispatcher supplies in
    /// production — so the read-before-write guard is active. Reusing one
    /// `tracked_ctx()` across calls shares its read-set (they hold the same
    /// `Arc`), which is how a read on one call is seen by a write on the next.
    fn tracked_ctx() -> ExecCtx {
        // Empty profile → the shared base workspace (Tier-P collapses an empty
        // profile to `base`), so these read-before-write guard tests seed and
        // assert at `root` exactly as before per-profile confinement existed.
        // The per-profile isolation path has its own dedicated tests below.
        ExecCtx {
            conversation_id: "conv-1".to_string(),
            profile: String::new(),
            reads: Some(Arc::new(ConversationReads::new())),
            allow_private_memory: false,
            session_mode: Default::default(),
            ..ExecCtx::default()
        }
    }

    /// A context bound to a specific named profile — exercises the Tier-P
    /// per-profile confinement path (tools resolve under `base/<profile>`).
    fn profile_ctx(profile: &str) -> ExecCtx {
        ExecCtx {
            conversation_id: "conv-1".to_string(),
            profile: profile.to_string(),
            reads: Some(Arc::new(ConversationReads::new())),
            allow_private_memory: false,
            session_mode: Default::default(),
            ..ExecCtx::default()
        }
    }

    #[tokio::test]
    async fn read_file_reads_within_workspace() {
        let root = workspace();
        let tool = ReadFileTool::new(&root);
        let input = ToolInput::new(json!({ "path": "hello.txt" }));
        match tool.run(input, &ctx()).await {
            ToolResult::Ok(v) => {
                assert_eq!(v["content"], "hello world\nsecond line\n");
                assert_eq!(v["path"], "hello.txt");
            }
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        }
    }

    #[tokio::test]
    async fn read_file_rejects_parent_dir_escape() {
        let root = workspace();
        let tool = ReadFileTool::new(&root);
        let input = ToolInput::new(json!({ "path": "../../../etc/passwd" }));
        match tool.run(input, &ctx()).await {
            ToolResult::Err(e) => assert!(e.contains("'..'"), "unexpected error: {e}"),
            ToolResult::Ok(v) => panic!("escape must be rejected, got {v:?}"),
        }
    }

    #[tokio::test]
    async fn read_file_rejects_absolute_path() {
        let root = workspace();
        let tool = ReadFileTool::new(&root);
        let input = ToolInput::new(json!({ "path": "/etc/passwd" }));
        assert!(matches!(tool.run(input, &ctx()).await, ToolResult::Err(_)));
    }

    #[tokio::test]
    async fn list_dir_lists_entries_sorted() {
        let root = workspace();
        let tool = ListDirTool::new(&root);
        let input = ToolInput::new(json!({ "path": "." }));
        match tool.run(input, &ctx()).await {
            ToolResult::Ok(v) => {
                let entries = v["entries"].as_array().unwrap();
                let names: Vec<&str> = entries
                    .iter()
                    .map(|e| e["name"].as_str().unwrap())
                    .collect();
                assert!(names.contains(&"hello.txt"));
                assert!(names.contains(&"sub"));
            }
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        }
    }

    #[tokio::test]
    async fn search_files_finds_by_name_and_content() {
        let root = workspace();
        let tool = SearchFilesTool::new(&root);
        let input = ToolInput::new(json!({ "query": "needle" }));
        match tool.run(input, &ctx()).await {
            ToolResult::Ok(v) => {
                let matches = v["matches"].as_array().unwrap();
                assert!(
                    matches
                        .iter()
                        .any(|m| m["path"].as_str() == Some("sub/notes.md")),
                    "expected a content hit in sub/notes.md, got {matches:?}"
                );
            }
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        }
    }

    // ── write / edit / delete ────────────────────────────────────────────

    #[tokio::test]
    async fn write_file_creates_then_overwrites() {
        let root = workspace();
        let tool = WriteFileTool::new(&root);

        let out = tool
            .run(
                ToolInput::new(json!({"path": "new.txt", "content": "first"})),
                &ctx(),
            )
            .await;
        match out {
            ToolResult::Ok(v) => {
                assert_eq!(v["created"], true);
                assert_eq!(v["bytes_written"], 5);
            }
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        }
        assert_eq!(
            std::fs::read_to_string(root.join("new.txt")).unwrap(),
            "first"
        );

        let out2 = tool
            .run(
                ToolInput::new(json!({"path": "new.txt", "content": "second!"})),
                &ctx(),
            )
            .await;
        assert!(
            matches!(out2, ToolResult::Ok(ref v) if v["created"] == false),
            "overwrite should report created=false, got {out2:?}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("new.txt")).unwrap(),
            "second!"
        );
    }

    #[tokio::test]
    async fn write_file_rejects_escape_and_writes_nothing_outside() {
        let root = workspace();
        let tool = WriteFileTool::new(&root);
        let out = tool
            .run(
                ToolInput::new(json!({"path": "../evil.txt", "content": "x"})),
                &ctx(),
            )
            .await;
        assert!(
            matches!(out, ToolResult::Err(_)),
            "escape must be rejected, got {out:?}"
        );
        assert!(
            !root.parent().unwrap().join("evil.txt").exists(),
            "nothing may be written outside the workspace"
        );
    }

    #[tokio::test]
    async fn edit_file_replaces_a_unique_match() {
        let root = workspace();
        let tool = EditFileTool::new(&root);
        let out = tool
            .run(
                ToolInput::new(
                    json!({"path": "hello.txt", "old": "second line", "new": "SECOND LINE"}),
                ),
                &ctx(),
            )
            .await;
        assert!(matches!(out, ToolResult::Ok(_)), "expected Ok, got {out:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("hello.txt")).unwrap(),
            "hello world\nSECOND LINE\n"
        );
    }

    #[tokio::test]
    async fn edit_file_refuses_missing_or_ambiguous_match() {
        let root = workspace();
        let tool = EditFileTool::new(&root);

        let miss = tool
            .run(
                ToolInput::new(json!({"path": "hello.txt", "old": "nope", "new": "x"})),
                &ctx(),
            )
            .await;
        assert!(
            matches!(miss, ToolResult::Err(ref e) if e.contains("not found")),
            "missing match must error, got {miss:?}"
        );

        // "l" occurs many times → ambiguous, must refuse.
        let ambig = tool
            .run(
                ToolInput::new(json!({"path": "hello.txt", "old": "l", "new": "L"})),
                &ctx(),
            )
            .await;
        assert!(
            matches!(ambig, ToolResult::Err(ref e) if e.contains("occurs")),
            "ambiguous match must error, got {ambig:?}"
        );
        // File is untouched by either failed edit.
        assert_eq!(
            std::fs::read_to_string(root.join("hello.txt")).unwrap(),
            "hello world\nsecond line\n"
        );
    }

    #[tokio::test]
    async fn delete_file_removes_a_file_but_refuses_a_dir() {
        let root = workspace();
        let tool = DeleteFileTool::new(&root);

        let ok = tool
            .run(ToolInput::new(json!({"path": "hello.txt"})), &ctx())
            .await;
        assert!(matches!(ok, ToolResult::Ok(_)), "expected Ok, got {ok:?}");
        assert!(!root.join("hello.txt").exists());

        let dir = tool
            .run(ToolInput::new(json!({"path": "sub"})), &ctx())
            .await;
        assert!(
            matches!(dir, ToolResult::Err(_)),
            "deleting a directory must be refused"
        );
        assert!(root.join("sub").exists(), "the directory must survive");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_refuses_a_symlink_leaf() {
        // A pre-existing in-workspace symlink must not be silently clobbered
        // (nor written through). Refuse, leaving both the link and its target
        // exactly as they were.
        let root = workspace();
        std::fs::write(root.join("real.txt"), "original").unwrap();
        std::os::unix::fs::symlink(root.join("real.txt"), root.join("link.txt")).unwrap();

        let tool = WriteFileTool::new(&root);
        let out = tool
            .run(
                ToolInput::new(json!({"path": "link.txt", "content": "new stuff"})),
                &ctx(),
            )
            .await;
        assert!(
            matches!(out, ToolResult::Err(ref e) if e.contains("symlink")),
            "writing through a symlink must be refused, got {out:?}"
        );
        // The real file is untouched and the link is still a link.
        assert_eq!(
            std::fs::read_to_string(root.join("real.txt")).unwrap(),
            "original"
        );
        assert!(
            std::fs::symlink_metadata(root.join("link.txt"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink must survive"
        );
    }

    #[test]
    fn write_tools_are_write_risk_read_tools_are_safe() {
        let root = workspace();
        assert_eq!(WriteFileTool::new(&root).risk(), RiskClass::Write);
        assert_eq!(EditFileTool::new(&root).risk(), RiskClass::Write);
        assert_eq!(DeleteFileTool::new(&root).risk(), RiskClass::Write);
        assert_eq!(ReadFileTool::new(&root).risk(), RiskClass::Safe);
    }

    // ── read-before-write guard ──────────────────────────────────────────

    #[tokio::test]
    async fn write_over_existing_unread_file_is_refused() {
        let root = workspace();
        let ctx = tracked_ctx();
        // hello.txt exists and has NOT been read this conversation.
        let out = WriteFileTool::new(&root)
            .run(
                ToolInput::new(json!({"path": "hello.txt", "content": "clobber"})),
                &ctx,
            )
            .await;
        assert!(
            matches!(out, ToolResult::Err(ref e) if e.contains("read_file it first")),
            "overwriting an unread existing file must be refused, got {out:?}"
        );
        // The file is untouched.
        assert_eq!(
            std::fs::read_to_string(root.join("hello.txt")).unwrap(),
            "hello world\nsecond line\n"
        );
    }

    #[tokio::test]
    async fn read_then_write_is_allowed() {
        let root = workspace();
        let ctx = tracked_ctx(); // one ctx → one shared read-set across both calls
        let _ = ReadFileTool::new(&root)
            .run(ToolInput::new(json!({"path": "hello.txt"})), &ctx)
            .await;
        let out = WriteFileTool::new(&root)
            .run(
                ToolInput::new(json!({"path": "hello.txt", "content": "updated"})),
                &ctx,
            )
            .await;
        assert!(
            matches!(out, ToolResult::Ok(_)),
            "read→write must be allowed, got {out:?}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("hello.txt")).unwrap(),
            "updated"
        );
    }

    #[tokio::test]
    async fn writing_a_brand_new_file_needs_no_prior_read() {
        let root = workspace();
        let ctx = tracked_ctx();
        // brand_new.txt does not exist → exempt from the guard.
        let out = WriteFileTool::new(&root)
            .run(
                ToolInput::new(json!({"path": "brand_new.txt", "content": "hi"})),
                &ctx,
            )
            .await;
        assert!(
            matches!(out, ToolResult::Ok(ref v) if v["created"] == true),
            "a new file is exempt from read-before-write, got {out:?}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("brand_new.txt")).unwrap(),
            "hi"
        );
    }

    #[tokio::test]
    async fn edit_without_read_is_refused_then_allowed_after_read() {
        let root = workspace();
        let ctx = tracked_ctx();
        let edit = EditFileTool::new(&root);

        // Blind edit → refused, file untouched.
        let refused = edit
            .run(
                ToolInput::new(json!({"path": "hello.txt", "old": "second line", "new": "X"})),
                &ctx,
            )
            .await;
        assert!(
            matches!(refused, ToolResult::Err(ref e) if e.contains("read_file it first")),
            "editing an unread file must be refused, got {refused:?}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("hello.txt")).unwrap(),
            "hello world\nsecond line\n"
        );

        // After a read, the same edit goes through.
        let _ = ReadFileTool::new(&root)
            .run(ToolInput::new(json!({"path": "hello.txt"})), &ctx)
            .await;
        let ok = edit
            .run(
                ToolInput::new(json!({"path": "hello.txt", "old": "second line", "new": "X"})),
                &ctx,
            )
            .await;
        assert!(
            matches!(ok, ToolResult::Ok(_)),
            "read→edit must be allowed, got {ok:?}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("hello.txt")).unwrap(),
            "hello world\nX\n"
        );
    }

    #[tokio::test]
    async fn guard_is_inert_without_a_reads_handle() {
        // No read-set wired (ExecCtx::default → reads=None): the guard is off,
        // so an isolated tool or an unwired dispatcher overwrites freely. This
        // is why the other fs tests using ctx() aren't affected by the guard.
        let root = workspace();
        let out = WriteFileTool::new(&root)
            .run(
                ToolInput::new(json!({"path": "hello.txt", "content": "z"})),
                &ctx(),
            )
            .await;
        assert!(
            matches!(out, ToolResult::Ok(_)),
            "no read-set → guard inert, got {out:?}"
        );
    }

    // ── read-before-write: review regressions ────────────────────────────

    /// Best-effort probe: is the workspace filesystem case-insensitive
    /// (macOS/Windows default)? Create a mixed-case file, see if the lowercased
    /// name resolves to it.
    fn fs_is_case_insensitive(root: &std::path::Path) -> bool {
        let upper = root.join("CaseProbe.tmp");
        if std::fs::write(&upper, "x").is_err() {
            return false;
        }
        let insensitive = root.join("caseprobe.tmp").exists();
        let _ = std::fs::remove_file(&upper);
        insensitive
    }

    #[tokio::test]
    async fn read_then_write_matches_despite_leaf_casing() {
        // Regression: read_file records the canonicalized (on-disk-cased) path,
        // write_file must check the same, or a real read→write of one file is
        // falsely refused on a case-insensitive FS. Only reproducible there.
        let root = workspace();
        if !fs_is_case_insensitive(&root) {
            return;
        }
        std::fs::write(root.join("Report.txt"), "orig").unwrap();
        let ctx = tracked_ctx();
        let read = ReadFileTool::new(&root)
            .run(ToolInput::new(json!({"path": "report.txt"})), &ctx)
            .await;
        assert!(
            matches!(read, ToolResult::Ok(_)),
            "lowercase read should succeed here, got {read:?}"
        );
        let write = WriteFileTool::new(&root)
            .run(
                ToolInput::new(json!({"path": "report.txt", "content": "new"})),
                &ctx,
            )
            .await;
        assert!(
            matches!(write, ToolResult::Ok(_)),
            "a real read→write of the same file must not be refused over leaf casing, got {write:?}"
        );
    }

    #[tokio::test]
    async fn write_new_file_then_overwrite_is_allowed() {
        // Regression: a freshly-created file must be recorded, so overwriting it
        // in the same conversation isn't falsely refused as "blind".
        let root = workspace();
        let ctx = tracked_ctx();
        let tool = WriteFileTool::new(&root);
        let first = tool
            .run(
                ToolInput::new(json!({"path": "fresh.txt", "content": "a"})),
                &ctx,
            )
            .await;
        assert!(
            matches!(first, ToolResult::Ok(ref v) if v["created"] == true),
            "create, got {first:?}"
        );
        let second = tool
            .run(
                ToolInput::new(json!({"path": "fresh.txt", "content": "b"})),
                &ctx,
            )
            .await;
        assert!(
            matches!(second, ToolResult::Ok(_)),
            "overwriting a just-created file must be allowed, got {second:?}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("fresh.txt")).unwrap(),
            "b"
        );
    }

    #[tokio::test]
    async fn read_then_write_in_subdir_is_allowed() {
        // Regression: exercise the parent != root path (the crux equivalence was
        // only tested at the workspace root before).
        let root = workspace(); // seeded with sub/notes.md
        let ctx = tracked_ctx();
        let _ = ReadFileTool::new(&root)
            .run(ToolInput::new(json!({"path": "sub/notes.md"})), &ctx)
            .await;
        let out = WriteFileTool::new(&root)
            .run(
                ToolInput::new(json!({"path": "sub/notes.md", "content": "rewritten"})),
                &ctx,
            )
            .await;
        assert!(
            matches!(out, ToolResult::Ok(_)),
            "read→write in a subdir must be allowed, got {out:?}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("sub/notes.md")).unwrap(),
            "rewritten"
        );
    }

    #[tokio::test]
    async fn a_large_but_writable_file_can_be_read_then_written() {
        // Regression: no "writable but unreadable" dead zone. 300 KiB is over the
        // OLD 256 KiB read cap but under the 1 MiB write cap.
        let root = workspace();
        let ctx = tracked_ctx();
        let big = "x".repeat(300 * 1024);
        std::fs::write(root.join("big.txt"), &big).unwrap();
        let read = ReadFileTool::new(&root)
            .run(ToolInput::new(json!({"path": "big.txt"})), &ctx)
            .await;
        assert!(
            matches!(read, ToolResult::Ok(_)),
            "a <=1MiB file must be readable, got {read:?}"
        );
        let write = WriteFileTool::new(&root)
            .run(
                ToolInput::new(json!({"path": "big.txt", "content": "small"})),
                &ctx,
            )
            .await;
        assert!(
            matches!(write, ToolResult::Ok(_)),
            "after reading, a large writable file must be overwritable, got {write:?}"
        );
    }

    // ── Tier-P: per-profile filesystem confinement ───────────────────────────

    #[test]
    fn profile_scope_mirrors_storage_and_fails_closed() {
        // A name `validate_profile_name` accepts gets its own subdir, lowercased
        // exactly the way `open_profile` folds its cache key + `<name>.db`.
        assert_eq!(
            profile_scope("work"),
            Ok(ProfileScope::Subdir("work".into()))
        );
        assert_eq!(
            profile_scope("profile_2-a"),
            Ok(ProfileScope::Subdir("profile_2-a".into()))
        );
        assert_eq!(
            profile_scope("Work"),
            Ok(ProfileScope::Subdir("work".into()))
        );
        // The case fold is load-bearing on a case-insensitive FS: `Work` and
        // `work` are ONE DB, so they must be ONE tree.
        assert_eq!(profile_scope("WORK"), profile_scope("work"));

        // The ONLY name that resolves to the shared base is the empty string —
        // the default/scratch ExecCtx, which `open_profile` also rejects, so it
        // can never alias a real profile.
        assert_eq!(profile_scope(""), Ok(ProfileScope::SharedBase));

        // Everything `validate_profile_name` rejects is an ERROR here, never a
        // path. This is the fail-closed half of M-03: a name storage will not
        // open a DB for must not be handed a filesystem tree either — and an
        // escaping form must not silently collapse onto the shared base, where
        // it would see every sibling profile's subtree by relative path.
        for bad in [
            "..",
            "../etc",
            "a/b",
            "a\\b",
            ".",
            "  ..  ",
            ".hidden",
            "sub/../..",
            "~",
            ".ssh",
            "my work",
            "café",
            "work@home",
            " work",
            "work ",
            "   ",
            "wo\trk",
            "work\0",
        ] {
            assert!(
                profile_scope(bad).is_err(),
                "profile {bad:?} must be rejected, got {:?}",
                profile_scope(bad)
            );
        }
        assert!(
            profile_scope(&"a".repeat(65)).is_err(),
            "over the 64-char cap"
        );
        assert!(
            profile_scope(&"a".repeat(64)).is_ok(),
            "exactly at the cap is fine"
        );

        // And the fail-closed root refuses to produce a path at all for those.
        let base = std::path::Path::new("/ws");
        assert_eq!(profile_workspace_root(base, "work"), Ok(base.join("work")));
        assert_eq!(profile_workspace_root(base, ""), Ok(base.to_path_buf()));
        assert!(profile_workspace_root(base, "../etc").is_err());
        assert!(profile_workspace_root(base, "my work").is_err());
        // No sentinel directory is ever invented for a bad name.
        for bad in ["..", "a/b", "my work", "   "] {
            let got = profile_workspace_root(base, bad);
            assert!(
                got.is_err(),
                "profile {bad:?} → {got:?} must be an error, not a path"
            );
        }

        // The infallible peek stays inside base for EVERY input: it is either
        // base itself or a direct (non-`..`) child of it.
        for name in [
            "work",
            "Work",
            "my work",
            "café",
            "..",
            "a/b",
            "",
            "~",
            ".ssh",
            "work@home",
            " work",
            "   ",
        ] {
            let got = profile_workspace_path(base, name);
            assert!(
                got == base || got.parent() == Some(base),
                "profile {name:?} → {got:?} must stay within base"
            );
        }
    }

    #[tokio::test]
    async fn two_profiles_get_physically_separate_trees() {
        // The SAME base + the SAME relative path, under two different profiles,
        // must be two different files — a `work` profile can't read `personal`'s.
        let root = workspace();
        let write = WriteFileTool::new(&root);
        let read = ReadFileTool::new(&root);

        // `work` writes secret.txt.
        let w = write
            .run(
                ToolInput::new(json!({"path": "secret.txt", "content": "work-only"})),
                &profile_ctx("work"),
            )
            .await;
        assert!(
            matches!(w, ToolResult::Ok(_)),
            "work write should succeed, got {w:?}"
        );

        // `personal` reading the SAME relative path must NOT see work's file.
        let cross = read
            .run(
                ToolInput::new(json!({"path": "secret.txt"})),
                &profile_ctx("personal"),
            )
            .await;
        assert!(
            matches!(cross, ToolResult::Err(_)),
            "personal must not see work's secret.txt — got {cross:?}"
        );

        // `work` reading its own file back does see it.
        let own = read
            .run(
                ToolInput::new(json!({"path": "secret.txt"})),
                &profile_ctx("work"),
            )
            .await;
        assert!(
            matches!(own, ToolResult::Ok(ref v) if v["content"] == "work-only"),
            "work must read back its own file, got {own:?}"
        );

        // And on disk they live in physically-separate per-profile subtrees.
        assert!(root.join("work").join("secret.txt").is_file());
        assert!(!root.join("personal").join("secret.txt").exists());
    }

    #[tokio::test]
    async fn per_profile_path_cannot_escape_its_subtree() {
        // Even a profile-bound call can't `..` out of its own per-profile root
        // into a sibling profile's tree (resolve_within rejects `..`).
        let root = workspace();
        std::fs::create_dir_all(root.join("personal")).unwrap();
        std::fs::write(root.join("personal").join("diary.txt"), "private").unwrap();
        let read = ReadFileTool::new(&root);
        let escape = read
            .run(
                ToolInput::new(json!({"path": "../personal/diary.txt"})),
                &profile_ctx("work"),
            )
            .await;
        assert!(
            matches!(escape, ToolResult::Err(ref e) if e.contains("'..'")),
            "a profile must not climb into a sibling profile's tree, got {escape:?}"
        );
    }

    #[tokio::test]
    async fn empty_profile_still_uses_the_shared_base() {
        // Backward-compat: a default/empty-profile ctx resolves at base, so a
        // file seeded at root is readable without any per-profile subdir.
        let root = workspace();
        let read = ReadFileTool::new(&root);
        let out = read
            .run(ToolInput::new(json!({"path": "hello.txt"})), &ctx())
            .await;
        assert!(
            matches!(out, ToolResult::Ok(ref v) if v["content"] == "hello world\nsecond line\n"),
            "empty profile must read the base-root file, got {out:?}"
        );
    }

    #[tokio::test]
    async fn an_invalid_profile_is_refused_by_every_tool_and_never_becomes_a_path() {
        // M-03 fail-closed. A profile string `validate_profile_name` rejects must
        // make every fs tool ERROR, not resolve. The two ways this has been got
        // wrong before are both asserted against below:
        //   1. collapsing to the shared base — the bad profile would then read
        //      and CLOBBER the base tree (every sibling profile's parent);
        //   2. inventing a `base/__invalid_profile__` sentinel — which turns a
        //      validation failure into a real directory on disk.
        let root = workspace();
        for bad in ["..", "a/b", " work", "my work", ".hidden"] {
            let bad_ctx = profile_ctx(bad);

            let w = WriteFileTool::new(&root)
                .run(
                    ToolInput::new(json!({"path": "planted.txt", "content": "x"})),
                    &bad_ctx,
                )
                .await;
            assert!(
                matches!(w, ToolResult::Err(ref e) if e.contains("invalid profile name")),
                "write_file under profile {bad:?} must be refused, got {w:?}"
            );

            let r = ReadFileTool::new(&root)
                .run(ToolInput::new(json!({"path": "hello.txt"})), &bad_ctx)
                .await;
            assert!(
                matches!(r, ToolResult::Err(ref e) if e.contains("invalid profile name")),
                "read_file under profile {bad:?} must be refused, got {r:?}"
            );

            let l = ListDirTool::new(&root)
                .run(ToolInput::new(json!({"path": "."})), &bad_ctx)
                .await;
            assert!(
                matches!(l, ToolResult::Err(ref e) if e.contains("invalid profile name")),
                "list_dir under profile {bad:?} must be refused, got {l:?}"
            );

            let s = SearchFilesTool::new(&root)
                .run(ToolInput::new(json!({"query": "needle"})), &bad_ctx)
                .await;
            assert!(
                matches!(s, ToolResult::Err(ref e) if e.contains("invalid profile name")),
                "search_files under profile {bad:?} must be refused, got {s:?}"
            );

            let ed = EditFileTool::new(&root)
                .run(
                    ToolInput::new(
                        json!({"path": "hello.txt", "old": "second line", "new": "PWNED"}),
                    ),
                    &bad_ctx,
                )
                .await;
            assert!(
                matches!(ed, ToolResult::Err(ref e) if e.contains("invalid profile name")),
                "edit_file under profile {bad:?} must be refused, got {ed:?}"
            );

            let d = DeleteFileTool::new(&root)
                .run(ToolInput::new(json!({"path": "hello.txt"})), &bad_ctx)
                .await;
            assert!(
                matches!(d, ToolResult::Err(ref e) if e.contains("invalid profile name")),
                "delete_file under profile {bad:?} must be refused, got {d:?}"
            );
        }

        // Nothing was written, read-through, deleted, or created anywhere.
        assert!(
            !root.join("planted.txt").exists(),
            "a rejected profile must not write at the shared base"
        );
        assert!(
            !root.join("__invalid_profile__").exists(),
            "no sentinel directory may be invented for a rejected profile"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("hello.txt")).unwrap(),
            "hello world\nsecond line\n",
            "a rejected profile must neither edit nor delete the base tree"
        );
        // No stray per-profile directory appeared for any of the bad names.
        let top: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let mut unexpected: Vec<&String> = top
            .iter()
            .filter(|n| !matches!(n.as_str(), "hello.txt" | "sub"))
            .collect();
        unexpected.sort();
        assert!(
            unexpected.is_empty(),
            "a rejected profile created something on disk: {unexpected:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── Tier-P: legacy-workspace migration ───────────────────────────────────

    #[tokio::test]
    async fn legacy_workspace_migrates_loose_files_into_the_default_profile_and_is_reachable() {
        // A pre-Tier-P install: loose files + a subdir pooled directly at the
        // shared workspace base. Loose FILES migrate under the default profile's
        // subtree (reachable by its tools); a DIRECTORY is left in place (never
        // moved — a dir can't be safely classified as legacy-vs-live).
        let ws = workspace(); // seeds hello.txt (file) + sub/notes.md (a subdir)
        migrate_legacy_workspace(&ws, "personal").unwrap();

        // The loose file is physically moved under personal/, not deleted.
        assert!(ws.join("personal").join("hello.txt").is_file());
        assert!(
            !ws.join("hello.txt").exists(),
            "the legacy file must be MOVED, not copied"
        );
        // The directory is left intact in place — NOT moved into personal.
        assert!(
            ws.join("sub").join("notes.md").is_file(),
            "a legacy dir stays put, data intact"
        );
        assert!(
            !ws.join("personal").join("sub").exists(),
            "a directory is never moved"
        );
        assert!(
            ws.join(LEGACY_MIGRATION_MARKER).is_file(),
            "marker recorded"
        );

        // The default ("personal") profile can now read its migrated file.
        let read = ReadFileTool::new(&ws);
        let out = read
            .run(
                ToolInput::new(json!({"path": "hello.txt"})),
                &profile_ctx("personal"),
            )
            .await;
        assert!(
            matches!(out, ToolResult::Ok(ref v) if v["content"] == "hello world\nsecond line\n"),
            "personal must read its migrated legacy file, got {out:?}"
        );
    }

    #[test]
    fn legacy_migration_is_idempotent_and_does_not_sweep_post_upgrade_files() {
        let ws = workspace();
        migrate_legacy_workspace(&ws, "personal").unwrap();
        // A file the user creates AFTER the migration (directly at base — e.g. a
        // different profile's fresh subtree, or scratch) must NOT be swept into
        // personal on a later boot.
        std::fs::write(ws.join("post_upgrade.txt"), "new").unwrap();
        std::fs::create_dir_all(ws.join("work")).unwrap();
        std::fs::write(ws.join("work").join("w.txt"), "work-file").unwrap();
        migrate_legacy_workspace(&ws, "personal").unwrap(); // second run: no-op

        assert!(
            ws.join("post_upgrade.txt").is_file(),
            "a post-migration base file stays put"
        );
        assert!(
            ws.join("work").join("w.txt").is_file(),
            "a fresh profile subtree is untouched"
        );
        assert!(!ws.join("personal").join("post_upgrade.txt").exists());
        assert!(!ws.join("personal").join("work").exists());
    }

    #[test]
    fn legacy_migration_never_clobbers_and_stamps_on_empty_root() {
        // Fresh install (no workspace dir yet): stamp the marker, create the dir,
        // move nothing.
        let base = std::env::temp_dir().join(format!("lhp-fs-fresh-{}", uuid::Uuid::new_v4()));
        migrate_legacy_workspace(&base, "personal").unwrap();
        assert!(base.join(LEGACY_MIGRATION_MARKER).is_file());
        assert!(
            !base.join("personal").exists(),
            "nothing to migrate → no default subtree forced"
        );
        let _ = std::fs::remove_dir_all(&base);

        // Clobber-safety: a destination entry already present is left in place.
        let ws = std::env::temp_dir().join(format!("lhp-fs-clob-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(ws.join("personal")).unwrap();
        std::fs::write(ws.join("personal").join("dup.txt"), "DEST").unwrap();
        std::fs::write(ws.join("dup.txt"), "LEGACY").unwrap();
        migrate_legacy_workspace(&ws, "personal").unwrap();
        assert_eq!(
            std::fs::read_to_string(ws.join("personal").join("dup.txt")).unwrap(),
            "DEST",
            "an existing destination entry must never be clobbered"
        );
        assert!(
            ws.join("dup.txt").is_file(),
            "the un-migratable legacy entry is left in place, not lost"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn legacy_migration_never_moves_a_directory_only_loose_files() {
        // REGRESSION (adversarial re-review, HIGH + MEDIUM): the structural
        // invariant that makes profile mis-attribution impossible. A DIRECTORY at
        // the workspace root — whether a live profile tree, an ORPHANED tree whose
        // DB desynced, or a legacy subdir — is NEVER moved, with NO marker and NO
        // known-profile list needed. Only loose regular FILES migrate. This also
        // means a legacy file whose NAME collides with a profile is moved (as a
        // file), never skipped-and-stranded into an ENOTDIR-breaking state.
        let ws = std::env::temp_dir().join(format!("lhp-fs-live-{}", uuid::Uuid::new_v4()));
        // An orphaned/live profile-shaped tree — its DB is NOT known here at all.
        std::fs::create_dir_all(ws.join("work")).unwrap();
        std::fs::write(ws.join("work").join("secret.txt"), "work-only").unwrap();
        // A plain legacy subdir (arbitrary name) and a loose legacy file.
        std::fs::create_dir_all(ws.join("project")).unwrap();
        std::fs::write(ws.join("project").join("main.rs").as_path(), "fn main(){}").unwrap();
        std::fs::write(ws.join("legacy_note.txt"), "legacy").unwrap();
        // No marker; no known-profile list exists in the API anymore.
        migrate_legacy_workspace(&ws, "personal").unwrap();

        // Every directory is untouched in place — NOT folded into personal.
        assert_eq!(
            std::fs::read_to_string(ws.join("work").join("secret.txt")).unwrap(),
            "work-only",
            "an orphaned/live profile tree must never be swept, even with no known-profile signal"
        );
        assert!(
            !ws.join("personal").join("work").exists(),
            "work dir must not be nested under personal"
        );
        assert!(
            ws.join("project").join("main.rs").is_file(),
            "a legacy subdir stays put, data intact"
        );
        assert!(
            !ws.join("personal").join("project").exists(),
            "no directory is ever moved"
        );
        // The genuinely-loose legacy file DID migrate.
        assert!(
            ws.join("personal").join("legacy_note.txt").is_file(),
            "loose legacy files still migrate"
        );
        assert!(!ws.join("legacy_note.txt").exists());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn legacy_migration_moves_a_file_named_like_a_profile_without_breaking_it() {
        // REGRESSION (adversarial re-review, MEDIUM): a legacy loose FILE whose
        // name equals a profile ("work") must be MOVED (it's a file), never left
        // at workspace/work where the work profile's `create_dir_all` would hit a
        // non-directory and every fs tool would fail with ENOTDIR.
        let ws = workspace();
        std::fs::write(ws.join("work"), "a file, not a dir").unwrap();
        migrate_legacy_workspace(&ws, "personal").unwrap();
        assert!(
            ws.join("personal").join("work").is_file(),
            "a profile-named FILE is migrated into personal, not skipped"
        );
        assert!(
            !ws.join("work").exists(),
            "workspace/work is freed, so the work profile can mkdir it"
        );

        // Proof there's no ENOTDIR breakage: the `work` profile now creates and
        // writes into its own fresh directory tree without error.
        let out = WriteFileTool::new(&ws)
            .run(
                ToolInput::new(json!({"path": "note.txt", "content": "hi"})),
                &profile_ctx("work"),
            )
            .await;
        assert!(
            matches!(out, ToolResult::Ok(_)),
            "the work profile's tools must work, got {out:?}"
        );
        assert!(
            ws.join("work").join("note.txt").is_file(),
            "work now has a real directory tree"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn legacy_migration_marker_is_content_checked_not_spoofable_by_name() {
        // REGRESSION (adversarial re-review, LOW): a pre-existing file literally
        // named `.tierp-migrated` (wrong content) must NOT be taken as "already
        // migrated" — otherwise legacy data would be stranded. Migration proceeds,
        // then (re)writes the marker with the real magic.
        let ws = workspace(); // seeds hello.txt at base
        std::fs::write(ws.join(LEGACY_MIGRATION_MARKER), "not our magic").unwrap();
        migrate_legacy_workspace(&ws, "personal").unwrap();

        assert!(
            ws.join("personal").join("hello.txt").is_file(),
            "a spoofed marker must not skip migration — legacy data still moves"
        );
        assert_eq!(
            std::fs::read_to_string(ws.join(LEGACY_MIGRATION_MARKER)).unwrap(),
            LEGACY_MIGRATION_MAGIC,
            "the marker is rewritten with the real magic after a real migration"
        );
        // And now it IS treated as done (idempotent second run).
        std::fs::write(ws.join("late.txt"), "post").unwrap();
        migrate_legacy_workspace(&ws, "personal").unwrap();
        assert!(
            ws.join("late.txt").is_file(),
            "a valid marker now short-circuits"
        );
        assert!(!ws.join("personal").join("late.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn legacy_migration_does_not_clobber_a_dangling_destination_symlink() {
        // REGRESSION (adversarial re-review, MEDIUM/HIGH): the never-clobber guard
        // must use lstat, not `Path::exists()` (which FOLLOWS symlinks). A DANGLING
        // destination symlink — e.g. a profile subfolder pointing at a not-yet-
        // mounted volume — must NOT be silently replaced/orphaned by an incoming
        // legacy file of the same name.
        let ws = std::env::temp_dir().join(format!("lhp-fs-dangle-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(ws.join("personal")).unwrap();
        // A dangling symlink at the destination (target does not exist).
        std::os::unix::fs::symlink(
            ws.join("nonexistent-mount-point"),
            ws.join("personal").join("photos"),
        )
        .unwrap();
        // A legacy loose file with the SAME name at the workspace root.
        std::fs::write(ws.join("photos"), "LEGACY DATA").unwrap();

        migrate_legacy_workspace(&ws, "personal").unwrap();

        // The dangling symlink is preserved as a symlink — NOT clobbered.
        let meta = std::fs::symlink_metadata(ws.join("personal").join("photos")).unwrap();
        assert!(
            meta.file_type().is_symlink(),
            "a dangling destination symlink must survive the migration untouched"
        );
        // The legacy file is left in place (collision), not lost.
        assert!(
            ws.join("photos").is_file(),
            "the un-migratable legacy file stays put, not lost"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }
}
