//! What the knowledge-graph view relies on: `pub use` re-exports followed
//! to the defining file, `super::` / crate-name paths, receiver calls typed
//! from declarations, calls written inside macros, and the `Imports` /
//! `Uses` edges.

mod common;

use std::fs;
use std::path::Path;

use ast_to_mermaid::graph::Store;
use ast_to_mermaid::model::{EdgeKind, EntityId};

use common::build_store;

fn write(root: &Path, rel: &str, src: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, src).expect("write");
}

fn id(s: &str) -> EntityId {
    EntityId::new(s)
}

fn has_edge(store: &Store, from: &str, to: &str, kind: &EdgeKind) -> bool {
    store
        .edges_from(&id(from))
        .iter()
        .any(|e| e.to == id(to) && e.kind == *kind)
}

fn externs(store: &Store) -> Vec<String> {
    store
        .atoms_by_kind("extern")
        .into_iter()
        .map(|a| a.id.as_str().to_owned())
        .collect()
}

#[test]
fn reexported_function_resolves_to_its_defining_file() {
    // `crate::render::draw_dot` names `render/dot.rs::draw_dot` through
    // `pub use dot::draw_dot;` in `render/mod.rs`.
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();
    write(root, "src/lib.rs", "pub mod render;\npub mod cli;\n");
    write(
        root,
        "src/render/mod.rs",
        "mod dot;\npub use dot::draw_dot;\n",
    );
    write(root, "src/render/dot.rs", "pub fn draw_dot() {}\n");
    write(
        root,
        "src/cli.rs",
        "use crate::render::draw_dot;\npub fn run() { draw_dot(); }\n",
    );

    let store = build_store(root);
    assert!(store.has_call_edge(
        &id("code:src/cli.rs::function::run"),
        &id("code:src/render/dot.rs::function::draw_dot"),
    ));
    assert!(externs(&store).is_empty(), "{:?}", externs(&store));
}

#[test]
fn reexport_chain_with_glob_reaches_the_file_from_a_binary() {
    // bin → `my_crate::cli::run_x` → lib `pub use cli::*` is not needed
    // here; cli/mod.rs `pub use run::run_x` → run/mod.rs `pub use
    // x::run_x` → run/x.rs. The crate's own name is unknown to a2m and
    // must not become an extern.
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();
    write(root, "src/lib.rs", "pub mod cli;\npub use cli::*;\n");
    write(
        root,
        "src/cli/mod.rs",
        "pub mod run;\npub use run::run_x;\n",
    );
    write(root, "src/cli/run/mod.rs", "mod x;\npub use x::run_x;\n");
    write(root, "src/cli/run/x.rs", "pub fn run_x() {}\n");
    write(
        root,
        "src/bin/tool.rs",
        "use my_crate::cli::run_x;\nfn main() { run_x(); }\n",
    );
    write(
        root,
        "src/bin/other.rs",
        "fn main() { my_crate::run_x(); }\n",
    );

    let store = build_store(root);
    let target = id("code:src/cli/run/x.rs::function::run_x");
    assert!(store.has_call_edge(&id("code:src/bin/tool.rs::function::main"), &target));
    assert!(
        !externs(&store).iter().any(|e| e.contains("my_crate::cli")),
        "{:?}",
        externs(&store)
    );
}

#[test]
fn super_path_resolves_to_the_parent_module() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();
    write(root, "src/lib.rs", "pub mod run;\n");
    write(
        root,
        "src/run/mod.rs",
        "mod analyze;\npub fn check_ref() {}\n",
    );
    write(
        root,
        "src/run/analyze.rs",
        "use super::check_ref;\npub fn run_analyze() { check_ref(); }\n",
    );

    let store = build_store(root);
    assert!(store.has_call_edge(
        &id("code:src/run/analyze.rs::function::run_analyze"),
        &id("code:src/run/mod.rs::function::check_ref"),
    ));
}

#[test]
fn typed_receivers_bind_to_the_declared_type() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();
    write(
        root,
        "src/store.rs",
        "pub struct Store;\n\
         impl Store {\n\
         \x20   pub fn new() -> Self { Store }\n\
         \x20   pub fn add(&self) {}\n\
         \x20   pub fn get(&self) {}\n\
         }\n",
    );
    write(
        root,
        "src/app.rs",
        "use crate::store::Store;\n\
         pub struct App { store: Store }\n\
         impl App {\n\
         \x20   pub fn run(&self) { self.store.add(); }\n\
         }\n\
         pub fn with_let() { let s = Store::new(); s.get(); }\n\
         pub fn with_param(s: &Store) { s.add(); }\n\
         pub fn with_option(o: Option<&Store>) { if let Some(s) = o { s.get(); } }\n\
         pub fn untyped(s: impl Fn()) { s.add(); }\n\
         pub fn std_only() { let mut v: Vec<u8> = Vec::new(); v.push(1); v.truncate(0); }\n",
    );

    let store = build_store(root);
    let add = "code:src/store.rs::function::Store::add";
    let get = "code:src/store.rs::function::Store::get";
    let calls = |from: &str, to: &str| store.has_call_edge(&id(from), &id(to));
    assert!(
        calls("code:src/app.rs::function::App::run", add),
        "self.field"
    );
    // `get` is a SKIP_CALLS name: kept because `Store` owns it.
    assert!(
        calls("code:src/app.rs::function::with_let", get),
        "let x = T::new()"
    );
    assert!(calls("code:src/app.rs::function::with_param", add), "x: &T");
    assert!(
        calls("code:src/app.rs::function::with_option", get),
        "if let Some"
    );
    assert!(
        !calls("code:src/app.rs::function::untyped", add),
        "untyped receiver"
    );
    assert!(
        externs(&store).is_empty(),
        "typed std receivers mint no extern: {:?}",
        externs(&store)
    );
}

#[test]
fn self_method_call_across_impl_blocks_does_not_mint_an_extern() {
    // `self.helper()` from a trait impl: the inherent method lives in
    // another impl block of the same type.
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();
    write(
        root,
        "src/point.rs",
        "use std::fmt::Display;\n\
         pub struct Point;\n\
         impl Point { fn helper(&self) {} fn twice(&self) { self.helper(); self.helper(); } }\n\
         impl Display for Point {\n\
         \x20   fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { self.helper(); Ok(()) }\n\
         }\n",
    );
    let store = build_store(root);
    let helper = id("code:src/point.rs::function::Point::helper");
    assert!(store.has_call_edge(&id("code:src/point.rs::function::Point::twice"), &helper));
    assert!(store.has_call_edge(
        &id("code:src/point.rs::function::Display for Point::fmt"),
        &helper
    ));
    assert!(externs(&store).is_empty(), "{:?}", externs(&store));
}

#[test]
fn local_enum_variants_and_types_mint_no_extern() {
    // `Kind::Rust(..)` builds a variant of a local enum; `serde_json::
    // from_str` is a real external call and keeps its extern.
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();
    write(root, "src/kind.rs", "pub enum Kind { Rust(u8) }\n");
    write(
        root,
        "src/make.rs",
        "use crate::kind::Kind;\n\
         pub fn make() { let _ = Kind::Rust(1); let _ = serde_json::from_str(\"1\"); }\n",
    );
    let store = build_store(root);
    assert_eq!(
        externs(&store),
        vec!["extern:serde_json::from_str".to_owned()]
    );
}

#[test]
fn calls_inside_macros_are_recorded() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();
    write(root, "src/fmt.rs", "pub fn summary() -> u8 { 1 }\n");
    write(
        root,
        "src/main_loop.rs",
        "use crate::fmt::summary;\n\
         pub fn report() { println!(\"{}\", summary()); }\n",
    );
    let store = build_store(root);
    assert!(store.has_call_edge(
        &id("code:src/main_loop.rs::function::report"),
        &id("code:src/fmt.rs::function::summary"),
    ));
}

#[test]
fn imports_and_type_uses_become_edges() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();
    write(
        root,
        "src/lib.rs",
        "pub mod graph;\npub mod model;\npub mod render;\n",
    );
    write(
        root,
        "src/graph/mod.rs",
        "mod memory;\npub use memory::Store;\n",
    );
    write(root, "src/graph/memory.rs", "pub struct Store;\n");
    write(root, "src/model.rs", "pub struct Atom;\n");
    write(
        root,
        "src/render.rs",
        "use crate::graph::Store;\n\
         use crate::model;\n\
         pub struct View { atom: model::Atom }\n\
         pub fn draw(s: &Store) -> View { todo!() }\n\
         #[cfg(test)]\nmod tests { use super::*; }\n",
    );
    let store = build_store(root);
    // The import of a re-exported name lands on the defining file.
    assert!(has_edge(
        &store,
        "code:src/render.rs",
        "code:src/graph/memory.rs",
        &EdgeKind::Imports
    ));
    assert!(has_edge(
        &store,
        "code:src/render.rs",
        "code:src/model.rs",
        &EdgeKind::Imports
    ));
    // `use super::*` in an inline test module is not a file import.
    assert!(!has_edge(
        &store,
        "code:src/render.rs",
        "code:src/lib.rs",
        &EdgeKind::Imports
    ));
    assert!(has_edge(
        &store,
        "code:src/render.rs::function::draw",
        "code:src/graph/memory.rs::struct::Store",
        &EdgeKind::Uses
    ));
    assert!(has_edge(
        &store,
        "code:src/render.rs::function::draw",
        "code:src/render.rs::struct::View",
        &EdgeKind::Uses
    ));
    assert!(has_edge(
        &store,
        "code:src/render.rs::struct::View",
        "code:src/model.rs::struct::Atom",
        &EdgeKind::Uses
    ));
}

#[test]
fn python_typed_receivers_and_package_reexports() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();
    write(root, "pkg/__init__.py", "from .impl import helper\n");
    write(root, "pkg/impl.py", "def helper():\n    pass\n");
    write(
        root,
        "pkg/service.py",
        "class Service:\n\
         \x20   def fetch(self):\n\
         \x20       pass\n",
    );
    write(
        root,
        "pkg/app.py",
        "from pkg import helper\n\
         from pkg.service import Service\n\
         \n\
         class App:\n\
         \x20   def __init__(self):\n\
         \x20       self.svc = Service()\n\
         \x20   def run(self):\n\
         \x20       self.svc.fetch()\n\
         \n\
         def go():\n\
         \x20   s = Service()\n\
         \x20   s.fetch()\n\
         \x20   helper()\n\
         \n\
         def typed(s: Service):\n\
         \x20   s.fetch()\n\
         \n\
         def untyped(s):\n\
         \x20   s.fetch()\n",
    );
    let store = build_store(root);
    let fetch = "code:pkg/service.py::function::Service::fetch";
    let calls = |from: &str, to: &str| store.has_call_edge(&id(from), &id(to));
    assert!(
        calls("code:pkg/app.py::function::App::run", fetch),
        "self.x = Cls()"
    );
    assert!(calls("code:pkg/app.py::function::go", fetch), "x = Cls()");
    assert!(calls("code:pkg/app.py::function::typed", fetch), "x: Cls");
    assert!(
        !calls("code:pkg/app.py::function::untyped", fetch),
        "untyped"
    );
    assert!(
        calls(
            "code:pkg/app.py::function::go",
            "code:pkg/impl.py::function::helper"
        ),
        "re-exported by pkg/__init__.py"
    );
    assert!(has_edge(
        &store,
        "code:pkg/app.py",
        "code:pkg/service.py",
        &EdgeKind::Imports
    ));
    assert!(has_edge(
        &store,
        "code:pkg/__init__.py",
        "code:pkg/impl.py",
        &EdgeKind::Imports
    ));
}

#[test]
fn calls_through_trait_objects_and_bounds_resolve_to_the_trait_method() {
    // #198: `store.get()` with `store: &dyn Store`, `&impl Store`,
    // `Box<dyn Store + Send>`, `S: Store`, `where S: Store`, or a field
    // typed by a bounded generic.
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();
    write(
        root,
        "src/t.rs",
        "pub trait Store { fn get(&self) -> u8; fn put(&self) {} }\n",
    );
    write(
        root,
        "src/d.rs",
        "use crate::t::Store;\n\
         pub fn by_dyn(s: &dyn Store) -> u8 { s.get() }\n\
         pub fn by_impl(s: &impl Store) -> u8 { s.get() }\n\
         pub fn by_box(s: Box<dyn Store + Send>) { s.put() }\n\
         pub fn by_bound<S: Store>(s: &S) -> u8 { s.get() }\n\
         pub fn by_where<S>(s: &S) where S: Clone + Store { s.put() }\n\
         pub struct Svc<S: Store> { store: S }\n\
         impl<S: Store> Svc<S> { pub fn run(&self) -> u8 { self.store.get() } }\n",
    );
    let store = build_store(root);
    let get = "code:src/t.rs::function::Store::get";
    let put = "code:src/t.rs::function::Store::put";
    let calls = |from: &str, to: &str| store.has_call_edge(&id(from), &id(to));
    assert!(calls("code:src/d.rs::function::by_dyn", get), "&dyn");
    assert!(calls("code:src/d.rs::function::by_impl", get), "&impl");
    assert!(
        calls("code:src/d.rs::function::by_box", put),
        "Box<dyn + Send>"
    );
    assert!(
        calls("code:src/d.rs::function::by_bound", get),
        "<S: Store>"
    );
    assert!(
        calls("code:src/d.rs::function::by_where", put),
        "where S: Store"
    );
    assert!(
        calls("code:src/d.rs::function::Svc<S>::run", get),
        "field S: Store"
    );
    assert!(externs(&store).is_empty(), "{:?}", externs(&store));
}

#[test]
fn impl_in_a_third_file_implements_the_trait_method_by_method() {
    // Type, trait and `impl Trait for Type` in three different files.
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path();
    write(root, "src/a.rs", "pub struct S;\n");
    write(root, "src/t.rs", "pub trait T { fn f(&self); }\n");
    write(
        root,
        "src/b.rs",
        "use crate::{a::S, t::T};\nimpl T for S { fn f(&self) {} }\n",
    );
    let store = build_store(root);
    assert!(has_edge(
        &store,
        "code:src/b.rs::impl::T for S",
        "code:src/t.rs::trait::T",
        &EdgeKind::Implements
    ));
    assert!(has_edge(
        &store,
        "code:src/b.rs::function::T for S::f",
        "code:src/t.rs::function::T::f",
        &EdgeKind::Implements
    ));
}
