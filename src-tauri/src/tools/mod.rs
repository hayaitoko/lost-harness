//! §3.1 Tool registry — `Capability`, `BodyEnv`, the `Tool` trait, and
//! `ToolRegistry`. Spec `docs/tooling-and-skills.md` §3.1 /
//! `docs/PLAN.md` §8 (M3 build order item 1).
//!
//! The core idea: every tool declares the environment capabilities it
//! needs (`requires()`); every body (the Tauri app vs. a headless server)
//! declares what it can actually offer (`BodyEnv`); the registry filters
//! automatically (`available_tools(env)`), so a `Display`-requiring tool
//! is simply absent from a headless environment's tool list instead of
//! failing at call time.
//!
//! Every tool call — native or MCP-provided — is meant to pass through the
//! unified hook chain (`crate::hooks`) before `Tool::run()` executes. Live
//! wiring of real tool calls through that chain lands later in M3 once
//! actual tool implementations exist; this module ships the trait +
//! registry + a couple of trivial example tools so the spine compiles and
//! is exercised by unit tests.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

// ── Capability ───────────────────────────────────────────────────────────

/// A single thing a tool might need from its running environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// Read/write access to the local filesystem.
    Filesystem,
    /// Outbound network access.
    Network,
    /// Ability to spawn a shell / run arbitrary commands.
    Shell,
    /// A screen exists (headless server never has this).
    Display,
    /// A microphone/speaker exists.
    Audio,
    /// Synthesize OS-level clicks/keystrokes (needs `Display`, tracked
    /// separately since a tool might want to gate on it explicitly).
    ComputerUse,
    /// Send/read email on the user's behalf.
    Email,
    /// Read/write the user's calendar.
    Calendar,
    /// Fetch and summarize web content.
    WebResearch,
    /// Allowed to run a job that may take a long time (minutes+) — a
    /// signal a headless server is happy to offer but a foregrounded app
    /// may want to bound.
    LongCompute,
}

// ── BodyEnv ──────────────────────────────────────────────────────────────

/// The set of capabilities the current running body (app vs. headless
/// server) can actually provide. `Tool::available()` checks a tool's
/// `requires()` against this set.
#[derive(Debug, Clone, Default)]
pub struct BodyEnv {
    capabilities: HashSet<Capability>,
}

impl BodyEnv {
    /// Build a `BodyEnv` from an arbitrary capability set.
    pub fn new(capabilities: impl IntoIterator<Item = Capability>) -> Self {
        Self {
            capabilities: capabilities.into_iter().collect(),
        }
    }

    /// Empty environment — no capabilities at all. Useful as a test
    /// baseline and as the honest starting point for a not-yet-configured
    /// body.
    pub fn empty() -> Self {
        Self {
            capabilities: HashSet::new(),
        }
    }

    /// The Tauri desktop app's default capability set: it has a screen, a
    /// filesystem, network, a shell, and (once M5/M6 land) computer-use
    /// and audio.
    pub fn app_default() -> Self {
        Self::new([
            Capability::Filesystem,
            Capability::Network,
            Capability::Shell,
            Capability::Display,
            Capability::Audio,
            Capability::ComputerUse,
            Capability::WebResearch,
        ])
    }

    /// The headless server companion's default capability set: no screen,
    /// no computer-use, no audio — but it never sleeps, so it can own
    /// long-running / always-on work.
    pub fn headless_server_default() -> Self {
        Self::new([
            Capability::Filesystem,
            Capability::Network,
            Capability::Email,
            Capability::Calendar,
            Capability::WebResearch,
            Capability::LongCompute,
        ])
    }

    /// Does this environment provide `cap`?
    pub fn has(&self, cap: Capability) -> bool {
        self.capabilities.contains(&cap)
    }

    /// Does this environment provide every capability in `caps`?
    pub fn has_all(&self, caps: &[Capability]) -> bool {
        caps.iter().all(|c| self.has(*c))
    }
}

// ── Tool I/O types ───────────────────────────────────────────────────────

/// Arguments passed to a tool invocation. Kept as a bare JSON value for
/// now — a typed-per-tool schema is later M3/M4 work.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolInput {
    pub args: serde_json::Value,
}

impl ToolInput {
    pub fn new(args: serde_json::Value) -> Self {
        Self { args }
    }

    pub fn empty() -> Self {
        Self {
            args: serde_json::Value::Null,
        }
    }
}

/// The outcome of running a tool.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolResult {
    Ok(serde_json::Value),
    Err(String),
}

/// Execution context handed to a tool alongside its input. Minimal for
/// now — conversation/profile scoping is enough for the trivial example
/// tools; real tools will likely need a handle back into storage/model
/// manager, added when they're built.
#[derive(Debug, Clone, Default)]
pub struct ExecCtx {
    pub conversation_id: String,
    pub profile: String,
}

// ── Tool trait ───────────────────────────────────────────────────────────

/// Something the agent can invoke. `run()` returns a boxed future so the
/// trait stays object-safe (`Box<dyn Tool>` / `&dyn Tool>`) while still
/// supporting async implementations — the same manual-boxed-future shape
/// `async-trait` generates, without adding the dependency for a milestone
/// that doesn't ship a real async tool yet.
pub trait Tool: Send + Sync {
    /// Stable identifier, e.g. `"read_file"`.
    fn name(&self) -> &str;

    /// The capabilities this tool needs from its running environment.
    fn requires(&self) -> &[Capability];

    /// Is this tool usable in `env`? Default: `requires() ⊆ env`.
    /// Tools with unusual availability logic (e.g. "available if EITHER
    /// of two capabilities is present") can override this.
    fn available(&self, env: &BodyEnv) -> bool {
        env.has_all(self.requires())
    }

    /// Execute the tool. Async-compatible via a boxed future so the trait
    /// remains object-safe.
    fn run<'a>(
        &'a self,
        input: ToolInput,
        ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;
}

// ── ToolRegistry ─────────────────────────────────────────────────────────

/// Holds every registered tool (native + — later — MCP-provided) and
/// filters them by environment capability.
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// Register a tool. Order of registration is preserved and reflected
    /// in `available_tools()`'s output order.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    /// How many tools are registered, regardless of availability.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Look up a registered tool by name, regardless of availability in
    /// any particular environment.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|t| t.as_ref())
    }

    /// The tools usable in `env` — every registered tool whose
    /// `requires()` is satisfied by `env`.
    pub fn available_tools(&self, env: &BodyEnv) -> Vec<&dyn Tool> {
        self.tools
            .iter()
            .filter(|t| t.available(env))
            .map(|t| t.as_ref())
            .collect()
    }
}

// ── Example / test tools ────────────────────────────────────────────────
//
// Trivial reference implementations. Real tools (file read/write/list,
// headless browser, delegate, ask-human, ...) are later M3 build-order
// items (PLAN.md §8, M3 item 10) — these exist purely to exercise the
// trait + registry with something concrete.

/// A tool with no environment requirements at all — always available.
pub struct EchoTool;

impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn requires(&self) -> &[Capability] {
        &[]
    }

    fn run<'a>(
        &'a self,
        input: ToolInput,
        _ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move { ToolResult::Ok(input.args) })
    }
}

/// A tool that needs a screen — used in tests to prove `Display`-gated
/// tools are filtered out of headless environments.
pub struct ScreenshotTool;

impl Tool for ScreenshotTool {
    fn name(&self) -> &str {
        "screenshot"
    }

    fn requires(&self) -> &[Capability] {
        &[Capability::Display]
    }

    fn run<'a>(
        &'a self,
        _input: ToolInput,
        _ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move { ToolResult::Ok(serde_json::json!({"took": "screenshot"})) })
    }
}

/// A tool that needs both filesystem and network — used to prove
/// multi-capability `requires()` is enforced as a set intersection, not
/// an "any of" check.
pub struct SyncFileTool;

impl Tool for SyncFileTool {
    fn name(&self) -> &str {
        "sync_file"
    }

    fn requires(&self) -> &[Capability] {
        &[Capability::Filesystem, Capability::Network]
    }

    fn run<'a>(
        &'a self,
        _input: ToolInput,
        _ctx: &'a ExecCtx,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move { ToolResult::Ok(serde_json::json!({"synced": true})) })
    }
}

#[cfg(test)]
mod tests;
