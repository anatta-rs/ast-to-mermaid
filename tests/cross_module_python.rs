//! Python-side cross-module resolution: class-method lifting, and the
//! receiver-method ghost-bind guard for `obj.fetch()` against an unrelated
//! `class.fetch`.

mod common;

use std::fs;

use ast_to_mermaid::model::EntityId;

use common::build_store;

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
