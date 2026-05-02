//! In-memory [`Store`] — the only storage backend needed by `ast-to-mermaid`.
//!
//! Backed by `HashMap<EntityId, CodeAtom>` for nodes and `Vec<Edge>` for
//! edges, guarded by a single `RwLock` for interior mutability.
//!
//! Designed for: tests, CLI one-shot runs, and the MVP self-bootstrap path.
//! NOT designed for: large datasets (no compaction), persistence (process-bound),
//! or concurrent writers (single writer at a time via the rwlock).

use crate::model::{CodeAtom, Edge, EdgeKind, EntityId};
use std::collections::HashMap;
use std::sync::RwLock;

// ── Store ─────────────────────────────────────────────────────────────────────

/// Lightweight in-memory graph of [`CodeAtom`]s and [`Edge`]s.
pub struct Store {
    inner: RwLock<Inner>,
}

#[derive(Default)]
struct Inner {
    atoms: HashMap<EntityId, CodeAtom>,
    edges: Vec<Edge>,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

impl Store {
    /// Construct an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner::default()),
        }
    }

    // ── Writes ────────────────────────────────────────────────────────────────

    /// Insert or replace an atom (upsert semantics).
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    pub fn add_atom(&self, atom: CodeAtom) {
        self.inner
            .write()
            .expect("rwlock not poisoned")
            .atoms
            .insert(atom.id.clone(), atom);
    }

    /// Record a directed edge.
    ///
    /// Both endpoints may be absent at insertion time — dangling edges are
    /// simply ignored by the renderers when they fetch the atoms.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    pub fn add_edge(&self, edge: Edge) {
        self.inner
            .write()
            .expect("rwlock not poisoned")
            .edges
            .push(edge);
    }

    // ── Reads ─────────────────────────────────────────────────────────────────

    /// Look up a single atom by id.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    #[must_use]
    pub fn get_atom(&self, id: &EntityId) -> Option<CodeAtom> {
        self.inner
            .read()
            .expect("rwlock not poisoned")
            .atoms
            .get(id)
            .cloned()
    }

    /// Return all atoms whose `file_path` matches `path`.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    #[must_use]
    pub fn atoms_in_file(&self, path: &str) -> Vec<CodeAtom> {
        let guard = self.inner.read().expect("rwlock not poisoned");
        let mut out: Vec<CodeAtom> = guard
            .atoms
            .values()
            .filter(|a| a.file_path == path)
            .cloned()
            .collect();
        // Deterministic order.
        out.sort_by_key(|a| a.id.clone());
        out
    }

    /// All atoms of a given kind string (e.g. `"function"`, `"module"`).
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    #[must_use]
    pub fn atoms_by_kind(&self, kind: &str) -> Vec<CodeAtom> {
        let guard = self.inner.read().expect("rwlock not poisoned");
        let mut out: Vec<CodeAtom> = guard
            .atoms
            .values()
            .filter(|a| a.kind == kind)
            .cloned()
            .collect();
        out.sort_by_key(|a| a.id.clone());
        out
    }

    /// All atoms for several kinds at once. Returns a `Vec<CodeAtom>` in a
    /// stable (id-sorted) order.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    #[must_use]
    pub fn atoms_by_kinds(&self, kinds: &[&str]) -> Vec<CodeAtom> {
        let guard = self.inner.read().expect("rwlock not poisoned");
        let mut out: Vec<CodeAtom> = guard
            .atoms
            .values()
            .filter(|a| kinds.contains(&a.kind.as_str()))
            .cloned()
            .collect();
        out.sort_by_key(|a| a.id.clone());
        out
    }

    /// All atoms (id-sorted).
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    #[must_use]
    pub fn all_atoms(&self) -> Vec<CodeAtom> {
        let guard = self.inner.read().expect("rwlock not poisoned");
        let mut out: Vec<CodeAtom> = guard.atoms.values().cloned().collect();
        out.sort_by_key(|a| a.id.clone());
        out
    }

    /// Outgoing edges from `from`, optionally filtered by kind.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    #[must_use]
    pub fn edges_from(&self, from: &EntityId) -> Vec<Edge> {
        let guard = self.inner.read().expect("rwlock not poisoned");
        guard
            .edges
            .iter()
            .filter(|e| &e.from == from)
            .cloned()
            .collect()
    }

    /// Incoming edges to `to`.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    #[must_use]
    pub fn edges_to(&self, to: &EntityId) -> Vec<Edge> {
        let guard = self.inner.read().expect("rwlock not poisoned");
        guard
            .edges
            .iter()
            .filter(|e| &e.to == to)
            .cloned()
            .collect()
    }

    /// All edges whose kind is `Calls`, outgoing from `from`.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    #[must_use]
    pub fn call_edges_from(&self, from: &EntityId) -> Vec<EntityId> {
        let guard = self.inner.read().expect("rwlock not poisoned");
        guard
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls && &e.from == from)
            .map(|e| e.to.clone())
            .collect()
    }

    /// All edges whose kind is `Calls`, incoming to `to`.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    #[must_use]
    pub fn call_edges_to(&self, to: &EntityId) -> Vec<EntityId> {
        let guard = self.inner.read().expect("rwlock not poisoned");
        guard
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Calls && &e.to == to)
            .map(|e| e.from.clone())
            .collect()
    }

    /// Items contained in `parent` (via `Contains` edges).
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    #[must_use]
    pub fn children_of(&self, parent: &EntityId) -> Vec<EntityId> {
        let guard = self.inner.read().expect("rwlock not poisoned");
        guard
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Contains && &e.from == parent)
            .map(|e| e.to.clone())
            .collect()
    }

    /// Reverse-path BFS from `from` walking `Calls` edges backwards, up to
    /// `hops` steps.
    ///
    /// Returns all paths found (each path = `[from, predecessor, …]`).
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    #[must_use]
    pub fn reverse_call_paths(&self, from: &EntityId, hops: u8) -> Vec<Vec<EntityId>> {
        use std::collections::{HashSet, VecDeque};

        if hops == 0 {
            return vec![vec![from.clone()]];
        }

        let guard = self.inner.read().expect("rwlock not poisoned");
        let mut out = Vec::new();
        let mut queue: VecDeque<Vec<EntityId>> = VecDeque::new();
        queue.push_back(vec![from.clone()]);

        while let Some(path) = queue.pop_front() {
            let depth = path.len() - 1;
            if depth >= usize::from(hops) {
                out.push(path);
                continue;
            }

            let tip = path.last().expect("path non-empty");
            let mut visited: HashSet<EntityId> = path.iter().cloned().collect();
            let mut extended = false;
            for e in &guard.edges {
                if e.kind == EdgeKind::Calls && &e.to == tip && !visited.contains(&e.from) {
                    let mut new_path = path.clone();
                    new_path.push(e.from.clone());
                    queue.push_back(new_path);
                    visited.insert(e.from.clone());
                    extended = true;
                }
            }
            if !extended {
                out.push(path);
            }
        }

        out
    }

    /// Whether a `Calls` edge already exists from `from` to `to`.
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    #[must_use]
    pub fn has_call_edge(&self, from: &EntityId, to: &EntityId) -> bool {
        let guard = self.inner.read().expect("rwlock not poisoned");
        guard
            .edges
            .iter()
            .any(|e| e.kind == EdgeKind::Calls && &e.from == from && &e.to == to)
    }

    /// Number of atoms stored (for tests / diagnostics).
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    #[must_use]
    pub fn atom_count(&self) -> usize {
        self.inner.read().expect("rwlock not poisoned").atoms.len()
    }

    /// Number of edges stored (for tests / diagnostics).
    ///
    /// # Panics
    ///
    /// Panics if the internal `RwLock` is poisoned.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.inner.read().expect("rwlock not poisoned").edges.len()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Edge, EdgeKind, EntityId};
    use pretty_assertions::assert_eq;

    fn atom(id: &str, kind: &str, name: &str, file: &str) -> CodeAtom {
        CodeAtom {
            id: EntityId::new(id),
            kind: kind.to_owned(),
            name: name.to_owned(),
            file_path: file.to_owned(),
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

    fn edge(from: &str, to: &str, kind: EdgeKind) -> Edge {
        Edge::new(EntityId::new(from), EntityId::new(to), kind)
    }

    #[test]
    fn new_store_is_empty() {
        let store = Store::new();
        assert_eq!(store.atom_count(), 0);
        assert_eq!(store.edge_count(), 0);
    }

    #[test]
    fn add_then_get_atom_roundtrips() {
        let store = Store::new();
        let a = atom(
            "code:src/lib.rs::function::foo",
            "function",
            "foo",
            "src/lib.rs",
        );
        store.add_atom(a.clone());
        let got = store
            .get_atom(&EntityId::new("code:src/lib.rs::function::foo"))
            .expect("present");
        assert_eq!(got.name, "foo");
    }

    #[test]
    fn add_atom_upserts() {
        let store = Store::new();
        store.add_atom(atom("code:x", "function", "first", "src/a.rs"));
        store.add_atom(atom("code:x", "function", "second", "src/a.rs"));
        assert_eq!(store.atom_count(), 1);
        assert_eq!(
            store
                .get_atom(&EntityId::new("code:x"))
                .expect("present")
                .name,
            "second"
        );
    }

    #[test]
    fn atoms_by_kind_filters_correctly() {
        let store = Store::new();
        store.add_atom(atom("code:a", "function", "a", "src/a.rs"));
        store.add_atom(atom("code:b", "module", "b", "src/b.rs"));
        store.add_atom(atom("code:c", "function", "c", "src/c.rs"));
        let fns = store.atoms_by_kind("function");
        assert_eq!(fns.len(), 2);
        let mods = store.atoms_by_kind("module");
        assert_eq!(mods.len(), 1);
    }

    #[test]
    fn atoms_in_file_filters_by_path() {
        let store = Store::new();
        store.add_atom(atom("code:1", "function", "f", "src/a.rs"));
        store.add_atom(atom("code:2", "function", "g", "src/b.rs"));
        let in_a = store.atoms_in_file("src/a.rs");
        assert_eq!(in_a.len(), 1);
        assert_eq!(in_a[0].name, "f");
    }

    #[test]
    fn edges_from_and_to_work() {
        let store = Store::new();
        store.add_edge(edge("a", "b", EdgeKind::Calls));
        store.add_edge(edge("a", "c", EdgeKind::Contains));
        let from_a = store.edges_from(&EntityId::new("a"));
        assert_eq!(from_a.len(), 2);
        let to_b = store.edges_to(&EntityId::new("b"));
        assert_eq!(to_b.len(), 1);
    }

    #[test]
    fn call_edges_from_and_to_filter_by_kind() {
        let store = Store::new();
        store.add_edge(edge("a", "b", EdgeKind::Calls));
        store.add_edge(edge("a", "c", EdgeKind::Contains));
        store.add_edge(edge("d", "b", EdgeKind::Calls));
        let calls_from_a = store.call_edges_from(&EntityId::new("a"));
        assert_eq!(calls_from_a.len(), 1);
        assert_eq!(calls_from_a[0].as_str(), "b");
        let calls_to_b = store.call_edges_to(&EntityId::new("b"));
        assert_eq!(calls_to_b.len(), 2);
    }

    #[test]
    fn children_of_returns_contains_targets() {
        let store = Store::new();
        store.add_edge(edge("mod", "fn1", EdgeKind::Contains));
        store.add_edge(edge("mod", "fn2", EdgeKind::Contains));
        store.add_edge(edge("mod", "ext", EdgeKind::Calls));
        let children = store.children_of(&EntityId::new("mod"));
        assert_eq!(children.len(), 2);
    }

    #[test]
    fn reverse_call_paths_zero_hops_returns_self() {
        let store = Store::new();
        let paths = store.reverse_call_paths(&EntityId::new("c"), 0);
        assert_eq!(paths, vec![vec![EntityId::new("c")]]);
    }

    #[test]
    fn reverse_call_paths_walks_back() {
        let store = Store::new();
        // a → b → c
        store.add_edge(edge("a", "b", EdgeKind::Calls));
        store.add_edge(edge("b", "c", EdgeKind::Calls));
        let paths = store.reverse_call_paths(&EntityId::new("c"), 2);
        let has_full = paths.iter().any(|p| {
            p.len() == 3 && p[0].as_str() == "c" && p[1].as_str() == "b" && p[2].as_str() == "a"
        });
        assert!(has_full, "expected c→b→a path, got {paths:?}");
    }

    #[test]
    fn reverse_call_paths_cycle_terminates() {
        let store = Store::new();
        store.add_edge(edge("a", "b", EdgeKind::Calls));
        store.add_edge(edge("b", "a", EdgeKind::Calls));
        // Must terminate, not loop forever.
        let paths = store.reverse_call_paths(&EntityId::new("a"), 3);
        for p in &paths {
            let ids: std::collections::HashSet<&EntityId> = p.iter().collect();
            assert_eq!(ids.len(), p.len(), "cycle in path: {p:?}");
        }
    }

    #[test]
    fn has_call_edge_checks_existence() {
        let store = Store::new();
        let a = EntityId::new("a");
        let b = EntityId::new("b");
        assert!(!store.has_call_edge(&a, &b));
        store.add_edge(edge("a", "b", EdgeKind::Calls));
        assert!(store.has_call_edge(&a, &b));
        assert!(!store.has_call_edge(&b, &a));
    }
}
