//! Acceptance test for issue #80 (C15): a panic in one writer must not
//! cascade-kill subsequent accessors on the same `Store`.
//!
//! We poison the internal `RwLock` by spawning a thread that grabs the
//! write lock and panics. After joining the thread (`Result::Err`), every
//! accessor on the main thread must keep working — `Store` recovers via
//! `unwrap_or_else(|e| e.into_inner())` instead of `.unwrap()`.

use std::sync::Arc;
use std::thread;

use ast_to_mermaid::Store;
use ast_to_mermaid::model::{CodeAtom, Edge, EdgeKind, EntityId};

fn atom(id: &str) -> CodeAtom {
    CodeAtom {
        id: EntityId::new(id),
        kind: "function".to_owned(),
        name: id.to_owned(),
        file_path: "src/lib.rs".to_owned(),
        line_start: 1,
        line_end: 10,
        doc: String::new(),
        signature: String::new(),
        content_hash: "deadbeef".to_owned(),
        calls: Vec::new(),
        method_calls: Vec::new(),
        parent: None,
    }
}

#[test]
fn store_lock_poison_recovers() {
    let store = Arc::new(Store::new());
    store.add_atom(atom("a"));
    store.add_edge(Edge::new(
        EntityId::new("a"),
        EntityId::new("b"),
        EdgeKind::Calls,
    ));

    // Spawn a writer that panics with the write lock held → the lock is
    // poisoned for the rest of the process.
    let s = Arc::clone(&store);
    let join = thread::spawn(move || {
        s.__poison_lock_for_tests();
    });
    let join_result = join.join();
    assert!(join_result.is_err(), "poisoner thread should have panicked");

    // Every accessor must still work — no `.unwrap()` panic on the
    // poisoned lock.
    assert_eq!(store.atom_count(), 1);
    assert_eq!(store.edge_count(), 1);
    assert!(
        store.get_atom(&EntityId::new("a")).is_some(),
        "get_atom must recover"
    );
    assert_eq!(
        store.atoms_in_file("src/lib.rs").len(),
        1,
        "atoms_in_file must recover"
    );
    assert_eq!(
        store.atoms_by_kind("function").len(),
        1,
        "atoms_by_kind must recover"
    );
    assert_eq!(
        store.atoms_by_kinds(&["function"]).len(),
        1,
        "atoms_by_kinds must recover"
    );
    assert_eq!(store.all_atoms().len(), 1, "all_atoms must recover");
    assert_eq!(
        store.edges_from(&EntityId::new("a")).len(),
        1,
        "edges_from must recover"
    );
    assert_eq!(
        store.edges_to(&EntityId::new("b")).len(),
        1,
        "edges_to must recover"
    );
    assert_eq!(
        store.call_edges_from(&EntityId::new("a")).len(),
        1,
        "call_edges_from must recover"
    );
    assert_eq!(
        store.call_edges_to(&EntityId::new("b")).len(),
        1,
        "call_edges_to must recover"
    );
    assert_eq!(
        store.children_of(&EntityId::new("a")).len(),
        0,
        "children_of must recover"
    );
    assert!(
        store.has_call_edge(&EntityId::new("a"), &EntityId::new("b")),
        "has_call_edge must recover"
    );
    assert_eq!(store.all_edges().len(), 1, "all_edges must recover");
    let (_preds, reachable) = store.reverse_call_paths(&EntityId::new("b"), 2);
    assert!(
        reachable.contains(&EntityId::new("a")),
        "reverse_call_paths must recover"
    );

    // Subsequent writes must also recover.
    store.add_atom(atom("c"));
    assert_eq!(store.atom_count(), 2, "add_atom must recover");
    store.add_edge(Edge::new(
        EntityId::new("a"),
        EntityId::new("c"),
        EdgeKind::Calls,
    ));
    assert_eq!(store.edge_count(), 2, "add_edge must recover");
}
