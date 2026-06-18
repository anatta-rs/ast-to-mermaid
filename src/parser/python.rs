//! Python-specific extractors: imports, docstrings, and call sites.
//! Driven by tree-sitter queries in `queries/python/*.scm`.

use std::collections::HashMap;
use tree_sitter::{Node, QueryCursor, StreamingIterator};

use super::ExtractedCalls;
use super::queries;
use crate::limits::max_ast_depth;

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
}

/// Walk every `import` / `from … import …` statement reachable from `root`
/// and build the [`PyImports`] rewrite maps.
pub(super) fn extract_imports(root: Node, source: &str) -> PyImports {
    let mut out = PyImports::default();
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
    out: &mut ExtractedCalls,
) {
    let query = &queries::PYTHON.calls;
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
                "attribute" => {
                    let Some((root, method)) = attribute_root_method(&cap.node, source) else {
                        continue;
                    };
                    if let Some(module_last) = imports.modules.get(root) {
                        // `mod.fn()` where `mod` is an imported module →
                        // resolver-eligible qualified call.
                        out.calls.push(format!("{module_last}::{method}"));
                    } else {
                        // Unknown receiver type — intra-class linking only.
                        out.method_calls.push(method.to_owned());
                    }
                }
                _ => {}
            }
        }
    }
}

/// From an `attribute` callee (`obj.method`, `a.b.method`,
/// `pkg.mod.fn`), return `(receiver_root_ident, method_name)`.
///
/// The method is the `attribute` field; the receiver root is the leftmost
/// identifier reached by descending the `object` chain. Returns `None` when
/// the receiver root isn't a plain identifier (e.g. `f().method`,
/// `d[k].method`) — those can't be mapped to an imported module.
fn attribute_root_method<'a>(node: &Node, source: &'a str) -> Option<(&'a str, &'a str)> {
    let method = node
        .child_by_field_name("attribute")
        .and_then(|n| node_text(&n, source))?;
    let mut current = node.child_by_field_name("object")?;
    for _ in 0..max_ast_depth() {
        match current.kind() {
            "identifier" => return Some((node_text(&current, source)?, method)),
            "attribute" => current = current.child_by_field_name("object")?,
            // Non-identifier receiver root (call/subscript/paren) — give up.
            _ => return None,
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
        extract_calls(&tree.root_node(), src, imports, &mut out);
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
            calls.calls.iter().any(|c| c == "catalog::load_catalog"),
            "calls={:?}",
            calls.calls
        );
    }

    #[test]
    fn module_attribute_call_rewritten_to_qualified() {
        let imp = imports_of("import lib.config as config\n");
        let calls = calls_of("config.current()\n", &imp);
        assert!(
            calls.calls.iter().any(|c| c == "config::current"),
            "calls={:?}",
            calls.calls
        );
    }

    #[test]
    fn unimported_bare_call_stays_bare() {
        let calls = calls_of("helper()\n", &PyImports::default());
        assert!(
            calls.calls.iter().any(|c| c == "helper"),
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
        assert!(calls.method_calls.iter().any(|m| m == "gettext"));
        assert!(calls.method_calls.iter().any(|m| m == "run"));
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
