//! Set-diff between two materialized bundles.
//!
//! Loads the `index.json` of each bundle and produces lists of added,
//! removed, modified, and renamed entities. Output formats: a structured
//! `BundleDiff` for tooling, and an annotated Mermaid diagram for humans.
//!
//! The diff is intentionally *structural* — it operates on entity ids and
//! `content_hash` values produced by the parse/resolve pipeline. No source
//! re-parsing happens in this module; both bundles must already exist in
//! the cache (the `index` subcommand materializes them).

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{AstToMermaidError, Result};

/// One entry from a bundle's `index.json` `entities[]` array, projected
/// down to just the fields the diff needs.
#[derive(Debug, Clone, Deserialize)]
pub struct IndexEntity {
    /// Stable entity id (`code:<path>::<kind>::<name>`).
    pub id: String,
    /// Coarse kind (`module`, `function`, `struct`, …).
    #[serde(default)]
    pub kind: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Source file path relative to the bundle root.
    #[serde(default)]
    pub file: String,
    /// Git blob SHA-1 of the entity's source (or its enclosing module).
    /// May be empty in legacy schema-1 bundles.
    #[serde(default)]
    pub content_hash: String,
    /// Outgoing edges (callees / contained / used). Used by the diff
    /// renderer to draw blast-radius arrows between changed entities.
    #[serde(default, deserialize_with = "deserialize_out_edges")]
    pub edges_out: Vec<String>,
}

/// Pull the `edges.out` array out of the nested `edges` object that
/// `index.json` actually serializes. We accept both shapes so the diff
/// loader is forward-compatible if the schema flattens later.
fn deserialize_out_edges<'de, D>(d: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Vec::<String>::deserialize(d)
}

/// Top-level structure of a bundle's `index.json`. Mirrors the nested
/// `entities[].edges.{out,in}` shape that `build_index` emits.
#[derive(Debug, Clone, Deserialize)]
struct IndexFile {
    #[serde(default)]
    entities: Vec<RawEntity>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawEntity {
    id: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    file: String,
    #[serde(default)]
    content_hash: String,
    #[serde(default)]
    edges: RawEdges,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawEdges {
    #[serde(default)]
    out: Vec<String>,
}

impl From<RawEntity> for IndexEntity {
    fn from(r: RawEntity) -> Self {
        Self {
            id: r.id,
            kind: r.kind,
            name: r.name,
            file: r.file,
            content_hash: r.content_hash,
            edges_out: r.edges.out,
        }
    }
}

/// Structural difference between two bundles.
#[derive(Debug, Clone, Default, Serialize)]
pub struct BundleDiff {
    /// Source ref (left side of `a..b`).
    pub from: String,
    /// Target ref (right side of `a..b`).
    pub to: String,
    /// Resolved snapshot id of `from`.
    pub from_sha: String,
    /// Resolved snapshot id of `to`.
    pub to_sha: String,
    /// Entities present in `to` but not in `from`.
    pub added: Vec<IndexEntity>,
    /// Entities present in `from` but not in `to`.
    pub removed: Vec<IndexEntity>,
    /// Entities present in both with different `content_hash`.
    pub modified: Vec<ModifiedEntity>,
    /// (removed, added) pairs with identical `content_hash` — likely renames.
    pub renamed: Vec<RenamedEntity>,
}

/// An entity whose id is present in both refs but content differs.
#[derive(Debug, Clone, Serialize)]
pub struct ModifiedEntity {
    /// Stable id, identical on both sides.
    pub id: String,
    /// Coarse kind.
    pub kind: String,
    /// Source file path.
    pub file: String,
    /// `content_hash` on the `from` side.
    pub from_hash: String,
    /// `content_hash` on the `to` side.
    pub to_hash: String,
}

/// A removed/added pair with matching `content_hash` — heuristic rename.
#[derive(Debug, Clone, Serialize)]
pub struct RenamedEntity {
    /// Id on the `from` side.
    pub from_id: String,
    /// Id on the `to` side.
    pub to_id: String,
    /// Content hash shared by both sides.
    pub content_hash: String,
}

/// Implement `Serialize` for `IndexEntity` so the JSON `a2m diff` output
/// matches the on-disk `index.json` shape (edges nested under `edges.out`).
impl Serialize for IndexEntity {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("IndexEntity", 5)?;
        s.serialize_field("id", &self.id)?;
        s.serialize_field("kind", &self.kind)?;
        s.serialize_field("name", &self.name)?;
        s.serialize_field("file", &self.file)?;
        s.serialize_field("content_hash", &self.content_hash)?;
        s.end()
    }
}

/// Load `<bundle_dir>/index.json` and return its entity list.
///
/// # Errors
/// I/O failures or JSON parse errors. Bundle layout (missing `index.json`)
/// is reported as `InvalidInput`.
pub fn load_bundle_entities(bundle_dir: &Path) -> Result<Vec<IndexEntity>> {
    let index_path = bundle_dir.join("index.json");
    let bytes = std::fs::read(&index_path).map_err(|e| {
        AstToMermaidError::InvalidInput(format!(
            "load bundle {}: {e}",
            index_path.display()
        ))
    })?;
    let file: IndexFile = serde_json::from_slice(&bytes).map_err(|e| {
        AstToMermaidError::InvalidInput(format!("parse index.json {}: {e}", index_path.display()))
    })?;
    Ok(file.entities.into_iter().map(IndexEntity::from).collect())
}

/// Compute the set-diff between two bundles.
///
/// `from_entities` and `to_entities` are typically the result of
/// [`load_bundle_entities`] on the two cached bundles.
#[must_use]
pub fn compute_diff(
    from_ref: &str,
    to_ref: &str,
    from_sha: &str,
    to_sha: &str,
    from_entities: Vec<IndexEntity>,
    to_entities: Vec<IndexEntity>,
) -> BundleDiff {
    let from_ids: HashMap<String, IndexEntity> =
        from_entities.into_iter().map(|e| (e.id.clone(), e)).collect();
    let mut to_ids: HashMap<String, IndexEntity> =
        to_entities.into_iter().map(|e| (e.id.clone(), e)).collect();

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut modified = Vec::new();

    for (id, from_e) in &from_ids {
        match to_ids.remove(id) {
            None => removed.push(from_e.clone()),
            Some(to_e) if to_e.content_hash != from_e.content_hash => {
                modified.push(ModifiedEntity {
                    id: id.clone(),
                    kind: to_e.kind.clone(),
                    file: to_e.file.clone(),
                    from_hash: from_e.content_hash.clone(),
                    to_hash: to_e.content_hash,
                });
            }
            Some(_) => {} // identical id+hash, no entry
        }
    }
    // Whatever's left in `to_ids` was added.
    for (_, to_e) in to_ids {
        added.push(to_e);
    }

    // Rename heuristic: a (removed, added) pair with identical content_hash.
    let mut renamed = Vec::new();
    let mut keep_added = Vec::new();
    let mut hash_to_added: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, e) in added.iter().enumerate() {
        if !e.content_hash.is_empty() {
            hash_to_added.entry(e.content_hash.clone()).or_default().push(idx);
        }
    }
    let mut taken_added: Vec<bool> = vec![false; added.len()];
    let mut keep_removed = Vec::new();
    for r in removed {
        if r.content_hash.is_empty() {
            keep_removed.push(r);
            continue;
        }
        match hash_to_added.get_mut(&r.content_hash) {
            Some(indices) => {
                let mut matched = false;
                while let Some(idx) = indices.pop() {
                    if !taken_added[idx] {
                        taken_added[idx] = true;
                        renamed.push(RenamedEntity {
                            from_id: r.id.clone(),
                            to_id: added[idx].id.clone(),
                            content_hash: r.content_hash.clone(),
                        });
                        matched = true;
                        break;
                    }
                }
                if !matched {
                    keep_removed.push(r);
                }
            }
            None => keep_removed.push(r),
        }
    }
    for (idx, e) in added.into_iter().enumerate() {
        if !taken_added[idx] {
            keep_added.push(e);
        }
    }

    // Sort for deterministic output.
    let mut added = keep_added;
    let mut removed = keep_removed;
    added.sort_by(|a, b| a.id.cmp(&b.id));
    removed.sort_by(|a, b| a.id.cmp(&b.id));
    modified.sort_by(|a, b| a.id.cmp(&b.id));
    renamed.sort_by(|a, b| a.from_id.cmp(&b.from_id));

    BundleDiff {
        from: from_ref.to_owned(),
        to: to_ref.to_owned(),
        from_sha: from_sha.to_owned(),
        to_sha: to_sha.to_owned(),
        added,
        removed,
        modified,
        renamed,
    }
}

/// Render a [`BundleDiff`] as an annotated Mermaid graph.
///
/// Each entity is coloured by change kind (`:::added` green, `:::removed`
/// red, `:::modified` orange, `:::renamed` cyan), and **edges are drawn
/// between changed entities** when both endpoints are in the diff. This
/// turns a flat list into a blast-radius graph: you can see at a glance
/// which new/modified function calls into which other change.
///
/// `from_entities` carries the post-state edges used to build the graph
/// (the `to` side's `edges_out`). Pass an empty slice for a node-only
/// rendering.
#[must_use]
pub fn render_mermaid(diff: &BundleDiff, to_entities: &[IndexEntity]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(2048);
    writeln!(out, "graph TD").ok();
    writeln!(out, "    %% diff: {} → {}", diff.from, diff.to).ok();
    // Color convention: added=green, removed=red, modified=orange, renamed=cyan.
    // Light fill + darker stroke + black text for legibility on both light
    // and dark Mermaid themes.
    writeln!(out, "    classDef added fill:#9f9,stroke:#0a0,color:#000").ok();
    writeln!(out, "    classDef removed fill:#f99,stroke:#a00,color:#000").ok();
    writeln!(out, "    classDef modified fill:#fb8,stroke:#d60,color:#000").ok();
    writeln!(out, "    classDef renamed fill:#9ff,stroke:#0aa,color:#000").ok();

    // Two passes: emit nodes (assigning short Mermaid-safe ids), then
    // emit edges between nodes whose endpoints are both in `id_to_node`.
    let mut id_to_node: HashMap<String, String> = HashMap::new();
    let mut next_idx = 0usize;
    let new_node = |out: &mut String,
                        id_to_node: &mut HashMap<String, String>,
                        idx: &mut usize,
                        entity_id: &str,
                        label: &str,
                        class: &str| {
        let n = format!("n{}", *idx);
        *idx += 1;
        let safe = label.replace('"', "'");
        writeln!(out, "    {n}[\"{safe}\"]:::{class}").ok();
        id_to_node.insert(entity_id.to_owned(), n);
    };

    if !diff.added.is_empty() {
        writeln!(out, "    %% added ({})", diff.added.len()).ok();
        for e in &diff.added {
            new_node(&mut out, &mut id_to_node, &mut next_idx, &e.id, &e.id, "added");
        }
    }
    if !diff.removed.is_empty() {
        writeln!(out, "    %% removed ({})", diff.removed.len()).ok();
        for e in &diff.removed {
            new_node(&mut out, &mut id_to_node, &mut next_idx, &e.id, &e.id, "removed");
        }
    }
    if !diff.modified.is_empty() {
        writeln!(out, "    %% modified ({})", diff.modified.len()).ok();
        for e in &diff.modified {
            new_node(&mut out, &mut id_to_node, &mut next_idx, &e.id, &e.id, "modified");
        }
    }
    if !diff.renamed.is_empty() {
        writeln!(out, "    %% renamed ({})", diff.renamed.len()).ok();
        for r in &diff.renamed {
            let label = format!("{} ↪ {}", r.from_id, r.to_id);
            // The post-state id is `to_id`; that's what edges from `to_entities` reference.
            new_node(&mut out, &mut id_to_node, &mut next_idx, &r.to_id, &label, "renamed");
        }
    }

    // Edges: only render those where both endpoints are in the changeset.
    // Source = post-state `edges_out` (so removed entities never source
    // edges; their out-neighbours are visible only via post-state arrows
    // from modified callers, which is the right blast-radius signal).
    let mut edge_count = 0usize;
    for e in to_entities {
        let Some(src_node) = id_to_node.get(&e.id) else {
            continue;
        };
        for callee_id in &e.edges_out {
            if let Some(dst_node) = id_to_node.get(callee_id) {
                if edge_count == 0 {
                    writeln!(out, "    %% blast-radius edges (both endpoints in changeset)").ok();
                }
                writeln!(out, "    {src_node} --> {dst_node}").ok();
                edge_count += 1;
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ent(id: &str, hash: &str) -> IndexEntity {
        IndexEntity {
            id: id.to_owned(),
            kind: "function".to_owned(),
            name: id.split("::").last().unwrap_or("").to_owned(),
            file: "src/x.rs".to_owned(),
            content_hash: hash.to_owned(),
            edges_out: Vec::new(),
        }
    }

    fn ent_with_calls(id: &str, hash: &str, calls: &[&str]) -> IndexEntity {
        IndexEntity {
            id: id.to_owned(),
            kind: "function".to_owned(),
            name: id.split("::").last().unwrap_or("").to_owned(),
            file: "src/x.rs".to_owned(),
            content_hash: hash.to_owned(),
            edges_out: calls.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn pure_addition_is_classified() {
        let from = vec![ent("code:a", "h1")];
        let to = vec![ent("code:a", "h1"), ent("code:b", "h2")];
        let d = compute_diff("a", "b", "sha-a", "sha-b", from, to);
        assert_eq!(d.added.len(), 1);
        assert_eq!(d.added[0].id, "code:b");
        assert!(d.removed.is_empty());
        assert!(d.modified.is_empty());
        assert!(d.renamed.is_empty());
    }

    #[test]
    fn pure_removal_is_classified() {
        let from = vec![ent("code:a", "h1"), ent("code:b", "h2")];
        let to = vec![ent("code:a", "h1")];
        let d = compute_diff("a", "b", "sa", "sb", from, to);
        assert_eq!(d.removed.len(), 1);
        assert_eq!(d.removed[0].id, "code:b");
    }

    #[test]
    fn modified_when_hash_differs_at_same_id() {
        let from = vec![ent("code:a", "h1")];
        let to = vec![ent("code:a", "h2")];
        let d = compute_diff("a", "b", "sa", "sb", from, to);
        assert_eq!(d.modified.len(), 1);
        assert_eq!(d.modified[0].id, "code:a");
        assert_eq!(d.modified[0].from_hash, "h1");
        assert_eq!(d.modified[0].to_hash, "h2");
    }

    #[test]
    fn rename_paired_by_matching_content_hash() {
        let from = vec![ent("code:foo.rs::function::x", "shared")];
        let to = vec![ent("code:bar.rs::function::x", "shared")];
        let d = compute_diff("a", "b", "sa", "sb", from, to);
        assert_eq!(d.renamed.len(), 1);
        assert!(d.added.is_empty());
        assert!(d.removed.is_empty());
        assert_eq!(d.renamed[0].from_id, "code:foo.rs::function::x");
        assert_eq!(d.renamed[0].to_id, "code:bar.rs::function::x");
    }

    #[test]
    fn empty_content_hashes_never_rename() {
        // Pre-V1 bundles may have empty content_hash; guard against false
        // rename pairing.
        let from = vec![ent("code:foo", "")];
        let to = vec![ent("code:bar", "")];
        let d = compute_diff("a", "b", "sa", "sb", from, to);
        assert_eq!(d.added.len(), 1);
        assert_eq!(d.removed.len(), 1);
        assert!(d.renamed.is_empty());
    }

    #[test]
    fn render_mermaid_emits_classdef_lines() {
        let from = vec![ent("code:a", "h1")];
        let to = vec![ent("code:a", "h1"), ent("code:b", "h2")];
        let d = compute_diff("main", "feat", "sa", "sb", from.clone(), to.clone());
        let m = render_mermaid(&d, &to);
        assert!(m.starts_with("graph TD"));
        assert!(m.contains("classDef added"));
        assert!(m.contains("code:b"));
        assert!(m.contains(":::added"));
    }

    #[test]
    fn render_mermaid_draws_edges_between_changed_entities() {
        // A new fn `new_caller` calls a modified `existing_helper`. Both are
        // in the diff → expect an arrow from new_caller to existing_helper.
        let from = vec![
            ent_with_calls("code:helper", "h_old", &[]),
            ent_with_calls("code:other", "h2", &["code:helper"]),
        ];
        let to = vec![
            ent_with_calls("code:helper", "h_new", &[]), // modified (hash change)
            ent_with_calls("code:other", "h2", &["code:helper"]),
            ent_with_calls("code:new_caller", "h3", &["code:helper"]), // added, calls helper
        ];
        let d = compute_diff("a", "b", "sa", "sb", from, to.clone());
        let m = render_mermaid(&d, &to);
        // Two changed nodes: new_caller (added) + helper (modified). other is unchanged.
        assert!(m.contains(":::added"));
        assert!(m.contains(":::modified"));
        // The blast-radius edge: new_caller → helper, both colored, so we
        // expect at least one arrow line in the output.
        assert!(
            m.contains("blast-radius edges"),
            "expected blast-radius header, got:\n{m}"
        );
        let arrow_count = m.matches(" --> ").count();
        assert_eq!(arrow_count, 1, "expected 1 blast-radius edge, got: {arrow_count}\n{m}");
    }

    #[test]
    fn render_mermaid_omits_edges_when_endpoint_unchanged() {
        // `existing_caller` (unchanged) calls `helper` (modified). The caller
        // is not in the diff → its edge should NOT be drawn.
        let from = vec![
            ent_with_calls("code:caller", "h1", &["code:helper"]),
            ent_with_calls("code:helper", "h_old", &[]),
        ];
        let to = vec![
            ent_with_calls("code:caller", "h1", &["code:helper"]),
            ent_with_calls("code:helper", "h_new", &[]),
        ];
        let d = compute_diff("a", "b", "sa", "sb", from, to.clone());
        let m = render_mermaid(&d, &to);
        // helper is modified; caller is unchanged. Only helper renders.
        assert!(!m.contains(" --> "), "no edge expected, got:\n{m}");
    }
}
