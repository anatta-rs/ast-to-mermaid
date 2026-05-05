//! Benchmarks for the `Store` adjacency-index accessors.
//!
//! Builds a synthetic 100k-atom / 1M-edge store and measures the four
//! accessors that previously linear-scanned the edge vector. Before the
//! adjacency index this took milliseconds per call; afterwards it should
//! take microseconds — see acceptance criterion in issue #66 (≥100×).

use ast_to_mermaid::graph::Store;
use ast_to_mermaid::model::{Edge, EdgeKind, EntityId};
use criterion::{Criterion, black_box, criterion_group, criterion_main};

const ATOMS: usize = 100_000;
const EDGES: usize = 1_000_000;

fn build_store() -> (Store, Vec<EntityId>) {
    let store = Store::new();
    let ids: Vec<EntityId> = (0..ATOMS).map(|i| EntityId::new(format!("n{i}"))).collect();
    // Spread edges deterministically: edge i goes from ids[i % ATOMS]
    // to ids[(i * 2654435761) % ATOMS] (Knuth multiplicative hash) so
    // every atom has roughly EDGES/ATOMS = 10 outgoing edges.
    for i in 0..EDGES {
        let from = ids[i % ATOMS].clone();
        let to_idx = (i.wrapping_mul(2_654_435_761)) % ATOMS;
        let to = ids[to_idx].clone();
        let kind = if i % 2 == 0 {
            EdgeKind::Calls
        } else {
            EdgeKind::Contains
        };
        store.add_edge(Edge::new(from, to, kind));
    }
    (store, ids)
}

fn bench_edges_from(c: &mut Criterion) {
    let (store, ids) = build_store();
    let mut i: usize = 0;
    c.bench_function("edges_from", |b| {
        b.iter(|| {
            let id = &ids[i % ATOMS];
            i = i.wrapping_add(1);
            black_box(store.edges_from(black_box(id)));
        });
    });
}

fn bench_edges_to(c: &mut Criterion) {
    let (store, ids) = build_store();
    let mut i: usize = 0;
    c.bench_function("edges_to", |b| {
        b.iter(|| {
            let id = &ids[i % ATOMS];
            i = i.wrapping_add(1);
            black_box(store.edges_to(black_box(id)));
        });
    });
}

fn bench_call_edges_from(c: &mut Criterion) {
    let (store, ids) = build_store();
    let mut i: usize = 0;
    c.bench_function("call_edges_from", |b| {
        b.iter(|| {
            let id = &ids[i % ATOMS];
            i = i.wrapping_add(1);
            black_box(store.call_edges_from(black_box(id)));
        });
    });
}

fn bench_has_call_edge(c: &mut Criterion) {
    let (store, ids) = build_store();
    let mut i: usize = 0;
    c.bench_function("has_call_edge", |b| {
        b.iter(|| {
            let from = &ids[i % ATOMS];
            let to = &ids[(i.wrapping_mul(7)) % ATOMS];
            i = i.wrapping_add(1);
            black_box(store.has_call_edge(black_box(from), black_box(to)));
        });
    });
}

#[allow(missing_docs)]
mod bench_group {
    use super::{
        bench_call_edges_from, bench_edges_from, bench_edges_to, bench_has_call_edge,
        criterion_group,
    };
    criterion_group!(
        benches,
        bench_edges_from,
        bench_edges_to,
        bench_call_edges_from,
        bench_has_call_edge
    );
}

criterion_main!(bench_group::benches);
