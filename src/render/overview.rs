//! Module-level overview renderer — one node per module with item counts +
//! cross-module call edges.

use crate::graph::Store;
use crate::model::EntityId;
use crate::render::util::{escape_label, mermaid_id};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct ModuleMeta {
    functions: usize,
    structs: usize,
    traits: usize,
}

impl ModuleMeta {
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

/// Render the overview-level Mermaid view.
///
/// One node per module with `(functions, structs, traits)` counts; one edge
/// per cross-module `calls` aggregation.
#[must_use]
pub fn render(store: &Store) -> String {
    let modules = store.atoms_by_kind("module");

    // Build a module-path → name + counts map.
    let mut modules_map: BTreeMap<String, (String, ModuleMeta)> = BTreeMap::new();
    for m in &modules {
        if m.file_path.is_empty() {
            continue;
        }
        modules_map
            .entry(m.file_path.clone())
            .or_insert_with(|| (m.name.clone(), ModuleMeta::default()));
    }

    // Pre-bucket Contains edges by parent at function entry. Each parent's
    // children list is read once via the forward adjacency index (O(degree))
    // — even if the same module appears multiple times in the rendering
    // frame, the lookup below stays a HashMap hit.
    let mut contains_by_parent: HashMap<EntityId, Vec<EntityId>> =
        HashMap::with_capacity(modules.len());
    for m in &modules {
        if m.file_path.is_empty() {
            continue;
        }
        contains_by_parent
            .entry(m.id.clone())
            .or_insert_with(|| store.children_of(&m.id));
    }

    // Bump item counts from the pre-computed Contains buckets.
    for m in &modules {
        if m.file_path.is_empty() {
            continue;
        }
        let Some(children) = contains_by_parent.get(&m.id) else {
            continue;
        };
        let Some((_, counts)) = modules_map.get_mut(&m.file_path) else {
            continue;
        };
        for child_id in children {
            if let Some(child) = store.get_atom(child_id) {
                counts.bump(&child.kind);
            }
        }
    }

    // Build function file_path cache.
    let functions = store.atoms_by_kind("function");
    let mut fn_to_path: HashMap<EntityId, &str> = HashMap::with_capacity(functions.len());
    for f in &functions {
        fn_to_path.insert(f.id.clone(), f.file_path.as_str());
    }

    // Cross-module call edges via forward adjacency. `call_edges_from`
    // returns only the callee ids of `Calls` edges, avoiding per-edge
    // clones and the post-hoc kind filter.
    let mut edges: BTreeMap<(String, String), usize> = BTreeMap::new();
    for caller in &functions {
        let Some(&from_path) = fn_to_path.get(&caller.id) else {
            continue;
        };
        for callee_id in store.call_edges_from(&caller.id) {
            let Some(&to_path) = fn_to_path.get(&callee_id) else {
                continue;
            };
            if to_path != from_path {
                *edges
                    .entry((from_path.to_owned(), to_path.to_owned()))
                    .or_default() += 1;
            }
        }
    }

    // Render Mermaid.
    let mut mermaid = String::from("graph TD\n");
    for (path, (name, counts)) in &modules_map {
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
    mermaid
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Store;
    use crate::model::{CodeAtom, Edge, EdgeKind, EntityId};

    fn module_atom(file_path: &str, name: &str) -> CodeAtom {
        CodeAtom {
            id: EntityId::new(format!("code:{file_path}")),
            kind: "module".to_owned(),
            name: name.to_owned(),
            file_path: file_path.to_owned(),
            line_start: 1,
            line_end: 1,
            doc: String::new(),
            signature: String::new(),
            content_hash: "h".to_owned(),
            calls: Vec::new(),
            method_calls: Vec::new(),
            parent: None,
        }
    }

    fn item_atom(file_path: &str, kind: &str, name: &str) -> CodeAtom {
        CodeAtom {
            id: EntityId::new(format!("code:{file_path}::{kind}::{name}")),
            kind: kind.to_owned(),
            name: name.to_owned(),
            file_path: file_path.to_owned(),
            line_start: 1,
            line_end: 10,
            doc: String::new(),
            signature: String::new(),
            content_hash: "h".to_owned(),
            calls: Vec::new(),
            method_calls: Vec::new(),
            parent: None,
        }
    }

    fn build_module(store: &Store, file_path: &str, module_name: &str, items: &[(&str, &str)]) {
        let m = module_atom(file_path, module_name);
        let mid = m.id.clone();
        store.add_atom(m);
        for (kind, name) in items {
            let a = item_atom(file_path, kind, name);
            store.add_edge(Edge::new(mid.clone(), a.id.clone(), EdgeKind::Contains));
            store.add_atom(a);
        }
    }

    #[test]
    fn empty_store_yields_only_header() {
        let store = Store::new();
        let out = render(&store);
        assert_eq!(out, "graph TD\n");
    }

    #[test]
    fn single_module_with_no_items_shows_zero_count() {
        let store = Store::new();
        build_module(&store, "src/lib.rs", "lib", &[]);
        let out = render(&store);
        assert!(out.contains("lib — 0 fn"), "got: {out}");
    }

    #[test]
    fn item_counts_correct_per_module() {
        let store = Store::new();
        build_module(
            &store,
            "src/lib.rs",
            "lib",
            &[
                ("function", "f1"),
                ("function", "f2"),
                ("struct", "S"),
                ("trait", "T"),
            ],
        );
        let out = render(&store);
        assert!(out.contains("lib — 2 fn, 1 struct, 1 trait"), "got: {out}");
    }

    #[test]
    fn modules_with_only_functions_use_short_label() {
        let store = Store::new();
        build_module(
            &store,
            "src/lib.rs",
            "lib",
            &[("function", "a"), ("function", "b")],
        );
        let out = render(&store);
        assert!(out.contains("lib — 2 fn"));
        assert!(!out.contains("struct"));
        assert!(!out.contains("trait"));
    }

    #[test]
    fn cross_module_call_emits_edge() {
        let store = Store::new();
        build_module(&store, "src/mod_a.rs", "mod_a", &[("function", "caller")]);
        build_module(&store, "src/mod_b.rs", "mod_b", &[("function", "helper")]);

        let cid = EntityId::new("code:src/mod_a.rs::function::caller");
        let hid = EntityId::new("code:src/mod_b.rs::function::helper");
        store.add_edge(Edge::new(cid, hid, EdgeKind::Calls));

        let out = render(&store);
        let from_id = mermaid_id("src/mod_a.rs");
        let to_id = mermaid_id("src/mod_b.rs");
        assert!(
            out.contains(&format!("{from_id} -->|\"1\"| {to_id}")),
            "expected edge in {out}"
        );
    }

    #[test]
    fn intra_module_calls_excluded_from_edges() {
        let store = Store::new();
        build_module(
            &store,
            "src/mod_a.rs",
            "mod_a",
            &[("function", "a"), ("function", "b")],
        );
        let aid = EntityId::new("code:src/mod_a.rs::function::a");
        let bid = EntityId::new("code:src/mod_a.rs::function::b");
        store.add_edge(Edge::new(aid, bid, EdgeKind::Calls));

        let out = render(&store);
        assert!(!out.contains("-->"));
    }

    #[test]
    fn multiple_calls_aggregate_to_single_edge() {
        let store = Store::new();
        build_module(
            &store,
            "src/mod_a.rs",
            "mod_a",
            &[("function", "c1"), ("function", "c2")],
        );
        build_module(&store, "src/mod_b.rs", "mod_b", &[("function", "h")]);

        let hid = EntityId::new("code:src/mod_b.rs::function::h");
        for c in ["c1", "c2"] {
            let cid = EntityId::new(format!("code:src/mod_a.rs::function::{c}"));
            store.add_edge(Edge::new(cid, hid.clone(), EdgeKind::Calls));
        }

        let out = render(&store);
        let from_id = mermaid_id("src/mod_a.rs");
        let to_id = mermaid_id("src/mod_b.rs");
        assert!(out.contains(&format!("{from_id} -->|\"2\"| {to_id}")));
    }

    #[test]
    fn modules_keyed_by_file_path_avoid_collapse() {
        let store = Store::new();
        build_module(
            &store,
            "crates/foo/src/queries.rs",
            "queries",
            &[("function", "f1")],
        );
        build_module(
            &store,
            "crates/bar/src/queries.rs",
            "queries",
            &[("function", "f2")],
        );

        let out = render(&store);
        assert!(out.contains(&mermaid_id("crates/foo/src/queries.rs")));
        assert!(out.contains(&mermaid_id("crates/bar/src/queries.rs")));
    }

    #[test]
    fn module_without_file_path_skipped() {
        let store = Store::new();
        let a = CodeAtom {
            id: EntityId::new("code:no-path"),
            kind: "module".to_owned(),
            name: "ghost".to_owned(),
            file_path: String::new(),
            line_start: 1,
            line_end: 1,
            doc: String::new(),
            signature: String::new(),
            content_hash: "h".to_owned(),
            calls: Vec::new(),
            method_calls: Vec::new(),
            parent: None,
        };
        store.add_atom(a);
        let out = render(&store);
        assert_eq!(out, "graph TD\n");
    }

    #[test]
    fn module_meta_default_and_bumps() {
        let mut c = ModuleMeta::default();
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

        let bare = ModuleMeta::default();
        assert_eq!(bare.label("m"), "m — 0 fn");
    }
}
