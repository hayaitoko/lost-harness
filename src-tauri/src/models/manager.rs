//! `ModelManager` — the in-memory registry of providers and their clients.
//!
//! Spec §4: the model picker reads from this to populate the list, and
//! the agent loop (§9) calls `get_client` to obtain a streamable client.
//!
//! Threading: providers and clients are behind `parking_lot::RwLock` so the
//! UI thread can read (`list_providers`, `get_client`) concurrently with the
//! agent loop mutating the registry (`add_provider`, `remove_provider`).
//! `parking_lot` is chosen over `std::sync::RwLock` to avoid the std variant's
//! write-starvation behaviour and because we're not `await`-ing across the
//! critical section.

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use parking_lot::RwLock;

use super::client::ModelClient;
use super::provider::Provider;

pub struct ModelManager {
    providers: RwLock<Vec<Provider>>,
    clients: RwLock<HashMap<String, ModelClient>>,
}

impl Default for ModelManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelManager {
    /// Empty registry. The Tauri `setup` hook is responsible for seeding it
    /// with the providers persisted in `global.db::endpoints`.
    pub fn new() -> Self {
        Self {
            providers: RwLock::new(Vec::new()),
            clients: RwLock::new(HashMap::new()),
        }
    }

    /// Register a provider. If a provider with the same id already exists it
    /// is replaced (id is the stable key, see `Provider::new`). The matching
    /// cached client (if any) is dropped so the next `get_client` rebuilds it
    /// from the new config.
    pub fn add_provider(&self, p: Provider) {
        let id = p.id.clone();
        {
            let mut providers = self.providers.write();
            if let Some(existing) = providers.iter_mut().find(|x| x.id == id) {
                *existing = p;
            } else {
                providers.push(p);
            }
        }
        self.clients.write().remove(&id);
    }

    /// Remove a provider and its cached client. No-op if the id is unknown.
    pub fn remove_provider(&self, id: &str) {
        {
            let mut providers = self.providers.write();
            providers.retain(|p| p.id != id);
        }
        self.clients.write().remove(id);
    }

    /// Snapshot of all registered providers. Cheap clone — `Provider` is
    /// `Clone` and contains no large fields.
    pub fn list_providers(&self) -> Vec<Provider> {
        self.providers.read().clone()
    }

    /// Lookup a provider by id. Returns `None` if not registered.
    pub fn get_provider(&self, id: &str) -> Option<Provider> {
        self.providers.read().iter().find(|p| p.id == id).cloned()
    }

    /// Get a clone of the cached client for `id`, building it on first use.
    /// The client is independent of the provider record (it just holds a
    /// `reqwest::Client` and a copy of the config), so cloning is cheap and
    /// gives the agent loop its own handle.
    pub fn get_client(&self, id: &str) -> Option<ModelClient> {
        if let Some(c) = self.clients.read().get(id) {
            return Some(clone_client(c));
        }
        let provider = self.get_provider(id)?;
        let client = ModelClient::new(provider).ok()?;
        self.clients.write().insert(id.to_string(), clone_client(&client));
        Some(client)
    }

    /// `GET /models` on the named provider. Returns an error if the id is
    /// unknown or the HTTP call fails — the caller (model picker) decides
    /// how to surface the error.
    pub async fn list_models_for(&self, id: &str) -> Result<Vec<String>> {
        let client = self
            .get_client(id)
            .ok_or_else(|| anyhow!("unknown provider: {id}"))?;
        client.list_models().await
    }
}

// Clone a `ModelClient` by going through its `Provider`. `reqwest::Client`
// is internally `Arc`-backed so this is cheap, and the API requires `Clone`
// semantics for handle-style access from the agent loop.
fn clone_client(c: &ModelClient) -> ModelClient {
    ModelClient::new(c.provider().clone())
        .expect("ModelClient::new only fails on reqwest builder, which can't fail on a valid existing client")
}
