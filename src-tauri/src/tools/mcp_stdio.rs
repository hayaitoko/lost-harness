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

use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};

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

/// One live server: its transport + the tool names it registered.
pub struct McpRuntimeEntry {
    pub transport: std::sync::Arc<StdioMcpTransport>,
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

/// Bring one persisted server UP: spawn + handshake → `tools/list` → wrap every
/// descriptor through the UNTOUCHED trust spine (`McpTool::new` namespaces,
/// sanitizes, derives risk) → hot-register into the live dispatcher → record in
/// the runtime. Fail-closed: any error tears the child down and registers
/// nothing. Returns the registered (namespaced) tool names.
pub async fn bring_up_server(
    row: &crate::storage::McpServerRow,
    dispatcher: &crate::tools::ToolDispatcher,
    runtime: &McpRuntime,
) -> Result<Vec<String>, String> {
    use super::mcp::{McpServerConfig, McpTool, McpTrustTier};
    use crate::tools::Tool as _; // for McpTool::name()
    let transport = std::sync::Arc::new(StdioMcpTransport::spawn(&row.command, &row.args).await?);
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
        let tool = McpTool::new(&cfg, d, transport.clone() as std::sync::Arc<dyn super::mcp::McpTransport>);
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
}
