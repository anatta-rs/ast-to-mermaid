//! End-to-end coverage for cross-module call resolution when several
//! candidate functions share the same name.
//!
//! Reproduces the original bug: `pipeline::analyze` calls `render(...)` after
//! `use crate::render::render;`, and `render::mod::render` itself dispatches
//! to `project::render`, `overview::render`, etc. The resolver must
//! disambiguate by leaning on the file-scope `use` imports (parser-side) and
//! the qualifier prefix preserved in inline `module::fn(...)` calls
//! (resolver-side).

use std::fs;
use std::path::Path;

use ast_to_mermaid::graph::Store;
use ast_to_mermaid::model::{Edge, EdgeKind, EntityId};
use ast_to_mermaid::parser::{CodeParser, Language};
use ast_to_mermaid::pipeline::walk_for_languages;
use ast_to_mermaid::resolve::resolve_cross_module_calls;

fn build_store(root: &Path) -> Store {
    let files = walk_for_languages(root).expect("walk");
    let store = Store::new();
    for (path, lang) in &files {
        let bytes = fs::read(path).expect("read");
        let parser = match lang {
            Language::Rust => CodeParser::rust(),
            Language::Python => CodeParser::python(),
        };
        let display = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string();
        parser.parse_into(&bytes, &display, &store).expect("parse");
    }
    resolve_cross_module_calls(&store);
    store
}

#[test]
fn use_import_resolves_to_mod_dot_rs_when_name_is_ambiguous() {
    // Caller does `use crate::render::render;` then `render(...)`. There are
    // 3 `render` functions in the workspace; only the one at
    // `src/render/mod.rs` should be linked.
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();

    fs::create_dir_all(root.join("src/render")).expect("mkdir");
    fs::write(
        root.join("src/pipeline.rs"),
        "use crate::render::render;\n\
         pub fn analyze() { render(); }\n",
    )
    .expect("write pipeline");
    fs::write(root.join("src/render/mod.rs"), "pub fn render() {}\n").expect("write render/mod");
    fs::write(root.join("src/render/project.rs"), "pub fn render() {}\n").expect("write project");
    fs::write(root.join("src/render/overview.rs"), "pub fn render() {}\n").expect("write overview");

    let store = build_store(root);

    let analyze = EntityId::new("code:src/pipeline.rs::function::analyze");
    let target = EntityId::new("code:src/render/mod.rs::function::render");
    let project = EntityId::new("code:src/render/project.rs::function::render");
    let overview = EntityId::new("code:src/render/overview.rs::function::render");

    assert!(
        store.has_call_edge(&analyze, &target),
        "expected edge analyze → src/render/mod.rs::render"
    );
    assert!(
        !store.has_call_edge(&analyze, &project),
        "must not bind to project::render"
    );
    assert!(
        !store.has_call_edge(&analyze, &overview),
        "must not bind to overview::render"
    );
}

#[test]
fn qualified_inline_calls_dispatch_to_correct_sibling_module() {
    // A `render::mod::render` that does `match { Project => project::render(),
    // Overview => overview::render() }` must produce one edge per sibling.
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();

    fs::create_dir_all(root.join("src/render")).expect("mkdir");
    fs::write(
        root.join("src/render/mod.rs"),
        "pub fn render() {\n\
         \x20   project::render();\n\
         \x20   overview::render();\n\
         }\n",
    )
    .expect("write mod");
    fs::write(root.join("src/render/project.rs"), "pub fn render() {}\n").expect("write project");
    fs::write(root.join("src/render/overview.rs"), "pub fn render() {}\n").expect("write overview");

    let store = build_store(root);

    let dispatcher = EntityId::new("code:src/render/mod.rs::function::render");
    let project = EntityId::new("code:src/render/project.rs::function::render");
    let overview = EntityId::new("code:src/render/overview.rs::function::render");

    assert!(store.has_call_edge(&dispatcher, &project));
    assert!(store.has_call_edge(&dispatcher, &overview));
}

#[test]
fn bare_call_to_unique_name_still_resolves() {
    // Regression guard for the simple case unchanged by this fix.
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();

    fs::create_dir_all(root.join("src")).expect("mkdir");
    fs::write(
        root.join("src/caller.rs"),
        "pub fn caller() { unique_helper(); }\n",
    )
    .expect("write caller");
    fs::write(root.join("src/helper.rs"), "pub fn unique_helper() {}\n").expect("write helper");

    let store = build_store(root);

    let caller = EntityId::new("code:src/caller.rs::function::caller");
    let helper = EntityId::new("code:src/helper.rs::function::unique_helper");
    assert!(store.has_call_edge(&caller, &helper));
}

#[test]
fn rust_receiver_method_call_does_not_ghost_bind_to_lifted_method() {
    // The headline regression. A caller writes `client.send()`; the parser
    // captures bare `send` from the `field_expression`. In another crate,
    // `DaemonHandle::send` is the only `send` in the graph (lifted impl
    // method). The pre-fix resolver would silently bind via unique-cross-
    // crate fallback; the fixed resolver leaves the call unresolved.
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();

    fs::create_dir_all(root.join("anatta-cli/src")).expect("mkdir cli");
    fs::create_dir_all(root.join("sigil-engine/src")).expect("mkdir engine");
    fs::write(
        root.join("anatta-cli/src/lib.rs"),
        "pub fn caller(client: &reqwest::Client) {\n\
         \x20   let _ = client.post(\"x\").send();\n\
         }\n",
    )
    .expect("write cli");
    fs::write(
        root.join("sigil-engine/src/daemon.rs"),
        "pub struct DaemonHandle;\n\
         impl DaemonHandle { pub fn send(&self) {} }\n",
    )
    .expect("write daemon");

    let store = build_store(root);

    let caller = EntityId::new("code:anatta-cli/src/lib.rs::function::caller");
    let target = EntityId::new("code:sigil-engine/src/daemon.rs::function::DaemonHandle::send");
    assert!(
        store.get_atom(&target).is_some(),
        "DaemonHandle::send must be lifted as a method atom"
    );
    assert!(
        !store.has_call_edge(&caller, &target),
        "bare receiver-style `send` must not bind to DaemonHandle::send"
    );
}

#[test]
fn rust_receiver_call_does_not_ghost_bind_to_free_fn_with_common_name() {
    // The sphere_tree case: `something.build()` (`.build()` is on an
    // external builder type — `reqwest::ClientBuilder`, `Session::builder`,
    // ort EP builders, …) must not link to a stray `fn build` defined
    // somewhere unrelated. The parser captures `build` as a method-style
    // call (`field_expression`) and routes it to `method_calls`, which the
    // resolver ignores cross-module.
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();

    fs::create_dir_all(root.join("inference-router/src")).expect("mkdir caller");
    fs::create_dir_all(root.join("sigil-research/src")).expect("mkdir target");
    fs::write(
        root.join("inference-router/src/reranker.rs"),
        "pub fn run(builder: SomeExternalBuilder) {\n\
         \x20   let _coreml = builder\n\
         \x20       .with_format(\"x\")\n\
         \x20       .build();\n\
         }\n",
    )
    .expect("write caller");
    fs::write(
        root.join("sigil-research/src/sphere_tree.rs"),
        "pub fn build(points: &[u8]) {}\n",
    )
    .expect("write target");

    let store = build_store(root);

    let caller = EntityId::new("code:inference-router/src/reranker.rs::function::run");
    let target = EntityId::new("code:sigil-research/src/sphere_tree.rs::function::build");
    assert!(
        store.get_atom(&target).is_some(),
        "free fn `build` must be lifted as an atom"
    );
    assert!(
        !store.has_call_edge(&caller, &target),
        "receiver-style `.build()` must not bind to free fn `build`"
    );
}

#[test]
fn python_caller_does_not_bind_to_rust_free_fn_with_same_name() {
    // Real anatta-rs case: `anatta/llm-services/shared/benchmark.py::main`
    // calls bare `print_summary(...)`; sigil-bench/src/metrics.rs has
    // `pub fn print_summary`. They share neither namespace nor linkage.
    // The resolver must filter candidates by language (file extension)
    // so the .py caller can never resolve to a .rs target.
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();

    fs::create_dir_all(root.join("py-tools")).expect("mkdir py");
    fs::create_dir_all(root.join("rust-bench/src")).expect("mkdir rs");
    fs::write(
        root.join("py-tools/benchmark.py"),
        "def main():\n\
         \x20   print_summary([1, 2, 3])\n",
    )
    .expect("write py");
    fs::write(
        root.join("rust-bench/src/metrics.rs"),
        "pub fn print_summary(xs: &[u32]) {}\n",
    )
    .expect("write rs");

    let store = build_store(root);

    let py_caller = EntityId::new("code:py-tools/benchmark.py::function::main");
    let rs_target = EntityId::new("code:rust-bench/src/metrics.rs::function::print_summary");
    assert!(store.get_atom(&py_caller).is_some());
    assert!(store.get_atom(&rs_target).is_some());
    assert!(
        !store.has_call_edge(&py_caller, &rs_target),
        ".py caller must not bind cross-language to .rs free fn"
    );
}

#[test]
fn rust_caller_does_not_bind_to_python_free_fn_with_same_name() {
    // Symmetric: a Rust `bench main()` calling bare `load_dataset(...)`
    // must not bind to `def load_dataset` in some Python module.
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();

    fs::create_dir_all(root.join("rust-bench/src")).expect("mkdir rs");
    fs::create_dir_all(root.join("py-tools")).expect("mkdir py");
    fs::write(
        root.join("rust-bench/src/synthetic_recall.rs"),
        "fn main() { load_dataset(); }\n\
         pub fn load_dataset() {}\n",
    )
    .expect("write rs");
    fs::write(
        root.join("py-tools/train.py"),
        "def load_dataset():\n    return None\n",
    )
    .expect("write py");

    let store = build_store(root);

    let rs_caller = EntityId::new("code:rust-bench/src/synthetic_recall.rs::function::main");
    let py_target = EntityId::new("code:py-tools/train.py::function::load_dataset");
    let rs_target =
        EntityId::new("code:rust-bench/src/synthetic_recall.rs::function::load_dataset");
    // The Rust target IS the right one (intra-file call).
    assert!(store.has_call_edge(&rs_caller, &rs_target));
    assert!(
        !store.has_call_edge(&rs_caller, &py_target),
        ".rs caller must not bind cross-language to .py free fn"
    );
}

#[test]
fn self_method_does_not_ghost_bind_to_unrelated_free_fn() {
    // Real anatta-rs case: `voronoi_tree::build_or_load` does
    // `Self::load(cache_path)` — an inherent method on the same impl.
    // If the intra-impl pass can't bind it (e.g. method declared in
    // another file via `impl` split), the resolver must NOT silently
    // bind to a global `fn load` defined in some test fixture.
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();

    fs::create_dir_all(root.join("engine/src")).expect("mkdir engine");
    fs::create_dir_all(root.join("ingester/tests")).expect("mkdir tests");
    fs::write(
        root.join("engine/src/voronoi_tree.rs"),
        "pub struct VoronoiTree;\n\
         impl VoronoiTree { pub fn build_or_load() { Self::load(); } }\n",
    )
    .expect("write engine");
    fs::write(
        root.join("ingester/tests/real_world.rs"),
        "fn load(path: &str) -> Vec<u8> { Vec::new() }\n\
         #[test] fn t() {}\n",
    )
    .expect("write test");

    let store = build_store(root);

    let caller =
        EntityId::new("code:engine/src/voronoi_tree.rs::function::VoronoiTree::build_or_load");
    let target = EntityId::new("code:ingester/tests/real_world.rs::function::load");
    assert!(
        !store.has_call_edge(&caller, &target),
        "Self::load must not bind to test-helper fn load in another crate"
    );
}

#[test]
fn closure_parameter_shadows_must_not_ghost_bind() {
    // Real anatta-rs case: `descend_beam(score: &dyn Fn(...))` calls
    // `score(c)` where `score` is a closure parameter, not a top-level
    // free fn. The resolver must not bind it to a free `pub fn score`
    // in some unrelated module just because their names match.
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();

    fs::create_dir_all(root.join("engine/src")).expect("mkdir engine");
    fs::create_dir_all(root.join("api/src")).expect("mkdir api");
    fs::write(
        root.join("engine/src/sphere_tree.rs"),
        "pub fn descend_beam(score: &dyn Fn(u32) -> f64) {\n\
         \x20   let _s = score(42);\n\
         }\n",
    )
    .expect("write engine");
    fs::write(
        root.join("api/src/graph_ops.rs"),
        "pub fn score(items: &[u32]) -> f64 { 0.0 }\n",
    )
    .expect("write api");

    let store = build_store(root);

    let caller = EntityId::new("code:engine/src/sphere_tree.rs::function::descend_beam");
    let target = EntityId::new("code:api/src/graph_ops.rs::function::score");
    assert!(
        !store.has_call_edge(&caller, &target),
        "closure-parameter `score` must not bind to free fn `score` in another crate"
    );
}

#[test]
fn recursive_self_call_must_not_bind_to_twin_file() {
    // Two files in the same crate each define `fn build_recursive`,
    // each one calling itself recursively. The intra-file linker
    // skips self-calls (caller == callee), leaving the recursion
    // unlinked. Without the same-file-candidate guard the resolver
    // would gladly bind `A::build_recursive → B::build_recursive`
    // (and vice versa) — a textbook false cycle.
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();

    fs::create_dir_all(root.join("research/src")).expect("mkdir");
    fs::write(
        root.join("research/src/sphere_tree.rs"),
        "pub fn build_recursive(n: u32) { if n > 0 { build_recursive(n - 1); } }\n",
    )
    .expect("write a");
    fs::write(
        root.join("research/src/emergent_tree.rs"),
        "pub fn build_recursive(n: u32) { if n > 0 { build_recursive(n - 1); } }\n",
    )
    .expect("write b");

    let store = build_store(root);

    let a = EntityId::new("code:research/src/sphere_tree.rs::function::build_recursive");
    let b = EntityId::new("code:research/src/emergent_tree.rs::function::build_recursive");
    assert!(
        !store.has_call_edge(&a, &b),
        "recursive build_recursive must not cross-link to emergent_tree's twin"
    );
    assert!(
        !store.has_call_edge(&b, &a),
        "and the reverse direction is just as wrong"
    );
}

#[test]
fn twin_files_in_same_crate_must_not_cross_link() {
    // Real anatta-rs case: `voronoi_tree.rs` and `voronoi_tree_full.rs`
    // each define a private `fn l2_sq`. Methods in each file call bare
    // `l2_sq(...)` — the parser links to the *local* fn. Without this
    // guard the resolver then excludes the local from `viable` (it's
    // already in `existing`) and binds to the OTHER file's same-named
    // helper via the same-crate fallback. The result is a false cycle
    // between two unrelated tree implementations.
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();

    fs::create_dir_all(root.join("engine/src")).expect("mkdir");
    fs::write(
        root.join("engine/src/voronoi_tree.rs"),
        "fn l2_sq(a: u32, b: u32) -> u32 { a * b }\n\
         pub fn descend() { l2_sq(1, 2); }\n",
    )
    .expect("write a");
    fs::write(
        root.join("engine/src/voronoi_tree_full.rs"),
        "fn l2_sq(a: u32, b: u32) -> u32 { a * b }\n\
         pub fn descend() { l2_sq(1, 2); }\n",
    )
    .expect("write b");

    let store = build_store(root);

    let a_descend = EntityId::new("code:engine/src/voronoi_tree.rs::function::descend");
    let b_descend = EntityId::new("code:engine/src/voronoi_tree_full.rs::function::descend");
    let a_local = EntityId::new("code:engine/src/voronoi_tree.rs::function::l2_sq");
    let b_local = EntityId::new("code:engine/src/voronoi_tree_full.rs::function::l2_sq");
    assert!(store.has_call_edge(&a_descend, &a_local), "intra-file A");
    assert!(store.has_call_edge(&b_descend, &b_local), "intra-file B");
    assert!(
        !store.has_call_edge(&a_descend, &b_local),
        "must not cross-link to twin file's same-named helper"
    );
    assert!(
        !store.has_call_edge(&b_descend, &a_local),
        "must not cross-link to twin file's same-named helper (reverse)"
    );
}

#[test]
fn twin_files_with_same_local_helpers_must_not_cross_link() {
    // Real anatta-rs case: `parse_python.rs` and `parse_rust.rs` test
    // files (Rust!) each define their own `fn find_atom`/`fn parse_fixture`.
    // The parser links each file's calls to its OWN local helpers; the
    // resolver must not then add a SECOND edge to the OTHER file's
    // same-named helper via the cross-crate fallback.
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();

    fs::create_dir_all(root.join("tests")).expect("mkdir");
    fs::write(
        root.join("tests/parse_python.rs"),
        "fn find_atom() {}\n\
         fn run() { find_atom(); }\n",
    )
    .expect("write py");
    fs::write(
        root.join("tests/parse_rust.rs"),
        "fn find_atom() {}\n\
         fn run() { find_atom(); }\n",
    )
    .expect("write rs");

    let store = build_store(root);

    let py_run = EntityId::new("code:tests/parse_python.rs::function::run");
    let py_local = EntityId::new("code:tests/parse_python.rs::function::find_atom");
    let rs_local = EntityId::new("code:tests/parse_rust.rs::function::find_atom");
    assert!(
        store.has_call_edge(&py_run, &py_local),
        "intra-file call must still link locally"
    );
    assert!(
        !store.has_call_edge(&py_run, &rs_local),
        "twin file's same-named helper must not be cross-linked"
    );
}

#[test]
fn rust_qualified_owner_method_call_links() {
    // `Owner::method(args)` (captured verbatim as scoped_identifier) must
    // resolve to the lifted method via the methods index.
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();

    fs::create_dir_all(root.join("src")).expect("mkdir");
    fs::write(
        root.join("src/cli.rs"),
        "pub fn caller() { DaemonHandle::send(); }\n",
    )
    .expect("write cli");
    fs::write(
        root.join("src/daemon.rs"),
        "pub struct DaemonHandle;\n\
         impl DaemonHandle { pub fn send() {} }\n",
    )
    .expect("write daemon");

    let store = build_store(root);

    let caller = EntityId::new("code:src/cli.rs::function::caller");
    let target = EntityId::new("code:src/daemon.rs::function::DaemonHandle::send");
    assert!(store.has_call_edge(&caller, &target));
}

#[test]
fn python_class_method_lifted_with_parent() {
    // Python class methods become first-class function atoms with
    // `parent = Some(class_name)` so cross-module qualifier-based
    // resolution works the same way as Rust impl methods.
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();

    fs::create_dir_all(root.join("pkg")).expect("mkdir");
    fs::write(
        root.join("pkg/foo.py"),
        "class Foo:\n\
         \x20   def method(self):\n\
         \x20       pass\n",
    )
    .expect("write foo");

    let store = build_store(root);

    let method_id = EntityId::new("code:pkg/foo.py::function::Foo::method");
    let atom = store
        .get_atom(&method_id)
        .expect("class method must be lifted");
    assert_eq!(atom.kind, "function");
    assert_eq!(atom.name, "method");
    assert_eq!(atom.parent.as_deref(), Some("Foo"));
}

#[test]
fn python_bare_receiver_call_does_not_ghost_bind_to_class_method() {
    // Same shape as the Rust ghost-edge test, but in Python: caller writes
    // `obj.fetch()` (no receiver type known). Some other module defines
    // `class Service: def fetch(self): ...`. The resolver must not link.
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();

    fs::create_dir_all(root.join("pkg")).expect("mkdir");
    fs::write(
        root.join("pkg/caller.py"),
        "def runner(obj):\n\
         \x20   obj.fetch()\n",
    )
    .expect("write caller");
    fs::write(
        root.join("pkg/service.py"),
        "class Service:\n\
         \x20   def fetch(self):\n\
         \x20       return 1\n",
    )
    .expect("write service");

    let store = build_store(root);

    let runner = EntityId::new("code:pkg/caller.py::function::runner");
    let target = EntityId::new("code:pkg/service.py::function::Service::fetch");
    assert!(
        store.get_atom(&target).is_some(),
        "Service.fetch must be lifted as a method atom"
    );
    assert!(
        !store.has_call_edge(&runner, &target),
        "bare receiver `obj.fetch()` must not bind to Service::fetch"
    );
}

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

    assert!(
        elapsed.as_secs_f64() < 1.0,
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
