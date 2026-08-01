//! Rust-specific extractors: `use` declarations, doc comments, and call
//! sites. Driven by tree-sitter queries in `queries/rust/*.scm`.

use std::collections::HashMap;
use tree_sitter::{Node, QueryCursor, StreamingIterator};

use super::ExtractedCalls;
use super::queries;
use crate::limits::max_ast_depth;
use crate::parser::Language;

// ── Use-import extraction ─────────────────────────────────────────────────────

/// One `use` declaration extracted from Rust source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UseDecl {
    /// Local name introduced into the file's scope (last path segment, or
    /// the alias after `as`). For wildcards this is `"*"`.
    pub local: String,
    /// Full crate-relative path of the imported symbol.
    pub path: String,
    /// `true` when the import is a glob (`use foo::*`).
    pub glob: bool,
}

/// Collect every `use_declaration` reachable from `root` — including `use`
/// items nested inside `mod { … }` blocks. Skips `pub use` re-exports
/// (they don't introduce a name in *this* file's scope).
///
/// Group forms (`use foo::{a, b}`), aliases (`use foo as bar`), wildcards
/// (`use foo::*`), and nested paths are all unfolded into one [`UseDecl`]
/// per leaf by [`flatten_use`].
pub(super) fn extract_use_decls(root: Node, source: &str) -> Vec<UseDecl> {
    let mut out: Vec<UseDecl> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&queries::RUST.uses, root, source.as_bytes());
    while let Some(m) = matches.next() {
        for cap in m.captures {
            let n = cap.node;
            if has_visibility_modifier(n) {
                continue;
            }
            if let Some(arg) = use_argument(&n) {
                flatten_use(arg, "", source, &mut out, 0);
            }
        }
    }
    out
}

/// Convert a [`UseDecl`] list into the `bare_name → qualified_path` lookup
/// table the call extractor uses to expand bare-name calls. Wildcards are
/// dropped (no single name to map).
pub(super) fn use_decls_to_imports(decls: &[UseDecl]) -> HashMap<String, String> {
    decls
        .iter()
        .filter(|d| !d.glob)
        .map(|d| (d.local.clone(), d.path.clone()))
        .collect()
}

fn has_visibility_modifier(n: Node) -> bool {
    let mut cursor = n.walk();
    n.children(&mut cursor)
        .any(|c| c.kind() == "visibility_modifier")
}

fn use_argument<'a>(n: &Node<'a>) -> Option<Node<'a>> {
    let mut cursor = n.walk();
    n.children(&mut cursor).find(|c| {
        !matches!(
            c.kind(),
            "use" | ";" | "visibility_modifier" | "line_comment" | "block_comment"
        )
    })
}

/// Recursively unfold a `use` argument tree into one [`UseDecl`] per
/// imported leaf.
///
/// Bounded by [`max_ast_depth`]: a deeply nested adversarial group form
/// (`use a::{b::{c::{...}}}`) short-circuits at the depth cap with a
/// `tracing::warn!` instead of overflowing the stack.
fn flatten_use(n: Node, prefix: &str, source: &str, out: &mut Vec<UseDecl>, depth: usize) {
    if depth >= max_ast_depth() {
        tracing::warn!(
            depth,
            limit = max_ast_depth(),
            "ast depth limit hit in flatten_use; truncating import unfolding",
        );
        return;
    }
    match n.kind() {
        "identifier" | "self" | "super" | "crate" | "metavariable" => {
            let name = n.utf8_text(source.as_bytes()).unwrap_or("");
            if name.is_empty() {
                return;
            }
            out.push(UseDecl {
                local: name.to_owned(),
                path: join_path(prefix, name),
                glob: false,
            });
        }
        "scoped_identifier" => {
            let text = n.utf8_text(source.as_bytes()).unwrap_or("");
            if text.is_empty() {
                return;
            }
            let local = text.rsplit("::").next().unwrap_or(text).to_owned();
            out.push(UseDecl {
                local,
                path: join_path(prefix, text),
                glob: false,
            });
        }
        "use_as_clause" => {
            let path_node = n.child_by_field_name("path");
            let alias_node = n.child_by_field_name("alias");
            if let (Some(p), Some(a)) = (path_node, alias_node) {
                let path_text = p.utf8_text(source.as_bytes()).unwrap_or("");
                let alias_text = a.utf8_text(source.as_bytes()).unwrap_or("");
                if path_text.is_empty() || alias_text.is_empty() {
                    return;
                }
                out.push(UseDecl {
                    local: alias_text.to_owned(),
                    path: join_path(prefix, path_text),
                    glob: false,
                });
            }
        }
        "scoped_use_list" => {
            let path_node = n.child_by_field_name("path");
            let list_node = n.child_by_field_name("list");
            let path_text = path_node
                .and_then(|p| p.utf8_text(source.as_bytes()).ok())
                .unwrap_or("");
            let combined = join_path(prefix, path_text);
            if let Some(list) = list_node {
                let mut cursor = list.walk();
                for child in list.children(&mut cursor) {
                    if !is_use_punctuation(child) {
                        flatten_use(child, &combined, source, out, depth + 1);
                    }
                }
            }
        }
        "use_list" => {
            let mut cursor = n.walk();
            for child in n.children(&mut cursor) {
                if !is_use_punctuation(child) {
                    flatten_use(child, prefix, source, out, depth + 1);
                }
            }
        }
        "use_wildcard" => {
            let mut cursor = n.walk();
            let path_node = n.children(&mut cursor).find(|c| c.kind() != "*");
            let path_text = path_node
                .and_then(|p| p.utf8_text(source.as_bytes()).ok())
                .unwrap_or("");
            out.push(UseDecl {
                local: "*".to_owned(),
                path: join_path(prefix, path_text),
                glob: true,
            });
        }
        _ => {}
    }
}

fn is_use_punctuation(n: Node) -> bool {
    matches!(n.kind(), "," | "{" | "}" | "::" | "*")
}

fn join_path(prefix: &str, tail: &str) -> String {
    match (prefix.is_empty(), tail.is_empty()) {
        (true, _) => tail.to_owned(),
        (false, true) => prefix.to_owned(),
        (false, false) => format!("{prefix}::{tail}"),
    }
}

// ── Doc comment extraction ────────────────────────────────────────────────────

pub(super) fn doc_comment(source: &str, item_row: usize) -> String {
    // Accept LF, CRLF, *and* bare CR as line terminators so the doc-comment
    // scanner survives even if a caller forgot to run `normalize_eol`.
    let lines: Vec<&str> = split_doc_lines(source);
    let mut doc_lines: Vec<&str> = Vec::new();
    let mut row = item_row;
    loop {
        if row == 0 {
            break;
        }
        row -= 1;
        let line = lines.get(row).copied().unwrap_or("").trim();
        if line.starts_with("///") || line.starts_with("//!") {
            doc_lines.push(line);
        } else {
            break;
        }
    }
    doc_lines.reverse();
    doc_lines
        .iter()
        .map(|l| l.trim_start_matches("///").trim_start_matches("//!").trim())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Split `source` on any of `\r\n`, `\n`, or bare `\r`, treating each as
/// one line terminator (so `\r\n` does not produce a phantom blank line).
fn split_doc_lines(source: &str) -> Vec<&str> {
    let bytes = source.as_bytes();
    let mut out: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                out.push(&source[start..i]);
                i += 1;
                start = i;
            }
            b'\r' => {
                out.push(&source[start..i]);
                i += 1;
                if bytes.get(i) == Some(&b'\n') {
                    i += 1;
                }
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < bytes.len() {
        out.push(&source[start..]);
    }
    out
}

// ── Call extraction ───────────────────────────────────────────────────────────

/// Extract every Rust call site within `node`'s subtree into `out`.
///
/// Bare-name calls (e.g. `foo()` after `use crate::other::foo`) are
/// normalised to their fully-qualified path via `imports`; qualified inline
/// calls (`module::foo`, `crate::path::foo`) are kept verbatim. Receiver-
/// style calls (`obj.method()`) are routed into `out.method_calls` instead
/// of `out.calls` so the cross-module resolver never matches them by name.
pub(super) fn extract_calls(
    node: &Node,
    source: &str,
    imports: &HashMap<String, String>,
    out: &mut ExtractedCalls,
) {
    let query = &queries::RUST.calls;
    // Capture index → name lookup so we can route each match by
    // capture role (`@call.fn`, `@call.field`).
    let cn_call_fn = query.capture_index_for_name("call.fn");
    let cn_call_field = query.capture_index_for_name("call.field");
    let mut cursor = QueryCursor::new();
    let bytes = source.as_bytes();
    let mut matches = cursor.matches(query, *node, bytes);
    while let Some(m) = matches.next() {
        for cap in m.captures {
            let Ok(text) = cap.node.utf8_text(bytes) else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            if Some(cap.index) == cn_call_fn {
                match cap.node.kind() {
                    "identifier" => {
                        // Bare call: expand via use imports if available.
                        let resolved = imports
                            .get(text)
                            .map_or_else(|| text.to_owned(), String::clone);
                        out.push_call(resolved, &cap.node, Language::Rust);
                    }
                    "scoped_identifier" => {
                        // Qualified — keep verbatim so the resolver
                        // can disambiguate by module / type.
                        out.push_call(text.to_owned(), &cap.node, Language::Rust);
                    }
                    _ => {}
                }
            } else if Some(cap.index) == cn_call_field {
                // `obj.method` → `method`. Receiver type is
                // unknown, so this never feeds the resolver — it
                // only powers intra-container `self.method()`
                // linking.
                out.method_calls.push(text.to_owned());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::graph::Store;
    use crate::model::EntityId;
    use crate::parser::CodeParser;
    use tree_sitter::Parser as TsParser;

    fn imports_from(src: &str) -> HashMap<String, String> {
        let mut p = TsParser::new();
        p.set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("rust grammar");
        let tree = p.parse(src, None).expect("parse");
        use_decls_to_imports(&extract_use_decls(tree.root_node(), src))
    }

    fn decls_from(src: &str) -> Vec<UseDecl> {
        let mut p = TsParser::new();
        p.set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("rust grammar");
        let tree = p.parse(src, None).expect("parse");
        extract_use_decls(tree.root_node(), src)
    }

    #[test]
    fn rust_parser_creates_module_and_function() {
        let store = Store::new();
        let src = b"pub fn hello() {}\npub fn world() { hello(); }\n";
        CodeParser::rust()
            .parse(src, "src/lib.rs")
            .expect("parse")
            .apply_to(&store);
        // module + 2 functions = 3 atoms
        assert_eq!(store.atom_count(), 3);
        // module contains 2 functions
        let module_id = EntityId::new("code:src/lib.rs");
        let children = store.children_of(&module_id);
        assert_eq!(children.len(), 2);
    }

    #[test]
    fn rust_intra_file_calls_resolved() {
        let store = Store::new();
        let src = b"fn a() { b(); }\nfn b() {}\n";
        CodeParser::rust()
            .parse(src, "src/lib.rs")
            .expect("parse")
            .apply_to(&store);
        let a = EntityId::new("code:src/lib.rs::function::a");
        let b = EntityId::new("code:src/lib.rs::function::b");
        assert!(store.has_call_edge(&a, &b));
    }

    #[test]
    fn rust_parser_struct_and_trait_atoms() {
        let store = Store::new();
        let src = b"pub struct Foo {}\npub trait Bar {}\n";
        CodeParser::rust()
            .parse(src, "src/types.rs")
            .expect("parse")
            .apply_to(&store);
        assert!(!store.atoms_by_kind("struct").is_empty());
        assert!(!store.atoms_by_kind("trait").is_empty());
    }

    #[test]
    fn rust_parser_enum_impl_const_static_macro() {
        let store = Store::new();
        let src = b"\
pub enum Color { Red }\n\
impl Color { pub fn is_red(&self) -> bool { true } }\n\
const MAX: u32 = 42;\n\
static NAME: &str = \"x\";\n\
macro_rules! say_hi { () => {} }\n\
pub type Alias = u32;\n";
        CodeParser::rust()
            .parse(src, "src/misc.rs")
            .expect("parse")
            .apply_to(&store);
        assert!(!store.atoms_by_kind("enum").is_empty());
        assert!(!store.atoms_by_kind("impl").is_empty());
        assert!(!store.atoms_by_kind("const").is_empty());
        assert!(!store.atoms_by_kind("static").is_empty());
        assert!(!store.atoms_by_kind("macro").is_empty());
        assert!(!store.atoms_by_kind("type_alias").is_empty());
    }

    #[test]
    fn rust_doc_comment_extracted() {
        let store = Store::new();
        let src = b"/// Does something.\npub fn documented() {}\n";
        CodeParser::rust()
            .parse(src, "src/lib.rs")
            .expect("parse")
            .apply_to(&store);
        let id = EntityId::new("code:src/lib.rs::function::documented");
        let atom = store.get_atom(&id).expect("atom");
        assert!(atom.doc.contains("Does something"), "doc={:?}", atom.doc);
    }

    #[test]
    fn rust_impl_for_trait_naming() {
        let store = Store::new();
        let src = b"pub trait Foo {}\npub struct Bar;\nimpl Foo for Bar {}\n";
        CodeParser::rust()
            .parse(src, "src/impl_trait.rs")
            .expect("parse")
            .apply_to(&store);
        // The impl atom should exist with "Foo for Bar" as name
        let atoms = store.atoms_by_kind("impl");
        assert!(!atoms.is_empty(), "expected impl atom");
        assert!(
            atoms
                .iter()
                .any(|a| a.name.contains("Foo") && a.name.contains("Bar")),
            "got names: {:?}",
            atoms.iter().map(|a| &a.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn use_imports_simple_path_records_leaf_name() {
        let map = imports_from("use crate::resolve::resolve_cross_module_calls;\n");
        assert_eq!(
            map.get("resolve_cross_module_calls").map(String::as_str),
            Some("crate::resolve::resolve_cross_module_calls")
        );
    }

    #[test]
    fn use_imports_group_form_unfolds() {
        let map = imports_from("use crate::render::{Level, render};\n");
        assert_eq!(
            map.get("render").map(String::as_str),
            Some("crate::render::render")
        );
        assert_eq!(
            map.get("Level").map(String::as_str),
            Some("crate::render::Level")
        );
    }

    #[test]
    fn use_imports_alias_keeps_alias_as_key() {
        let map = imports_from("use crate::error::Result as Res;\n");
        assert_eq!(
            map.get("Res").map(String::as_str),
            Some("crate::error::Result")
        );
        assert!(!map.contains_key("Result"));
    }

    #[test]
    fn use_imports_nested_group_unfolds_recursively() {
        let map = imports_from("use crate::a::{b::c, d};\n");
        assert_eq!(map.get("c").map(String::as_str), Some("crate::a::b::c"));
        assert_eq!(map.get("d").map(String::as_str), Some("crate::a::d"));
    }

    #[test]
    fn bare_call_expanded_via_use_import() {
        let store = Store::new();
        let src = b"\
use crate::other::helper;\n\
fn caller() { helper(); }\n";
        CodeParser::rust()
            .parse(src, "src/lib.rs")
            .expect("parse")
            .apply_to(&store);
        let id = EntityId::new("code:src/lib.rs::function::caller");
        let atom = store.get_atom(&id).expect("atom");
        // Bare `helper()` should be normalised to the imported full path.
        assert!(
            atom.calls.iter().any(|c| c.name == "crate::other::helper"),
            "calls={:?}",
            atom.calls
        );
    }

    #[test]
    fn qualified_inline_call_keeps_full_path() {
        let store = Store::new();
        let src = b"fn caller() { project::render(s); }\n";
        CodeParser::rust()
            .parse(src, "src/lib.rs")
            .expect("parse")
            .apply_to(&store);
        let id = EntityId::new("code:src/lib.rs::function::caller");
        let atom = store.get_atom(&id).expect("atom");
        assert!(
            atom.calls.iter().any(|c| c.name == "project::render"),
            "calls={:?}",
            atom.calls
        );
    }

    #[test]
    fn intra_file_linker_skips_qualified_calls() {
        // A locally-defined `render` should NOT be linked from a call written
        // as `module::render(...)` — that's a cross-module call.
        let store = Store::new();
        let src = b"\
fn render(s: u32) {}\n\
fn caller() { project::render(0); }\n";
        CodeParser::rust()
            .parse(src, "src/lib.rs")
            .expect("parse")
            .apply_to(&store);
        let caller_id = EntityId::new("code:src/lib.rs::function::caller");
        let local_render = EntityId::new("code:src/lib.rs::function::render");
        assert!(
            !store.has_call_edge(&caller_id, &local_render),
            "qualified call should not bind to same-file function"
        );
    }

    #[test]
    fn use_imports_pub_use_is_skipped() {
        // `pub use` is a re-export, not a binding in this file's scope.
        // The local name should NOT appear in the imports map (otherwise a
        // bare call to `Foo` here would get rewritten to `crate::other::Foo`).
        let map = imports_from("pub use crate::other::Foo;\n");
        assert!(map.is_empty(), "pub use must not bind locally: {map:?}");
    }

    #[test]
    fn use_imports_wildcard_is_recorded_as_glob() {
        let decls = decls_from("use crate::prelude::*;\n");
        assert_eq!(decls.len(), 1);
        assert_eq!(decls[0].local, "*");
        assert_eq!(decls[0].path, "crate::prelude");
        assert!(decls[0].glob);
        // Wildcards drop out of the bare→qualified map (no specific name to
        // bind), so the call extractor doesn't accidentally rewrite something.
        let map = imports_from("use crate::prelude::*;\n");
        assert!(map.is_empty());
    }

    #[test]
    fn use_imports_recurse_into_inline_modules() {
        // tree-sitter parses `mod inner { use … }` with the use_declaration
        // nested under the mod's body. The walker must descend.
        let src = "\
mod inner {\n\
    use crate::other::helper;\n\
}\n";
        let map = imports_from(src);
        assert_eq!(
            map.get("helper").map(String::as_str),
            Some("crate::other::helper")
        );
    }

    #[test]
    fn use_imports_self_super_crate_path_roots() {
        let src = "\
use self::sub::a;\n\
use super::sib::b;\n\
use crate::root::c;\n";
        let map = imports_from(src);
        assert_eq!(map.get("a").map(String::as_str), Some("self::sub::a"));
        assert_eq!(map.get("b").map(String::as_str), Some("super::sib::b"));
        assert_eq!(map.get("c").map(String::as_str), Some("crate::root::c"));
    }

    #[test]
    fn impl_methods_become_first_class_atoms() {
        let store = Store::new();
        let src = b"\
pub struct Foo;\n\
impl Foo {\n\
    pub fn build(&self) -> u32 { 0 }\n\
    pub fn update(&self) {}\n\
}\n";
        CodeParser::rust()
            .parse(src, "src/foo.rs")
            .expect("parse")
            .apply_to(&store);
        // Expect: 1 module + 1 struct + 1 impl + 2 method = 5 atoms.
        assert_eq!(store.atom_count(), 5, "impl methods must produce atoms");
        let build_id = EntityId::new("code:src/foo.rs::function::Foo::build");
        let update_id = EntityId::new("code:src/foo.rs::function::Foo::update");
        let build = store.get_atom(&build_id).expect("build atom");
        let update = store.get_atom(&update_id).expect("update atom");
        assert_eq!(build.kind, "function");
        assert_eq!(build.name, "build");
        assert_eq!(update.name, "update");
    }

    #[test]
    fn impl_method_atoms_are_contained_by_their_impl() {
        let store = Store::new();
        let src = b"\
pub struct Foo;\n\
impl Foo { pub fn build(&self) {} }\n";
        CodeParser::rust()
            .parse(src, "src/foo.rs")
            .expect("parse")
            .apply_to(&store);
        let impl_id = EntityId::new("code:src/foo.rs::impl::Foo");
        let build_id = EntityId::new("code:src/foo.rs::function::Foo::build");
        let children = store.children_of(&impl_id);
        assert!(
            children.contains(&build_id),
            "impl→method Contains edge missing: {children:?}"
        );
    }

    #[test]
    fn intra_impl_calls_resolve_directly() {
        // `update` calls `build` — both methods of the same impl. The
        // file-wide name lookup can't disambiguate "build" because every
        // impl might define it; the impl-local linker resolves it directly.
        let store = Store::new();
        let src = b"\
pub struct Foo;\n\
impl Foo {\n\
    pub fn build(&self) -> u32 { 0 }\n\
    pub fn update(&self) { let _ = self.build(); }\n\
}\n";
        CodeParser::rust()
            .parse(src, "src/foo.rs")
            .expect("parse")
            .apply_to(&store);
        let update_id = EntityId::new("code:src/foo.rs::function::Foo::update");
        let build_id = EntityId::new("code:src/foo.rs::function::Foo::build");
        assert!(
            store.has_call_edge(&update_id, &build_id),
            "intra-impl method call not linked"
        );
    }

    #[test]
    fn methods_in_different_impls_do_not_collide() {
        // `Foo::new` and `Bar::new` both exist; a call to `new` inside
        // `Foo::build` must NOT silently resolve to `Bar::new` (and vice
        // versa). The intra-impl linker only sees this impl's methods.
        let store = Store::new();
        let src = b"\
pub struct Foo;\n\
pub struct Bar;\n\
impl Foo {\n\
    pub fn new() -> Self { Self }\n\
    pub fn build() { let _ = Self::new(); }\n\
}\n\
impl Bar {\n\
    pub fn new() -> Self { Self }\n\
}\n";
        CodeParser::rust()
            .parse(src, "src/foo.rs")
            .expect("parse")
            .apply_to(&store);
        let foo_build = EntityId::new("code:src/foo.rs::function::Foo::build");
        let foo_new = EntityId::new("code:src/foo.rs::function::Foo::new");
        let bar_new = EntityId::new("code:src/foo.rs::function::Bar::new");
        assert!(store.has_call_edge(&foo_build, &foo_new));
        assert!(!store.has_call_edge(&foo_build, &bar_new));
    }

    #[test]
    fn trait_impl_methods_use_full_impl_name_in_id() {
        // `impl Display for Foo` produces an impl named "Display for Foo";
        // its methods get ids prefixed with that full string.
        let store = Store::new();
        let src = b"\
pub struct Foo;\n\
pub trait Display { fn fmt(&self); }\n\
impl Display for Foo { fn fmt(&self) {} }\n";
        CodeParser::rust()
            .parse(src, "src/foo.rs")
            .expect("parse")
            .apply_to(&store);
        let fmt_id = EntityId::new("code:src/foo.rs::function::Display for Foo::fmt");
        let fmt = store
            .get_atom(&fmt_id)
            .expect("trait-impl method atom must exist");
        assert_eq!(fmt.name, "fmt");
    }
}
