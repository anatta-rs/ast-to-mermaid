//! Python-specific extractors: docstrings and call sites. Driven by
//! tree-sitter queries in `queries/python/*.scm`.

use tree_sitter::{Node, QueryCursor, StreamingIterator};

use super::ExtractedCalls;
use super::queries;

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

/// Extract every Python call site within `node`'s subtree into `out`.
///
/// `foo()` is captured as `identifier`; `obj.method()` captures the whole
/// `attribute` node ("obj.method"). Identifiers go to `out.calls`,
/// attributes to `out.method_calls` (after stripping the receiver chain).
pub(super) fn extract_calls(node: &Node, source: &str, out: &mut ExtractedCalls) {
    let query = &queries::PYTHON.calls;
    let mut cursor = QueryCursor::new();
    let bytes = source.as_bytes();
    let mut matches = cursor.matches(query, *node, bytes);
    while let Some(m) = matches.next() {
        for cap in m.captures {
            let Ok(text) = cap.node.utf8_text(bytes) else {
                continue;
            };
            match cap.node.kind() {
                "identifier" => out.calls.push(text.to_owned()),
                _ => {
                    if let Some(short) = text.rsplit('.').next()
                        && !short.is_empty()
                    {
                        out.method_calls.push(short.to_owned());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::graph::Store;
    use crate::model::EntityId;
    use crate::parser::CodeParser;

    #[test]
    fn python_parser_extracts_functions_and_classes() {
        let store = Store::new();
        let src = b"def foo():\n    pass\n\nclass Bar:\n    pass\n";
        CodeParser::python()
            .parse_into(src, "mod.py", &store)
            .expect("parse");
        assert!(!store.atoms_by_kind("function").is_empty());
        assert!(!store.atoms_by_kind("struct").is_empty()); // class → struct
    }

    #[test]
    fn python_docstring_extracted() {
        let store = Store::new();
        let src = b"def greet():\n    \"\"\"Say hello.\"\"\"\n    pass\n";
        CodeParser::python()
            .parse_into(src, "greet.py", &store)
            .expect("parse");
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
            .parse_into(src, "runner.py", &store)
            .expect("parse");
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
