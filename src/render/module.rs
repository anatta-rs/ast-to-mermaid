//! Module-zoom renderer — `level=module --target=<X>`.
//!
//! Shows all items inside a target module as a Mermaid `subgraph`, plus
//! external functions that call into the module (incoming) or are called
//! from inside (outgoing). Edge labels carry the function name on each
//! side.

use crate::error::{AstToMermaidError, Result};
use crate::model::EntityId;
use crate::render::AdjMaps;
use crate::render::lookup::resolve_module;
use crate::render::snapshot::AtomSnapshot;
use crate::render::util::{escape_label_flowchart, sanitize_id};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;

/// Render the module-zoom view of `target` against `snapshot`.
///
/// `adj` supplies the shared forward `Contains` and forward + reverse
/// `Calls` adjacencies — the only graph data this view needs once
/// `resolve_module` has located the target.
///
/// `snapshot` is the borrowed `id → &CodeAtom` view: every per-child and
/// per-neighbor lookup is an O(1) `HashMap` probe with no `RwLock` traffic
/// and no [`crate::model::CodeAtom`] clones.
///
/// # Errors
///
/// - [`AstToMermaidError::InvalidInput`] when `target` doesn't resolve to a
///   unique module.
#[allow(clippy::too_many_lines)]
pub fn render(adj: &AdjMaps, snapshot: &AtomSnapshot<'_>, target: &str) -> Result<String> {
    let module_id = resolve_module(snapshot, target)?;
    let module_atom = snapshot
        .get(&module_id)
        .ok_or_else(|| AstToMermaidError::InvalidInput(format!("module vanished: {module_id}")))?;
    let module_label = module_atom.name.as_str();
    let module_path = module_atom.file_path.as_str();

    // 1. Items inside the module via the shared `Contains` adjacency. Two
    //    tiers:
    //    - top-level items directly contained by the module.
    //    - methods nested inside `impl` blocks (drawn as their own
    //      sub-subgraph).
    let mut top_items: BTreeMap<EntityId, &str> = BTreeMap::new(); // id → kind
    let mut impl_methods: BTreeMap<EntityId, BTreeMap<EntityId, &str>> = BTreeMap::new(); // impl_id → (method_id → kind)
    let mut inside_set: HashSet<EntityId> = HashSet::new();
    for child_arc in adj.children(&module_id) {
        let child_id: &EntityId = child_arc;
        let Some(atom) = snapshot.get(child_id) else {
            continue;
        };
        top_items.insert(child_id.clone(), atom.kind.as_str());
        inside_set.insert(child_id.clone());
        if atom.kind == "impl" {
            let mut method_map: BTreeMap<EntityId, &str> = BTreeMap::new();
            for method_arc in adj.children(child_id) {
                let method_id: &EntityId = method_arc;
                if let Some(matom) = snapshot.get(method_id) {
                    method_map.insert(method_id.clone(), matom.kind.as_str());
                    inside_set.insert(method_id.clone());
                }
            }
            if !method_map.is_empty() {
                impl_methods.insert(child_id.clone(), method_map);
            }
        }
    }

    // 2. Walk outgoing + incoming `Calls` neighbors of every function item —
    //    both top-level functions and impl methods.
    let mut function_items: Vec<EntityId> = top_items
        .iter()
        .filter(|(_, kind)| **kind == "function")
        .map(|(id, _)| id.clone())
        .collect();
    for methods in impl_methods.values() {
        for (mid, kind) in methods {
            if *kind == "function" {
                function_items.push(mid.clone());
            }
        }
    }

    let mut outgoing: BTreeMap<(EntityId, EntityId), String> = BTreeMap::new(); // (inside, outside) → outside name
    let mut incoming: BTreeMap<(EntityId, EntityId), String> = BTreeMap::new(); // (outside, inside) → outside name

    for item_id in &function_items {
        for callee_arc in adj.callees(item_id) {
            let callee_id: &EntityId = callee_arc;
            if !inside_set.contains(callee_id)
                && let Some(ext) = snapshot.get(callee_id)
            {
                outgoing
                    .entry((item_id.clone(), callee_id.clone()))
                    .or_insert_with(|| ext.name.clone());
            }
        }
        for caller_arc in adj.callers(item_id) {
            let caller_id: &EntityId = caller_arc;
            if !inside_set.contains(caller_id)
                && let Some(ext) = snapshot.get(caller_id)
            {
                incoming
                    .entry((caller_id.clone(), item_id.clone()))
                    .or_insert_with(|| ext.name.clone());
            }
        }
    }

    // 3. Render Mermaid.
    let subgraph_id = sanitize_id(module_path);
    let mut mermaid = format!("graph TD\n    subgraph {subgraph_id}[\"");
    let header = escape_label_flowchart(&format!("{module_label} ({module_path})"));
    mermaid.push_str(&header);
    mermaid.push_str("\"]\n");

    // Sorted item list for deterministic output.
    let mut sorted_items: Vec<(&EntityId, &&str)> = top_items.iter().collect();
    sorted_items.sort_by_key(|(id, _)| id.as_str());

    for (item_id, kind) in &sorted_items {
        let Some(atom) = snapshot.get(item_id) else {
            continue;
        };
        // For impl atoms with method children, emit a nested subgraph.
        if **kind == "impl"
            && let Some(methods) = impl_methods.get(item_id)
        {
            let impl_subgraph_id = sanitize_id(&format!("impl_{}", item_id.as_str()));
            let impl_label = escape_label_flowchart(&format!("impl {}", atom.name));
            writeln!(
                mermaid,
                "        subgraph {impl_subgraph_id}[\"{impl_label}\"]"
            )
            .expect("string write is infallible");
            let mut sorted_methods: Vec<(&EntityId, &&str)> = methods.iter().collect();
            sorted_methods.sort_by_key(|(id, _)| id.as_str());
            for (mid, mkind) in sorted_methods {
                if let Some(matom) = snapshot.get(mid) {
                    let id = sanitize_id(mid.as_str());
                    let label =
                        escape_label_flowchart(&format!("{} {}", short_kind(mkind), matom.name));
                    let shape = node_shape(mkind, &id, &label);
                    writeln!(mermaid, "            {shape}").expect("string write is infallible");
                }
            }
            writeln!(mermaid, "        end").expect("string write is infallible");
            continue;
        }
        let id = sanitize_id(item_id.as_str());
        let label = escape_label_flowchart(&format!("{} {}", short_kind(kind), atom.name));
        let shape = node_shape(kind, &id, &label);
        writeln!(mermaid, "        {shape}").expect("string write is infallible");
    }
    mermaid.push_str("    end\n");

    // External nodes for outgoing/incoming.
    let mut external_seen: HashSet<EntityId> = HashSet::new();
    let all_external: Vec<(&EntityId, &String)> = outgoing
        .iter()
        .map(|((_, ext), name)| (ext, name))
        .chain(incoming.iter().map(|((ext, _), name)| (ext, name)))
        .collect();
    for (ext_id, ext_name) in all_external {
        if external_seen.insert(ext_id.clone()) {
            let id = sanitize_id(ext_id.as_str());
            let label = escape_label_flowchart(ext_name);
            writeln!(mermaid, "    {id}([\"{label}\"])").expect("string write is infallible");
        }
    }

    for (inside, outside) in outgoing.keys() {
        let from_id = sanitize_id(inside.as_str());
        let to_id = sanitize_id(outside.as_str());
        writeln!(mermaid, "    {from_id} --> {to_id}").expect("string write is infallible");
    }
    for (outside, inside) in incoming.keys() {
        let from_id = sanitize_id(outside.as_str());
        let to_id = sanitize_id(inside.as_str());
        writeln!(mermaid, "    {from_id} --> {to_id}").expect("string write is infallible");
    }

    Ok(mermaid)
}

fn short_kind(kind: &str) -> &'static str {
    match kind {
        "function" => "fn",
        "struct" => "struct",
        "trait" => "trait",
        "impl" => "impl",
        "enum" => "enum",
        "type_alias" => "type",
        "const" => "const",
        "static" => "static",
        "macro" => "macro",
        _ => "?",
    }
}

fn node_shape(kind: &str, id: &str, label: &str) -> String {
    match kind {
        "struct" | "enum" | "type_alias" => format!("{id}({label})"),
        "trait" => format!("{id}{{{{{label}}}}}"),
        "impl" => format!("{id}[/{label}/]"),
        "const" | "static" => format!("{id}[({label})]"),
        "macro" => format!("{id}[[{label}]]"),
        _ => format!("{id}[\"{label}\"]"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Store;
    use crate::model::{CodeAtom, Edge, EdgeKind, EntityId};

    fn run(store: &Store, target: &str) -> Result<String> {
        let adj = AdjMaps::build(store);
        store.with_atoms(|atoms| {
            let snap = AtomSnapshot::build(atoms);
            render(&adj, &snap, target)
        })
    }

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

    fn build_module(store: &Store, file_path: &str, items: &[(&str, &str)]) {
        let m = module_atom(file_path, "mod");
        let mid = m.id.clone();
        store.add_atom(m);
        for (kind, name) in items {
            let a = item_atom(file_path, kind, name);
            store.add_edge(Edge::new(mid.clone(), a.id.clone(), EdgeKind::Contains));
            store.add_atom(a);
        }
    }

    #[test]
    fn empty_target_errors() {
        let store = Store::new();
        let err = run(&store, "").expect_err("must error");
        assert!(matches!(err, AstToMermaidError::InvalidInput(_)));
    }

    #[test]
    fn missing_module_errors() {
        let store = Store::new();
        let err = run(&store, "ghost").expect_err("must error");
        assert!(err.to_string().contains("no module"));
    }

    #[test]
    fn module_with_items_renders_subgraph() {
        let store = Store::new();
        build_module(
            &store,
            "src/foo.rs",
            &[
                ("function", "f1"),
                ("struct", "S"),
                ("trait", "T"),
                ("impl", "I"),
                ("enum", "E"),
                ("type_alias", "TA"),
                ("const", "C"),
                ("static", "ST"),
                ("macro", "M"),
            ],
        );
        let out = run(&store, "src/foo.rs").expect("render");
        assert!(out.contains("subgraph"));
        assert!(out.contains("mod (src/foo.rs)"));
        assert!(out.contains("fn f1\"]"));
        assert!(out.contains("(struct S)"));
        assert!(out.contains("{{trait T}}"));
        assert!(out.contains("[/impl I/]"));
        assert!(out.contains("[(const C)]"));
        assert!(out.contains("[[macro M]]"));
        assert!(out.contains("(type TA)"));
    }

    #[test]
    fn outgoing_and_incoming_calls_render_external_nodes() {
        let store = Store::new();
        build_module(&store, "src/mod_a.rs", &[("function", "caller")]);
        build_module(&store, "src/mod_b.rs", &[("function", "helper")]);
        build_module(&store, "src/mod_c.rs", &[("function", "caller_outside")]);

        let caller = EntityId::new("code:src/mod_a.rs::function::caller");
        let helper = EntityId::new("code:src/mod_b.rs::function::helper");
        let outside = EntityId::new("code:src/mod_c.rs::function::caller_outside");
        store.add_edge(Edge::new(caller.clone(), helper, EdgeKind::Calls));
        store.add_edge(Edge::new(outside, caller, EdgeKind::Calls));

        let out = run(&store, "src/mod_a.rs").expect("render");
        assert!(out.contains("subgraph"));
        assert!(out.contains("([\"helper\"])"));
        assert!(out.contains("([\"caller_outside\"])"));
        assert!(out.contains(" --> "));
    }

    #[test]
    fn intra_module_calls_excluded_from_arrows() {
        let store = Store::new();
        build_module(
            &store,
            "src/foo.rs",
            &[("function", "a"), ("function", "b")],
        );
        let aid = EntityId::new("code:src/foo.rs::function::a");
        let bid = EntityId::new("code:src/foo.rs::function::b");
        store.add_edge(Edge::new(aid, bid, EdgeKind::Calls));

        let out = run(&store, "src/foo.rs").expect("render");
        assert!(out.contains("fn a\"]"));
        assert!(out.contains("fn b\"]"));
        let arrows = out.matches("-->").count();
        assert_eq!(arrows, 0, "expected no arrows, got {arrows}\n{out}");
    }

    #[test]
    fn short_kind_covers_known() {
        for k in [
            "function",
            "struct",
            "trait",
            "impl",
            "enum",
            "type_alias",
            "const",
            "static",
            "macro",
        ] {
            assert!(!short_kind(k).is_empty());
        }
        assert_eq!(short_kind("unknown"), "?");
    }

    #[test]
    fn impl_methods_render_as_nested_subgraph() {
        // An impl with two methods becomes a nested subgraph inside the
        // module subgraph; each method shows as its own `fn` node.
        let store = Store::new();
        let m = module_atom("src/foo.rs", "foo");
        store.add_atom(m.clone());
        let impl_a = item_atom("src/foo.rs", "impl", "Foo");
        let m1 = CodeAtom {
            id: EntityId::new("code:src/foo.rs::function::Foo::build"),
            kind: "function".to_owned(),
            name: "build".to_owned(),
            file_path: "src/foo.rs".to_owned(),
            line_start: 1,
            line_end: 5,
            doc: String::new(),
            signature: String::new(),
            content_hash: "h".to_owned(),
            calls: Vec::new(),
            method_calls: Vec::new(),
            parent: None,
        };
        let m2 = CodeAtom {
            id: EntityId::new("code:src/foo.rs::function::Foo::update"),
            kind: "function".to_owned(),
            name: "update".to_owned(),
            file_path: "src/foo.rs".to_owned(),
            line_start: 6,
            line_end: 10,
            doc: String::new(),
            signature: String::new(),
            content_hash: "h".to_owned(),
            calls: Vec::new(),
            method_calls: Vec::new(),
            parent: None,
        };
        store.add_edge(Edge::new(
            m.id.clone(),
            impl_a.id.clone(),
            EdgeKind::Contains,
        ));
        store.add_edge(Edge::new(
            impl_a.id.clone(),
            m1.id.clone(),
            EdgeKind::Contains,
        ));
        store.add_edge(Edge::new(
            impl_a.id.clone(),
            m2.id.clone(),
            EdgeKind::Contains,
        ));
        store.add_atom(impl_a);
        store.add_atom(m1);
        store.add_atom(m2);

        let out = run(&store, "src/foo.rs").expect("render");
        // Outer module subgraph + nested impl subgraph = two `subgraph` lines.
        let nesting = out.matches("subgraph").count();
        assert!(
            nesting >= 2,
            "expected nested subgraph for impl, got {nesting}\n{out}"
        );
        assert!(out.contains("[\"impl Foo\"]"), "impl label missing: {out}");
        assert!(out.contains("fn build"), "method label missing: {out}");
        assert!(out.contains("fn update"), "method label missing: {out}");
    }

    #[test]
    fn impl_method_calls_to_outside_render_as_external_arrows() {
        // A method inside an impl that calls a function in another module
        // produces an outgoing arrow from the method node.
        let store = Store::new();
        // Outer module with an impl and a method.
        let m = module_atom("src/foo.rs", "foo");
        store.add_atom(m.clone());
        let impl_a = item_atom("src/foo.rs", "impl", "Foo");
        let method = CodeAtom {
            id: EntityId::new("code:src/foo.rs::function::Foo::build"),
            kind: "function".to_owned(),
            name: "build".to_owned(),
            file_path: "src/foo.rs".to_owned(),
            line_start: 1,
            line_end: 5,
            doc: String::new(),
            signature: String::new(),
            content_hash: "h".to_owned(),
            calls: Vec::new(),
            method_calls: Vec::new(),
            parent: None,
        };
        store.add_edge(Edge::new(
            m.id.clone(),
            impl_a.id.clone(),
            EdgeKind::Contains,
        ));
        store.add_edge(Edge::new(
            impl_a.id.clone(),
            method.id.clone(),
            EdgeKind::Contains,
        ));
        store.add_atom(impl_a);
        store.add_atom(method.clone());
        // Outside module + function being called.
        build_module(&store, "src/bar.rs", &[("function", "helper")]);
        let helper_id = EntityId::new("code:src/bar.rs::function::helper");
        store.add_edge(Edge::new(method.id, helper_id, EdgeKind::Calls));

        let out = run(&store, "src/foo.rs").expect("render");
        assert!(
            out.contains("([\"helper\"])"),
            "external `helper` node missing: {out}"
        );
        assert!(out.contains(" --> "), "expected an arrow: {out}");
    }

    #[test]
    fn node_shape_per_kind() {
        assert_eq!(node_shape("function", "id", "L"), "id[\"L\"]");
        assert_eq!(node_shape("struct", "id", "L"), "id(L)");
        assert_eq!(node_shape("enum", "id", "L"), "id(L)");
        assert_eq!(node_shape("type_alias", "id", "L"), "id(L)");
        assert_eq!(node_shape("trait", "id", "L"), "id{{L}}");
        assert_eq!(node_shape("impl", "id", "L"), "id[/L/]");
        assert_eq!(node_shape("const", "id", "L"), "id[(L)]");
        assert_eq!(node_shape("static", "id", "L"), "id[(L)]");
        assert_eq!(node_shape("macro", "id", "L"), "id[[L]]");
        assert_eq!(node_shape("unknown", "id", "L"), "id[\"L\"]");
    }
}
