//! Core library for ast-to-mermaid.
//!
//! This crate hosts the parser (tree-sitter Rust+Python), the renderer
//! (Mermaid 5 levels), the cross-module resolver, and the storage impls
//! ([`polystore`] traits backed by `InMemory`, embedded `SurrealDB`, etc.).
//!
//! `v0.1.0` is a scaffold — module stubs only. Real functionality lands
//! in subsequent PRs (parser, render, resolve, store).
//!
//! # Re-exports
//!
//! Convenience re-exports from [`polystore`] so consumers don't need a
//! separate dependency line for the trait surface.
//!
//! ```
//! use ast_to_mermaid_core::{GraphStore, KvStore, VectorStore, Scope, EntityId};
//! let _ = Scope::new("ns", "repo", "branch");
//! let _ = EntityId::new("fn:foo");
//! # fn _force_use<G: GraphStore<u8, u8>, K: KvStore, V: VectorStore>(_: &G, _: &K, _: &V) {}
//! ```

#![warn(missing_docs)]
#![deny(unsafe_code)]

pub mod cli;
pub mod error;
pub mod mcp;

pub use error::{AstToMermaidError, Result};
pub use polystore::{Direction, EntityId, GraphStore, KvStore, Scope, VectorHit, VectorStore};
