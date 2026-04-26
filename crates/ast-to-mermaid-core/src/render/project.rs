//! Project-level renderer — one node per crate with counts + cross-crate
//! call edges.
//!
//! Best-effort layout aware of the `crates/<X>/` workspace convention. For
//! flat repos (no `crates/` prefix), the leading path segment is used.

use crate::render::util::{crate_name, escape_label, mermaid_id};
use ingester_core::{Atom, Relation};
use polystore::{Direction, GraphStore, Result};
use std::collections::BTreeMap;
use std::fmt::Write as _;

#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct CrateCounts {
    modules: usize,
    functions: usize,
    structs: usize,
    traits: usize,
    impls: usize,
    enums: usize,
}

impl CrateCounts {
    fn bump(&mut self, kind: &str) {
        match kind {
            "module" => self.modules += 1,
            "function" => self.functions += 1,
            "struct" => self.structs += 1,
            "trait" => self.traits += 1,
            "impl" => self.impls += 1,
            "enum" => self.enums += 1,
            _ => {}
        }
    }

    fn label(&self, name: &str) -> String {
        format!(
            "{name} — {} mod, {} fn, {} struct",
            self.modules, self.functions, self.structs
        )
    }
}

fn crate_of_atom(atom: &Atom) -> Option<String> {
    atom.metadata
        .get("file_path")
        .and_then(|v| v.as_str())
        .map(|p| crate_name(p).to_owned())
}

/// Render the project-level Mermaid view of `store`.
///
/// One node per crate with `(modules, functions, structs)` counts; one edge
/// per cross-crate `calls` aggregation labelled with the call count.
///
/// # Errors
///
/// Propagates any error from the underlying store.
pub async fn render<S>(store: &S) -> Result<String>
where
    S: GraphStore<Atom, Relation>,
{
    // 1. Count entities per crate by walking each kind once.
    let mut counts: BTreeMap<String, CrateCounts> = BTreeMap::new();
    for kind in ["module", "function", "struct", "trait", "impl", "enum"] {
        let ids = store.list_by_kind(kind).await?;
        for id in &ids {
            if let Some(atom) = store.get_node(id).await?
                && let Some(c) = crate_of_atom(&atom)
            {
                counts.entry(c).or_default().bump(kind);
            }
        }
    }

    // 2. Aggregate cross-crate call edges (function → function across crates).
    let mut edges: BTreeMap<(String, String), usize> = BTreeMap::new();
    let function_ids = store.list_by_kind("function").await?;
    for caller_id in &function_ids {
        let Some(caller_atom) = store.get_node(caller_id).await? else {
            continue;
        };
        let Some(caller_crate) = crate_of_atom(&caller_atom) else {
            continue;
        };
        let outs = store.neighbors(caller_id, Direction::Outgoing).await?;
        for (target_id, rel) in outs {
            if rel.kind != "calls" {
                continue;
            }
            let Some(target_atom) = store.get_node(&target_id).await? else {
                continue;
            };
            let Some(target_crate) = crate_of_atom(&target_atom) else {
                continue;
            };
            if target_crate != caller_crate {
                *edges
                    .entry((caller_crate.clone(), target_crate))
                    .or_default() += 1;
            }
        }
    }

    // 3. Render Mermaid.
    let mut mermaid = String::from("graph TD\n");
    for (name, c) in &counts {
        let id = mermaid_id(name);
        let label = escape_label(&c.label(name));
        writeln!(mermaid, "    {id}[\"{label}\"]").expect("writing to String");
    }
    for ((from, to), n) in &edges {
        let from_id = mermaid_id(from);
        let to_id = mermaid_id(to);
        if from_id != to_id {
            writeln!(mermaid, "    {from_id} -->|\"{n} calls\"| {to_id}")
                .expect("writing to String");
        }
    }
    Ok(mermaid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{InMemoryStore, ingest_parse_output};
    use ingester_core::{AtomId, ParseOutput};
    use polystore::Scope;

    fn scope() -> Scope {
        Scope::new("ns", "repo", "branch")
    }

    fn module_atom(file_path: &str, name: &str) -> Atom {
        Atom::new(
            AtomId::new(format!("code:{file_path}")),
            "module",
            name,
            "h",
        )
        .with_metadata(serde_json::json!({"file_path": file_path}))
    }

    fn item_atom(file_path: &str, kind: &str, name: &str, calls: &[&str]) -> Atom {
        let id = AtomId::new(format!("code:{file_path}::{kind}::{name}"));
        let mut meta = serde_json::json!({"file_path": file_path});
        if !calls.is_empty() {
            meta["calls"] = serde_json::json!(calls);
        }
        Atom::new(id, kind, name, "h").with_metadata(meta)
    }

    fn build(file_path: &str, items: &[(&str, &str, &[&str])]) -> ParseOutput {
        let mut o = ParseOutput::default();
        let m = module_atom(
            file_path,
            file_path
                .rsplit('/')
                .next()
                .unwrap_or(file_path)
                .strip_suffix(".rs")
                .unwrap_or(file_path),
        );
        let mid = m.id.clone();
        o.atoms.push(m);
        for (kind, name, calls) in items {
            let a = item_atom(file_path, kind, name, calls);
            o.relations
                .push(Relation::new(mid.clone(), a.id.clone(), "contains"));
            o.atoms.push(a);
        }
        o
    }

    #[tokio::test]
    async fn empty_store_renders_minimal_graph() {
        let store = InMemoryStore::new(scope());
        let out = render(&store).await.expect("ok");
        assert_eq!(out, "graph TD\n");
    }

    #[tokio::test]
    async fn single_crate_counts_modules_functions_structs() {
        let store = InMemoryStore::new(scope());
        let a = build(
            "crates/foo/src/lib.rs",
            &[
                ("function", "f1", &[]),
                ("function", "f2", &[]),
                ("struct", "S", &[]),
            ],
        );
        ingest_parse_output(&store, &a).await.expect("ingest");
        let out = render(&store).await.expect("render");
        assert!(out.contains("foo — 1 mod, 2 fn, 1 struct"));
    }

    #[tokio::test]
    async fn cross_crate_calls_emit_edges_with_counts() {
        let store = InMemoryStore::new(scope());
        // crate_a fn caller calls helper (in crate_b)
        let a = build("crates/crate_a/src/lib.rs", &[("function", "caller", &[])]);
        let b = build("crates/crate_b/src/lib.rs", &[("function", "helper", &[])]);
        ingest_parse_output(&store, &a).await.expect("a");
        ingest_parse_output(&store, &b).await.expect("b");

        // Manually add a cross-crate calls edge.
        let caller_id =
            polystore::EntityId::new("code:crates/crate_a/src/lib.rs::function::caller");
        let helper_id =
            polystore::EntityId::new("code:crates/crate_b/src/lib.rs::function::helper");
        store
            .add_edge(
                &caller_id,
                &helper_id,
                Relation::new(
                    AtomId::new(caller_id.as_str()),
                    AtomId::new(helper_id.as_str()),
                    "calls",
                ),
            )
            .await
            .expect("edge");

        let out = render(&store).await.expect("render");
        assert!(out.contains("crate_a"));
        assert!(out.contains("crate_b"));
        assert!(
            out.contains("crate_a -->|\"1 calls\"| crate_b"),
            "expected cross-crate edge in {out}"
        );
    }

    #[tokio::test]
    async fn intra_crate_calls_do_not_emit_edges() {
        let store = InMemoryStore::new(scope());
        let a = build(
            "crates/foo/src/lib.rs",
            &[("function", "a", &[]), ("function", "b", &[])],
        );
        ingest_parse_output(&store, &a).await.expect("a");
        let aid = polystore::EntityId::new("code:crates/foo/src/lib.rs::function::a");
        let bid = polystore::EntityId::new("code:crates/foo/src/lib.rs::function::b");
        store
            .add_edge(
                &aid,
                &bid,
                Relation::new(
                    AtomId::new(aid.as_str()),
                    AtomId::new(bid.as_str()),
                    "calls",
                ),
            )
            .await
            .expect("edge");

        let out = render(&store).await.expect("render");
        // No `--> ` cross-crate edge should appear (intra-crate filtered out).
        assert!(!out.contains("-->"));
    }

    #[tokio::test]
    async fn multiple_calls_aggregate_to_single_edge_with_count() {
        let store = InMemoryStore::new(scope());
        let a = build(
            "crates/crate_a/src/lib.rs",
            &[
                ("function", "caller_one", &[]),
                ("function", "caller_two", &[]),
            ],
        );
        let b = build("crates/crate_b/src/lib.rs", &[("function", "helper", &[])]);
        ingest_parse_output(&store, &a).await.expect("a");
        ingest_parse_output(&store, &b).await.expect("b");

        let helper_id =
            polystore::EntityId::new("code:crates/crate_b/src/lib.rs::function::helper");
        for caller in ["caller_one", "caller_two"] {
            let cid = polystore::EntityId::new(format!(
                "code:crates/crate_a/src/lib.rs::function::{caller}"
            ));
            store
                .add_edge(
                    &cid,
                    &helper_id,
                    Relation::new(
                        AtomId::new(cid.as_str()),
                        AtomId::new(helper_id.as_str()),
                        "calls",
                    ),
                )
                .await
                .expect("edge");
        }

        let out = render(&store).await.expect("render");
        assert!(out.contains("crate_a -->|\"2 calls\"| crate_b"));
    }

    #[tokio::test]
    async fn non_calls_relations_ignored_in_edges() {
        let store = InMemoryStore::new(scope());
        let a = build("crates/crate_a/src/lib.rs", &[("function", "f", &[])]);
        let b = build("crates/crate_b/src/lib.rs", &[("function", "g", &[])]);
        ingest_parse_output(&store, &a).await.expect("a");
        ingest_parse_output(&store, &b).await.expect("b");

        // RELATES_TO with role uses — must NOT count as a call edge.
        let fid = polystore::EntityId::new("code:crates/crate_a/src/lib.rs::function::f");
        let gid = polystore::EntityId::new("code:crates/crate_b/src/lib.rs::function::g");
        store
            .add_edge(
                &fid,
                &gid,
                Relation::new(AtomId::new(fid.as_str()), AtomId::new(gid.as_str()), "uses"),
            )
            .await
            .expect("edge");

        let out = render(&store).await.expect("render");
        assert!(!out.contains("-->"));
    }

    #[tokio::test]
    async fn atom_without_file_path_skipped() {
        let store = InMemoryStore::new(scope());
        let mut o = ParseOutput::default();
        let mut m = module_atom("crates/foo/src/lib.rs", "lib");
        m.metadata = serde_json::json!({});
        o.atoms.push(m);
        ingest_parse_output(&store, &o).await.expect("ingest");
        let out = render(&store).await.expect("render");
        assert_eq!(out, "graph TD\n");
    }

    #[test]
    fn crate_counts_default_and_bump() {
        let mut c = CrateCounts::default();
        assert_eq!(c.functions, 0);
        c.bump("function");
        c.bump("function");
        c.bump("struct");
        c.bump("module");
        c.bump("unknown_kind"); // ignored
        assert_eq!(c.modules, 1);
        assert_eq!(c.functions, 2);
        assert_eq!(c.structs, 1);
        assert_eq!(c.label("foo"), "foo — 1 mod, 2 fn, 1 struct");
    }

    #[test]
    fn crate_counts_bumps_all_kinds() {
        let mut c = CrateCounts::default();
        for k in ["module", "function", "struct", "trait", "impl", "enum"] {
            c.bump(k);
        }
        assert_eq!(c.modules, 1);
        assert_eq!(c.functions, 1);
        assert_eq!(c.structs, 1);
        assert_eq!(c.traits, 1);
        assert_eq!(c.impls, 1);
        assert_eq!(c.enums, 1);
    }
}
