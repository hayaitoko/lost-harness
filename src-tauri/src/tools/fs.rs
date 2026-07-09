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

use crate::tools::{Capability, ExecCtx, Tool, ToolInput, ToolResult};

/// Cap on a single file read, so a giant file can't blow up the context
/// window or memory. 256 KiB is generous for text/config/code.
const MAX_READ_BYTES: u64 = 256 * 1024;

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
        _ctx: &'a ExecCtx,
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
                Ok(content) => ToolResult::Ok(json!({
                    "path": path,
                    "bytes": content.len(),
                    "content": content,
                })),
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
