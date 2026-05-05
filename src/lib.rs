//! ast-to-mermaid — Mermaid diagram renderer for code ASTs.
//!
//! Parses Rust and Python source trees with tree-sitter, builds an in-memory
//! call/dependency graph, and renders [Mermaid](https://mermaid.js.org) diagrams
//! at five levels of detail.
//!
//! Self-contained — no external graph backend, no async runtime, no database.
//!
//! Five symbol-graph rendering levels:
//! - [`Level::Project`] — one node per crate, cross-crate calls.
//! - [`Level::Overview`] — one node per module, cross-module calls.
//! - [`Level::Module`] — subgraph for one module + external callers/callees.
//! - [`Level::Function`] — central function + direct callers/callees.
//! - [`Level::Impact`] — reverse call chain (N hops, default 3).
//!
//! Plus an orthogonal [`sequence`] view — Mermaid `sequenceDiagram`
//! extracted from one function's body in source order.

#![warn(missing_docs)]
#![deny(unsafe_code)]

pub mod artifacts;
pub mod cache;
pub mod cli;
pub mod diff;
pub mod error;
pub mod git_source;
pub mod graph;
pub mod limits;
pub mod model;
pub mod parser;
pub mod pipeline;
pub mod render;
pub mod resolve;
pub mod sequence;

pub use cli::*;
pub use error::{AstToMermaidError, Result};
pub use graph::Store;
pub use model::{AtomKind, CodeAtom, Edge, EdgeKind, EntityId};
pub use parser::{CodeParser, Language};
pub use pipeline::{AnalyzeOptions, AnalyzeReport, analyze};
pub use render::{Level, render};
pub use resolve::resolve_cross_module_calls;
