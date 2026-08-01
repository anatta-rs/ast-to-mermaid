//! Flow renderer — `a2m flow --target=<X>`.
//!
//! Forward call graph from one entry point, with each edge annotated by
//! the rank and control-flow context of its call site.
//!
//! # Why a separate view
//!
//! `impact` walks both directions over a fixed 3 hops and says nothing
//! about order; `sequence` gives order and control flow but for a single
//! body, without resolving its targets. Neither answers "start at `main`
//! and show me what it sets off". This does.
//!
//! # Honesty constraints
//!
//! Three cases would make this view *misleading* rather than merely
//! incomplete, and each is handled explicitly:
//!
//! - **A rank without its context.** `main` calling `_init` inside an
//!   `if` then `_run` after would render `|1|` `|2|`, implying a sequence
//!   the code never promises. Rank and markers are emitted together or
//!   not at all — [`edge_label`].
//! - **A shared node behind a cycle.** With `A→B→C→A` and `A→D→C`, a
//!   global visited-set marks `C` seen via `B` and drops `D→C`, which is
//!   not recursive at all — the reader concludes `D` never calls `C`. The
//!   set is therefore **per path**, see [`walk`].
//! - **A cycle hidden by the depth limit.** `A→B→C→A` at `--depth 2`
//!   stops before `C` loops back, so the graph looks acyclic. Stopping on
//!   a node that still reaches the current path emits a dotted
//!   `cycle (depth limit)` edge instead of silence.

use crate::error::Result;
use crate::model::{CodeAtom, EntityId, call_flags};
use crate::render::AdjMaps;
use crate::render::lookup::resolve_function;
use crate::render::snapshot::AtomSnapshot;
use crate::render::util::{escape_label_flowchart, sanitize_id};
use crate::resolve::EXTERN_KIND;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// Default forward depth.
pub const DEFAULT_DEPTH: u8 = 3;

/// How external (out-of-graph) leaves are treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum External {
    /// Shown at depth 1 only — enough to see that `main` ends on
    /// `runApp`, without the SDK swamping deeper levels. On a Flutter
    /// widget tree the external-to-owned ratio past depth 1 can exceed
    /// 10:1.
    NearOnly,
    /// Shown at every depth.
    Always,
    /// Never shown.
    Never,
}

/// One rendered edge, keyed for stable output.
type EdgeKey = (String, String);

/// Render the forward flow from `target`.
///
/// # Errors
///
/// Same as [`crate::render::lookup::resolve_function`].
///
/// # Panics
///
/// Never in practice: the only `expect` reads back the id
/// `resolve_function` just vouched for.
pub fn render(
    adj: &AdjMaps,
    snapshot: &AtomSnapshot<'_>,
    target: &str,
    depth: u8,
    external: External,
) -> Result<String> {
    let target_id = resolve_function(snapshot, target)?;
    let target_atom = snapshot
        .get(&target_id)
        .expect("resolve_function vouched the id exists");

    let mut nodes: BTreeMap<String, NodeInfo> = BTreeMap::new();
    let mut edges: BTreeMap<EdgeKey, EdgeInfo> = BTreeMap::new();
    let mut in_cycle: BTreeSet<String> = BTreeSet::new();

    nodes.insert(
        target_id.as_str().to_owned(),
        NodeInfo {
            label: target_atom.name.clone(),
            external: false,
        },
    );

    walk(
        adj,
        snapshot,
        &target_id,
        depth,
        external,
        &mut vec![target_id.as_str().to_owned()],
        &mut nodes,
        &mut edges,
        &mut in_cycle,
    );

    Ok(emit(&target_id, target_atom, &nodes, &edges, &in_cycle))
}

struct NodeInfo {
    label: String,
    external: bool,
}

struct EdgeInfo {
    /// `None` for a cycle-back edge, which has no call site of its own.
    label: Option<String>,
    dotted: bool,
}

/// Depth-first forward walk.
///
/// `path` is the chain of ids from the target down to the current node.
/// Membership in `path` — not "seen anywhere" — is what identifies a
/// cycle, which is the whole point: a node reached again on a *different*
/// branch is a legitimate shared callee, not recursion.
#[expect(
    clippy::too_many_arguments,
    reason = "walk state is threaded explicitly rather than boxed into a struct used once"
)]
fn walk(
    adj: &AdjMaps,
    snapshot: &AtomSnapshot<'_>,
    from: &EntityId,
    remaining: u8,
    external: External,
    path: &mut Vec<String>,
    nodes: &mut BTreeMap<String, NodeInfo>,
    edges: &mut BTreeMap<EdgeKey, EdgeInfo>,
    in_cycle: &mut BTreeSet<String>,
) {
    let Some(from_atom) = snapshot.get(from) else {
        return;
    };
    let depth_used = path.len().saturating_sub(1);

    for callee_id in adj.callees(from) {
        let Some(to_atom) = snapshot.get(callee_id) else {
            continue;
        };
        let is_external = to_atom.kind == EXTERN_KIND;
        if !show_external(external, is_external, depth_used) {
            continue;
        }

        let key = (from.as_str().to_owned(), callee_id.as_str().to_owned());
        let to_key = callee_id.as_str().to_owned();

        // Cycle: the callee is already on the path we came down.
        if path.contains(&to_key) {
            for id in path.iter() {
                in_cycle.insert(id.clone());
            }
            in_cycle.insert(to_key.clone());
            edges.entry(key).or_insert(EdgeInfo {
                label: Some("recursive".to_owned()),
                dotted: true,
            });
            continue;
        }

        nodes.entry(to_key.clone()).or_insert_with(|| NodeInfo {
            label: to_atom.name.clone(),
            external: is_external,
        });
        edges.entry(key).or_insert_with(|| EdgeInfo {
            label: edge_label(from_atom, to_atom),
            dotted: false,
        });

        if remaining <= 1 {
            // Out of depth. If this callee still reaches something on the
            // current path, the cycle exists but would be invisible —
            // say so rather than render an acyclic-looking graph.
            if let Some(back) = reaches_path(adj, callee_id, path) {
                edges
                    .entry((to_key.clone(), back.clone()))
                    .or_insert(EdgeInfo {
                        label: Some("cycle (depth limit)".to_owned()),
                        dotted: true,
                    });
            }
            continue;
        }

        // Externals are leaves by definition — nothing to expand into.
        if is_external {
            continue;
        }
        path.push(to_key);
        walk(
            adj,
            snapshot,
            callee_id,
            remaining - 1,
            external,
            path,
            nodes,
            edges,
            in_cycle,
        );
        path.pop();
    }
}

/// Whether an external leaf is shown at this depth.
fn show_external(external: External, is_external: bool, depth_used: usize) -> bool {
    if !is_external {
        return true;
    }
    match external {
        External::Always => true,
        External::Never => false,
        External::NearOnly => depth_used == 0,
    }
}

/// Does `node` call anything already on `path`? Returns the first such id.
///
/// One hop only: this answers "would expanding here close a cycle", which
/// is exactly what the depth cut is about to hide.
fn reaches_path(adj: &AdjMaps, node: &EntityId, path: &[String]) -> Option<String> {
    adj.callees(node)
        .iter()
        .map(|id| id.as_str().to_owned())
        .find(|id| path.contains(id))
}

/// Label for a `caller → callee` edge: rank plus control-flow markers.
///
/// The call site is found by matching the callee's name against the tail
/// of each recorded call name — the parser stores `module::fn` or
/// `Owner::method`, the atom carries the bare `fn` / `method`. When a
/// caller hits the same target several times the lowest-ranked site wins
/// and a multiplicity marker is appended, since one edge stands for all
/// of them.
///
/// Returns `None` when no site matches — an edge the resolver produced by
/// a route the parser did not record. Emitting a bare rank there would be
/// inventing one.
fn edge_label(from_atom: &CodeAtom, to_atom: &CodeAtom) -> Option<String> {
    let matches: Vec<_> = from_atom
        .calls
        .iter()
        .filter(|site| {
            site.name == to_atom.name
                || site
                    .name
                    .rsplit("::")
                    .next()
                    .is_some_and(|tail| tail == to_atom.name)
        })
        .collect();
    let first = matches.iter().min_by_key(|s| s.rank)?;

    let mut parts = vec![(first.rank + 1).to_string()];
    if first.has(call_flags::AWAIT) {
        parts.push("await".to_owned());
    }
    if first.has(call_flags::CONDITIONAL) {
        parts.push("alt".to_owned());
    }
    if first.has(call_flags::REPEATED) {
        parts.push("loop".to_owned());
    }
    if matches.len() > 1 {
        parts.push(format!("x{}", matches.len()));
    }
    Some(parts.join(" "))
}

fn emit(
    target_id: &EntityId,
    target_atom: &CodeAtom,
    nodes: &BTreeMap<String, NodeInfo>,
    edges: &BTreeMap<EdgeKey, EdgeInfo>,
    in_cycle: &BTreeSet<String>,
) -> String {
    let mut out = String::from("graph TD\n");
    out.push_str("    classDef cycle fill:#fde,stroke:#c39,stroke-width:2px,color:#111\n");
    out.push_str("    classDef external fill:#eee,stroke:#999,stroke-dasharray:3 3,color:#555\n");

    let target_node = sanitize_id(target_id.as_str());
    let target_label = escape_label_flowchart(&format!("fn {} (entry)", target_atom.name));
    let _ = writeln!(out, "    {target_node}((\"{target_label}\"))");

    for (id, info) in nodes {
        if id == target_id.as_str() {
            continue;
        }
        let nid = sanitize_id(id);
        let label = escape_label_flowchart(&info.label);
        if info.external {
            let _ = writeln!(out, "    {nid}[\"{label}\"]:::external");
        } else {
            let _ = writeln!(out, "    {nid}[\"{label}\"]");
        }
    }

    for (from, to) in edges.keys() {
        let info = &edges[&(from.clone(), to.clone())];
        let fid = sanitize_id(from);
        let tid = sanitize_id(to);
        let arrow = if info.dotted { "-.->" } else { "-->" };
        match &info.label {
            Some(l) => {
                let l = escape_label_flowchart(l);
                let _ = writeln!(out, "    {fid} {arrow}|\"{l}\"| {tid}");
            }
            None => {
                let _ = writeln!(out, "    {fid} {arrow} {tid}");
            }
        }
    }

    for id in in_cycle {
        let _ = writeln!(out, "    class {} cycle", sanitize_id(id));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Store;
    use crate::model::{CallSite, EdgeKind};

    fn atom(name: &str, calls: &[(&str, u8)]) -> CodeAtom {
        CodeAtom {
            id: EntityId::new(format!("code:m.rs::function::{name}")),
            kind: "function".to_owned(),
            name: name.to_owned(),
            file_path: "m.rs".to_owned(),
            line_start: 1,
            line_end: 2,
            doc: String::new(),
            signature: String::new(),
            content_hash: "h".to_owned(),
            calls: calls
                .iter()
                .enumerate()
                .map(|(i, (n, f))| CallSite {
                    name: (*n).to_owned(),
                    rank: u16::try_from(i).unwrap_or(0),
                    flags: *f,
                })
                .collect(),
            method_calls: Vec::new(),
            parent: None,
        }
    }

    fn id_of(name: &str) -> EntityId {
        EntityId::new(format!("code:m.rs::function::{name}"))
    }

    /// Build a store from `(caller, callees)` pairs and render the flow.
    fn flow_of(defs: &[(&str, &[(&str, u8)])], target: &str, depth: u8) -> String {
        let store = Store::new();
        for (name, calls) in defs {
            store.add_atom(atom(name, calls));
        }
        for (name, calls) in defs {
            for (callee, _) in *calls {
                store.add_edge(crate::model::Edge::new(
                    id_of(name),
                    id_of(callee),
                    EdgeKind::Calls,
                ));
            }
        }
        let adj = AdjMaps::build(&store);
        store
            .with_atoms(|atoms| {
                let snap = AtomSnapshot::build(atoms);
                render(&adj, &snap, target, depth, External::NearOnly)
            })
            .expect("render")
    }

    #[test]
    fn edge_carries_rank_and_markers_together() {
        let out = flow_of(
            &[
                (
                    "main",
                    &[("a", call_flags::AWAIT), ("b", call_flags::CONDITIONAL)],
                ),
                ("a", &[]),
                ("b", &[]),
            ],
            "main",
            2,
        );
        assert!(out.contains("|\"1 await\"|"), "{out}");
        assert!(out.contains("|\"2 alt\"|"), "{out}");
    }

    /// The bug a global visited-set would cause: `C` is reached first
    /// through the cycle `A→B→C`, so `D→C` gets dropped and the reader
    /// concludes `D` never calls `C`.
    #[test]
    fn a_node_shared_with_a_cycle_keeps_its_other_inbound_edge() {
        let out = flow_of(
            &[
                ("a", &[("b", 0), ("d", 0)]),
                ("b", &[("c", 0)]),
                ("c", &[("a", 0)]),
                ("d", &[("c", 0)]),
            ],
            "a",
            5,
        );
        let d = sanitize_id(id_of("d").as_str());
        let c = sanitize_id(id_of("c").as_str());
        assert!(
            out.contains(&format!("{d} -->"))
                && out.lines().any(|l| l.contains(&d) && l.contains(&c)),
            "d -> c must survive the cycle through b:\n{out}"
        );
    }

    #[test]
    fn a_cycle_styles_its_nodes() {
        let out = flow_of(
            &[("a", &[("b", 0)]), ("b", &[("c", 0)]), ("c", &[("a", 0)])],
            "a",
            5,
        );
        assert!(out.contains("class "), "cycle nodes must be styled:\n{out}");
        assert!(out.contains("recursive"), "{out}");
    }

    /// A cycle cut by the depth limit must not leave an acyclic-looking
    /// graph — the dotted edge is what says "it loops back".
    #[test]
    fn a_cycle_cut_by_depth_is_still_announced() {
        let out = flow_of(
            &[("a", &[("b", 0)]), ("b", &[("c", 0)]), ("c", &[("a", 0)])],
            "a",
            2,
        );
        assert!(
            out.contains("cycle (depth limit)"),
            "depth cut must not hide the cycle:\n{out}"
        );
    }

    #[test]
    fn depth_one_shows_only_direct_calls() {
        let out = flow_of(
            &[("a", &[("b", 0)]), ("b", &[("c", 0)]), ("c", &[])],
            "a",
            1,
        );
        let c = sanitize_id(id_of("c").as_str());
        assert!(!out.contains(&c), "c is two hops away:\n{out}");
    }

    /// No call site matches, so there is no rank to state — the edge is
    /// drawn bare rather than carrying an invented `1`.
    #[test]
    fn an_edge_without_a_matching_call_site_has_no_label() {
        let store = Store::new();
        store.add_atom(atom("a", &[]));
        store.add_atom(atom("b", &[]));
        store.add_edge(crate::model::Edge::new(
            id_of("a"),
            id_of("b"),
            EdgeKind::Calls,
        ));
        let adj = AdjMaps::build(&store);
        let out = store
            .with_atoms(|atoms| {
                let snap = AtomSnapshot::build(atoms);
                render(&adj, &snap, "a", 2, External::NearOnly)
            })
            .expect("render");
        assert!(!out.contains("|\""), "no invented label:\n{out}");
    }
}
