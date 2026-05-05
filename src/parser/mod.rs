//! Code parser — tree-sitter Rust + Python → [`CodeAtom`]s.
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
//!
//! # Layout
//!
//! Per-language extractors live in sibling modules — `rust`, `python`,
//! `typescript`. This file holds the shared types ([`Language`],
//! [`CodeParser`], [`ParseFailure`], [`ParseUnit`]) and the dispatch that
//! routes each file at its language module.

use crate::error::{AstToMermaidError, Result};
use crate::graph::Store;
use crate::model::{CodeAtom, Edge, EdgeKind, EntityId};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use tree_sitter::{Node, Parser as TsParser, QueryCursor, StreamingIterator};

mod python;
mod queries;
mod rust;
mod typescript;

// ── Parse output ─────────────────────────────────────────────────────────────

/// Per-file failure recorded during the parse phase. The pipeline catches
/// each file's error, appends a [`ParseFailure`], and continues — so a
/// single malformed file no longer aborts the whole run.
///
/// Currently only the path + a one-line reason are surfaced; the slot is
/// reserved so future iterations can fold in line/col when the underlying
/// parser supplies them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseFailure {
    /// Display path of the file we failed on (relative-to-root,
    /// slash-joined — same shape as the path stored on every atom).
    pub path: String,
    /// Human-readable failure reason, typically the wrapped parser error
    /// message.
    pub reason: String,
}

/// Output of parsing one file: the atoms (module + items + lifted impl
/// methods) and the intra-file edges (Contains, intra-file Calls). Cross-
/// module edges are added later by the resolver.
///
/// This is the unit the content-addressed cache stores keyed by git blob
/// SHA-1 — two files with identical content produce identical `ParseUnit`s,
/// so a `git ls-tree` blob hit avoids tree-sitter entirely.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ParseUnit {
    /// Module atom + per-item atoms + lifted impl-method atoms.
    pub atoms: Vec<CodeAtom>,
    /// Contains edges (module → item, impl → method) and intra-file Calls
    /// edges (free-fn → free-fn, intra-impl method → method).
    pub edges: Vec<Edge>,
}

impl ParseUnit {
    /// Apply this unit's atoms and edges to `store`, in their recorded order.
    pub fn apply_to(self, store: &Store) {
        for a in self.atoms {
            store.add_atom(a);
        }
        for e in self.edges {
            store.add_edge(e);
        }
    }

    /// Number of atoms this unit contributed (cache-hit replay metric).
    #[must_use]
    pub fn atom_count(&self) -> usize {
        self.atoms.len()
    }
}

/// Git blob SHA-1 hex digest of `bytes`: `SHA1("blob {len}\0" + bytes)`.
///
/// Produces the same value as `git hash-object <file>` — used as the
/// cache key for atom-level memoization. Distinct from `hex_sha256`,
/// which we use for the user-visible `content_hash` field on atoms.
#[must_use]
pub fn git_blob_sha1(bytes: &[u8]) -> String {
    let mut hasher = sha1_smol::Sha1::new();
    hasher.update(format!("blob {}\0", bytes.len()).as_bytes());
    hasher.update(bytes);
    hasher.digest().to_string()
}

/// Strip a leading UTF-8 BOM (`EF BB BF`) if present, otherwise return
/// `bytes` unchanged. Tree-sitter's first-token detection trips on the
/// BOM and reports the whole file as malformed.
#[must_use]
pub fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

/// Replace bare `\r` (CR-only line endings, classic-Mac style) with `\n`.
/// `\r\n` (CRLF) is left as-is — tree-sitter and `str::lines` already
/// handle it. Returns `Cow::Borrowed` when no rewrite is needed so the
/// hot path stays allocation-free.
#[must_use]
pub fn normalize_eol(bytes: &[u8]) -> Cow<'_, [u8]> {
    let needs_rewrite = bytes
        .iter()
        .enumerate()
        .any(|(i, &b)| b == b'\r' && bytes.get(i + 1) != Some(&b'\n'));
    if !needs_rewrite {
        return Cow::Borrowed(bytes);
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' && bytes.get(i + 1) != Some(&b'\n') {
            out.push(b'\n');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    Cow::Owned(out)
}

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

    /// Parse `content` from `file_path` into a [`ParseUnit`] (atoms + intra-
    /// file edges). Output is fully determined by `(content, file_path,
    /// language)` — same inputs always produce byte-identical units, which
    /// is what makes content-addressed caching by git blob SHA-1 correct.
    ///
    /// # Errors
    ///
    /// - `InvalidInput` for non-UTF-8 content.
    /// - `InvalidInput` when tree-sitter fails to parse.
    #[allow(clippy::too_many_lines)]
    pub fn parse(&self, content: &[u8], file_path: &str) -> Result<ParseUnit> {
        // Normalise three encoding edges before tree-sitter sees the bytes:
        // a leading UTF-8 BOM (trips first-token detection) and CR-only
        // line endings (break line-based scanners like `rust_doc_comment`).
        // Both are no-ops for well-formed UTF-8 / LF / CRLF input.
        let normalized = normalize_eol(strip_bom(content));
        let content: &[u8] = normalized.as_ref();
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
        let module_hash = hex_sha256(content);
        let root = tree.root_node();
        let module_line_end = u32::try_from(root.end_position().row).unwrap_or(u32::MAX) + 1;
        atoms.push(CodeAtom {
            id: module_id.clone(),
            kind: "module".to_owned(),
            name: module_name,
            file_path: file_path.to_owned(),
            line_start: 1,
            line_end: module_line_end,
            doc: String::new(),
            signature: String::new(),
            content_hash: module_hash,
            calls: Vec::new(),
            method_calls: Vec::new(),
            parent: None,
        });

        // ── File-scope use imports (Rust only) ────────────────────────────────
        let imports: HashMap<String, String> = if self.language == Language::Rust {
            let decls = rust::extract_use_decls(root, text);
            rust::use_decls_to_imports(&decls)
        } else {
            HashMap::new()
        };

        // ── Item atoms ────────────────────────────────────────────────────────
        let items_query = match self.language {
            Language::Rust => &queries::RUST.items,
            Language::Python => &queries::PYTHON.items,
        };
        let mut name_to_id: HashMap<String, EntityId> = HashMap::new();
        let mut items: Vec<(EntityId, Vec<String>)> = Vec::new();

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(items_query, root, text.as_bytes());
        while let Some(m) = matches.next() {
            for capture in m.captures {
                let item_node = capture.node;
                let Some((atom, call_names)) =
                    extract_item(&item_node, text, file_path, self.language, &imports)
                else {
                    continue;
                };
                let item_id = atom.id.clone();
                let item_name = atom.name.clone();
                name_to_id.insert(item_name.clone(), item_id.clone());

                edges.push(Edge::new(
                    module_id.clone(),
                    item_id.clone(),
                    EdgeKind::Contains,
                ));
                items.push((item_id.clone(), call_names));
                atoms.push(atom);

                // For Rust `impl` blocks and Python `class` definitions,
                // lift every method to a first-class function atom.
                let method_descent = match (self.language, item_node.kind()) {
                    (Language::Rust, "impl_item") => Some((
                        item_name.as_str(),
                        impl_owner_type(&item_name),
                        &queries::RUST.impl_methods,
                    )),
                    (Language::Python, "class_definition") => Some((
                        item_name.as_str(),
                        item_name.as_str(),
                        &queries::PYTHON.class_methods,
                    )),
                    _ => None,
                };
                if let Some((container_name, parent_type, method_query)) = method_descent {
                    let methods = extract_methods(
                        &item_node,
                        &MethodCtx {
                            container_atom_id: &item_id,
                            container_name,
                            parent_type,
                            method_query,
                            source: text,
                            file_path,
                            language: self.language,
                            imports: &imports,
                        },
                        &mut atoms,
                        &mut edges,
                    );
                    items.extend(methods);
                }
            }
        }

        // ── Intra-file call edges ─────────────────────────────────────────────
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
    // Python decorators — unwrap inner definition iteratively. Tree-sitter
    // can in principle hand us arbitrarily nested `decorated_definition`
    // nodes (a malicious or pathological source — `@a` then `@b` etc.
    // each producing another wrapper); the previous tail-call would blow
    // the stack on adversarial input. The loop is bounded by the input
    // tree depth and short-circuits the moment an inner definition is
    // missing.
    let mut node = *node;
    if !language.item_node_kinds().contains(&node.kind()) {
        return None;
    }
    while node.kind() == "decorated_definition" {
        node = node.child_by_field_name("definition")?;
        if !language.item_node_kinds().contains(&node.kind()) {
            return None;
        }
    }
    let ts_kind = node.kind();

    let atom_kind = language.map_node_kind(ts_kind)?;
    let item_name = extract_name(&node, source, ts_kind)?;

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

    let doc = doc_for(language, &node, source);

    // Call names for functions.
    let extracted = if ts_kind == "function_item" || ts_kind == "function_definition" {
        extract_calls(&node, source, language, imports)
    } else {
        ExtractedCalls::default()
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
        calls: extracted.calls.clone(),
        method_calls: extracted.method_calls.clone(),
        parent: None,
    };

    Some((atom, extracted.calls))
}

// ── Method descent (Rust impl + Python class) ────────────────────────────────

/// Context bundle for [`extract_methods`] — the fields are the same
/// per-file values [`CodeParser::parse`] threads through extraction.
#[derive(Clone, Copy)]
struct MethodCtx<'a> {
    /// Atom whose body we descend into (Rust `impl_item` or Python class).
    container_atom_id: &'a EntityId,
    /// Display name of the container (e.g. `"Foo"`, `"Display for Foo"`,
    /// or a Python class name). Used to build child method ids and as the
    /// prefix for intra-container `<owner>::method` calls.
    container_name: &'a str,
    /// Receiver type name to record on each method atom's `parent` field.
    /// For Rust trait impls this is just the type (`Foo`, not `Display for
    /// Foo`); for Python classes it is the class name.
    parent_type: &'a str,
    /// Compiled tree-sitter query that captures method nodes (`@method`)
    /// inside the container body.
    method_query: &'a tree_sitter::Query,
    source: &'a str,
    file_path: &'a str,
    language: Language,
    imports: &'a HashMap<String, String>,
}

/// Walk the body of a method container (Rust `impl_item` or Python
/// `class_definition`) and emit every method as a first-class function atom.
///
/// Each method atom:
/// - id   = `code:{file}::function::{container_name}::{method_name}` (the
///   container disambiguates methods sharing names across containers)
/// - kind = `"function"` (so the resolver and renderers treat them like
///   free functions)
/// - name = bare method name (display label)
/// - parent = `Some(parent_type)` — the receiver type. Drives the
///   resolver's qualified-only matching for cross-module method calls.
///
/// Edges emitted:
/// - `Contains`: container atom → method atom
/// - `Calls`: bare-name calls inside one method that match the bare name of
///   another method *in the same container* are linked directly (the global
///   resolver can't see this scope, so we do it here).
///
/// Returns the `(method_id, call_names)` tuples so the caller can feed them
/// into the file-wide cross-module resolver pass.
#[allow(clippy::too_many_lines)]
fn extract_methods(
    container_node: &Node,
    ctx: &MethodCtx,
    out_atoms: &mut Vec<CodeAtom>,
    out_edges: &mut Vec<Edge>,
) -> Vec<(EntityId, Vec<String>)> {
    let MethodCtx {
        container_atom_id,
        container_name,
        parent_type,
        method_query,
        source,
        file_path,
        language,
        imports,
    } = *ctx;

    // Pass 1: extract every method, build the container-local name lookup.
    let mut method_id_by_name: HashMap<String, EntityId> = HashMap::new();
    let mut pending: Vec<(EntityId, CodeAtom, ExtractedCalls)> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(method_query, *container_node, source.as_bytes());
    while let Some(m) = matches.next() {
        for capture in m.captures {
            // Python class bodies can hold `decorated_definition` wrappers;
            // unwrap to the inner `function_definition` so the `name` field
            // and span come from the actual def, not the decorator chain.
            let inner = if capture.node.kind() == "decorated_definition" {
                let Some(def) = capture.node.child_by_field_name("definition") else {
                    continue;
                };
                def
            } else {
                capture.node
            };
            let Some(method_name) = inner
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
                .map(str::to_owned)
            else {
                continue;
            };

            let item_text = inner.utf8_text(source.as_bytes()).unwrap_or_default();
            let content_hash = format!("sha256:{}", hex_sha256(item_text.as_bytes()));
            let line_start = u32::try_from(inner.start_position().row).unwrap_or(u32::MAX) + 1;
            let line_end = u32::try_from(inner.end_position().row).unwrap_or(u32::MAX) + 1;
            let signature = item_text
                .lines()
                .next()
                .unwrap_or_default()
                .trim_end_matches('{')
                .trim()
                .to_owned();
            let doc = doc_for(language, &inner, source);
            let extracted = extract_calls(&inner, source, language, imports);

            let id = EntityId::new(format!(
                "code:{file_path}::function::{container_name}::{method_name}"
            ));
            let atom = CodeAtom {
                id: id.clone(),
                kind: "function".to_owned(),
                name: method_name.clone(),
                file_path: file_path.to_owned(),
                line_start,
                line_end,
                doc,
                signature,
                content_hash,
                calls: extracted.calls.clone(),
                method_calls: extracted.method_calls.clone(),
                parent: Some(parent_type.to_owned()),
            };
            method_id_by_name.insert(method_name, id.clone());
            pending.push((id, atom, extracted));
        }
    }

    // Pass 2: emit atoms + Contains edges + intra-container Calls edges.
    let owner_prefix = format!("{container_name}::");
    let parent_prefix = format!("{parent_type}::");
    let mut out: Vec<(EntityId, Vec<String>)> = Vec::with_capacity(pending.len());
    for (method_id, atom, extracted) in pending {
        out_edges.push(Edge::new(
            container_atom_id.clone(),
            method_id.clone(),
            EdgeKind::Contains,
        ));
        // Intra-container linking sees BOTH `calls` (qualified / free-form)
        // AND `method_calls` (`self.method()`-shaped). The receiver type
        // *is* known here — it's this container — so we do want to bind
        // a sibling method when the bare name matches.
        for call_name in extracted.calls.iter().chain(extracted.method_calls.iter()) {
            // Normalise: bare `foo`, `Self::foo`, `<container>::foo`, and
            // `<parent_type>::foo` all refer to a sibling method of the
            // same container. The two prefixes differ for Rust trait impls
            // (`"Display for Foo"` vs `"Foo"`); for Python they coincide.
            let local_target = if let Some(rest) = call_name.strip_prefix("Self::") {
                Some(rest)
            } else if !call_name.contains("::") {
                Some(call_name.as_str())
            } else if let Some(rest) = call_name.strip_prefix(&owner_prefix) {
                Some(rest)
            } else {
                call_name.strip_prefix(&parent_prefix)
            };
            let Some(target_name) = local_target else {
                continue;
            };
            if let Some(target_id) = method_id_by_name.get(target_name)
                && *target_id != method_id
            {
                out_edges.push(Edge::new(
                    method_id.clone(),
                    target_id.clone(),
                    EdgeKind::Calls,
                ));
            }
        }
        out_atoms.push(atom);
        // Hand the resolver-eligible calls back to the caller so they
        // feed into the cross-module pass — `method_calls` are
        // intentionally dropped at this boundary.
        out.push((method_id, extracted.calls));
    }

    out
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

// ── Doc + call dispatch ──────────────────────────────────────────────────────

fn doc_for(language: Language, node: &Node, source: &str) -> String {
    match language {
        Language::Rust => rust::doc_comment(source, node.start_position().row),
        Language::Python => python::docstring(node, source),
    }
}

/// Bundle of call-site captures coming out of [`extract_calls`].
///
/// `calls` are resolver-eligible: free-fn calls (bare identifier, possibly
/// expanded via `use` imports) and qualified `module::foo` /
/// `Owner::method` paths.
///
/// `method_calls` are receiver-style captures (`obj.method()`) — the
/// receiver type is unknown, so they only carry weight for intra-
/// container linking (`self.method()`). The cross-module resolver
/// ignores them; that is what stops `client.build()` from ghost-binding
/// to a free fn `build` defined in some unrelated module.
#[derive(Debug, Default, Clone)]
pub(super) struct ExtractedCalls {
    pub(super) calls: Vec<String>,
    pub(super) method_calls: Vec<String>,
}

/// Dispatch call extraction to the matching language module, then
/// deduplicate the resulting lists in-place. Per-language extractors
/// append to `out` without worrying about duplicates so the dedupe pass
/// stays in one place.
fn extract_calls(
    node: &Node,
    source: &str,
    language: Language,
    imports: &HashMap<String, String>,
) -> ExtractedCalls {
    let mut out = ExtractedCalls::default();
    match language {
        Language::Rust => rust::extract_calls(node, source, imports, &mut out),
        Language::Python => python::extract_calls(node, source, &mut out),
    }
    let mut seen: HashSet<String> = HashSet::new();
    out.calls.retain(|c| seen.insert(c.clone()));
    seen.clear();
    out.method_calls.retain(|c| seen.insert(c.clone()));
    out
}

// ── Utilities ─────────────────────────────────────────────────────────────────

/// SHA-256 hex digest of `bytes`.
fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as FmtWrite;
        write!(out, "{byte:02x}").expect("string write is infallible");
    }
    out
}

/// Extract the module name (file stem) from a path.
#[must_use]
pub fn module_name(path: &str) -> &str {
    let basename = path.rsplit('/').next().unwrap_or(path);
    basename.rsplit_once('.').map_or(basename, |(stem, _)| stem)
}

/// Reduce an `impl` block's owner string to just the implementing type.
///
/// `extract_name` produces `"Foo"` for inherent impls and `"Trait for Foo"`
/// for trait impls (possibly with generics or `path::Trait`). The resolver
/// only cares about the receiver type, so we strip the `Trait for ` prefix
/// and any generics tail.
///
/// `Foo`                            → `Foo`
/// `Display for Foo`                → `Foo`
/// `fmt::Debug for Foo`             → `Foo`
/// `Iterator<Item = u32> for Foo<T>`→ `Foo<T>`  (generics on the *type* are
///                                              kept verbatim — the resolver
///                                              also uses the type's bare
///                                              name only when matching)
fn impl_owner_type(owner: &str) -> &str {
    owner.split_once(" for ").map_or(owner, |(_, t)| t).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_utf8_errors() {
        let err = CodeParser::rust()
            .parse(&[0xff, 0xfe], "bad.rs")
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
    fn module_name_no_extension() {
        // file with no dot → returns full basename
        assert_eq!(module_name("Makefile"), "Makefile");
    }

    #[test]
    fn hex_sha256_deterministic() {
        let a = hex_sha256(b"hello");
        let b = hex_sha256(b"hello");
        assert_eq!(a, b);
        let c = hex_sha256(b"world");
        assert_ne!(a, c);
    }

    #[test]
    fn language_name_returns_correct_tag() {
        assert_eq!(Language::Rust.name(), "rust");
        assert_eq!(Language::Python.name(), "python");
    }

    #[test]
    fn code_parser_language_accessor() {
        assert_eq!(CodeParser::rust().language(), Language::Rust);
        assert_eq!(CodeParser::python().language(), Language::Python);
    }

    #[test]
    fn encoding_edges_strip_bom_removes_leading_efbbbf() {
        assert_eq!(strip_bom(&[0xEF, 0xBB, 0xBF, b'a', b'b']), b"ab");
        // Idempotent on input without BOM.
        assert_eq!(strip_bom(b"ab"), b"ab");
        // Only leading BOM is stripped — mid-buffer bytes are kept.
        assert_eq!(
            strip_bom(&[b'a', 0xEF, 0xBB, 0xBF, b'b']),
            &[b'a', 0xEF, 0xBB, 0xBF, b'b']
        );
        // Partial / single-byte input doesn't panic.
        assert_eq!(strip_bom(b""), b"");
        assert_eq!(strip_bom(&[0xEF]), &[0xEF]);
    }

    #[test]
    fn encoding_edges_normalize_eol_rewrites_cr_only_keeps_crlf() {
        // CR-only → LF.
        assert_eq!(normalize_eol(b"a\rb\rc").as_ref(), b"a\nb\nc");
        // CRLF stays as-is.
        assert_eq!(normalize_eol(b"a\r\nb").as_ref(), b"a\r\nb");
        // Mixed: CRLF preserved, lone CR rewritten.
        assert_eq!(normalize_eol(b"a\r\nb\rc").as_ref(), b"a\r\nb\nc");
        // No change → borrowed (no allocation).
        let s: &[u8] = b"plain\nlf\nonly";
        let cow = normalize_eol(s);
        assert!(matches!(cow, std::borrow::Cow::Borrowed(_)));
        // Trailing bare CR is rewritten too.
        assert_eq!(normalize_eol(b"end\r").as_ref(), b"end\n");
    }

    #[test]
    fn encoding_edges_parser_accepts_file_with_bom() {
        // EF BB BF prefix on otherwise-valid Rust source. Without
        // BOM-stripping tree-sitter reports the whole file as malformed.
        let mut src: Vec<u8> = vec![0xEF, 0xBB, 0xBF];
        src.extend_from_slice(b"/// hello\nfn main() {}\n");
        let store = Store::new();
        CodeParser::rust()
            .parse(&src, "bom.rs")
            .expect("parse must succeed after BOM strip")
            .apply_to(&store);
        let id = EntityId::new("code:bom.rs::function::main");
        let atom = store.get_atom(&id).expect("function atom");
        assert!(atom.doc.contains("hello"), "doc={:?}", atom.doc);
    }

    #[test]
    fn encoding_edges_parser_accepts_cr_only_line_endings() {
        // Classic-Mac line endings: bare \r between lines.
        let src = b"/// docline\rfn cr_only() {}\r";
        let store = Store::new();
        CodeParser::rust()
            .parse(src, "cr.rs")
            .expect("parse must succeed after CR-only normalisation")
            .apply_to(&store);
        let id = EntityId::new("code:cr.rs::function::cr_only");
        let atom = store.get_atom(&id).expect("function atom");
        assert!(atom.doc.contains("docline"), "doc={:?}", atom.doc);
    }

    #[test]
    fn extract_item_decorator_unwraps_to_inner_function() {
        // A Python `@deco`-prefixed top-level function. tree-sitter wraps
        // it in `decorated_definition`; extract_item must unwrap that and
        // surface the inner `function_definition`'s name + body.
        let src = b"@deco\ndef wrapped():\n    pass\n";
        let store = Store::new();
        CodeParser::python()
            .parse(src, "deco.py")
            .expect("parse")
            .apply_to(&store);
        let id = EntityId::new("code:deco.py::function::wrapped");
        let atom = store
            .get_atom(&id)
            .expect("decorated function should produce a function atom");
        assert_eq!(atom.name, "wrapped");
        assert_eq!(atom.kind, "function");
    }

    #[test]
    fn extract_item_handles_stacked_decorators_iteratively() {
        // Multiple stacked decorators on one definition. The wrapper is a
        // single `decorated_definition` (tree-sitter's grammar collapses
        // the chain), but the iterative unwrap must still terminate
        // correctly and yield the inner def. This is the canonical
        // shape that exercises the loop body.
        let src = b"@deco_one\n@deco_two\n@deco_three\ndef triple():\n    pass\n";
        let store = Store::new();
        CodeParser::python()
            .parse(src, "stacked.py")
            .expect("parse")
            .apply_to(&store);
        let id = EntityId::new("code:stacked.py::function::triple");
        assert!(
            store.get_atom(&id).is_some(),
            "stacked-decorator function must extract"
        );
    }
}
