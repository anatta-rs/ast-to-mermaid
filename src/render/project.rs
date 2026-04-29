//! Project-level renderer — one node per crate with counts + cross-crate
//! call edges.
//!
//! Best-effort layout aware of the `crates/<X>/` workspace convention. For
//! flat repos (no `crates/` prefix), the leading path segment is used.

use crate::graph::Store;
use crate::model::EdgeKind;
use crate::render::util::{crate_name, escape_label, mermaid_id};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

const KINDS: &[&str] = &["module", "function", "struct", "trait", "impl", "enum"];

#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct CrateCounts {
    modules: usize,
    functions: usize,
    structs: usize,
}

impl CrateCounts {
    fn bump(&mut self, kind: &str) {
        match kind {
            "module" => self.modules += 1,
            "function" => self.functions += 1,
            "struct" => self.structs += 1,
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

/// Render the project-level Mermaid view of `store`.
///
/// One node per crate with `(modules, functions, structs)` counts; one edge
/// per cross-crate `calls` aggregation labelled with the call count.
#[must_use]
pub fn render(store: &Store) -> String {
    let atoms = store.atoms_by_kinds(KINDS);

    // Build crate-count map.
    let mut counts: BTreeMap<String, CrateCounts> = BTreeMap::new();
    for atom in &atoms {
        let c = crate_name(&atom.file_path).to_owned();
        if !c.is_empty() {
            counts.entry(c).or_default().bump(&atom.kind);
        }
    }

    // Build a file_path → crate_name map for the edge pass.
    let mut file_to_crate: HashMap<String, String> = HashMap::new();
    for atom in &atoms {
        let c = crate_name(&atom.file_path).to_owned();
        if !c.is_empty() {
            file_to_crate.insert(atom.file_path.clone(), c);
        }
    }

    // Cross-crate call edges.
    let mut edges: BTreeMap<(String, String), usize> = BTreeMap::new();
    let functions = store.atoms_by_kind("function");
    for caller in &functions {
        let Some(caller_crate) = file_to_crate.get(&caller.file_path) else {
            continue;
        };
        let calls_out = store.edges_from(&caller.id);
        for edge in calls_out {
            if edge.kind != EdgeKind::Calls {
                continue;
            }
            if let Some(callee) = store.get_atom(&edge.to)
                && let Some(callee_crate) = file_to_crate.get(&callee.file_path)
                && callee_crate != caller_crate
            {
                *edges
                    .entry((caller_crate.clone(), callee_crate.clone()))
                    .or_default() += 1;
            }
        }
    }

    // Render Mermaid.
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
        }
    }

    fn fn_atom(file_path: &str, name: &str) -> CodeAtom {
        CodeAtom {
            id: EntityId::new(format!("code:{file_path}::function::{name}")),
            kind: "function".to_owned(),
            name: name.to_owned(),
            file_path: file_path.to_owned(),
            line_start: 1,
            line_end: 10,
            doc: String::new(),
            signature: String::new(),
            content_hash: "h".to_owned(),
            calls: Vec::new(),
        }
    }

    fn struct_atom(file_path: &str, name: &str) -> CodeAtom {
        CodeAtom {
            id: EntityId::new(format!("code:{file_path}::struct::{name}")),
            kind: "struct".to_owned(),
            name: name.to_owned(),
            file_path: file_path.to_owned(),
            line_start: 1,
            line_end: 5,
            doc: String::new(),
            signature: String::new(),
            content_hash: "h".to_owned(),
            calls: Vec::new(),
        }
    }

    #[test]
    fn empty_store_renders_minimal_graph() {
        let store = Store::new();
        let out = render(&store);
        assert_eq!(out, "graph TD\n");
    }

    #[test]
    fn single_crate_counts_modules_functions_structs() {
        let store = Store::new();
        store.add_atom(module_atom("crates/foo/src/lib.rs", "lib"));
        store.add_atom(fn_atom("crates/foo/src/lib.rs", "f1"));
        store.add_atom(fn_atom("crates/foo/src/lib.rs", "f2"));
        store.add_atom(struct_atom("crates/foo/src/lib.rs", "S"));
        let out = render(&store);
        assert!(out.contains("foo — 1 mod, 2 fn, 1 struct"));
    }

    #[test]
    fn cross_crate_calls_emit_edges_with_counts() {
        let store = Store::new();
        store.add_atom(module_atom("crates/crate_a/src/lib.rs", "lib"));
        store.add_atom(module_atom("crates/crate_b/src/lib.rs", "lib"));
        let caller = fn_atom("crates/crate_a/src/lib.rs", "caller");
        let helper = fn_atom("crates/crate_b/src/lib.rs", "helper");
        let caller_id = caller.id.clone();
        let helper_id = helper.id.clone();
        store.add_atom(caller);
        store.add_atom(helper);
        store.add_edge(Edge::new(caller_id, helper_id, EdgeKind::Calls));

        let out = render(&store);
        assert!(out.contains("crate_a"));
        assert!(out.contains("crate_b"));
        assert!(
            out.contains("crate_a -->|\"1 calls\"| crate_b"),
            "expected cross-crate edge in {out}"
        );
    }

    #[test]
    fn intra_crate_calls_do_not_emit_edges() {
        let store = Store::new();
        store.add_atom(module_atom("crates/foo/src/lib.rs", "lib"));
        let a = fn_atom("crates/foo/src/lib.rs", "a");
        let b = fn_atom("crates/foo/src/lib.rs", "b");
        let aid = a.id.clone();
        let bid = b.id.clone();
        store.add_atom(a);
        store.add_atom(b);
        store.add_edge(Edge::new(aid, bid, EdgeKind::Calls));

        let out = render(&store);
        assert!(!out.contains("-->"));
    }

    #[test]
    fn multiple_calls_aggregate_to_single_edge_with_count() {
        let store = Store::new();
        store.add_atom(module_atom("crates/crate_a/src/lib.rs", "lib"));
        store.add_atom(module_atom("crates/crate_b/src/lib.rs", "lib"));
        let c1 = fn_atom("crates/crate_a/src/lib.rs", "caller_one");
        let c2 = fn_atom("crates/crate_a/src/lib.rs", "caller_two");
        let helper = fn_atom("crates/crate_b/src/lib.rs", "helper");
        let c1_id = c1.id.clone();
        let c2_id = c2.id.clone();
        let helper_id = helper.id.clone();
        store.add_atom(c1);
        store.add_atom(c2);
        store.add_atom(helper);
        store.add_edge(Edge::new(c1_id, helper_id.clone(), EdgeKind::Calls));
        store.add_edge(Edge::new(c2_id, helper_id, EdgeKind::Calls));

        let out = render(&store);
        assert!(out.contains("crate_a -->|\"2 calls\"| crate_b"));
    }

    #[test]
    fn non_calls_relations_ignored_in_edges() {
        let store = Store::new();
        store.add_atom(module_atom("crates/crate_a/src/lib.rs", "lib"));
        store.add_atom(module_atom("crates/crate_b/src/lib.rs", "lib"));
        let f = fn_atom("crates/crate_a/src/lib.rs", "f");
        let g = fn_atom("crates/crate_b/src/lib.rs", "g");
        let fid = f.id.clone();
        let gid = g.id.clone();
        store.add_atom(f);
        store.add_atom(g);
        store.add_edge(Edge::new(fid, gid, EdgeKind::Uses));

        let out = render(&store);
        assert!(!out.contains("-->"));
    }

    #[test]
    fn atom_without_file_path_skipped() {
        let store = Store::new();
        // Atom with empty file_path — should produce empty crate name → skipped.
        let a = CodeAtom {
            id: EntityId::new("code:ghost"),
            kind: "module".to_owned(),
            name: "ghost".to_owned(),
            file_path: String::new(),
            line_start: 1,
            line_end: 1,
            doc: String::new(),
            signature: String::new(),
            content_hash: "h".to_owned(),
            calls: Vec::new(),
        };
        store.add_atom(a);
        let out = render(&store);
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
}
