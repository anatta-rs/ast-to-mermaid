//! Resolve user-friendly target strings to concrete atom ids.
//!
//! The CLI / MCP layer accepts targets like `"CodeParser"` or
//! `"crates/foo/src/lib.rs"`. These helpers walk the [`GraphStore`] and
//! return the matching `EntityId`, or an error listing the candidates when
//! the input is ambiguous.

use crate::error::{AstToMermaidError, Result};
use ingester_core::{Atom, Relation};
use polystore::{EntityId, GraphStore};

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
pub async fn resolve_module<S>(store: &S, target: &str) -> Result<EntityId>
where
    S: GraphStore<Atom, Relation>,
{
    if target.is_empty() {
        return Err(AstToMermaidError::InvalidInput(
            "module target cannot be empty".to_owned(),
        ));
    }

    let module_ids = store.list_by_kind("module").await?;

    // Pass 1: exact id.
    for id in &module_ids {
        if id.as_str() == target {
            return Ok(id.clone());
        }
    }

    // Pass 2: file_path match (only when target looks path-like).
    if target.contains('/') {
        for id in &module_ids {
            if let Some(atom) = store.get_node(id).await?
                && let Some(p) = atom.metadata.get("file_path").and_then(|v| v.as_str())
                && p == target
            {
                return Ok(id.clone());
            }
        }
    }

    // Pass 3: name match (must be unique).
    let mut matches: Vec<EntityId> = Vec::new();
    for id in &module_ids {
        if let Some(atom) = store.get_node(id).await?
            && atom.name == target
        {
            matches.push(id.clone());
        }
    }
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
/// 2. Exact `name` match — errors if multiple functions share that name.
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
pub async fn resolve_function<S>(store: &S, target: &str) -> Result<EntityId>
where
    S: GraphStore<Atom, Relation>,
{
    if target.is_empty() {
        return Err(AstToMermaidError::InvalidInput(
            "function target cannot be empty".to_owned(),
        ));
    }

    let function_ids = store.list_by_kind("function").await?;

    for id in &function_ids {
        if id.as_str() == target {
            return Ok(id.clone());
        }
    }

    let mut matches: Vec<EntityId> = Vec::new();
    for id in &function_ids {
        if let Some(atom) = store.get_node(id).await?
            && atom.name == target
        {
            matches.push(id.clone());
        }
    }
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
    use crate::store::{InMemoryStore, ingest_parse_output};
    use ingester_core::{AtomId, ParseOutput};
    use polystore::Scope;

    fn scope() -> Scope {
        Scope::new("ns", "repo", "branch")
    }

    fn module(file_path: &str, name: &str) -> Atom {
        Atom::new(
            AtomId::new(format!("code:{file_path}")),
            "module",
            name,
            "h",
        )
        .with_metadata(serde_json::json!({"file_path": file_path}))
    }

    fn function(file_path: &str, name: &str) -> Atom {
        Atom::new(
            AtomId::new(format!("code:{file_path}::function::{name}")),
            "function",
            name,
            "h",
        )
        .with_metadata(serde_json::json!({"file_path": file_path}))
    }

    fn output_with(atoms: Vec<Atom>) -> ParseOutput {
        ParseOutput {
            atoms,
            ..ParseOutput::default()
        }
    }

    async fn make_store(atoms: Vec<Atom>) -> InMemoryStore {
        let store = InMemoryStore::new(scope());
        ingest_parse_output(&store, &output_with(atoms))
            .await
            .expect("ingest");
        store
    }

    #[tokio::test]
    async fn resolve_module_empty_target_errors() {
        let store = make_store(vec![]).await;
        let err = resolve_module(&store, "")
            .await
            .expect_err("empty rejected");
        assert!(matches!(err, AstToMermaidError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn resolve_module_by_exact_id() {
        let store = make_store(vec![module("src/foo.rs", "foo")]).await;
        let id = resolve_module(&store, "code:src/foo.rs").await.expect("ok");
        assert_eq!(id.as_str(), "code:src/foo.rs");
    }

    #[tokio::test]
    async fn resolve_module_by_file_path() {
        let store = make_store(vec![module("src/foo.rs", "foo")]).await;
        let id = resolve_module(&store, "src/foo.rs").await.expect("ok");
        assert_eq!(id.as_str(), "code:src/foo.rs");
    }

    #[tokio::test]
    async fn resolve_module_by_name_unique() {
        let store = make_store(vec![module("src/foo.rs", "foo")]).await;
        let id = resolve_module(&store, "foo").await.expect("ok");
        assert_eq!(id.as_str(), "code:src/foo.rs");
    }

    #[tokio::test]
    async fn resolve_module_by_name_ambiguous_errors() {
        let store = make_store(vec![
            module("src/a/queries.rs", "queries"),
            module("src/b/queries.rs", "queries"),
        ])
        .await;
        let err = resolve_module(&store, "queries")
            .await
            .expect_err("ambiguous");
        assert!(err.to_string().contains("ambiguous"));
        assert!(err.to_string().contains("2 candidates"));
    }

    #[tokio::test]
    async fn resolve_module_no_match_errors() {
        let store = make_store(vec![module("src/foo.rs", "foo")]).await;
        let err = resolve_module(&store, "ghost").await.expect_err("missing");
        assert!(err.to_string().contains("no module"));
    }

    #[tokio::test]
    async fn resolve_function_by_exact_id() {
        let store = make_store(vec![function("src/lib.rs", "foo")]).await;
        let id = resolve_function(&store, "code:src/lib.rs::function::foo")
            .await
            .expect("ok");
        assert_eq!(id.as_str(), "code:src/lib.rs::function::foo");
    }

    #[tokio::test]
    async fn resolve_function_by_name_unique() {
        let store = make_store(vec![function("src/lib.rs", "foo")]).await;
        let id = resolve_function(&store, "foo").await.expect("ok");
        assert_eq!(id.as_str(), "code:src/lib.rs::function::foo");
    }

    #[tokio::test]
    async fn resolve_function_by_name_ambiguous_errors() {
        let store = make_store(vec![
            function("src/a.rs", "render"),
            function("src/b.rs", "render"),
            function("src/c.rs", "render"),
        ])
        .await;
        let err = resolve_function(&store, "render")
            .await
            .expect_err("ambiguous");
        assert!(err.to_string().contains("ambiguous"));
        assert!(err.to_string().contains("3 candidates"));
    }

    #[tokio::test]
    async fn resolve_function_ambiguous_truncates_long_lists() {
        let mut atoms = Vec::new();
        for i in 0..6 {
            atoms.push(function(&format!("src/m{i}.rs"), "f"));
        }
        let store = make_store(atoms).await;
        let err = resolve_function(&store, "f").await.expect_err("ambiguous");
        let s = err.to_string();
        assert!(s.contains("6 candidates"), "got: {s}");
        assert!(s.contains("..."), "list should be truncated, got: {s}");
    }

    #[tokio::test]
    async fn resolve_function_empty_target_errors() {
        let store = make_store(vec![]).await;
        assert!(matches!(
            resolve_function(&store, "").await.expect_err("empty"),
            AstToMermaidError::InvalidInput(_)
        ));
    }

    #[tokio::test]
    async fn resolve_function_no_match_errors() {
        let store = make_store(vec![function("src/lib.rs", "foo")]).await;
        let err = resolve_function(&store, "ghost")
            .await
            .expect_err("missing");
        assert!(err.to_string().contains("no function"));
    }
}
