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
/// (they don't introduce a name in *this* file's scope); those are read by
/// [`extract_pub_use_decls`].
///
/// Group forms (`use foo::{a, b}`), aliases (`use foo as bar`), wildcards
/// (`use foo::*`), and nested paths are all unfolded into one [`UseDecl`]
/// per leaf by [`flatten_use`].
pub(super) fn extract_use_decls(root: Node, source: &str) -> Vec<UseDecl> {
    collect_use_decls(root, source, false, false)
}

/// The file's own imports — `use` and `pub use` at the top of the file,
/// not those of an inline `mod tests { use super::*; }`, whose paths are
/// relative to a module the graph does not model.
pub(super) fn extract_file_imports(root: Node, source: &str) -> Vec<UseDecl> {
    let mut out = collect_use_decls(root, source, false, true);
    out.extend(collect_use_decls(root, source, true, true));
    out
}

/// The `pub use` / `pub(crate) use` declarations of a file: the names it
/// re-exports, which is what lets `crate::render::mermaid_to_dot` reach a
/// function defined in `render/dot.rs`.
pub(super) fn extract_pub_use_decls(root: Node, source: &str) -> Vec<UseDecl> {
    collect_use_decls(root, source, true, true)
}

fn collect_use_decls(root: Node, source: &str, public: bool, top_level: bool) -> Vec<UseDecl> {
    let mut out: Vec<UseDecl> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&queries::RUST.uses, root, source.as_bytes());
    while let Some(m) = matches.next() {
        for cap in m.captures {
            let n = cap.node;
            if has_visibility_modifier(n) != public
                || (top_level && n.parent().is_some_and(|p| p.kind() != "source_file"))
            {
                continue;
            }
            if let Some(arg) = use_argument(&n) {
                flatten_use(arg, "", source, &mut out, 0);
            }
        }
    }
    out
}

/// What a Rust file tells the call extractor beyond a body's own text.
#[derive(Debug, Default)]
pub(super) struct RustScope {
    /// `use` map: local name → path as written (see [`use_decls_to_imports`]).
    pub imports: HashMap<String, String>,
    /// Field types of the structs this file declares: struct → field →
    /// type. Types `self.field.method()`.
    pub fields: HashMap<String, HashMap<String, String>>,
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
/// normalised to their fully-qualified path via the file's imports, and so
/// is the first segment of a qualified call (`render::foo()` after
/// `use crate::render`). Receiver-style calls (`obj.method()`) become
/// `Type::method` when the receiver's type is declared — a typed `let`, a
/// parameter, `self`, or a field of `self` — and are otherwise routed into
/// `out.method_calls` so the cross-module resolver never matches them by
/// name.
pub(super) fn extract_calls(
    node: &Node,
    source: &str,
    scope: &RustScope,
    self_type: Option<&str>,
    out: &mut ExtractedCalls,
) {
    let query = &queries::RUST.calls;
    // Capture index → name lookup so we can route each match by
    // capture role (`@call.fn`, `@call.field`).
    let cn_call_fn = query.capture_index_for_name("call.fn");
    let cn_call_field = query.capture_index_for_name("call.field");
    let locals = local_types(*node, source, self_type);
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
                        let resolved = scope
                            .imports
                            .get(text)
                            .map_or_else(|| text.to_owned(), String::clone);
                        out.push_call(resolved, &cap.node, Language::Rust);
                    }
                    "scoped_identifier" => {
                        // Qualified — keep the path so the resolver can
                        // disambiguate by module / type, with its head
                        // expanded through the imports when it names one.
                        out.push_call(expand_head(text, &scope.imports), &cap.node, Language::Rust);
                    }
                    _ => {}
                }
            } else if Some(cap.index) == cn_call_field {
                let owner = cap
                    .node
                    .parent()
                    .and_then(|fe| fe.child_by_field_name("value"))
                    .and_then(|recv| receiver_type(recv, source, &locals, scope, self_type, 0));
                if let Some(owner) = owner {
                    out.push_typed_call(format!("{owner}::{text}"), &cap.node, Language::Rust);
                } else {
                    // `obj.method` → `method`. Receiver type is
                    // unknown, so this never feeds the resolver — it
                    // only powers intra-container linking.
                    out.push_method_call(text.to_owned(), &cap.node, Language::Rust);
                }
            }
        }
    }
    macro_calls(*node, source, scope, out);
}

/// Calls written inside macro arguments: `eprintln!("{}", summary(x))`,
/// `assert_eq!(parse(s), …)`, `vec![Foo::new()]`.
///
/// tree-sitter does not parse a macro's arguments — they are a flat
/// `token_tree` — so the query above never sees them, and a function
/// called only from a `format!` looked dead. Here a call is read off the
/// tokens: a path (`a`, `a::b`) directly followed by a parenthesised
/// group. After a `.` it is a method call, kept untyped. A path followed
/// by `!` is a nested macro, not a call.
fn macro_calls(node: Node, source: &str, scope: &RustScope, out: &mut ExtractedCalls) {
    let bytes = source.as_bytes();
    let mut stack = vec![node];
    let mut visited = 0usize;
    while let Some(n) = stack.pop() {
        visited += 1;
        if visited > 200_000 {
            break;
        }
        if n.kind() == "token_tree" {
            let mut c = n.walk();
            let tokens: Vec<Node> = n.children(&mut c).collect();
            let mut i = 0;
            while i < tokens.len() {
                let t = tokens[i];
                if t.kind() == "token_tree" {
                    if t.child(0).is_some_and(|o| o.kind() == "(")
                        && let Some(end) = i.checked_sub(1)
                        && tokens[end].kind() == "identifier"
                    {
                        // Walk back over `ident (:: ident)*`.
                        let mut start = end;
                        while start >= 2
                            && tokens[start - 1].kind() == "::"
                            && tokens[start - 2].kind() == "identifier"
                        {
                            start -= 2;
                        }
                        let path: Vec<&str> = (start..=end)
                            .step_by(2)
                            .filter_map(|k| tokens[k].utf8_text(bytes).ok())
                            .collect();
                        let callee = tokens[end];
                        let after_dot = start
                            .checked_sub(1)
                            .is_some_and(|k| tokens[k].kind() == ".");
                        if after_dot {
                            out.push_method_call(path.join("::"), &callee, Language::Rust);
                        } else if path.len() == 1 {
                            let name = path[0];
                            let resolved = scope
                                .imports
                                .get(name)
                                .map_or_else(|| name.to_owned(), String::clone);
                            out.push_call(resolved, &callee, Language::Rust);
                        } else {
                            out.push_call(
                                expand_head(&path.join("::"), &scope.imports),
                                &callee,
                                Language::Rust,
                            );
                        }
                    }
                    stack.push(t);
                }
                i += 1;
            }
            continue;
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            stack.push(child);
        }
    }
}

/// Expand the head of a qualified path through the file's imports:
/// `render::foo` after `use crate::render` → `crate::render::foo`.
/// Path roots (`crate`, `self`, `super`, `Self`) are never rewritten.
fn expand_head(path: &str, imports: &HashMap<String, String>) -> String {
    let (head, rest) = path.split_once("::").unwrap_or((path, ""));
    if rest.is_empty() || matches!(head, "crate" | "self" | "super" | "Self") {
        return path.to_owned();
    }
    match imports.get(head) {
        Some(full) => format!("{full}::{rest}"),
        None => path.to_owned(),
    }
}

/// Type a call receiver evaluates to, when the source declares it.
///
/// `self` → the impl owner; a name → its typed `let` / parameter;
/// `recv.field` → the field's declared type on `recv`'s struct. Anything
/// else (a call result, an index, a closure parameter) is unknown.
fn receiver_type(
    recv: Node,
    source: &str,
    locals: &HashMap<String, Option<String>>,
    scope: &RustScope,
    self_type: Option<&str>,
    depth: usize,
) -> Option<String> {
    if depth > 4 {
        return None;
    }
    match recv.kind() {
        "self" => self_type.map(str::to_owned),
        "identifier" => locals.get(recv.utf8_text(source.as_bytes()).ok()?)?.clone(),
        "field_expression" => {
            let base = recv.child_by_field_name("value")?;
            let field = recv.child_by_field_name("field")?;
            let owner = receiver_type(base, source, locals, scope, self_type, depth + 1)?;
            scope
                .fields
                .get(&owner)?
                .get(field.utf8_text(source.as_bytes()).ok()?)
                .cloned()
        }
        _ => None,
    }
}

/// Name → declared type for every binding in `body` whose type is
/// spelled out: typed parameters, `let x: T`, `let x = T::f(..)`,
/// `let x = T { .. }`.
///
/// Flat over the body, not scoped: a name bound to two different types
/// maps to `None` and is never used — a wrong type would bind a call to
/// the wrong method, which is worse than no edge.
fn local_types(
    body: Node,
    source: &str,
    self_type: Option<&str>,
) -> HashMap<String, Option<String>> {
    let mut out: HashMap<String, Option<String>> = HashMap::new();
    let record = |out: &mut HashMap<String, Option<String>>, name: &str, ty: Option<String>| {
        let Some(ty) = ty else { return };
        out.entry(name.to_owned())
            .and_modify(|e| {
                if e.as_deref() != Some(ty.as_str()) {
                    *e = None;
                }
            })
            .or_insert(Some(ty));
    };
    let mut unwrapped: Vec<Node> = Vec::new();
    // Bindings declared `Option<T>` / `Result<T, _>`, mapped to `T`.
    let mut inner: HashMap<String, Option<String>> = HashMap::new();
    let mut stack = vec![body];
    let mut visited = 0usize;
    while let Some(n) = stack.pop() {
        visited += 1;
        if visited > 200_000 {
            break;
        }
        match n.kind() {
            "parameter" => {
                if let (Some(pat), Some(ty)) = (
                    n.child_by_field_name("pattern"),
                    n.child_by_field_name("type"),
                ) && let Some(name) = binding_name(pat, source)
                {
                    record(&mut out, name, type_name(ty, source, self_type));
                    record(
                        &mut inner,
                        name,
                        type_name_inner(ty, source, self_type, true),
                    );
                }
            }
            "let_declaration" => {
                if let Some(name) = n
                    .child_by_field_name("pattern")
                    .and_then(|p| binding_name(p, source))
                {
                    let ty = match n.child_by_field_name("type") {
                        Some(t) => {
                            record(
                                &mut inner,
                                name,
                                type_name_inner(t, source, self_type, true),
                            );
                            type_name(t, source, self_type)
                        }
                        None => n
                            .child_by_field_name("value")
                            .and_then(|v| value_type(v, source, self_type, 0)),
                    };
                    record(&mut out, name, ty);
                } else if n.child_by_field_name("type").is_none() {
                    unwrapped.push(n);
                }
            }
            "let_condition" => unwrapped.push(n),
            _ => {}
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            stack.push(child);
        }
    }
    // `if let Some(c) = cache` / `let Ok(f) = File::open(p) else { … }`:
    // `c` has the type the value has, seen through `Option` / `Result`.
    // Read after the main pass so the value's own binding is known.
    for n in unwrapped {
        let (Some(pat), Some(value)) = (
            n.child_by_field_name("pattern"),
            n.child_by_field_name("value"),
        ) else {
            continue;
        };
        let Some(name) = unwrapping_binding(pat, source) else {
            continue;
        };
        let ty = match value.kind() {
            "identifier" => value
                .utf8_text(source.as_bytes())
                .ok()
                .and_then(|v| inner.get(v).cloned().flatten()),
            _ => value_type(value, source, self_type, 0),
        };
        record(&mut out, name, ty);
    }
    // `fn f<S: Store>(s: &S)`: a binding typed by a generic parameter
    // exposes the methods of the parameter's trait bound.
    let bounds = enclosing_bounds(body, source);
    if !bounds.is_empty() {
        for ty in out.values_mut().flatten() {
            if let Some(b) = bounds.get(ty.as_str()) {
                ty.clone_from(b);
            }
        }
    }
    out
}

/// Generic parameter → trait bound, for the generics in scope at `node`:
/// those of the function itself and of its enclosing `impl` / `trait`
/// (`<S: Store>` and `where S: Store`). Marker traits (`Send`, `Clone`,
/// …) are skipped; a parameter keeps its first meaningful bound.
pub(super) fn enclosing_bounds(node: Node, source: &str) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    let mut cur = Some(node);
    for _ in 0..6 {
        let Some(n) = cur else { break };
        if matches!(
            n.kind(),
            "function_item"
                | "function_signature_item"
                | "impl_item"
                | "trait_item"
                | "struct_item"
        ) {
            generic_bounds(n, source, &mut out);
        }
        cur = n.parent();
    }
    out
}

/// Read `<T: Bound>` and `where T: Bound` of one item into `out`.
fn generic_bounds(item: Node, source: &str, out: &mut HashMap<String, String>) {
    let mut add = |left: Option<Node>, bounds: Option<Node>| {
        let (Some(left), Some(bounds)) = (left, bounds) else {
            return;
        };
        let Ok(param) = left.utf8_text(source.as_bytes()) else {
            return;
        };
        let mut c = bounds.walk();
        let first = bounds
            .named_children(&mut c)
            .filter(|b| !is_marker_bound(b, source))
            .find_map(|b| type_name(b, source, None));
        if let Some(b) = first {
            out.entry(param.to_owned()).or_insert(b);
        }
    };
    let mut c = item.walk();
    for child in item.named_children(&mut c) {
        match child.kind() {
            "type_parameters" => {
                let mut c2 = child.walk();
                for p in child.named_children(&mut c2) {
                    match p.kind() {
                        "type_parameter" => {
                            add(
                                p.child_by_field_name("name"),
                                p.child_by_field_name("bounds"),
                            );
                        }
                        // Older grammars.
                        "constrained_type_parameter" => {
                            add(
                                p.child_by_field_name("left"),
                                p.child_by_field_name("bounds"),
                            );
                        }
                        _ => {}
                    }
                }
            }
            "where_clause" => {
                let mut c2 = child.walk();
                for p in child.named_children(&mut c2) {
                    if p.kind() == "where_predicate" {
                        add(
                            p.child_by_field_name("left"),
                            p.child_by_field_name("bounds"),
                        );
                    }
                }
            }
            _ => {}
        }
    }
}

/// `Send`, `Sync`, `Clone`, … and lifetimes: bounds that say nothing
/// about which methods a receiver has.
fn is_marker_bound(b: &Node, source: &str) -> bool {
    if b.kind() == "lifetime" {
        return true;
    }
    let text = b.utf8_text(source.as_bytes()).unwrap_or_default();
    let name = text
        .rsplit("::")
        .next()
        .unwrap_or(text)
        .trim_start_matches('?');
    matches!(
        name,
        "Send"
            | "Sync"
            | "Sized"
            | "Unpin"
            | "Clone"
            | "Copy"
            | "Debug"
            | "Default"
            | "Eq"
            | "PartialEq"
            | "Ord"
            | "PartialOrd"
            | "Hash"
            | "Display"
    )
}

/// The identifier bound by `Some(x)` / `Ok(x)` / `Some(mut x)`.
fn unwrapping_binding<'a>(pat: Node, source: &'a str) -> Option<&'a str> {
    if pat.kind() != "tuple_struct_pattern" {
        return None;
    }
    let ctor = pat
        .child_by_field_name("type")?
        .utf8_text(source.as_bytes())
        .ok()?;
    if !matches!(ctor, "Some" | "Ok") {
        return None;
    }
    let mut c = pat.walk();
    let inner: Vec<Node> = pat.named_children(&mut c).skip(1).collect();
    match inner.as_slice() {
        [one] => binding_name(*one, source),
        _ => None,
    }
}

/// The identifier a pattern binds: `x`, or `mut x`.
fn binding_name<'a>(pat: Node, source: &'a str) -> Option<&'a str> {
    match pat.kind() {
        "identifier" => pat.utf8_text(source.as_bytes()).ok(),
        "mut_pattern" => {
            let mut c = pat.walk();
            pat.named_children(&mut c)
                .find(|n| n.kind() == "identifier")
                .and_then(|n| n.utf8_text(source.as_bytes()).ok())
        }
        _ => None,
    }
}

/// Type a value expression is known to produce: `T::f(..)` (an associated
/// function of a capitalised path), `T { .. }`, and those behind `?`,
/// `&` or `.await`. `Arc::new(x)` / `Box::new(x)` / `Rc::new(x)` see
/// through to `x` — method calls auto-deref.
fn value_type(v: Node, source: &str, self_type: Option<&str>, depth: usize) -> Option<String> {
    if depth > 4 {
        return None;
    }
    match v.kind() {
        "call_expression" => {
            let func = v.child_by_field_name("function")?;
            if func.kind() != "scoped_identifier" {
                return None;
            }
            let owner = func.child_by_field_name("path")?;
            let owner = owner.utf8_text(source.as_bytes()).ok()?;
            let owner = owner.rsplit("::").next().unwrap_or(owner);
            let owner = owner.split('<').next().unwrap_or(owner);
            if matches!(owner, "Arc" | "Rc" | "Box") {
                let args = v.child_by_field_name("arguments")?;
                let mut c = args.walk();
                let first = args.named_children(&mut c).next()?;
                return value_type(first, source, self_type, depth + 1);
            }
            capitalised(owner, self_type)
        }
        "struct_expression" => {
            let name = v.child_by_field_name("name")?;
            let text = name.utf8_text(source.as_bytes()).ok()?;
            capitalised(text.rsplit("::").next().unwrap_or(text), self_type)
        }
        // Every arm producing the same known type: `match lang {
        // Rust => CodeParser::rust(), … }`.
        "match_expression" => {
            let body = v.child_by_field_name("body")?;
            let mut c = body.walk();
            let mut ty: Option<String> = None;
            for arm in body.named_children(&mut c) {
                if arm.kind() != "match_arm" {
                    continue;
                }
                let t = value_type(
                    arm.child_by_field_name("value")?,
                    source,
                    self_type,
                    depth + 1,
                )?;
                if ty.as_ref().is_some_and(|prev| *prev != t) {
                    return None;
                }
                ty = Some(t);
            }
            ty
        }
        "try_expression" | "reference_expression" | "await_expression" => {
            let mut c = v.walk();
            let inner = v.named_children(&mut c).next()?;
            value_type(inner, source, self_type, depth + 1)
        }
        _ => None,
    }
}

/// `Self` → the impl owner; a capitalised name → itself; else `None`.
fn capitalised(name: &str, self_type: Option<&str>) -> Option<String> {
    if name == "Self" {
        return self_type.map(str::to_owned);
    }
    name.chars()
        .next()
        .is_some_and(char::is_uppercase)
        .then(|| name.to_owned())
}

/// The type whose methods a value of type `ty` exposes: `T`, `&T`,
/// `&mut T`, `path::T`, `T<..>`, and `Arc<T>` / `Rc<T>` / `Box<T>`
/// (auto-deref). `None` for primitives, tuples, `dyn` and `impl` types.
pub(super) fn type_name(ty: Node, source: &str, self_type: Option<&str>) -> Option<String> {
    type_name_inner(ty, source, self_type, false)
}

/// [`type_name`], or with `unwrap` the type inside an `Option` /
/// `Result`.
fn type_name_inner(
    ty: Node,
    source: &str,
    self_type: Option<&str>,
    unwrap: bool,
) -> Option<String> {
    let mut n = ty;
    for _ in 0..8 {
        match n.kind() {
            "type_identifier" => {
                return capitalised(n.utf8_text(source.as_bytes()).ok()?, self_type);
            }
            "scoped_type_identifier" => n = n.child_by_field_name("name")?,
            "reference_type" => n = n.child_by_field_name("type")?,
            // `dyn Store` / `impl Store`: methods are the trait's.
            "dynamic_type" | "abstract_type" => n = n.child_by_field_name("trait")?,
            // `dyn Store + Send`: the first bound that is not a marker.
            "bounded_type" => {
                let mut c = n.walk();
                n = n
                    .named_children(&mut c)
                    .find(|b| !is_marker_bound(b, source))?;
            }
            "generic_type" => {
                let outer = n.child_by_field_name("type")?;
                let outer_text = outer.utf8_text(source.as_bytes()).ok()?;
                let outer_name = outer_text.rsplit("::").next().unwrap_or(outer_text);
                // Auto-deref wrappers. `Option` / `Result` only when the
                // caller is unwrapping (`if let Some(x) = opt`): calling
                // `opt.take()` is `Option::take`, not the inner type's.
                if matches!(outer_name, "Arc" | "Rc" | "Box")
                    || (unwrap && matches!(outer_name, "Option" | "Result"))
                {
                    let args = n.child_by_field_name("type_arguments")?;
                    let mut c = args.walk();
                    n = args.named_children(&mut c).next()?;
                } else {
                    n = outer;
                }
            }
            _ => return None,
        }
    }
    None
}

/// Field types of every `struct` in the file: struct → field → type.
pub(super) fn collect_struct_fields(
    root: Node,
    source: &str,
) -> HashMap<String, HashMap<String, String>> {
    let mut out: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == "struct_item" {
            let name = n
                .child_by_field_name("name")
                .and_then(|x| x.utf8_text(source.as_bytes()).ok());
            let body = n.child_by_field_name("body");
            if let (Some(name), Some(body)) = (name, body) {
                // `struct Svc<S: Store> { store: S }` → `store: Store`.
                let mut bounds = HashMap::new();
                generic_bounds(n, source, &mut bounds);
                let mut fields = HashMap::new();
                let mut c = body.walk();
                for f in body.named_children(&mut c) {
                    if f.kind() != "field_declaration" {
                        continue;
                    }
                    let fname = f
                        .child_by_field_name("name")
                        .and_then(|x| x.utf8_text(source.as_bytes()).ok());
                    let fty = f
                        .child_by_field_name("type")
                        .and_then(|t| type_name(t, source, Some(name)))
                        .map(|t| bounds.get(&t).cloned().unwrap_or(t));
                    if let (Some(fname), Some(fty)) = (fname, fty) {
                        fields.insert(fname.to_owned(), fty);
                    }
                }
                out.insert(name.to_owned(), fields);
            }
            continue;
        }
        // Items only live at module level or inside `mod { }` blocks.
        if matches!(n.kind(), "source_file" | "mod_item" | "declaration_list") {
            let mut c = n.walk();
            for child in n.children(&mut c) {
                stack.push(child);
            }
        }
    }
    out
}

/// Types named in an item's declaration — struct / enum fields, a fn's
/// parameters and return type, a type alias' target. Names the file
/// imports carry their `use` path (`crate::graph::Store`), so the resolver
/// can bind them exactly; others stay bare.
pub(super) fn type_refs(
    item: Node,
    source: &str,
    imports: &HashMap<String, String>,
) -> Vec<String> {
    let roots: Vec<Node> = match item.kind() {
        "function_item" | "function_signature_item" => ["parameters", "return_type"]
            .iter()
            .filter_map(|f| item.child_by_field_name(f))
            .collect(),
        "struct_item" | "enum_item" | "union_item" => {
            item.child_by_field_name("body").into_iter().collect()
        }
        "type_item" => item.child_by_field_name("type").into_iter().collect(),
        _ => Vec::new(),
    };
    let mut out: Vec<String> = Vec::new();
    let mut stack = roots;
    let mut visited = 0usize;
    while let Some(n) = stack.pop() {
        visited += 1;
        if visited > 10_000 {
            break;
        }
        if n.kind() == "type_identifier"
            && let Ok(name) = n.utf8_text(source.as_bytes())
            && name != "Self"
        {
            let full = imports
                .get(name)
                .cloned()
                .unwrap_or_else(|| name.to_owned());
            if !out.contains(&full) {
                out.push(full);
            }
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            stack.push(child);
        }
    }
    out
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
