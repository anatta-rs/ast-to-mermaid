//! Pre-computed forward / reverse adjacency maps consumed by every level
//! renderer.
//!
//! Built once per bundle (or per `analyze`) invocation by [`AdjMaps::build`]
//! and threaded as `&AdjMaps` into [`crate::render::render`]. Replaces the
//! per-call `Store::call_edges_from` / `edges_from` / `children_of` sweeps
//! that previously dominated bundle wall-time.
//!
//! Storage trick: every `EntityId` is interned once into an `Arc<EntityId>`
//! up front. Each forward and reverse map then holds Arc clones — refcount
//! bumps, no `String` allocation — so for an edge `(a, b, Calls)` the `a`
//! arc lives in `callees`'s key bucket *and* in `callers[b]`'s value vec
//! without a duplicate copy of the id string. On a 1M-atom / 10M-edge
//! graph the prior `HashMap<EntityId, Vec<EntityId>>` design held ~40M
//! deep-cloned `EntityId`s; this design holds 1M unique allocations plus
//! 4×E refcount bumps.

use crate::graph::Store;
use crate::model::{EdgeKind, EntityId};
use std::collections::HashMap;
use std::sync::Arc;

/// Forward + reverse adjacency for the four edge kinds the renderers care
/// about (`Calls`, `Contains`, `Uses`, `Implements`).
///
/// Lookups take `&EntityId`; the underlying maps are keyed by
/// `Arc<EntityId>` and yield `&[Arc<EntityId>]` slices.
#[derive(Default)]
pub struct AdjMaps {
    callees: HashMap<Arc<EntityId>, Vec<Arc<EntityId>>>,
    /// Call-site ranks per forward `Calls` edge, kept index-for-index
    /// with `callees`: `callee_sites[id][i]` describes `callees[id][i]`.
    /// The two are pushed in lockstep in [`AdjMaps::build`] and nowhere
    /// else, which is what makes the invariant hold.
    callee_sites: HashMap<Arc<EntityId>, Vec<Vec<u16>>>,
    callers: HashMap<Arc<EntityId>, Vec<Arc<EntityId>>>,
    children: HashMap<Arc<EntityId>, Vec<Arc<EntityId>>>,
    uses_out: HashMap<Arc<EntityId>, Vec<Arc<EntityId>>>,
    uses_in: HashMap<Arc<EntityId>, Vec<Arc<EntityId>>>,
    implements_out: HashMap<Arc<EntityId>, Vec<Arc<EntityId>>>,
    implements_in: HashMap<Arc<EntityId>, Vec<Arc<EntityId>>>,
}

impl AdjMaps {
    /// Build all seven adjacency buckets in a single sweep over `store`'s
    /// edge slice.
    ///
    /// Pre-interns every atom id into an `Arc` so the edge sweep mostly hits
    /// the cache and forward + reverse buckets share heap storage. Edge
    /// endpoints not present in the atom set (dangling references) get a
    /// fresh `Arc` allocated on the fly — rare in practice.
    #[must_use]
    pub fn build(store: &Store) -> Self {
        let intern: HashMap<EntityId, Arc<EntityId>> = store.with_atoms(|atoms| {
            let mut m = HashMap::with_capacity(atoms.len());
            for atom in atoms {
                m.insert(atom.id.clone(), Arc::new(atom.id.clone()));
            }
            m
        });
        let arc_of = |id: &EntityId| -> Arc<EntityId> {
            intern
                .get(id)
                .map_or_else(|| Arc::new(id.clone()), Arc::clone)
        };

        let mut maps = Self::default();
        store.with_edges(|edges| {
            for edge in edges {
                let from = arc_of(&edge.from);
                let to = arc_of(&edge.to);
                match edge.kind {
                    EdgeKind::Calls => {
                        maps.callees
                            .entry(Arc::clone(&from))
                            .or_default()
                            .push(Arc::clone(&to));
                        maps.callee_sites
                            .entry(Arc::clone(&from))
                            .or_default()
                            .push(edge.sites.clone());
                        maps.callers.entry(to).or_default().push(from);
                    }
                    EdgeKind::Contains => {
                        maps.children.entry(from).or_default().push(to);
                    }
                    EdgeKind::Uses => {
                        maps.uses_out
                            .entry(Arc::clone(&from))
                            .or_default()
                            .push(Arc::clone(&to));
                        maps.uses_in.entry(to).or_default().push(from);
                    }
                    EdgeKind::Implements => {
                        maps.implements_out
                            .entry(Arc::clone(&from))
                            .or_default()
                            .push(Arc::clone(&to));
                        maps.implements_in.entry(to).or_default().push(from);
                    }
                }
            }
        });
        maps
    }

    /// Forward `Calls`: ids called by `id`.
    #[must_use]
    pub fn callees(&self, id: &EntityId) -> &[Arc<EntityId>] {
        self.callees.get(id).map_or(&[], Vec::as_slice)
    }

    /// Forward `Calls` paired with the call-site ranks each edge came
    /// from, ascending.
    ///
    /// An empty rank slice means "unknown", not "no site": edges built
    /// before the ranks were recorded — a bundle read back from an older
    /// cache, or any `Edge::new` — carry none. Callers must not read that
    /// as evidence about the caller's body.
    pub fn callees_with_sites(
        &self,
        id: &EntityId,
    ) -> impl Iterator<Item = (&Arc<EntityId>, &[u16])> {
        let sites = self.callee_sites.get(id);
        self.callees
            .get(id)
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .enumerate()
            .map(move |(i, callee)| {
                let ranks = sites.and_then(|s| s.get(i)).map_or(&[][..], Vec::as_slice);
                (callee, ranks)
            })
    }

    /// Reverse `Calls`: ids that call `id`.
    #[must_use]
    pub fn callers(&self, id: &EntityId) -> &[Arc<EntityId>] {
        self.callers.get(id).map_or(&[], Vec::as_slice)
    }

    /// Forward `Contains`: items contained by `id` (e.g. a module's atoms).
    #[must_use]
    pub fn children(&self, id: &EntityId) -> &[Arc<EntityId>] {
        self.children.get(id).map_or(&[], Vec::as_slice)
    }

    /// Forward `Uses`: ids used by `id`.
    #[must_use]
    pub fn uses_out(&self, id: &EntityId) -> &[Arc<EntityId>] {
        self.uses_out.get(id).map_or(&[], Vec::as_slice)
    }

    /// Reverse `Uses`: ids that use `id`.
    #[must_use]
    pub fn uses_in(&self, id: &EntityId) -> &[Arc<EntityId>] {
        self.uses_in.get(id).map_or(&[], Vec::as_slice)
    }

    /// Forward `Implements`: traits/types `id` implements.
    #[must_use]
    pub fn implements_out(&self, id: &EntityId) -> &[Arc<EntityId>] {
        self.implements_out.get(id).map_or(&[], Vec::as_slice)
    }

    /// Reverse `Implements`: ids that implement `id`.
    #[must_use]
    pub fn implements_in(&self, id: &EntityId) -> &[Arc<EntityId>] {
        self.implements_in.get(id).map_or(&[], Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CodeAtom, Edge};

    fn atom(id: &str, kind: &str) -> CodeAtom {
        CodeAtom {
            id: EntityId::new(id),
            kind: kind.to_owned(),
            name: id.to_owned(),
            file_path: "src/lib.rs".to_owned(),
            line_start: 1,
            line_end: 1,
            doc: String::new(),
            signature: String::new(),
            content_hash: "h".to_owned(),
            calls: Vec::new(),
            method_calls: Vec::new(),
            parent: None,
        }
    }

    #[test]
    fn build_buckets_all_edge_kinds() {
        let store = Store::new();
        store.add_atom(atom("a", "function"));
        store.add_atom(atom("b", "function"));
        store.add_atom(atom("m", "module"));
        store.add_atom(atom("t", "trait"));

        let a = EntityId::new("a");
        let b = EntityId::new("b");
        let m = EntityId::new("m");
        let t = EntityId::new("t");
        store.add_edge(Edge::new(a.clone(), b.clone(), EdgeKind::Calls));
        store.add_edge(Edge::new(m.clone(), a.clone(), EdgeKind::Contains));
        store.add_edge(Edge::new(a.clone(), m.clone(), EdgeKind::Uses));
        store.add_edge(Edge::new(a.clone(), t.clone(), EdgeKind::Implements));

        let adj = AdjMaps::build(&store);
        assert_eq!(adj.callees(&a).len(), 1);
        assert_eq!(adj.callers(&b).len(), 1);
        assert_eq!(adj.children(&m).len(), 1);
        assert_eq!(adj.uses_out(&a).len(), 1);
        assert_eq!(adj.uses_in(&m).len(), 1);
        assert_eq!(adj.implements_out(&a).len(), 1);
        assert_eq!(adj.implements_in(&t).len(), 1);
    }

    /// Forward + reverse Arcs for the same id must point to the same heap
    /// allocation. This is the type-level assertion the spec asks for: it
    /// proves we're not deep-cloning `EntityId` strings across maps.
    #[test]
    fn forward_and_reverse_share_arc_allocation() {
        let store = Store::new();
        store.add_atom(atom("a", "function"));
        store.add_atom(atom("b", "function"));
        store.add_edge(Edge::new(
            EntityId::new("a"),
            EntityId::new("b"),
            EdgeKind::Calls,
        ));
        let adj = AdjMaps::build(&store);

        // `a` appears as a key in `callees` and as a value in `callers[b]` —
        // both must refer to the same Arc allocation.
        let a_key = adj
            .callees
            .keys()
            .find(|k| k.as_str() == "a")
            .expect("a key");
        let a_in_callers = adj
            .callers(&EntityId::new("b"))
            .iter()
            .find(|c| c.as_str() == "a")
            .expect("a in callers[b]");
        assert!(
            Arc::ptr_eq(a_key, a_in_callers),
            "forward + reverse must share the same Arc<EntityId>"
        );
    }

    #[test]
    fn unknown_id_returns_empty_slice() {
        let adj = AdjMaps::default();
        assert!(adj.callees(&EntityId::new("missing")).is_empty());
        assert!(adj.callers(&EntityId::new("missing")).is_empty());
        assert!(adj.children(&EntityId::new("missing")).is_empty());
    }
}
