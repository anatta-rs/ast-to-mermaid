//! Core library for ast-to-mermaid.
//!
//! Wires the [`ingester`](https://github.com/anatta-rs/ingester) parsers into
//! a [`polystore`](https://github.com/anatta-rs/polystore)-backed graph and
//! renders [Mermaid](https://mermaid.js.org) diagrams from it.
//!
//! v0.2 ships:
//! - Re-exports of polystore traits + ingester atom types.
//! - [`store::InMemoryStore`] — in-memory `GraphStore<Atom, Relation>`.
//! - CLI dispatch (still stubs — analyze lands in v0.3).
//! - MCP entry point stub.
//!
//! Renderer + resolver + concrete CLI implementations land in subsequent PRs
//! as the MVP path completes.

#![warn(missing_docs)]
#![deny(unsafe_code)]

pub mod cli;
pub mod error;
pub mod mcp;
pub mod resolve;
pub mod store;

pub use error::{AstToMermaidError, Result};
pub use ingester_code::CodeParser;
pub use ingester_core::{Atom, AtomId, Origin, ParseOutput, Parser, Relation};
pub use polystore::{Direction, EntityId, GraphStore, KvStore, Scope, VectorHit, VectorStore};
pub use resolve::resolve_cross_module_calls;
pub use store::InMemoryStore;
