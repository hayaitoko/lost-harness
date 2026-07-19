//! Read-only filesystem tools — the first real tools wired through the M3
//! spine. Spec `docs/PLAN.md` §8 (M3 build order item 10: "file read/list/
//! search"). Write/delete tools are deliberately a later round, because
//! they need the approval spine (item 9) which isn't built yet.
//!
//! Every tool here requires `Capability::Filesystem` and is confined to a
//! single **workspace root**: paths are relative, `..` is rejected, and the
//! canonicalized target must stay inside the root (so a symlink can't be
//! used to escape). This is defense-in-depth *below* the hook chain — even
//! before a call reaches the sandbox/permission gates, a read tool
//! structurally cannot wander to `/etc/shadow`.

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

/// Resolve `rel` against `root`, rejecting anything that could escape the
/// workspace. Requires the target to exist (uses `canonicalize`), which is
/// correct for the read-only tools here.
fn resolve_within(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(format!("path must be relative to the workspace, got: {rel}"));
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

/// Wave 5.4 / M7 (Tier-P) — the PER-PROFILE workspace root under `base`. Each
/// profile gets its own physically-separate directory (`base/<profile>`), the
/// same in-process isolation walled memory (§7) gives, so a `work` profile's
/// files never sit in the `personal` profile's tree. This is the primary
/// filesystem-confinement boundary; a kernel jail (Tier-K) is the on-target
/// hardening layered ON TOP.
///
/// Pure + TRAVERSAL-SAFE by construction. The rule MIRRORS
/// `Storage::open_profile` EXACTLY: apply the identical denylist to the RAW name
/// (reject only the path-escaping forms — `/`, `\`, `..`, a leading `.`, empty),
/// then use it VERBATIM as the subdir. This byte-for-byte equivalence is
/// load-bearing and adversarially verified: `open_profile` keys a distinct
/// `profiles/<name>.db` (and a distinct walled-memory island) off the raw name,
/// so this MUST bucket by the raw name too, or two names `open_profile` treats
/// as DISTINCT profiles would share one filesystem tree. Two traps that broke
/// earlier cuts, both now avoided:
/// - An allowlist (`[A-Za-z0-9_-]`) collapses a space/Unicode/punctuation name
///   that `open_profile` accepts ("my work", "café") down to `base`.
/// - A `.trim()` before the denylist collapses `" work"`/`"work "` onto `work`
///   (and a whitespace-only name onto `base` itself), even though `open_profile`
///   does NOT trim and treats each as its own profile. So do NOT trim.
///
/// Because a passing name has no separator and isn't `..`, `base.join(name)` is
/// always a unique direct child of `base` (distinct names → distinct trees, no
/// escape); the only name that collapses to `base` is the EMPTY string — the
/// default/scratch `ExecCtx`, which `open_profile` also rejects, so it can never
/// alias a real profile. Callers (the fs tools) create the dir; the
/// `ProtectedPathHook` uses the path read-only.
pub fn profile_workspace_path(base: &std::path::Path, profile: &str) -> PathBuf {
    if profile.is_empty()
        || profile.contains('/')
        || profile.contains('\\')
        || profile.contains("..")
        || profile.starts_with('.')
    {
        return base.to_path_buf();
    }
    base.join(profile)
}

/// The filename that records the legacy-workspace migration already ran.
const LEGACY_MIGRATION_MARKER: &str = ".tierp-migrated";
/// The marker's CONTENT sentinel. Presence-only checks are unsafe (a legacy
/// file that happens to be named `.tierp-migrated` would spoof "already done"
/// and strand real data); we treat migration as done only when the marker holds
/// this exact magic, and we (re)write it ourselves.
const LEGACY_MIGRATION_MAGIC: &str = "lost-harness tier-p workspace migration v1\n";

fn migration_is_done(marker: &std::path::Path) -> bool {
    std::fs::read_to_string(marker).map(|s| s == LEGACY_MIGRATION_MAGIC).unwrap_or(false)
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
            let ws = profile_workspace_path(&self.root, &ctx.profile);
            let _ = std::fs::create_dir_all(&ws);
            let resolved = match resolve_within(&ws, path) {
                Ok(p) => p,
                Err(e) => return ToolResult::Err(e),
            };
            let meta = match std::fs::metadata(&resolved) {
                Ok(m) => m,
                Err(e) => return ToolResult::Err(format!("stat '{path}': {e}")),
            };
            if !meta.is_file() {
                return ToolResult::Err(format!("'{path}' is not a file"));
            }
            if meta.len() > MAX_READ_BYTES {
                return ToolResult::Err(format!(
                    "'{path}' is {} bytes, over the {MAX_READ_BYTES}-byte read limit",
                    meta.len()
                ));
            }
            match std::fs::read_to_string(&resolved) {
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
                Err(e) => ToolResult::Err(format!("read '{path}': {e} (not UTF-8 text?)")),
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
            let ws = profile_workspace_path(&self.root, &ctx.profile);
            let _ = std::fs::create_dir_all(&ws);
            let resolved = match resolve_within(&ws, path) {
                Ok(p) => p,
                Err(e) => return ToolResult::Err(e),
            };
            let read_dir = match std::fs::read_dir(&resolved) {
                Ok(rd) => rd,
                Err(e) => return ToolResult::Err(format!("list '{path}': {e}")),
            };
            let mut entries = Vec::new();
            for entry in read_dir.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let kind = match entry.file_type() {
                    Ok(t) if t.is_dir() => "dir",
                    Ok(t) if t.is_file() => "file",
                    Ok(t) if t.is_symlink() => "symlink",
                    _ => "other",
                };
                entries.push(json!({ "name": name, "kind": kind }));
            }
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
            let ws = profile_workspace_path(&self.root, &ctx.profile);
            let _ = std::fs::create_dir_all(&ws);
            let canon_root = match ws.canonicalize() {
                Ok(r) => r,
                Err(e) => return ToolResult::Err(format!("workspace root unavailable: {e}")),
            };
            let needle = query.to_lowercase();
            let mut matches = Vec::new();
            let mut scanned = 0usize;
            walk(&canon_root, &canon_root, 0, &needle, &mut matches, &mut scanned);
            let truncated = matches.len() >= SEARCH_MAX_RESULTS
                || scanned >= SEARCH_MAX_FILES_SCANNED;
            ToolResult::Ok(json!({
                "query": query,
                "matches": matches,
                "truncated": truncated,
            }))
        })
    }
}

/// Recursive, bounded workspace walk. `rel` paths in results are relative to
/// the workspace root so nothing leaks the absolute on-disk location.
fn walk(
    root: &Path,
    dir: &Path,
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
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        if matches.len() >= SEARCH_MAX_RESULTS || *scanned >= SEARCH_MAX_FILES_SCANNED {
            return;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            walk(root, &path, depth + 1, needle, matches, scanned);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        *scanned += 1;
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let name_hit = rel.to_lowercase().contains(needle);

        // Content match — only for reasonably-sized files we can read as text.
        let mut content_hit: Option<serde_json::Value> = None;
        if let Ok(meta) = entry.metadata() {
            if meta.len() <= SEARCH_MAX_FILE_BYTES {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    if let Some((lineno, line)) = text
                        .lines()
                        .enumerate()
                        .find(|(_, l)| l.to_lowercase().contains(needle))
                    {
                        let snippet: String = line.trim().chars().take(200).collect();
                        content_hit = Some(json!({ "line": lineno + 1, "snippet": snippet }));
                    }
                }
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
        return Err(format!("path must be relative to the workspace, got: {rel}"));
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
            let Some(path) = arg_str(&input, "path") else {
                return ToolResult::Err("write_file requires a string \"path\" arg".to_string());
            };
            let Some(content) = arg_str(&input, "content") else {
                return ToolResult::Err(
                    "write_file requires a string \"content\" arg".to_string(),
                );
            };
            if content.len() > MAX_WRITE_BYTES {
                return ToolResult::Err(format!(
                    "content is {} bytes, over the {MAX_WRITE_BYTES}-byte write limit",
                    content.len()
                ));
            }
            let ws = profile_workspace_path(&self.root, &ctx.profile);
            let _ = std::fs::create_dir_all(&ws);
            let resolved = match resolve_within_new(&ws, path) {
                Ok(p) => p,
                Err(e) => return ToolResult::Err(e),
            };
            // Refuse to clobber a directory with a file write.
            if resolved.is_dir() {
                return ToolResult::Err(format!("'{path}' is a directory"));
            }
            let existed = resolved.exists();
            // Read-before-write: refuse to overwrite an EXISTING file the agent
            // hasn't read this conversation, so it can't clobber blind (matches
            // Claude Code). A brand-new file is exempt — nothing to lose. No-op
            // unless the dispatcher wired a read-set into the context.
            if existed {
                if let Some(reads) = &ctx.reads {
                    // Match read_file's recorded key: it records the FULLY
                    // canonicalized path (leaf case/normalization corrected to
                    // the on-disk form). `resolved` here keeps the RAW requested
                    // leaf (resolve_within_new only canonicalizes the parent),
                    // which differs on case-insensitive / Unicode-normalizing
                    // filesystems (macOS/Windows) — so canonicalize the existing
                    // target before the membership check, or a real read→write
                    // is falsely refused.
                    let key = std::fs::canonicalize(&resolved).unwrap_or_else(|_| resolved.clone());
                    if !reads.contains(&ctx.conversation_id, &key) {
                        return ToolResult::Err(format!(
                            "refusing to write '{path}': read_file it first so you're not overwriting blind"
                        ));
                    }
                }
            }
            match atomic_write(&resolved, content) {
                Ok(()) => {
                    // The agent authored this content, so it isn't "blind" to
                    // the file — record it (by canonical path, now that it
                    // exists) so a later overwrite/edit this conversation isn't
                    // refused. Covers the create-then-overwrite case.
                    if let Some(reads) = &ctx.reads {
                        let key =
                            std::fs::canonicalize(&resolved).unwrap_or_else(|_| resolved.clone());
                        reads.record(&ctx.conversation_id, key);
                    }
                    ToolResult::Ok(json!({
                        "path": path,
                        "bytes_written": content.len(),
                        "created": !existed,
                    }))
                }
                Err(e) => ToolResult::Err(format!("write '{path}': {e}")),
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
            let Some(path) = arg_str(&input, "path") else {
                return ToolResult::Err("edit_file requires a string \"path\" arg".to_string());
            };
            let Some(old) = arg_str(&input, "old") else {
                return ToolResult::Err("edit_file requires a string \"old\" arg".to_string());
            };
            let Some(new) = arg_str(&input, "new") else {
                return ToolResult::Err("edit_file requires a string \"new\" arg".to_string());
            };
            if old.is_empty() {
                return ToolResult::Err("edit_file \"old\" must not be empty".to_string());
            }
            // Must exist — use the strict resolver.
            let ws = profile_workspace_path(&self.root, &ctx.profile);
            let _ = std::fs::create_dir_all(&ws);
            let resolved = match resolve_within(&ws, path) {
                Ok(p) => p,
                Err(e) => return ToolResult::Err(e),
            };
            // Read-before-write: an edit rewrites the file, so require it was
            // read this conversation first (a blind-edit guard). No-op unless
            // the dispatcher wired a read-set into the context.
            if let Some(reads) = &ctx.reads {
                if !reads.contains(&ctx.conversation_id, &resolved) {
                    return ToolResult::Err(format!(
                        "refusing to edit '{path}': read_file it first so you're not editing blind"
                    ));
                }
            }
            let content = match std::fs::read_to_string(&resolved) {
                Ok(c) => c,
                Err(e) => return ToolResult::Err(format!("read '{path}': {e} (not UTF-8 text?)")),
            };
            let count = content.matches(old).count();
            if count == 0 {
                return ToolResult::Err(format!("edit_file: \"old\" not found in '{path}'"));
            }
            if count > 1 {
                return ToolResult::Err(format!(
                    "edit_file: \"old\" occurs {count} times in '{path}' — make it unique (add surrounding context)"
                ));
            }
            let updated = content.replacen(old, new, 1);
            if updated.len() > MAX_WRITE_BYTES {
                return ToolResult::Err(format!(
                    "result is {} bytes, over the {MAX_WRITE_BYTES}-byte write limit",
                    updated.len()
                ));
            }
            match atomic_write(&resolved, &updated) {
                Ok(()) => ToolResult::Ok(json!({
                    "path": path,
                    "replaced": 1,
                    "bytes": updated.len(),
                })),
                Err(e) => ToolResult::Err(format!("write '{path}': {e}")),
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
            let Some(path) = arg_str(&input, "path") else {
                return ToolResult::Err("delete_file requires a string \"path\" arg".to_string());
            };
            let ws = profile_workspace_path(&self.root, &ctx.profile);
            let _ = std::fs::create_dir_all(&ws);
            let resolved = match resolve_within(&ws, path) {
                Ok(p) => p,
                Err(e) => return ToolResult::Err(e),
            };
            if resolved.is_dir() {
                return ToolResult::Err(format!(
                    "'{path}' is a directory — delete_file only removes files"
                ));
            }
            match std::fs::remove_file(&resolved) {
                Ok(()) => ToolResult::Ok(json!({ "path": path, "deleted": true })),
                Err(e) => ToolResult::Err(format!("delete '{path}': {e}")),
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
                let names: Vec<&str> =
                    entries.iter().map(|e| e["name"].as_str().unwrap()).collect();
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
                    matches.iter().any(|m| m["path"].as_str() == Some("sub/notes.md")),
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
            .run(ToolInput::new(json!({"path": "new.txt", "content": "first"})), &ctx())
            .await;
        match out {
            ToolResult::Ok(v) => {
                assert_eq!(v["created"], true);
                assert_eq!(v["bytes_written"], 5);
            }
            ToolResult::Err(e) => panic!("expected Ok, got Err({e})"),
        }
        assert_eq!(std::fs::read_to_string(root.join("new.txt")).unwrap(), "first");

        let out2 = tool
            .run(ToolInput::new(json!({"path": "new.txt", "content": "second!"})), &ctx())
            .await;
        assert!(
            matches!(out2, ToolResult::Ok(ref v) if v["created"] == false),
            "overwrite should report created=false, got {out2:?}"
        );
        assert_eq!(std::fs::read_to_string(root.join("new.txt")).unwrap(), "second!");
    }

    #[tokio::test]
    async fn write_file_rejects_escape_and_writes_nothing_outside() {
        let root = workspace();
        let tool = WriteFileTool::new(&root);
        let out = tool
            .run(ToolInput::new(json!({"path": "../evil.txt", "content": "x"})), &ctx())
            .await;
        assert!(matches!(out, ToolResult::Err(_)), "escape must be rejected, got {out:?}");
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
                ToolInput::new(json!({"path": "hello.txt", "old": "second line", "new": "SECOND LINE"})),
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
            .run(ToolInput::new(json!({"path": "hello.txt", "old": "nope", "new": "x"})), &ctx())
            .await;
        assert!(
            matches!(miss, ToolResult::Err(ref e) if e.contains("not found")),
            "missing match must error, got {miss:?}"
        );

        // "l" occurs many times → ambiguous, must refuse.
        let ambig = tool
            .run(ToolInput::new(json!({"path": "hello.txt", "old": "l", "new": "L"})), &ctx())
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

        let ok = tool.run(ToolInput::new(json!({"path": "hello.txt"})), &ctx()).await;
        assert!(matches!(ok, ToolResult::Ok(_)), "expected Ok, got {ok:?}");
        assert!(!root.join("hello.txt").exists());

        let dir = tool.run(ToolInput::new(json!({"path": "sub"})), &ctx()).await;
        assert!(matches!(dir, ToolResult::Err(_)), "deleting a directory must be refused");
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
            .run(ToolInput::new(json!({"path": "link.txt", "content": "new stuff"})), &ctx())
            .await;
        assert!(
            matches!(out, ToolResult::Err(ref e) if e.contains("symlink")),
            "writing through a symlink must be refused, got {out:?}"
        );
        // The real file is untouched and the link is still a link.
        assert_eq!(std::fs::read_to_string(root.join("real.txt")).unwrap(), "original");
        assert!(
            std::fs::symlink_metadata(root.join("link.txt")).unwrap().file_type().is_symlink(),
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
            .run(ToolInput::new(json!({"path": "hello.txt", "content": "clobber"})), &ctx)
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
            .run(ToolInput::new(json!({"path": "hello.txt", "content": "updated"})), &ctx)
            .await;
        assert!(matches!(out, ToolResult::Ok(_)), "read→write must be allowed, got {out:?}");
        assert_eq!(std::fs::read_to_string(root.join("hello.txt")).unwrap(), "updated");
    }

    #[tokio::test]
    async fn writing_a_brand_new_file_needs_no_prior_read() {
        let root = workspace();
        let ctx = tracked_ctx();
        // brand_new.txt does not exist → exempt from the guard.
        let out = WriteFileTool::new(&root)
            .run(ToolInput::new(json!({"path": "brand_new.txt", "content": "hi"})), &ctx)
            .await;
        assert!(
            matches!(out, ToolResult::Ok(ref v) if v["created"] == true),
            "a new file is exempt from read-before-write, got {out:?}"
        );
        assert_eq!(std::fs::read_to_string(root.join("brand_new.txt")).unwrap(), "hi");
    }

    #[tokio::test]
    async fn edit_without_read_is_refused_then_allowed_after_read() {
        let root = workspace();
        let ctx = tracked_ctx();
        let edit = EditFileTool::new(&root);

        // Blind edit → refused, file untouched.
        let refused = edit
            .run(ToolInput::new(json!({"path": "hello.txt", "old": "second line", "new": "X"})), &ctx)
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
            .run(ToolInput::new(json!({"path": "hello.txt", "old": "second line", "new": "X"})), &ctx)
            .await;
        assert!(matches!(ok, ToolResult::Ok(_)), "read→edit must be allowed, got {ok:?}");
        assert_eq!(std::fs::read_to_string(root.join("hello.txt")).unwrap(), "hello world\nX\n");
    }

    #[tokio::test]
    async fn guard_is_inert_without_a_reads_handle() {
        // No read-set wired (ExecCtx::default → reads=None): the guard is off,
        // so an isolated tool or an unwired dispatcher overwrites freely. This
        // is why the other fs tests using ctx() aren't affected by the guard.
        let root = workspace();
        let out = WriteFileTool::new(&root)
            .run(ToolInput::new(json!({"path": "hello.txt", "content": "z"})), &ctx())
            .await;
        assert!(matches!(out, ToolResult::Ok(_)), "no read-set → guard inert, got {out:?}");
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
        assert!(matches!(read, ToolResult::Ok(_)), "lowercase read should succeed here, got {read:?}");
        let write = WriteFileTool::new(&root)
            .run(ToolInput::new(json!({"path": "report.txt", "content": "new"})), &ctx)
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
            .run(ToolInput::new(json!({"path": "fresh.txt", "content": "a"})), &ctx)
            .await;
        assert!(matches!(first, ToolResult::Ok(ref v) if v["created"] == true), "create, got {first:?}");
        let second = tool
            .run(ToolInput::new(json!({"path": "fresh.txt", "content": "b"})), &ctx)
            .await;
        assert!(
            matches!(second, ToolResult::Ok(_)),
            "overwriting a just-created file must be allowed, got {second:?}"
        );
        assert_eq!(std::fs::read_to_string(root.join("fresh.txt")).unwrap(), "b");
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
            .run(ToolInput::new(json!({"path": "sub/notes.md", "content": "rewritten"})), &ctx)
            .await;
        assert!(matches!(out, ToolResult::Ok(_)), "read→write in a subdir must be allowed, got {out:?}");
        assert_eq!(std::fs::read_to_string(root.join("sub/notes.md")).unwrap(), "rewritten");
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
        assert!(matches!(read, ToolResult::Ok(_)), "a <=1MiB file must be readable, got {read:?}");
        let write = WriteFileTool::new(&root)
            .run(ToolInput::new(json!({"path": "big.txt", "content": "small"})), &ctx)
            .await;
        assert!(
            matches!(write, ToolResult::Ok(_)),
            "after reading, a large writable file must be overwritable, got {write:?}"
        );
    }

    // ── Tier-P: per-profile filesystem confinement ───────────────────────────

    #[test]
    fn profile_workspace_path_is_traversal_safe() {
        let base = std::path::Path::new("/ws");
        // A simple identifier gets its own subdir.
        assert_eq!(profile_workspace_path(base, "work"), base.join("work"));
        assert_eq!(profile_workspace_path(base, "profile_2-a"), base.join("profile_2-a"));
        // ONLY the empty string collapses to the shared base (the default/
        // scratch ctx, which open_profile also rejects). Nothing else does.
        assert_eq!(profile_workspace_path(base, ""), base.to_path_buf());
        // A name `open_profile` ALSO accepts — a space, Unicode, or punctuation
        // that isn't a path separator — must get its OWN verbatim subdir, NOT
        // collapse to base. Otherwise two distinct real profiles would silently
        // share one tree (the isolation hole an allowlist would open).
        assert_eq!(profile_workspace_path(base, "my work"), base.join("my work"));
        assert_eq!(profile_workspace_path(base, "café"), base.join("café"));
        assert_eq!(profile_workspace_path(base, "work@home"), base.join("work@home"));
        // Distinct valid names never map onto the same tree.
        assert_ne!(
            profile_workspace_path(base, "my work"),
            profile_workspace_path(base, "personal space"),
        );
        // REGRESSION (adversarial review, HIGH): the helper must NOT trim. Since
        // open_profile does not trim, `" work"`, `"work "`, and `"work"` are
        // THREE distinct profiles (each its own profiles/<name>.db) — they must
        // map to THREE distinct trees, never collide onto base/work. And a
        // whitespace-only name (a distinct profile per open_profile) must get its
        // own subdir, never collapse to base itself (which would expose every
        // sibling profile's tree through an ordinary relative path).
        assert_eq!(profile_workspace_path(base, " work"), base.join(" work"));
        assert_eq!(profile_workspace_path(base, "work "), base.join("work "));
        assert_ne!(profile_workspace_path(base, " work"), profile_workspace_path(base, "work"));
        assert_ne!(profile_workspace_path(base, "work "), profile_workspace_path(base, "work"));
        assert_ne!(profile_workspace_path(base, " work"), profile_workspace_path(base, "work "));
        assert_ne!(profile_workspace_path(base, "   "), base.to_path_buf());
        assert_eq!(profile_workspace_path(base, "   "), base.join("   "));
        // The path-escaping forms `open_profile` rejects (`/`, `\`, `..`, a
        // leading `.`) collapse to base fail-safe — never a climb-out.
        for evil in ["..", "../etc", "a/b", "a\\b", ".", "  ..  ", ".hidden", "sub/../.."] {
            assert_eq!(
                profile_workspace_path(base, evil),
                base.to_path_buf(),
                "an escaping profile {evil:?} must collapse to base, never escape it"
            );
        }
        // Core invariant: the result is NEVER outside base — it is either base
        // itself or a direct (non-`..`) child of it, for every input.
        for name in ["work", "my work", "café", "..", "a/b", "", "~", ".ssh", "work@home", " work", "   "] {
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
        assert!(matches!(w, ToolResult::Ok(_)), "work write should succeed, got {w:?}");

        // `personal` reading the SAME relative path must NOT see work's file.
        let cross = read
            .run(ToolInput::new(json!({"path": "secret.txt"})), &profile_ctx("personal"))
            .await;
        assert!(
            matches!(cross, ToolResult::Err(_)),
            "personal must not see work's secret.txt — got {cross:?}"
        );

        // `work` reading its own file back does see it.
        let own = read
            .run(ToolInput::new(json!({"path": "secret.txt"})), &profile_ctx("work"))
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
        assert!(!ws.join("hello.txt").exists(), "the legacy file must be MOVED, not copied");
        // The directory is left intact in place — NOT moved into personal.
        assert!(ws.join("sub").join("notes.md").is_file(), "a legacy dir stays put, data intact");
        assert!(!ws.join("personal").join("sub").exists(), "a directory is never moved");
        assert!(ws.join(LEGACY_MIGRATION_MARKER).is_file(), "marker recorded");

        // The default ("personal") profile can now read its migrated file.
        let read = ReadFileTool::new(&ws);
        let out = read
            .run(ToolInput::new(json!({"path": "hello.txt"})), &profile_ctx("personal"))
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

        assert!(ws.join("post_upgrade.txt").is_file(), "a post-migration base file stays put");
        assert!(ws.join("work").join("w.txt").is_file(), "a fresh profile subtree is untouched");
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
        assert!(!base.join("personal").exists(), "nothing to migrate → no default subtree forced");
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
        assert!(ws.join("dup.txt").is_file(), "the un-migratable legacy entry is left in place, not lost");
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
        assert!(!ws.join("personal").join("work").exists(), "work dir must not be nested under personal");
        assert!(ws.join("project").join("main.rs").is_file(), "a legacy subdir stays put, data intact");
        assert!(!ws.join("personal").join("project").exists(), "no directory is ever moved");
        // The genuinely-loose legacy file DID migrate.
        assert!(ws.join("personal").join("legacy_note.txt").is_file(), "loose legacy files still migrate");
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
        assert!(!ws.join("work").exists(), "workspace/work is freed, so the work profile can mkdir it");

        // Proof there's no ENOTDIR breakage: the `work` profile now creates and
        // writes into its own fresh directory tree without error.
        let out = WriteFileTool::new(&ws)
            .run(ToolInput::new(json!({"path": "note.txt", "content": "hi"})), &profile_ctx("work"))
            .await;
        assert!(matches!(out, ToolResult::Ok(_)), "the work profile's tools must work, got {out:?}");
        assert!(ws.join("work").join("note.txt").is_file(), "work now has a real directory tree");
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
        assert!(ws.join("late.txt").is_file(), "a valid marker now short-circuits");
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
        assert!(ws.join("photos").is_file(), "the un-migratable legacy file stays put, not lost");
        let _ = std::fs::remove_dir_all(&ws);
    }
}
