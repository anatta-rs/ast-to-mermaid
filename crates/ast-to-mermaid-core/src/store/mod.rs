//! Storage impls — concrete backends behind the [`polystore::GraphStore`] trait.
//!
//! v0.2 ships the in-memory backend only. Embedded `SurrealDB` and other
//! backends land in subsequent PRs.

pub mod memory;

pub use memory::{InMemoryStore, ingest_parse_output};
