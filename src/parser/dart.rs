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
use crate::parser::Language;

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
    /// Declared types of the identifiers this file binds, used to qualify
    /// `obj.method()`. Lives here rather than in a second parameter so a
    /// file's scope stays one value threaded through one call.
    pub receivers: ReceiverTypes,
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
    let mut out = DartImports {
        receivers: collect_receiver_types(root, source),
        ..DartImports::default()
    };
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
            // The `as x` prefix. `alias` is a field of the enclosing
            // `import_specification`, *not* of the `library_import` — so
            // the parent has to be asked, not the directive root.
            "identifier"
                if child
                    .parent()
                    .is_some_and(|p| is_field(&p, &child, "alias")) =>
            {
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

// ── Receiver types ────────────────────────────────────────────────────────────

/// Declared types of the identifiers a file binds, used to turn
/// `obj.method()` into a qualified `Type::method`.
///
/// Scope is the **file**, not the exact lexical scope. Following nested
/// scopes (function → block → closure) would be more faithful but much
/// heavier, and the same name rarely carries two types in one Dart file.
/// Precision is traded for a guard instead: a name seen with two different
/// types becomes ambiguous and is never used, the same unicity rule
/// `resolve.rs` applies to candidates.
#[derive(Debug, Default, Clone)]
pub(super) struct ReceiverTypes {
    /// `name → Some(type)` when unambiguous, `name → None` once a second,
    /// different type has been seen for it.
    types: HashMap<String, Option<String>>,
}

impl ReceiverTypes {
    /// Type bound to `name`, or `None` when unknown or ambiguous.
    fn get(&self, name: &str) -> Option<&str> {
        self.types.get(name)?.as_deref()
    }

    fn insert(&mut self, name: &str, ty: &str) {
        match self.types.get_mut(name) {
            // Second, different type for this name — poison the entry
            // rather than pick a side.
            Some(slot @ Some(_)) if slot.as_deref() != Some(ty) => *slot = None,
            Some(_) => {}
            None => {
                self.types.insert(name.to_owned(), Some(ty.to_owned()));
            }
        }
    }
}

/// Walk `root` and record every identifier whose type the syntax states.
///
/// Four shapes carry a type without any semantic analysis:
///   - class fields — `declaration` with a `type` sibling and an
///     `initialized_identifier_list`;
///   - parameters — `formal_parameter` with `type` + `name:`;
///   - annotated locals — `initialized_variable_definition` with `type`;
///   - constructed locals — the same node without a `type`, where the
///     initialiser is a constructor call (`final buf = StringBuffer()`).
pub(super) fn collect_receiver_types(root: Node, source: &str) -> ReceiverTypes {
    let mut out = ReceiverTypes::default();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "declaration" => collect_field_types(&node, source, &mut out),
            "formal_parameter" => {
                if let (Some(ty), Some(name)) = (
                    first_type_name(&node, source),
                    node.child_by_field_name("name")
                        .and_then(|n| node_text(&n, source)),
                ) {
                    out.insert(name, ty);
                }
            }
            "initialized_variable_definition" => {
                let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|n| node_text(&n, source))
                else {
                    continue;
                };
                if let Some(ty) = first_type_name(&node, source) {
                    out.insert(name, ty);
                } else if let Some(ty) = constructed_type(&node, source) {
                    out.insert(name, ty);
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    out
}

/// `final UserDao dao;` — the type is a sibling of the identifier list
/// rather than a field of either, so both are read off the `declaration`.
fn collect_field_types(node: &Node, source: &str, out: &mut ReceiverTypes) {
    let Some(ty) = first_type_name(node, source) else {
        return;
    };
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "initialized_identifier_list" {
            continue;
        }
        let mut inner = child.walk();
        for ident in child.children(&mut inner) {
            if ident.kind() != "initialized_identifier" {
                continue;
            }
            if let Some(name) = ident
                .child_by_field_name("name")
                .and_then(|n| node_text(&n, source))
            {
                out.insert(name, ty);
            }
        }
    }
}

/// First `type_identifier` under `node`'s `type` child.
///
/// `List<Foo> xs` yields `List`, not `Foo` — the receiver's own type is
/// what a later `xs.add()` needs. `List` is not in the graph, so nothing
/// is emitted for it, which is the correct outcome rather than a wrong one.
fn first_type_name<'a>(node: &Node, source: &'a str) -> Option<&'a str> {
    let mut cursor = node.walk();
    let ty = node.children(&mut cursor).find(|c| c.kind() == "type")?;
    // Breadth-first, and shallowest-first within a level: `List<Note>`
    // nests `Note` under `type_arguments`, so a depth-first walk reaches
    // the type *argument* before the container and would bind `xs.add()`
    // to `Note` instead of `List`.
    let mut queue = std::collections::VecDeque::from([ty]);
    while let Some(n) = queue.pop_front() {
        if n.kind() == "type_identifier" {
            return node_text(&n, source);
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            queue.push_back(child);
        }
    }
    None
}

/// `final buf = StringBuffer();` — no annotation, but the initialiser is a
/// constructor call and its callee names the type.
fn constructed_type<'a>(node: &Node, source: &'a str) -> Option<&'a str> {
    let value = node.child_by_field_name("value")?;
    if value.kind() != "call_expression" {
        return None;
    }
    let callee = value.child_by_field_name("function")?;
    if callee.kind() != "identifier" {
        return None;
    }
    let name = node_text(&callee, source)?;
    is_type_name(name).then_some(name)
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
                    let resolved = imports
                        .symbols
                        .get(name)
                        .map_or_else(|| name.to_owned(), String::clone);
                    out.push_call(resolved, &cap.node, Language::Dart);
                }
                // `obj.method()` and its null-aware twin `obj?.method()`
                // share a shape; #174 handled the latter in the sequence
                // walker but not here, so it used to fall through to
                // `_ => {}` and vanish from the graph entirely.
                "member_expression" | "null_aware_member_expression" => {
                    let Some((root, method)) = member_root_method(&cap.node, source) else {
                        continue;
                    };
                    if let Some(module_stem) = imports.modules.get(root) {
                        // `models.fn()` where `models` is an import alias →
                        // resolver-eligible qualified call. Checked first:
                        // an alias could in principle be capitalised, and
                        // the import is the stronger signal.
                        out.push_call(
                            format!("{module_stem}::{method}"),
                            &cap.node,
                            Language::Dart,
                        );
                    } else if direct_receiver(&cap.node, source).is_some_and(is_type_name) {
                        // `NotificationService.initialize()` — a leading
                        // capital is Dart's type convention, enforced by
                        // `camel_case_types` in the official lints. This is
                        // the same shape Rust spells `Type::method`, which
                        // the resolver already indexes by `(owner, name)`.
                        //
                        // The receiver must be *direct*. `member_root_method`
                        // collapses a whole chain, so `AppTextStyles.caption
                        // .copyWith()` would report `AppTextStyles` — but
                        // `copyWith` belongs to whatever `caption` is, not to
                        // `AppTextStyles`. Qualifying that invents a method
                        // the class does not have, and the resolver then
                        // manufactures an extern for it.
                        //
                        // Emitting it qualified does not weaken anything
                        // else: the resolver still requires a target that
                        // exists in the graph and is unique, so an SDK class
                        // such as `MediaQuery.of()` resolves to nothing
                        // rather than to something wrong.
                        out.push_call(format!("{root}::{method}"), &cap.node, Language::Dart);
                    } else if let Some(ty) =
                        direct_receiver(&cap.node, source).and_then(|r| imports.receivers.get(r))
                    {
                        // Lowercase receiver whose type the file states —
                        // `final UserDao dao;` then `dao.fetch()`. Same
                        // qualified shape as a capitalised receiver, and the
                        // resolver applies the same unicity rule to it.
                        //
                        // Direct receiver only, for the reason spelled out
                        // above: in `a.b.method()` the method belongs to the
                        // type of `a.b`, which the table does not know.
                        out.push_call(format!("{ty}::{method}"), &cap.node, Language::Dart);
                    } else {
                        // Type unknown or ambiguous. Intra-container linking
                        // only; binding it to a same-named method elsewhere
                        // is exactly the ghost-bind `resolve.rs` refuses.
                        out.method_calls.push(method.to_owned());
                    }
                }
                _ => {}
            }
        }
    }
}

/// The callee's **immediate** receiver, when it is a plain identifier.
///
/// Distinct from [`member_root_method`], which walks a whole chain down to
/// its leftmost identifier. Here only `A.method()` qualifies — in
/// `A.b.method()` the method belongs to the type of `A.b`, which we do not
/// know, so treating `A` as the owner would attribute a method to a class
/// that never declared it.
fn direct_receiver<'a>(node: &Node, source: &'a str) -> Option<&'a str> {
    let object = node.child_by_field_name("object")?;
    if object.kind() != "identifier" {
        return None;
    }
    node_text(&object, source)
}

/// Whether `name` reads as a Dart type rather than an instance.
///
/// Dart's `camel_case_types` lint — on by default through `flutter_lints`
/// and `lints` — makes the leading capital a reliable signal. A private
/// type is `_Foo`, so the underscore is skipped before testing.
fn is_type_name(name: &str) -> bool {
    name.trim_start_matches('_')
        .chars()
        .next()
        .is_some_and(char::is_uppercase)
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
            // Both spellings chain through `object`, so `a?.b.method()`
            // collapses to `a` like its plain counterpart.
            "member_expression" | "null_aware_member_expression" => {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Call names only — tests assert on what is called, the rank and
    /// flags are covered by their own tests.
    fn names(sites: &[crate::model::CallSite]) -> Vec<String> {
        sites.iter().map(|s| s.name.clone()).collect()
    }

    use crate::parser::Language;

    fn imports_of(src: &str) -> DartImports {
        let mut p = tree_sitter::Parser::new();
        p.set_language(&Language::Dart.ts_language())
            .expect("set_language");
        let tree = p.parse(src, None).expect("parse");
        extract_imports(tree.root_node(), src)
    }

    /// `alias` is a field of `import_specification`, not of the
    /// `library_import` above it. Asking the directive root returns `None`
    /// and the alias is dropped, so `models.parseUser()` never rewrites
    /// and the call never resolves.
    #[test]
    fn as_alias_maps_to_the_uri_stem() {
        let i = imports_of("import 'package:demo/models/user.dart' as models;\n");
        assert_eq!(i.modules.get("models").map(String::as_str), Some("user"));
    }

    #[test]
    fn show_binds_each_named_symbol() {
        let i = imports_of("import '../models/user.dart' show User, parseUser;\n");
        assert_eq!(
            i.symbols.get("parseUser").map(String::as_str),
            Some("user::parseUser")
        );
        assert_eq!(
            i.symbols.get("User").map(String::as_str),
            Some("user::User")
        );
    }

    /// `hide` subtracts from a set we don't know, so it must bind nothing
    /// — binding the hidden names would be exactly backwards.
    #[test]
    fn hide_binds_nothing() {
        let i = imports_of("import '../models/user.dart' hide User;\n");
        assert!(i.symbols.is_empty(), "got {:?}", i.symbols);
    }

    /// SDK URIs have no module in the graph; emitting an edge towards
    /// `dart:async` would invent a node that does not exist.
    #[test]
    fn sdk_uris_are_skipped() {
        let i = imports_of("import 'dart:async' as async_lib;\nimport 'dart:math';\n");
        assert!(i.modules.is_empty(), "got {:?}", i.modules);
    }

    /// The three URI shapes that do resolve all key on the file stem.
    #[test]
    fn package_and_relative_uris_key_on_the_file_stem() {
        for src in [
            "import 'package:demo/models/user.dart' as m;\n",
            "import '../models/user.dart' as m;\n",
            "import 'user.dart' as m;\n",
        ] {
            assert_eq!(
                imports_of(src).modules.get("m").map(String::as_str),
                Some("user"),
                "for {src:?}"
            );
        }
    }

    /// `part` / `part_of` merge a file into its parent library rather than
    /// importing it — treating them as imports would duplicate edges.
    #[test]
    fn part_directives_are_not_imports() {
        let i = imports_of("part of 'user.dart';\npart 'user.g.dart';\n");
        assert!(i.modules.is_empty() && i.symbols.is_empty(), "got {i:?}");
    }

    // ── Call extraction ──────────────────────────────────────────────────

    fn calls_of(src: &str) -> ExtractedCalls {
        let mut p = tree_sitter::Parser::new();
        p.set_language(&Language::Dart.ts_language())
            .expect("set_language");
        let tree = p.parse(src, None).expect("parse");
        let root = tree.root_node();
        let imports = extract_imports(root, src);
        let mut out = ExtractedCalls::default();
        extract_calls(&root, src, &imports, &mut out);
        out
    }

    /// Top-level Dart functions had their calls silently dropped for the
    /// whole life of the Dart support: the caller in `parser/mod.rs` only
    /// tested `function_item` and `function_definition`. Methods were
    /// unaffected, which kept the graph populated enough to hide it. This
    /// pins the property at the level where it broke.
    #[test]
    fn a_top_level_function_reports_its_calls() {
        let src = "void main() { helper(); }\nvoid helper() {}\n";
        let unit = super::super::CodeParser::dart()
            .parse(src.as_bytes(), "solo.dart")
            .expect("parse");
        let main_atom = unit
            .atoms
            .iter()
            .find(|a| a.name == "main" && a.kind == "function")
            .expect("main atom");
        assert_eq!(
            names(&main_atom.calls),
            vec!["helper".to_owned()],
            "a top-level function must report what it calls"
        );
    }

    /// A bare call with no matching import stays bare — the resolver's
    /// same-package pass is what links it, not a rewrite here.
    #[test]
    fn bare_call_without_import_is_left_alone() {
        let c = calls_of("void f() { helper(); }\n");
        assert_eq!(names(&c.calls), vec!["helper".to_owned()]);
        assert!(c.method_calls.is_empty());
    }

    /// `show`n symbols rewrite into the `module::symbol` shape the
    /// cross-module resolver understands.
    #[test]
    fn shown_symbol_call_is_rewritten_to_qualified_form() {
        let c = calls_of(
            "import '../models/user.dart' show parseUser;\nvoid f() { parseUser('x'); }\n",
        );
        assert!(
            names(&c.calls).contains(&"user::parseUser".to_owned()),
            "got {:?}",
            c.calls
        );
    }

    /// `models.parseUser()` where `models` is an import alias resolves
    /// through the alias to the target module.
    #[test]
    fn aliased_module_call_is_rewritten() {
        let c = calls_of(
            "import 'package:demo/models/user.dart' as models;\nvoid f() { models.parseUser('x'); }\n",
        );
        assert!(
            names(&c.calls).contains(&"user::parseUser".to_owned()),
            "got {:?}",
            c.calls
        );
    }

    /// An unknown receiver cannot be resolved to a module, so the call
    /// lands in `method_calls` for intra-container linking only — it must
    /// not ghost-bind to a same-named free function.
    #[test]
    fn unknown_receiver_lands_in_method_calls() {
        let c = calls_of("void f() { widget.build(); }\n");
        assert_eq!(c.method_calls, vec!["build".to_owned()]);
        assert!(c.calls.is_empty(), "got {:?}", names(&c.calls));
    }

    /// A capitalised receiver is a type, not an instance — Dart's
    /// `camel_case_types` lint makes that reliable. The call is the same
    /// shape Rust spells `Type::method`, which the resolver already
    /// indexes, so emitting it qualified is what lets it link at all.
    #[test]
    fn capitalised_receiver_is_a_qualified_type_call() {
        let c = calls_of("void f() { NotificationService.initialize(); }\n");
        assert_eq!(
            names(&c.calls),
            vec!["NotificationService::initialize".to_owned()]
        );
        assert!(c.method_calls.is_empty(), "got {:?}", c.method_calls);
    }

    /// Private types are spelled `_Foo`; the underscore must not hide the
    /// capital.
    #[test]
    fn private_type_receiver_is_still_a_type() {
        let c = calls_of("void f() { _CacheEntry.parse('x'); }\n");
        assert_eq!(names(&c.calls), vec!["_CacheEntry::parse".to_owned()]);
    }

    /// An import alias wins over the capital rule: the import is the
    /// stronger signal about where the symbol actually lives.
    #[test]
    fn import_alias_takes_priority_over_the_capital_rule() {
        let c = calls_of(
            "import 'package:demo/models/user.dart' as Models;\nvoid f() { Models.parseUser('x'); }\n",
        );
        assert_eq!(names(&c.calls), vec!["user::parseUser".to_owned()]);
    }

    /// Lowercase receivers whose type the file never states keep the old
    /// behaviour — this is the ghost-bind guard.
    #[test]
    fn lowercase_receiver_of_unknown_type_lands_in_method_calls() {
        let c = calls_of("void f() { notificationService.initialize(); }\n");
        assert_eq!(c.method_calls, vec!["initialize".to_owned()]);
        assert!(c.calls.is_empty(), "got {:?}", names(&c.calls));
    }

    // ── Receiver-type inference ──────────────────────────────────────────

    /// A class field states its type: `final UserDao dao;` then
    /// `dao.fetch()` is as qualified as `UserDao.fetch()` would be.
    #[test]
    fn field_declaration_types_its_receiver() {
        let c = calls_of("class R {\n  final UserDao dao;\n  void run() { dao.fetch(); }\n}\n");
        assert_eq!(names(&c.calls), vec!["UserDao::fetch".to_owned()]);
        assert!(c.method_calls.is_empty(), "got {:?}", c.method_calls);
    }

    /// The single largest source on a real project: 921 typed parameters.
    #[test]
    fn parameter_declaration_types_its_receiver() {
        let c = calls_of("void run(NotifService svc) { svc.notify(); }\n");
        assert_eq!(names(&c.calls), vec!["NotifService::notify".to_owned()]);
    }

    #[test]
    fn annotated_local_types_its_receiver() {
        let c = calls_of("void f() { final Logger log = Logger(); log.warn(); }\n");
        assert!(
            names(&c.calls).contains(&"Logger::warn".to_owned()),
            "got {:?}",
            c.calls
        );
    }

    /// No annotation, but the initialiser names the type.
    #[test]
    fn constructed_local_types_its_receiver() {
        let c = calls_of("void f() { final buf = StringBuffer(); buf.write('x'); }\n");
        assert!(
            names(&c.calls).contains(&"StringBuffer::write".to_owned()),
            "got {:?}",
            c.calls
        );
    }

    /// File scope is a deliberate approximation, so the guard has to hold:
    /// one name with two types is unusable, not a coin flip.
    #[test]
    fn a_name_with_two_types_is_ambiguous_and_unused() {
        let c = calls_of("void a(UserDao x) { x.run(); }\nvoid b(OrderDao x) { x.run(); }\n");
        assert_eq!(c.method_calls, vec!["run".to_owned(), "run".to_owned()]);
        assert!(
            c.calls.is_empty(),
            "ambiguous name must not qualify: {:?}",
            c.calls
        );
    }

    /// Re-declaring the *same* type is not ambiguity.
    #[test]
    fn a_name_repeated_with_one_type_stays_usable() {
        let c = calls_of("void a(UserDao x) { x.run(); }\nvoid b(UserDao x) { x.run(); }\n");
        assert_eq!(
            names(&c.calls),
            vec!["UserDao::run".to_owned(), "UserDao::run".to_owned()]
        );
    }

    /// `List<Foo> xs` binds `xs` to `List`, not `Foo` — a later `xs.add()`
    /// belongs to the container. `List` is not in the graph, so nothing is
    /// emitted, which is right rather than wrong.
    #[test]
    fn generic_type_binds_the_container_not_its_argument() {
        let c = calls_of("void f(List<Note> xs) { xs.add(1); }\n");
        assert_eq!(names(&c.calls), vec!["List::add".to_owned()]);
    }

    /// The chain rule from #184 still applies: only a direct receiver is
    /// typed by the table.
    #[test]
    fn chained_receiver_is_not_typed_from_the_table() {
        let c =
            calls_of("class R {\n  final UserDao dao;\n  void run() { dao.cache.clear(); }\n}\n");
        assert_eq!(c.method_calls, vec!["clear".to_owned()]);
        assert!(c.calls.is_empty(), "got {:?}", names(&c.calls));
    }

    /// `session?.release()` used to hit `_ => {}` and disappear from the
    /// graph entirely — #174 fixed this kind in the sequence walker but
    /// not in the parser.
    #[test]
    fn null_aware_receiver_is_classified_like_its_plain_twin() {
        let lower = calls_of("void f() { session?.release(); }\n");
        assert_eq!(lower.method_calls, vec!["release".to_owned()]);

        let upper = calls_of("void f() { NotificationService?.initialize(); }\n");
        assert_eq!(
            names(&upper.calls),
            vec!["NotificationService::initialize".to_owned()]
        );
    }

    /// A chained null-aware receiver collapses to its leftmost identifier.
    #[test]
    fn chained_null_aware_collapses_to_root() {
        let c = calls_of("void f() { xs?.first?.run(); }\n");
        assert_eq!(c.method_calls, vec!["run".to_owned()]);
    }

    /// The capital rule needs a *direct* receiver. In
    /// `AppTextStyles.caption.copyWith()` the method belongs to whatever
    /// `caption` is — a `TextStyle` from the SDK — not to `AppTextStyles`.
    /// Qualifying it would attribute `copyWith` to a class that never
    /// declared it, and the resolver would mint an extern for the
    /// invented pair. Found on a real project: 164 such externs.
    #[test]
    fn chained_type_receiver_is_not_qualified() {
        let c = calls_of("void f() { AppTextStyles.caption.copyWith(); }\n");
        assert_eq!(c.method_calls, vec!["copyWith".to_owned()]);
        assert!(
            !names(&c.calls)
                .iter()
                .any(|s| s.starts_with("AppTextStyles::")),
            "must not attribute copyWith to AppTextStyles: {:?}",
            c.calls
        );
    }

    /// The direct case still qualifies — this is what the change is for.
    #[test]
    fn direct_type_receiver_is_qualified() {
        let c = calls_of("void f() { AppTextStyles.caption(); }\n");
        assert_eq!(names(&c.calls), vec!["AppTextStyles::caption".to_owned()]);
    }

    /// `a.b.method()` collapses to the leftmost identifier, so an aliased
    /// root still resolves and a non-aliased one still does not.
    #[test]
    fn chained_receiver_collapses_to_its_root() {
        let c = calls_of("void f() { obj.inner.method(); }\n");
        assert_eq!(c.method_calls, vec!["method".to_owned()]);
    }

    /// A receiver that is itself a call has no stable identifier to key
    /// on, and the extractor gives up on the outer call entirely — so
    /// `render` is **dropped**, while the inner `build` still surfaces.
    ///
    /// This mirrors `python.rs::attribute_root_method`, which bails the
    /// same way on a call/subscript/paren receiver. Pinned rather than
    /// fixed: making Dart alone recover the method would diverge the two
    /// extractors, and the fix belongs in both at once. The sequence
    /// walker does not share the limitation — its own receiver walk
    /// descends through `call_expression`.
    #[test]
    fn call_receiver_drops_the_outer_call_like_python_does() {
        let c = calls_of("void f() { build().render(); }\n");
        assert!(
            names(&c.calls).contains(&"build".to_owned()),
            "got {:?}",
            names(&c.calls)
        );
        assert!(
            !c.method_calls.contains(&"render".to_owned()),
            "known limitation shared with python.rs: {c:?}"
        );
    }

    /// Nested calls in arguments are captured — the query matches every
    /// `call_expression` under the walked node, not just the outermost.
    #[test]
    fn nested_argument_calls_are_captured() {
        let c = calls_of("void f() { outer(inner()); }\n");
        assert!(
            names(&c.calls).contains(&"outer".to_owned()),
            "got {:?}",
            names(&c.calls)
        );
        assert!(
            names(&c.calls).contains(&"inner".to_owned()),
            "got {:?}",
            names(&c.calls)
        );
    }

    /// Cascades carry `property:` and no `function:`, so the call query
    /// does not match them. That is deliberate: the sequence walker gives
    /// them their own handler, and matching here would attribute the link
    /// to the wrong receiver.
    #[test]
    fn cascade_links_are_not_call_query_matches() {
        let c = calls_of("void f() { buffer..write('a'); }\n");
        assert!(
            !names(&c.calls).contains(&"write".to_owned())
                && !c.method_calls.contains(&"write".to_owned()),
            "cascade must not surface here: {c:?}"
        );
    }

    /// A URI that is only a bare filename still yields a stem, and an
    /// empty `show` list binds nothing.
    #[test]
    fn degenerate_uris_do_not_panic() {
        assert!(imports_of("import '';\n").modules.is_empty());
        assert!(imports_of("export 'other.dart';\n").symbols.is_empty());
    }
}
