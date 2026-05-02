//! Cross-module call resolver.
//!
//! The parser already emits intra-file `calls` edges (function A calling
//! function B in the same file). This module walks the populated [`Store`]
//! and resolves cross-module calls — function A in `mod_a.rs` calling
//! function B in `mod_b.rs`.
//!
//! # Algorithm
//!
//! 1. List every `function` atom in the store.
//! 2. Split atoms into two indices:
//!    - `free_fns_by_name` (`atom.parent == None`) keyed by `name`;
//!    - `methods_by_owner_name` (`atom.parent == Some(owner)`) keyed by
//!      `(owner, name)`.
//! 3. Load existing `calls` edges so we never emit a duplicate.
//! 4. For each function whose `calls` field is non-empty:
//!    - Skip stdlib / iterator method noise (see [`SKIP_CALLS`]).
//!    - Each call name is either a bare identifier (e.g. `foo`) or a
//!      qualified path (e.g. `module::foo`, `Owner::method`,
//!      `crate::render::render`). The parser normalises bare calls via the
//!      file's `use` imports so the qualifier is present whenever the
//!      source provided one.
//!    - **Qualified calls** (`Q::name`, last qualifier segment `last`):
//!      try the method index `(last, name)` first — these are real, type-
//!      qualified Rust/Python method calls. If empty, fall back to the
//!      free-fn-by-module-name lookup (file's module name == `last`).
//!    - **Bare calls** (no qualifier — typically `obj.method()` where the
//!      receiver type is unknown): only match free functions. Method
//!      atoms are intentionally excluded so that `client.send()` cannot
//!      ghost-bind to an unrelated `DaemonHandle::send`.
//!    - In every viable-set, prefer a unique same-crate candidate, then a
//!      unique cross-crate one.
//!    - On ambiguity or zero matches: skip.
//!    - Emit a `calls` edge.
//!
//! Returns the number of new edges added.

use crate::graph::Store;
use crate::model::{CodeAtom, Edge, EdgeKind, EntityId};
use std::collections::{HashMap, HashSet};

/// Atom kind used for synthetic atoms representing calls into external
/// crates whose source isn't part of this run (e.g. `serde_json::to_string`,
/// `divan::main`).
pub const EXTERN_KIND: &str = "extern";

/// Function names skipped during resolution because they are common
/// stdlib / iterator / Python builtin methods. Linking them produces
/// huge call-graph noise without illuminating actual cross-module
/// dependencies.
#[doc(hidden)]
pub const SKIP_CALLS: &[&str] = &[
    // Rust Option/Result
    "unwrap",
    "expect",
    "ok",
    "err",
    "is_some",
    "is_none",
    "is_ok",
    "is_err",
    "unwrap_or",
    "unwrap_or_default",
    "unwrap_or_else",
    // Iterator / collection methods
    "map",
    "and_then",
    "or_else",
    "filter",
    "fold",
    "collect",
    "iter",
    "iter_mut",
    "into_iter",
    "next",
    "count",
    "sum",
    "any",
    "all",
    "find",
    "position",
    // Conversion
    "to_string",
    "to_owned",
    "to_vec",
    "clone",
    "as_ref",
    "as_mut",
    "as_str",
    "as_bytes",
    "as_slice",
    "from",
    "into",
    "try_into",
    "try_from",
    // Container ops
    "len",
    "is_empty",
    "push",
    "pop",
    "insert",
    "remove",
    "get",
    "contains",
    "new",
    "default",
    "with_capacity",
    "drain",
    "extend",
    // I/O macros / functions
    "println",
    "print",
    "eprintln",
    "eprint",
    "format",
    "write",
    "writeln",
    // Python builtins
    "len",
    "str",
    "int",
    "list",
    "dict",
    "tuple",
    "set",
    "type",
    "range",
    "enumerate",
    "zip",
    "sorted",
    "reversed",
    "isinstance",
];

/// Walk the store, resolve cross-module calls, and add `calls` edges.
///
/// Returns the number of new edges emitted.
pub fn resolve_cross_module_calls(store: &Store) -> usize {
    let functions = store.atoms_by_kind("function");
    if functions.is_empty() {
        return 0;
    }

    // Two indices: free fns by name (`parent == None`) and methods by
    // `(parent, name)`. Splitting them is what makes bare-name calls
    // (`obj.method()`) refuse to bind to lifted methods of unrelated
    // types — the central fix this module exists to enforce.
    let mut free_fns_by_name: HashMap<String, Vec<usize>> = HashMap::new();
    let mut methods_by_owner_name: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for (idx, atom) in functions.iter().enumerate() {
        match &atom.parent {
            None => free_fns_by_name
                .entry(atom.name.clone())
                .or_default()
                .push(idx),
            Some(owner) => methods_by_owner_name
                .entry((owner.clone(), atom.name.clone()))
                .or_default()
                .push(idx),
        }
    }

    // Snapshot existing calls edges to avoid duplicates.
    let mut existing: HashSet<(EntityId, EntityId)> = HashSet::new();
    for atom in &functions {
        for target_id in store.call_edges_from(&atom.id) {
            existing.insert((atom.id.clone(), target_id));
        }
    }

    let skip_set: HashSet<&str> = SKIP_CALLS.iter().copied().collect();
    let mut added = 0;
    // Track synthesised extern atoms so we never emit duplicates: many
    // callers will reference the same external symbol.
    let mut extern_atoms: HashSet<EntityId> = HashSet::new();

    for caller_idx in 0..functions.len() {
        let caller = &functions[caller_idx];
        if caller.calls.is_empty() {
            continue;
        }

        let caller_id = caller.id.clone();
        let caller_crate = crate_root(caller);
        let caller_lang = lang_of(&caller.file_path);

        for call_name in &caller.calls {
            let (path_prefix, fn_name) = split_call_name(call_name);
            if skip_set.contains(fn_name) {
                continue;
            }

            let pick = pick_target(
                fn_name,
                path_prefix,
                caller_idx,
                caller_crate.as_deref(),
                caller_lang,
                &functions,
                &free_fns_by_name,
                &methods_by_owner_name,
                &existing,
                &caller_id,
            );

            if let Some(target_idx) = pick.target {
                let target_id = functions[target_idx].id.clone();
                store.add_edge(Edge::new(
                    caller_id.clone(),
                    target_id.clone(),
                    EdgeKind::Calls,
                ));
                existing.insert((caller_id.clone(), target_id));
                added += 1;
            } else if !pick.suppress_extern
                && let Some(prefix) = path_prefix.filter(|p| is_external_qualifier(p))
            {
                added += emit_extern_call(
                    store,
                    prefix,
                    fn_name,
                    &caller_id,
                    &mut extern_atoms,
                    &mut existing,
                );
            }
        }
    }

    added
}

/// Result of [`pick_target`].
///
/// `target` is the resolved candidate index, if any. `suppress_extern`
/// is set when the qualifier turned out to refer to an in-graph type or
/// module that simply ended in ambiguity — in that case the caller must
/// not synthesise an `extern` atom (the symbol isn't external; we just
/// can't pick one of several local candidates).
struct Pick {
    target: Option<usize>,
    suppress_extern: bool,
}

impl Pick {
    fn resolved(idx: usize) -> Self {
        Self {
            target: Some(idx),
            suppress_extern: true,
        }
    }
    fn ambiguous() -> Self {
        Self {
            target: None,
            suppress_extern: true,
        }
    }
    fn unresolved() -> Self {
        Self {
            target: None,
            suppress_extern: false,
        }
    }
}

/// Pick the local-atom index that a call should resolve to, given the call
/// name's qualifier prefix and the available candidates.
#[allow(clippy::too_many_arguments)]
fn pick_target(
    fn_name: &str,
    path_prefix: Option<&str>,
    caller_idx: usize,
    caller_crate: Option<&str>,
    caller_lang: &str,
    functions: &[CodeAtom],
    free_fns_by_name: &HashMap<String, Vec<usize>>,
    methods_by_owner_name: &HashMap<(String, String), Vec<usize>>,
    existing: &HashSet<(EntityId, EntityId)>,
    caller_id: &EntityId,
) -> Pick {
    let filter_viable = |cands: &[usize]| -> Vec<usize> {
        cands
            .iter()
            .copied()
            .filter(|&idx| {
                // Cross-language filtering: a Python caller can never
                // resolve to a Rust target by bare-name match (or vice
                // versa). They share neither namespace nor linkage —
                // any same-name hit is a coincidence.
                idx != caller_idx
                    && lang_of(&functions[idx].file_path) == caller_lang
                    && !existing.contains(&(caller_id.clone(), functions[idx].id.clone()))
            })
            .collect()
    };
    // "Does this name already have a candidate inside the caller's own
    // file?" — when yes, the call is local by construction and the
    // resolver must not bind it to a same-named atom in another file.
    // Two distinct shapes hit this:
    //   1. The parser's intra-file linker has already linked
    //      caller → local_target (covers `find_atom` / `l2_sq` / …
    //      twins).
    //   2. The bare call is recursive (`fn build_recursive() { …
    //      build_recursive(); }`) — the intra-file linker filters out
    //      self-calls, so no `existing` edge would catch it; the
    //      same-file candidate suffices.
    let caller_file = functions[caller_idx].file_path.as_str();
    let same_file_candidate_exists = |cands: &[usize]| -> bool {
        cands
            .iter()
            .any(|&idx| functions[idx].file_path == caller_file)
    };

    // A qualifier whose final segment is an in-crate path root
    // (`Self`, `crate`, `self`, `super`) refers to code the parser's
    // intra-impl / intra-file pass already had a chance to link. It
    // must NOT fall through to bare-name resolution: a `Self::method`
    // whose target the intra-impl pass missed (e.g. inherent methods
    // declared in another file) is better left unresolved than ghost-
    // bound to an unrelated free fn of the same name. Multi-segment
    // paths like `crate::render::render` keep their own resolution
    // path below — only the bare in-crate root is rejected here.
    if let Some(last) = path_prefix.and_then(|q| q.rsplit("::").next())
        && matches!(last, "crate" | "self" | "super" | "Self")
    {
        return Pick::unresolved();
    }

    if let Some(last_seg) = path_prefix
        .and_then(|q| q.rsplit("::").next())
        .filter(|m| !m.is_empty())
    {
        // 1) Try the method index — the qualifier may be a type/class
        //    name (e.g. `DaemonHandle::send`).
        if let Some(cands) = methods_by_owner_name.get(&(last_seg.to_owned(), fn_name.to_owned())) {
            let viable = filter_viable(cands);
            if let Some(idx) = pick_unique_with_same_crate_pref(&viable, caller_crate, functions) {
                return Pick::resolved(idx);
            }
            if !viable.is_empty() {
                // Methods matched but ambiguous: type is in-graph, do not
                // silently pick one *and* do not invent an extern.
                return Pick::ambiguous();
            }
        }
        // 2) Fall back to free-fn-by-module-name (the legacy semantics
        //    for `module::foo` qualifiers). The intra-file linker
        //    skips qualified calls, so the `already_locally_resolved`
        //    guard we apply to bare calls below is not needed here —
        //    the qualifier itself disambiguates.
        if let Some(cands) = free_fns_by_name.get(fn_name) {
            let viable: Vec<usize> = filter_viable(cands)
                .into_iter()
                .filter(|&idx| file_module_name(&functions[idx].file_path) == last_seg)
                .collect();
            if let Some(idx) = pick_unique_with_same_crate_pref(&viable, caller_crate, functions) {
                return Pick::resolved(idx);
            }
            if !viable.is_empty() {
                // Module had candidates but ambiguous — same reasoning:
                // the qualifier is in-graph, suppress extern.
                return Pick::ambiguous();
            }
        }
        // No method nor module candidate: leave it for the extern path.
        return Pick::unresolved();
    }

    // Bare call (no qualifier). In a sound program, a bare cross-file
    // call requires a `use` import (Rust) or `from … import …` (Python)
    // — both of which the parser already rewrites into a qualified path
    // when present. So an *unrewritten* bare call here means: a local
    // binding (closure/parameter), a local fn already linked by the
    // intra-file pass, or a typo/dead reference. None of those should
    // ghost-bind to a same-named free fn in some unrelated crate, so we
    // restrict to same-crate matches only and drop the historical
    // unique-cross-crate fallback.
    let Some(cands) = free_fns_by_name.get(fn_name) else {
        return Pick::unresolved();
    };
    if same_file_candidate_exists(cands) {
        return Pick::unresolved();
    }
    let viable: Vec<usize> = filter_viable(cands)
        .into_iter()
        .filter(|&idx| crate_root(&functions[idx]).as_deref() == caller_crate)
        .collect();
    match pick_unique_with_same_crate_pref(&viable, caller_crate, functions) {
        Some(idx) => Pick::resolved(idx),
        None => Pick::unresolved(),
    }
}

/// Apply same-crate preference and return the unique resolution if any.
///
/// - Empty input → `None`.
/// - Exactly one same-crate candidate → that one (regardless of cross-
///   crate count).
/// - Zero same-crate candidates and exactly one cross-crate → that one.
/// - Otherwise (ambiguous) → `None`.
fn pick_unique_with_same_crate_pref(
    candidates: &[usize],
    caller_crate: Option<&str>,
    functions: &[CodeAtom],
) -> Option<usize> {
    if candidates.is_empty() {
        return None;
    }
    let same_crate: Vec<usize> = candidates
        .iter()
        .copied()
        .filter(|&idx| crate_root(&functions[idx]).as_deref() == caller_crate)
        .collect();
    if same_crate.len() == 1 {
        return Some(same_crate[0]);
    }
    if same_crate.is_empty() && candidates.len() == 1 {
        return Some(candidates[0]);
    }
    None
}

/// Emit a synthetic `extern` atom (deduped) plus a `Calls` edge from the
/// caller to it. Returns the number of new edges added (0 or 1).
fn emit_extern_call(
    store: &Store,
    prefix: &str,
    fn_name: &str,
    caller_id: &EntityId,
    extern_atoms: &mut HashSet<EntityId>,
    existing: &mut HashSet<(EntityId, EntityId)>,
) -> usize {
    let extern_id = EntityId::new(format!("extern:{prefix}::{fn_name}"));
    if extern_atoms.insert(extern_id.clone()) {
        store.add_atom(CodeAtom {
            id: extern_id.clone(),
            kind: EXTERN_KIND.to_owned(),
            name: fn_name.to_owned(),
            file_path: String::new(),
            line_start: 0,
            line_end: 0,
            doc: String::new(),
            signature: format!("{prefix}::{fn_name}"),
            content_hash: String::new(),
            calls: Vec::new(),
            method_calls: Vec::new(),
            parent: None,
        });
    }
    if existing.contains(&(caller_id.clone(), extern_id.clone())) {
        return 0;
    }
    store.add_edge(Edge::new(
        caller_id.clone(),
        extern_id.clone(),
        EdgeKind::Calls,
    ));
    existing.insert((caller_id.clone(), extern_id));
    1
}

/// Whether a call qualifier should be considered "external" (not a known
/// in-crate path root). The Rust language reserves `crate` / `self` /
/// `super` as path roots that always refer to the current crate; `Self`
/// refers to the enclosing impl's owner type (handled at parser level).
/// Anything else is presumed external.
fn is_external_qualifier(prefix: &str) -> bool {
    let head = prefix.split("::").next().unwrap_or(prefix);
    !matches!(head, "crate" | "self" | "super" | "Self") && !head.is_empty()
}

/// Walk the store, resolve `impl Trait for Type` blocks to their target
/// trait atoms, and add `Implements` edges.
///
/// An `impl` atom whose `name` has the form `Trait for Type` is a trait
/// impl; if a trait atom with name `Trait` (or matching the last `::`
/// segment of `Trait`'s path) exists in the store, an `Implements` edge
/// is added from the impl atom to that trait atom. Inherent impls
/// (`impl Type`) and impls whose trait isn't a local atom are skipped
/// (the latter usually means the trait comes from an external crate).
///
/// Same-crate preference applies on ambiguity, mirroring the call
/// resolver.
///
/// Returns the number of new edges emitted.
pub fn resolve_implements_edges(store: &Store) -> usize {
    let impls = store.atoms_by_kind("impl");
    if impls.is_empty() {
        return 0;
    }
    let traits = store.atoms_by_kind("trait");
    if traits.is_empty() {
        return 0;
    }

    let mut trait_by_name: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, atom) in traits.iter().enumerate() {
        trait_by_name
            .entry(atom.name.clone())
            .or_default()
            .push(idx);
    }

    let mut added = 0;
    for impl_atom in &impls {
        // `impl Trait for Type` produces name `"Trait for Type"`. Inherent
        // impls produce just `"Type"` and have no trait part.
        let Some((trait_part, _type_part)) = impl_atom.name.split_once(" for ") else {
            continue;
        };
        // Strip generics off the trait part if present (e.g. `Foo<T>` → `Foo`).
        let trait_no_generics = trait_part.split('<').next().unwrap_or(trait_part);
        // For `fmt::Debug` we match on the last `::` segment: `Debug`.
        let trait_short = trait_no_generics
            .rsplit("::")
            .next()
            .unwrap_or(trait_no_generics)
            .trim();
        if trait_short.is_empty() {
            continue;
        }

        let Some(candidates) = trait_by_name.get(trait_short) else {
            continue;
        };
        let impl_crate = crate_root(impl_atom);
        let same_crate: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|&idx| crate_root(&traits[idx]) == impl_crate)
            .collect();
        let target_idx = if same_crate.len() == 1 {
            Some(same_crate[0])
        } else if candidates.len() == 1 && same_crate.is_empty() {
            Some(candidates[0])
        } else {
            None
        };
        if let Some(idx) = target_idx {
            store.add_edge(Edge::new(
                impl_atom.id.clone(),
                traits[idx].id.clone(),
                EdgeKind::Implements,
            ));
            added += 1;
        }
    }

    added
}

/// Split a call name into `(qualifier_path, short_name)`.
///
/// `foo` → `(None, "foo")`.
/// `module::foo` → `(Some("module"), "foo")`.
/// `crate::render::render` → `(Some("crate::render"), "render")`.
fn split_call_name(name: &str) -> (Option<&str>, &str) {
    if let Some(idx) = name.rfind("::") {
        let (prefix, rest) = name.split_at(idx);
        (Some(prefix), &rest[2..])
    } else {
        (None, name)
    }
}

/// Source-language tag derived from a file's extension. Used to keep
/// the resolver from matching a Python caller's `foo()` against a Rust
/// `pub fn foo` (or vice versa) — they share neither namespace nor
/// linkage. Files with no extension return `""` and only match other
/// extension-less files.
fn lang_of(file_path: &str) -> &str {
    let basename = file_path.rsplit('/').next().unwrap_or(file_path);
    basename.rsplit_once('.').map_or("", |(_, ext)| ext)
}

/// Module name that a file represents:
///   `src/foo.rs`         → `"foo"`
///   `src/foo/mod.rs`     → `"foo"` (use parent dir, not the literal `"mod"`)
///   `crate_a/src/lib.rs` → `"lib"`
fn file_module_name(file_path: &str) -> &str {
    let basename = file_path.rsplit('/').next().unwrap_or(file_path);
    let stem = basename.rsplit_once('.').map_or(basename, |(s, _)| s);
    if stem == "mod" {
        // Walk up one level to find the directory name.
        let parent = file_path.rsplit_once('/').map_or("", |(p, _)| p);
        parent.rsplit('/').next().unwrap_or(parent)
    } else {
        stem
    }
}

/// Best-effort crate-root extraction from an atom's `file_path`.
///
/// - `crate_a/src/foo.rs` → `Some("crate_a")` (split at `/src/`).
/// - `src/foo.rs` → `Some("")` (top-level `src/` — treated as one
///   crate so a tempdir test layout works).
/// - `tests/foo.rs`, `mod.py` → `Some(file_path)` (each its own root —
///   prevents bare-name cross-file binds in loose layouts).
/// - empty path → `None`.
fn crate_root(atom: &CodeAtom) -> Option<String> {
    if atom.file_path.is_empty() {
        return None;
    }
    if let Some(idx) = atom.file_path.find("/src/") {
        return Some(atom.file_path[..idx].to_owned());
    }
    if atom.file_path.starts_with("src/") {
        return Some(String::new());
    }
    Some(atom.file_path.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Store;
    use crate::model::{CodeAtom, EntityId};

    fn function_atom(file_path: &str, name: &str, calls: &[&str]) -> CodeAtom {
        CodeAtom {
            id: EntityId::new(format!("code:{file_path}::function::{name}")),
            kind: "function".to_owned(),
            name: name.to_owned(),
            file_path: file_path.to_owned(),
            line_start: 1,
            line_end: 3,
            doc: String::new(),
            signature: String::new(),
            content_hash: "deadbeef".to_owned(),
            calls: calls.iter().map(|s| (*s).to_owned()).collect(),
            method_calls: Vec::new(),
            parent: None,
        }
    }

    fn module_atom(file_path: &str) -> CodeAtom {
        let stem = file_path
            .rsplit('/')
            .next()
            .unwrap_or(file_path)
            .strip_suffix(".rs")
            .unwrap_or(file_path);
        CodeAtom {
            id: EntityId::new(format!("code:{file_path}")),
            kind: "module".to_owned(),
            name: stem.to_owned(),
            file_path: file_path.to_owned(),
            line_start: 1,
            line_end: 1,
            doc: String::new(),
            signature: String::new(),
            content_hash: "h0".to_owned(),
            calls: Vec::new(),
            method_calls: Vec::new(),
            parent: None,
        }
    }

    fn build_store(file_path: &str, fns: &[(&str, &[&str])]) -> Store {
        let store = Store::new();
        store.add_atom(module_atom(file_path));
        for (name, calls) in fns {
            store.add_atom(function_atom(file_path, name, calls));
        }
        store
    }

    fn add_to_store(store: &Store, file_path: &str, fns: &[(&str, &[&str])]) {
        store.add_atom(module_atom(file_path));
        for (name, calls) in fns {
            store.add_atom(function_atom(file_path, name, calls));
        }
    }

    #[test]
    fn empty_store_yields_zero_edges() {
        let store = Store::new();
        let added = resolve_cross_module_calls(&store);
        assert_eq!(added, 0);
    }

    #[test]
    fn single_module_with_no_calls_yields_zero_edges() {
        let store = build_store("crate_a/src/lib.rs", &[("standalone", &[])]);
        assert_eq!(resolve_cross_module_calls(&store), 0);
    }

    #[test]
    fn unique_cross_module_call_resolves() {
        let store = Store::new();
        add_to_store(&store, "crate_a/src/mod_a.rs", &[("caller", &["helper"])]);
        add_to_store(&store, "crate_a/src/mod_b.rs", &[("helper", &[])]);

        let added = resolve_cross_module_calls(&store);
        assert_eq!(added, 1);

        let caller_id = EntityId::new("code:crate_a/src/mod_a.rs::function::caller");
        let helper_id = EntityId::new("code:crate_a/src/mod_b.rs::function::helper");
        assert!(store.has_call_edge(&caller_id, &helper_id));
    }

    #[test]
    fn ambiguous_same_crate_call_skipped() {
        let store = Store::new();
        add_to_store(&store, "crate_a/src/mod_a.rs", &[("caller", &["helper"])]);
        add_to_store(&store, "crate_a/src/mod_b.rs", &[("helper", &[])]);
        add_to_store(&store, "crate_a/src/mod_c.rs", &[("helper", &[])]);
        assert_eq!(resolve_cross_module_calls(&store), 0, "ambiguity must skip");
    }

    #[test]
    fn skip_calls_blocklist_filters_noise() {
        let store = Store::new();
        add_to_store(&store, "crate_a/src/mod_a.rs", &[("caller", &["unwrap"])]);
        add_to_store(&store, "crate_a/src/mod_b.rs", &[("unwrap", &[])]);
        assert_eq!(resolve_cross_module_calls(&store), 0);
    }

    #[test]
    fn does_not_duplicate_existing_call_edges() {
        let store = Store::new();
        add_to_store(&store, "crate_a/src/mod_a.rs", &[("caller", &["helper"])]);
        add_to_store(&store, "crate_a/src/mod_b.rs", &[("helper", &[])]);

        let first = resolve_cross_module_calls(&store);
        assert_eq!(first, 1);
        let second = resolve_cross_module_calls(&store);
        assert_eq!(second, 0);
    }

    #[test]
    fn unknown_call_name_silently_skipped() {
        let store = build_store("crate_a/src/mod_a.rs", &[("caller", &["nonexistent"])]);
        assert_eq!(resolve_cross_module_calls(&store), 0);
    }

    #[test]
    fn self_call_skipped() {
        let store = build_store("crate_a/src/mod_a.rs", &[("caller", &["caller"])]);
        assert_eq!(resolve_cross_module_calls(&store), 0);
    }

    #[test]
    fn bare_call_does_not_cross_crate_resolve() {
        // Previously this asserted that a bare-name call would heuristic-
        // ally fall through to the unique cross-crate candidate. That
        // heuristic was the source of every "closure-parameter shadows
        // a graph-level free fn" ghost (e.g. `score(c)` inside a
        // descend_* helper binding to `api::routes::graph_ops::score`).
        //
        // In real Rust/Python, a bare cross-file call requires a `use`
        // / `import` that the parser already rewrites into a qualified
        // path — so an unrewritten bare call here is, by construction,
        // local. The resolver now refuses to bind it cross-crate.
        let store = Store::new();
        add_to_store(&store, "crate_a/src/mod_a.rs", &[("caller", &["xtra"])]);
        add_to_store(&store, "crate_b/src/lib.rs", &[("xtra", &[])]);
        assert_eq!(resolve_cross_module_calls(&store), 0);
    }

    #[test]
    fn ambiguous_cross_crate_call_skipped() {
        let store = Store::new();
        add_to_store(&store, "crate_a/src/mod_a.rs", &[("caller", &["helper"])]);
        add_to_store(&store, "crate_b/src/lib.rs", &[("helper", &[])]);
        add_to_store(&store, "crate_c/src/lib.rs", &[("helper", &[])]);
        assert_eq!(resolve_cross_module_calls(&store), 0);
    }

    #[test]
    fn same_crate_preferred_over_cross_crate() {
        let store = Store::new();
        add_to_store(&store, "crate_a/src/mod_a.rs", &[("caller", &["helper"])]);
        add_to_store(&store, "crate_a/src/mod_b.rs", &[("helper", &[])]);
        add_to_store(&store, "crate_b/src/lib.rs", &[("helper", &[])]);
        let added = resolve_cross_module_calls(&store);
        assert_eq!(added, 1);

        let caller_id = EntityId::new("code:crate_a/src/mod_a.rs::function::caller");
        let calls = store.call_edges_from(&caller_id);
        assert_eq!(calls.len(), 1);
        assert!(calls[0].as_str().contains("crate_a/src/mod_b.rs"));
    }

    #[test]
    fn skip_calls_includes_common_noise() {
        let set: HashSet<&str> = SKIP_CALLS.iter().copied().collect();
        for noise in [
            "unwrap",
            "expect",
            "map",
            "filter",
            "collect",
            "clone",
            "to_string",
            "len",
            "new",
            "println",
            "format",
        ] {
            assert!(set.contains(noise), "missing from SKIP_CALLS: {noise}");
        }
    }

    #[test]
    fn crate_root_extracts_prefix_before_src() {
        let atom = function_atom("crate_a/src/mod.rs", "f", &[]);
        assert_eq!(crate_root(&atom), Some("crate_a".to_owned()));
    }

    #[test]
    fn crate_root_returns_none_without_file_path() {
        let mut atom = function_atom("crate_a/src/mod.rs", "f", &[]);
        atom.file_path = String::new();
        assert!(crate_root(&atom).is_none());
    }

    #[test]
    fn crate_root_returns_full_path_when_no_src() {
        let atom = function_atom("just/a/path.rs", "f", &[]);
        assert_eq!(crate_root(&atom), Some("just/a/path.rs".to_owned()));
    }

    #[test]
    fn split_call_name_handles_bare_and_qualified() {
        assert_eq!(split_call_name("foo"), (None, "foo"));
        assert_eq!(split_call_name("module::foo"), (Some("module"), "foo"));
        assert_eq!(
            split_call_name("crate::render::render"),
            (Some("crate::render"), "render")
        );
    }

    #[test]
    fn file_module_name_uses_parent_dir_for_mod_files() {
        assert_eq!(file_module_name("src/render/mod.rs"), "render");
        assert_eq!(file_module_name("src/foo.rs"), "foo");
        assert_eq!(file_module_name("crate_a/src/lib.rs"), "lib");
    }

    #[test]
    fn qualified_call_disambiguates_between_same_named_functions() {
        // Two `render` functions in different sibling modules. A call to
        // `project::render` must resolve to the project module, not the
        // overview module.
        let store = Store::new();
        add_to_store(
            &store,
            "crate_a/src/render/mod.rs",
            &[("dispatch", &["project::render", "overview::render"])],
        );
        add_to_store(&store, "crate_a/src/render/project.rs", &[("render", &[])]);
        add_to_store(&store, "crate_a/src/render/overview.rs", &[("render", &[])]);

        let added = resolve_cross_module_calls(&store);
        assert_eq!(added, 2, "both qualified calls should resolve");

        let dispatch = EntityId::new("code:crate_a/src/render/mod.rs::function::dispatch");
        let project = EntityId::new("code:crate_a/src/render/project.rs::function::render");
        let overview = EntityId::new("code:crate_a/src/render/overview.rs::function::render");
        assert!(store.has_call_edge(&dispatch, &project));
        assert!(store.has_call_edge(&dispatch, &overview));
    }

    #[test]
    fn qualified_call_via_use_import_finds_target_at_mod_dot_rs() {
        // `use crate::render::render;` followed by `render(...)` is captured
        // by the parser as `crate::render::render`. Resolver must match that
        // against `src/render/mod.rs` (where `mod` resolves to parent dir).
        let store = Store::new();
        add_to_store(
            &store,
            "crate_a/src/pipeline.rs",
            &[("analyze", &["crate::render::render"])],
        );
        add_to_store(&store, "crate_a/src/render/mod.rs", &[("render", &[])]);
        // Decoy: another `render` in a sibling file that should NOT match.
        add_to_store(&store, "crate_a/src/render/project.rs", &[("render", &[])]);

        let added = resolve_cross_module_calls(&store);
        assert_eq!(added, 1, "exactly one match expected");

        let analyze = EntityId::new("code:crate_a/src/pipeline.rs::function::analyze");
        let target = EntityId::new("code:crate_a/src/render/mod.rs::function::render");
        assert!(store.has_call_edge(&analyze, &target));
    }

    #[test]
    fn qualified_call_with_no_module_match_routes_to_extern() {
        // The qualifier is explicit. If no candidate's module matches, the
        // resolver must NOT silently bind to a same-named function in some
        // unrelated module. Instead, it emits a synthetic `extern` atom
        // representing the external symbol so the call is still visible.
        let store = Store::new();
        add_to_store(
            &store,
            "crate_a/src/mod_a.rs",
            &[("caller", &["nonexistent::helper"])],
        );
        add_to_store(&store, "crate_a/src/mod_b.rs", &[("helper", &[])]);
        let added = resolve_cross_module_calls(&store);
        // Exactly one new edge: caller → extern:nonexistent::helper.
        assert_eq!(added, 1);

        let caller_id = EntityId::new("code:crate_a/src/mod_a.rs::function::caller");
        let extern_id = EntityId::new("extern:nonexistent::helper");
        let local_helper = EntityId::new("code:crate_a/src/mod_b.rs::function::helper");
        assert!(store.has_call_edge(&caller_id, &extern_id));
        assert!(!store.has_call_edge(&caller_id, &local_helper));
        // The extern atom itself exists with kind = "extern".
        let atom = store.get_atom(&extern_id).expect("extern atom must exist");
        assert_eq!(atom.kind, "extern");
        assert_eq!(atom.name, "helper");
    }

    #[test]
    fn bare_call_resolution_unchanged() {
        // Sanity: prior unique-name behaviour still works.
        let store = Store::new();
        add_to_store(&store, "crate_a/src/mod_a.rs", &[("caller", &["helper"])]);
        add_to_store(&store, "crate_a/src/mod_b.rs", &[("helper", &[])]);
        assert_eq!(resolve_cross_module_calls(&store), 1);
    }

    fn impl_atom(file_path: &str, name: &str) -> CodeAtom {
        CodeAtom {
            id: EntityId::new(format!("code:{file_path}::impl::{name}")),
            kind: "impl".to_owned(),
            name: name.to_owned(),
            file_path: file_path.to_owned(),
            line_start: 1,
            line_end: 3,
            doc: String::new(),
            signature: String::new(),
            content_hash: "deadbeef".to_owned(),
            calls: Vec::new(),
            method_calls: Vec::new(),
            parent: None,
        }
    }

    fn trait_atom(file_path: &str, name: &str) -> CodeAtom {
        CodeAtom {
            id: EntityId::new(format!("code:{file_path}::trait::{name}")),
            kind: "trait".to_owned(),
            name: name.to_owned(),
            file_path: file_path.to_owned(),
            line_start: 1,
            line_end: 3,
            doc: String::new(),
            signature: String::new(),
            content_hash: "deadbeef".to_owned(),
            calls: Vec::new(),
            method_calls: Vec::new(),
            parent: None,
        }
    }

    #[test]
    fn implements_links_local_trait() {
        let store = Store::new();
        store.add_atom(trait_atom("crate_a/src/distance/mod.rs", "Distance"));
        store.add_atom(impl_atom(
            "crate_a/src/distance/cosine.rs",
            "Distance for Cosine",
        ));
        let added = resolve_implements_edges(&store);
        assert_eq!(added, 1);

        let impl_id =
            EntityId::new("code:crate_a/src/distance/cosine.rs::impl::Distance for Cosine");
        let trait_id = EntityId::new("code:crate_a/src/distance/mod.rs::trait::Distance");
        let edges = store.edges_from(&impl_id);
        assert!(
            edges
                .iter()
                .any(|e| e.to == trait_id && e.kind.as_str() == "implements")
        );
    }

    #[test]
    fn implements_skipped_for_inherent_impls() {
        let store = Store::new();
        store.add_atom(trait_atom("crate_a/src/distance/mod.rs", "Distance"));
        // `impl Foo` (inherent) — name has no " for " separator.
        store.add_atom(impl_atom("crate_a/src/foo.rs", "Foo"));
        assert_eq!(resolve_implements_edges(&store), 0);
    }

    #[test]
    fn implements_skipped_when_trait_is_external() {
        let store = Store::new();
        // No `Display` trait atom → trait is from std/external; skip.
        store.add_atom(impl_atom("crate_a/src/foo.rs", "Display for Foo"));
        assert_eq!(resolve_implements_edges(&store), 0);
    }

    #[test]
    fn implements_handles_path_qualified_trait() {
        // `impl fmt::Debug for Foo` — match on the last `::` segment.
        let store = Store::new();
        store.add_atom(trait_atom("crate_a/src/fmt.rs", "Debug"));
        store.add_atom(impl_atom("crate_a/src/foo.rs", "fmt::Debug for Foo"));
        assert_eq!(resolve_implements_edges(&store), 1);
    }

    #[test]
    fn implements_handles_generic_trait_name() {
        // `impl Iterator<Item = u32> for Foo` → trait portion has generics.
        let store = Store::new();
        store.add_atom(trait_atom("crate_a/src/iter.rs", "Iterator"));
        store.add_atom(impl_atom(
            "crate_a/src/foo.rs",
            "Iterator<Item = u32> for Foo",
        ));
        assert_eq!(resolve_implements_edges(&store), 1);
    }

    #[test]
    fn extern_atom_deduped_across_callers() {
        // Two callers reference the same external symbol. Exactly one
        // extern atom should be created, with two incoming edges.
        let store = Store::new();
        add_to_store(&store, "crate_a/src/a.rs", &[("alpha", &["divan::main"])]);
        add_to_store(&store, "crate_a/src/b.rs", &[("beta", &["divan::main"])]);
        let added = resolve_cross_module_calls(&store);
        assert_eq!(added, 2, "two caller→extern edges");

        let extern_id = EntityId::new("extern:divan::main");
        let extern_atoms: Vec<_> = store
            .all_atoms()
            .into_iter()
            .filter(|a| a.kind == "extern" && a.id == extern_id)
            .collect();
        assert_eq!(extern_atoms.len(), 1, "extern atom must be deduped");
        let callers = store.call_edges_to(&extern_id);
        assert_eq!(callers.len(), 2);
    }

    #[test]
    fn extern_skipped_for_in_crate_qualifiers() {
        // `crate::foo`, `self::foo`, `super::foo`, and `Self::foo` all
        // refer to in-crate code (or to the enclosing impl). They must
        // never produce extern atoms — being unresolved here just means
        // we couldn't find a target, not that they're external.
        let store = Store::new();
        add_to_store(
            &store,
            "crate_a/src/a.rs",
            &[(
                "caller",
                &[
                    "crate::missing::thing",
                    "Self::missing",
                    "super::missing",
                    "self::missing",
                ],
            )],
        );
        let _ = resolve_cross_module_calls(&store);
        let externs: Vec<_> = store.atoms_by_kind("extern");
        assert!(
            externs.is_empty(),
            "in-crate qualifiers must not extern: {externs:?}"
        );
    }

    #[test]
    fn extern_not_emitted_for_bare_calls() {
        // Bare unresolved name (no `::`) is most likely a stdlib method
        // already filtered by SKIP_CALLS, or a typo, or an ambiguity.
        // Don't conjure an extern atom from a bare name.
        let store = Store::new();
        add_to_store(
            &store,
            "crate_a/src/a.rs",
            &[("caller", &["completely_unknown"])],
        );
        let _ = resolve_cross_module_calls(&store);
        let externs: Vec<_> = store.atoms_by_kind("extern");
        assert!(externs.is_empty());
    }

    /// Helper: build a method atom (parent = Some(owner)). Methods get
    /// the same id scheme the parser uses for lifted impl/class methods:
    /// `code:{file}::function::{owner}::{name}`.
    fn method_atom(file_path: &str, owner: &str, name: &str, calls: &[&str]) -> CodeAtom {
        CodeAtom {
            id: EntityId::new(format!("code:{file_path}::function::{owner}::{name}")),
            kind: "function".to_owned(),
            name: name.to_owned(),
            file_path: file_path.to_owned(),
            line_start: 1,
            line_end: 3,
            doc: String::new(),
            signature: String::new(),
            content_hash: "deadbeef".to_owned(),
            calls: calls.iter().map(|s| (*s).to_owned()).collect(),
            method_calls: Vec::new(),
            parent: Some(owner.to_owned()),
        }
    }

    #[test]
    fn bare_call_does_not_bind_to_lifted_method_of_unrelated_type() {
        // The headline regression. A caller in `cli` writes
        // `client.post(url).send()` — the parser captures bare `send` from
        // the `field_expression`. Elsewhere, `DaemonHandle::send` is the
        // only `send` in the whole graph, lifted with parent=DaemonHandle.
        // The old resolver would silently bind the bare call to it via
        // unique-cross-crate fallback. The new resolver must skip:
        // bare-name calls only match free fns, never lifted methods.
        let store = Store::new();
        store.add_atom(module_atom("anatta-cli/src/foo.rs"));
        store.add_atom(function_atom("anatta-cli/src/foo.rs", "caller", &["send"]));
        store.add_atom(module_atom("sigil-engine/src/daemon.rs"));
        store.add_atom(method_atom(
            "sigil-engine/src/daemon.rs",
            "DaemonHandle",
            "send",
            &[],
        ));

        let added = resolve_cross_module_calls(&store);
        assert_eq!(
            added, 0,
            "bare 'send' must not ghost-bind to DaemonHandle::send"
        );

        let caller_id = EntityId::new("code:anatta-cli/src/foo.rs::function::caller");
        let target_id =
            EntityId::new("code:sigil-engine/src/daemon.rs::function::DaemonHandle::send");
        assert!(!store.has_call_edge(&caller_id, &target_id));
    }

    #[test]
    fn qualified_owner_method_call_resolves() {
        // `use crate::daemon::DaemonHandle;` then `DaemonHandle::send(h)`
        // is captured as scoped_identifier `DaemonHandle::send`. Must
        // resolve to the lifted method.
        let store = Store::new();
        store.add_atom(module_atom("crate_a/src/cli.rs"));
        store.add_atom(function_atom(
            "crate_a/src/cli.rs",
            "caller",
            &["DaemonHandle::send"],
        ));
        store.add_atom(module_atom("crate_a/src/daemon.rs"));
        store.add_atom(method_atom(
            "crate_a/src/daemon.rs",
            "DaemonHandle",
            "send",
            &[],
        ));

        let added = resolve_cross_module_calls(&store);
        assert_eq!(added, 1);

        let caller_id = EntityId::new("code:crate_a/src/cli.rs::function::caller");
        let target_id = EntityId::new("code:crate_a/src/daemon.rs::function::DaemonHandle::send");
        assert!(store.has_call_edge(&caller_id, &target_id));
    }

    #[test]
    fn qualified_owner_method_ambiguous_across_crates_skips() {
        // Two crates both define `DaemonStatus::tick` (same struct name,
        // independent impls). A bare-prefixed `DaemonStatus::tick` from a
        // *third* crate has no same-crate winner and two cross-crate
        // candidates: must skip rather than silently pick one. This
        // reproduces the user's `health.rs ↔ daemon.rs` ghost-edge case.
        let store = Store::new();
        store.add_atom(module_atom("crate_a/src/lib.rs"));
        store.add_atom(function_atom(
            "crate_a/src/lib.rs",
            "caller",
            &["DaemonStatus::tick"],
        ));
        store.add_atom(module_atom("crate_b/src/health.rs"));
        store.add_atom(method_atom(
            "crate_b/src/health.rs",
            "DaemonStatus",
            "tick",
            &[],
        ));
        store.add_atom(module_atom("crate_c/src/daemon.rs"));
        store.add_atom(method_atom(
            "crate_c/src/daemon.rs",
            "DaemonStatus",
            "tick",
            &[],
        ));

        let added = resolve_cross_module_calls(&store);
        assert_eq!(added, 0, "ambiguous cross-crate methods must skip");
    }

    #[test]
    fn qualified_owner_method_same_crate_preferred() {
        // Same-crate preference still applies for methods.
        let store = Store::new();
        store.add_atom(module_atom("crate_a/src/cli.rs"));
        store.add_atom(function_atom(
            "crate_a/src/cli.rs",
            "caller",
            &["DaemonHandle::send"],
        ));
        store.add_atom(module_atom("crate_a/src/daemon.rs"));
        store.add_atom(method_atom(
            "crate_a/src/daemon.rs",
            "DaemonHandle",
            "send",
            &[],
        ));
        store.add_atom(module_atom("crate_b/src/lib.rs"));
        store.add_atom(method_atom(
            "crate_b/src/lib.rs",
            "DaemonHandle",
            "send",
            &[],
        ));

        let added = resolve_cross_module_calls(&store);
        assert_eq!(added, 1);

        let caller_id = EntityId::new("code:crate_a/src/cli.rs::function::caller");
        let same_crate = EntityId::new("code:crate_a/src/daemon.rs::function::DaemonHandle::send");
        let cross_crate = EntityId::new("code:crate_b/src/lib.rs::function::DaemonHandle::send");
        assert!(store.has_call_edge(&caller_id, &same_crate));
        assert!(!store.has_call_edge(&caller_id, &cross_crate));
    }

    #[test]
    fn module_qualified_call_still_falls_back_to_free_fn_lookup() {
        // `module::foo` (qualifier is a module name, not a type name)
        // must still resolve via the legacy `file_module_name` match.
        // This is the exact pattern preserved by the methods-first /
        // module-second lookup chain.
        let store = Store::new();
        add_to_store(
            &store,
            "crate_a/src/render/mod.rs",
            &[("dispatch", &["project::render"])],
        );
        add_to_store(&store, "crate_a/src/render/project.rs", &[("render", &[])]);

        let added = resolve_cross_module_calls(&store);
        assert_eq!(added, 1);

        let dispatch = EntityId::new("code:crate_a/src/render/mod.rs::function::dispatch");
        let target = EntityId::new("code:crate_a/src/render/project.rs::function::render");
        assert!(store.has_call_edge(&dispatch, &target));
    }
}
