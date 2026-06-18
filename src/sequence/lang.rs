//! Per-language node-kind spec for the sequence subsystem.
//!
//! The walker in [`super::visit`] keys off tree-sitter node kinds. Those
//! kinds are **disjoint** between the Rust and Python grammars (Rust emits
//! `function_item` / `call_expression` / `for_expression`, Python emits
//! `function_definition` / `call` / `for_statement`), so the walker can run
//! one unified `match` over both vocabularies and only branch on the
//! language where the *semantics* genuinely diverge (constructor/macro
//! skipping, callee classification, `if`/`match` lifting).
//!
//! This module concentrates the handful of structural facts that the
//! function-index builders ([`super::build_fn_index`],
//! [`super::list_functions_in_tree`]) need per language: which node kind is
//! a function, which is a method container (`impl` / `class`), and how to
//! read a container's owner name.
//!
//! Reuses [`crate::parser::Language`] as the canonical language enum rather
//! than introducing a sequence-local copy.

use crate::parser::Language;
use tree_sitter::Node;

/// Sequence-specific view over a [`Language`]. Implemented as an extension
/// trait so the canonical enum stays the single source of truth.
pub(super) trait SeqLang {
    /// Tree-sitter node kind for a function definition.
    fn fn_kind(self) -> &'static str;

    /// Tree-sitter node kind for a method container — a Rust `impl` block
    /// or a Python `class`.
    fn container_kind(self) -> &'static str;

    /// Extract the owner name of a method container node (`impl Foo` → `Foo`,
    /// `class Widget` → `Widget`). Returns `None` when the name can't be read.
    fn container_name(self, node: &Node, source: &str) -> Option<String>;
}

impl SeqLang for Language {
    fn fn_kind(self) -> &'static str {
        match self {
            Self::Rust => "function_item",
            Self::Python => "function_definition",
        }
    }

    fn container_kind(self) -> &'static str {
        match self {
            Self::Rust => "impl_item",
            Self::Python => "class_definition",
        }
    }

    fn container_name(self, node: &Node, source: &str) -> Option<String> {
        match self {
            // `impl Foo` / `impl Trait for Foo` — the receiver type is the
            // `type` field. We key methods on the bare type so `Foo::method`
            // resolves regardless of any trait prefix.
            Self::Rust => node
                .child_by_field_name("type")
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                .map(str::to_owned),
            // `class Widget:` — the class name is the `name` field.
            Self::Python => node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                .map(str::to_owned),
        }
    }
}
