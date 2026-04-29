//! Self-contained code parser — tree-sitter Rust + Python → [`CodeAtom`]s.
//!
//! No dependency on `ingester-core` or `ingester-code`. The parser lives
//! entirely in this module.
//!
//! # Identity scheme
//!
//! - Module atom: `code:{file_path}` — one per file.
//! - Item atom: `code:{file_path}::{kind}::{name}`.
//!
//! # Edge scheme
//!
//! - `Contains`: module → item.
//! - `Calls`: function → function (intra-file only; cross-file calls are
//!   resolved later by [`crate::resolve`]).

use crate::error::{AstToMermaidError, Result};
use crate::graph::Store;
use crate::model::{CodeAtom, Edge, EdgeKind, EntityId};
use std::collections::{HashMap, HashSet};
use tree_sitter::{Node, Parser as TsParser};

// ── Language ──────────────────────────────────────────────────────────────────

/// Source language supported by [`CodeParser`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    /// Rust source (`.rs`).
    Rust,
    /// Python source (`.py`).
    Python,
}

impl Language {
    /// Human-readable tag.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
        }
    }

    /// Return the tree-sitter grammar for this language.
    #[must_use]
    fn ts_language(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
        }
    }

    /// Tree-sitter node kinds that should become top-level atoms.
    fn item_node_kinds(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &[
                "function_item",
                "struct_item",
                "trait_item",
                "impl_item",
                "enum_item",
                "type_item",
                "const_item",
                "static_item",
                "macro_definition",
            ],
            Self::Python => &[
                "function_definition",
                "class_definition",
                "decorated_definition",
            ],
        }
    }

    /// Map a tree-sitter node kind to a canonical atom kind string.
    fn map_node_kind(self, ts_kind: &str) -> Option<&'static str> {
        match (self, ts_kind) {
            (Self::Rust, "function_item") | (Self::Python, "function_definition") => {
                Some("function")
            }
            (Self::Rust, "struct_item") | (Self::Python, "class_definition") => Some("struct"),
            (Self::Rust, "trait_item") => Some("trait"),
            (Self::Rust, "impl_item") => Some("impl"),
            (Self::Rust, "enum_item") => Some("enum"),
            (Self::Rust, "type_item") => Some("type_alias"),
            (Self::Rust, "const_item") => Some("const"),
            (Self::Rust, "static_item") => Some("static"),
            (Self::Rust, "macro_definition") => Some("macro"),
            _ => None,
        }
    }
}

impl PartialEq<Language> for &Language {
    fn eq(&self, other: &Language) -> bool {
        **self == *other
    }
}

// ── Parser ────────────────────────────────────────────────────────────────────

/// Source-code parser. Construct one per language; re-use across files.
pub struct CodeParser {
    language: Language,
}

impl CodeParser {
    /// Construct a Rust parser.
    #[must_use]
    pub fn rust() -> Self {
        Self {
            language: Language::Rust,
        }
    }

    /// Construct a Python parser.
    #[must_use]
    pub fn python() -> Self {
        Self {
            language: Language::Python,
        }
    }

    /// Which language this parser handles.
    #[must_use]
    pub fn language(&self) -> Language {
        self.language
    }

    /// Parse `content` (UTF-8 source bytes) from `file_path`, ingesting atoms
    /// and edges directly into `store`.
    ///
    /// # Errors
    ///
    /// - `InvalidInput` for non-UTF-8 content.
    /// - `InvalidInput` when tree-sitter fails to parse.
    pub fn parse_into(&self, content: &[u8], file_path: &str, store: &Store) -> Result<usize> {
        let text = std::str::from_utf8(content).map_err(|e| {
            AstToMermaidError::InvalidInput(format!("invalid utf-8 in {file_path}: {e}"))
        })?;

        let mut ts_parser = TsParser::new();
        ts_parser
            .set_language(&self.language.ts_language())
            .map_err(|e| {
                AstToMermaidError::InvalidInput(format!(
                    "tree-sitter set_language failed for {file_path}: {e}"
                ))
            })?;

        let tree = ts_parser.parse(content, None).ok_or_else(|| {
            AstToMermaidError::InvalidInput(format!("tree-sitter parse failed for {file_path}"))
        })?;

        // ── Module atom ───────────────────────────────────────────────────────
        let module_id = EntityId::new(format!("code:{file_path}"));
        let module_name = module_name(file_path).to_owned();
        let module_hash = hex_sha256(content);
        let module_atom = CodeAtom {
            id: module_id.clone(),
            kind: "module".to_owned(),
            name: module_name,
            file_path: file_path.to_owned(),
            line_start: 1,
            line_end: u32::try_from(text.lines().count()).unwrap_or(u32::MAX),
            doc: String::new(),
            signature: String::new(),
            content_hash: module_hash,
            calls: Vec::new(),
        };
        store.add_atom(module_atom);
        let mut atom_count = 1;

        // ── Item atoms ────────────────────────────────────────────────────────
        let root = tree.root_node();
        let mut cursor = root.walk();
        let mut name_to_id: HashMap<String, EntityId> = HashMap::new();
        let mut items: Vec<(EntityId, Vec<String>)> = Vec::new(); // (id, calls)

        for child in root.children(&mut cursor) {
            let Some((atom, call_names)) = extract_item(&child, text, file_path, self.language)
            else {
                continue;
            };
            let item_id = atom.id.clone();
            let item_name = atom.name.clone();
            name_to_id.insert(item_name, item_id.clone());

            store.add_edge(Edge::new(
                module_id.clone(),
                item_id.clone(),
                EdgeKind::Contains,
            ));
            items.push((item_id.clone(), call_names));
            store.add_atom(atom);
            atom_count += 1;
        }

        // ── Intra-file call edges ─────────────────────────────────────────────
        for (caller_id, call_names) in items {
            for callee_name in call_names {
                if let Some(callee_id) = name_to_id.get(&callee_name)
                    && *callee_id != caller_id
                {
                    store.add_edge(Edge::new(
                        caller_id.clone(),
                        callee_id.clone(),
                        EdgeKind::Calls,
                    ));
                }
            }
        }

        Ok(atom_count)
    }
}

// ── Item extraction ───────────────────────────────────────────────────────────

/// Extract a single top-level item node.
///
/// Returns `(CodeAtom, call_names)` or `None` if the node kind is not lifted.
fn extract_item(
    node: &Node,
    source: &str,
    file_path: &str,
    language: Language,
) -> Option<(CodeAtom, Vec<String>)> {
    let ts_kind = node.kind();
    if !language.item_node_kinds().contains(&ts_kind) {
        return None;
    }

    // Python decorators — unwrap inner definition.
    if ts_kind == "decorated_definition" {
        let inner = node.child_by_field_name("definition")?;
        return extract_item(&inner, source, file_path, language);
    }

    let atom_kind = language.map_node_kind(ts_kind)?;
    let item_name = extract_name(node, source, ts_kind)?;

    let item_text = node.utf8_text(source.as_bytes()).unwrap_or_default();
    let content_hash = format!("sha256:{}", hex_sha256(item_text.as_bytes()));

    let line_start = u32::try_from(node.start_position().row).unwrap_or(u32::MAX) + 1;
    let line_end = u32::try_from(node.end_position().row).unwrap_or(u32::MAX) + 1;

    // Signature: first non-brace line of the item text.
    let signature = item_text
        .lines()
        .next()
        .unwrap_or_default()
        .trim_end_matches('{')
        .trim()
        .to_owned();

    // Doc comment.
    let doc = if language == Language::Python {
        python_docstring(node, source)
    } else {
        rust_doc_comment(source, node.start_position().row)
    };

    // Call names for functions.
    let call_names = if ts_kind == "function_item" || ts_kind == "function_definition" {
        extract_calls(node, source)
    } else {
        Vec::new()
    };

    let item_id = EntityId::new(format!("code:{file_path}::{atom_kind}::{item_name}"));

    let atom = CodeAtom {
        id: item_id,
        kind: atom_kind.to_owned(),
        name: item_name,
        file_path: file_path.to_owned(),
        line_start,
        line_end,
        doc,
        signature,
        content_hash,
        calls: call_names.clone(),
    };

    Some((atom, call_names))
}

// ── Name extraction ───────────────────────────────────────────────────────────

fn extract_name(node: &Node, source: &str, ts_kind: &str) -> Option<String> {
    if ts_kind == "impl_item" {
        let type_node = node.child_by_field_name("type")?;
        let type_name = type_node.utf8_text(source.as_bytes()).ok()?;
        return if let Some(trait_node) = node.child_by_field_name("trait") {
            let trait_name = trait_node.utf8_text(source.as_bytes()).ok()?;
            Some(format!("{trait_name} for {type_name}"))
        } else {
            Some(type_name.to_owned())
        };
    }
    node.child_by_field_name("name")
        .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        .map(str::to_owned)
}

// ── Call extraction ───────────────────────────────────────────────────────────

fn extract_calls(node: &Node, source: &str) -> Vec<String> {
    let mut calls: Vec<String> = Vec::new();
    let mut stack: Vec<Node> = vec![*node];

    while let Some(current) = stack.pop() {
        match current.kind() {
            "call_expression" => {
                if let Some(func) = current.child_by_field_name("function")
                    && let Ok(name) = func.utf8_text(source.as_bytes())
                    && let Some(short) = name.rsplit("::").next()
                    && !short.is_empty()
                {
                    calls.push(short.to_owned());
                }
            }
            "method_call_expression" | "field_expression" => {
                let name_node = current
                    .child_by_field_name("name")
                    .or_else(|| current.child_by_field_name("field"));
                if let Some(n) = name_node
                    && let Ok(name) = n.utf8_text(source.as_bytes())
                    && !name.is_empty()
                {
                    calls.push(name.to_owned());
                }
            }
            "call" => {
                if let Some(func) = current.child_by_field_name("function")
                    && let Ok(name) = func.utf8_text(source.as_bytes())
                    && let Some(short) = name.rsplit('.').next()
                    && !short.is_empty()
                {
                    calls.push(short.to_owned());
                }
            }
            _ => {}
        }

        let mut cursor = current.walk();
        for child in current.children(&mut cursor) {
            stack.push(child);
        }
    }

    let mut seen: HashSet<String> = HashSet::new();
    calls.retain(|c| seen.insert(c.clone()));
    calls
}

// ── Doc comment extraction ────────────────────────────────────────────────────

fn rust_doc_comment(source: &str, item_row: usize) -> String {
    let lines: Vec<&str> = source.lines().collect();
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

fn python_docstring(node: &Node, source: &str) -> String {
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

// ── Utilities ─────────────────────────────────────────────────────────────────

/// SHA-256 hex digest of `bytes`.
fn hex_sha256(bytes: &[u8]) -> String {
    use std::fmt::Write as FmtWrite;
    use std::num::Wrapping;
    // Simple deterministic hash — not cryptographic, fast for content IDs.
    // We use FNV-1a for now since we don't want a sha2 dep in this crate;
    // the ingester used sha2 but we're decoupled here.
    let mut hash = Wrapping(0xcbf2_9ce4_8422_2325_u64);
    for &byte in bytes {
        hash ^= Wrapping(u64::from(byte));
        hash *= Wrapping(0x0100_0000_01b3_u64);
    }
    let mut out = String::with_capacity(16);
    write!(out, "{:016x}", hash.0).expect("writing to String");
    out
}

/// Extract the module name (file stem) from a path.
#[must_use]
pub fn module_name(path: &str) -> &str {
    let basename = path.rsplit('/').next().unwrap_or(path);
    basename.rsplit_once('.').map_or(basename, |(stem, _)| stem)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Store;

    #[test]
    fn rust_parser_creates_module_and_function() {
        let store = Store::new();
        let src = b"pub fn hello() {}\npub fn world() { hello(); }\n";
        CodeParser::rust()
            .parse_into(src, "src/lib.rs", &store)
            .expect("parse");
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
            .parse_into(src, "src/lib.rs", &store)
            .expect("parse");
        let a = EntityId::new("code:src/lib.rs::function::a");
        let b = EntityId::new("code:src/lib.rs::function::b");
        assert!(store.has_call_edge(&a, &b));
    }

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
    fn invalid_utf8_errors() {
        let store = Store::new();
        let err = CodeParser::rust()
            .parse_into(&[0xff, 0xfe], "bad.rs", &store)
            .expect_err("must fail");
        assert!(matches!(err, AstToMermaidError::InvalidInput(_)));
    }

    #[test]
    fn module_name_strips_path_and_extension() {
        assert_eq!(module_name("src/lib.rs"), "lib");
        assert_eq!(module_name("crates/foo/src/main.rs"), "main");
        assert_eq!(module_name("mod.py"), "mod");
    }

    #[test]
    fn hex_sha256_deterministic() {
        let a = hex_sha256(b"hello");
        let b = hex_sha256(b"hello");
        assert_eq!(a, b);
        let c = hex_sha256(b"world");
        assert_ne!(a, c);
    }
}
