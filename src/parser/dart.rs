//! Dart-specific extractors: imports and call sites.
//! Driven by tree-sitter queries in `queries/dart/*.scm`.
//!
//! Dart doc comments use `///`, the same convention as Rust, so
//! [`super::rust::doc_comment`] is reused verbatim rather than duplicated
//! here — see the dispatch in [`super::doc_for`].

use std::collections::HashMap;
use tree_sitter::{Node, QueryCursor, StreamingIterator};

use super::ExtractedCalls;
use super::queries;

// ── Import extraction ─────────────────────────────────────────────────────────

/// File-scope Dart imports, split by how they rewrite call sites.
///
/// Mirrors [`super::python::PyImports`]: the cross-module resolver only
/// links calls shaped `<module>::<symbol>`, so we rewrite Dart call sites
/// into that shape using the file's imports.
///
/// Dart URIs come in three shapes and only two are linkable:
///   - `dart:async` — SDK, no module in the graph, skipped
///   - `package:app/models/user.dart` — linkable
///   - `../models/user.dart` — linkable
///
/// In every linkable case the qualifier is the URI's **file stem**
/// (`.../user.dart` → `user`), which is what the resolver's module-name
/// fallback matches against.
#[derive(Debug, Default, Clone)]
pub(super) struct DartImports {
    /// `import '…/user.dart' show User, Role;` → `User` → `user::User`.
    /// Rewrites bare calls (`User()` → `user::User`).
    pub symbols: HashMap<String, String>,
    /// `import '…/user.dart' as models;` → `models` → `user`.
    /// Rewrites `models.fn()` → `user::fn`.
    pub modules: HashMap<String, String>,
}

/// Walk every `import` / `export` directive reachable from `root` and
/// build the [`DartImports`] rewrite maps.
pub(super) fn extract_imports(root: Node, source: &str) -> DartImports {
    let mut out = DartImports::default();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&queries::DART.imports, root, source.as_bytes());
    while let Some(m) = matches.next() {
        for cap in m.captures {
            handle_directive(&cap.node, source, &mut out);
        }
    }
    out
}

/// `import '<uri>' [as <alias>] [show a, b];`
///
/// A bare `import '<uri>';` with neither alias nor `show` binds every
/// public name of the target into scope; we cannot tell which bare call
/// came from where, so it contributes nothing — the same trade-off
/// `python.rs` makes for plain dotted `import a.b.c`.
fn handle_directive(node: &Node, source: &str, out: &mut DartImports) {
    let Some(stem) = uri_stem(node, source) else {
        return;
    };

    let mut cursor = node.walk();
    let mut alias: Option<String> = None;
    let mut shown: Vec<String> = Vec::new();
    for child in descendants(node, &mut cursor) {
        match child.kind() {
            // The `as x` prefix is the directive's `alias` field.
            "identifier" if is_field(node, &child, "alias") => {
                if let Some(t) = node_text(&child, source) {
                    alias = Some(t.to_owned());
                }
            }
            // `show A, B` / `hide A, B`. We only honour `show`: `hide`
            // subtracts from an unknown set, so it tells us nothing about
            // which names are bound.
            "combinator" if combinator_is_show(&child, source) => {
                let mut c2 = child.walk();
                for ident in child.children(&mut c2) {
                    if ident.kind() == "identifier"
                        && let Some(t) = node_text(&ident, source)
                    {
                        shown.push(t.to_owned());
                    }
                }
            }
            _ => {}
        }
    }

    if let Some(a) = alias {
        out.modules.insert(a, stem.clone());
    }
    for s in shown {
        out.symbols.insert(s.clone(), format!("{stem}::{s}"));
    }
}

/// True when a `combinator` node is a `show` (not a `hide`).
fn combinator_is_show(node: &Node, source: &str) -> bool {
    node.utf8_text(source.as_bytes())
        .is_ok_and(|t| t.trim_start().starts_with("show"))
}

/// Whether `child` sits in `parent`'s `field` slot.
fn is_field(parent: &Node, child: &Node, field: &str) -> bool {
    parent
        .child_by_field_name(field)
        .is_some_and(|n| n.id() == child.id())
}

/// Direct children plus one level down — the alias and combinators hang
/// off `import_specification`, not off `library_import` itself.
fn descendants<'a>(node: &'a Node, cursor: &mut tree_sitter::TreeCursor<'a>) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    for child in node.children(cursor) {
        out.push(child);
        let mut inner = child.walk();
        for g in child.children(&mut inner) {
            out.push(g);
        }
    }
    out
}

/// File stem of a directive's URI: `'package:a/models/user.dart'` → `user`.
/// Returns `None` for `dart:` SDK URIs, which have no module in the graph.
fn uri_stem(node: &Node, source: &str) -> Option<String> {
    let raw = find_uri_text(node, source)?;
    let uri = raw.trim_matches(['\'', '"']);
    if uri.starts_with("dart:") {
        return None;
    }
    let file = uri.rsplit('/').next().unwrap_or(uri);
    Some(file.strip_suffix(".dart").unwrap_or(file).to_owned())
}

/// The URI string literal of an import/export directive. The literal is
/// nested a few levels down (`configurable_uri` → `uri` → `string_literal`),
/// so we scan rather than hard-code the chain.
fn find_uri_text<'a>(node: &Node, source: &'a str) -> Option<&'a str> {
    let mut stack = vec![*node];
    while let Some(n) = stack.pop() {
        if n.kind() == "uri" {
            return node_text(&n, source);
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            stack.push(child);
        }
    }
    None
}

// ── Call extraction ───────────────────────────────────────────────────────────

/// Collect call sites under `node`, rewriting them against `imports`.
///
/// Dart's `call_expression` shares Rust's `function:` + `arguments:` field
/// layout, so the callee is either a bare `identifier` or a
/// `member_expression` — the direct analogue of Python's `attribute`.
pub(super) fn extract_calls(
    node: &Node,
    source: &str,
    imports: &DartImports,
    out: &mut ExtractedCalls,
) {
    let query = &queries::DART.calls;
    let mut cursor = QueryCursor::new();
    let bytes = source.as_bytes();
    let mut matches = cursor.matches(query, *node, bytes);
    while let Some(m) = matches.next() {
        for cap in m.captures {
            match cap.node.kind() {
                "identifier" => {
                    let Some(name) = node_text(&cap.node, source) else {
                        continue;
                    };
                    out.calls.push(
                        imports
                            .symbols
                            .get(name)
                            .map_or_else(|| name.to_owned(), String::clone),
                    );
                }
                "member_expression" => {
                    let Some((root, method)) = member_root_method(&cap.node, source) else {
                        continue;
                    };
                    if let Some(module_stem) = imports.modules.get(root) {
                        // `models.fn()` where `models` is an import alias →
                        // resolver-eligible qualified call.
                        out.calls.push(format!("{module_stem}::{method}"));
                    } else {
                        // Unknown receiver type — intra-container linking only.
                        out.method_calls.push(method.to_owned());
                    }
                }
                _ => {}
            }
        }
    }
}

/// From a `member_expression` callee (`obj.method`, `a.b.method`), return
/// `(receiver_root_ident, method_name)`.
///
/// The method is the outermost `property:`; the root is the innermost
/// `object:` once nested member expressions are peeled off. A receiver that
/// is itself a call (`f().g()`) has no stable identifier, so it yields
/// `None` and the call lands in `method_calls`.
fn member_root_method<'a>(node: &Node, source: &'a str) -> Option<(&'a str, &'a str)> {
    let method = node_text(&node.child_by_field_name("property")?, source)?;
    let mut current = node.child_by_field_name("object")?;
    loop {
        match current.kind() {
            "member_expression" => {
                current = current.child_by_field_name("object")?;
            }
            "identifier" => return Some((node_text(&current, source)?, method)),
            _ => return None,
        }
    }
}

fn node_text<'a>(node: &Node, source: &'a str) -> Option<&'a str> {
    node.utf8_text(source.as_bytes()).ok()
}
