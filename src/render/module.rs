//! Module-zoom renderer — `level=module --target=<X>`.
//!
//! Shows all items inside a target module as a Mermaid `subgraph`, plus
//! external functions that call into the module (incoming) or are called
//! from inside (outgoing). Edge labels carry the function name on each
//! side.

use crate::error::{AstToMermaidError, Result};
use crate::graph::Store;
use crate::model::{EdgeKind, EntityId};
use crate::render::lookup::resolve_module;
use crate::render::util::{escape_label, mermaid_id};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;

/// Render the module-zoom view of `target` against `store`.
///
/// # Errors
///
/// - [`AstToMermaidError::InvalidInput`] when `target` doesn't resolve to a
///   unique module.
#[allow(clippy::too_many_lines)]
pub fn render(store: &Store, target: &str) -> Result<String> {
    let module_id = resolve_module(store, target)?;
    let module_atom = store
        .get_atom(&module_id)
        .ok_or_else(|| AstToMermaidError::InvalidInput(format!("module vanished: {module_id}")))?;
    let module_label = module_atom.name.clone();
    let module_path = module_atom.file_path.clone();

    // 1. Items inside the module via `Contains` edges.
    let child_ids = store.children_of(&module_id);
    let mut item_ids: BTreeMap<EntityId, String> = BTreeMap::new(); // id → kind
    for id in &child_ids {
        if let Some(atom) = store.get_atom(id) {
            item_ids.insert(id.clone(), atom.kind.clone());
        }
    }
    let inside_set: HashSet<EntityId> = item_ids.keys().cloned().collect();

    // 2. Walk outgoing + incoming neighbors of function items.
    let function_items: Vec<EntityId> = item_ids
        .iter()
        .filter(|(_, kind)| kind.as_str() == "function")
        .map(|(id, _)| id.clone())
        .collect();

    let mut outgoing: BTreeMap<(EntityId, EntityId), String> = BTreeMap::new(); // (inside, outside) → outside name
    let mut incoming: BTreeMap<(EntityId, EntityId), String> = BTreeMap::new(); // (outside, inside) → outside name

    for item_id in &function_items {
        for edge in store.edges_from(item_id) {
            if edge.kind == EdgeKind::Calls
                && !inside_set.contains(&edge.to)
                && let Some(ext) = store.get_atom(&edge.to)
            {
                outgoing
                    .entry((item_id.clone(), edge.to.clone()))
                    .or_insert_with(|| ext.name.clone());
            }
        }
        for edge in store.edges_to(item_id) {
            if edge.kind == EdgeKind::Calls
                && !inside_set.contains(&edge.from)
                && let Some(ext) = store.get_atom(&edge.from)
            {
                incoming
                    .entry((edge.from.clone(), item_id.clone()))
                    .or_insert_with(|| ext.name.clone());
            }
        }
    }

    // 3. Render Mermaid.
    let subgraph_id = mermaid_id(&module_path);
    let mut mermaid = format!("graph TD\n    subgraph {subgraph_id}[\"");
    let header = escape_label(&format!("{module_label} ({module_path})"));
    mermaid.push_str(&header);
    mermaid.push_str("\"]\n");

    // Sorted item list for deterministic output.
    let mut sorted_items: Vec<(&EntityId, &String)> = item_ids.iter().collect();
    sorted_items.sort_by_key(|(id, _)| id.as_str());

    for (item_id, kind) in &sorted_items {
        if let Some(atom) = store.get_atom(item_id) {
            let id = mermaid_id(item_id.as_str());
            let label = escape_label(&format!("{} {}", short_kind(kind), atom.name));
            let shape = node_shape(kind, &id, &label);
            writeln!(mermaid, "        {shape}").expect("writing");
        }
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
            let id = mermaid_id(ext_id.as_str());
            let label = escape_label(ext_name);
            writeln!(mermaid, "    {id}([\"{label}\"])").expect("writing");
        }
    }

    for (inside, outside) in outgoing.keys() {
        let from_id = mermaid_id(inside.as_str());
        let to_id = mermaid_id(outside.as_str());
        writeln!(mermaid, "    {from_id} --> {to_id}").expect("writing");
    }
    for (outside, inside) in incoming.keys() {
        let from_id = mermaid_id(outside.as_str());
        let to_id = mermaid_id(inside.as_str());
        writeln!(mermaid, "    {from_id} --> {to_id}").expect("writing");
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
        let err = render(&store, "").expect_err("must error");
        assert!(matches!(err, AstToMermaidError::InvalidInput(_)));
    }

    #[test]
    fn missing_module_errors() {
        let store = Store::new();
        let err = render(&store, "ghost").expect_err("must error");
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
        let out = render(&store, "src/foo.rs").expect("render");
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

        let out = render(&store, "src/mod_a.rs").expect("render");
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

        let out = render(&store, "src/foo.rs").expect("render");
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
