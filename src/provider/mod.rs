//! Concrete provider implementations.
//!
//! Each adapter implements [`crate::llm::Provider`] and converts an external
//! wire format into the neutral streaming events the agent loop consumes, so no
//! provider's JSON shape leaks upward (`doc/architecture.md` §9). See
//! `doc/profile.md` for how providers are configured.

pub mod openai;

pub use openai::OpenAiProvider;

use std::sync::Arc;

use crate::config::{ProviderType, ResolvedModel};
use crate::llm::Provider;

/// Construct a concrete [`Provider`] from a resolved model selection.
///
/// Phase 1 wires only [`ProviderType::OpenaiChat`]; other types are rejected
/// earlier by [`crate::config::ConfigStore::resolve`], so this returns `None`
/// for them defensively rather than panicking.
#[must_use]
pub fn build(resolved: &ResolvedModel) -> Option<Arc<dyn Provider>> {
    match resolved.provider_type {
        ProviderType::OpenaiChat => Some(Arc::new(OpenAiProvider::new(
            resolved.provider_name.clone(),
            resolved.base_url.clone(),
            resolved.api_key.clone(),
        ))),
        _ => None,
    }
}
