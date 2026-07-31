//! Shared helpers for the cross-module-resolution integration tests.

use std::fs;
use std::path::Path;

use ast_to_mermaid::graph::Store;
use ast_to_mermaid::parser::{CodeParser, Language};
use ast_to_mermaid::pipeline::walk_for_languages;
use ast_to_mermaid::resolve::resolve_cross_module_calls;

/// Walk `root`, parse every supported source file, apply the result to a
/// fresh [`Store`], and run the cross-module resolver. Mirrors the bundle
/// pipeline minus the artifact emission step.
pub fn build_store(root: &Path) -> Store {
    let files = walk_for_languages(root).expect("walk");
    let store = Store::new();
    for (path, lang) in &files {
        let bytes = fs::read(path).expect("read");
        let parser = match lang {
            Language::Rust => CodeParser::rust(),
            Language::Python => CodeParser::python(),
            Language::Dart => CodeParser::dart(),
        };
        let display = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string();
        parser
            .parse(&bytes, &display)
            .expect("parse")
            .apply_to(&store);
    }
    resolve_cross_module_calls(&store);
    store
}
