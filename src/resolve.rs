//! Cross-module call resolver.
//!
//! `ingester-code` already emits intra-module `calls` edges (function A
//! calling function B in the SAME file). This module walks the populated
//! [`Store`] and resolves CROSS-module calls — function A in `mod_a.rs`
//! calling function B in `mod_b.rs`.
//!
//! # Algorithm
//!
//! 1. List every `function` atom in the store.
//! 2. Build a `name → atom_id` index across all of them.
//! 3. Load existing `calls` edges so we never emit a duplicate.
//! 4. For each function whose `calls` field is non-empty:
//!    - Skip stdlib / iterator method noise (see [`SKIP_CALLS`]).
//!    - Find candidates by name.
//!    - Prefer a unique candidate in the same crate (file-path prefix
//!      before `/src/`); fall back to a unique cross-crate candidate.
//!    - On ambiguity (multiple candidates) or zero matches: skip.
//!    - Emit a `calls` edge.
//!
//! Returns the number of new edges added.

use crate::graph::Store;
use crate::model::{CodeAtom, Edge, EdgeKind, EntityId};
use std::collections::{HashMap, HashSet};

/// Function names skipped during resolution because they are common
/// stdlib / iterator / Python builtin methods. Linking them produces
/// huge call-graph noise without illuminating actual cross-module
/// dependencies.
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

    // Build name → vec<index> index.
    let mut name_to_indices: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, atom) in functions.iter().enumerate() {
        name_to_indices
            .entry(atom.name.clone())
            .or_default()
            .push(idx);
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

    for caller_idx in 0..functions.len() {
        let caller = &functions[caller_idx];
        if caller.calls.is_empty() {
            continue;
        }

        let caller_id = caller.id.clone();
        let caller_crate = crate_root(caller);

        for call_name in &caller.calls {
            if skip_set.contains(call_name.as_str()) {
                continue;
            }

            let Some(candidates) = name_to_indices.get(call_name.as_str()) else {
                continue;
            };

            // Filter: different function, not already linked.
            let viable: Vec<usize> = candidates
                .iter()
                .copied()
                .filter(|&idx| {
                    idx != caller_idx
                        && !existing.contains(&(caller_id.clone(), functions[idx].id.clone()))
                })
                .collect();

            // Prefer same-crate.
            let same_crate: Vec<usize> = viable
                .iter()
                .copied()
                .filter(|&idx| crate_root(&functions[idx]) == caller_crate)
                .collect();

            let target_idx = if same_crate.len() == 1 {
                Some(same_crate[0])
            } else if viable.len() == 1 && same_crate.is_empty() {
                Some(viable[0])
            } else {
                None
            };

            if let Some(target_idx) = target_idx {
                let target_id = functions[target_idx].id.clone();
                let edge = Edge::new(caller_id.clone(), target_id.clone(), EdgeKind::Calls);
                store.add_edge(edge);
                existing.insert((caller_id.clone(), target_id));
                added += 1;
            }
        }
    }

    added
}

/// Best-effort crate-root extraction from an atom's `file_path`.
///
/// Splits at `/src/` and returns the prefix. `None` for atoms without a
/// `file_path` or paths without `/src/` (treated as their own root).
fn crate_root(atom: &CodeAtom) -> Option<String> {
    if atom.file_path.is_empty() {
        return None;
    }
    atom.file_path.split("/src/").next().map(str::to_owned)
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
    fn cross_crate_unique_candidate_resolves() {
        let store = Store::new();
        add_to_store(&store, "crate_a/src/mod_a.rs", &[("caller", &["xtra"])]);
        add_to_store(&store, "crate_b/src/lib.rs", &[("xtra", &[])]);
        assert_eq!(resolve_cross_module_calls(&store), 1);
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
}
