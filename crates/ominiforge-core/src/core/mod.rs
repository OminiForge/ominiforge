//! Core protocol types: the event envelope, identifiers, and domain payloads.
//!
//! This module is the foundation every other subsystem depends on, and depends
//! on nothing above it. It defines the unified event protocol described in
//! `doc/event-schema.md`: a shared envelope (`CoreEvent`) plus a domain-tagged
//! [`EventPayload`] enum (Turn / Model / Tool / Session / Artifact / Injection
//! / Error).
//!
//! Persisted events are immutable. Schema evolution is append-compatible:
//! consumers ignore unknown fields and skip unknown event types rather than
//! failing. Breaking changes bump [`SCHEMA_VERSION`].

mod envelope;
mod ids;
pub mod payload;

pub use envelope::{CoreEvent, EventSource, SourceKind};
pub use ids::{ArtifactId, EventId, SessionId, TurnId};
pub use payload::EventPayload;

/// Current event protocol version, embedded in every persisted [`CoreEvent`].
///
/// Bumped only for breaking changes; additive changes keep the same version.
pub const SCHEMA_VERSION: &str = "ominiforge.event.v1";
