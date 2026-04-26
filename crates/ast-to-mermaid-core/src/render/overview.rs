//! Module-level overview renderer — one node per module with item counts +
//! cross-module call edges.

use crate::render::util::{escape_label, mermaid_id};
use ingester_core::{Atom, Relation};
use polystore::{Direction, EntityId, GraphStore, Result};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct ModuleCounts {
    functions: usize,
    structs: usize,
    traits: usize,
}

impl ModuleCounts {
    fn bump(&mut self, kind: &str) {
        match kind {
            "function" => self.functions += 1,
            "struct" => self.structs += 1,
            "trait" => self.traits += 1,
            _ => {}
        }
    }

    fn label(&self, name: &str) -> String {
        if self.structs > 0 || self.traits > 0 {
            format!(
                "{name} — {} fn, {} struct, {} trait",
                self.functions, self.structs, self.traits
            )
        } else {
            format!("{name} — {} fn", self.functions)
        }
    }
}

fn file_path_of(atom: &Atom) -> Option<&str> {
    atom.metadata.get("file_path").and_then(|v| v.as_str())
}

/// Render the overview-level Mermaid view.
///
/// One node per module with `(functions, structs, traits)` counts; one edge
/// per cross-module `calls` aggregation.
///
/// # Errors
///
/// Propagates any error from the underlying store.
pub async fn render<S>(store: &S) -> Result<String>
where
    S: GraphStore<Atom, Relation>,
{
    // 1. Bulk-list modules + functions in one shot. The contains-edge pass
    //    needs the items themselves, so we fetch them via neighbors_bulk
    //    rather than re-querying by kind.
    let kind_groups = store.list_by_kinds(&["module", "function"]).await?;
    let mut module_ids: Vec<EntityId> = Vec::new();
    let mut function_ids: Vec<EntityId> = Vec::new();
    for (kind, ids) in &kind_groups {
        match kind.as_str() {
            "module" => module_ids = ids.clone(),
            "function" => function_ids = ids.clone(),
            _ => {}
        }
    }

    // 2. Bulk-fetch modules + their contained items (via neighbors_bulk on
    //    the module set). One round-trip each on a backend that overrides.
    let module_atoms = store.get_nodes_bulk(&module_ids).await?;
    let module_neighbors = store
        .neighbors_bulk(&module_ids, Direction::Outgoing)
        .await?;

    // Collect all item ids referenced by `contains` so we can fetch their
    // kinds in one more bulk get.
    let mut item_ids: Vec<EntityId> = Vec::new();
    for (_, outs) in &module_neighbors {
        for (item_id, rel) in outs {
            if rel.kind == "contains" {
                item_ids.push(item_id.clone());
            }
        }
    }
    let item_atoms = store.get_nodes_bulk(&item_ids).await?;
    let item_kind: HashMap<EntityId, String> = item_ids
        .iter()
        .zip(item_atoms)
        .filter_map(|(id, opt)| opt.map(|a| (id.clone(), a.kind)))
        .collect();

    // Build modules map keyed by file_path (collapses by file).
    let mut modules: BTreeMap<String, (String, ModuleCounts)> = BTreeMap::new();
    let mut id_to_path: HashMap<EntityId, String> = HashMap::new();
    for (mid, opt) in module_ids.iter().zip(module_atoms) {
        let Some(matom) = opt else { continue };
        let Some(path) = file_path_of(&matom).map(str::to_owned) else {
            continue;
        };
        id_to_path.insert(mid.clone(), path.clone());
        modules
            .entry(path)
            .or_insert_with(|| (matom.name.clone(), ModuleCounts::default()));
    }

    // 3. Bump counts using the cached item kinds.
    for (mid, outs) in &module_neighbors {
        let Some(path) = id_to_path.get(mid) else {
            continue;
        };
        let Some((_, counts)) = modules.get_mut(path) else {
            continue;
        };
        for (item_id, rel) in outs {
            if rel.kind != "contains" {
                continue;
            }
            if let Some(kind) = item_kind.get(item_id) {
                counts.bump(kind);
            }
        }
    }

    // 4. Cross-module call edges. Bulk-fetch function atoms + their outgoing
    //    edges to build a function_id → file_path cache and then aggregate.
    let function_atoms = store.get_nodes_bulk(&function_ids).await?;
    let fn_path_cache: HashMap<EntityId, String> = function_ids
        .iter()
        .zip(function_atoms)
        .filter_map(|(id, opt)| {
            opt.and_then(|a| file_path_of(&a).map(|p| (id.clone(), p.to_owned())))
        })
        .collect();
    let function_neighbors = store
        .neighbors_bulk(&function_ids, Direction::Outgoing)
        .await?;
    let mut edges: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (caller_id, outs) in function_neighbors {
        let Some(from_path) = fn_path_cache.get(&caller_id).cloned() else {
            continue;
        };
        for (target_id, rel) in outs {
            if rel.kind != "calls" {
                continue;
            }
            let Some(to_path) = fn_path_cache.get(&target_id).cloned() else {
                continue;
            };
            if to_path != from_path {
                *edges.entry((from_path.clone(), to_path)).or_default() += 1;
            }
        }
    }

    // 4. Render Mermaid.
    let mut mermaid = String::from("graph TD\n");
    for (path, (name, counts)) in &modules {
        let id = mermaid_id(path);
        let label = escape_label(&counts.label(name));
        writeln!(mermaid, "    {id}[\"{label}\"]").expect("writing to String");
    }
    for ((from, to), n) in &edges {
        let from_id = mermaid_id(from);
        let to_id = mermaid_id(to);
        if from_id != to_id {
            writeln!(mermaid, "    {from_id} -->|\"{n}\"| {to_id}").expect("writing to String");
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

    fn build_module(file_path: &str, module_name: &str, items: &[(&str, &str)]) -> ParseOutput {
        let mut o = ParseOutput::default();
        let m = Atom::new(
            AtomId::new(format!("code:{file_path}")),
            "module",
            module_name,
            "h",
        )
        .with_metadata(serde_json::json!({"file_path": file_path}));
        let mid = m.id.clone();
        o.atoms.push(m);
        for (kind, name) in items {
            let a = Atom::new(
                AtomId::new(format!("code:{file_path}::{kind}::{name}")),
                *kind,
                *name,
                "h",
            )
            .with_metadata(serde_json::json!({"file_path": file_path}));
            o.relations
                .push(Relation::new(mid.clone(), a.id.clone(), "contains"));
            o.atoms.push(a);
        }
        o
    }

    #[tokio::test]
    async fn empty_store_yields_only_header() {
        let store = InMemoryStore::new(scope());
        let out = render(&store).await.expect("render");
        assert_eq!(out, "graph TD\n");
    }

    #[tokio::test]
    async fn single_module_with_no_items_shows_zero_count() {
        let store = InMemoryStore::new(scope());
        let o = build_module("src/lib.rs", "lib", &[]);
        ingest_parse_output(&store, &o).await.expect("ingest");
        let out = render(&store).await.expect("render");
        assert!(out.contains("lib — 0 fn"), "got: {out}");
    }

    #[tokio::test]
    async fn item_counts_correct_per_module() {
        let store = InMemoryStore::new(scope());
        let o = build_module(
            "src/lib.rs",
            "lib",
            &[
                ("function", "f1"),
                ("function", "f2"),
                ("struct", "S"),
                ("trait", "T"),
            ],
        );
        ingest_parse_output(&store, &o).await.expect("ingest");
        let out = render(&store).await.expect("render");
        assert!(out.contains("lib — 2 fn, 1 struct, 1 trait"), "got: {out}");
    }

    #[tokio::test]
    async fn modules_with_only_functions_use_short_label() {
        let store = InMemoryStore::new(scope());
        let o = build_module("src/lib.rs", "lib", &[("function", "a"), ("function", "b")]);
        ingest_parse_output(&store, &o).await.expect("ingest");
        let out = render(&store).await.expect("render");
        assert!(out.contains("lib — 2 fn"));
        assert!(!out.contains("struct"));
        assert!(!out.contains("trait"));
    }

    #[tokio::test]
    async fn cross_module_call_emits_edge() {
        let store = InMemoryStore::new(scope());
        let a = build_module("src/mod_a.rs", "mod_a", &[("function", "caller")]);
        let b = build_module("src/mod_b.rs", "mod_b", &[("function", "helper")]);
        ingest_parse_output(&store, &a).await.expect("a");
        ingest_parse_output(&store, &b).await.expect("b");

        let cid = EntityId::new("code:src/mod_a.rs::function::caller");
        let hid = EntityId::new("code:src/mod_b.rs::function::helper");
        store
            .add_edge(
                &cid,
                &hid,
                Relation::new(
                    AtomId::new(cid.as_str()),
                    AtomId::new(hid.as_str()),
                    "calls",
                ),
            )
            .await
            .expect("edge");

        let out = render(&store).await.expect("render");
        let from_id = mermaid_id("src/mod_a.rs");
        let to_id = mermaid_id("src/mod_b.rs");
        assert!(
            out.contains(&format!("{from_id} -->|\"1\"| {to_id}")),
            "expected edge in {out}"
        );
    }

    #[tokio::test]
    async fn intra_module_calls_excluded_from_edges() {
        let store = InMemoryStore::new(scope());
        let a = build_module(
            "src/mod_a.rs",
            "mod_a",
            &[("function", "a"), ("function", "b")],
        );
        ingest_parse_output(&store, &a).await.expect("a");
        let aid = EntityId::new("code:src/mod_a.rs::function::a");
        let bid = EntityId::new("code:src/mod_a.rs::function::b");
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
        assert!(!out.contains("-->"));
    }

    #[tokio::test]
    async fn multiple_calls_aggregate_to_single_edge() {
        let store = InMemoryStore::new(scope());
        let a = build_module(
            "src/mod_a.rs",
            "mod_a",
            &[("function", "c1"), ("function", "c2")],
        );
        let b = build_module("src/mod_b.rs", "mod_b", &[("function", "h")]);
        ingest_parse_output(&store, &a).await.expect("a");
        ingest_parse_output(&store, &b).await.expect("b");

        let hid = EntityId::new("code:src/mod_b.rs::function::h");
        for c in ["c1", "c2"] {
            let cid = EntityId::new(format!("code:src/mod_a.rs::function::{c}"));
            store
                .add_edge(
                    &cid,
                    &hid,
                    Relation::new(
                        AtomId::new(cid.as_str()),
                        AtomId::new(hid.as_str()),
                        "calls",
                    ),
                )
                .await
                .expect("edge");
        }

        let out = render(&store).await.expect("render");
        let from_id = mermaid_id("src/mod_a.rs");
        let to_id = mermaid_id("src/mod_b.rs");
        assert!(out.contains(&format!("{from_id} -->|\"2\"| {to_id}")));
    }

    #[tokio::test]
    async fn modules_keyed_by_file_path_avoid_collapse() {
        // Two modules both named "queries" but in different files.
        let store = InMemoryStore::new(scope());
        let a = build_module(
            "crates/foo/src/queries.rs",
            "queries",
            &[("function", "f1")],
        );
        let b = build_module(
            "crates/bar/src/queries.rs",
            "queries",
            &[("function", "f2")],
        );
        ingest_parse_output(&store, &a).await.expect("a");
        ingest_parse_output(&store, &b).await.expect("b");

        let out = render(&store).await.expect("render");
        // Both should appear with distinct IDs (mermaid_id of the file_path).
        assert!(out.contains(&mermaid_id("crates/foo/src/queries.rs")));
        assert!(out.contains(&mermaid_id("crates/bar/src/queries.rs")));
    }

    #[tokio::test]
    async fn module_without_file_path_skipped() {
        let store = InMemoryStore::new(scope());
        let mut o = ParseOutput::default();
        let m = Atom::new(AtomId::new("code:no-path"), "module", "ghost", "h");
        // No file_path metadata at all.
        o.atoms.push(m);
        ingest_parse_output(&store, &o).await.expect("ingest");
        let out = render(&store).await.expect("render");
        assert_eq!(out, "graph TD\n");
    }

    #[test]
    fn module_counts_default_and_bumps() {
        let mut c = ModuleCounts::default();
        c.bump("function");
        c.bump("function");
        c.bump("struct");
        c.bump("trait");
        c.bump("module"); // ignored
        c.bump("enum"); // ignored
        assert_eq!(c.functions, 2);
        assert_eq!(c.structs, 1);
        assert_eq!(c.traits, 1);
        assert_eq!(c.label("m"), "m — 2 fn, 1 struct, 1 trait");

        let bare = ModuleCounts::default();
        assert_eq!(bare.label("m"), "m — 0 fn");
    }
}
