//! In-memory [`polystore::GraphStore`] implementation for ingester atoms.
//!
//! Backed by `HashMap<EntityId, Atom>` for nodes and `Vec<EdgeRecord>` for
//! edges, guarded by a single `RwLock` for interior mutability (the trait
//! methods take `&self`).
//!
//! Designed for: tests, CLI one-shot runs, and the MVP self-bootstrap path.
//! NOT designed for: large datasets (no compaction), persistence (process-bound),
//! or concurrent writers (single writer at a time via the rwlock).

use async_trait::async_trait;
use ingester_core::{Atom, AtomId, Relation};
use polystore::{Direction, EntityId, GraphStore, PolystoreError, Result, Scope};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::RwLock;

/// In-memory backend bound to a single tenant scope.
pub struct InMemoryStore {
    scope: Scope,
    inner: RwLock<Inner>,
}

#[derive(Default)]
struct Inner {
    nodes: HashMap<EntityId, Atom>,
    edges: Vec<EdgeRecord>,
}

#[derive(Clone)]
struct EdgeRecord {
    from: EntityId,
    to: EntityId,
    relation: Relation,
}

impl InMemoryStore {
    /// Construct an empty store bound to `scope`.
    #[must_use]
    pub fn new(scope: Scope) -> Self {
        Self {
            scope,
            inner: RwLock::new(Inner::default()),
        }
    }

    /// Number of nodes currently stored.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned (only possible if a previous
    /// call panicked while holding the write lock).
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.inner.read().expect("rwlock not poisoned").nodes.len()
    }

    /// Number of edges currently stored.
    ///
    /// # Panics
    ///
    /// Panics if the internal lock is poisoned.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.inner.read().expect("rwlock not poisoned").edges.len()
    }
}

/// Bridge: ingester [`AtomId`] → polystore [`EntityId`].
fn atom_id_to_entity(id: &AtomId) -> EntityId {
    EntityId::new(id.as_str())
}

#[async_trait]
impl GraphStore<Atom, Relation> for InMemoryStore {
    fn scope(&self) -> &Scope {
        &self.scope
    }

    async fn upsert_node(&self, id: &EntityId, node: Atom) -> Result<()> {
        self.inner
            .write()
            .expect("rwlock not poisoned")
            .nodes
            .insert(id.clone(), node);
        Ok(())
    }

    async fn get_node(&self, id: &EntityId) -> Result<Option<Atom>> {
        Ok(self
            .inner
            .read()
            .expect("rwlock not poisoned")
            .nodes
            .get(id)
            .cloned())
    }

    async fn delete_node(&self, id: &EntityId) -> Result<()> {
        let mut guard = self.inner.write().expect("rwlock not poisoned");
        guard.nodes.remove(id);
        // Cascade: drop any edge touching this node.
        guard.edges.retain(|e| &e.from != id && &e.to != id);
        Ok(())
    }

    async fn add_edge(&self, from: &EntityId, to: &EntityId, edge: Relation) -> Result<()> {
        let guard_read = self.inner.read().expect("rwlock not poisoned");
        if !guard_read.nodes.contains_key(from) {
            return Err(PolystoreError::NotFound(format!("from node: {from}")));
        }
        if !guard_read.nodes.contains_key(to) {
            return Err(PolystoreError::NotFound(format!("to node: {to}")));
        }
        drop(guard_read);

        self.inner
            .write()
            .expect("rwlock not poisoned")
            .edges
            .push(EdgeRecord {
                from: from.clone(),
                to: to.clone(),
                relation: edge,
            });
        Ok(())
    }

    async fn neighbors(
        &self,
        id: &EntityId,
        direction: Direction,
    ) -> Result<Vec<(EntityId, Relation)>> {
        let guard = self.inner.read().expect("rwlock not poisoned");
        let mut out = Vec::new();
        for e in &guard.edges {
            match direction {
                Direction::Outgoing if e.from == *id => {
                    out.push((e.to.clone(), e.relation.clone()));
                }
                Direction::Incoming if e.to == *id => {
                    out.push((e.from.clone(), e.relation.clone()));
                }
                Direction::Both => {
                    if e.from == *id {
                        out.push((e.to.clone(), e.relation.clone()));
                    } else if e.to == *id {
                        out.push((e.from.clone(), e.relation.clone()));
                    }
                }
                _ => {}
            }
        }
        Ok(out)
    }

    async fn list_by_kind(&self, kind: &str) -> Result<Vec<EntityId>> {
        let guard = self.inner.read().expect("rwlock not poisoned");
        let mut out: Vec<EntityId> = guard
            .nodes
            .iter()
            .filter(|(_, atom)| atom.kind == kind)
            .map(|(id, _)| id.clone())
            .collect();
        // Deterministic iteration order for snapshot-friendly callers.
        out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(out)
    }

    async fn search_by_name(&self, query: &str, top_k: usize) -> Result<Vec<(EntityId, Atom)>> {
        if top_k == 0 {
            return Ok(Vec::new());
        }
        let needle = query.to_lowercase();
        let guard = self.inner.read().expect("rwlock not poisoned");
        let mut hits: Vec<(EntityId, Atom)> = guard
            .nodes
            .iter()
            .filter(|(_, atom)| atom.name.to_lowercase().contains(&needle))
            .map(|(id, atom)| (id.clone(), atom.clone()))
            .collect();
        // Stable order: shorter names first (closer match), then alphabetical.
        hits.sort_by(|a, b| {
            a.1.name
                .len()
                .cmp(&b.1.name.len())
                .then_with(|| a.1.name.cmp(&b.1.name))
        });
        hits.truncate(top_k);
        Ok(hits)
    }

    async fn reverse_path(&self, from: &EntityId, hops: u8) -> Result<Vec<Vec<EntityId>>> {
        if hops == 0 {
            return Ok(vec![vec![from.clone()]]);
        }

        // BFS on incoming edges. Each path is a Vec<EntityId> [from, predecessor, ...].
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
            let mut extended = false;
            let mut visited = HashSet::new();
            for node in &path {
                visited.insert(node.clone());
            }
            for e in &guard.edges {
                if e.to == *tip && !visited.contains(&e.from) {
                    let mut new_path = path.clone();
                    new_path.push(e.from.clone());
                    queue.push_back(new_path);
                    extended = true;
                }
            }
            if !extended {
                out.push(path);
            }
        }

        Ok(out)
    }
}

/// Convenience helper: ingest a parser [`ParseOutput`] into an [`InMemoryStore`].
///
/// Walks `output.atoms` (upserts each), then `output.relations` (adds each
/// edge). Atom contents (the `output.contents` map) are NOT stored — they
/// belong to a `KvStore`, which is out of scope for v0.2.
///
/// # Errors
///
/// Returns the first error from any underlying store operation. Edges that
/// reference unknown atoms produce [`PolystoreError::NotFound`].
pub async fn ingest_parse_output(
    store: &InMemoryStore,
    output: &ingester_core::ParseOutput,
) -> Result<()> {
    for atom in &output.atoms {
        let id = atom_id_to_entity(&atom.id);
        store.upsert_node(&id, atom.clone()).await?;
    }
    for rel in &output.relations {
        let from = atom_id_to_entity(&rel.from);
        let to = atom_id_to_entity(&rel.to);
        store.add_edge(&from, &to, rel.clone()).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn scope() -> Scope {
        Scope::new("ns", "repo", "branch")
    }

    fn make_atom(id: &str, kind: &str, name: &str) -> Atom {
        Atom::new(AtomId::new(id), kind, name, "deadbeef")
    }

    #[test]
    fn new_store_is_empty_and_carries_scope() {
        let store = InMemoryStore::new(scope());
        assert_eq!(store.node_count(), 0);
        assert_eq!(store.edge_count(), 0);
        assert_eq!(store.scope().to_string(), "ns/repo/branch");
    }

    #[test]
    fn atom_id_to_entity_bridges_string_repr() {
        let aid = AtomId::new("code:foo.rs");
        let eid = atom_id_to_entity(&aid);
        assert_eq!(eid.as_str(), "code:foo.rs");
    }

    #[tokio::test]
    async fn upsert_then_get_returns_stored_atom() {
        let store = InMemoryStore::new(scope());
        let id = EntityId::new("code:x");
        let atom = make_atom("code:x", "function", "foo");
        store.upsert_node(&id, atom.clone()).await.expect("upsert");
        let got = store.get_node(&id).await.expect("get").expect("present");
        assert_eq!(got.name, "foo");
    }

    #[tokio::test]
    async fn get_missing_returns_none() {
        let store = InMemoryStore::new(scope());
        let id = EntityId::new("code:missing");
        assert!(store.get_node(&id).await.expect("get").is_none());
    }

    #[tokio::test]
    async fn upsert_replaces_existing_node() {
        let store = InMemoryStore::new(scope());
        let id = EntityId::new("code:x");
        store
            .upsert_node(&id, make_atom("code:x", "function", "first"))
            .await
            .expect("upsert");
        store
            .upsert_node(&id, make_atom("code:x", "function", "second"))
            .await
            .expect("upsert");
        assert_eq!(store.node_count(), 1);
        let got = store.get_node(&id).await.expect("get").expect("present");
        assert_eq!(got.name, "second");
    }

    #[tokio::test]
    async fn delete_node_cascades_to_edges() {
        let store = InMemoryStore::new(scope());
        let a = EntityId::new("a");
        let b = EntityId::new("b");
        store
            .upsert_node(&a, make_atom("a", "function", "a"))
            .await
            .expect("upsert");
        store
            .upsert_node(&b, make_atom("b", "function", "b"))
            .await
            .expect("upsert");
        store
            .add_edge(
                &a,
                &b,
                Relation::new(AtomId::new("a"), AtomId::new("b"), "calls"),
            )
            .await
            .expect("edge");
        assert_eq!(store.edge_count(), 1);
        store.delete_node(&a).await.expect("delete");
        assert_eq!(store.node_count(), 1);
        assert_eq!(store.edge_count(), 0);
    }

    #[tokio::test]
    async fn delete_missing_node_is_idempotent() {
        let store = InMemoryStore::new(scope());
        store.delete_node(&EntityId::new("nope")).await.expect("ok");
    }

    #[tokio::test]
    async fn add_edge_rejects_unknown_endpoints() {
        let store = InMemoryStore::new(scope());
        let a = EntityId::new("a");
        let b = EntityId::new("b");
        store
            .upsert_node(&a, make_atom("a", "function", "a"))
            .await
            .expect("upsert");

        // 'b' missing
        let err = store
            .add_edge(
                &a,
                &b,
                Relation::new(AtomId::new("a"), AtomId::new("b"), "calls"),
            )
            .await
            .expect_err("must fail on missing to");
        assert!(matches!(err, PolystoreError::NotFound(_)));

        // 'a' missing (after deleting it)
        store.delete_node(&a).await.expect("ok");
        let err = store
            .add_edge(
                &a,
                &b,
                Relation::new(AtomId::new("a"), AtomId::new("b"), "calls"),
            )
            .await
            .expect_err("must fail on missing from");
        assert!(matches!(err, PolystoreError::NotFound(_)));
    }

    #[tokio::test]
    async fn neighbors_outgoing_incoming_both() {
        let store = InMemoryStore::new(scope());
        let a = EntityId::new("a");
        let b = EntityId::new("b");
        let c = EntityId::new("c");
        for (id, name) in [(&a, "a"), (&b, "b"), (&c, "c")] {
            store
                .upsert_node(id, make_atom(name, "function", name))
                .await
                .expect("upsert");
        }
        // a → b, a → c, b → c
        for (from, to) in [(&a, &b), (&a, &c), (&b, &c)] {
            store
                .add_edge(
                    from,
                    to,
                    Relation::new(
                        AtomId::new(from.as_str()),
                        AtomId::new(to.as_str()),
                        "calls",
                    ),
                )
                .await
                .expect("edge");
        }

        let outs = store
            .neighbors(&a, Direction::Outgoing)
            .await
            .expect("neighbors");
        let out_ids: Vec<&str> = outs.iter().map(|(n, _)| n.as_str()).collect();
        assert!(out_ids.contains(&"b"));
        assert!(out_ids.contains(&"c"));
        assert_eq!(out_ids.len(), 2);

        let ins = store
            .neighbors(&c, Direction::Incoming)
            .await
            .expect("neighbors");
        let in_ids: Vec<&str> = ins.iter().map(|(n, _)| n.as_str()).collect();
        assert!(in_ids.contains(&"a"));
        assert!(in_ids.contains(&"b"));
        assert_eq!(in_ids.len(), 2);

        let both = store
            .neighbors(&b, Direction::Both)
            .await
            .expect("neighbors");
        let both_ids: Vec<&str> = both.iter().map(|(n, _)| n.as_str()).collect();
        assert!(both_ids.contains(&"a")); // incoming a → b
        assert!(both_ids.contains(&"c")); // outgoing b → c
        assert_eq!(both_ids.len(), 2);
    }

    #[tokio::test]
    async fn list_by_kind_filters_and_sorts() {
        let store = InMemoryStore::new(scope());
        for (id, kind) in [
            ("code:fn:a", "function"),
            ("code:struct:s", "struct"),
            ("code:fn:b", "function"),
        ] {
            let eid = EntityId::new(id);
            store
                .upsert_node(&eid, make_atom(id, kind, id))
                .await
                .expect("upsert");
        }
        let fns = store.list_by_kind("function").await.expect("list");
        let ids: Vec<&str> = fns.iter().map(EntityId::as_str).collect();
        assert_eq!(ids, vec!["code:fn:a", "code:fn:b"]);

        let structs = store.list_by_kind("struct").await.expect("list");
        assert_eq!(structs.len(), 1);

        let none = store.list_by_kind("trait").await.expect("list");
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn search_by_name_substring_top_k() {
        let store = InMemoryStore::new(scope());
        for name in ["hello", "hello_world", "world", "Helmut"] {
            let id = EntityId::new(format!("code:{name}"));
            store
                .upsert_node(&id, make_atom(id.as_str(), "function", name))
                .await
                .expect("upsert");
        }
        // Case-insensitive substring `hel`
        let hits = store.search_by_name("hel", 10).await.expect("search");
        let names: Vec<&str> = hits.iter().map(|(_, a)| a.name.as_str()).collect();
        assert!(names.contains(&"hello"));
        assert!(names.contains(&"hello_world"));
        assert!(names.contains(&"Helmut"));

        // Top-k limits
        let limited = store.search_by_name("hel", 2).await.expect("search");
        assert_eq!(limited.len(), 2);
        // Shortest first ordering
        assert_eq!(limited[0].1.name, "hello");

        // top_k = 0 short-circuits
        let zero = store.search_by_name("hel", 0).await.expect("search");
        assert!(zero.is_empty());

        // No match returns empty
        let empty = store.search_by_name("zzz", 10).await.expect("search");
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn reverse_path_walks_incoming_edges() {
        let store = InMemoryStore::new(scope());
        // a → b → c. From c, reverse path of length 2 should reach a.
        for n in ["a", "b", "c"] {
            let eid = EntityId::new(n);
            store
                .upsert_node(&eid, make_atom(n, "function", n))
                .await
                .expect("upsert");
        }
        for (from, to) in [("a", "b"), ("b", "c")] {
            let f = EntityId::new(from);
            let t = EntityId::new(to);
            store
                .add_edge(
                    &f,
                    &t,
                    Relation::new(AtomId::new(from), AtomId::new(to), "calls"),
                )
                .await
                .expect("edge");
        }

        let paths_0 = store
            .reverse_path(&EntityId::new("c"), 0)
            .await
            .expect("paths");
        assert_eq!(paths_0, vec![vec![EntityId::new("c")]]);

        let paths_2 = store
            .reverse_path(&EntityId::new("c"), 2)
            .await
            .expect("paths");
        // Expect at least one path c → b → a
        let has_full = paths_2.iter().any(|p| {
            p.len() == 3 && p[0].as_str() == "c" && p[1].as_str() == "b" && p[2].as_str() == "a"
        });
        assert!(has_full, "expected reverse path c → b → a, got {paths_2:?}");

        // hops=1 should reach b but not a
        let paths_1 = store
            .reverse_path(&EntityId::new("c"), 1)
            .await
            .expect("paths");
        assert!(paths_1.iter().any(|p| p.len() == 2 && p[1].as_str() == "b"));
        assert!(paths_1.iter().all(|p| p.iter().all(|n| n.as_str() != "a")));
    }

    #[tokio::test]
    async fn reverse_path_visits_each_node_once_per_path() {
        // a → b, b → a (cycle). reverse_path from a, hops=3 must terminate
        // and not loop forever.
        let store = InMemoryStore::new(scope());
        for n in ["a", "b"] {
            let eid = EntityId::new(n);
            store
                .upsert_node(&eid, make_atom(n, "function", n))
                .await
                .expect("upsert");
        }
        for (from, to) in [("a", "b"), ("b", "a")] {
            let f = EntityId::new(from);
            let t = EntityId::new(to);
            store
                .add_edge(
                    &f,
                    &t,
                    Relation::new(AtomId::new(from), AtomId::new(to), "calls"),
                )
                .await
                .expect("edge");
        }
        let paths = store
            .reverse_path(&EntityId::new("a"), 3)
            .await
            .expect("paths");
        // Must terminate. Every path must be acyclic (no node repeated within it).
        for p in &paths {
            let mut seen = HashSet::new();
            for n in p {
                assert!(seen.insert(n.clone()), "cycle in path: {p:?}");
            }
        }
    }

    #[tokio::test]
    async fn ingest_parse_output_round_trip() {
        let store = InMemoryStore::new(scope());
        let mut output = ingester_core::ParseOutput::default();
        let a = AtomId::new("code:m");
        let b = AtomId::new("code:m::function::foo");
        output.atoms.push(Atom::new(a.clone(), "module", "m", "h0"));
        output
            .atoms
            .push(Atom::new(b.clone(), "function", "foo", "h1"));
        output
            .relations
            .push(Relation::new(a.clone(), b.clone(), "contains"));

        ingest_parse_output(&store, &output)
            .await
            .expect("ingest ok");

        assert_eq!(store.node_count(), 2);
        assert_eq!(store.edge_count(), 1);

        let fns = store.list_by_kind("function").await.expect("list");
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].as_str(), "code:m::function::foo");

        let neighbors = store
            .neighbors(&EntityId::new("code:m"), Direction::Outgoing)
            .await
            .expect("neighbors");
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].0.as_str(), "code:m::function::foo");
        assert_eq!(neighbors[0].1.kind, "contains");
    }

    #[tokio::test]
    async fn ingest_parse_output_propagates_missing_endpoint_error() {
        let store = InMemoryStore::new(scope());
        let mut output = ingester_core::ParseOutput::default();
        // Relation references atoms that aren't in `atoms` — should NotFound.
        output.relations.push(Relation::new(
            AtomId::new("ghost-a"),
            AtomId::new("ghost-b"),
            "calls",
        ));
        let err = ingest_parse_output(&store, &output)
            .await
            .expect_err("must fail");
        assert!(matches!(err, PolystoreError::NotFound(_)));
    }
}
