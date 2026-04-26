//! Function-zoom renderer — `level=function --target=<X>`.
//!
//! Centers a single function and shows its direct callers (incoming) and
//! callees (outgoing).

use crate::error::Result;
use crate::render::lookup::resolve_function;
use crate::render::util::{escape_label, mermaid_id};
use ingester_core::{Atom, Relation};
use polystore::{Direction, GraphStore};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Render the function-zoom view of `target` against `store`.
///
/// # Errors
///
/// Same as [`crate::render::lookup::resolve_function`].
pub async fn render<S>(store: &S, target: &str) -> Result<String>
where
    S: GraphStore<Atom, Relation>,
{
    let center_id = resolve_function(store, target).await?;
    let center_atom = store
        .get_node(&center_id)
        .await?
        .expect("resolve_function vouched the id exists");

    let mut who_calls: BTreeMap<String, Atom> = BTreeMap::new();
    for (other_id, rel) in store.neighbors(&center_id, Direction::Incoming).await? {
        if rel.kind != "calls" {
            continue;
        }
        if let Some(atom) = store.get_node(&other_id).await? {
            who_calls.insert(other_id.as_str().to_owned(), atom);
        }
    }

    let mut whom_called: BTreeMap<String, Atom> = BTreeMap::new();
    for (other_id, rel) in store.neighbors(&center_id, Direction::Outgoing).await? {
        if rel.kind != "calls" {
            continue;
        }
        if let Some(atom) = store.get_node(&other_id).await? {
            whom_called.insert(other_id.as_str().to_owned(), atom);
        }
    }

    let target_node_id = mermaid_id(center_id.as_str());
    let target_label = escape_label(&format!("fn {} (target)", center_atom.name));

    let mut mermaid = String::from("graph TD\n");
    writeln!(mermaid, "    {target_node_id}((\"{target_label}\"))").expect("writing");

    for (caller_id, caller_atom) in &who_calls {
        let id = mermaid_id(caller_id);
        let label = escape_label(&caller_atom.name);
        writeln!(mermaid, "    {id}[\"{label}\"]").expect("writing");
        writeln!(mermaid, "    {id} --> {target_node_id}").expect("writing");
    }
    for (callee_id, callee_atom) in &whom_called {
        let id = mermaid_id(callee_id);
        let label = escape_label(&callee_atom.name);
        writeln!(mermaid, "    {id}[\"{label}\"]").expect("writing");
        writeln!(mermaid, "    {target_node_id} --> {id}").expect("writing");
    }

    Ok(mermaid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AstToMermaidError;
    use crate::store::{InMemoryStore, ingest_parse_output};
    use ingester_core::{AtomId, ParseOutput};
    use polystore::{EntityId, Scope};

    fn scope() -> Scope {
        Scope::new("ns", "repo", "branch")
    }

    fn function_atom(file_path: &str, name: &str) -> Atom {
        Atom::new(
            AtomId::new(format!("code:{file_path}::function::{name}")),
            "function",
            name,
            "h",
        )
        .with_metadata(serde_json::json!({"file_path": file_path}))
    }

    fn output(atoms: Vec<Atom>) -> ParseOutput {
        ParseOutput {
            atoms,
            ..ParseOutput::default()
        }
    }

    #[tokio::test]
    async fn missing_target_errors() {
        let store = InMemoryStore::new(scope());
        let err = render(&store, "ghost").await.expect_err("must error");
        assert!(matches!(err, AstToMermaidError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn empty_target_errors() {
        let store = InMemoryStore::new(scope());
        let err = render(&store, "").await.expect_err("must error");
        assert!(matches!(err, AstToMermaidError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn isolated_function_renders_only_target() {
        let store = InMemoryStore::new(scope());
        ingest_parse_output(&store, &output(vec![function_atom("src/lib.rs", "foo")]))
            .await
            .expect("ingest");
        let out = render(&store, "foo").await.expect("render");
        assert!(out.contains("fn foo (target)"));
        // No --> arrows when target has neither callers nor callees.
        assert_eq!(out.matches("-->").count(), 0);
    }

    #[tokio::test]
    async fn callers_appear_with_arrows_in() {
        let store = InMemoryStore::new(scope());
        ingest_parse_output(
            &store,
            &output(vec![
                function_atom("src/a.rs", "caller_one"),
                function_atom("src/a.rs", "caller_two"),
                function_atom("src/lib.rs", "target"),
            ]),
        )
        .await
        .expect("ingest");

        let target_id = EntityId::new("code:src/lib.rs::function::target");
        for caller in ["caller_one", "caller_two"] {
            let cid = EntityId::new(format!("code:src/a.rs::function::{caller}"));
            store
                .add_edge(
                    &cid,
                    &target_id,
                    Relation::new(AtomId::new("c"), AtomId::new("t"), "calls"),
                )
                .await
                .expect("edge");
        }

        let out = render(&store, "target").await.expect("render");
        assert!(out.contains("caller_one"));
        assert!(out.contains("caller_two"));
        // 2 incoming arrows
        assert_eq!(out.matches("--> ").count(), 2);
    }

    #[tokio::test]
    async fn callees_appear_with_arrows_out() {
        let store = InMemoryStore::new(scope());
        ingest_parse_output(
            &store,
            &output(vec![
                function_atom("src/lib.rs", "target"),
                function_atom("src/b.rs", "helper_one"),
                function_atom("src/b.rs", "helper_two"),
            ]),
        )
        .await
        .expect("ingest");

        let target_id = EntityId::new("code:src/lib.rs::function::target");
        for callee in ["helper_one", "helper_two"] {
            let cid = EntityId::new(format!("code:src/b.rs::function::{callee}"));
            store
                .add_edge(
                    &target_id,
                    &cid,
                    Relation::new(AtomId::new("t"), AtomId::new("h"), "calls"),
                )
                .await
                .expect("edge");
        }

        let out = render(&store, "target").await.expect("render");
        assert!(out.contains("helper_one"));
        assert!(out.contains("helper_two"));
        assert_eq!(out.matches("--> ").count(), 2);
    }

    #[tokio::test]
    async fn non_calls_relations_ignored() {
        let store = InMemoryStore::new(scope());
        ingest_parse_output(
            &store,
            &output(vec![
                function_atom("src/lib.rs", "target"),
                function_atom("src/b.rs", "noise"),
            ]),
        )
        .await
        .expect("ingest");
        let t = EntityId::new("code:src/lib.rs::function::target");
        let n = EntityId::new("code:src/b.rs::function::noise");
        store
            .add_edge(
                &t,
                &n,
                Relation::new(AtomId::new("t"), AtomId::new("n"), "uses"),
            )
            .await
            .expect("edge");

        let out = render(&store, "target").await.expect("render");
        // `noise` should NOT appear because the edge is `uses`, not `calls`.
        assert!(!out.contains("noise"));
    }
}
