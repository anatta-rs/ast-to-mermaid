//! In-memory [`Store`] — the only storage backend needed by `ast-to-mermaid`.
//!
//! Backed by `HashMap<EntityId, CodeAtom>` for nodes and `Vec<Edge>` for
//! edges, guarded by a single `RwLock` for interior mutability.
//!
//! Forward and reverse adjacency indices (`forward_idx`, `reverse_idx`)
//! map each endpoint to the indices of its incident edges in the `edges`
//! vector. This makes the six edge accessors O(degree) instead of O(E),
//! which matters once an edge list grows past a few thousand entries.
//!
//! Designed for: tests, CLI one-shot runs, and the MVP self-bootstrap path.
//! NOT designed for: persistence (process-bound) or concurrent writers
//! (single writer at a time via the rwlock).
//!
//! Invariant: `forward_idx[e.from]` and `reverse_idx[e.to]` both contain
//! the index of `e` in `edges`, for every edge. The only mutation point
//! that maintains it is [`Store::add_edge`]; a `pub(crate)` escape hatch
//! [`Store::rebuild_indices`] recomputes both maps from scratch for use
//! by future bulk-load paths.
//!
//! Poison recovery: every accessor takes the lock via [`Store::read_or_recover`]
//! / [`Store::write_or_recover`], which fall back to `PoisonError::into_inner`
//! when the lock is poisoned. We trust the inner state across a poison
//! because every mutation is a single, self-contained operation: `add_atom`
//! is one `HashMap::insert`, `add_edge` is one `Vec::push` plus two
//! `HashMap::entry().or_default().push()` — no compound update can be torn
//! apart by a panic in the middle. A poisoned guard therefore observes a
//! state that is either fully pre-mutation or fully post-mutation, never
//! halfway. This lets a panic in one writer not cascade-kill every
//! subsequent reader.

use crate::model::{CodeAtom, Edge, EdgeKind, EntityId};
use std::collections::HashMap;
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

// ── Store ─────────────────────────────────────────────────────────────────────

/// Lightweight in-memory graph of [`CodeAtom`]s and [`Edge`]s.
pub struct Store {
    inner: RwLock<Inner>,
}

#[derive(Default)]
struct Inner {
    atoms: HashMap<EntityId, CodeAtom>,
    edges: Vec<Edge>,
    forward_idx: HashMap<EntityId, Vec<usize>>,
    reverse_idx: HashMap<EntityId, Vec<usize>>,
}

impl Inner {
    #[allow(dead_code)] // reserved: see Store::rebuild_indices
    fn rebuild_indices(&mut self) {
        self.forward_idx.clear();
        self.reverse_idx.clear();
        for (i, e) in self.edges.iter().enumerate() {
            self.forward_idx.entry(e.from.clone()).or_default().push(i);
            self.reverse_idx.entry(e.to.clone()).or_default().push(i);
        }
    }
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

    // ── Lock helpers ──────────────────────────────────────────────────────────

    /// Acquire a read guard, recovering from a poisoned lock.
    ///
    /// Equivalent to `RwLock::read().unwrap_or_else(PoisonError::into_inner)`.
    /// See the module docs for why partial mutations are impossible and
    /// the recovered state is therefore safe to read.
    fn read_or_recover(&self) -> RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Acquire a write guard, recovering from a poisoned lock.
    ///
    /// Equivalent to `RwLock::write().unwrap_or_else(PoisonError::into_inner)`.
    /// Does not re-poison: each mutation here is a single self-contained
    /// op (see module docs), so subsequent writers see a consistent state.
    fn write_or_recover(&self) -> RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(PoisonError::into_inner)
    }

    // ── Writes ────────────────────────────────────────────────────────────────

    /// Insert or replace an atom (upsert semantics).
    pub fn add_atom(&self, atom: CodeAtom) {
        self.write_or_recover().atoms.insert(atom.id.clone(), atom);
    }

    /// Record a directed edge.
    ///
    /// Both endpoints may be absent at insertion time — dangling edges are
    /// simply ignored by the renderers when they fetch the atoms.
    ///
    /// This is the sole mutation point for `forward_idx` / `reverse_idx`:
    /// the new edge's index is appended to both maps in lockstep with the
    /// `edges` push, so the invariant described at the module level holds.
    pub fn add_edge(&self, edge: Edge) {
        let mut guard = self.write_or_recover();
        let idx = guard.edges.len();
        let from = edge.from.clone();
        let to = edge.to.clone();
        guard.edges.push(edge);
        guard.forward_idx.entry(from).or_default().push(idx);
        guard.reverse_idx.entry(to).or_default().push(idx);
    }

    /// Recompute `forward_idx` and `reverse_idx` from `edges`.
    ///
    /// Reserved for future bulk-load paths (e.g. bundle reconstruction)
    /// that may want to populate `edges` directly. Not used by `add_edge`,
    /// which maintains the maps incrementally.
    #[allow(dead_code)] // reserved for bundle reconstruction; see module docs
    pub(crate) fn rebuild_indices(&self) {
        self.write_or_recover().rebuild_indices();
    }

    // ── Reads ─────────────────────────────────────────────────────────────────

    /// Look up a single atom by id.
    #[must_use]
    pub fn get_atom(&self, id: &EntityId) -> Option<CodeAtom> {
        self.read_or_recover().atoms.get(id).cloned()
    }

    /// Return all atoms whose `file_path` matches `path`.
    #[must_use]
    pub fn atoms_in_file(&self, path: &str) -> Vec<CodeAtom> {
        let guard = self.read_or_recover();
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
    #[must_use]
    pub fn atoms_by_kind(&self, kind: &str) -> Vec<CodeAtom> {
        let guard = self.read_or_recover();
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
    #[must_use]
    pub fn atoms_by_kinds(&self, kinds: &[&str]) -> Vec<CodeAtom> {
        let guard = self.read_or_recover();
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
    #[must_use]
    pub fn all_atoms(&self) -> Vec<CodeAtom> {
        let guard = self.read_or_recover();
        let mut out: Vec<CodeAtom> = guard.atoms.values().cloned().collect();
        out.sort_by_key(|a| a.id.clone());
        out
    }

    /// Outgoing edges from `from`, optionally filtered by kind.
    #[must_use]
    pub fn edges_from(&self, from: &EntityId) -> Vec<Edge> {
        let guard = self.read_or_recover();
        match guard.forward_idx.get(from) {
            Some(idxs) => idxs.iter().map(|&i| guard.edges[i].clone()).collect(),
            None => Vec::new(),
        }
    }

    /// Incoming edges to `to`.
    #[must_use]
    pub fn edges_to(&self, to: &EntityId) -> Vec<Edge> {
        let guard = self.read_or_recover();
        match guard.reverse_idx.get(to) {
            Some(idxs) => idxs.iter().map(|&i| guard.edges[i].clone()).collect(),
            None => Vec::new(),
        }
    }

    /// All edges whose kind is `Calls`, outgoing from `from`.
    #[must_use]
    pub fn call_edges_from(&self, from: &EntityId) -> Vec<EntityId> {
        let guard = self.read_or_recover();
        let Some(idxs) = guard.forward_idx.get(from) else {
            return Vec::new();
        };
        idxs.iter()
            .map(|&i| &guard.edges[i])
            .filter(|e| e.kind == EdgeKind::Calls)
            .map(|e| e.to.clone())
            .collect()
    }

    /// All edges whose kind is `Calls`, incoming to `to`.
    #[must_use]
    pub fn call_edges_to(&self, to: &EntityId) -> Vec<EntityId> {
        let guard = self.read_or_recover();
        let Some(idxs) = guard.reverse_idx.get(to) else {
            return Vec::new();
        };
        idxs.iter()
            .map(|&i| &guard.edges[i])
            .filter(|e| e.kind == EdgeKind::Calls)
            .map(|e| e.from.clone())
            .collect()
    }

    /// Items contained in `parent` (via `Contains` edges).
    #[must_use]
    pub fn children_of(&self, parent: &EntityId) -> Vec<EntityId> {
        let guard = self.read_or_recover();
        let Some(idxs) = guard.forward_idx.get(parent) else {
            return Vec::new();
        };
        idxs.iter()
            .map(|&i| &guard.edges[i])
            .filter(|e| e.kind == EdgeKind::Contains)
            .map(|e| e.to.clone())
            .collect()
    }

    /// Reverse-path BFS from `target` walking `Calls` edges backwards, up to
    /// `hops` steps.
    ///
    /// Returns `(predecessors, reachable)`:
    ///
    /// - `predecessors[caller]` is the list of nodes (closer to `target`)
    ///   that `caller` calls within the BFS-reachable region. Walking these
    ///   in any order from a node leads back to `target`.
    /// - `reachable` is the set of nodes within `hops` reverse-call-distance
    ///   of `target`, in BFS order with `target` first.
    ///
    /// The map only ever stores at most `O(E_reachable)` entries — there is
    /// no path cloning, so a high fan-in target with `hops = 3` no longer
    /// blows up to `F^3` cloned `Vec<EntityId>` paths. Callers that need
    /// individual paths can use [`reconstruct_path`] to walk the map.
    #[must_use]
    pub fn reverse_call_paths(
        &self,
        target: &EntityId,
        hops: u8,
    ) -> (HashMap<EntityId, Vec<EntityId>>, Vec<EntityId>) {
        use std::collections::HashSet;

        let mut predecessors: HashMap<EntityId, Vec<EntityId>> = HashMap::new();
        let mut reachable: Vec<EntityId> = vec![target.clone()];

        if hops == 0 {
            return (predecessors, reachable);
        }

        let guard = self.read_or_recover();
        let mut visited: HashSet<EntityId> = HashSet::new();
        visited.insert(target.clone());
        // BFS spanning tree: each visited node remembers the first node
        // that discovered it. Used to test simple-path reachability so
        // that back-edges into the current path are skipped, matching
        // the legacy per-path visited semantics.
        let mut bfs_pred: HashMap<EntityId, EntityId> = HashMap::new();
        let mut frontier: Vec<EntityId> = vec![target.clone()];

        for _ in 0..usize::from(hops) {
            let mut next_frontier: Vec<EntityId> = Vec::new();
            for node in &frontier {
                let Some(idxs) = guard.reverse_idx.get(node) else {
                    continue;
                };
                for &idx in idxs {
                    let edge = &guard.edges[idx];
                    if edge.kind != EdgeKind::Calls {
                        continue;
                    }
                    let caller = &edge.from;
                    // `target` itself never appears as a caller in the
                    // impact view: edges flow caller → target, never the
                    // other way. Skipping here also guards cycles where
                    // some node calls back into the target.
                    if caller == target {
                        continue;
                    }
                    // Skip if the caller is already on the BFS-tree path
                    // from `node` back to `target` — adding edge
                    // (caller, node) would mean the caller appears twice
                    // along the same impact path, which the legacy code
                    // disallowed via its per-path visited check.
                    if path_contains(&bfs_pred, node, caller) {
                        continue;
                    }
                    predecessors
                        .entry(caller.clone())
                        .or_default()
                        .push(node.clone());
                    if visited.insert(caller.clone()) {
                        bfs_pred.insert(caller.clone(), node.clone());
                        reachable.push(caller.clone());
                        next_frontier.push(caller.clone());
                    }
                }
            }
            if next_frontier.is_empty() {
                break;
            }
            frontier = next_frontier;
        }

        (predecessors, reachable)
    }

    /// Whether a `Calls` edge already exists from `from` to `to`.
    #[must_use]
    pub fn has_call_edge(&self, from: &EntityId, to: &EntityId) -> bool {
        let guard = self.read_or_recover();
        let Some(idxs) = guard.forward_idx.get(from) else {
            return false;
        };
        idxs.iter()
            .map(|&i| &guard.edges[i])
            .any(|e| e.kind == EdgeKind::Calls && &e.to == to)
    }

    /// Snapshot of all edges in insertion order.
    ///
    /// Yields every `Edge` exactly once — the natural counterpart to
    /// [`Store::all_atoms`]. Use this when a consumer needs to bucket the
    /// full edge list in a single sweep (e.g. building several adjacency
    /// maps at once) rather than paying O(E) per atom.
    #[must_use]
    pub fn all_edges(&self) -> Vec<Edge> {
        self.read_or_recover().edges.clone()
    }

    /// Number of atoms stored (for tests / diagnostics).
    #[must_use]
    pub fn atom_count(&self) -> usize {
        self.read_or_recover().atoms.len()
    }

    /// Number of edges stored (for tests / diagnostics).
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.read_or_recover().edges.len()
    }

    /// Test-only hook: take the write lock and panic, leaving the lock
    /// poisoned for the rest of the test.
    ///
    /// Hidden from rustdoc and prefixed with double underscores to keep it
    /// clearly out of the supported API. Used by `tests/store_lock_poison.rs`
    /// to verify the recovery path because integration tests cannot reach
    /// the private `inner` field.
    #[doc(hidden)]
    pub fn __poison_lock_for_tests(&self) {
        let _guard = self.write_or_recover();
        panic!("intentional poison for tests");
    }
}

/// Whether `needle` appears on the BFS spanning-tree path from `start`
/// back to the BFS root.
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

/// Walk the predecessor map produced by [`Store::reverse_call_paths`] from
/// `from` back to the BFS root (`target`).
///
/// The returned path is `[from, …, target]`. When `from == target`, the
/// path is just `[target]`. Picks the first successor at each branch, so
/// the path is one canonical witness — the full edge set lives in the
/// predecessor map itself.
#[must_use]
pub fn reconstruct_path<S: std::hash::BuildHasher>(
    predecessors: &HashMap<EntityId, Vec<EntityId>, S>,
    from: &EntityId,
) -> Vec<EntityId> {
    use std::collections::HashSet;

    let mut path = vec![from.clone()];
    let mut seen: HashSet<EntityId> = HashSet::new();
    seen.insert(from.clone());
    let mut current = from.clone();
    while let Some(succs) = predecessors.get(&current) {
        let Some(next) = succs.first() else { break };
        if !seen.insert(next.clone()) {
            break;
        }
        path.push(next.clone());
        current = next.clone();
    }
    path
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
        let (predecessors, reachable) = store.reverse_call_paths(&EntityId::new("c"), 0);
        assert!(predecessors.is_empty());
        assert_eq!(reachable, vec![EntityId::new("c")]);
    }

    #[test]
    fn reverse_call_paths_walks_back() {
        let store = Store::new();
        // a → b → c
        store.add_edge(edge("a", "b", EdgeKind::Calls));
        store.add_edge(edge("b", "c", EdgeKind::Calls));
        let target = EntityId::new("c");
        let (predecessors, reachable) = store.reverse_call_paths(&target, 2);

        // Reachable contains target plus both transitive callers.
        let set: std::collections::HashSet<_> = reachable.iter().collect();
        assert!(set.contains(&EntityId::new("a")));
        assert!(set.contains(&EntityId::new("b")));
        assert!(set.contains(&target));

        // Reconstructed path `a → … → c` reads `[a, b, c]`.
        let path = super::reconstruct_path(&predecessors, &EntityId::new("a"));
        assert_eq!(
            path,
            vec![EntityId::new("a"), EntityId::new("b"), EntityId::new("c")]
        );
    }

    #[test]
    fn reverse_call_paths_cycle_terminates() {
        let store = Store::new();
        store.add_edge(edge("a", "b", EdgeKind::Calls));
        store.add_edge(edge("b", "a", EdgeKind::Calls));
        // Must terminate, not loop forever.
        let (predecessors, reachable) = store.reverse_call_paths(&EntityId::new("a"), 3);
        // Each node visited at most once.
        let unique: std::collections::HashSet<_> = reachable.iter().collect();
        assert_eq!(unique.len(), reachable.len());
        // Predecessor map never contains a self-loop entry.
        for (k, vs) in &predecessors {
            assert!(!vs.contains(k), "self-loop in predecessors[{k:?}]");
        }
    }

    #[test]
    fn reverse_call_paths_diamond_records_all_call_edges() {
        // a → b, a → c, b → d, c → d. With target = d, hops = 2 the
        // predecessor map must capture all four caller→callee edges so
        // that the impact view does not lose the parallel branch.
        let store = Store::new();
        for (f, t) in [("a", "b"), ("a", "c"), ("b", "d"), ("c", "d")] {
            store.add_edge(edge(f, t, EdgeKind::Calls));
        }
        let (predecessors, _reachable) = store.reverse_call_paths(&EntityId::new("d"), 2);

        let mut edges: Vec<(String, String)> = predecessors
            .iter()
            .flat_map(|(k, vs)| {
                vs.iter()
                    .map(|v| (k.as_str().to_owned(), v.as_str().to_owned()))
            })
            .collect();
        edges.sort();
        assert_eq!(
            edges,
            vec![
                ("a".to_owned(), "b".to_owned()),
                ("a".to_owned(), "c".to_owned()),
                ("b".to_owned(), "d".to_owned()),
                ("c".to_owned(), "d".to_owned()),
            ]
        );
    }

    #[test]
    fn reconstruct_path_returns_singleton_for_target() {
        let store = Store::new();
        let target = EntityId::new("only");
        let (preds, _reachable) = store.reverse_call_paths(&target, 3);
        assert_eq!(super::reconstruct_path(&preds, &target), vec![target]);
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

    #[test]
    fn rebuild_indices_recomputes_from_edges() {
        let store = Store::new();
        store.add_edge(edge("a", "b", EdgeKind::Calls));
        store.add_edge(edge("a", "c", EdgeKind::Contains));
        store.add_edge(edge("d", "b", EdgeKind::Calls));
        // Forcing a rebuild must not change observable behaviour.
        store.rebuild_indices();
        let from_a = store.edges_from(&EntityId::new("a"));
        assert_eq!(from_a.len(), 2);
        let to_b = store.edges_to(&EntityId::new("b"));
        assert_eq!(to_b.len(), 2);
        assert!(store.has_call_edge(&EntityId::new("a"), &EntityId::new("b")));
        assert!(!store.has_call_edge(&EntityId::new("a"), &EntityId::new("c")));
    }
}
