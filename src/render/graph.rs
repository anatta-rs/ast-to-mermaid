//! Whole-project knowledge graph: every file, its key items, and the
//! typed edges between them, in one diagram.
//!
//! The other views either aggregate (`project`, `overview`: one node per
//! crate / module, call counts) or zoom in (`module`, `function`,
//! `impact`, `flow`: one target). This one shows the project at entity
//! level, organised the way a reader draws it by hand:
//!
//! - nested `subgraph`s per directory (down to `group_depth`), one
//!   `subgraph` per file holding the file node and its items;
//! - one `classDef` per kind — file, function, type, external dependency;
//! - labelled edges: `calls`, `imports` (file → file), `references`
//!   (type use), `implements`, `depends on` (file → external crate);
//! - every node defined before the first edge.
//!
//! A real project has far more items than a diagram can hold, so items
//! are ranked — cross-file degree first — and cut at `max_nodes`. A file
//! appears with the items it holds; it then carries the file-level edges
//! (`imports`, `depends on`).

use super::adj::AdjMaps;
use super::snapshot::AtomSnapshot;
use super::util::{escape_label_flowchart, sanitize_id};
use crate::model::{CodeAtom, EntityId};
use crate::resolve::EXTERN_KIND;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;

/// Atom kinds an `impl … for <Type>` can name.
const TYPE_KINDS: &[&str] = &["struct", "enum", "union", "type_alias"];

/// Label of a trait method → implementation edge (drawn with
/// `implements`).
const DISPATCHES: &str = "dispatches to";

/// Default cap on rendered nodes. GitHub renders about 500 edges; past
/// ~150 nodes a `graph TD` stops being readable anyway.
pub const DEFAULT_MAX_NODES: usize = 150;

/// Default directory nesting rendered as subgraphs.
pub const DEFAULT_GROUP_DEPTH: usize = 2;

/// Item kinds shown by default.
pub const DEFAULT_KINDS: &[&str] = &["function", "struct", "enum", "trait", "type_alias"];

/// Edge kinds drawn by default.
pub const DEFAULT_EDGES: &[GraphEdge] = &[
    GraphEdge::Calls,
    GraphEdge::Imports,
    GraphEdge::References,
    GraphEdge::Implements,
    GraphEdge::Extern,
];

/// One kind of edge the graph can draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GraphEdge {
    /// Function → function.
    Calls,
    /// File → file.
    Imports,
    /// Item → type its declaration names.
    References,
    /// Type → trait it implements.
    Implements,
    /// File → item it defines. Off by default: the file subgraph already
    /// says it.
    Defines,
    /// File → external crate / package it calls into.
    Extern,
}

impl GraphEdge {
    /// Label drawn on the edge, also the name accepted by `--edges`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Calls => "calls",
            Self::Imports => "imports",
            Self::References => "references",
            Self::Implements => "implements",
            Self::Defines => "defines",
            Self::Extern => "depends on",
        }
    }

    /// Parse a `--edges` entry (`calls`, `imports`, `references` / `uses`,
    /// `implements`, `defines` / `contains`, `extern`).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "calls" => Self::Calls,
            "imports" => Self::Imports,
            "references" | "uses" => Self::References,
            "implements" => Self::Implements,
            "defines" | "contains" => Self::Defines,
            "extern" | "deps" => Self::Extern,
            _ => return None,
        })
    }
}

/// Options for [`render`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GraphOptions {
    /// Cap on rendered items plus the files holding them. External
    /// dependency nodes (one per crate) come on top.
    pub max_nodes: usize,
    /// Item kinds to show besides files (`function`, `struct`, …).
    pub kinds: Vec<String>,
    /// Edge kinds to draw.
    pub edges: Vec<GraphEdge>,
    /// Directory levels rendered as nested subgraphs; `0` for none.
    pub group_depth: usize,
    /// Show private items with no cross-file edge too.
    pub include_private: bool,
}

impl Default for GraphOptions {
    fn default() -> Self {
        Self {
            max_nodes: DEFAULT_MAX_NODES,
            kinds: DEFAULT_KINDS.iter().map(|k| (*k).to_owned()).collect(),
            edges: DEFAULT_EDGES.to_vec(),
            group_depth: DEFAULT_GROUP_DEPTH,
            include_private: false,
        }
    }
}

/// Render the knowledge graph of everything in `snapshot`.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn render(adj: &AdjMaps, snapshot: &AtomSnapshot<'_>, opts: &GraphOptions) -> String {
    let wants = |e: GraphEdge| opts.edges.contains(&e);

    // ── Files ───────────────────────────────────────────────────────────
    let mut files: BTreeMap<&str, &CodeAtom> = BTreeMap::new();
    for atom in snapshot.iter() {
        if atom.kind == "module" {
            files.insert(atom.file_path.as_str(), atom);
        }
    }

    // ── Items: candidates, ranked ───────────────────────────────────────
    let in_file = |id: &EntityId| snapshot.get(id).map(|a| a.file_path.as_str());
    let mut candidates: Vec<(usize, usize, &CodeAtom)> = Vec::new();
    for atom in snapshot.iter() {
        if atom.kind == "module"
            || atom.kind == EXTERN_KIND
            || atom.file_path.is_empty()
            || !opts.kinds.contains(&atom.kind)
        {
            continue;
        }
        let neighbours: Vec<&EntityId> = adj
            .callers(&atom.id)
            .iter()
            .chain(adj.callees(&atom.id))
            .chain(adj.uses_in(&atom.id))
            .chain(adj.uses_out(&atom.id))
            .map(AsRef::as_ref)
            .filter(|id| snapshot.get(id).is_some_and(|a| a.kind != EXTERN_KIND))
            .collect();
        let cross = neighbours
            .iter()
            .filter(|id| in_file(id).is_some_and(|f| f != atom.file_path))
            .count();
        if !opts.include_private && cross == 0 && !is_public(atom) {
            continue;
        }
        candidates.push((cross, neighbours.len(), atom));
    }
    candidates.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(b.1.cmp(&a.1))
            .then_with(|| a.2.id.cmp(&b.2.id))
    });

    // External crates the shown files call into: one node per crate head.
    let mut deps: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    if wants(GraphEdge::Extern) {
        for atom in snapshot.iter() {
            if atom.kind != "function" || atom.file_path.is_empty() {
                continue;
            }
            for callee in adj.callees(&atom.id) {
                if let Some(ext) = snapshot.get(callee).filter(|a| a.kind == EXTERN_KIND) {
                    let head = ext.signature.split("::").next().unwrap_or_default();
                    if is_dependency_name(head) {
                        deps.entry(head.to_owned())
                            .or_default()
                            .insert(atom.file_path.as_str());
                    }
                }
            }
        }
    }

    // Fill the node budget in rank order. An item brings its file node
    // along, so it costs two the first time its file appears; a file is
    // drawn only for the items it holds — fifty bare file nodes would eat
    // a small budget before any item made it in.
    let mut used = 0usize;
    let mut kept_files: BTreeSet<&str> = BTreeSet::new();
    let mut shown: Vec<&CodeAtom> = Vec::new();
    for (_, _, atom) in &candidates {
        let file = atom.file_path.as_str();
        if !files.contains_key(file) {
            continue;
        }
        let cost = 1 + usize::from(!kept_files.contains(file));
        if used + cost > opts.max_nodes {
            continue;
        }
        used += cost;
        kept_files.insert(file);
        shown.push(atom);
    }
    let truncated = candidates.len() - shown.len();
    files.retain(|path, _| kept_files.contains(path));
    // External dependencies sit outside the budget: one node per crate
    // or package, a handful per project, and dropping them would hide the
    // very edges the view adds over `overview`.
    deps.retain(|_, users| {
        users.retain(|u| kept_files.contains(u));
        !users.is_empty()
    });
    let shown_ids: HashMap<&EntityId, &CodeAtom> = shown.iter().map(|a| (&a.id, *a)).collect();

    // ── Nodes ───────────────────────────────────────────────────────────
    let mut out = String::with_capacity(8192);
    writeln!(out, "graph TD").ok();
    writeln!(
        out,
        "    %% a2m graph: {} files, {} items, {} external deps",
        files.len(),
        shown.len(),
        deps.len()
    )
    .ok();
    if truncated > 0 {
        writeln!(
            out,
            "    %% truncated {truncated} lower-ranked items (raise --max-nodes, narrow --kinds, or use --format dot)"
        )
        .ok();
    }
    writeln!(
        out,
        "    classDef fileNode fill:#FFFACD,stroke:#DAA520,stroke-width:2px,color:#000"
    )
    .ok();
    writeln!(
        out,
        "    classDef functionNode fill:#87CEEB,stroke:#4682B4,stroke-width:2px,color:#000"
    )
    .ok();
    writeln!(
        out,
        "    classDef classNode fill:#DDA0DD,stroke:#8A2BE2,stroke-width:2px,color:#000"
    )
    .ok();
    writeln!(
        out,
        "    classDef dependencyNode fill:#90EE90,stroke:#2E8B57,stroke-width:2px,color:#000"
    )
    .ok();

    let mut items_by_file: BTreeMap<&str, Vec<&CodeAtom>> = BTreeMap::new();
    for atom in &shown {
        items_by_file
            .entry(atom.file_path.as_str())
            .or_default()
            .push(atom);
    }
    for items in items_by_file.values_mut() {
        items.sort_by(|a, b| {
            a.line_start
                .cmp(&b.line_start)
                .then_with(|| a.id.cmp(&b.id))
        });
    }

    // Directory tree, cut at `group_depth`.
    let mut tree = DirTree::default();
    for path in files.keys() {
        let dirs: Vec<&str> = path.rsplit_once('/').map_or_else(Vec::new, |(d, _)| {
            d.split('/').take(opts.group_depth).collect()
        });
        tree.insert(&dirs, path);
    }
    write_tree(&mut out, &tree, "", 1, &files, &items_by_file);

    for head in deps.keys() {
        writeln!(
            out,
            "    {}([\"{}\"]):::dependencyNode",
            dep_id(head),
            escape_label_flowchart(head)
        )
        .ok();
    }

    // ── Edges ───────────────────────────────────────────────────────────
    let mut edges: BTreeSet<(String, &'static str, String)> = BTreeSet::new();
    let node = |a: &CodeAtom| sanitize_id(a.id.as_str());
    for atom in &shown {
        if wants(GraphEdge::Calls) {
            for callee in adj.callees(&atom.id) {
                if let Some(t) = shown_ids.get(callee.as_ref()) {
                    edges.insert((node(atom), GraphEdge::Calls.label(), node(t)));
                }
            }
        }
        if wants(GraphEdge::References) {
            for used in adj.uses_out(&atom.id) {
                if let Some(t) = shown_ids.get(used.as_ref())
                    && t.id != atom.id
                {
                    edges.insert((node(atom), GraphEdge::References.label(), node(t)));
                }
            }
        }
        if wants(GraphEdge::Defines)
            && let Some(file) = files.get(atom.file_path.as_str())
        {
            edges.insert((node(file), GraphEdge::Defines.label(), node(atom)));
        }
    }
    if wants(GraphEdge::Implements) {
        // The edge leaves the `impl` block, which is rarely shown; draw it
        // from the implementing type instead. The impl often lives in a
        // third file (`impl AuthStore for SqliteStore` in `sqlite/auth.rs`,
        // the struct in `sqlite/mod.rs`): look in the impl's file first,
        // then in the files it imports, then for a unique type of that name.
        let types: Vec<&CodeAtom> = shown
            .iter()
            .copied()
            .filter(|s| s.parent.is_none() && TYPE_KINDS.contains(&s.kind.as_str()))
            .collect();
        for atom in snapshot.iter().filter(|a| a.kind == "impl") {
            let Some((_, ty)) = atom.name.split_once(" for ") else {
                continue;
            };
            let ty = ty.split('<').next().unwrap_or(ty).trim();
            let ty = ty.rsplit("::").next().unwrap_or(ty);
            let named: Vec<&CodeAtom> = types.iter().copied().filter(|s| s.name == ty).collect();
            let imported: Vec<&str> = adj
                .imports_out(&EntityId::new(format!("code:{}", atom.file_path)))
                .iter()
                .filter_map(|m| snapshot.get(m))
                .map(|m| m.file_path.as_str())
                .collect();
            let local: Vec<&CodeAtom> = named
                .iter()
                .copied()
                .filter(|s| s.file_path == atom.file_path)
                .collect();
            let via_import: Vec<&CodeAtom> = named
                .iter()
                .copied()
                .filter(|s| imported.contains(&s.file_path.as_str()))
                .collect();
            let owner = match (local.as_slice(), via_import.as_slice(), named.as_slice()) {
                ([one], _, _) | ([], [one], _) | ([], [], [one]) => Some(*one),
                _ => None,
            };
            for tr in adj.implements_out(&atom.id) {
                if let (Some(owner), Some(t)) = (owner, shown_ids.get(tr.as_ref())) {
                    edges.insert((node(owner), GraphEdge::Implements.label(), node(t)));
                }
            }
        }
        // Method level: a trait method dispatches to each implementation.
        for atom in &shown {
            for tr in adj.implements_out(&atom.id) {
                if let Some(t) = shown_ids.get(tr.as_ref())
                    && t.kind == "function"
                {
                    edges.insert((node(t), DISPATCHES, node(atom)));
                }
            }
        }
    }
    if wants(GraphEdge::Imports) {
        // An import between two files whose items are already linked says
        // nothing the item edge does not; drawing both doubled the edge
        // count of a real project and buried the structure.
        let mut linked: BTreeSet<(&str, &str)> = BTreeSet::new();
        let file_of: HashMap<String, &str> = shown
            .iter()
            .map(|a| (node(a), a.file_path.as_str()))
            .collect();
        for (from, _, to) in &edges {
            if let (Some(f), Some(t)) = (file_of.get(from), file_of.get(to)) {
                linked.insert((f, t));
            }
        }
        for file in files.values() {
            for to in adj.imports_out(&file.id) {
                if let Some(t) = snapshot.get(to).filter(|a| a.kind == "module")
                    && t.id != file.id
                    && files.contains_key(t.file_path.as_str())
                    && !linked.contains(&(file.file_path.as_str(), t.file_path.as_str()))
                {
                    edges.insert((node(file), GraphEdge::Imports.label(), node(t)));
                }
            }
        }
    }
    for (head, users) in &deps {
        for path in users {
            if let Some(file) = files.get(path) {
                edges.insert((node(file), GraphEdge::Extern.label(), dep_id(head)));
            }
        }
    }
    for (from, label, to) in &edges {
        writeln!(out, "    {from} --\"{label}\"--> {to}").ok();
    }
    out
}

/// Whether an item is part of its file's interface: `pub` in Rust, no
/// leading underscore in Python and Dart.
fn is_public(atom: &CodeAtom) -> bool {
    if is_rust(&atom.file_path) {
        atom.signature.starts_with("pub")
    } else {
        !atom.name.starts_with('_')
    }
}

/// Whether an extern's qualifier head names a crate / package worth a
/// dependency node: lower-case (a capitalised head is a type, not a
/// crate), and neither the standard library nor a primitive type
/// (`u32::try_from`).
fn is_dependency_name(head: &str) -> bool {
    head.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && !matches!(
            head,
            "std"
                | "core"
                | "alloc"
                | "self"
                | "super"
                | "crate"
                | "u8"
                | "u16"
                | "u32"
                | "u64"
                | "u128"
                | "usize"
                | "i8"
                | "i16"
                | "i32"
                | "i64"
                | "i128"
                | "isize"
                | "f32"
                | "f64"
                | "bool"
                | "char"
                | "str"
        )
}

fn is_rust(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("rs"))
}

fn dep_id(head: &str) -> String {
    format!("dep_{}", sanitize_id(head))
}

/// Directories → files, nested.
#[derive(Default)]
struct DirTree<'a> {
    dirs: BTreeMap<&'a str, DirTree<'a>>,
    files: Vec<&'a str>,
}

impl<'a> DirTree<'a> {
    fn insert(&mut self, dirs: &[&'a str], file: &'a str) {
        match dirs.split_first() {
            None => self.files.push(file),
            Some((head, rest)) => self.dirs.entry(head).or_default().insert(rest, file),
        }
    }
}

fn write_tree(
    out: &mut String,
    tree: &DirTree<'_>,
    prefix: &str,
    depth: usize,
    files: &BTreeMap<&str, &CodeAtom>,
    items_by_file: &BTreeMap<&str, Vec<&CodeAtom>>,
) {
    let pad = "    ".repeat(depth);
    for (name, sub) in &tree.dirs {
        let path = if prefix.is_empty() {
            (*name).to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        writeln!(
            out,
            "{pad}subgraph dir_{}[\"{}/\"]",
            sanitize_id(&path),
            escape_label_flowchart(&path)
        )
        .ok();
        write_tree(out, sub, &path, depth + 1, files, items_by_file);
        writeln!(out, "{pad}end").ok();
    }
    for path in &tree.files {
        let Some(file) = files.get(path) else {
            continue;
        };
        let file_node = format!(
            "{}[\"{}\"]:::fileNode",
            sanitize_id(file.id.as_str()),
            escape_label_flowchart(path.rsplit('/').next().unwrap_or(path))
        );
        let Some(items) = items_by_file.get(path) else {
            writeln!(out, "{pad}{file_node}").ok();
            continue;
        };
        writeln!(
            out,
            "{pad}subgraph file_{}[\"{}\"]",
            sanitize_id(path),
            escape_label_flowchart(path)
        )
        .ok();
        writeln!(out, "{pad}    {file_node}").ok();
        for item in items {
            writeln!(out, "{pad}    {}", item_node(item)).ok();
        }
        writeln!(out, "{pad}end").ok();
    }
}

/// Node definition for an item: shape and class by kind.
fn item_node(atom: &CodeAtom) -> String {
    let id = sanitize_id(atom.id.as_str());
    let oo = !is_rust(&atom.file_path);
    match atom.kind.as_str() {
        "function" => {
            let name = match &atom.parent {
                Some(owner) => format!(
                    "{}::{}()",
                    owner.split('<').next().unwrap_or(owner).trim(),
                    atom.name
                ),
                None => format!("{}()", atom.name),
            };
            format!("{id}(\"{}\"):::functionNode", escape_label_flowchart(&name))
        }
        kind => {
            let shown_kind = match kind {
                "struct" if oo => "class",
                "type_alias" => "type",
                k => k,
            };
            format!(
                "{id}[\"{}\"]:::classNode",
                escape_label_flowchart(&format!("{shown_kind} {}", atom.name))
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Store;
    use crate::model::{AtomRefs, Edge, EdgeKind};

    fn atom(id: &str, kind: &str, name: &str, file: &str, sig: &str) -> CodeAtom {
        CodeAtom {
            id: EntityId::new(id),
            kind: kind.to_owned(),
            name: name.to_owned(),
            file_path: file.to_owned(),
            line_start: 1,
            line_end: 1,
            doc: String::new(),
            signature: sig.to_owned(),
            content_hash: String::new(),
            calls: Vec::new(),
            method_calls: Vec::new(),
            parent: None,
            refs: AtomRefs::default(),
        }
    }

    fn sample() -> Store {
        let store = Store::new();
        store.add_atom(atom("code:lib.rs", "module", "lib", "lib.rs", ""));
        store.add_atom(atom(
            "code:render/mod.rs",
            "module",
            "mod",
            "render/mod.rs",
            "",
        ));
        store.add_atom(atom(
            "code:lib.rs::function::run",
            "function",
            "run",
            "lib.rs",
            "pub fn run()",
        ));
        store.add_atom(atom(
            "code:render/mod.rs::function::draw",
            "function",
            "draw",
            "render/mod.rs",
            "fn draw()",
        ));
        store.add_atom(atom(
            "code:render/mod.rs::function::helper",
            "function",
            "helper",
            "render/mod.rs",
            "fn helper()",
        ));
        store.add_atom(atom(
            "extern:serde_json::to_string",
            EXTERN_KIND,
            "to_string",
            "",
            "serde_json::to_string",
        ));
        store.add_edge(Edge::new(
            EntityId::new("code:lib.rs::function::run"),
            EntityId::new("code:render/mod.rs::function::draw"),
            EdgeKind::Calls,
        ));
        store.add_edge(Edge::new(
            EntityId::new("code:render/mod.rs::function::draw"),
            EntityId::new("extern:serde_json::to_string"),
            EdgeKind::Calls,
        ));
        store.add_edge(Edge::new(
            EntityId::new("code:lib.rs"),
            EntityId::new("code:render/mod.rs"),
            EdgeKind::Imports,
        ));
        store
    }

    fn render_sample(opts: &GraphOptions) -> String {
        let store = sample();
        let adj = AdjMaps::build(&store);
        store.with_atoms(|atoms| render(&adj, &AtomSnapshot::build(atoms), opts))
    }

    #[test]
    fn nodes_precede_edges_and_groups_follow_directories() {
        let out = render_sample(&GraphOptions::default());
        let first_edge = out
            .lines()
            .position(|l| l.contains("-->"))
            .expect("an edge");
        let lines: Vec<&str> = out.lines().collect();
        let last_node = lines
            .iter()
            .rposition(|l| l.contains(":::") && !l.contains("classDef"))
            .expect("a node");
        assert!(last_node < first_edge, "{out}");
        assert!(out.contains("subgraph dir_render[\"render/\"]"), "{out}");
        assert!(out.contains("subgraph file_render_mod_rs"), "{out}");
        assert!(out.contains("[\"mod.rs\"]:::fileNode"), "{out}");
        assert!(out.contains("(\"run()\"):::functionNode"), "{out}");
        assert!(out.contains("--\"calls\"-->"), "{out}");
        // `lib.rs` imports `render/mod.rs`, but `run → draw` already links
        // the two files: the import would only repeat it.
        assert!(!out.contains("--\"imports\"-->"), "{out}");
        assert!(out.contains("--\"depends on\"--> dep_serde_json"), "{out}");
    }

    #[test]
    fn private_items_without_cross_file_edges_are_hidden() {
        let out = render_sample(&GraphOptions::default());
        // `draw` is private but called from another file: shown.
        assert!(out.contains("draw()"), "{out}");
        // `helper` is private and isolated: hidden unless asked for.
        assert!(!out.contains("helper()"), "{out}");
        let all = render_sample(&GraphOptions {
            include_private: true,
            ..GraphOptions::default()
        });
        assert!(all.contains("helper()"), "{all}");
    }

    #[test]
    fn max_nodes_counts_items_with_their_files() {
        // Budget 3: `run` + `lib.rs` (2), then `draw` would need
        // `render/mod.rs` too (2 more) and is cut; the dependency only
        // used by the cut file goes with it.
        let out = render_sample(&GraphOptions {
            max_nodes: 3,
            ..GraphOptions::default()
        });
        assert!(out.contains("%% truncated 1 "), "{out}");
        assert_eq!(out.matches(":::functionNode").count(), 1, "{out}");
        assert_eq!(out.matches(":::fileNode").count(), 1, "{out}");
        assert!(!out.contains("dep_serde_json"), "{out}");
    }

    #[test]
    fn imports_to_files_not_drawn_are_dropped() {
        // `render/mod.rs` imports `cache.rs`, which holds no shown item:
        // an edge to it would make Mermaid invent an undeclared node.
        let store = sample();
        store.add_atom(atom("code:cache.rs", "module", "cache", "cache.rs", ""));
        store.add_edge(Edge::new(
            EntityId::new("code:render/mod.rs"),
            EntityId::new("code:cache.rs"),
            EdgeKind::Imports,
        ));
        let adj = AdjMaps::build(&store);
        let out = store.with_atoms(|atoms| {
            render(&adj, &AtomSnapshot::build(atoms), &GraphOptions::default())
        });
        assert!(!out.contains("code_cache_rs"), "{out}");
    }

    #[test]
    fn implements_is_drawn_from_a_type_declared_in_another_file() {
        // `impl T for S` in b.rs, `S` in a.rs, `T` in t.rs; b.rs imports a.rs.
        let store = Store::new();
        for (file, kind, name, sig) in [
            ("a.rs", "struct", "S", "pub struct S"),
            ("t.rs", "trait", "T", "pub trait T"),
            ("b.rs", "impl", "T for S", "impl T for S"),
        ] {
            store.add_atom(atom(&format!("code:{file}"), "module", file, file, ""));
            store.add_atom(atom(
                &format!("code:{file}::{kind}::{name}"),
                kind,
                name,
                file,
                sig,
            ));
        }
        let mut f = atom(
            "code:t.rs::function::T::f",
            "function",
            "f",
            "t.rs",
            "fn f(&self);",
        );
        f.parent = Some("T".to_owned());
        store.add_atom(f);
        let mut g = atom(
            "code:b.rs::function::T for S::f",
            "function",
            "f",
            "b.rs",
            "fn f(&self) {}",
        );
        g.parent = Some("S".to_owned());
        store.add_atom(g);
        for (from, to, kind) in [
            (
                "code:b.rs::impl::T for S",
                "code:t.rs::trait::T",
                EdgeKind::Implements,
            ),
            (
                "code:b.rs::function::T for S::f",
                "code:t.rs::function::T::f",
                EdgeKind::Implements,
            ),
            ("code:b.rs", "code:a.rs", EdgeKind::Imports),
        ] {
            store.add_edge(Edge::new(EntityId::new(from), EntityId::new(to), kind));
        }
        let adj = AdjMaps::build(&store);
        let out = store.with_atoms(|atoms| {
            render(
                &adj,
                &AtomSnapshot::build(atoms),
                &GraphOptions {
                    include_private: true,
                    ..GraphOptions::default()
                },
            )
        });
        let s = sanitize_id("code:a.rs::struct::S");
        let t = sanitize_id("code:t.rs::trait::T");
        assert!(
            out.contains(&format!("{s} --\"implements\"--> {t}")),
            "{out}"
        );
        let tf = sanitize_id("code:t.rs::function::T::f");
        let sf = sanitize_id("code:b.rs::function::T for S::f");
        assert!(
            out.contains(&format!("{tf} --\"dispatches to\"--> {sf}")),
            "{out}"
        );
    }

    #[test]
    fn std_and_primitive_heads_are_not_dependencies() {
        assert!(is_dependency_name("serde_json"));
        assert!(!is_dependency_name("std"));
        assert!(!is_dependency_name("u32"));
        assert!(!is_dependency_name("Imports"));
    }

    #[test]
    fn edges_option_filters_kinds() {
        let out = render_sample(&GraphOptions {
            edges: vec![GraphEdge::Imports],
            ..GraphOptions::default()
        });
        assert!(out.contains("imports"), "{out}");
        assert!(!out.contains("--\"calls\"-->"), "{out}");
        assert!(!out.contains("dep_serde_json"), "{out}");
    }
}
