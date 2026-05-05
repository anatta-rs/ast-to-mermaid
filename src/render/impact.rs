//! Impact renderer — `level=impact --target=<X>`.
//!
//! Walks the reverse call chain from the target function up to N hops,
//! showing every distinct path. Useful to answer: "if I change this
//! function, who is impacted?"

use crate::error::Result;
use crate::model::EntityId;
use crate::render::AdjMaps;
use crate::render::lookup::resolve_function;
use crate::render::snapshot::AtomSnapshot;
use crate::render::util::{escape_label_flowchart, sanitize_id};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

/// Default reverse-walk depth.
pub const DEFAULT_HOPS: u8 = 3;

/// Render the impact view of `target` for `hops` reverse steps.
///
/// `hops = 0` returns just the target node. The renderer collapses
/// duplicate nodes across paths and emits one edge per unique caller →
/// callee pair seen in the BFS frontier.
///
/// `adj` supplies the reverse `Calls` adjacency that the BFS walks back
/// from `target` — same map the bundle's other levels share.
///
/// `snapshot` is the borrowed `id → &CodeAtom` view: every per-caller
/// name lookup is an O(1) `HashMap` probe with no `RwLock` traffic and no
/// [`crate::model::CodeAtom`] clones.
///
/// # Errors
///
/// Same as [`crate::render::lookup::resolve_function`].
pub fn render(
    adj: &AdjMaps,
    snapshot: &AtomSnapshot<'_>,
    target: &str,
    hops: u8,
) -> Result<String> {
    let target_id = resolve_function(snapshot, target)?;
    let target_atom = snapshot
        .get(&target_id)
        .expect("resolve_function vouched the id exists");

    // BFS over `adj.callers` from `target_id`. Per-path-visited semantics
    // mirror `Store::reverse_call_paths`: a caller can appear in the
    // reachable region multiple times via different paths, but the same
    // caller cannot reappear along a single BFS-tree path back to the
    // root (otherwise we'd render an impossible self-loop along that
    // chain).
    let mut edges: BTreeSet<(String, String)> = BTreeSet::new();
    let mut nodes: BTreeMap<String, String> = BTreeMap::new();
    nodes.insert(target_id.as_str().to_owned(), target_atom.name.clone());

    let mut visited: HashSet<EntityId> = HashSet::new();
    visited.insert(target_id.clone());
    let mut bfs_pred: HashMap<EntityId, EntityId> = HashMap::new();
    let mut frontier: Vec<EntityId> = vec![target_id.clone()];

    for _ in 0..usize::from(hops) {
        let mut next_frontier: Vec<EntityId> = Vec::new();
        for node in &frontier {
            for caller_arc in adj.callers(node) {
                let caller: &EntityId = caller_arc;
                if caller == &target_id {
                    continue;
                }
                if path_contains(&bfs_pred, node, caller) {
                    continue;
                }
                edges.insert((caller.as_str().to_owned(), node.as_str().to_owned()));
                if visited.insert(caller.clone()) {
                    bfs_pred.insert(caller.clone(), node.clone());
                    let name = snapshot
                        .get(caller)
                        .map_or_else(|| "?".to_owned(), |a| a.name.clone());
                    nodes.insert(caller.as_str().to_owned(), name);
                    next_frontier.push(caller.clone());
                }
            }
        }
        if next_frontier.is_empty() {
            break;
        }
        frontier = next_frontier;
    }

    let mut mermaid = String::from("graph BT\n");
    let target_node_id = sanitize_id(target_id.as_str());
    let target_label = escape_label_flowchart(&format!("fn {} (impacted)", target_atom.name));
    writeln!(mermaid, "    {target_node_id}((\"{target_label}\"))")
        .expect("string write is infallible");
    for (id, name) in &nodes {
        if id == target_id.as_str() {
            continue;
        }
        let nid = sanitize_id(id);
        let label = escape_label_flowchart(name);
        writeln!(mermaid, "    {nid}[\"{label}\"]").expect("string write is infallible");
    }
    for (from, to) in &edges {
        let fid = sanitize_id(from);
        let tid = sanitize_id(to);
        writeln!(mermaid, "    {fid} --> {tid}").expect("string write is infallible");
    }
    Ok(mermaid)
}

/// Whether `needle` appears on the BFS spanning-tree path from `start`
/// back to the BFS root. Local copy of the helper that
/// `Store::reverse_call_paths` uses internally — kept here so the
/// adjacency-driven BFS does not need to reach back into the store.
fn path_contains(
    bfs_pred: &HashMap<EntityId, EntityId>,
    start: &EntityId,
    needle: &EntityId,
) -> bool {
    let mut cur = start;
    loop {
        if cur == needle {
            return true;
        }
        match bfs_pred.get(cur) {
            Some(next) => cur = next,
            None => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AstToMermaidError;
    use crate::graph::Store;
    use crate::model::{CodeAtom, Edge, EdgeKind, EntityId};

    fn run(store: &Store, target: &str, hops: u8) -> Result<String> {
        let adj = AdjMaps::build(store);
        store.with_atoms(|atoms| {
            let snap = AtomSnapshot::build(atoms);
            render(&adj, &snap, target, hops)
        })
    }

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
        let err = run(&store, "ghost", 3).expect_err("must error");
        assert!(matches!(err, AstToMermaidError::InvalidInput(_)));
    }

    #[test]
    fn isolated_function_renders_only_target() {
        let store = Store::new();
        store.add_atom(fn_atom("src/lib.rs", "foo"));
        let out = run(&store, "foo", DEFAULT_HOPS).expect("render");
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

        let out = run(&store, "c", 2).expect("render");
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

        let out = run(&store, "b", 0).expect("render");
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

        let out = run(&store, "d", 2).expect("render");
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
