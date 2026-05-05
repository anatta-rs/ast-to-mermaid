//! Impact renderer — `level=impact --target=<X>`.
//!
//! Walks the reverse call chain from the target function up to N hops,
//! showing every distinct path. Useful to answer: "if I change this
//! function, who is impacted?"

use crate::error::Result;
use crate::graph::Store;
use crate::render::lookup::resolve_function;
use crate::render::util::{escape_label, mermaid_id};
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
pub fn render(store: &Store, target: &str, hops: u8) -> Result<String> {
    let target_id = resolve_function(store, target)?;
    let target_atom = store
        .get_atom(&target_id)
        .expect("resolve_function vouched the id exists");

    let (predecessors, reachable) = store.reverse_call_paths(&target_id, hops);

    // Drain the predecessor map directly — every entry is one
    // caller→callee edge in the BFS-reachable region. No path cloning,
    // no per-path reconstruction.
    let mut edges: BTreeSet<(String, String)> = BTreeSet::new();
    for (caller, callees) in &predecessors {
        for callee in callees {
            edges.insert((caller.as_str().to_owned(), callee.as_str().to_owned()));
        }
    }

    // Build node name map.
    let mut nodes: BTreeMap<String, String> = BTreeMap::new();
    nodes.insert(target_id.as_str().to_owned(), target_atom.name.clone());
    for node_id in &reachable {
        if node_id == &target_id {
            continue;
        }
        let name = store
            .get_atom(node_id)
            .map_or_else(|| "?".to_owned(), |a| a.name.clone());
        nodes.insert(node_id.as_str().to_owned(), name);
    }

    let mut mermaid = String::from("graph BT\n");
    let target_node_id = mermaid_id(target_id.as_str());
    let target_label = escape_label(&format!("fn {} (impacted)", target_atom.name));
    writeln!(mermaid, "    {target_node_id}((\"{target_label}\"))").expect("writing");
    for (id, name) in &nodes {
        if id == target_id.as_str() {
            continue;
        }
        let nid = mermaid_id(id);
        let label = escape_label(name);
        writeln!(mermaid, "    {nid}[\"{label}\"]").expect("writing");
    }
    for (from, to) in &edges {
        let fid = mermaid_id(from);
        let tid = mermaid_id(to);
        writeln!(mermaid, "    {fid} --> {tid}").expect("writing");
    }
    Ok(mermaid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AstToMermaidError;
    use crate::graph::Store;
    use crate::model::{CodeAtom, Edge, EdgeKind, EntityId};

    fn fn_atom(file_path: &str, name: &str) -> CodeAtom {
        CodeAtom {
            id: EntityId::new(format!("code:{file_path}::function::{name}")),
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

    #[test]
    fn missing_target_errors() {
        let store = Store::new();
        let err = render(&store, "ghost", 3).expect_err("must error");
        assert!(matches!(err, AstToMermaidError::InvalidInput(_)));
    }

    #[test]
    fn isolated_function_renders_only_target() {
        let store = Store::new();
        store.add_atom(fn_atom("src/lib.rs", "foo"));
        let out = render(&store, "foo", DEFAULT_HOPS).expect("render");
        assert!(out.contains("fn foo (impacted)"));
        assert_eq!(out.matches("--> ").count(), 0);
    }

    #[test]
    fn linear_chain_walks_back_n_hops() {
        let store = Store::new();
        for n in ["a", "b", "c"] {
            store.add_atom(fn_atom("src/m.rs", n));
        }
        for (from, to) in [("a", "b"), ("b", "c")] {
            let f = EntityId::new(format!("code:src/m.rs::function::{from}"));
            let t = EntityId::new(format!("code:src/m.rs::function::{to}"));
            store.add_edge(Edge::new(f, t, EdgeKind::Calls));
        }

        let out = render(&store, "c", 2).expect("render");
        assert!(out.contains("fn c (impacted)"));
        assert!(out.contains("\"a\""));
        assert!(out.contains("\"b\""));
        assert_eq!(out.matches("--> ").count(), 2);
    }

    #[test]
    fn zero_hops_renders_only_target() {
        let store = Store::new();
        store.add_atom(fn_atom("src/m.rs", "a"));
        store.add_atom(fn_atom("src/m.rs", "b"));
        let a = EntityId::new("code:src/m.rs::function::a");
        let b = EntityId::new("code:src/m.rs::function::b");
        store.add_edge(Edge::new(a, b, EdgeKind::Calls));

        let out = render(&store, "b", 0).expect("render");
        assert!(out.contains("fn b (impacted)"));
        assert!(!out.contains("\"a\""));
    }

    #[test]
    fn diamond_dedup_unique_nodes_and_edges() {
        let store = Store::new();
        for n in ["a", "b", "c", "d"] {
            store.add_atom(fn_atom("src/m.rs", n));
        }
        for (from, to) in [("a", "b"), ("a", "c"), ("b", "d"), ("c", "d")] {
            let f = EntityId::new(format!("code:src/m.rs::function::{from}"));
            let t = EntityId::new(format!("code:src/m.rs::function::{to}"));
            store.add_edge(Edge::new(f, t, EdgeKind::Calls));
        }

        let out = render(&store, "d", 2).expect("render");
        for n in ["\"a\"", "\"b\"", "\"c\""] {
            assert!(out.contains(n), "missing {n} in:\n{out}");
        }
        assert_eq!(out.matches("--> ").count(), 4);
    }

    #[test]
    fn default_hops_is_3() {
        assert_eq!(DEFAULT_HOPS, 3);
    }
}
