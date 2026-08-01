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
//! Five cases would make this view *misleading* rather than merely
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
//! - **A call the resolver could not bind, shown as nothing at all.**
//!   Walking only the resolved edges hid most of what a body does: on a
//!   Flutter `main`, 4 of 13 calls. Those sites are now drawn as
//!   `unresolved` leaves — see [`unresolved_leaves`]. They keep a
//!   `classDef` of their own, because an `extern` is an atom the
//!   resolver *created* from a known module while an `unresolved` leaf
//!   is a site nothing is known about; styling them alike would claim
//!   knowledge that does not exist.
//! - **Stdlib noise removed in silence.** [`SKIP_CALLS`] keeps `clone` /
//!   `unwrap` / `len` out of the graph, which is right, but a reader
//!   comparing ranks would find gaps and no explanation. The count is
//!   emitted as a Mermaid comment instead.
//!
//! # Why the ranks come from the edge
//!
//! Sites are matched to edges by the ranks the resolver recorded on
//! [`crate::model::Edge::sites`], never by name. Matching on names
//! confuses homonyms — with `Baz::bar()` resolved and `foo.bar()` not,
//! one absorbs the other and a real call silently disappears.

use crate::error::Result;
use crate::model::{CallSite, CodeAtom, EntityId, call_flags};
use crate::render::AdjMaps;
use crate::render::lookup::resolve_function;
use crate::render::snapshot::AtomSnapshot;
use crate::render::util::{escape_label_flowchart, sanitize_id};
use crate::resolve::{EXTERN_KIND, SKIP_CALLS};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;

/// Default forward depth.
pub const DEFAULT_DEPTH: u8 = 3;

/// How leaves that expand into nothing — `extern` atoms and unresolved
/// call sites alike — are treated.
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

/// What a node stands for. Drives its `classDef`, and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    /// An atom parsed from the sources.
    Owned,
    /// An atom the resolver synthesised for a known external module.
    Extern,
    /// A call site with no edge — the target is genuinely unknown.
    Unresolved,
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

    let mut state = WalkState {
        nodes: BTreeMap::new(),
        edges: BTreeMap::new(),
        in_cycle: BTreeSet::new(),
        skipped: 0,
    };

    state.nodes.insert(
        target_id.as_str().to_owned(),
        NodeInfo {
            label: target_atom.name.clone(),
            kind: NodeKind::Owned,
        },
    );

    walk(
        adj,
        snapshot,
        &target_id,
        depth,
        external,
        &mut vec![target_id.as_str().to_owned()],
        &mut state,
    );

    Ok(emit(&target_id, target_atom, &state))
}

/// Everything the walk accumulates, so the recursion threads one
/// `&mut` instead of five.
struct WalkState {
    nodes: BTreeMap<String, NodeInfo>,
    edges: BTreeMap<EdgeKey, EdgeInfo>,
    in_cycle: BTreeSet<String>,
    /// Call sites dropped because they name stdlib noise
    /// ([`SKIP_CALLS`]). Counted so [`emit`] can say so — a rank gap the
    /// reader cannot explain is worse than a longer graph.
    skipped: usize,
}

struct NodeInfo {
    label: String,
    kind: NodeKind,
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
fn walk(
    adj: &AdjMaps,
    snapshot: &AtomSnapshot<'_>,
    from: &EntityId,
    remaining: u8,
    external: External,
    path: &mut Vec<String>,
    state: &mut WalkState,
) {
    let Some(from_atom) = snapshot.get(from) else {
        return;
    };
    let depth_used = path.len().saturating_sub(1);

    // Merge first: the same caller → callee pair can arrive as several
    // edges (the intra-file linker emits one per site), and the ranks of
    // all of them belong to the single edge this view draws.
    let mut merged: BTreeMap<&EntityId, Vec<u16>> = BTreeMap::new();
    for (callee_id, ranks) in adj.callees_with_sites(from) {
        merged
            .entry(callee_id.as_ref())
            .or_default()
            .extend_from_slice(ranks);
    }

    // Every rank accounted for by an edge, gathered before any filtering:
    // a callee hidden by `--external` still explains its site, and must
    // not come back as an unresolved leaf.
    let claimed: HashSet<u16> = merged.values().flatten().copied().collect();

    for (callee_id, ranks) in &merged {
        let Some(to_atom) = snapshot.get(callee_id) else {
            continue;
        };
        let is_external = to_atom.kind == EXTERN_KIND;
        if !show_leaf(external, is_external, depth_used) {
            continue;
        }

        let key = (from.as_str().to_owned(), callee_id.as_str().to_owned());
        let to_key = callee_id.as_str().to_owned();

        // Cycle: the callee is already on the path we came down.
        if path.contains(&to_key) {
            for id in path.iter() {
                state.in_cycle.insert(id.clone());
            }
            state.in_cycle.insert(to_key.clone());
            state.edges.entry(key).or_insert(EdgeInfo {
                label: Some("recursive".to_owned()),
                dotted: true,
            });
            continue;
        }

        state
            .nodes
            .entry(to_key.clone())
            .or_insert_with(|| NodeInfo {
                label: to_atom.name.clone(),
                kind: if is_external {
                    NodeKind::Extern
                } else {
                    NodeKind::Owned
                },
            });
        state.edges.entry(key).or_insert_with(|| EdgeInfo {
            label: edge_label(from_atom, ranks),
            dotted: false,
        });

        if remaining <= 1 {
            // Out of depth. If this callee still reaches something on the
            // current path, the cycle exists but would be invisible —
            // say so rather than render an acyclic-looking graph.
            if let Some(back) = reaches_path(adj, callee_id, path) {
                state
                    .edges
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
            state,
        );
        path.pop();
    }

    unresolved_leaves(
        from, from_atom, &claimed, &merged, snapshot, external, depth_used, state,
    );
}

/// Whether a leaf — `extern` atom or unresolved site — is shown at this
/// depth. Owned atoms are never subject to the policy.
fn show_leaf(external: External, is_leaf: bool, depth_used: usize) -> bool {
    if !is_leaf {
        return true;
    }
    match external {
        External::Always => true,
        External::Never => false,
        External::NearOnly => depth_used == 0,
    }
}

/// Draw the call sites of `from_atom` that no edge accounts for.
///
/// Two populations, kept apart because only one of them has ranks:
///
/// - `calls` whose rank is not in `claimed`. The resolver either found
///   no target, found several, or the target lives outside the graph.
/// - **every** `method_calls` entry, which the resolver never even
///   looks at (it holds `obj.m()` shapes whose receiver type is
///   unknown). The exception is a name the parser's intra-container
///   linker already bound to a sibling: that edge exists but carries no
///   rank, so it is recognised the one way it was created — by exact
///   name. This mirrors the linker's own rule rather than guessing.
#[expect(
    clippy::too_many_arguments,
    reason = "one call site; bundling these into a struct would only move the list"
)]
fn unresolved_leaves(
    from: &EntityId,
    from_atom: &CodeAtom,
    claimed: &HashSet<u16>,
    merged: &BTreeMap<&EntityId, Vec<u16>>,
    snapshot: &AtomSnapshot<'_>,
    external: External,
    depth_used: usize,
    state: &mut WalkState,
) {
    if !show_leaf(external, true, depth_used) {
        // Still count what stdlib filtering removed: the policy hides
        // the leaves, it does not make the calls stop existing.
        state.skipped += count_skipped(from_atom);
        return;
    }

    let linked_names: HashSet<&str> = merged
        .keys()
        .filter_map(|id| snapshot.get(id))
        .map(|atom| atom.name.as_str())
        .collect();

    // Degraded mode: some edge out of here carries no rank at all, so
    // `claimed` cannot be trusted to be complete and a site it misses
    // may well be one that edge already stands for. Fall back to the
    // name for those — a leaf duplicating an edge is a worse lie than a
    // site folded into it. When every edge has its ranks (the normal
    // case) this stays off, and names are never consulted.
    let degraded = merged.values().any(Vec::is_empty);

    // Group by name so two calls to the same unknown target share one
    // leaf, the way two calls to a known one share one edge.
    let mut leaves: BTreeMap<&str, Vec<&CallSite>> = BTreeMap::new();
    for site in &from_atom.calls {
        if claimed.contains(&site.rank) {
            continue;
        }
        if is_skipped(&site.name) {
            state.skipped += 1;
            continue;
        }
        if degraded {
            let (_, tail) = crate::resolve::split_call_name(&site.name);
            if linked_names.contains(tail) {
                continue;
            }
        }
        leaves.entry(site.name.as_str()).or_default().push(site);
    }
    for name in &from_atom.method_calls {
        if is_skipped(name) {
            state.skipped += 1;
            continue;
        }
        if linked_names.contains(name.as_str()) {
            continue;
        }
        leaves.entry(name.as_str()).or_default();
    }

    for (name, sites) in leaves {
        let to_key = format!("unresolved::{}::{name}", from.as_str());
        state
            .nodes
            .entry(to_key.clone())
            .or_insert_with(|| NodeInfo {
                label: name.to_owned(),
                kind: NodeKind::Unresolved,
            });
        state
            .edges
            .entry((from.as_str().to_owned(), to_key))
            .or_insert_with(|| EdgeInfo {
                label: sites_label(&sites),
                dotted: true,
            });
    }
}

/// Call sites of `atom` that [`SKIP_CALLS`] removes, both lists.
fn count_skipped(atom: &CodeAtom) -> usize {
    atom.calls
        .iter()
        .map(|s| s.name.as_str())
        .chain(atom.method_calls.iter().map(String::as_str))
        .filter(|name| is_skipped(name))
        .count()
}

/// Is this call name stdlib noise the resolver deliberately drops?
///
/// Compared on the last path segment, exactly as the resolver does —
/// `Vec::push` and a bare `push` are the same noise.
fn is_skipped(name: &str) -> bool {
    let (_, fn_name) = crate::resolve::split_call_name(name);
    SKIP_CALLS.contains(&fn_name)
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
/// `ranks` are the call sites the resolver recorded on the edge. When a
/// caller hits the same target several times the lowest-ranked site wins
/// and a multiplicity marker is appended, since one edge stands for all
/// of them.
///
/// Returns `None` when the edge carries no rank — a bundle from before
/// the ranks were recorded, or an edge the intra-container linker built
/// from a `method_calls` entry, which has no rank to give. Emitting a
/// bare `1` there would be inventing one.
fn edge_label(from_atom: &CodeAtom, ranks: &[u16]) -> Option<String> {
    let sites: Vec<&CallSite> = ranks
        .iter()
        .filter_map(|rank| from_atom.calls.iter().find(|s| s.rank == *rank))
        .collect();
    sites_label(&sites)
}

/// Shared by resolved edges and unresolved leaves: both stand for a set
/// of call sites and describe it the same way.
fn sites_label(sites: &[&CallSite]) -> Option<String> {
    let first = sites.iter().min_by_key(|s| s.rank)?;

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
    if sites.len() > 1 {
        parts.push(format!("x{}", sites.len()));
    }
    Some(parts.join(" "))
}

fn emit(target_id: &EntityId, target_atom: &CodeAtom, state: &WalkState) -> String {
    let mut out = String::from("graph TD\n");
    out.push_str("    classDef cycle fill:#fde,stroke:#c39,stroke-width:2px,color:#111\n");
    out.push_str("    classDef external fill:#eee,stroke:#999,stroke-dasharray:3 3,color:#555\n");
    out.push_str("    classDef unresolved fill:#fff,stroke:#bbb,stroke-dasharray:2 4,color:#777\n");

    let target_node = sanitize_id(target_id.as_str());
    let target_label = escape_label_flowchart(&format!("fn {} (entry)", target_atom.name));
    let _ = writeln!(out, "    {target_node}((\"{target_label}\"))");

    for (id, info) in &state.nodes {
        if id == target_id.as_str() {
            continue;
        }
        let nid = sanitize_id(id);
        let label = escape_label_flowchart(&info.label);
        match info.kind {
            NodeKind::Owned => {
                let _ = writeln!(out, "    {nid}[\"{label}\"]");
            }
            NodeKind::Extern => {
                let _ = writeln!(out, "    {nid}[\"{label}\"]:::external");
            }
            NodeKind::Unresolved => {
                let _ = writeln!(out, "    {nid}[\"{label}\"]:::unresolved");
            }
        }
    }

    for ((from, to), info) in &state.edges {
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

    for id in &state.in_cycle {
        let _ = writeln!(out, "    class {} cycle", sanitize_id(id));
    }

    // Say what was dropped. A reader comparing ranks would otherwise find
    // gaps with nothing to explain them.
    if state.skipped > 0 {
        let _ = writeln!(
            out,
            "    %% {} stdlib call(s) hidden (SKIP_CALLS)",
            state.skipped
        );
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
    ///
    /// Every callee named here also exists as an atom, so each edge
    /// carries the rank of the site that produced it — the shape the
    /// resolver actually emits.
    fn flow_of(defs: &[(&str, &[(&str, u8)])], target: &str, depth: u8) -> String {
        flow_with(defs, target, depth, External::NearOnly)
    }

    fn flow_with(
        defs: &[(&str, &[(&str, u8)])],
        target: &str,
        depth: u8,
        external: External,
    ) -> String {
        let store = Store::new();
        for (name, calls) in defs {
            store.add_atom(atom(name, calls));
        }
        let defined: BTreeSet<&str> = defs.iter().map(|(n, _)| *n).collect();
        for (name, calls) in defs {
            for (rank, (callee, _)) in calls.iter().enumerate() {
                if !defined.contains(callee) {
                    continue; // no atom → no edge, exactly like an unresolved call
                }
                store.add_edge(crate::model::Edge::calls_from_sites(
                    id_of(name),
                    id_of(callee),
                    vec![u16::try_from(rank).unwrap_or(0)],
                ));
            }
        }
        let adj = AdjMaps::build(&store);
        store
            .with_atoms(|atoms| {
                let snap = AtomSnapshot::build(atoms);
                render(&adj, &snap, target, depth, external)
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

    /// The bug this whole pass exists for: a call the resolver could not
    /// bind used to render as nothing at all.
    #[test]
    fn a_call_with_no_edge_becomes_an_unresolved_leaf() {
        let out = flow_of(
            &[("main", &[("known", 0), ("runApp", call_flags::AWAIT)])],
            "main",
            3,
        );
        assert!(out.contains("[\"runApp\"]:::unresolved"), "{out}");
        assert!(
            out.contains("|\"2 await\"|"),
            "rank and marker kept:\n{out}"
        );
    }

    /// Two sites sharing their last path segment: one resolved, one not.
    /// Matching sites to edges by name folded the second into the first
    /// and a real call vanished. The ranks keep them apart.
    #[test]
    fn homonymous_sites_yield_both_an_edge_and_a_leaf() {
        let store = Store::new();
        store.add_atom(atom("main", &[("Baz::bar", 0), ("bar", 0)]));
        store.add_atom(atom("bar", &[]));
        // Only the first site resolved.
        store.add_edge(crate::model::Edge::calls_from_sites(
            id_of("main"),
            id_of("bar"),
            vec![0],
        ));
        let adj = AdjMaps::build(&store);
        let out = store
            .with_atoms(|atoms| {
                let snap = AtomSnapshot::build(atoms);
                render(&adj, &snap, "main", 3, External::NearOnly)
            })
            .expect("render");
        assert!(
            out.contains("|\"1\"|"),
            "resolved site keeps rank 1:\n{out}"
        );
        assert!(
            out.contains(":::unresolved") && out.contains("|\"2\"|"),
            "the unresolved twin survives with its own rank:\n{out}"
        );
    }

    /// Stdlib noise stays out, but the reader is told — otherwise the
    /// rank gap it leaves has no explanation.
    #[test]
    fn skipped_stdlib_calls_are_hidden_but_counted() {
        let out = flow_of(
            &[("main", &[("clone", 0), ("unwrap", 0), ("real", 0)])],
            "main",
            3,
        );
        assert!(!out.contains("clone"), "{out}");
        assert!(
            out.contains("%% 2 stdlib call(s) hidden (SKIP_CALLS)"),
            "the drop must be announced:\n{out}"
        );
        assert!(out.contains("[\"real\"]:::unresolved"), "{out}");
    }

    /// `Vec::push` is the same noise as a bare `push` — the resolver
    /// compares the last segment, and so must this.
    #[test]
    fn qualified_stdlib_calls_are_skipped_too() {
        let out = flow_of(&[("main", &[("Vec::push", 0)])], "main", 3);
        assert!(!out.contains("push"), "{out}");
        assert!(out.contains("%% 1 stdlib call(s) hidden"), "{out}");
    }

    #[test]
    fn external_never_drops_unresolved_leaves() {
        let out = flow_with(&[("main", &[("runApp", 0)])], "main", 3, External::Never);
        assert!(!out.contains(":::unresolved"), "{out}");
    }

    /// Past depth 0 the near-only policy holds them back, exactly as it
    /// does for `extern` atoms.
    #[test]
    fn near_only_keeps_deep_unresolved_leaves_out() {
        let out = flow_of(
            &[("main", &[("mid", 0)]), ("mid", &[("deepUnknown", 0)])],
            "main",
            3,
        );
        assert!(out.contains("[\"mid\"]"), "{out}");
        assert!(!out.contains("deepUnknown"), "{out}");
        let all = flow_with(
            &[("main", &[("mid", 0)]), ("mid", &[("deepUnknown", 0)])],
            "main",
            3,
            External::Always,
        );
        assert!(
            all.contains("deepUnknown"),
            "--external always shows it:\n{all}"
        );
    }

    /// `method_calls` are never resolved, so each is a leaf — but one the
    /// intra-container linker already bound is not, or the graph would
    /// show the same call twice.
    #[test]
    fn method_calls_become_leaves_unless_already_linked() {
        let store = Store::new();
        let mut caller = atom("main", &[]);
        caller.method_calls = vec!["getPreferences".to_owned(), "sibling".to_owned()];
        store.add_atom(caller);
        store.add_atom(atom("sibling", &[]));
        // The linker's edge: built from a `method_calls` entry, so no rank.
        store.add_edge(crate::model::Edge::new(
            id_of("main"),
            id_of("sibling"),
            EdgeKind::Calls,
        ));
        let adj = AdjMaps::build(&store);
        let out = store
            .with_atoms(|atoms| {
                let snap = AtomSnapshot::build(atoms);
                render(&adj, &snap, "main", 3, External::NearOnly)
            })
            .expect("render");
        assert!(out.contains("[\"getPreferences\"]:::unresolved"), "{out}");
        assert!(
            !out.contains("unresolved::code:m.rs::function::main::sibling"),
            "the linked sibling must not be drawn twice:\n{out}"
        );
    }

    /// Degraded mode: an edge with no ranks at all (a bundle written
    /// before they were recorded). Its site must not come back as a leaf
    /// beside the very edge it produced.
    #[test]
    fn an_edge_without_ranks_does_not_duplicate_its_site() {
        let store = Store::new();
        store.add_atom(atom("main", &[("helper", 0)]));
        store.add_atom(atom("helper", &[]));
        store.add_edge(crate::model::Edge::new(
            id_of("main"),
            id_of("helper"),
            EdgeKind::Calls,
        ));
        let adj = AdjMaps::build(&store);
        let out = store
            .with_atoms(|atoms| {
                let snap = AtomSnapshot::build(atoms);
                render(&adj, &snap, "main", 3, External::NearOnly)
            })
            .expect("render");
        assert!(!out.contains(":::unresolved"), "no phantom twin:\n{out}");
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
