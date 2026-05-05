//! Cross-language guards: the resolver must filter call candidates by source
//! language so a `.py` caller never resolves to a `.rs` free fn (and the
//! reverse direction).

mod common;

use std::fs;

use ast_to_mermaid::model::EntityId;

use common::build_store;

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
