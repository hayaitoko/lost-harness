//! `Provider` — configuration for one model endpoint.
//!
//! Spec §4 (model picker groups endpoints by provider) + §7 (privacy gate
//! needs to know if an endpoint is cloud or local). The endpoint URL is
//! parsed by `crate::agent::egress::is_private_endpoint` to decide
//! routing — we do not duplicate the private-range logic here.

use serde::{Deserialize, Serialize};

/// Where a model runs. Used by the model picker (§4) to group and label
/// endpoints in the UI.
///
/// Serializes lowercase (`"local"`/`"cloud"`/`"custom"`) to match the
/// frontend, which sends `kind` lowercase and compares `p.kind === "local"`
/// (provider-catalog.ts, providers.svelte.ts, ProviderSettings.svelte,
/// ModelPicker.svelte). Without this the IPC returns PascalCase `"Cloud"` and
/// the frontend's kind checks silently fail in the real Tauri shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// On-device (llama.cpp, LM Studio, Ollama) — never egresses.
    Local,
    /// Hosted by a third party (OpenAI, Anthropic, OpenRouter) — egresses.
    Cloud,
    /// User-defined: a private server on the user's own network (Tadashi,
    /// self-hosted, etc.). May or may not egress depending on `base_url`.
    Custom,
}

/// Configuration for a single model endpoint.
///
/// `base_url` should be the *root* of the OpenAI-compatible surface — the
/// client appends `/models` and `/chat/completions` itself, so for OpenAI
/// you'd pass `https://api.openai.com/v1` (note the `/v1`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    /// Stable identifier used by `ModelManager` and persisted in storage.
    pub id: String,
    /// Human-friendly name shown in the model picker (e.g. "OpenAI").
    pub name: String,
    /// Root URL of the OpenAI-compatible surface.
    pub base_url: String,
    /// Bearer token. `None` for local endpoints that don't require auth.
    pub api_key: Option<String>,
    /// What kind of endpoint this is. Used by the UI to group endpoints.
    pub kind: ProviderKind,
    /// Q1: the endpoint's API supports OpenAI-style structured tool calls
    /// (`tools` request param + `tool_calls` deltas). When true the agent
    /// loop uses the native transport; otherwise the fenced dialect.
    #[serde(default)]
    pub supports_native_tools: bool,
}

impl Provider {
    /// Build a new `Provider` with the given fields. The id should be unique
    /// across the registry — `ModelManager::add_provider` will replace an
    /// existing provider with the same id.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        base_url: impl Into<String>,
        api_key: Option<String>,
        kind: ProviderKind,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            base_url: base_url.into(),
            api_key,
            kind,
            supports_native_tools: false,
        }
    }

    /// Builder: mark this endpoint as supporting native structured tool calls.
    pub fn with_native_tools(mut self, supported: bool) -> Self {
        self.supports_native_tools = supported;
        self
    }

    /// A provider is "local" if its `kind` is `Local`. Use `is_private` for
    /// a network-level check on custom / cloud endpoints.
    pub fn is_local(&self) -> bool {
        matches!(self.kind, ProviderKind::Local)
    }

    /// A provider is "private" if its `base_url` resolves to a private /
    /// loopback / tailnet endpoint. This is the load-bearing check used by
    /// the §7 privacy gate: a `Custom` provider pointing at `http://10.0.0.5`
    /// is private, but a `Custom` provider pointing at `https://api.example.com`
    /// is not, even though the user added it manually.
    pub fn is_private(&self) -> bool {
        crate::agent::egress::is_private_endpoint(&self.base_url)
    }
}
