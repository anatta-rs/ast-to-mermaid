//! Impact renderer — `level=impact --target=<X>`.
//!
//! Walks the reverse call chain from the target function up to N hops,
//! showing every distinct path. Useful to answer: "if I change this
//! function, who is impacted?"

use crate::error::Result;
use crate::render::lookup::resolve_function;
use crate::render::util::{escape_label, mermaid_id};
use ingester_core::{Atom, Relation};
use polystore::{EntityId, GraphStore};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// Default reverse-walk depth.
pub const DEFAULT_HOPS: u8 = 3;

/// Render the impact view of `target` for `hops` reverse steps.
///
/// `hops = 0` returns just the target node. The renderer collapses
/// duplicate nodes across paths and emits one edge per unique caller →
/// callee pair seen in the BFS frontier.
///
/// # Errors
///
/// Same as [`crate::render::lookup::resolve_function`].
pub async fn render<S>(store: &S, target: &str, hops: u8) -> Result<String>
where
    S: GraphStore<Atom, Relation>,
{
    let target_id = resolve_function(store, target).await?;
    let target_atom = store
        .get_node(&target_id)
        .await?
        .expect("resolve_function vouched the id exists");

    let paths = store.reverse_path(&target_id, hops).await?;

    // Collect distinct nodes (with their atoms) and edges (caller → callee).
    let mut nodes: BTreeMap<String, Option<Atom>> = BTreeMap::new();
    let mut edges: BTreeSet<(String, String)> = BTreeSet::new();
    nodes.insert(target_id.as_str().to_owned(), Some(target_atom.clone()));

    for path in &paths {
        // path[0] is the target; path[i+1] is a caller (predecessor) of path[i].
        for window in path.windows(2) {
            let downstream = &window[0];
            let predecessor = &window[1];
            if !nodes.contains_key(predecessor.as_str()) {
                let atom = store.get_node(predecessor).await?;
                nodes.insert(predecessor.as_str().to_owned(), atom);
            }
            edges.insert((
                predecessor.as_str().to_owned(),
                downstream.as_str().to_owned(),
            ));
        }
    }

    let mut mermaid = String::from("graph BT\n");
    let target_node_id = mermaid_id(target_id.as_str());
    let target_label = escape_label(&format!("fn {} (impacted)", target_atom.name));
    writeln!(mermaid, "    {target_node_id}((\"{target_label}\"))").expect("writing");
    for (id, atom) in &nodes {
        if id == target_id.as_str() {
            continue;
        }
        let nid = mermaid_id(id);
        let label = escape_label(atom.as_ref().map_or("?", |a| a.name.as_str()));
        writeln!(mermaid, "    {nid}[\"{label}\"]").expect("writing");
    }
    for (from, to) in &edges {
        let fid = mermaid_id(from);
        let tid = mermaid_id(to);
        writeln!(mermaid, "    {fid} --> {tid}").expect("writing");
    }
    Ok(mermaid)
}

#[allow(dead_code)]
fn _ensure_id_used(_: EntityId) {}

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

    fn fn_atom(file_path: &str, name: &str) -> Atom {
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
        let err = render(&store, "ghost", 3).await.expect_err("must error");
        assert!(matches!(err, AstToMermaidError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn isolated_function_renders_only_target() {
        let store = InMemoryStore::new(scope());
        ingest_parse_output(&store, &output(vec![fn_atom("src/lib.rs", "foo")]))
            .await
            .expect("ingest");
        let out = render(&store, "foo", DEFAULT_HOPS).await.expect("render");
        assert!(out.contains("fn foo (impacted)"));
        assert_eq!(out.matches("--> ").count(), 0);
    }

    #[tokio::test]
    async fn linear_chain_walks_back_n_hops() {
        let store = InMemoryStore::new(scope());
        // a → b → c (a calls b, b calls c). Impact on c: walking back 2 hops
        // reaches a.
        ingest_parse_output(
            &store,
            &output(vec![
                fn_atom("src/m.rs", "a"),
                fn_atom("src/m.rs", "b"),
                fn_atom("src/m.rs", "c"),
            ]),
        )
        .await
        .expect("ingest");

        for (from, to) in [("a", "b"), ("b", "c")] {
            let f = EntityId::new(format!("code:src/m.rs::function::{from}"));
            let t = EntityId::new(format!("code:src/m.rs::function::{to}"));
            store
                .add_edge(
                    &f,
                    &t,
                    Relation::new(AtomId::new(from), AtomId::new(to), "calls"),
                )
                .await
                .expect("edge");
        }

        let out = render(&store, "c", 2).await.expect("render");
        assert!(out.contains("fn c (impacted)"));
        assert!(out.contains("\"a\""));
        assert!(out.contains("\"b\""));
        // Two arrows: a → b, b → c
        assert_eq!(out.matches("--> ").count(), 2);
    }

    #[tokio::test]
    async fn zero_hops_renders_only_target() {
        let store = InMemoryStore::new(scope());
        ingest_parse_output(
            &store,
            &output(vec![fn_atom("src/m.rs", "a"), fn_atom("src/m.rs", "b")]),
        )
        .await
        .expect("ingest");
        let a = EntityId::new("code:src/m.rs::function::a");
        let b = EntityId::new("code:src/m.rs::function::b");
        store
            .add_edge(
                &a,
                &b,
                Relation::new(AtomId::new("a"), AtomId::new("b"), "calls"),
            )
            .await
            .expect("edge");

        let out = render(&store, "b", 0).await.expect("render");
        assert!(out.contains("fn b (impacted)"));
        // Zero hops means no callers shown.
        assert!(!out.contains("\"a\""));
    }

    #[tokio::test]
    async fn diamond_dedup_unique_nodes_and_edges() {
        // a → b → d, a → c → d. Impact on d, hops=2: reach a via two paths
        // but a appears once.
        let store = InMemoryStore::new(scope());
        ingest_parse_output(
            &store,
            &output(vec![
                fn_atom("src/m.rs", "a"),
                fn_atom("src/m.rs", "b"),
                fn_atom("src/m.rs", "c"),
                fn_atom("src/m.rs", "d"),
            ]),
        )
        .await
        .expect("ingest");
        for (from, to) in [("a", "b"), ("a", "c"), ("b", "d"), ("c", "d")] {
            let f = EntityId::new(format!("code:src/m.rs::function::{from}"));
            let t = EntityId::new(format!("code:src/m.rs::function::{to}"));
            store
                .add_edge(
                    &f,
                    &t,
                    Relation::new(AtomId::new(from), AtomId::new(to), "calls"),
                )
                .await
                .expect("edge");
        }

        let out = render(&store, "d", 2).await.expect("render");
        // 3 unique caller-side nodes: a, b, c
        for n in ["\"a\"", "\"b\"", "\"c\""] {
            assert!(out.contains(n), "missing {n} in:\n{out}");
        }
        // 4 distinct edges (a→b, a→c, b→d, c→d).
        assert_eq!(out.matches("--> ").count(), 4);
    }

    #[test]
    fn default_hops_is_3() {
        assert_eq!(DEFAULT_HOPS, 3);
    }
}
