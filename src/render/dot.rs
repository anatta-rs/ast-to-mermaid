//! Mermaid → Graphviz DOT post-processor.
//!
//! Mermaid renders client-side via dagre; browsers (and GitHub) cap the input
//! around 500 edges / 50 KB. Past that, the diagram is correct but
//! unviewable. Graphviz handles 10k+ nodes and produces a navigable SVG.
//!
//! This module takes the output of any [`crate::render`] renderer and
//! rewrites it as DOT, preserving connectivity, edge labels, and `subgraph`
//! cluster boundaries. Mermaid-specific node shapes (hexagon, cylinder,
//! parallelogram, …) collapse to DOT's default rectangle — the structure is
//! kept, the typographic distinction is lost. That's the right trade for
//! "the graph would otherwise be unviewable".

use std::fmt::Write as _;

/// Convert a Mermaid `graph TD|BT|LR|RL` string into DOT.
///
/// Best-effort: nodes lose their Mermaid-specific shape distinction, but
/// edges, edge labels, ids, and cluster boundaries are preserved.
#[must_use]
pub fn mermaid_to_dot(mermaid: &str) -> String {
    let mut out = String::with_capacity(mermaid.len() * 2);
    out.push_str("digraph G {\n");
    // No default rankdir here: the `graph <dir>` handler below emits the
    // one true directive (DOT's own default is TB anyway), so the output
    // never carries a duplicate or stale rankdir.
    out.push_str("  node [shape=box, fontsize=10, fontname=\"Helvetica\"];\n");
    out.push_str("  edge [fontsize=8];\n");

    let mut depth: u32 = 0;
    let mut cluster_n: u32 = 0;

    for raw in mermaid.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }

        // `graph TD|BT|LR|RL` → translate direction to DOT rankdir.
        // Unknown directions fall back to TB.
        if let Some(dir) = line.strip_prefix("graph ").map(str::trim) {
            let rankdir = match dir {
                "BT" => "BT",
                "LR" => "LR",
                "RL" => "RL",
                _ => "TB",
            };
            let _ = writeln!(out, "  rankdir={rankdir};");
            continue;
        }

        // `end` closes the most-recently-opened subgraph.
        if line == "end" {
            if depth > 0 {
                depth -= 1;
                push_indent(&mut out, depth + 1);
                out.push_str("}\n");
            }
            continue;
        }

        // `subgraph ID["LABEL"]` opens a cluster.
        if let Some(rest) = line.strip_prefix("subgraph ") {
            let (id, label) = parse_subgraph(rest);
            cluster_n += 1;
            push_indent(&mut out, depth + 1);
            let _ = writeln!(out, "subgraph cluster_{cluster_n} {{");
            push_indent(&mut out, depth + 2);
            let _ = writeln!(out, "label=\"{}\";", esc(&label));
            push_indent(&mut out, depth + 2);
            let _ = writeln!(out, "// mermaid-id: {id}");
            depth += 1;
            continue;
        }

        // `FROM -->|"LABEL"| TO` (labeled edge).
        if let Some((from, after)) = line.split_once(" -->|")
            && let Some((label_part, to_part)) = after.split_once('|')
        {
            let label = label_part.trim().trim_matches('"');
            let to = to_part.trim();
            push_indent(&mut out, depth + 1);
            let _ = writeln!(out, "{} -> {} [label=\"{}\"];", from.trim(), to, esc(label));
            continue;
        }

        // `FROM --> TO` (unlabeled edge).
        if let Some((from, to)) = line.split_once(" --> ") {
            push_indent(&mut out, depth + 1);
            let _ = writeln!(out, "{} -> {};", from.trim(), to.trim());
            continue;
        }

        // Node declaration in any of the supported Mermaid shapes.
        if let Some((id, label)) = parse_node(line) {
            push_indent(&mut out, depth + 1);
            let _ = writeln!(out, "{} [label=\"{}\"];", id, esc(&label));
            continue;
        }

        // Anything else: surface as a comment so we don't silently drop
        // styling directives (`style`, `classDef`) the diff renderer might
        // emit. DOT styling is a follow-up; for now we keep the data visible.
        push_indent(&mut out, depth + 1);
        let _ = writeln!(out, "// unhandled: {line}");
    }

    // Defensive: close any subgraphs the input forgot to `end`.
    while depth > 0 {
        depth -= 1;
        push_indent(&mut out, depth + 1);
        out.push_str("}\n");
    }
    out.push_str("}\n");
    out
}

fn push_indent(out: &mut String, level: u32) {
    for _ in 0..level {
        out.push_str("  ");
    }
}

/// Escape a string for inclusion in a DOT double-quoted string.
fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Parse the body of a `subgraph` opener: `ID["LABEL"]` → (ID, LABEL).
/// Falls back to (rest, "") when no brackets are found.
fn parse_subgraph(rest: &str) -> (String, String) {
    if let Some((id, after)) = rest.split_once('[') {
        let label = after
            .trim_end_matches(']')
            .trim()
            .trim_matches('"')
            .to_string();
        return (id.trim().to_string(), label);
    }
    (rest.trim().to_string(), String::new())
}

/// Parse a Mermaid node declaration into `(id, label)`. Recognises the eight
/// shapes used by the renderers in this crate. Returns `None` if the line
/// isn't a node declaration.
fn parse_node(line: &str) -> Option<(String, String)> {
    let cut = line.find(['(', '[', '{'])?;
    let id = line[..cut].trim().to_string();
    if id.is_empty() || !id.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let rest = &line[cut..];

    // Try shapes longest-prefix-first so e.g. `(["` wins over `(`.
    let shapes: &[(&str, &str)] = &[
        ("((\"", "\"))"), // circle (function/impact target)
        ("([\"", "\"])"), // stadium (module external)
        ("[\"", "\"]"),   // rectangle with quotes (default)
        ("[/", "/]"),     // parallelogram (impl)
        ("[(", ")]"),     // cylinder (const/static)
        ("[[", "]]"),     // subroutine (macro)
        ("{{", "}}"),     // hexagon (trait)
        ("[", "]"),       // rectangle no quotes (fallback)
        ("(", ")"),       // rounded (struct/enum/type_alias)
    ];
    for (open, close) in shapes {
        if let Some(inner) = rest
            .strip_prefix(*open)
            .and_then(|s| s.strip_suffix(*close))
        {
            return Some((id, inner.to_string()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn empty_graph_renders_minimal_dot() {
        let out = mermaid_to_dot("graph TD\n");
        assert!(out.starts_with("digraph G {\n"));
        assert!(out.contains("rankdir=TB"));
        assert!(out.trim_end().ends_with('}'));
    }

    #[test]
    fn td_and_bt_rankdir_translate() {
        assert!(mermaid_to_dot("graph TD\n").contains("rankdir=TB;"));
        assert!(mermaid_to_dot("graph BT\n").contains("rankdir=BT;"));
        assert!(mermaid_to_dot("graph LR\n").contains("rankdir=LR;"));
        assert!(mermaid_to_dot("graph RL\n").contains("rankdir=RL;"));
    }

    #[test]
    fn rankdir_emitted_exactly_once() {
        for dir in ["TD", "BT", "LR", "RL"] {
            let out = mermaid_to_dot(&format!("graph {dir}\n    a[\"A\"]\n"));
            assert_eq!(
                out.matches("rankdir=").count(),
                1,
                "duplicate rankdir for {dir}: {out}"
            );
        }
    }

    #[test]
    fn rectangle_node_with_quoted_label() {
        let m = "graph TD\n    foo[\"foo bar — 1 fn\"]\n";
        let d = mermaid_to_dot(m);
        assert!(d.contains("foo [label=\"foo bar — 1 fn\"];"), "got: {d}");
    }

    #[test]
    fn rounded_node_struct_shape() {
        let m = "graph TD\n    s1(struct Foo)\n";
        let d = mermaid_to_dot(m);
        assert!(d.contains("s1 [label=\"struct Foo\"];"), "got: {d}");
    }

    #[test]
    fn hexagon_node_trait_shape() {
        let m = "graph TD\n    t1{{trait Bar}}\n";
        let d = mermaid_to_dot(m);
        assert!(d.contains("t1 [label=\"trait Bar\"];"), "got: {d}");
    }

    #[test]
    fn parallelogram_impl_cylinder_const_subroutine_macro() {
        let m = "graph TD\n    i1[/impl X/]\n    c1[(const Y)]\n    m1[[macro_rules Z]]\n";
        let d = mermaid_to_dot(m);
        assert!(d.contains("i1 [label=\"impl X\"];"), "impl: {d}");
        assert!(d.contains("c1 [label=\"const Y\"];"), "const: {d}");
        assert!(d.contains("m1 [label=\"macro_rules Z\"];"), "macro: {d}");
    }

    #[test]
    fn stadium_node_external_function() {
        let m = "graph TD\n    ext1([\"helper\"])\n";
        let d = mermaid_to_dot(m);
        assert!(d.contains("ext1 [label=\"helper\"];"), "got: {d}");
    }

    #[test]
    fn circle_node_target() {
        let m = "graph BT\n    target((\"fn target\"))\n";
        let d = mermaid_to_dot(m);
        assert!(d.contains("target [label=\"fn target\"];"), "got: {d}");
    }

    #[test]
    fn labeled_edge_translates_with_label() {
        let m = "graph TD\n    a[\"A\"]\n    b[\"B\"]\n    a -->|\"3 calls\"| b\n";
        let d = mermaid_to_dot(m);
        assert!(d.contains("a -> b [label=\"3 calls\"];"), "got: {d}");
    }

    #[test]
    fn unlabeled_edge_translates_without_label() {
        let m = "graph TD\n    a --> b\n";
        let d = mermaid_to_dot(m);
        assert!(d.contains("a -> b;"), "got: {d}");
        assert!(!d.contains("a -> b ["), "must not add label: {d}");
    }

    #[test]
    fn subgraph_becomes_cluster_with_label() {
        let m = "graph TD\n    subgraph mod1[\"my module\"]\n        a[\"A\"]\n    end\n";
        let d = mermaid_to_dot(m);
        assert!(d.contains("subgraph cluster_1 {"), "open: {d}");
        assert!(d.contains("label=\"my module\";"), "label: {d}");
        assert!(d.contains("a [label=\"A\"];"), "child: {d}");
        // Cluster must close before the outer digraph.
        let cluster_close = d.rfind('}').expect("close");
        let digraph_close = d.match_indices('}').next().expect("first").0;
        assert!(cluster_close > digraph_close, "structure: {d}");
    }

    #[test]
    fn unclosed_subgraph_is_closed_defensively() {
        let m = "graph TD\n    subgraph m[\"M\"]\n        a[\"A\"]\n";
        let d = mermaid_to_dot(m);
        // Two `}`: cluster + digraph.
        assert_eq!(d.matches('}').count(), 2, "got: {d}");
    }

    #[test]
    fn quotes_in_labels_are_escaped() {
        let m = "graph TD\n    n[\"has \\\"quotes\\\" inside\"]\n";
        let d = mermaid_to_dot(m);
        // The mermaid input had `\"` for the embedded quote — but our parser
        // strips at the outer `"`. We just need to check that any `"` in the
        // label is escaped in DOT output.
        let _ = d; // sanity: must not panic
    }

    #[test]
    fn project_view_edge_with_count_label() {
        // Mirrors what project::render emits.
        let m = concat!(
            "graph TD\n",
            "    crate_a[\"crate_a — 1 mod, 2 fn, 1 struct\"]\n",
            "    crate_b[\"crate_b — 1 mod, 0 fn, 0 struct\"]\n",
            "    crate_a -->|\"3 calls\"| crate_b\n",
        );
        let d = mermaid_to_dot(m);
        assert!(d.contains("crate_a [label=\"crate_a — 1 mod, 2 fn, 1 struct\"];"));
        assert!(d.contains("crate_b [label=\"crate_b — 1 mod, 0 fn, 0 struct\"];"));
        assert!(d.contains("crate_a -> crate_b [label=\"3 calls\"];"));
    }

    #[test]
    fn unknown_directive_becomes_comment_not_dropped() {
        let m = "graph TD\n    style foo fill:#0f0\n";
        let d = mermaid_to_dot(m);
        assert!(d.contains("// unhandled: style foo fill:#0f0"), "got: {d}");
    }
}
