//! Python-specific extractors: imports, docstrings, and call sites.
//! Driven by tree-sitter queries in `queries/python/*.scm`.

use std::collections::HashMap;
use tree_sitter::{Node, QueryCursor, StreamingIterator};

use super::ExtractedCalls;
use super::queries;
use crate::limits::max_ast_depth;
use crate::parser::Language;

// ── Import extraction ─────────────────────────────────────────────────────────

/// File-scope Python imports, split by how they rewrite call sites.
///
/// The cross-module resolver only links calls whose name is a qualified
/// path (`<module>::<symbol>`); bare names and `obj.method()` receivers it
/// leaves alone. So we rewrite Python call sites into that shape using the
/// file's imports — mirroring what the Rust extractor does with `use`.
///
/// We emit the qualifier as the module's **last dotted component** (e.g.
/// `lib.catalog` → `catalog`) so the resolver's existing module-name
/// fallback (`file_module_name == last qualifier segment`) matches without
/// any resolver-core change.
#[derive(Debug, Default, Clone)]
pub(super) struct PyImports {
    /// `from m import s` / `from m import s as a` → local name → `last::s`.
    /// Rewrites bare calls (`s()` → `last::s`).
    pub symbols: HashMap<String, String>,
    /// `import m` / `import m as x` / `from . import sub` → local alias →
    /// module last component. Rewrites `x.fn()` → `last::fn`.
    pub modules: HashMap<String, String>,
    /// Modules the file imports, as written: `lib.catalog`, `.mod`,
    /// `..pkg.mod`. A `from m import name` records both `m.name` (in case
    /// `name` is a submodule) and `m`; the resolver keeps whichever
    /// exists.
    pub targets: Vec<String>,
    /// Attribute types of the classes this file declares: class → attr →
    /// type, from `self.x = Cls(..)` and class-level `x: Cls`. Types
    /// `self.x.method()`.
    pub fields: HashMap<String, HashMap<String, String>>,
}

/// Walk every `import` / `from … import …` statement reachable from `root`
/// and build the [`PyImports`] rewrite maps.
pub(super) fn extract_imports(root: Node, source: &str) -> PyImports {
    let mut out = PyImports {
        fields: collect_class_fields(root, source),
        ..PyImports::default()
    };
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&queries::PYTHON.imports, root, source.as_bytes());
    while let Some(m) = matches.next() {
        for cap in m.captures {
            match cap.node.kind() {
                "import_statement" => handle_import(&cap.node, source, &mut out),
                "import_from_statement" => handle_from_import(&cap.node, source, &mut out),
                _ => {}
            }
        }
    }
    out
}

/// `import os` / `import a.b as x` / `import a, b`. Plain multi-segment
/// dotted imports (`import a.b.c` with no alias) are skipped — the bound
/// name is the head (`a`) but the module is the leaf, which we can't map
/// unambiguously; aliasing or `from` covers the common case.
fn handle_import(node: &Node, source: &str, out: &mut PyImports) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let dotted = match child.kind() {
            "aliased_import" => child.child_by_field_name("name"),
            "dotted_name" => Some(child),
            _ => None,
        };
        if let Some(d) = dotted {
            let full = dotted_segments(&d, source).join(".");
            if !full.is_empty() && !out.targets.contains(&full) {
                out.targets.push(full);
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "aliased_import" => {
                let alias = child
                    .child_by_field_name("alias")
                    .and_then(|n| node_text(&n, source));
                let module = child.child_by_field_name("name");
                if let (Some(alias), Some(module)) = (alias, module)
                    && let Some(last) = dotted_last(&module, source)
                {
                    out.modules.insert(alias.to_owned(), last.to_owned());
                }
            }
            "dotted_name" => {
                // Single-segment `import os` only — bind `os → os`.
                let segs = dotted_segments(&child, source);
                if segs.len() == 1 {
                    out.modules.insert(segs[0].to_owned(), segs[0].to_owned());
                }
            }
            _ => {}
        }
    }
}

/// `from m import a, b as c` and relative forms (`from . import sub`,
/// `from .mod import x`).
fn handle_from_import(node: &Node, source: &str, out: &mut PyImports) {
    let module_last = node
        .child_by_field_name("module_name")
        .and_then(|m| module_name_last(&m, source));
    // The module as written, dots included (`..pkg.mod`): the source of
    // the file-level `Imports` edge.
    let module_full: Option<String> = node
        .child_by_field_name("module_name")
        .and_then(|m| m.utf8_text(source.as_bytes()).ok())
        .map(|t| t.chars().filter(|c| !c.is_whitespace()).collect());
    if let Some(full) = &module_full {
        let mut cursor = node.walk();
        let mut seen_import_kw = false;
        for child in node.children(&mut cursor) {
            if child.kind() == "import" {
                seen_import_kw = true;
                continue;
            }
            let name = match child.kind() {
                "dotted_name" if seen_import_kw => dotted_last(&child, source),
                "aliased_import" if seen_import_kw => child
                    .child_by_field_name("name")
                    .and_then(|n| dotted_last(&n, source)),
                _ => None,
            };
            if let Some(name) = name {
                let sep = if full.ends_with('.') { "" } else { "." };
                let sub = format!("{full}{sep}{name}");
                if !out.targets.contains(&sub) {
                    out.targets.push(sub);
                }
            }
        }
        if !out.targets.contains(full) {
            out.targets.push(full.clone());
        }
    }

    let mut cursor = node.walk();
    // Imported names are the `name`-field children (dotted_name /
    // aliased_import) that follow the `import` keyword.
    let mut seen_import_kw = false;
    for child in node.children(&mut cursor) {
        if child.kind() == "import" {
            seen_import_kw = true;
            continue;
        }
        if !seen_import_kw {
            continue;
        }
        match child.kind() {
            "dotted_name" => {
                let Some(sym) = dotted_last(&child, source) else {
                    continue;
                };
                match &module_last {
                    Some(m) => {
                        out.symbols.insert(sym.to_owned(), format!("{m}::{sym}"));
                    }
                    // `from . import sub` — `sub` is itself a submodule.
                    None => {
                        out.modules.insert(sym.to_owned(), sym.to_owned());
                    }
                }
            }
            "aliased_import" => {
                let alias = child
                    .child_by_field_name("alias")
                    .and_then(|n| node_text(&n, source));
                let sym = child
                    .child_by_field_name("name")
                    .and_then(|n| dotted_last(&n, source));
                if let (Some(alias), Some(sym)) = (alias, sym) {
                    match &module_last {
                        Some(m) => {
                            out.symbols.insert(alias.to_owned(), format!("{m}::{sym}"));
                        }
                        None => {
                            out.modules.insert(alias.to_owned(), sym.to_owned());
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Last component of a `module_name` field, which is either a `dotted_name`
/// (`lib.catalog` → `catalog`) or a `relative_import` (`.mod` → `mod`,
/// bare `.` → `None`).
fn module_name_last<'a>(node: &Node, source: &'a str) -> Option<&'a str> {
    match node.kind() {
        "dotted_name" => dotted_last(node, source),
        "relative_import" => {
            // The optional `dotted_name` child carries the module path;
            // a bare `.` (just `import_prefix`) has none.
            let mut cursor = node.walk();
            node.children(&mut cursor)
                .find(|c| c.kind() == "dotted_name")
                .and_then(|d| dotted_last(&d, source))
        }
        _ => None,
    }
}

/// Last identifier segment of a `dotted_name` (`a.b.c` → `c`, `a` → `a`).
fn dotted_last<'a>(node: &Node, source: &'a str) -> Option<&'a str> {
    dotted_segments(node, source).pop()
}

/// All identifier segments of a `dotted_name`, in order.
fn dotted_segments<'a>(node: &Node, source: &'a str) -> Vec<&'a str> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .filter(|c| c.kind() == "identifier")
        .filter_map(|c| node_text(&c, source))
        .collect()
}

fn node_text<'a>(node: &Node, source: &'a str) -> Option<&'a str> {
    node.utf8_text(source.as_bytes()).ok()
}

// ── Doc comment extraction ────────────────────────────────────────────────────

pub(super) fn docstring(node: &Node, source: &str) -> String {
    // Look for the first expression_statement child whose expression is a string.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "block" {
            continue;
        }
        // Only check the first statement in the block.
        let first_stmt = child.children(&mut child.walk()).next();
        let Some(stmt) = first_stmt else { break };
        if stmt.kind() != "expression_statement" {
            break;
        }
        let mut expr_cursor = stmt.walk();
        for expr in stmt.children(&mut expr_cursor) {
            if expr.kind() == "string"
                && let Ok(text) = expr.utf8_text(source.as_bytes())
            {
                return text
                    .trim_matches(|c| c == '"' || c == '\'')
                    .trim()
                    .to_owned();
            }
        }
        break;
    }
    String::new()
}

// ── Call extraction ───────────────────────────────────────────────────────────

/// Extract every Python call site within `node`'s subtree into `out`,
/// rewriting names against the file's `imports` so the cross-module
/// resolver can link them.
///
/// - `foo()` (`identifier`): if `foo` is an imported symbol, push the
///   qualified `<module_last>::foo` to `out.calls`; otherwise push the bare
///   name (a same-file fn the intra-file linker will resolve).
/// - `mod.fn()` (`attribute` whose receiver root is an imported module
///   alias): push the qualified `<module_last>::fn` to `out.calls`.
/// - `obj.method()` (any other receiver — `self`, a local instance, a
///   call chain): the receiver type is unknown, so push the bare method
///   name to `out.method_calls` (powers intra-class `self.method()`
///   linking only; the cross-module resolver ignores it).
pub(super) fn extract_calls(
    node: &Node,
    source: &str,
    imports: &PyImports,
    self_type: Option<&str>,
    out: &mut ExtractedCalls,
) {
    let query = &queries::PYTHON.calls;
    let locals = local_types(*node, source, imports);
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
                    out.push_call(resolved, &cap.node, Language::Python);
                }
                "attribute" => {
                    let Some((root, method)) = attribute_root_method(&cap.node, source) else {
                        continue;
                    };
                    if let Some(owner) =
                        receiver_type(&cap.node, source, &locals, imports, self_type)
                    {
                        out.push_typed_call(
                            format!("{owner}::{method}"),
                            &cap.node,
                            Language::Python,
                        );
                        continue;
                    }
                    let Some(root) = root else {
                        // Receiver is not a name (a chained call, a
                        // subscript): the owner is unknowable, but the
                        // call happened. Intra-class linking only.
                        out.push_method_call(method.to_owned(), &cap.node, Language::Python);
                        continue;
                    };
                    if let Some(module_last) = imports.modules.get(root) {
                        // `mod.fn()` where `mod` is an imported module →
                        // resolver-eligible qualified call.
                        out.push_call(
                            format!("{module_last}::{method}"),
                            &cap.node,
                            Language::Python,
                        );
                    } else {
                        // Unknown receiver type — intra-class linking only.
                        out.push_method_call(method.to_owned(), &cap.node, Language::Python);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Class a callee `recv.method`'s direct receiver is an instance of:
/// `self` → the enclosing class; a name → its annotated / constructed
/// type; `self.x` → the attribute's type on the enclosing class.
fn receiver_type(
    callee: &Node,
    source: &str,
    locals: &HashMap<String, Option<String>>,
    imports: &PyImports,
    self_type: Option<&str>,
) -> Option<String> {
    let recv = callee.child_by_field_name("object")?;
    match recv.kind() {
        "identifier" => {
            let name = node_text(&recv, source)?;
            if name == "self" {
                return self_type.map(str::to_owned);
            }
            locals.get(name)?.clone()
        }
        "attribute" => {
            let base = recv.child_by_field_name("object")?;
            if node_text(&base, source)? != "self" {
                return None;
            }
            let attr = node_text(&recv.child_by_field_name("attribute")?, source)?;
            imports.fields.get(self_type?)?.get(attr).cloned()
        }
        _ => None,
    }
}

/// Name → class for every binding in `body` whose class is spelled out:
/// annotated parameters, `x: Cls`, `x = Cls(..)`. A name seen with two
/// classes maps to `None` (see the Rust twin, `rust::local_types`).
fn local_types(body: Node, source: &str, imports: &PyImports) -> HashMap<String, Option<String>> {
    let mut out: HashMap<String, Option<String>> = HashMap::new();
    let mut record = |name: &str, ty: Option<String>| {
        let Some(ty) = ty else { return };
        out.entry(name.to_owned())
            .and_modify(|e| {
                if e.as_deref() != Some(ty.as_str()) {
                    *e = None;
                }
            })
            .or_insert(Some(ty));
    };
    let mut stack = vec![body];
    let mut visited = 0usize;
    while let Some(n) = stack.pop() {
        visited += 1;
        if visited > 200_000 {
            break;
        }
        match n.kind() {
            "typed_parameter" => {
                let name = {
                    let mut c = n.walk();
                    n.named_children(&mut c)
                        .find(|x| x.kind() == "identifier")
                        .and_then(|x| node_text(&x, source))
                };
                if let (Some(name), Some(ty)) = (name, n.child_by_field_name("type")) {
                    record(name, annotation_class(&ty, source));
                }
            }
            "typed_default_parameter" => {
                let name = n
                    .child_by_field_name("name")
                    .and_then(|x| node_text(&x, source));
                if let (Some(name), Some(ty)) = (name, n.child_by_field_name("type")) {
                    record(name, annotation_class(&ty, source));
                }
            }
            "assignment" => {
                let left = n.child_by_field_name("left");
                if let Some(left) = left.filter(|l| l.kind() == "identifier")
                    && let Some(name) = node_text(&left, source)
                {
                    let ty = match n.child_by_field_name("type") {
                        Some(t) => annotation_class(&t, source),
                        None => n
                            .child_by_field_name("right")
                            .and_then(|r| constructed_class(&r, source, imports)),
                    };
                    record(name, ty);
                }
            }
            _ => {}
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            stack.push(child);
        }
    }
    out
}

/// Class named by an annotation, when it is a plain (possibly dotted)
/// capitalised name: `Cls`, `mod.Cls`, `"Cls"` is not followed.
fn annotation_class(ty: &Node, source: &str) -> Option<String> {
    let mut c = ty.walk();
    let inner = if ty.kind() == "type" {
        ty.named_children(&mut c).next()?
    } else {
        *ty
    };
    let name = match inner.kind() {
        "identifier" => node_text(&inner, source)?,
        "attribute" => node_text(&inner.child_by_field_name("attribute")?, source)?,
        _ => return None,
    };
    capitalised(name)
}

/// Class instantiated by `Cls(..)` / `mod.Cls(..)`.
fn constructed_class(value: &Node, source: &str, imports: &PyImports) -> Option<String> {
    if value.kind() != "call" {
        return None;
    }
    let func = value.child_by_field_name("function")?;
    let name = match func.kind() {
        "identifier" => {
            let n = node_text(&func, source)?;
            // `from m import Cls as Alias` → the class is `Cls`.
            imports
                .symbols
                .get(n)
                .and_then(|q| q.rsplit("::").next())
                .unwrap_or(n)
        }
        "attribute" => node_text(&func.child_by_field_name("attribute")?, source)?,
        _ => return None,
    };
    capitalised(name)
}

fn capitalised(name: &str) -> Option<String> {
    name.chars()
        .next()
        .is_some_and(char::is_uppercase)
        .then(|| name.to_owned())
}

/// Attribute types of every class in the file: `self.x = Cls(..)` in any
/// method, and class-level `x: Cls`.
fn collect_class_fields(root: Node, source: &str) -> HashMap<String, HashMap<String, String>> {
    let empty = PyImports::default();
    let mut out: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut stack = vec![root];
    let mut visited = 0usize;
    while let Some(n) = stack.pop() {
        visited += 1;
        if visited > 200_000 {
            break;
        }
        if n.kind() == "class_definition"
            && let Some(class) = n
                .child_by_field_name("name")
                .and_then(|x| node_text(&x, source))
            && let Some(body) = n.child_by_field_name("body")
        {
            let fields = out.entry(class.to_owned()).or_default();
            let mut inner = vec![body];
            while let Some(m) = inner.pop() {
                if m.kind() == "class_definition" {
                    continue;
                }
                if m.kind() == "assignment"
                    && let Some(left) = m.child_by_field_name("left")
                {
                    let attr = match left.kind() {
                        "attribute" => left
                            .child_by_field_name("object")
                            .and_then(|o| node_text(&o, source))
                            .filter(|o| *o == "self")
                            .and_then(|_| left.child_by_field_name("attribute"))
                            .and_then(|a| node_text(&a, source)),
                        // Class-level `x: Cls` — only meaningful annotated.
                        "identifier" if m.child_by_field_name("type").is_some() => {
                            node_text(&left, source)
                        }
                        _ => None,
                    };
                    let ty = match m.child_by_field_name("type") {
                        Some(t) => annotation_class(&t, source),
                        None => m
                            .child_by_field_name("right")
                            .and_then(|r| constructed_class(&r, source, &empty)),
                    };
                    if let (Some(attr), Some(ty)) = (attr, ty) {
                        fields.insert(attr.to_owned(), ty);
                    }
                }
                let mut c = m.walk();
                for child in m.children(&mut c) {
                    inner.push(child);
                }
            }
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            stack.push(child);
        }
    }
    out
}

/// Classes named in a function's parameter and return annotations, or in
/// a class' annotated attributes.
pub(super) fn type_refs(item: Node, source: &str) -> Vec<String> {
    let mut roots: Vec<Node> = Vec::new();
    match item.kind() {
        "function_definition" => {
            roots.extend(item.child_by_field_name("parameters"));
            roots.extend(item.child_by_field_name("return_type"));
        }
        "class_definition" => {
            if let Some(body) = item.child_by_field_name("body") {
                let mut c = body.walk();
                for stmt in body.named_children(&mut c) {
                    let mut c2 = stmt.walk();
                    roots.extend(
                        stmt.named_children(&mut c2)
                            .filter(|a| a.kind() == "assignment")
                            .filter_map(|a| a.child_by_field_name("type")),
                    );
                }
            }
        }
        _ => {}
    }
    let mut out: Vec<String> = Vec::new();
    let mut stack: Vec<(Node, bool)> = roots.into_iter().map(|r| (r, r.kind() == "type")).collect();
    let mut visited = 0usize;
    while let Some((n, in_type)) = stack.pop() {
        visited += 1;
        if visited > 10_000 {
            break;
        }
        let in_type = in_type || n.kind() == "type";
        if in_type
            && n.kind() == "identifier"
            && let Some(name) = node_text(&n, source).and_then(capitalised)
            && !out.contains(&name)
        {
            out.push(name);
        }
        // Default values are expressions, not annotations.
        if n.kind() == "default_parameter" {
            continue;
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            stack.push((child, in_type));
        }
    }
    out
}

/// From an `attribute` callee (`obj.method`, `a.b.method`,
/// `pkg.mod.fn`), return `(receiver_root_ident, method_name)`.
///
/// The method is the `attribute` field; the receiver root is the leftmost
/// identifier reached by descending the `object` chain. Returns `None` when
/// the receiver root isn't a plain identifier (e.g. `f().method`,
/// `d[k].method`) — those can't be mapped to an imported module.
fn attribute_root_method<'a>(node: &Node, source: &'a str) -> Option<(Option<&'a str>, &'a str)> {
    let method = node
        .child_by_field_name("attribute")
        .and_then(|n| node_text(&n, source))?;
    let Some(mut current) = node.child_by_field_name("object") else {
        return Some((None, method));
    };
    for _ in 0..max_ast_depth() {
        match current.kind() {
            "identifier" => return Some((node_text(&current, source), method)),
            "attribute" => {
                let Some(next) = current.child_by_field_name("object") else {
                    return Some((None, method));
                };
                current = next;
            }
            // Non-identifier receiver root (call / subscript / paren), as
            // in `build().render()`. Nothing can be said about what owns
            // the method — but the call is real, and returning `None`
            // dropped it from the graph entirely. Report the method with
            // no root so it lands in `method_calls`, which is exactly the
            // "receiver type unknown" bucket.
            _ => return Some((None, method)),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{PyImports, extract_calls, extract_imports};
    use crate::graph::Store;
    use crate::model::EntityId;
    use crate::parser::{CodeParser, ExtractedCalls};
    use tree_sitter::Parser as TsParser;

    fn parse(src: &str) -> tree_sitter::Tree {
        let mut p = TsParser::new();
        p.set_language(&tree_sitter_python::LANGUAGE.into())
            .expect("python grammar");
        p.parse(src, None).expect("parse")
    }

    fn imports_of(src: &str) -> PyImports {
        let tree = parse(src);
        extract_imports(tree.root_node(), src)
    }

    fn calls_of(src: &str, imports: &PyImports) -> ExtractedCalls {
        let tree = parse(src);
        let mut out = ExtractedCalls::default();
        extract_calls(&tree.root_node(), src, imports, None, &mut out);
        out
    }

    #[test]
    fn from_import_maps_symbol_to_module_last() {
        let imp = imports_of("from lib.catalog import load_catalog\n");
        assert_eq!(
            imp.symbols.get("load_catalog").map(String::as_str),
            Some("catalog::load_catalog")
        );
    }

    #[test]
    fn from_import_alias_keeps_alias_key() {
        let imp = imports_of("from lib.catalog import load_catalog as lc\n");
        assert_eq!(
            imp.symbols.get("lc").map(String::as_str),
            Some("catalog::load_catalog")
        );
        assert!(!imp.symbols.contains_key("load_catalog"));
    }

    #[test]
    fn import_aliased_module_maps_to_last_component() {
        let imp = imports_of("import lib.config as config\n");
        assert_eq!(
            imp.modules.get("config").map(String::as_str),
            Some("config")
        );
    }

    #[test]
    fn import_single_module_maps_to_itself() {
        let imp = imports_of("import os\n");
        assert_eq!(imp.modules.get("os").map(String::as_str), Some("os"));
    }

    #[test]
    fn relative_from_import_submodule_is_module() {
        // `from . import sib` — sib is itself a submodule alias.
        let imp = imports_of("from . import sib\n");
        assert_eq!(imp.modules.get("sib").map(String::as_str), Some("sib"));
        // `from .mod import rel` — rel is a symbol of module `mod`.
        let imp2 = imports_of("from .mod import rel\n");
        assert_eq!(
            imp2.symbols.get("rel").map(String::as_str),
            Some("mod::rel")
        );
    }

    #[test]
    fn bare_call_rewritten_via_symbol_import() {
        let imp = imports_of("from lib.catalog import load_catalog\n");
        let calls = calls_of("load_catalog()\n", &imp);
        assert!(
            calls
                .calls
                .iter()
                .any(|c| c.name == "catalog::load_catalog"),
            "calls={:?}",
            calls.calls
        );
    }

    #[test]
    fn module_attribute_call_rewritten_to_qualified() {
        let imp = imports_of("import lib.config as config\n");
        let calls = calls_of("config.current()\n", &imp);
        assert!(
            calls.calls.iter().any(|c| c.name == "config::current"),
            "calls={:?}",
            calls.calls
        );
    }

    #[test]
    fn unimported_bare_call_stays_bare() {
        let calls = calls_of("helper()\n", &PyImports::default());
        assert!(
            calls.calls.iter().any(|c| c.name == "helper"),
            "{:?}",
            calls.calls
        );
    }

    #[test]
    fn instance_method_call_stays_method_call() {
        // `self.method()` / `obj.method()` — receiver type unknown, must not
        // become a resolver-eligible qualified call.
        let calls = calls_of("self.gettext(k)\nobj.run()\n", &PyImports::default());
        assert!(calls.calls.is_empty(), "calls leaked: {:?}", calls.calls);
        assert!(calls.method_calls.iter().any(|m| m.name == "gettext"));
        assert!(calls.method_calls.iter().any(|m| m.name == "run"));
    }

    #[test]
    fn cross_module_python_resolves_end_to_end() {
        // Integration: importer module calls an imported symbol; after parse
        // + resolve, a Calls edge must exist.
        let store = Store::new();
        CodeParser::python()
            .parse(
                b"from lib.catalog import load_catalog\n\ndef use_it():\n    return load_catalog('en')\n",
                "lib/translator.py",
            )
            .expect("parse")
            .apply_to(&store);
        CodeParser::python()
            .parse(
                b"def load_catalog(lang):\n    return {}\n",
                "lib/catalog.py",
            )
            .expect("parse")
            .apply_to(&store);
        let added = crate::resolve::resolve_cross_module_calls(&store);
        assert_eq!(added, 1, "expected one cross-module edge");
        let from = EntityId::new("code:lib/translator.py::function::use_it");
        let to = EntityId::new("code:lib/catalog.py::function::load_catalog");
        assert!(store.has_call_edge(&from, &to));
    }

    #[test]
    fn python_parser_extracts_functions_and_classes() {
        let store = Store::new();
        let src = b"def foo():\n    pass\n\nclass Bar:\n    pass\n";
        CodeParser::python()
            .parse(src, "mod.py")
            .expect("parse")
            .apply_to(&store);
        assert!(!store.atoms_by_kind("function").is_empty());
        assert!(!store.atoms_by_kind("struct").is_empty()); // class → struct
    }

    #[test]
    fn python_docstring_extracted() {
        let store = Store::new();
        let src = b"def greet():\n    \"\"\"Say hello.\"\"\"\n    pass\n";
        CodeParser::python()
            .parse(src, "greet.py")
            .expect("parse")
            .apply_to(&store);
        let id = EntityId::new("code:greet.py::function::greet");
        let atom = store.get_atom(&id).expect("atom");
        assert!(atom.doc.contains("Say hello"), "doc={:?}", atom.doc);
    }

    #[test]
    fn python_method_calls_extracted() {
        let store = Store::new();
        // obj.method() — triggers method_call_expression in Python grammar
        let src = b"def runner():\n    obj.run()\n    helper()\n";
        CodeParser::python()
            .parse(src, "runner.py")
            .expect("parse")
            .apply_to(&store);
        let id = EntityId::new("code:runner.py::function::runner");
        let atom = store.get_atom(&id).expect("atom");
        // calls list should contain extracted call names
        assert!(
            !atom.calls.is_empty(),
            "expected some calls, got {:?}",
            atom.calls
        );
    }
}
