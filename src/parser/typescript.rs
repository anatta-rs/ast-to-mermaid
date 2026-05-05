//! TypeScript extractors — placeholder.
//!
//! The dispatch in [`super`] is structured as a per-language module so a
//! TypeScript backend can land alongside Rust and Python without
//! re-touching `parser/mod.rs`. When the grammar dependency
//! (`tree-sitter-typescript`) is added and a [`super::Language::TypeScript`]
//! variant is wired up, this module will host the same trio that the Rust
//! and Python modules expose: doc-comment lookup, top-level item kinds,
//! and call-site extraction. Until then the file deliberately stays empty
//! — keeping the per-language file layout visible at a glance is the
//! whole point of the split.
