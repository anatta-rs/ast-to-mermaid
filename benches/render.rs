//! Benchmarks for the project- and overview-level renderers.
//!
//! Builds a synthetic 10k-function fixture spread across several crates and
//! measures `render::project::render` end-to-end. Before C2 the loop was
//! `O(F·E)`; after C2 it's `O(F·avg_degree)` thanks to forward adjacency. The
//! acceptance criterion in issue #67 is ≥10× speedup on this fixture.
//!
//! The overview bench targets issue #68 — same forward-adjacency switch
//! plus pre-bucketed Contains edges — at 1k modules / 10k functions, where
//! the pre-fix nested loop was `O(modules·E)`.

use ast_to_mermaid::graph::Store;
use ast_to_mermaid::model::{CodeAtom, Edge, EdgeKind, EntityId};
use ast_to_mermaid::render::AdjMaps;
use ast_to_mermaid::{Level, render};
use criterion::{Criterion, black_box, criterion_group, criterion_main};

const FUNCTIONS: usize = 10_000;
const CRATES: usize = 10;
const CALLS_PER_FN: usize = 5;

const OVERVIEW_MODULES: usize = 1_000;
const OVERVIEW_FNS_PER_MODULE: usize = 10;
const OVERVIEW_CALLS_PER_FN: usize = 5;

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

fn build_store() -> Store {
    let store = Store::new();
    let mut ids: Vec<EntityId> = Vec::with_capacity(FUNCTIONS);
    for i in 0..FUNCTIONS {
        let crate_idx = i % CRATES;
        let file_path = format!("crates/c{crate_idx}/src/lib.rs");
        let atom = fn_atom(&file_path, &format!("f{i}"));
        ids.push(atom.id.clone());
        store.add_atom(atom);
    }
    // Spread edges deterministically. Multiplicative-hash target index so
    // most calls land in *other* crates.
    for (i, from) in ids.iter().enumerate() {
        for k in 0..CALLS_PER_FN {
            let target = i.wrapping_mul(2_654_435_761).wrapping_add(k) % FUNCTIONS;
            store.add_edge(Edge::new(
                from.clone(),
                ids[target].clone(),
                EdgeKind::Calls,
            ));
        }
    }
    store
}

fn module_atom(file_path: &str, name: &str) -> CodeAtom {
    CodeAtom {
        id: EntityId::new(format!("code:{file_path}")),
        kind: "module".to_owned(),
        name: name.to_owned(),
        file_path: file_path.to_owned(),
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

fn build_overview_store() -> Store {
    let store = Store::new();
    let mut fn_ids: Vec<EntityId> = Vec::with_capacity(OVERVIEW_MODULES * OVERVIEW_FNS_PER_MODULE);
    for m in 0..OVERVIEW_MODULES {
        let file_path = format!("crates/c{}/src/m{m}.rs", m % CRATES);
        let module = module_atom(&file_path, &format!("m{m}"));
        let module_id = module.id.clone();
        store.add_atom(module);
        for k in 0..OVERVIEW_FNS_PER_MODULE {
            let f = fn_atom(&file_path, &format!("f{m}_{k}"));
            let fid = f.id.clone();
            store.add_atom(f);
            store.add_edge(Edge::new(
                module_id.clone(),
                fid.clone(),
                EdgeKind::Contains,
            ));
            fn_ids.push(fid);
        }
    }
    let total = fn_ids.len();
    for (i, from) in fn_ids.iter().enumerate() {
        for k in 0..OVERVIEW_CALLS_PER_FN {
            let target = i.wrapping_mul(2_654_435_761).wrapping_add(k) % total;
            store.add_edge(Edge::new(
                from.clone(),
                fn_ids[target].clone(),
                EdgeKind::Calls,
            ));
        }
    }
    store
}

fn bench_project(c: &mut Criterion) {
    let store = build_store();
    let adj = AdjMaps::build(&store);
    c.bench_function("project", |b| {
        b.iter(|| {
            black_box(
                render(Level::Project, black_box(&store), black_box(&adj), None).expect("render"),
            );
        });
    });
}

fn bench_overview(c: &mut Criterion) {
    let store = build_overview_store();
    let adj = AdjMaps::build(&store);
    c.bench_function("overview", |b| {
        b.iter(|| {
            black_box(
                render(Level::Overview, black_box(&store), black_box(&adj), None).expect("render"),
            );
        });
    });
}

#[allow(missing_docs)]
mod bench_group {
    use super::{bench_overview, bench_project, criterion_group};
    criterion_group!(benches, bench_project, bench_overview);
}

criterion_main!(bench_group::benches);
