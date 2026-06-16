//! Concrete provider implementations.
//!
//! Each adapter implements [`crate::llm::Provider`] and converts an external
//! wire format into the neutral streaming events the agent loop consumes, so no
//! provider's JSON shape leaks upward (`doc/architecture.md` §9). See
//! `doc/profile.md` for how providers are configured.

pub mod openai;

pub use openai::OpenAiProvider;
