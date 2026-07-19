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

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

pub mod ask_human;
pub mod calling;
pub mod computer_use;
pub mod cron;
pub mod delegate;
pub mod dispatch;
pub mod exec;
pub mod fetch;
pub mod fs;
pub mod mcp;
pub mod memory;
pub mod session_search;
pub mod skills;
pub mod system_status;

pub use calling::ToolCall;
pub use dispatch::{ToolDispatcher, TurnOutcome};
pub use mcp::{
    McpServerConfig, McpTool, McpToolAnnotations, McpToolDescriptor, McpTransport, McpTrustTier,
};

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

/// Per-conversation record of which files the agent has `read_file`'d this
/// session, keyed by conversation id. Drives the **read-before-write guard**:
/// `write_file` (on an existing file) and `edit_file` refuse to touch a path
/// that isn't in this set, so the agent can't blind-clobber a file it never
/// looked at — the same rule Claude Code enforces.
///
/// Keys are **canonical** paths (post-`canonicalize`), so a read and a later
/// write agree on identity regardless of how the path was spelled. Interior
/// mutability (a `Mutex`) lets the dispatcher share one handle across turns
/// through `&ExecCtx` without threading `&mut` everywhere.
#[derive(Debug, Default)]
pub struct ConversationReads {
    inner: Mutex<HashMap<String, HashSet<PathBuf>>>,
}

impl ConversationReads {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `path` (already canonicalized) was read in `conversation`.
    pub fn record(&self, conversation: &str, path: PathBuf) {
        let mut guard = self.inner.lock().expect("ConversationReads mutex poisoned");
        guard.entry(conversation.to_string()).or_default().insert(path);
    }

    /// Has `path` been read in `conversation` this session?
    pub fn contains(&self, conversation: &str, path: &Path) -> bool {
        let guard = self.inner.lock().expect("ConversationReads mutex poisoned");
        guard.get(conversation).is_some_and(|set| set.contains(path))
    }
}

/// Execution context handed to a tool alongside its input. Carries the
/// conversation/profile scope and — when wired by the dispatcher — the shared
/// read-tracking handle behind the read-before-write guard. `reads` is `None`
/// in isolated tool tests (guard inert); the dispatcher injects the shared
/// handle in production. Real tools will likely also need a handle back into
/// storage/model manager, added when they're built.
#[derive(Debug, Clone, Default)]
pub struct ExecCtx {
    pub conversation_id: String,
    pub profile: String,
    pub reads: Option<Arc<ConversationReads>>,
    /// Whether this turn's endpoint may read private-local memory (PLAN §9).
    /// `true` only on a local/private endpoint; the dispatcher sets it to
    /// `!is_cloud` at the `tool.run` boundary. **Safe default is `false`** — an
    /// unset/`Default` context (tests, unknown endpoint) never surfaces a
    /// private-local fact into model context. `recall_memory` reads this.
    pub allow_private_memory: bool,
    /// The conversation's permission mode (Q11). The dispatcher stamps this into
    /// each tool call's `EventContext` so the `SessionModeHook` can apply it.
    /// Defaults to `Normal` (no-op) for any context that doesn't set one.
    pub session_mode: crate::hooks::SessionMode,
    /// Wave 4.3c: the id of the provider serving THIS turn (the caller's own
    /// model), stamped by `AgentLoop::stream_to_provider` from the turn's
    /// resolved `provider`. `delegate` reads this as the `resolve_seat`
    /// inherit-fallback target when a persona's seat is unbound. Empty string
    /// default (via `ExecCtx::default()`) — a context that never stamps this
    /// (every existing test site) simply makes `resolve_seat`'s inherit
    /// fallback resolve to an empty pair, which `delegate` treats as "no
    /// caller model to inherit" and reports as an error rather than silently
    /// dispatching against nothing.
    pub caller_provider_id: String,
    /// Wave 4.3c: sibling of `caller_provider_id` — the model id serving this
    /// turn. See that field's doc for the inherit-fallback contract.
    pub caller_model: String,
    /// Wave 4.3c: this turn's privacy binding (Auto/Public/Private), stamped by
    /// the dispatcher. `delegate` reads it so a delegated helper INHERITS the
    /// parent turn's binding and can never run weaker — a Private conversation's
    /// helper stays local, never silently downgraded to Auto/cloud. Defaults to
    /// `Auto` for any context that doesn't stamp it.
    pub binding: crate::agent::gate::Binding,
}

// ── RiskClass ──────────────────────────────────────────────────────────────

/// How much a tool call can do — one deterministic property that drives its
/// default gating (`lib::build_tool_dispatcher` pre-trusts `Safe` tools and
/// routes everything else through the approval spine) and, later, UI badges
/// and memory scope (PLAN §3, the risk taxonomy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskClass {
    /// Read-only, no side effects (read_file, list_dir, search_files).
    Safe,
    /// Mutates local state the user owns (write/edit/delete files).
    Write,
    /// Reaches beyond this machine (network egress, email). Reserved.
    External,
    /// Irreversible or high-blast-radius. Reserved.
    Dangerous,
}

impl RiskClass {
    /// Lowercase stable discriminant for the frontend (the approval dialog's
    /// risk badge + matrix-driven button layout key off this). Distinct from
    /// the `Debug`/capitalized form the audit column stores.
    pub fn as_str(self) -> &'static str {
        match self {
            RiskClass::Safe => "safe",
            RiskClass::Write => "write",
            RiskClass::External => "external",
            RiskClass::Dangerous => "dangerous",
        }
    }
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

    /// One-line human-readable description, shown to the model in the tool
    /// catalog (`calling::render_tool_catalog`). Default empty — the catalog
    /// then lists the tool by name only. Real tools should override with a
    /// short "what it does + args" line.
    fn description(&self) -> &str {
        ""
    }

    /// How risky this tool is. Drives its default gating: `Safe` (read-only)
    /// tools are pre-trusted; anything state-changing routes through the
    /// approval spine. Defaults to `Safe`, so a state-changing tool MUST
    /// override it (forgetting to would only ever make a tool *more* trusted
    /// than intended — so the default is the safe direction only for reads;
    /// every mutating tool sets this explicitly).
    fn risk(&self) -> RiskClass {
        RiskClass::Safe
    }

    /// JSON Schema for this tool's `args` (Q1). Native tool-use endpoints
    /// consume it verbatim as the function's `parameters`; the fenced-dialect
    /// catalog can render it as arg docs. Default = permissive object, so a
    /// tool without a schema still works on both transports. Validation
    /// remains a dispatch-boundary concern — `ToolInput.args` stays bare JSON.
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "additionalProperties": true })
    }

    /// For `External` (egress) tools, the human-readable destination this call
    /// reaches — a URL host, an email recipient — surfaced in the approval
    /// dialog as the consent to grant (`ApprovalRequest.destination`). Default
    /// `None` (non-egress tools). Server-derived from the call, never client
    /// input. The dispatcher calls this only to populate the prompt; it is not
    /// itself a gate.
    fn destination(&self, _args: &serde_json::Value) -> Option<String> {
        None
    }

    /// The capabilities this tool needs from its running environment.
    fn requires(&self) -> &[Capability];

    /// Is this tool usable in `env`? Default: `requires() ⊆ env`.
    /// Tools with unusual availability logic (e.g. "available if EITHER
    /// of two capabilities is present") can override this.
    fn available(&self, env: &BodyEnv) -> bool {
        env.has_all(self.requires())
    }

    /// Text used for pattern/denylist matching (the `SandboxHook` floor,
    /// `PermissionHook` rules) — NOT necessarily what's shown to the user for
    /// approval. Defaults to the canonical `"{name} {args}"` form. Override
    /// when a tool's args wrap something that should be matched in decoded
    /// form rather than its JSON envelope (e.g. `shell_exec`'s `command`
    /// string) — quotes/escaping inside JSON create needless mismatch surface
    /// for a substring-based denylist.
    fn match_text(&self, args: &serde_json::Value) -> String {
        format!("{} {}", self.name(), args)
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
/// filters them by environment capability. Tools are held behind `Arc` so a
/// bounded sub-registry (Wave 4.3 agent toolbelts) can SHARE the same tool
/// instances via [`ToolRegistry::restricted_to`] without re-constructing them.
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<std::sync::Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// Register a tool. Order of registration is preserved and reflected
    /// in `available_tools()`'s output order. Takes a `Box` (callers keep using
    /// `register(Box::new(MyTool))`); it's converted to `Arc` internally.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(std::sync::Arc::from(tool));
    }

    /// A bounded sub-registry: exactly the registered tools whose name is in
    /// `allowed`, sharing the same `Arc`'d tool instances (no rebuild). This is
    /// the structural chokepoint for a Wave-4.3 agent's toolbelt — the effective
    /// belt is `allowed ∩ registered`, an INTERSECTION never a widening: a name
    /// in `allowed` but not registered simply yields nothing, and a registered
    /// tool not in `allowed` is physically absent from the result, so it can't be
    /// listed (`available_tools`/catalog) OR looked up (`get`) — enforcement is
    /// the registry's contents, not a filter that some call site might skip.
    pub fn restricted_to(&self, allowed: &std::collections::HashSet<String>) -> ToolRegistry {
        ToolRegistry {
            tools: self
                .tools
                .iter()
                .filter(|t| allowed.contains(t.name()))
                .cloned()
                .collect(),
        }
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

    /// Every registered tool's name (regardless of env). Used to build a
    /// full-belt-but-headless sub-dispatcher for unattended cron runs.
    pub fn all_names(&self) -> std::collections::HashSet<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
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
