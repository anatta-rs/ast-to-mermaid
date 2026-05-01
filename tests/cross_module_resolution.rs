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
use ast_to_mermaid::model::EntityId;
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
