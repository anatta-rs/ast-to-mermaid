//! Cross-module call resolver.
//!
//! `ingester-code` already emits intra-module `calls` edges (function A
//! calling function B in the SAME file). This module walks the populated
//! [`polystore::GraphStore`] and resolves CROSS-module calls — function A
//! in `mod_a.rs` calling function B in `mod_b.rs`.
//!
//! # Algorithm
//!
//! 1. List every `function` atom in the store.
//! 2. Build a `name → atom_id` index across all of them.
//! 3. Load existing `calls` edges so we never emit a duplicate.
//! 4. For each function whose metadata holds an `calls` array (set by
//!    `ingester-code`), and for each entry in that array:
//!    - Skip stdlib / iterator method noise (see [`SKIP_CALLS`]).
//!    - Find candidates by name.
//!    - Prefer a unique candidate in the same crate (file-path prefix
//!      before `/src/`); fall back to a unique cross-crate candidate.
//!    - On ambiguity (multiple candidates) or zero matches: skip.
//!    - Emit a `calls` edge.
//!
//! Returns the number of new edges added.

use ingester_core::{Atom, AtomId, Relation};
use polystore::{Direction, EntityId, GraphStore, Result};
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
///
/// # Errors
///
/// Propagates the first error from the underlying store
/// (e.g. backend connectivity, missing nodes).
pub async fn resolve_cross_module_calls<S>(store: &S) -> Result<usize>
where
    S: GraphStore<Atom, Relation>,
{
    // 1. Snapshot every function atom.
    let function_ids = store.list_by_kind("function").await?;
    let mut functions: Vec<(EntityId, Atom)> = Vec::with_capacity(function_ids.len());
    for id in &function_ids {
        if let Some(atom) = store.get_node(id).await? {
            functions.push((id.clone(), atom));
        }
    }
    if functions.is_empty() {
        return Ok(0);
    }

    // 2. Build name index.
    let mut name_to_indices: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, (_, atom)) in functions.iter().enumerate() {
        name_to_indices
            .entry(atom.name.clone())
            .or_default()
            .push(idx);
    }

    // 3. Snapshot existing `calls` edges so we don't duplicate.
    let mut existing: HashSet<(EntityId, EntityId)> = HashSet::new();
    for (caller_id, _) in &functions {
        let outs = store.neighbors(caller_id, Direction::Outgoing).await?;
        for (target, rel) in outs {
            if rel.kind == "calls" {
                existing.insert((caller_id.clone(), target));
            }
        }
    }

    let skip_set: HashSet<&str> = SKIP_CALLS.iter().copied().collect();
    let mut added = 0;

    // 4. Resolve.
    for caller_idx in 0..functions.len() {
        let calls = call_names(&functions[caller_idx].1);
        if calls.is_empty() {
            continue;
        }

        let caller_id = functions[caller_idx].0.clone();
        let caller_crate = crate_root(&functions[caller_idx].1);

        for call_name in calls {
            if skip_set.contains(call_name.as_str()) {
                continue;
            }

            let Some(candidates) = name_to_indices.get(&call_name) else {
                continue;
            };

            // Filter: different function, not already linked.
            let viable: Vec<usize> = candidates
                .iter()
                .copied()
                .filter(|&idx| {
                    idx != caller_idx
                        && !existing.contains(&(caller_id.clone(), functions[idx].0.clone()))
                })
                .collect();

            // Prefer same-crate.
            let same_crate: Vec<usize> = viable
                .iter()
                .copied()
                .filter(|&idx| crate_root(&functions[idx].1) == caller_crate)
                .collect();

            let target_idx = if same_crate.len() == 1 {
                Some(same_crate[0])
            } else if viable.len() == 1 && same_crate.is_empty() {
                // No same-crate match but exactly one cross-crate candidate.
                Some(viable[0])
            } else {
                None // Ambiguous (multiple) or no match.
            };

            if let Some(target_idx) = target_idx {
                let target_id = functions[target_idx].0.clone();
                let edge = Relation::new(
                    AtomId::new(caller_id.as_str()),
                    AtomId::new(target_id.as_str()),
                    "calls",
                );
                store.add_edge(&caller_id, &target_id, edge).await?;
                existing.insert((caller_id.clone(), target_id));
                added += 1;
            }
        }
    }

    Ok(added)
}

/// Extract `calls: Vec<String>` from an atom's metadata, returning an
/// empty vec if absent or malformed.
fn call_names(atom: &Atom) -> Vec<String> {
    atom.metadata
        .get("calls")
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
        .unwrap_or_default()
}

/// Best-effort crate-root extraction from an atom's `file_path` metadata.
///
/// Splits at `/src/` and returns the prefix. `None` for atoms without a
/// `file_path` or paths without `/src/` (treated as their own root).
fn crate_root(atom: &Atom) -> Option<String> {
    let path = atom.metadata.get("file_path").and_then(|v| v.as_str())?;
    path.split("/src/").next().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{InMemoryStore, ingest_parse_output};
    use ingester_core::ParseOutput;
    use polystore::Scope;

    fn scope() -> Scope {
        Scope::new("ns", "repo", "branch")
    }

    fn function_atom(file_path: &str, name: &str, calls: &[&str]) -> Atom {
        let id = AtomId::new(format!("code:{file_path}::function::{name}"));
        let mut metadata = serde_json::json!({
            "file_path": file_path,
            "language": "rust",
            "line_start": 1,
            "line_end": 3,
        });
        if !calls.is_empty() {
            metadata["calls"] = serde_json::json!(calls);
        }
        Atom::new(id, "function", name, "deadbeef").with_metadata(metadata)
    }

    fn module_atom(file_path: &str) -> Atom {
        let id = AtomId::new(format!("code:{file_path}"));
        let metadata = serde_json::json!({
            "file_path": file_path,
            "language": "rust",
        });
        let stem = file_path
            .rsplit('/')
            .next()
            .unwrap_or(file_path)
            .strip_suffix(".rs")
            .unwrap_or(file_path);
        Atom::new(id, "module", stem, "h0").with_metadata(metadata)
    }

    /// Helper: build a `ParseOutput` with module + functions + module-CONTAINS edges.
    fn build_output(file_path: &str, fns: &[(&str, &[&str])]) -> ParseOutput {
        let mut output = ParseOutput::default();
        let module = module_atom(file_path);
        let module_id = module.id.clone();
        output.atoms.push(module);
        for (name, calls) in fns {
            let f = function_atom(file_path, name, calls);
            output
                .relations
                .push(Relation::new(module_id.clone(), f.id.clone(), "contains"));
            output.atoms.push(f);
        }
        output
    }

    #[tokio::test]
    async fn empty_store_yields_zero_edges() {
        let store = InMemoryStore::new(scope());
        let added = resolve_cross_module_calls(&store).await.expect("ok");
        assert_eq!(added, 0);
    }

    #[tokio::test]
    async fn single_module_with_no_calls_yields_zero_edges() {
        let store = InMemoryStore::new(scope());
        let out = build_output("crate_a/src/lib.rs", &[("standalone", &[])]);
        ingest_parse_output(&store, &out).await.expect("ingest");
        let added = resolve_cross_module_calls(&store).await.expect("ok");
        assert_eq!(added, 0);
    }

    #[tokio::test]
    async fn unique_cross_module_call_resolves() {
        let store = InMemoryStore::new(scope());
        // mod_a calls helper(); mod_b defines helper().
        let a = build_output("crate_a/src/mod_a.rs", &[("caller", &["helper"])]);
        let b = build_output("crate_a/src/mod_b.rs", &[("helper", &[])]);
        ingest_parse_output(&store, &a).await.expect("a");
        ingest_parse_output(&store, &b).await.expect("b");

        let added = resolve_cross_module_calls(&store).await.expect("ok");
        assert_eq!(added, 1);

        // Verify: caller → helper edge exists.
        let caller_id = EntityId::new("code:crate_a/src/mod_a.rs::function::caller");
        let helper_id = EntityId::new("code:crate_a/src/mod_b.rs::function::helper");
        let outs = store
            .neighbors(&caller_id, Direction::Outgoing)
            .await
            .expect("ok");
        let calls_out: Vec<&EntityId> = outs
            .iter()
            .filter(|(_, r)| r.kind == "calls")
            .map(|(t, _)| t)
            .collect();
        assert_eq!(calls_out, vec![&helper_id]);
    }

    #[tokio::test]
    async fn ambiguous_same_crate_call_skipped() {
        let store = InMemoryStore::new(scope());
        // helper() defined in TWO modules of the same crate — ambiguous.
        let a = build_output("crate_a/src/mod_a.rs", &[("caller", &["helper"])]);
        let b = build_output("crate_a/src/mod_b.rs", &[("helper", &[])]);
        let c = build_output("crate_a/src/mod_c.rs", &[("helper", &[])]);
        for o in &[a, b, c] {
            ingest_parse_output(&store, o).await.expect("ingest");
        }
        let added = resolve_cross_module_calls(&store).await.expect("ok");
        assert_eq!(added, 0, "ambiguity must skip");
    }

    #[tokio::test]
    async fn skip_calls_blocklist_filters_noise() {
        let store = InMemoryStore::new(scope());
        // caller calls `unwrap`, which is in SKIP_CALLS — must NOT link
        // even if a function named `unwrap` exists.
        let a = build_output("crate_a/src/mod_a.rs", &[("caller", &["unwrap"])]);
        let b = build_output("crate_a/src/mod_b.rs", &[("unwrap", &[])]);
        ingest_parse_output(&store, &a).await.expect("a");
        ingest_parse_output(&store, &b).await.expect("b");
        let added = resolve_cross_module_calls(&store).await.expect("ok");
        assert_eq!(added, 0);
    }

    #[tokio::test]
    async fn does_not_duplicate_existing_call_edges() {
        let store = InMemoryStore::new(scope());
        let a = build_output("crate_a/src/mod_a.rs", &[("caller", &["helper"])]);
        let b = build_output("crate_a/src/mod_b.rs", &[("helper", &[])]);
        ingest_parse_output(&store, &a).await.expect("a");
        ingest_parse_output(&store, &b).await.expect("b");

        // First pass: 1 edge.
        let first = resolve_cross_module_calls(&store).await.expect("ok");
        assert_eq!(first, 1);

        // Second pass: 0 (idempotent).
        let second = resolve_cross_module_calls(&store).await.expect("ok");
        assert_eq!(second, 0);
    }

    #[tokio::test]
    async fn unknown_call_name_silently_skipped() {
        let store = InMemoryStore::new(scope());
        // caller calls `nonexistent` which has no function atom.
        let a = build_output("crate_a/src/mod_a.rs", &[("caller", &["nonexistent"])]);
        ingest_parse_output(&store, &a).await.expect("a");
        let added = resolve_cross_module_calls(&store).await.expect("ok");
        assert_eq!(added, 0);
    }

    #[tokio::test]
    async fn self_call_skipped() {
        let store = InMemoryStore::new(scope());
        // recursive: caller in its own metadata.calls (e.g. recursion).
        let out = build_output("crate_a/src/mod_a.rs", &[("caller", &["caller"])]);
        ingest_parse_output(&store, &out).await.expect("ingest");
        let added = resolve_cross_module_calls(&store).await.expect("ok");
        assert_eq!(added, 0);
    }

    #[tokio::test]
    async fn cross_crate_unique_candidate_resolves() {
        let store = InMemoryStore::new(scope());
        // No same-crate match, but a unique cross-crate candidate.
        let a = build_output("crate_a/src/mod_a.rs", &[("caller", &["xtra"])]);
        let b = build_output("crate_b/src/lib.rs", &[("xtra", &[])]);
        ingest_parse_output(&store, &a).await.expect("a");
        ingest_parse_output(&store, &b).await.expect("b");
        let added = resolve_cross_module_calls(&store).await.expect("ok");
        assert_eq!(added, 1);
    }

    #[tokio::test]
    async fn ambiguous_cross_crate_call_skipped() {
        let store = InMemoryStore::new(scope());
        // 2 cross-crate candidates, no same-crate — must skip.
        let a = build_output("crate_a/src/mod_a.rs", &[("caller", &["helper"])]);
        let b = build_output("crate_b/src/lib.rs", &[("helper", &[])]);
        let c = build_output("crate_c/src/lib.rs", &[("helper", &[])]);
        for o in &[a, b, c] {
            ingest_parse_output(&store, o).await.expect("ingest");
        }
        let added = resolve_cross_module_calls(&store).await.expect("ok");
        assert_eq!(added, 0);
    }

    #[tokio::test]
    async fn same_crate_preferred_over_cross_crate() {
        let store = InMemoryStore::new(scope());
        // Same-crate target wins over cross-crate target with same name.
        let a = build_output("crate_a/src/mod_a.rs", &[("caller", &["helper"])]);
        let b = build_output("crate_a/src/mod_b.rs", &[("helper", &[])]);
        let c = build_output("crate_b/src/lib.rs", &[("helper", &[])]);
        for o in &[a, b, c] {
            ingest_parse_output(&store, o).await.expect("ingest");
        }
        let added = resolve_cross_module_calls(&store).await.expect("ok");
        assert_eq!(added, 1);

        let caller_id = EntityId::new("code:crate_a/src/mod_a.rs::function::caller");
        let outs = store
            .neighbors(&caller_id, Direction::Outgoing)
            .await
            .expect("ok");
        let calls_targets: Vec<&str> = outs
            .iter()
            .filter(|(_, r)| r.kind == "calls")
            .map(|(t, _)| t.as_str())
            .collect();
        // Must point to crate_a's helper, not crate_b's.
        assert_eq!(calls_targets.len(), 1);
        assert!(calls_targets[0].contains("crate_a/src/mod_b.rs"));
    }

    #[tokio::test]
    async fn malformed_calls_metadata_is_treated_as_empty() {
        let store = InMemoryStore::new(scope());
        // Atom whose metadata.calls is the wrong shape (string instead of array).
        let mut output = ParseOutput::default();
        let module = module_atom("crate_a/src/mod_a.rs");
        let module_id = module.id.clone();
        output.atoms.push(module);
        let mut bad = function_atom("crate_a/src/mod_a.rs", "caller", &[]);
        bad.metadata["calls"] = serde_json::json!("not_an_array");
        output
            .relations
            .push(Relation::new(module_id, bad.id.clone(), "contains"));
        output.atoms.push(bad);
        ingest_parse_output(&store, &output).await.expect("ingest");

        let added = resolve_cross_module_calls(&store).await.expect("ok");
        assert_eq!(added, 0);
    }

    #[tokio::test]
    async fn function_without_file_path_is_handled() {
        let store = InMemoryStore::new(scope());
        let mut module = module_atom("crate_a/src/mod_a.rs");
        module.metadata = serde_json::json!({});
        let module_id = module.id.clone();
        let mut output = ParseOutput::default();
        output.atoms.push(module);
        let mut f = function_atom("crate_a/src/mod_a.rs", "caller", &["helper"]);
        // Strip file_path metadata.
        f.metadata
            .as_object_mut()
            .expect("object")
            .remove("file_path");
        output
            .relations
            .push(Relation::new(module_id, f.id.clone(), "contains"));
        output.atoms.push(f);
        // Helper in another crate, fully tagged.
        let other = build_output("crate_b/src/lib.rs", &[("helper", &[])]);
        ingest_parse_output(&store, &output).await.expect("a");
        ingest_parse_output(&store, &other).await.expect("b");

        let added = resolve_cross_module_calls(&store).await.expect("ok");
        // Caller has no crate_root → falls into cross-crate branch with 1 candidate → resolves.
        assert_eq!(added, 1);
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
    fn call_names_returns_empty_when_no_metadata() {
        let atom = Atom::new(AtomId::new("code:x"), "function", "foo", "h");
        assert!(call_names(&atom).is_empty());
    }

    #[test]
    fn call_names_parses_string_array() {
        let atom = Atom::new(AtomId::new("code:x"), "function", "foo", "h")
            .with_metadata(serde_json::json!({"calls": ["a", "b"]}));
        assert_eq!(call_names(&atom), vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn crate_root_extracts_prefix_before_src() {
        let atom = function_atom("crate_a/src/mod.rs", "f", &[]);
        assert_eq!(crate_root(&atom), Some("crate_a".to_owned()));
    }

    #[test]
    fn crate_root_returns_none_without_file_path() {
        let mut atom = function_atom("crate_a/src/mod.rs", "f", &[]);
        atom.metadata = serde_json::json!({});
        assert!(crate_root(&atom).is_none());
    }

    #[test]
    fn crate_root_returns_full_path_when_no_src() {
        let atom = function_atom("just/a/path.rs", "f", &[]);
        assert_eq!(crate_root(&atom), Some("just/a/path.rs".to_owned()));
    }
}
