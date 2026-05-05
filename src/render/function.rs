//! Function-zoom renderer — `level=function --target=<X>`.
//!
//! Centers a single function and shows its direct callers (incoming) and
//! callees (outgoing).

use crate::error::Result;
use crate::model::EntityId;
use crate::render::AdjMaps;
use crate::render::lookup::resolve_function;
use crate::render::snapshot::AtomSnapshot;
use crate::render::util::{escape_label_flowchart, sanitize_id};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Render the function-zoom view of `target` against `snapshot`.
///
/// `adj` supplies the shared forward + reverse `Calls` adjacency — the only
/// graph data this view needs once `resolve_function` has located the
/// center.
///
/// `snapshot` is the borrowed `id → &CodeAtom` view: each direct caller /
/// callee lookup is an O(1) `HashMap` probe with no `RwLock` traffic and
/// no [`crate::model::CodeAtom`] clones.
///
/// # Errors
///
/// Same as [`crate::render::lookup::resolve_function`].
pub fn render(adj: &AdjMaps, snapshot: &AtomSnapshot<'_>, target: &str) -> Result<String> {
    let center_id = resolve_function(snapshot, target)?;
    let center_atom = snapshot
        .get(&center_id)
        .expect("resolve_function vouched the id exists");

    let mut who_calls: BTreeMap<String, String> = BTreeMap::new(); // id → name
    for caller_arc in adj.callers(&center_id) {
        let caller_id: &EntityId = caller_arc;
        if let Some(atom) = snapshot.get(caller_id) {
            who_calls.insert(caller_id.as_str().to_owned(), atom.name.clone());
        }
    }

    let mut whom_called: BTreeMap<String, String> = BTreeMap::new(); // id → name
    for callee_arc in adj.callees(&center_id) {
        let callee_id: &EntityId = callee_arc;
        if let Some(atom) = snapshot.get(callee_id) {
            whom_called.insert(callee_id.as_str().to_owned(), atom.name.clone());
        }
    }

    let target_node_id = sanitize_id(center_id.as_str());
    let target_label = escape_label_flowchart(&format!("fn {} (target)", center_atom.name));

    let mut mermaid = String::from("graph TD\n");
    writeln!(mermaid, "    {target_node_id}((\"{target_label}\"))")
        .expect("string write is infallible");

    for (caller_id, caller_name) in &who_calls {
        let id = sanitize_id(caller_id);
        let label = escape_label_flowchart(caller_name);
        writeln!(mermaid, "    {id}[\"{label}\"]").expect("string write is infallible");
        writeln!(mermaid, "    {id} --> {target_node_id}").expect("string write is infallible");
    }
    for (callee_id, callee_name) in &whom_called {
        let id = sanitize_id(callee_id);
        let label = escape_label_flowchart(callee_name);
        writeln!(mermaid, "    {id}[\"{label}\"]").expect("string write is infallible");
        writeln!(mermaid, "    {target_node_id} --> {id}").expect("string write is infallible");
    }

    Ok(mermaid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AstToMermaidError;
    use crate::graph::Store;
    use crate::model::{CodeAtom, Edge, EdgeKind, EntityId};

    fn run(store: &Store, target: &str) -> Result<String> {
        let adj = AdjMaps::build(store);
        store.with_atoms(|atoms| {
            let snap = AtomSnapshot::build(atoms);
            render(&adj, &snap, target)
        })
    }

    fn function_atom(file_path: &str, name: &str) -> CodeAtom {
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
        let err = run(&store, "ghost").expect_err("must error");
        assert!(matches!(err, AstToMermaidError::InvalidInput(_)));
    }

    #[test]
    fn empty_target_errors() {
        let store = Store::new();
        let err = run(&store, "").expect_err("must error");
        assert!(matches!(err, AstToMermaidError::InvalidInput(_)));
    }

    #[test]
    fn isolated_function_renders_only_target() {
        let store = Store::new();
        store.add_atom(function_atom("src/lib.rs", "foo"));
        let out = run(&store, "foo").expect("render");
        assert!(out.contains("fn foo (target)"));
        assert_eq!(out.matches("-->").count(), 0);
    }

    #[test]
    fn callers_appear_with_arrows_in() {
        let store = Store::new();
        store.add_atom(function_atom("src/a.rs", "caller_one"));
        store.add_atom(function_atom("src/a.rs", "caller_two"));
        store.add_atom(function_atom("src/lib.rs", "target"));

        let target_id = EntityId::new("code:src/lib.rs::function::target");
        for caller in ["caller_one", "caller_two"] {
            let cid = EntityId::new(format!("code:src/a.rs::function::{caller}"));
            store.add_edge(Edge::new(cid, target_id.clone(), EdgeKind::Calls));
        }

        let out = run(&store, "target").expect("render");
        assert!(out.contains("caller_one"));
        assert!(out.contains("caller_two"));
        assert_eq!(out.matches("--> ").count(), 2);
    }

    #[test]
    fn callees_appear_with_arrows_out() {
        let store = Store::new();
        store.add_atom(function_atom("src/lib.rs", "target"));
        store.add_atom(function_atom("src/b.rs", "helper_one"));
        store.add_atom(function_atom("src/b.rs", "helper_two"));

        let target_id = EntityId::new("code:src/lib.rs::function::target");
        for callee in ["helper_one", "helper_two"] {
            let cid = EntityId::new(format!("code:src/b.rs::function::{callee}"));
            store.add_edge(Edge::new(target_id.clone(), cid, EdgeKind::Calls));
        }

        let out = run(&store, "target").expect("render");
        assert!(out.contains("helper_one"));
        assert!(out.contains("helper_two"));
        assert_eq!(out.matches("--> ").count(), 2);
    }

    #[test]
    fn non_calls_relations_ignored() {
        let store = Store::new();
        store.add_atom(function_atom("src/lib.rs", "target"));
        store.add_atom(function_atom("src/b.rs", "noise"));
        let t = EntityId::new("code:src/lib.rs::function::target");
        let n = EntityId::new("code:src/b.rs::function::noise");
        store.add_edge(Edge::new(t, n, EdgeKind::Uses));

        let out = run(&store, "target").expect("render");
        assert!(!out.contains("noise"));
    }
}
