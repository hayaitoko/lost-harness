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
            let resolved = match resolve_within(&self.root, path) {
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
        _ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let path = arg_str(&input, "path").unwrap_or(".");
            let resolved = match resolve_within(&self.root, path) {
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
        _ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let Some(query) = arg_str(&input, "query") else {
                return ToolResult::Err("search_files requires a string \"query\" arg".to_string());
            };
            if query.is_empty() {
                return ToolResult::Err("search_files \"query\" must not be empty".to_string());
            }
            let canon_root = match self.root.canonicalize() {
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
            let resolved = match resolve_within_new(&self.root, path) {
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
            let resolved = match resolve_within(&self.root, path) {
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
        _ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let Some(path) = arg_str(&input, "path") else {
                return ToolResult::Err("delete_file requires a string \"path\" arg".to_string());
            };
            let resolved = match resolve_within(&self.root, path) {
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
        ExecCtx {
            conversation_id: "conv-1".to_string(),
            profile: "personal".to_string(),
            reads: Some(Arc::new(ConversationReads::new())),
            allow_private_memory: false,
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
}
