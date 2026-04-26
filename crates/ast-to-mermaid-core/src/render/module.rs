//! Module-zoom renderer — `level=module --target=<X>`.
//!
//! Shows all items inside a target module as a Mermaid `subgraph`, plus
//! external functions that call into the module (incoming) or are called
//! from inside (outgoing). Edge labels carry the function name on each
//! side.

use crate::error::{AstToMermaidError, Result};
use crate::render::lookup::resolve_module;
use crate::render::util::{escape_label, mermaid_id};
use ingester_core::{Atom, Relation};
use polystore::{Direction, EntityId, GraphStore};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;

/// Render the module-zoom view of `target` against `store`.
///
/// # Errors
///
/// - [`AstToMermaidError::InvalidInput`] when `target` doesn't resolve to a
///   unique module.
/// - Any storage-layer error during the traversal.
pub async fn render<S>(store: &S, target: &str) -> Result<String>
where
    S: GraphStore<Atom, Relation>,
{
    let module_id = resolve_module(store, target).await?;
    let module_atom = store
        .get_node(&module_id)
        .await?
        .ok_or_else(|| AstToMermaidError::InvalidInput(format!("module vanished: {module_id}")))?;
    let module_label = module_atom.name.clone();
    let module_path = module_atom
        .metadata
        .get("file_path")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_owned();

    // 1. Items inside the module via `contains` outgoing edges.
    let mut item_ids: BTreeMap<String, EntityId> = BTreeMap::new();
    let mut item_kinds: BTreeMap<String, String> = BTreeMap::new();
    let outs = store.neighbors(&module_id, Direction::Outgoing).await?;
    for (item_id, rel) in outs {
        if rel.kind != "contains" {
            continue;
        }
        if let Some(atom) = store.get_node(&item_id).await? {
            item_ids.insert(item_id.as_str().to_owned(), item_id.clone());
            item_kinds.insert(item_id.as_str().to_owned(), atom.kind);
        }
    }
    let inside_set: HashSet<String> = item_ids.values().map(|id| id.as_str().to_owned()).collect();

    // 2. Outgoing call edges (item → function in other module).
    let mut outgoing: BTreeMap<(String, String), Atom> = BTreeMap::new();
    for (key, item_id) in &item_ids {
        let kind = item_kinds.get(key).cloned().unwrap_or_default();
        if kind != "function" {
            continue;
        }
        let calls_out = store.neighbors(item_id, Direction::Outgoing).await?;
        for (target_id, rel) in calls_out {
            if rel.kind != "calls" {
                continue;
            }
            if inside_set.contains(target_id.as_str()) {
                continue; // intra-module call, not an external arrow
            }
            if let Some(atom) = store.get_node(&target_id).await? {
                outgoing.insert((key.clone(), target_id.as_str().to_owned()), atom);
            }
        }
    }

    // 3. Incoming call edges (external function → item in module).
    let mut incoming: BTreeMap<(String, String), Atom> = BTreeMap::new();
    for (key, item_id) in &item_ids {
        let kind = item_kinds.get(key).cloned().unwrap_or_default();
        if kind != "function" {
            continue;
        }
        let callers = store.neighbors(item_id, Direction::Incoming).await?;
        for (caller_id, rel) in callers {
            if rel.kind != "calls" {
                continue;
            }
            if inside_set.contains(caller_id.as_str()) {
                continue;
            }
            if let Some(atom) = store.get_node(&caller_id).await? {
                incoming.insert((caller_id.as_str().to_owned(), key.clone()), atom);
            }
        }
    }

    // 4. Render Mermaid.
    let subgraph_id = mermaid_id(&module_path);
    let mut mermaid = format!("graph TD\n    subgraph {subgraph_id}[\"");
    let header = escape_label(&format!("{module_label} ({module_path})"));
    mermaid.push_str(&header);
    mermaid.push_str("\"]\n");
    for (key, item_id) in &item_ids {
        let kind = item_kinds.get(key).cloned().unwrap_or_default();
        if let Some(atom) = store.get_node(item_id).await? {
            let id = mermaid_id(item_id.as_str());
            let label = escape_label(&format!("{} {}", short_kind(&kind), atom.name));
            let shape = node_shape(&kind, &id, &label);
            writeln!(mermaid, "        {shape}").expect("writing");
        }
    }
    mermaid.push_str("    end\n");

    // External nodes for outgoing/incoming.
    let mut external_seen: HashSet<String> = HashSet::new();
    for ((_, ext_id), ext_atom) in outgoing.iter().chain(incoming.iter()) {
        if !external_seen.insert(ext_id.clone()) {
            continue;
        }
        let id = mermaid_id(ext_id);
        let label = escape_label(&ext_atom.name);
        writeln!(mermaid, "    {id}([\"{label}\"])").expect("writing");
    }

    for (from_inside, to_outside) in outgoing.keys() {
        let from_id = mermaid_id(from_inside);
        let to_id = mermaid_id(to_outside);
        writeln!(mermaid, "    {from_id} --> {to_id}").expect("writing");
    }
    for (from_outside, to_inside) in incoming.keys() {
        let from_id = mermaid_id(from_outside);
        let to_id = mermaid_id(to_inside);
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
    // Visually distinguish kinds: rectangles for functions/unknown, rounded
    // for structs/enums, hex for traits, double-rounded for macros.
    match kind {
        "struct" | "enum" | "type_alias" => format!("{id}({label})"),
        "trait" => format!("{id}{{{{{label}}}}}"),
        "impl" => format!("{id}[/{label}/]"),
        "const" | "static" => format!("{id}[({label})]"),
        "macro" => format!("{id}[[{label}]]"),
        // function and any unknown kind use the default rectangle.
        _ => format!("{id}[\"{label}\"]"),
    }
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

    fn item(file_path: &str, kind: &str, name: &str) -> Atom {
        Atom::new(
            AtomId::new(format!("code:{file_path}::{kind}::{name}")),
            kind,
            name,
            "h",
        )
        .with_metadata(serde_json::json!({"file_path": file_path}))
    }

    fn build(file_path: &str, items: &[(&str, &str)]) -> ParseOutput {
        let mut o = ParseOutput::default();
        let m = module_atom(file_path, "mod");
        let mid = m.id.clone();
        o.atoms.push(m);
        for (kind, name) in items {
            let a = item(file_path, kind, name);
            o.relations
                .push(Relation::new(mid.clone(), a.id.clone(), "contains"));
            o.atoms.push(a);
        }
        o
    }

    #[tokio::test]
    async fn empty_target_errors() {
        let store = InMemoryStore::new(scope());
        let err = render(&store, "").await.expect_err("must error");
        assert!(matches!(err, AstToMermaidError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn missing_module_errors() {
        let store = InMemoryStore::new(scope());
        let err = render(&store, "ghost").await.expect_err("must error");
        assert!(err.to_string().contains("no module"));
    }

    #[tokio::test]
    async fn module_with_items_renders_subgraph() {
        let store = InMemoryStore::new(scope());
        let o = build(
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
        ingest_parse_output(&store, &o).await.expect("ingest");
        let out = render(&store, "src/foo.rs").await.expect("render");
        assert!(out.contains("subgraph"));
        assert!(out.contains("mod (src/foo.rs)"));
        // Function uses square shape
        assert!(out.contains("fn f1\"]"));
        // Struct uses paren shape
        assert!(out.contains("(struct S)"));
        // Trait uses {{...}}
        assert!(out.contains("{{trait T}}"));
        // Impl uses /.../
        assert!(out.contains("[/impl I/]"));
        // Const/static uses [(...)]
        assert!(out.contains("[(const C)]"));
        // Macro uses [[...]]
        assert!(out.contains("[[macro M]]"));
        // Type alias uses paren shape
        assert!(out.contains("(type TA)"));
    }

    #[tokio::test]
    async fn outgoing_and_incoming_calls_render_external_nodes() {
        let store = InMemoryStore::new(scope());
        let target = build("src/mod_a.rs", &[("function", "caller")]);
        let other = build("src/mod_b.rs", &[("function", "helper")]);
        let third = build("src/mod_c.rs", &[("function", "caller_outside")]);
        for o in &[target, other, third] {
            ingest_parse_output(&store, o).await.expect("ingest");
        }

        // caller (mod_a) → helper (mod_b)
        let caller = EntityId::new("code:src/mod_a.rs::function::caller");
        let helper = EntityId::new("code:src/mod_b.rs::function::helper");
        let outside = EntityId::new("code:src/mod_c.rs::function::caller_outside");
        store
            .add_edge(
                &caller,
                &helper,
                Relation::new(AtomId::new("a"), AtomId::new("b"), "calls"),
            )
            .await
            .expect("edge");
        // outside → caller (incoming)
        store
            .add_edge(
                &outside,
                &caller,
                Relation::new(AtomId::new("c"), AtomId::new("a"), "calls"),
            )
            .await
            .expect("edge");

        let out = render(&store, "src/mod_a.rs").await.expect("render");
        assert!(out.contains("subgraph"));
        // External rounded nodes (parentheses)
        assert!(out.contains("([\"helper\"])"));
        assert!(out.contains("([\"caller_outside\"])"));
        // Outgoing edge: caller → helper
        assert!(out.contains(" --> "));
    }

    #[tokio::test]
    async fn intra_module_calls_excluded_from_arrows() {
        let store = InMemoryStore::new(scope());
        let o = build("src/foo.rs", &[("function", "a"), ("function", "b")]);
        ingest_parse_output(&store, &o).await.expect("ingest");
        let aid = EntityId::new("code:src/foo.rs::function::a");
        let bid = EntityId::new("code:src/foo.rs::function::b");
        store
            .add_edge(
                &aid,
                &bid,
                Relation::new(AtomId::new("a"), AtomId::new("b"), "calls"),
            )
            .await
            .expect("edge");

        let out = render(&store, "src/foo.rs").await.expect("render");
        // Subgraph has both nodes, but no external --> arrow drawn
        // (intra-module edges aren't represented in this view).
        assert!(out.contains("fn a\"]"));
        assert!(out.contains("fn b\"]"));
        // No `-->` on any line outside the subgraph contents.
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
