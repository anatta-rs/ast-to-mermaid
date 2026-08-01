//! `a2m flow` must account for every call site a body records, not only
//! the ones the resolver managed to bind.
//!
//! Walking the resolved edges alone hid most of what an entry point does
//! — on a real Flutter `main`, 4 calls out of 10 recorded. These tests
//! run the full parse → resolve → render pipeline on each supported
//! language, because the gap only appears once real edges exist: a unit
//! test with a hand-built store cannot show that an edge and a leaf
//! disagree about the same body.

mod common;

use std::fs;

use ast_to_mermaid::render::AdjMaps;
use ast_to_mermaid::render::flow::{External, render};
use ast_to_mermaid::render::snapshot::AtomSnapshot;

use common::build_store;

/// Render the flow from `target` over a tree written from `files`.
fn flow_of(files: &[(&str, &str)], target: &str, external: External) -> String {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();
    for (path, body) in files {
        let full = root.join(path);
        if let Some(dir) = full.parent() {
            fs::create_dir_all(dir).expect("mkdir");
        }
        fs::write(&full, body).expect("write");
    }
    let store = build_store(root);
    let adj = AdjMaps::build(&store);
    store
        .with_atoms(|atoms| {
            let snap = AtomSnapshot::build(atoms);
            render(&adj, &snap, target, 3, external)
        })
        .expect("render")
}

#[test]
fn rust_unresolved_call_is_shown_as_a_leaf() {
    let out = flow_of(
        &[(
            "src/main.rs",
            "fn helper() {}\n\
             fn main() {\n\
             \x20   helper();\n\
             \x20   some_unknown_crate::launch();\n\
             }\n",
        )],
        "main",
        External::NearOnly,
    );
    assert!(
        out.contains("[\"helper\"]") && !out.contains("[\"helper\"]:::unresolved"),
        "the resolved call stays a plain node:\n{out}"
    );
    assert!(
        out.contains(":::unresolved") || out.contains(":::external"),
        "the unknown call must appear as a leaf, not vanish:\n{out}"
    );
}

#[test]
fn python_unresolved_call_is_shown_as_a_leaf() {
    let out = flow_of(
        &[(
            "pkg/app.py",
            "def helper():\n\
             \x20   pass\n\
             \n\
             def main():\n\
             \x20   helper()\n\
             \x20   launch_rocket()\n",
        )],
        "main",
        External::NearOnly,
    );
    assert!(out.contains("[\"helper\"]"), "{out}");
    assert!(
        out.contains("[\"launch_rocket\"]:::unresolved"),
        "the unbound call must be drawn:\n{out}"
    );
}

#[test]
fn dart_unresolved_call_is_shown_as_a_leaf() {
    let out = flow_of(
        &[(
            "lib/main.dart",
            "Future<void> initThing() async {}\n\
             \n\
             void main() async {\n\
             \x20 WidgetsFlutterBinding.ensureInitialized();\n\
             \x20 await initThing();\n\
             \x20 runApp(const MyApp());\n\
             }\n",
        )],
        "main",
        External::NearOnly,
    );
    assert!(out.contains("[\"initThing\"]"), "{out}");
    assert!(
        out.contains("[\"runApp\"]:::unresolved"),
        "a bare SDK call has no qualifier, so nothing but a leaf can \
         carry it:\n{out}"
    );
}

/// The regression that motivated the change: every call the body records
/// is accounted for exactly once — as an edge or as a leaf, never both
/// and never neither.
#[test]
fn every_recorded_call_is_accounted_for_exactly_once() {
    let out = flow_of(
        &[(
            "lib/main.dart",
            "Future<void> initThing() async {}\n\
             \n\
             void main() async {\n\
             \x20 WidgetsFlutterBinding.ensureInitialized();\n\
             \x20 await initThing();\n\
             \x20 runApp(const MyApp());\n\
             }\n",
        )],
        "main",
        External::NearOnly,
    );
    for name in ["ensureInitialized", "initThing", "runApp"] {
        let hits = out.lines().filter(|l| l.contains(name)).count();
        assert!(hits > 0, "`{name}` is missing entirely:\n{out}");
    }
    // `initThing` resolves, so it must not also appear as a leaf.
    assert!(
        !out.contains("unresolved") || !out.contains("::initThing\"]"),
        "a resolved call must not be duplicated as a leaf:\n{out}"
    );
}

#[test]
fn external_never_hides_every_leaf() {
    let out = flow_of(
        &[(
            "lib/main.dart",
            "void main() {\n\
             \x20 runApp(const MyApp());\n\
             }\n",
        )],
        "main",
        External::Never,
    );
    assert!(!out.contains(":::unresolved"), "{out}");
}
