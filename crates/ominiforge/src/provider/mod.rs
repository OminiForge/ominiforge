//! Concrete provider implementations.
//!
//! Each adapter implements [`crate::llm::Provider`] and converts an external
//! wire format into the neutral streaming events the agent loop consumes, so no
//! provider's JSON shape leaks upward (`doc/design/runtime-architecture.md` §9). See
//! `doc/profile.md` for how providers are configured.

pub mod openai;

pub use openai::OpenAiProvider;

use std::sync::Arc;

use crate::config::{ProviderType, ResolvedModel};
use crate::llm::{Provider, RetryingProvider};

/// Construct a concrete [`Provider`] from a resolved model selection.
///
/// Phase 1 wires only [`ProviderType::OpenaiChat`]; other types are rejected
/// earlier by [`crate::config::ConfigStore::resolve`], so this returns `None`
/// for them defensively rather than panicking.
///
/// The adapter is wrapped in a [`RetryingProvider`] so transient failures —
/// dropped connections and 429/5xx statuses (e.g. an overloaded inference
/// engine) — are retried with backoff instead of aborting the turn. The agent
/// loop drives the front-end notification from the final retried error
/// ([`crate::agent::StreamSink::on_retry`]).
#[must_use]
pub fn build(resolved: &ResolvedModel) -> Option<Arc<dyn Provider>> {
    match resolved.provider_type {
        ProviderType::OpenaiChat => Some(Arc::new(RetryingProvider::with_defaults(Arc::new(
            OpenAiProvider::new(
                resolved.provider_name.clone(),
                resolved.base_url.clone(),
                resolved.api_key.clone(),
            ),
        )))),
        _ => None,
    }
}
