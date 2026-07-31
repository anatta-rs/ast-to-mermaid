//! Compiled tree-sitter queries, lazily initialised once per language.
//!
//! `Query::new` parses the `.scm` source and validates it against the
//! grammar — non-trivial work that we want to do once and share across
//! every file we parse.
//!
//! The query files live in `src/parser/queries/{rust,python,dart}/*.scm` and
//! are bundled into the binary via `include_str!`.

use std::sync::LazyLock;
use tree_sitter::Query;

/// Cached Rust queries.
pub(super) struct RustQueries {
    /// Top-level items (function, struct, trait, impl, enum, …).
    pub items: Query,
    /// Methods inside `impl` blocks.
    pub impl_methods: Query,
    /// Call sites inside a function body.
    pub calls: Query,
    /// `use` declarations anywhere in the file (including nested mods).
    pub uses: Query,
}

/// Cached Python queries.
pub(super) struct PythonQueries {
    /// Top-level items (`function_definition`, `class_definition`,
    /// `decorated_definition`).
    pub items: Query,
    /// Methods inside `class` blocks.
    pub class_methods: Query,
    /// Call sites inside a function body.
    pub calls: Query,
    /// `import` / `from … import …` statements anywhere in the file.
    pub imports: Query,
}

/// Cached Dart queries.
pub(super) struct DartQueries {
    /// Top-level items (`class`, `mixin`, `extension`, `enum`,
    /// `function_declaration`, `type_alias`).
    pub items: Query,
    /// Methods inside a container body — Dart has four container kinds,
    /// so this covers more ground than its Rust/Python counterparts.
    pub class_methods: Query,
    /// Call sites inside a function body.
    pub calls: Query,
    /// `import` / `export` directives anywhere in the file.
    pub imports: Query,
}

pub(super) static RUST: LazyLock<RustQueries> = LazyLock::new(|| {
    let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
    RustQueries {
        items: Query::new(&lang, include_str!("queries/rust/items.scm"))
            .expect("queries/rust/items.scm must compile"),
        impl_methods: Query::new(&lang, include_str!("queries/rust/impl_methods.scm"))
            .expect("queries/rust/impl_methods.scm must compile"),
        calls: Query::new(&lang, include_str!("queries/rust/calls.scm"))
            .expect("queries/rust/calls.scm must compile"),
        uses: Query::new(&lang, include_str!("queries/rust/uses.scm"))
            .expect("queries/rust/uses.scm must compile"),
    }
});

pub(super) static PYTHON: LazyLock<PythonQueries> = LazyLock::new(|| {
    let lang: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
    PythonQueries {
        items: Query::new(&lang, include_str!("queries/python/items.scm"))
            .expect("queries/python/items.scm must compile"),
        class_methods: Query::new(&lang, include_str!("queries/python/class_methods.scm"))
            .expect("queries/python/class_methods.scm must compile"),
        calls: Query::new(&lang, include_str!("queries/python/calls.scm"))
            .expect("queries/python/calls.scm must compile"),
        imports: Query::new(&lang, include_str!("queries/python/imports.scm"))
            .expect("queries/python/imports.scm must compile"),
    }
});

pub(super) static DART: LazyLock<DartQueries> = LazyLock::new(|| {
    let lang: tree_sitter::Language = tree_sitter_dart::LANGUAGE.into();
    DartQueries {
        items: Query::new(&lang, include_str!("queries/dart/items.scm"))
            .expect("queries/dart/items.scm must compile"),
        class_methods: Query::new(&lang, include_str!("queries/dart/class_methods.scm"))
            .expect("queries/dart/class_methods.scm must compile"),
        calls: Query::new(&lang, include_str!("queries/dart/calls.scm"))
            .expect("queries/dart/calls.scm must compile"),
        imports: Query::new(&lang, include_str!("queries/dart/imports.scm"))
            .expect("queries/dart/imports.scm must compile"),
    }
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_queries_compile() {
        // Forces lazy init; panics on parse error in any .scm.
        let _ = &RUST.items;
        let _ = &RUST.impl_methods;
        let _ = &RUST.calls;
        let _ = &RUST.uses;
    }

    #[test]
    fn python_queries_compile() {
        let _ = &PYTHON.items;
        let _ = &PYTHON.class_methods;
        let _ = &PYTHON.calls;
        let _ = &PYTHON.imports;
    }

    #[test]
    fn dart_queries_compile() {
        let _ = &DART.items;
        let _ = &DART.class_methods;
        let _ = &DART.calls;
        let _ = &DART.imports;
    }
}
