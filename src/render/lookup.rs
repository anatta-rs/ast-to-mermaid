//! Resolve user-friendly target strings to concrete [`EntityId`]s.
//!
//! The CLI / MCP layer accepts targets like `"CodeParser"` or
//! `"crates/foo/src/lib.rs"`. These helpers walk an [`AtomSnapshot`] and
//! return the matching `EntityId`, or an error listing the candidates when
//! the input is ambiguous.
//!
//! Snapshot-driven (not [`crate::graph::Store`]-driven) on purpose: the
//! whole render pipeline holds a single read guard via
//! [`crate::graph::Store::with_atoms`], and resolving against the snapshot
//! avoids re-acquiring the same lock recursively.

use crate::error::{AstToMermaidError, Result};
use crate::model::EntityId;
use crate::render::snapshot::AtomSnapshot;

/// Resolve a target string to a `module` atom id.
///
/// Resolution order:
/// 1. Exact `EntityId` match (caller already has the id).
/// 2. Exact `file_path` metadata match (targets containing `/`).
/// 3. Exact `name` match — errors if multiple modules share that name.
///
/// # Errors
///
/// Returns [`AstToMermaidError::InvalidInput`] if no match or ambiguous.
///
/// # Panics
///
/// Panics on internal `Vec` invariants we just established (`len==1` after
/// match) — these expects are safety nets for changes to the surrounding
/// logic, never reachable through normal use.
pub fn resolve_module(snapshot: &AtomSnapshot<'_>, target: &str) -> Result<EntityId> {
    if target.is_empty() {
        return Err(AstToMermaidError::InvalidInput(
            "module target cannot be empty".to_owned(),
        ));
    }

    let modules = || snapshot.iter().filter(|a| a.kind == "module");

    // Pass 1: exact id.
    for m in modules() {
        if m.id.as_str() == target {
            return Ok(m.id.clone());
        }
    }

    // Pass 2: file_path match (only when target looks path-like).
    if target.contains('/') {
        for m in modules() {
            if m.file_path == target {
                return Ok(m.id.clone());
            }
        }
    }

    // Pass 3: name match (must be unique).
    let mut matches: Vec<EntityId> = modules()
        .filter(|m| m.name == target)
        .map(|m| m.id.clone())
        .collect();
    matches.sort();

    match matches.len() {
        0 => Err(AstToMermaidError::InvalidInput(format!(
            "no module found matching {target:?}"
        ))),
        1 => Ok(matches.into_iter().next().expect("len==1")),
        n => Err(AstToMermaidError::InvalidInput(format!(
            "ambiguous module target {target:?}: {n} candidates ({})",
            matches
                .iter()
                .map(|id| id.as_str().to_owned())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Resolve a target string to a `function` atom id.
///
/// Resolution order:
/// 1. Exact `EntityId` match.
/// 2. `Type::method` shorthand — when the target contains `::`, the last
///    segment is treated as the method name and the preceding segments as
///    a substring hint matched against the candidate's id. This lets you
///    write `--target HnswBuilder::build` and find
///    `code:src/hnsw.rs::function::HnswBuilder<'a, D, M, M0>::build`
///    (the substring `HnswBuilder` is present in the id).
/// 3. Exact `name` match — errors if multiple functions share that name.
///    Tip: pass a fully-qualified id like
///    `code:crates/foo/src/lib.rs::function::bar` to disambiguate.
///
/// # Errors
///
/// Returns [`AstToMermaidError::InvalidInput`] if no match or ambiguous.
///
/// # Panics
///
/// Panics on internal `Vec` invariants we just established (`len==1` after
/// match) — never reachable through normal use.
pub fn resolve_function(snapshot: &AtomSnapshot<'_>, target: &str) -> Result<EntityId> {
    if target.is_empty() {
        return Err(AstToMermaidError::InvalidInput(
            "function target cannot be empty".to_owned(),
        ));
    }

    let functions = || snapshot.iter().filter(|a| a.kind == "function");

    // Pass 1: exact id.
    for f in functions() {
        if f.id.as_str() == target {
            return Ok(f.id.clone());
        }
    }

    // Pass 2: Type::method shorthand.
    if let Some((owner_hint, method_name)) = target.rsplit_once("::")
        && !owner_hint.is_empty()
        && !method_name.is_empty()
    {
        let mut matches: Vec<EntityId> = functions()
            .filter(|f| f.name == method_name && f.id.as_str().contains(owner_hint))
            .map(|f| f.id.clone())
            .collect();
        matches.sort();
        match matches.len() {
            0 => {} // fall through to bare-name pass
            1 => return Ok(matches.into_iter().next().expect("len==1")),
            n => {
                return Err(AstToMermaidError::InvalidInput(format!(
                    "ambiguous function target {target:?}: {n} candidates ({}{})",
                    matches
                        .iter()
                        .take(3)
                        .map(|id| id.as_str().to_owned())
                        .collect::<Vec<_>>()
                        .join(", "),
                    if n > 3 { ", ..." } else { "" }
                )));
            }
        }
    }

    // Pass 3: exact name (may be ambiguous).
    let mut matches: Vec<EntityId> = functions()
        .filter(|f| f.name == target)
        .map(|f| f.id.clone())
        .collect();
    matches.sort();

    match matches.len() {
        0 => Err(AstToMermaidError::InvalidInput(format!(
            "no function found matching {target:?}"
        ))),
        1 => Ok(matches.into_iter().next().expect("len==1")),
        n => Err(AstToMermaidError::InvalidInput(format!(
            "ambiguous function target {target:?}: {n} candidates ({}{})",
            matches
                .iter()
                .take(3)
                .map(|id| id.as_str().to_owned())
                .collect::<Vec<_>>()
                .join(", "),
            if n > 3 { ", ..." } else { "" }
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Store;
    use crate::model::CodeAtom;

    fn module(id: &str, file_path: &str, name: &str) -> CodeAtom {
        CodeAtom {
            id: EntityId::new(id),
            kind: "module".to_owned(),
            name: name.to_owned(),
            file_path: file_path.to_owned(),
            line_start: 1,
            line_end: 1,
            doc: String::new(),
            signature: String::new(),
            content_hash: "h".to_owned(),
            calls: Vec::new(),
            method_calls: Vec::new(),
            parent: None,
        }
    }

    fn function(id: &str, file_path: &str, name: &str) -> CodeAtom {
        CodeAtom {
            id: EntityId::new(id),
            kind: "function".to_owned(),
            name: name.to_owned(),
            file_path: file_path.to_owned(),
            line_start: 1,
            line_end: 10,
            doc: String::new(),
            signature: String::new(),
            content_hash: "h".to_owned(),
            calls: Vec::new(),
            method_calls: Vec::new(),
            parent: None,
        }
    }

    fn with_snap<F, R>(store: &Store, f: F) -> R
    where
        F: FnOnce(&AtomSnapshot<'_>) -> R,
    {
        store.with_atoms(|atoms| {
            let snap = AtomSnapshot::build(atoms);
            f(&snap)
        })
    }

    #[test]
    fn resolve_module_empty_target_errors() {
        let store = Store::new();
        let err = with_snap(&store, |s| resolve_module(s, "")).expect_err("empty rejected");
        assert!(matches!(err, AstToMermaidError::InvalidInput(_)));
    }

    #[test]
    fn resolve_module_by_exact_id() {
        let store = Store::new();
        store.add_atom(module("code:src/foo.rs", "src/foo.rs", "foo"));
        let id = with_snap(&store, |s| resolve_module(s, "code:src/foo.rs")).expect("ok");
        assert_eq!(id.as_str(), "code:src/foo.rs");
    }

    #[test]
    fn resolve_module_by_file_path() {
        let store = Store::new();
        store.add_atom(module("code:src/foo.rs", "src/foo.rs", "foo"));
        let id = with_snap(&store, |s| resolve_module(s, "src/foo.rs")).expect("ok");
        assert_eq!(id.as_str(), "code:src/foo.rs");
    }

    #[test]
    fn resolve_module_by_name_unique() {
        let store = Store::new();
        store.add_atom(module("code:src/foo.rs", "src/foo.rs", "foo"));
        let id = with_snap(&store, |s| resolve_module(s, "foo")).expect("ok");
        assert_eq!(id.as_str(), "code:src/foo.rs");
    }

    #[test]
    fn resolve_module_by_name_ambiguous_errors() {
        let store = Store::new();
        store.add_atom(module("code:a/queries.rs", "a/queries.rs", "queries"));
        store.add_atom(module("code:b/queries.rs", "b/queries.rs", "queries"));
        let err = with_snap(&store, |s| resolve_module(s, "queries")).expect_err("ambiguous");
        assert!(err.to_string().contains("ambiguous"));
        assert!(err.to_string().contains("2 candidates"));
    }

    #[test]
    fn resolve_module_no_match_errors() {
        let store = Store::new();
        store.add_atom(module("code:src/foo.rs", "src/foo.rs", "foo"));
        let err = with_snap(&store, |s| resolve_module(s, "ghost")).expect_err("missing");
        assert!(err.to_string().contains("no module"));
    }

    #[test]
    fn resolve_function_by_exact_id() {
        let store = Store::new();
        store.add_atom(function(
            "code:src/lib.rs::function::foo",
            "src/lib.rs",
            "foo",
        ));
        let id = with_snap(&store, |s| {
            resolve_function(s, "code:src/lib.rs::function::foo")
        })
        .expect("ok");
        assert_eq!(id.as_str(), "code:src/lib.rs::function::foo");
    }

    #[test]
    fn resolve_function_by_name_unique() {
        let store = Store::new();
        store.add_atom(function(
            "code:src/lib.rs::function::foo",
            "src/lib.rs",
            "foo",
        ));
        let id = with_snap(&store, |s| resolve_function(s, "foo")).expect("ok");
        assert_eq!(id.as_str(), "code:src/lib.rs::function::foo");
    }

    #[test]
    fn resolve_function_by_name_ambiguous_errors() {
        let store = Store::new();
        for file in ["src/a.rs", "src/b.rs", "src/c.rs"] {
            let id = format!("code:{file}::function::render");
            store.add_atom(function(&id, file, "render"));
        }
        let err = with_snap(&store, |s| resolve_function(s, "render")).expect_err("ambiguous");
        assert!(err.to_string().contains("ambiguous"));
        assert!(err.to_string().contains("3 candidates"));
    }

    #[test]
    fn resolve_function_ambiguous_truncates_long_lists() {
        let store = Store::new();
        for i in 0..6_u8 {
            let id = format!("code:src/m{i}.rs::function::f");
            let file = format!("src/m{i}.rs");
            store.add_atom(function(&id, &file, "f"));
        }
        let err = with_snap(&store, |s| resolve_function(s, "f")).expect_err("ambiguous");
        let s = err.to_string();
        assert!(s.contains("6 candidates"), "got: {s}");
        assert!(s.contains("..."), "list should be truncated, got: {s}");
    }

    #[test]
    fn resolve_function_empty_target_errors() {
        let store = Store::new();
        assert!(matches!(
            with_snap(&store, |s| resolve_function(s, "")).expect_err("empty"),
            AstToMermaidError::InvalidInput(_)
        ));
    }

    #[test]
    fn resolve_function_no_match_errors() {
        let store = Store::new();
        store.add_atom(function(
            "code:src/lib.rs::function::foo",
            "src/lib.rs",
            "foo",
        ));
        let err = with_snap(&store, |s| resolve_function(s, "ghost")).expect_err("missing");
        assert!(err.to_string().contains("no function"));
    }

    #[test]
    fn resolve_function_type_method_shorthand_disambiguates_by_owner() {
        // Three impls all have `build`. `Foo::build` must match exactly one.
        let store = Store::new();
        for owner in ["Foo", "Bar", "Baz"] {
            let id = format!(
                "code:src/{}.rs::function::{owner}::build",
                owner.to_lowercase()
            );
            let file = format!("src/{}.rs", owner.to_lowercase());
            store.add_atom(function(&id, &file, "build"));
        }
        let id = with_snap(&store, |s| resolve_function(s, "Foo::build")).expect("ok");
        assert_eq!(id.as_str(), "code:src/foo.rs::function::Foo::build");
    }

    #[test]
    fn resolve_function_type_method_shorthand_handles_generics_in_id() {
        // The user passes `HnswBuilder::build` but the actual id has the
        // generics: `HnswBuilder<'a, D, M, M0>::build`. Substring match on
        // the owner hint must still succeed.
        let store = Store::new();
        store.add_atom(function(
            "code:src/hnsw.rs::function::HnswBuilder<'a, D, M, M0>::build",
            "src/hnsw.rs",
            "build",
        ));
        // Decoy with the same method name but a different owner.
        store.add_atom(function(
            "code:src/python.rs::function::PyWriter::build",
            "src/python.rs",
            "build",
        ));
        let id = with_snap(&store, |s| resolve_function(s, "HnswBuilder::build")).expect("ok");
        assert!(
            id.as_str().contains("HnswBuilder<'a, D, M, M0>"),
            "got {id}"
        );
    }

    #[test]
    fn resolve_function_type_method_shorthand_falls_through_when_no_match() {
        // The owner hint matches no atom; resolver falls through to bare-name
        // matching on the method portion.
        let store = Store::new();
        store.add_atom(function(
            "code:src/lib.rs::function::foo",
            "src/lib.rs",
            "foo",
        ));
        // `Nonexistent::foo` — no atom contains `Nonexistent`. Falls through
        // to bare-name match on `Nonexistent::foo` which fails too.
        let err =
            with_snap(&store, |s| resolve_function(s, "Nonexistent::foo")).expect_err("no match");
        assert!(err.to_string().contains("no function"));
    }
}
