//! ast-to-mermaid — Mermaid diagram renderer for code ASTs.
//!
//! Wires the [`ingester`](https://github.com/anatta-rs/ingester) parsers into
//! a [`polystore`](https://github.com/anatta-rs/polystore)-backed graph and
//! renders [Mermaid](https://mermaid.js.org) diagrams from it.
//!
//! Five rendering levels:
//! - [`Level::Project`] — one node per crate, cross-crate calls.
//! - [`Level::Overview`] — one node per module, cross-module calls.
//! - [`Level::Module`] — subgraph for one module + external callers/callees.
//! - [`Level::Function`] — central function + direct callers/callees.
//! - [`Level::Impact`] — reverse call chain (N hops, default 3).

#![warn(missing_docs)]
#![deny(unsafe_code)]

pub mod cli_support;
pub mod error;
pub mod graph;
pub mod pipeline;
pub mod render;
pub mod resolve;

pub use error::{AstToMermaidError, Result};
pub use graph::InMemoryStore;
pub use ingester_code::CodeParser;
pub use ingester_core::{Atom, AtomId, Origin, ParseOutput, Parser, Relation};
pub use pipeline::{AnalyzeOptions, AnalyzeReport, analyze};
pub use polystore::{Direction, EntityId, GraphStore, KvStore, Scope, VectorHit, VectorStore};
pub use render::{Level, render};
pub use resolve::resolve_cross_module_calls;
