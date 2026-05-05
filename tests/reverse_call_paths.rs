//! Algorithmic guard for [`Store::reverse_call_paths`]: a high-fan-in
//! synthetic graph that the legacy path-cloning BFS would OOM on must
//! finish in bounded time under the predecessor-map rewrite.

use ast_to_mermaid::graph::Store;
use ast_to_mermaid::model::{Edge, EdgeKind, EntityId};

#[test]
fn reverse_call_paths_handles_high_fanin() {
    // Pathological fan-in: 1 target, two layers of F callers each, with
    // every layer-2 caller calling every layer-1 caller. The legacy
    // path-cloning BFS enumerates `F^hops` simple paths and OOMs for
    // F=1k; the predecessor-map rewrite visits `1 + 2F` distinct nodes
    // and finishes in milliseconds.
    //
    // We use the Store API directly here — exercising the algorithmic
    // change does not require parsing fixture files.
    let store = Store::new();
    let target = EntityId::new("target");
    let f: usize = 1000;

    let mut layer1: Vec<EntityId> = Vec::with_capacity(f);
    for i in 0..f {
        let id = EntityId::new(format!("l1_{i}"));
        store.add_edge(Edge::new(id.clone(), target.clone(), EdgeKind::Calls));
        layer1.push(id);
    }
    for i in 0..f {
        let l2 = EntityId::new(format!("l2_{i}"));
        for l1 in &layer1 {
            store.add_edge(Edge::new(l2.clone(), l1.clone(), EdgeKind::Calls));
        }
    }

    let started = std::time::Instant::now();
    let (predecessors, reachable) = store.reverse_call_paths(&target, 3);
    let elapsed = started.elapsed();

    // 5 s ceiling instead of 1 s: the legacy path-cloning BFS would not
    // finish on this fixture under any timeout (it OOMs the runner). The
    // predecessor-map rewrite finishes in low single-digit seconds even on
    // the slowest GH Actions runner — we only need to assert "completes in
    // bounded time", not "fast on dev hardware".
    assert!(
        elapsed.as_secs_f64() < 5.0,
        "reverse_call_paths(F={f}, hops=3) took {elapsed:?}; legacy code OOMs here"
    );

    // 1 target + F layer-1 callers + F layer-2 callers.
    assert_eq!(reachable.len(), 1 + 2 * f);

    // Every layer-1 caller has exactly one downstream entry (the target);
    // every layer-2 caller has F downstream entries (every layer-1 node).
    // Total predecessor entries = F + F*F.
    let edge_count: usize = predecessors.values().map(Vec::len).sum();
    assert_eq!(edge_count, f + f * f);
}
