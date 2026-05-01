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
use serde::{Deserialize, Serialize};

/// Output of parsing one file: the atoms (module + items) and the
/// intra-file edges (Contains module→item, intra-file Calls). Cross-module
/// edges are not in here — they're produced later by the resolver.
///
/// This is the unit the content-addressed cache stores keyed by git blob
/// SHA. Two files with identical content produce identical `ParseUnit`s.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ParseUnit {
    /// Module atom + per-item atoms.
    pub atoms: Vec<CodeAtom>,
    /// Contains edges (module → item) and intra-file Calls edges.
    pub edges: Vec<Edge>,
}

impl ParseUnit {
    /// Apply this unit's atoms and edges to `store`, in order.
    pub fn apply_to(&self, store: &Store) {
        for a in &self.atoms {
            store.add_atom(a.clone());
        }
        for e in &self.edges {
            store.add_edge(e.clone());
        }
    }

    /// How many atoms this unit contributed (cache-hit replay metric).
    #[must_use]
    pub fn atom_count(&self) -> usize {
        self.atoms.len()
    }
}
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

    /// Parse `content` (UTF-8 source bytes) from `file_path` into a
    /// [`ParseUnit`] (atoms + intra-file edges). The output is fully
    /// determined by `(content, file_path, language)` — same inputs always
    /// produce byte-identical outputs, which is what makes content-addressed
    /// caching by git blob SHA correct.
    ///
    /// # Errors
    ///
    /// - `InvalidInput` for non-UTF-8 content.
    /// - `InvalidInput` when tree-sitter fails to parse.
    pub fn parse(&self, content: &[u8], file_path: &str) -> Result<ParseUnit> {
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

        let mut atoms: Vec<CodeAtom> = Vec::new();
        let mut edges: Vec<Edge> = Vec::new();

        // ── Module atom ───────────────────────────────────────────────────────
        let module_id = EntityId::new(format!("code:{file_path}"));
        let module_name = module_name(file_path).to_owned();
        let module_hash = git_blob_sha1(content);
        atoms.push(CodeAtom {
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
        });

        // ── File-scope use imports (Rust only) ────────────────────────────────
        let root = tree.root_node();
        let imports: HashMap<String, String> = if self.language == Language::Rust {
            extract_use_imports(root, text)
        } else {
            HashMap::new()
        };

        // ── Item atoms ────────────────────────────────────────────────────────
        let mut cursor = root.walk();
        let mut name_to_id: HashMap<String, EntityId> = HashMap::new();
        let mut items: Vec<(EntityId, Vec<String>)> = Vec::new();

        for child in root.children(&mut cursor) {
            let Some((atom, call_names)) =
                extract_item(&child, text, file_path, self.language, &imports)
            else {
                continue;
            };
            let item_id = atom.id.clone();
            let item_name = atom.name.clone();
            name_to_id.insert(item_name, item_id.clone());

            edges.push(Edge::new(
                module_id.clone(),
                item_id.clone(),
                EdgeKind::Contains,
            ));
            items.push((item_id.clone(), call_names));
            atoms.push(atom);
        }

        // ── Intra-file call edges ─────────────────────────────────────────────
        // Only resolve bare-name calls intra-file; qualified calls are
        // intentionally cross-module and handled by [`crate::resolve`].
        for (caller_id, call_names) in items {
            for callee_name in call_names {
                if callee_name.contains("::") {
                    continue;
                }
                if let Some(callee_id) = name_to_id.get(&callee_name)
                    && *callee_id != caller_id
                {
                    edges.push(Edge::new(
                        caller_id.clone(),
                        callee_id.clone(),
                        EdgeKind::Calls,
                    ));
                }
            }
        }

        Ok(ParseUnit { atoms, edges })
    }

    /// Backward-compat wrapper: parse and write the result into `store`.
    /// Equivalent to `self.parse(...)?.apply_to(store)` then returning the
    /// atom count. Existing callers don't see a behaviour change.
    ///
    /// # Errors
    /// Forwards errors from [`Self::parse`].
    pub fn parse_into(&self, content: &[u8], file_path: &str, store: &Store) -> Result<usize> {
        let unit = self.parse(content, file_path)?;
        let count = unit.atom_count();
        unit.apply_to(store);
        Ok(count)
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
    imports: &HashMap<String, String>,
) -> Option<(CodeAtom, Vec<String>)> {
    let ts_kind = node.kind();
    if !language.item_node_kinds().contains(&ts_kind) {
        return None;
    }

    // Python decorators — unwrap inner definition.
    if ts_kind == "decorated_definition" {
        let inner = node.child_by_field_name("definition")?;
        return extract_item(&inner, source, file_path, language, imports);
    }

    let atom_kind = language.map_node_kind(ts_kind)?;
    let item_name = extract_name(node, source, ts_kind)?;

    let item_text = node.utf8_text(source.as_bytes()).unwrap_or_default();
    let content_hash = git_blob_sha1(item_text.as_bytes());

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
        extract_calls(node, source, imports)
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

fn extract_calls(node: &Node, source: &str, imports: &HashMap<String, String>) -> Vec<String> {
    let mut calls: Vec<String> = Vec::new();
    let mut stack: Vec<Node> = vec![*node];

    while let Some(current) = stack.pop() {
        match current.kind() {
            "call_expression" => {
                if let Some(func) = current.child_by_field_name("function") {
                    match func.kind() {
                        "identifier" => {
                            if let Ok(name) = func.utf8_text(source.as_bytes())
                                && !name.is_empty()
                            {
                                // Bare call: expand via use imports if available.
                                let normalized = imports
                                    .get(name)
                                    .cloned()
                                    .unwrap_or_else(|| name.to_owned());
                                calls.push(normalized);
                            }
                        }
                        "scoped_identifier" => {
                            // Already qualified — keep the full path so the
                            // resolver can disambiguate by module.
                            if let Ok(name) = func.utf8_text(source.as_bytes())
                                && !name.is_empty()
                            {
                                calls.push(name.to_owned());
                            }
                        }
                        // Other shapes (field_expression for chained calls,
                        // generic_function, etc.) are handled by the inner
                        // call/method-call branches as we recurse.
                        _ => {}
                    }
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

// ── Use-import extraction (Rust) ──────────────────────────────────────────────

/// Build a file-scope map of `bare_name → fully_qualified_path` from every
/// `use_declaration` at the file root. The qualified path mirrors the source
/// (e.g. `use crate::render::render;` ⇒ `render → crate::render::render`).
///
/// Group forms (`use crate::foo::{a, b}`), aliases (`use foo as bar`), and
/// nested paths are all unfolded. Wildcards (`use foo::*`) are ignored.
fn extract_use_imports(root: Node, source: &str) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() != "use_declaration" {
            continue;
        }
        let mut sub = child.walk();
        for inner in child.children(&mut sub) {
            // Skip the `use` keyword and trailing `;`.
            if matches!(inner.kind(), "use" | ";") {
                continue;
            }
            collect_use_paths(inner, source, "", &mut out);
        }
    }
    out
}

fn collect_use_paths(node: Node, source: &str, prefix: &str, out: &mut HashMap<String, String>) {
    match node.kind() {
        "scoped_identifier" => {
            let text = node.utf8_text(source.as_bytes()).unwrap_or_default();
            let Some(name) = text.rsplit("::").next().filter(|s| !s.is_empty()) else {
                return;
            };
            let full = if prefix.is_empty() {
                text.to_owned()
            } else {
                format!("{prefix}::{text}")
            };
            out.insert(name.to_owned(), full);
        }
        "identifier" => {
            let name = node.utf8_text(source.as_bytes()).unwrap_or_default();
            if name.is_empty() {
                return;
            }
            let full = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}::{name}")
            };
            out.insert(name.to_owned(), full);
        }
        "use_as_clause" => {
            // children: <path> "as" <alias>
            let mut cursor = node.walk();
            let mut path_text: Option<String> = None;
            let mut alias_name: Option<String> = None;
            for ch in node.children(&mut cursor) {
                match ch.kind() {
                    "scoped_identifier" | "identifier" => {
                        let text = ch
                            .utf8_text(source.as_bytes())
                            .unwrap_or_default()
                            .to_owned();
                        if path_text.is_none() {
                            path_text = Some(text);
                        } else if alias_name.is_none() {
                            alias_name = Some(text);
                        }
                    }
                    _ => {}
                }
            }
            if let (Some(path), Some(alias)) = (path_text, alias_name) {
                let full = if prefix.is_empty() {
                    path
                } else {
                    format!("{prefix}::{path}")
                };
                out.insert(alias, full);
            }
        }
        "scoped_use_list" => {
            // children: <path: scoped_identifier|identifier> "::" <use_list>
            let mut cursor = node.walk();
            let mut path_text = String::new();
            let mut list_node: Option<Node> = None;
            for ch in node.children(&mut cursor) {
                match ch.kind() {
                    "scoped_identifier" | "identifier" | "self" | "crate" | "super"
                        if list_node.is_none() && path_text.is_empty() =>
                    {
                        ch.utf8_text(source.as_bytes())
                            .unwrap_or_default()
                            .clone_into(&mut path_text);
                    }
                    "use_list" => list_node = Some(ch),
                    _ => {}
                }
            }
            let new_prefix = if prefix.is_empty() {
                path_text
            } else {
                format!("{prefix}::{path_text}")
            };
            if let Some(list) = list_node {
                let mut c = list.walk();
                for ch in list.children(&mut c) {
                    collect_use_paths(ch, source, &new_prefix, out);
                }
            }
        }
        "use_list" => {
            let mut c = node.walk();
            for ch in node.children(&mut c) {
                collect_use_paths(ch, source, prefix, out);
            }
        }
        // `use_wildcard`, punctuation, etc. — nothing to record.
        _ => {}
    }
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

/// Git blob SHA-1 hex digest: `SHA1("blob N\0" + bytes)`.
///
/// Matches the identity git itself assigns to file blobs — the same value
/// you'd get from `git hash-object <path>`. Cache keys produced here align
/// with `git ls-tree` output.
#[must_use]
pub fn git_blob_sha1(bytes: &[u8]) -> String {
    let mut hasher = sha1_smol::Sha1::new();
    hasher.update(format!("blob {}\0", bytes.len()).as_bytes());
    hasher.update(bytes);
    hasher.digest().to_string()
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
    fn git_blob_sha1_deterministic() {
        let a = git_blob_sha1(b"hello");
        let b = git_blob_sha1(b"hello");
        assert_eq!(a, b);
        let c = git_blob_sha1(b"world");
        assert_ne!(a, c);
    }

    #[test]
    fn git_blob_sha1_matches_git_hash_object() {
        // `git hash-object` for the empty blob is well-known.
        assert_eq!(
            git_blob_sha1(b""),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        // `git hash-object` of "hello\n" ↔ documented value.
        assert_eq!(
            git_blob_sha1(b"hello\n"),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn language_name_returns_correct_tag() {
        assert_eq!(Language::Rust.name(), "rust");
        assert_eq!(Language::Python.name(), "python");
    }

    #[test]
    fn language_equality_via_ref() {
        let r = Language::Rust;
        assert!(r == Language::Rust);
        assert!(!(r == Language::Python));
    }

    #[test]
    fn code_parser_language_accessor() {
        assert_eq!(CodeParser::rust().language(), Language::Rust);
        assert_eq!(CodeParser::python().language(), Language::Python);
    }

    #[test]
    fn rust_parser_struct_and_trait_atoms() {
        let store = Store::new();
        let src = b"pub struct Foo {}\npub trait Bar {}\n";
        CodeParser::rust()
            .parse_into(src, "src/types.rs", &store)
            .expect("parse");
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
            .parse_into(src, "src/misc.rs", &store)
            .expect("parse");
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
            .parse_into(src, "src/lib.rs", &store)
            .expect("parse");
        let id = EntityId::new("code:src/lib.rs::function::documented");
        let atom = store.get_atom(&id).expect("atom");
        assert!(atom.doc.contains("Does something"), "doc={:?}", atom.doc);
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

    #[test]
    fn rust_impl_for_trait_naming() {
        let store = Store::new();
        let src = b"pub trait Foo {}\npub struct Bar;\nimpl Foo for Bar {}\n";
        CodeParser::rust()
            .parse_into(src, "src/impl_trait.rs", &store)
            .expect("parse");
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
    fn module_name_no_extension() {
        // file with no dot → returns full basename
        assert_eq!(module_name("Makefile"), "Makefile");
    }

    fn imports_from(src: &str) -> HashMap<String, String> {
        let mut p = TsParser::new();
        p.set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("rust grammar");
        let tree = p.parse(src, None).expect("parse");
        extract_use_imports(tree.root_node(), src)
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
            .parse_into(src, "src/lib.rs", &store)
            .expect("parse");
        let id = EntityId::new("code:src/lib.rs::function::caller");
        let atom = store.get_atom(&id).expect("atom");
        // Bare `helper()` should be normalised to the imported full path.
        assert!(
            atom.calls.iter().any(|c| c == "crate::other::helper"),
            "calls={:?}",
            atom.calls
        );
    }

    #[test]
    fn qualified_inline_call_keeps_full_path() {
        let store = Store::new();
        let src = b"fn caller() { project::render(s); }\n";
        CodeParser::rust()
            .parse_into(src, "src/lib.rs", &store)
            .expect("parse");
        let id = EntityId::new("code:src/lib.rs::function::caller");
        let atom = store.get_atom(&id).expect("atom");
        assert!(
            atom.calls.iter().any(|c| c == "project::render"),
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
            .parse_into(src, "src/lib.rs", &store)
            .expect("parse");
        let caller_id = EntityId::new("code:src/lib.rs::function::caller");
        let local_render = EntityId::new("code:src/lib.rs::function::render");
        assert!(
            !store.has_call_edge(&caller_id, &local_render),
            "qualified call should not bind to same-file function"
        );
    }
}
