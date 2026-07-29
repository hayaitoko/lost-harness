//! C3 — the REAL MCP wire transport: a **stdio JSON-RPC client** (the first
//! concrete [`McpTransport`]; SSE/HTTP can follow later). Spawns the server as
//! a child process, speaks newline-delimited JSON-RPC 2.0 over its stdin/stdout
//! (the MCP stdio framing), and performs the `initialize` → `initialized`
//! lifecycle before any tool call. Replaces [`super::mcp::UnwiredTransport`] as
//! the thing production hands to [`super::mcp::McpTool`] — the existing trust
//! spine (namespacing, sanitization, risk derivation, guard-wrap) is UNTOUCHED:
//! this module is transport only.
//!
//! Concurrency: one child = one serialized request/response channel. A single
//! `tokio::sync::Mutex` guards the whole round-trip (write request → read until
//! the matching `id`), which is the simplest correct shape; per-call demux is a
//! later optimization no current caller needs. Every round-trip is bounded by
//! [`RPC_TIMEOUT`] so a hung server can't wedge a tool call forever.
//!
//! Fail-closed: a spawn/handshake error never yields a half-initialized
//! transport; a JSON-RPC `error`, an MCP `isError` result, a dead child, or a
//! timeout all surface as `Err` — flowing through `McpTool::run`'s existing
//! `ToolResult::Err` arm with no new error path.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

use super::mcp::{McpToolAnnotations, McpToolDescriptor, McpTransport};

/// Wall-clock bound per JSON-RPC round-trip (a hung server never wedges a call).
const RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// The MCP protocol revision we offer at `initialize`.
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
/// Cap on a single response line — a malicious/broken server can't OOM us by
/// streaming an endless line.
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

// ── H-07: executable pinning ────────────────────────────────────────────────
//
// A registered stdio MCP server is third-party code we spawn with the app's own
// privileges. Registration is the moment the user consents to *that specific
// binary*; nothing afterwards re-asks. So registration records the canonical
// absolute path AND the SHA-256 of the file, and every later bring-up
// (including the silent auto-start at boot) re-resolves + re-hashes and refuses
// to spawn on any mismatch. A swapped `~/.local/bin/foo` therefore cannot ride
// the old consent.
//
// NOT covered here, and deliberately so: the child still runs with the app's
// full privileges once it does start. See `review-fixes/progress/P08.md`.

/// Is `p` a file we could actually exec? Mirrors what the OS will do when
/// `StdioMcpTransport::spawn` hands `command` to `execvp`.
#[cfg(unix)]
fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(p: &Path) -> bool {
    p.is_file()
}

/// Resolve `command` to an absolute, symlink-free path the same way the spawned
/// child would. A command containing a separator is a path; a bare name is
/// looked up along `PATH` — the same `PATH` the child inherits (see the
/// allowlist in [`StdioMcpTransport::spawn`]), so resolution and execution
/// agree.
pub fn resolve_executable(command: &str) -> Result<PathBuf, String> {
    let candidate = if command.contains('/') || command.contains('\\') {
        PathBuf::from(command)
    } else {
        let path_var = std::env::var_os("PATH")
            .ok_or_else(|| "PATH is unset — cannot resolve an MCP server command".to_string())?;
        std::env::split_paths(&path_var)
            .map(|dir| dir.join(command))
            .find(|p| is_executable_file(p))
            .ok_or_else(|| format!("MCP server command `{command}` not found on PATH"))?
    };
    std::fs::canonicalize(&candidate).map_err(|e| {
        format!(
            "cannot resolve MCP server command `{}`: {e}",
            candidate.display()
        )
    })
}

/// Hex-encoded SHA-256 of the file at `path`, read in chunks (a server binary
/// can be hundreds of MB — never slurp it whole).
pub fn sha256_of_file(path: &Path) -> Result<String, String> {
    use std::io::Read as _;
    let mut f =
        std::fs::File::open(path).map_err(|e| format!("cannot read `{}`: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("cannot read `{}`: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Registration-time pin: the canonical path + its SHA-256, as a pair to store
/// on the `mcp_servers` row.
pub fn resolve_and_hash_executable(command: &str) -> Result<(String, String), String> {
    let path = resolve_executable(command)?;
    let hash = sha256_of_file(&path)?;
    Ok((path.to_string_lossy().to_string(), hash))
}

/// Bring-up-time check. Fails CLOSED in all three bad cases:
/// * the command now resolves somewhere else,
/// * the file at that path hashes differently than at registration,
/// * the row has no pin at all (registered before migration v9) — we cannot
///   attest to a binary we never measured, so the user must re-register.
pub fn verify_pinned_executable(
    command: &str,
    pinned_path: Option<&str>,
    pinned_hash: Option<&str>,
) -> Result<(), String> {
    let (Some(expected_path), Some(expected_hash)) = (pinned_path, pinned_hash) else {
        return Err(format!(
            "MCP server `{command}` has no pinned executable hash — it was registered before \
             executable pinning existed. Remove and re-register it to approve its binary."
        ));
    };
    let actual_path = resolve_executable(command)?;
    let actual_path = actual_path.to_string_lossy().to_string();
    if actual_path != expected_path {
        return Err(format!(
            "refusing to start MCP server `{command}`: its executable moved (approved \
             `{expected_path}`, now resolves to `{actual_path}`). Remove and re-register it."
        ));
    }
    let actual_hash = sha256_of_file(Path::new(&actual_path))?;
    if actual_hash != expected_hash {
        return Err(format!(
            "refusing to start MCP server `{command}`: the binary at `{actual_path}` changed \
             since it was approved (sha256 {expected_hash} → {actual_hash}). Remove and \
             re-register it."
        ));
    }
    Ok(())
}

struct Io {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// The stdio transport for ONE spawned MCP server.
pub struct StdioMcpTransport {
    /// Held so the child stays alive as long as the transport does;
    /// `kill_on_drop` reaps it when the last Arc drops.
    child: tokio::sync::Mutex<Child>,
    io: tokio::sync::Mutex<Io>,
    next_id: AtomicI64,
}

impl StdioMcpTransport {
    /// Spawn `command args…` and run the MCP `initialize` handshake. Any
    /// failure kills the child and returns `Err` — never a half-initialized
    /// transport.
    pub async fn spawn(command: &str, args: &[String]) -> Result<Self, String> {
        // Treat a registered MCP server like third-party software: never hand
        // it the desktop app's full environment (provider keys, tracing
        // credentials, CI tokens, etc.). Re-introduce only the small process
        // environment needed to find executables and behave normally.
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args).env_clear();
        for key in ["PATH", "HOME", "TMPDIR", "USER", "LANG"] {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("couldn't spawn MCP server `{command}`: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin pipe on the MCP child")?;
        let stdout = child.stdout.take().ok_or("no stdout pipe on the MCP child")?;
        let transport = Self {
            child: tokio::sync::Mutex::new(child),
            io: tokio::sync::Mutex::new(Io { stdin, stdout: BufReader::new(stdout) }),
            next_id: AtomicI64::new(0),
        };

        // MCP lifecycle: initialize (request/response) → initialized (notification).
        let init = transport
            .rpc(
                "initialize",
                serde_json::json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "lost-harness", "version": env!("CARGO_PKG_VERSION")},
                }),
            )
            .await;
        if let Err(e) = init {
            transport.shutdown().await;
            return Err(format!("MCP initialize handshake failed: {e}"));
        }
        if let Err(e) = transport
            .notify("notifications/initialized", serde_json::json!({}))
            .await
        {
            transport.shutdown().await;
            return Err(format!("MCP initialized notification failed: {e}"));
        }
        Ok(transport)
    }

    /// One serialized JSON-RPC request/response round-trip, bounded by
    /// [`RPC_TIMEOUT`]. Skips non-matching lines (notifications, other ids —
    /// shouldn't occur under the serialize-everything lock, but tolerated).
    async fn rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let fut = async {
            let mut io = self.io.lock().await;
            let line = format!("{request}\n");
            io.stdin
                .write_all(line.as_bytes())
                .await
                .map_err(|e| format!("MCP stdin write failed: {e}"))?;
            io.stdin.flush().await.map_err(|e| format!("MCP stdin flush failed: {e}"))?;
            loop {
                let mut buf = String::new();
                // A bounded reader wrapper so a runaway line can't OOM us
                // (UFCS disambiguates tokio's AsyncReadExt::take from
                // Iterator::take; tokio's `Take<AsyncBufRead>` stays BufRead).
                let mut limited = AsyncReadExt::take(&mut io.stdout, MAX_LINE_BYTES as u64);
                let n = limited
                    .read_line(&mut buf)
                    .await
                    .map_err(|e| format!("MCP stdout read failed: {e}"))?;
                if n == 0 {
                    return Err("MCP server closed its stdout (exited?)".to_string());
                }
                let Ok(msg) = serde_json::from_str::<serde_json::Value>(&buf) else {
                    continue; // non-JSON noise — skip
                };
                if msg.get("id").and_then(|v| v.as_i64()) != Some(id) {
                    continue; // a notification / another id — skip
                }
                if let Some(err) = msg.get("error") {
                    return Err(format!("MCP server error: {err}"));
                }
                return Ok(msg.get("result").cloned().unwrap_or(serde_json::Value::Null));
            }
        };
        tokio::time::timeout(RPC_TIMEOUT, fut)
            .await
            .map_err(|_| format!("MCP call `{method}` timed out after {RPC_TIMEOUT:?}"))?
    }

    /// Fire a JSON-RPC notification (no id, no response). Timeout-bounded like
    /// every other wire interaction (review nit — a full pipe buffer on a
    /// stdin-ignoring child must not hang the handshake).
    async fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), String> {
        let note = serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params});
        let fut = async {
            let mut io = self.io.lock().await;
            let line = format!("{note}\n");
            io.stdin
                .write_all(line.as_bytes())
                .await
                .map_err(|e| format!("MCP stdin write failed: {e}"))?;
            io.stdin.flush().await.map_err(|e| format!("MCP stdin flush failed: {e}"))
        };
        tokio::time::timeout(RPC_TIMEOUT, fut)
            .await
            .map_err(|_| format!("MCP notification `{method}` timed out"))?
    }

    /// `tools/list` → descriptors for the registration path. Inherent (not on
    /// the [`McpTransport`] trait): discovery is a registration-time concern,
    /// not a per-call one.
    pub async fn list_tools(&self) -> Result<Vec<McpToolDescriptor>, String> {
        let result = self.rpc("tools/list", serde_json::json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .ok_or("MCP tools/list result carries no `tools` array")?;
        Ok(tools
            .iter()
            .filter_map(|t| {
                let name = t.get("name")?.as_str()?.to_string();
                let ann = t.get("annotations");
                Some(McpToolDescriptor {
                    name,
                    description: t
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .to_string(),
                    annotations: McpToolAnnotations {
                        read_only_hint: ann
                            .and_then(|a| a.get("readOnlyHint"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                        destructive_hint: ann
                            .and_then(|a| a.get("destructiveHint"))
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                    },
                    input_schema: t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or(serde_json::json!({"type": "object"})),
                })
            })
            .collect())
    }

    /// Kill the child promptly (the remove-server path; drop-order via Arcs
    /// would eventually reap it, but explicit is better for UX).
    pub async fn shutdown(&self) {
        let mut child = self.child.lock().await;
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

impl McpTransport for StdioMcpTransport {
    fn call_tool<'a>(
        &'a self,
        tool_name: &'a str,
        args: serde_json::Value,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            let result = self
                .rpc(
                    "tools/call",
                    serde_json::json!({"name": tool_name, "arguments": args}),
                )
                .await?;
            // MCP's server-side tool-error signal — surfaced as Err so it flows
            // through the SAME ToolResult::Err arm as any transport failure.
            if result.get("isError").and_then(|v| v.as_bool()).unwrap_or(false) {
                return Err(format!("MCP tool `{tool_name}` reported an error: {result}"));
            }
            // Return the raw result envelope unmodified (content shaping is a
            // later UX pass — unwrapping content[0].text here would be a lossy
            // assumption some servers don't match).
            Ok(result)
        })
    }
}

// ── the runtime registry + server lifecycle (registration/list/remove) ──────

/// One live server's concrete transport. Stdio owns a child to tear down;
/// Streamable HTTP is connection/session state only and needs no process kill.
pub enum McpRuntimeTransport {
    Stdio(std::sync::Arc<StdioMcpTransport>),
    Http(std::sync::Arc<super::mcp_http::HttpMcpTransport>),
}

impl McpRuntimeTransport {
    fn as_transport(&self) -> std::sync::Arc<dyn super::mcp::McpTransport> {
        match self {
            Self::Stdio(transport) => transport.clone(),
            Self::Http(transport) => transport.clone(),
        }
    }

    async fn list_tools(&self) -> Result<Vec<McpToolDescriptor>, String> {
        match self {
            Self::Stdio(transport) => transport.list_tools().await,
            Self::Http(transport) => transport.list_tools().await,
        }
    }

    pub async fn shutdown(self) {
        if let Self::Stdio(transport) = self {
            transport.shutdown().await;
        }
    }
}

/// One live server: its transport + the tool names it registered.
pub struct McpRuntimeEntry {
    pub transport: McpRuntimeTransport,
    pub tool_names: Vec<String>,
}

/// The in-process registry of LIVE MCP servers, keyed by the persisted row id.
/// The persisted config (`storage::McpServerRow`) is the durable truth; this is
/// derived session state (spawned children die with the app).
#[derive(Default)]
pub struct McpRuntime {
    pub servers: parking_lot::Mutex<std::collections::HashMap<String, McpRuntimeEntry>>,
}

impl McpRuntime {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Bring one persisted server UP: either spawn stdio or connect Streamable HTTP,
/// then handshake → `tools/list` → wrap every descriptor through the untouched
/// trust spine. A URL beginning with `https://` (or loopback `http://`) is a
/// remote Streamable HTTP endpoint; all other commands are local stdio spawns.
/// Fail-closed: a connection/handshake/list failure registers and persists
/// nothing. Returns the registered (namespaced) tool names.
pub async fn bring_up_server(
    row: &crate::storage::McpServerRow,
    dispatcher: &crate::tools::ToolDispatcher,
    runtime: &McpRuntime,
) -> Result<Vec<String>, String> {
    use super::mcp::{McpServerConfig, McpTool, McpTrustTier};
    use crate::tools::Tool as _; // for McpTool::name()
    let is_http = row.command.trim_start().starts_with("https://")
        || row.command.trim_start().starts_with("http://");
    let transport = if is_http {
        McpRuntimeTransport::Http(std::sync::Arc::new(
            super::mcp_http::HttpMcpTransport::connect(&row.command).await?,
        ))
    } else {
        // H-07: the pinned binary is the consent. Re-check it on EVERY bring-up
        // — including the unattended auto-start at boot — before we spawn.
        verify_pinned_executable(
            &row.command,
            row.executable_path.as_deref(),
            row.executable_hash.as_deref(),
        )?;
        McpRuntimeTransport::Stdio(std::sync::Arc::new(
            StdioMcpTransport::spawn(&row.command, &row.args).await?,
        ))
    };
    let descriptors = match transport.list_tools().await {
        Ok(d) => d,
        Err(e) => {
            transport.shutdown().await;
            return Err(format!("MCP tools/list failed: {e}"));
        }
    };
    let cfg = McpServerConfig {
        server_name: row.name.clone(),
        // Unknown tier strings fail CLOSED to Remote (the stricter tier).
        tier: if row.tier == "local" { McpTrustTier::Local } else { McpTrustTier::Remote },
        trusted_read_only: row.trusted_read_only,
        capabilities: row
            .capabilities
            .iter()
            .filter_map(|s| crate::tools::Capability::from_capability_str(s))
            .collect(),
    };
    let mut names = Vec::new();
    for d in &descriptors {
        let tool = McpTool::new(&cfg, d, transport.as_transport());
        let name = tool.name().to_string();
        // hot_register refuses shadowing — a foreign server can never displace
        // a native tool or another server's tool. Refusals are logged loudly
        // (registration-time namespace uniqueness makes them near-impossible,
        // but a silent narrowing must never look like success).
        if dispatcher.hot_register(Box::new(tool)) {
            names.push(name);
        } else {
            tracing::warn!(tool = %name, "MCP tool name refused (would shadow an existing tool) — skipped");
        }
    }
    runtime.servers.lock().insert(
        row.id.clone(),
        McpRuntimeEntry { transport, tool_names: names.clone() },
    );
    Ok(names)
}

/// Tear one server DOWN: unregister its tools, kill its child, drop the entry.
pub async fn tear_down_server(
    id: &str,
    dispatcher: &crate::tools::ToolDispatcher,
    runtime: &McpRuntime,
) -> bool {
    let entry = runtime.servers.lock().remove(id);
    match entry {
        Some(entry) => {
            for name in &entry.tool_names {
                dispatcher.hot_unregister(name);
            }
            entry.transport.shutdown().await;
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic local stdio fixture server (plain `sh`): answers our
    /// exact call sequence — initialize (id 0), swallow the initialized
    /// notification, tools/list (id 1), then tools/call replies (ids 2, 3).
    fn fixture_script() -> String {
        let dir = std::env::temp_dir().join(format!("lhp-mcp-fixture-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.sh");
        std::fs::write(
            &path,
            concat!(
                "read line\n",
                "printf '{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"serverInfo\":{\"name\":\"fixture\",\"version\":\"0\"}}}\\n'\n",
                "read line\n", // the initialized notification — no reply
                "read line\n",
                "printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[{\"name\":\"echo_upper\",\"description\":\"upper-cases text\",\"annotations\":{\"readOnlyHint\":true},\"inputSchema\":{\"type\":\"object\"}}]}}\\n'\n",
                "read line\n",
                "printf '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"HELLO\"}]}}\\n'\n",
                "read line\n",
                "printf '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"isError\":true,\"content\":[{\"type\":\"text\",\"text\":\"boom\"}]}}\\n'\n",
            ),
        )
        .unwrap();
        path.to_string_lossy().to_string()
    }

    fn environment_fixture_script() -> String {
        let dir = std::env::temp_dir().join(format!("lhp-mcp-env-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.sh");
        std::fs::write(
            &path,
            concat!(
                "read line\n",
                "printf '{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{}}\\n'\n",
                "read line\n",
                "read line\n",
                "printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[{\"name\":\"env_probe\",\"description\":\"%s\",\"inputSchema\":{\"type\":\"object\"}}]}}\\n' \"${LHP_MCP_SECRET_SHOULD_NOT_LEAK-unset}\"\n",
            ),
        )
        .unwrap();
        path.to_string_lossy().to_string()
    }

    #[tokio::test]
    async fn live_fixture_handshake_list_call_and_error_paths() {
        let script = fixture_script();
        let t = StdioMcpTransport::spawn("sh", &[script])
            .await
            .expect("fixture handshake succeeds");

        // tools/list → the descriptor, annotations parsed.
        let tools = t.list_tools().await.expect("tools/list");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo_upper");
        assert!(tools[0].annotations.read_only_hint);

        // tools/call → the raw result envelope.
        let out = t
            .call_tool("echo_upper", serde_json::json!({"text": "hello"}))
            .await
            .expect("tools/call");
        assert_eq!(out["content"][0]["text"], "HELLO");

        // isError → Err through the SAME arm as a transport failure.
        let err = t
            .call_tool("echo_upper", serde_json::json!({}))
            .await
            .expect_err("isError surfaces as Err");
        assert!(err.contains("reported an error"), "got: {err}");

        t.shutdown().await;
    }

    #[tokio::test]
    async fn child_environment_is_allowlisted_but_path_still_works() {
        let marker = "LHP_MCP_SECRET_SHOULD_NOT_LEAK";
        std::env::set_var(marker, "super-secret");
        let script = environment_fixture_script();
        let t = StdioMcpTransport::spawn("sh", &[script])
            .await
            .expect("allowlisted PATH must still resolve the shell fixture");
        let tools = t.list_tools().await.expect("environment fixture lists tools");
        assert_eq!(tools[0].description, "unset", "parent secret leaked to MCP child");
        t.shutdown().await;
        std::env::remove_var(marker);
    }

    #[tokio::test]
    async fn live_fixture_bring_up_dispatch_through_the_spine_and_tear_down() {
        use crate::agent::gate::Binding;
        use crate::hooks::HookChain;
        use crate::tools::dispatch::ToolOutcome;
        use crate::tools::{BodyEnv, Capability, ExecCtx, ToolCall, ToolDispatcher, ToolRegistry};

        let script = fixture_script();
        // H-07: bring-up now demands a matching pin. Measure the shell the same
        // way registration would (never a hardcoded digest — it differs per host).
        let (sh_path, sh_hash) =
            resolve_and_hash_executable("sh").expect("the shell must resolve on PATH");
        let row = crate::storage::McpServerRow {
            id: "srv1".into(),
            name: "fixture".into(),
            command: "sh".into(),
            args: vec![script],
            tier: "remote".into(),
            trusted_read_only: false,
            capabilities: vec![],
            enabled: true,
            created_at: 1,
            executable_path: Some(sh_path),
            executable_hash: Some(sh_hash),
        };
        // Remote tier forces the Network capability — the env must grant it.
        let dispatcher = ToolDispatcher::new(
            ToolRegistry::new(),
            HookChain::new(),
            BodyEnv::new([Capability::Network]),
        );
        let runtime = McpRuntime::new();

        // Bring-up: spawn + handshake + tools/list + hot-register through the
        // untouched trust spine (namespaced name proves it went through).
        let names = bring_up_server(&row, &dispatcher, &runtime).await.expect("bring-up");
        assert_eq!(names, vec!["mcp__fixture__echo_upper".to_string()]);

        // A REAL dispatch through the dispatcher reaches the child and returns
        // the raw MCP result envelope.
        let out = dispatcher
            .dispatch(
                &ToolCall { name: names[0].clone(), args: serde_json::json!({"text": "hello"}) },
                &ExecCtx::default(),
                Binding::Public,
                true,
            )
            .await;
        match out {
            ToolOutcome::Ok(v) => assert_eq!(v["content"][0]["text"], "HELLO"),
            other => panic!("expected the MCP result, got {other:?}"),
        }

        // Tear-down: tools unregistered, child killed.
        assert!(tear_down_server("srv1", &dispatcher, &runtime).await);
        let gone = dispatcher
            .dispatch(
                &ToolCall { name: names[0].clone(), args: serde_json::json!({}) },
                &ExecCtx::default(),
                Binding::Public,
                true,
            )
            .await;
        assert!(matches!(gone, ToolOutcome::Unknown(_)), "a removed server's tools are gone");
    }

    #[tokio::test]
    async fn a_dead_server_fails_closed_at_spawn_or_call() {
        // A command that exits immediately: the handshake read hits EOF → Err,
        // never a half-initialized transport.
        let r = StdioMcpTransport::spawn("true", &[]).await;
        assert!(r.is_err(), "an exiting child can never hand back a transport");
        // A nonexistent binary fails at spawn.
        let r2 = StdioMcpTransport::spawn("/nonexistent/lhp-mcp-server", &[]).await;
        assert!(r2.is_err());
    }

    // ── H-07: executable pinning ────────────────────────────────────────────

    /// A standalone, executable copy of the working MCP fixture, so a test can
    /// mutate the very file that would be spawned. Returns its path.
    fn executable_fixture_server() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lhp-mcp-pin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server");
        let body = std::fs::read_to_string(fixture_script()).unwrap();
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn pinned_row(id: &str, command: &str) -> crate::storage::McpServerRow {
        let (path, hash) = resolve_and_hash_executable(command).expect("fixture resolves");
        crate::storage::McpServerRow {
            id: id.into(),
            name: "pinned".into(),
            command: command.into(),
            args: vec![],
            tier: "remote".into(),
            trusted_read_only: false,
            capabilities: vec![],
            enabled: true,
            created_at: 1,
            executable_path: Some(path),
            executable_hash: Some(hash),
        }
    }

    /// H-07 gap (a): once the approved binary changes on disk, the unattended
    /// auto-start path (`bring_up_server`, exactly what lib.rs runs at boot)
    /// must REFUSE — not spawn the new binary under the old consent.
    #[tokio::test]
    async fn a_changed_binary_blocks_auto_start() {
        use crate::hooks::HookChain;
        use crate::tools::{BodyEnv, Capability, ToolDispatcher, ToolRegistry};

        let server = executable_fixture_server();
        let command = server.to_string_lossy().to_string();
        let row = pinned_row("pin1", &command);

        let dispatcher = ToolDispatcher::new(
            ToolRegistry::new(),
            HookChain::new(),
            BodyEnv::new([Capability::Network]),
        );
        let runtime = McpRuntime::new();

        // Control: the untouched binary matches its pin and comes up fine.
        let names = bring_up_server(&row, &dispatcher, &runtime)
            .await
            .expect("an unmodified, pinned binary must still start");
        assert_eq!(names, vec!["mcp__pinned__echo_upper".to_string()]);
        assert!(tear_down_server("pin1", &dispatcher, &runtime).await);

        // Now swap the binary's contents in place — same path, same mode, same
        // row. This is the attack: consent was given for a different file.
        let original = std::fs::read_to_string(&server).unwrap();
        std::fs::write(&server, format!("{original}# tampered\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Sanity: the file really does hash differently now.
        assert_ne!(
            sha256_of_file(&server).unwrap(),
            row.executable_hash.clone().unwrap(),
            "the test must actually have changed the binary"
        );

        let err = bring_up_server(&row, &dispatcher, &runtime)
            .await
            .expect_err("a changed binary must NOT auto-start");
        assert!(
            err.contains("changed since it was approved"),
            "expected a hash-mismatch refusal, got: {err}"
        );
        // And nothing was registered as a side effect of the refusal.
        assert!(
            runtime.servers.lock().get("pin1").is_none(),
            "a refused bring-up must leave no live server behind"
        );

        let _ = std::fs::remove_dir_all(server.parent().unwrap());
    }

    /// A row with no pin at all (written before migration v9) is NOT trusted:
    /// we never measured that binary, so bring-up fails closed.
    #[test]
    fn an_unpinned_row_fails_closed() {
        let err = verify_pinned_executable("sh", None, None)
            .expect_err("an unmeasured binary must not be trusted");
        assert!(err.contains("no pinned executable hash"), "got: {err}");

        let (path, _) = resolve_and_hash_executable("sh").unwrap();
        let err2 = verify_pinned_executable("sh", Some(&path), None)
            .expect_err("a path without a hash is still unmeasured");
        assert!(err2.contains("no pinned executable hash"), "got: {err2}");
    }

    /// The same command resolving to a DIFFERENT file is a refusal too, even if
    /// that other file happens to be a legitimate executable.
    #[test]
    fn a_moved_executable_fails_closed() {
        let (sh_path, sh_hash) = resolve_and_hash_executable("sh").unwrap();
        let err = verify_pinned_executable("sh", Some("/nonexistent/approved-sh"), Some(&sh_hash))
            .expect_err("a relocated executable must not start");
        assert!(err.contains("its executable moved"), "got: {err}");
        // Control: the true pin passes.
        verify_pinned_executable("sh", Some(&sh_path), Some(&sh_hash))
            .expect("the recorded pin must still verify");
    }

    /// The pin must survive symlink games: `resolve_executable` canonicalizes,
    /// so a symlink pointing at the approved file resolves to the same target.
    #[test]
    fn hashing_is_content_sensitive() {
        let dir = std::env::temp_dir().join(format!("lhp-mcp-hash-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a");
        std::fs::write(&a, b"hello").unwrap();
        let h1 = sha256_of_file(&a).unwrap();
        assert_eq!(
            h1, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            "sha256(\"hello\") must be the standard digest"
        );
        std::fs::write(&a, b"hello!").unwrap();
        assert_ne!(h1, sha256_of_file(&a).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
