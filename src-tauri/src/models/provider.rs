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

/// C5 (Q6): where a `Local`-kind provider came from — descriptive metadata only,
/// so the UI (and future backend logic) can tell a user-typed local endpoint
/// from the app's own bundled sidecar WITHOUT string-sniffing the id. Never a
/// routing input (`enforce_local_routing` stays origin-blind). Only meaningful
/// when `kind == Local`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalOrigin {
    /// The user added this endpoint (Settings → Providers) — LM Studio, Ollama,
    /// a hand-run llama.cpp. Persisted to `endpoints`.
    UserAdded,
    /// The bundled sidecar, lazily spawned by `models::runner::ensure_running`
    /// (M8 S4). Ephemeral session state, id `local-runner:<catalog_id>`, never
    /// persisted to `endpoints`.
    BundledRunner,
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
    /// C5: origin of a `Local` provider (bundled sidecar vs user-added). `None`
    /// for Cloud/Custom and for any Local provider built via plain `new`.
    #[serde(default)]
    pub local_origin: Option<LocalOrigin>,
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
            local_origin: None,
        }
    }

    /// Builder: mark this endpoint as supporting native structured tool calls.
    pub fn with_native_tools(mut self, supported: bool) -> Self {
        self.supports_native_tools = supported;
        self
    }

    /// Builder: stamp the origin of a `Local` provider (C5).
    pub fn with_local_origin(mut self, origin: LocalOrigin) -> Self {
        self.local_origin = Some(origin);
        self
    }

    /// True when this is the app's bundled sidecar (M8 S4 lazy-spawn) rather
    /// than a user-added local endpoint — descriptive only (never a routing
    /// input). Lets the UI say "started your local model" vs "switched to <name>".
    pub fn is_bundled_runner(&self) -> bool {
        matches!(self.local_origin, Some(LocalOrigin::BundledRunner))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_origin_defaults_to_none_and_is_builder_set() {
        // C5: a plain Local provider has no origin and isn't a bundled runner —
        // keeps every existing Provider::new call site's meaning unchanged.
        let plain = Provider::new("p", "P", "http://127.0.0.1:1234/v1", None, ProviderKind::Local);
        assert_eq!(plain.local_origin, None);
        assert!(!plain.is_bundled_runner());

        // The builder stamps the bundled-sidecar origin.
        let bundled = plain.clone().with_local_origin(LocalOrigin::BundledRunner);
        assert!(bundled.is_bundled_runner());

        // A user-added local endpoint is explicitly UserAdded → not a runner.
        let user = Provider::new("lm", "LM", "http://127.0.0.1:1234/v1", None, ProviderKind::Local)
            .with_local_origin(LocalOrigin::UserAdded);
        assert!(!user.is_bundled_runner());

        // Cloud is never a bundled runner regardless.
        let cloud = Provider::new("c", "C", "https://api.x/v1", None, ProviderKind::Cloud);
        assert!(!cloud.is_bundled_runner());
    }

    #[test]
    fn local_origin_serde_round_trips_and_old_json_defaults_none() {
        // Old persisted JSON without `local_origin` deserializes to None.
        let old = r#"{"id":"p","name":"P","base_url":"http://127.0.0.1:1234/v1","api_key":null,"kind":"local"}"#;
        let p: Provider = serde_json::from_str(old).unwrap();
        assert_eq!(p.local_origin, None);
    }
}
