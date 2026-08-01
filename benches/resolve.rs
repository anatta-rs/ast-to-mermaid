//! Benchmark for [`resolve_cross_module_calls`] on a 100k-fn synthetic
//! store.
//!
//! Spec target (issue #119, C28 acceptance criterion): ≥ 5× faster than
//! the pre-borrow-API revision. Hot path was an `O(N_atoms)` deep clone of
//! every `CodeAtom` plus `EntityId` clones inside `filter_viable`. Now it
//! borrows under one `with_atoms` read guard with a `(usize, usize)`
//! existing-edge set.
//!
//! Layout: 1000 modules × 100 functions, every fn has 5 cross-module
//! calls — half qualified `module::name`, half bare. That mirrors the
//! distribution observed on the anatta monorepo where the cliff first
//! showed up.

use ast_to_mermaid::Store;
use ast_to_mermaid::model::{CallSite, CodeAtom, EntityId};
use ast_to_mermaid::resolve::resolve_cross_module_calls;
use criterion::{Criterion, black_box, criterion_group, criterion_main};

const MODULES: usize = 1000;
const FNS_PER_MODULE: usize = 100;
const CALLS_PER_FN: usize = 5;

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

fn function_atom(file_path: &str, name: &str, calls: Vec<String>) -> CodeAtom {
    CodeAtom {
        id: EntityId::new(format!("code:{file_path}::function::{name}")),
        kind: "function".to_owned(),
        name: name.to_owned(),
        file_path: file_path.to_owned(),
        line_start: 1,
        line_end: 3,
        doc: String::new(),
        signature: String::new(),
        content_hash: "h".to_owned(),
        calls: calls.into_iter().map(CallSite::bare).collect(),
        method_calls: Vec::new(),
        parent: None,
    }
}

fn build_store() -> Store {
    let store = Store::new();
    // Same crate so the same-crate pref kicks in (mirrors real bundles).
    let crate_prefix = "crate_a/src";
    for m in 0..MODULES {
        let file = format!("{crate_prefix}/m{m}.rs");
        let module_name = format!("m{m}");
        store.add_atom(module_atom(&file, &module_name));
        for f in 0..FNS_PER_MODULE {
            let fn_name = format!("fn_{m}_{f}");
            // Build CALLS_PER_FN deterministic calls into other modules.
            // Knuth multiplicative hash over the function's global slot.
            let global_slot = m * FNS_PER_MODULE + f;
            let calls: Vec<String> = (0..CALLS_PER_FN)
                .map(|c| {
                    let target = global_slot
                        .wrapping_mul(2_654_435_761)
                        .wrapping_add(c.wrapping_mul(7_919))
                        % (MODULES * FNS_PER_MODULE);
                    let tm = target / FNS_PER_MODULE;
                    let tf = target % FNS_PER_MODULE;
                    if c % 2 == 0 {
                        format!("m{tm}::fn_{tm}_{tf}")
                    } else {
                        format!("fn_{tm}_{tf}")
                    }
                })
                .collect();
            store.add_atom(function_atom(&file, &fn_name, calls));
        }
    }
    store
}

fn bench_resolve_cross_module_calls(c: &mut Criterion) {
    // Keep one representative iteration cheap by building once. Each
    // benchmark sample resolves on the same populated store; the
    // resolver is idempotent so subsequent samples become a pure read
    // pass over the existing edges (no further mutations). That is
    // exactly what we want to time: the hot snapshot + filter_viable
    // path without confusing new-edge mutations into the measurement.
    let store = build_store();
    // Warm: first call adds the edges. Subsequent timed calls measure
    // the steady-state pass.
    let _ = resolve_cross_module_calls(&store);
    c.bench_function("resolve_cross_module_calls_100k_fn", |b| {
        b.iter(|| {
            black_box(resolve_cross_module_calls(black_box(&store)));
        });
    });
}

#[allow(missing_docs)]
mod bench_group {
    use super::{bench_resolve_cross_module_calls, criterion_group};
    criterion_group!(benches, bench_resolve_cross_module_calls);
}

criterion_main!(bench_group::benches);
