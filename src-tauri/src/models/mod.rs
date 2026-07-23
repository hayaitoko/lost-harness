//! §4 Model Manager — OpenAI-compatible HTTP client + endpoint registry.
//!
//! Modules:
//!   - `provider` — endpoint config (`Provider`, `ProviderKind`)
//!   - `client`   — per-provider HTTP client (`ModelClient`)
//!   - `manager`  — registry of providers, lookup by id
//!   - `sse`      — incremental SSE stream parser (delta / done / error)
//!
//! Spec §4 (Model Picker) + §8 (Model API Abstraction) + §9 (Agent Loop).
//! The agent loop calls into this module to select a model, build a request,
//! and consume the response stream. Privacy enforcement is layered on top via
//! `crate::agent::egress::is_private_endpoint` (see `Provider::is_private`).

pub mod calculator;
pub mod content;
pub mod provider;
pub mod client;
pub mod manager;
pub mod catalog;
pub mod download;
pub mod gguf_meta;
pub mod hardware;
pub mod hf_search;
pub mod pricing;
#[cfg(feature = "local-runner")]
pub mod runner;
pub mod seat;
pub mod sse;

#[cfg(test)]
mod tests;

pub use client::{ChatMessage, ModelClient, OwnOutput};
pub use manager::ModelManager;
pub use provider::{Provider, ProviderKind};
pub use seat::resolve_seat;
