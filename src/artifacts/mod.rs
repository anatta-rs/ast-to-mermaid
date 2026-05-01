//! Rich 4-layer artifact emission.
//!
//! [`emit_artifacts`] walks a populated [`Store`] and produces:
//! - `overview.mmd` — top-level project Mermaid, GitHub-renderable.
//! - Per-entity `<id>.mmd` — Mermaid with `%% key: value` comments + `classDef`.
//! - Per-entity `<id>.meta.json` — full machine-readable sidecar JSON.
//! - `index.json` — global catalog: all entities + artifact paths + edges.
//!
//! # Usage
//!
//! ```
//! use ast_to_mermaid::{Store, artifacts::emit_artifacts};
//! let store = Store::new();
//! // … populate store …
//! let artifacts = emit_artifacts(&store, "/analyzed/root");
//! println!("{}", artifacts.overview_mmd);
//! ```

use crate::graph::Store;
use crate::model::{AtomKind, CodeAtom, EdgeKind, EntityId};
use crate::render::{Level, render};
use serde_json::{Value, json};
use std::collections::HashMap;

/// A per-entity artifact bundle.
pub struct EntityArtifact {
    /// Entity id.
    pub id: EntityId,
    /// Atom kind.
    pub kind: AtomKind,
    /// Per-entity mermaid diagram with `%%` header comments + `classDef`.
    pub mmd: String,
    /// Full machine-readable sidecar JSON.
    pub meta: Value,
}

/// The full artifact set produced by [`emit_artifacts`].
pub struct ArtifactSet {
    /// Global top-level Mermaid (project view).
    pub overview_mmd: String,
    /// Per-entity bundles.
    pub entities: Vec<EntityArtifact>,
    /// Global catalog JSON.
    pub index_json: Value,
}

/// Emit the 4-layer artifact set for all atoms in `store`.
///
/// `source_root` is embedded in `index.json` as metadata only.
#[must_use]
pub fn emit_artifacts(store: &Store, source_root: &str) -> ArtifactSet {
    let overview_mmd = render(Level::Project, store, None).unwrap_or_default();

    let atoms = store.all_atoms();
    let mut entities: Vec<EntityArtifact> = Vec::with_capacity(atoms.len());

    // Build a reverse-edge cache: entity_id → vec of caller ids (Calls incoming).
    let mut callers_of: HashMap<EntityId, Vec<EntityId>> = HashMap::new();
    for atom in &atoms {
        let incoming = store.call_edges_to(&atom.id);
        if !incoming.is_empty() {
            callers_of.insert(atom.id.clone(), incoming);
        }
    }

    for atom in &atoms {
        let kind = AtomKind::parse(&atom.kind);
        let outgoing = store.call_edges_from(&atom.id);
        let incoming = callers_of.get(&atom.id).cloned().unwrap_or_default();

        // Children (contained items) and parent module.
        let children = store.children_of(&atom.id);

        let mmd = entity_mmd(atom, &outgoing, &incoming);
        let meta = entity_meta(atom, &outgoing, &incoming, &children, store);

        entities.push(EntityArtifact {
            id: atom.id.clone(),
            kind,
            mmd,
            meta,
        });
    }

    // Sort entities deterministically by id.
    entities.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));

    let now = chrono_now();
    let index_json = build_index(&entities, source_root, &now);

    ArtifactSet {
        overview_mmd,
        entities,
        index_json,
    }
}

/// Write the artifact set to `out_dir`.
///
/// Creates:
/// - `<out_dir>/overview.mmd`
/// - `<out_dir>/index.json`
/// - `<out_dir>/entities/<sanitized-id>.mmd`
/// - `<out_dir>/entities/<sanitized-id>.meta.json`
///
/// # Errors
///
/// Propagates any I/O error from file creation or writing.
pub fn write_artifacts(
    artifacts: &ArtifactSet,
    out_dir: &std::path::Path,
) -> crate::error::Result<()> {
    use std::fs;
    fs::create_dir_all(out_dir)?;
    let entities_dir = out_dir.join("entities");
    fs::create_dir_all(&entities_dir)?;

    fs::write(out_dir.join("overview.mmd"), &artifacts.overview_mmd)?;
    fs::write(
        out_dir.join("index.json"),
        serde_json::to_string_pretty(&artifacts.index_json).unwrap_or_default(),
    )?;

    for entity in &artifacts.entities {
        let base = sanitize_id(entity.id.as_str());
        fs::write(entities_dir.join(format!("{base}.mmd")), &entity.mmd)?;
        fs::write(
            entities_dir.join(format!("{base}.meta.json")),
            serde_json::to_string_pretty(&entity.meta).unwrap_or_default(),
        )?;
    }

    Ok(())
}

// ── Per-entity mermaid ────────────────────────────────────────────────────────

fn entity_mmd(atom: &CodeAtom, outgoing: &[EntityId], incoming: &[EntityId]) -> String {
    use std::fmt::Write as FmtWrite;

    let mut out = String::new();
    // Header comments.
    writeln!(out, "%% id: {}", atom.id.as_str()).expect("write");
    writeln!(out, "%% kind: {}", atom.kind).expect("write");
    writeln!(
        out,
        "%% file: {}:{}-{}",
        atom.file_path, atom.line_start, atom.line_end
    )
    .expect("write");
    writeln!(out, "%% content_hash: {}", atom.content_hash).expect("write");
    if !atom.signature.is_empty() {
        writeln!(out, "%% signature: {}", atom.signature).expect("write");
    }

    writeln!(out, "graph LR").expect("write");

    // classDef for known kinds.
    let (fill, stroke) = class_colors(&atom.kind);
    writeln!(
        out,
        "  classDef {kind} fill:{fill},stroke:{stroke}",
        kind = atom.kind
    )
    .expect("write");
    // Also add classDef for connected kinds.
    writeln!(out, "  classDef function fill:#e1f5fe,stroke:#01579b").expect("write");
    writeln!(out, "  classDef module fill:#f3e5f5,stroke:#4a148c").expect("write");

    let self_id = mermaid_id_short(atom.id.as_str());
    writeln!(out, "  {self_id}:::{}[{}]", atom.kind, atom.name).expect("write");

    for callee_id in outgoing {
        let callee_name = callee_id
            .as_str()
            .rsplit("::")
            .next()
            .unwrap_or(callee_id.as_str());
        let cid = mermaid_id_short(callee_id.as_str());
        writeln!(
            out,
            "  {self_id} -- calls --> {cid}:::function[{callee_name}]"
        )
        .expect("write");
    }
    for caller_id in incoming {
        let caller_name = caller_id
            .as_str()
            .rsplit("::")
            .next()
            .unwrap_or(caller_id.as_str());
        let cid = mermaid_id_short(caller_id.as_str());
        writeln!(
            out,
            "  {cid}:::function[{caller_name}] -- calls --> {self_id}"
        )
        .expect("write");
    }

    out
}

fn mermaid_id_short(id: &str) -> String {
    // Use the last segment after `::` as the node id, sanitized.
    let short = id.rsplit("::").next().unwrap_or(id);
    short
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn class_colors(kind: &str) -> (&'static str, &'static str) {
    match kind {
        "function" => ("#e1f5fe", "#01579b"),
        "module" => ("#f3e5f5", "#4a148c"),
        "struct" => ("#e8f5e9", "#1b5e20"),
        "trait" => ("#fff8e1", "#f57f17"),
        "impl" => ("#fce4ec", "#880e4f"),
        "enum" => ("#e8eaf6", "#283593"),
        "const" | "static" => ("#fffde7", "#f9a825"),
        "macro" => ("#e0f2f1", "#004d40"),
        _ => ("#f5f5f5", "#424242"),
    }
}

// ── Per-entity meta JSON ──────────────────────────────────────────────────────

fn entity_meta(
    atom: &CodeAtom,
    outgoing: &[EntityId],
    incoming: &[EntityId],
    children: &[EntityId],
    store: &Store,
) -> Value {
    // Collect import paths (for modules: atoms that import this module's file).
    let imports: Vec<String> = store
        .edges_from(&atom.id)
        .iter()
        .filter(|e| e.kind == EdgeKind::Uses)
        .map(|e| e.to.as_str().to_owned())
        .collect();
    let imported_by: Vec<String> = store
        .edges_to(&atom.id)
        .iter()
        .filter(|e| e.kind == EdgeKind::Uses)
        .map(|e| e.from.as_str().to_owned())
        .collect();

    json!({
        "id": atom.id.as_str(),
        "kind": atom.kind,
        "name": atom.name,
        "file": atom.file_path,
        "line_start": atom.line_start,
        "line_end": atom.line_end,
        "signature": atom.signature,
        "doc": atom.doc,
        "content_hash": atom.content_hash,
        "callers": incoming.iter().map(EntityId::as_str).collect::<Vec<_>>(),
        "callees": outgoing.iter().map(EntityId::as_str).collect::<Vec<_>>(),
        "children": children.iter().map(EntityId::as_str).collect::<Vec<_>>(),
        "imports": imports,
        "imported_by": imported_by,
    })
}

// ── Index JSON ────────────────────────────────────────────────────────────────

fn build_index(entities: &[EntityArtifact], source_root: &str, generated_at: &str) -> Value {
    let entity_list: Vec<Value> = entities
        .iter()
        .map(|e| {
            let base = sanitize_id(e.id.as_str());
            // Extract callees + callers from meta.
            let out_edges: Vec<&str> = e
                .meta
                .get("callees")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            let in_edges: Vec<&str> = e
                .meta
                .get("callers")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            json!({
                "id": e.id.as_str(),
                "kind": e.kind.as_str(),
                "name": e.meta.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                "file": e.meta.get("file").and_then(|v| v.as_str()).unwrap_or(""),
                "mmd_path": format!("entities/{base}.mmd"),
                "meta_path": format!("entities/{base}.meta.json"),
                "edges": {
                    "out": out_edges,
                    "in": in_edges,
                },
            })
        })
        .collect();

    json!({
        "schema": 2,
        "entities": entity_list,
        "generated_at": generated_at,
        "source_root": source_root,
    })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Sanitize an entity id for use as a filename.
/// `[^a-zA-Z0-9._-]` → `_`.
#[must_use]
pub fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Returns an RFC 3339-ish timestamp. Falls back to a fixed string if the
/// system clock is unavailable (unusual but possible in sandboxes).
fn chrono_now() -> String {
    // Use std::time — no chrono dep. Format manually.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // Produce a minimal ISO-8601 string: YYYY-MM-DDTHH:MM:SSZ
    let s = secs;
    let sec = s % 60;
    let min = (s / 60) % 60;
    let hour = (s / 3_600) % 24;
    let days = s / 86_400;
    // Rough calendar computation (good enough for metadata).
    let year = 1970 + days / 365;
    let month = (days % 365) / 30 + 1;
    let day = (days % 365) % 30 + 1;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Store;
    use crate::model::{CodeAtom, Edge, EdgeKind, EntityId};

    fn fn_atom(id: &str, file: &str, name: &str) -> CodeAtom {
        CodeAtom {
            id: EntityId::new(id),
            kind: "function".to_owned(),
            name: name.to_owned(),
            file_path: file.to_owned(),
            line_start: 1,
            line_end: 10,
            doc: "Does something useful.".to_owned(),
            signature: format!("pub fn {name}()"),
            content_hash: "hash123".to_owned(),
            calls: Vec::new(),
        }
    }

    fn module_atom(file: &str) -> CodeAtom {
        CodeAtom {
            id: EntityId::new(format!("code:{file}")),
            kind: "module".to_owned(),
            name: "lib".to_owned(),
            file_path: file.to_owned(),
            line_start: 1,
            line_end: 50,
            doc: String::new(),
            signature: String::new(),
            content_hash: "mhash".to_owned(),
            calls: Vec::new(),
        }
    }

    #[test]
    fn emit_artifacts_produces_expected_structure() {
        let store = Store::new();
        let m = module_atom("src/lib.rs");
        let f = fn_atom("code:src/lib.rs::function::foo", "src/lib.rs", "foo");
        let g = fn_atom("code:src/lib.rs::function::bar", "src/lib.rs", "bar");
        let mid = m.id.clone();
        let fid = f.id.clone();
        let gid = g.id.clone();
        store.add_atom(m);
        store.add_atom(f);
        store.add_atom(g);
        store.add_edge(Edge::new(mid.clone(), fid.clone(), EdgeKind::Contains));
        store.add_edge(Edge::new(mid.clone(), gid.clone(), EdgeKind::Contains));
        store.add_edge(Edge::new(fid.clone(), gid.clone(), EdgeKind::Calls));

        let artifacts = emit_artifacts(&store, "/src");

        // overview must be non-empty
        assert!(!artifacts.overview_mmd.is_empty());

        // 3 entities
        assert_eq!(artifacts.entities.len(), 3);

        // index has correct entity count
        let index_entities = artifacts.index_json["entities"].as_array().expect("array");
        assert_eq!(index_entities.len(), 3);
        assert_eq!(artifacts.index_json["source_root"], "/src");

        // entity with calls edge has callee in meta
        let foo_artifact = artifacts
            .entities
            .iter()
            .find(|e| e.id.as_str() == "code:src/lib.rs::function::foo")
            .expect("foo");
        let callees = foo_artifact.meta["callees"].as_array().expect("array");
        assert_eq!(callees.len(), 1);
        assert_eq!(callees[0], "code:src/lib.rs::function::bar");

        // mmd has %% comments
        assert!(foo_artifact.mmd.contains("%% id:"));
        assert!(foo_artifact.mmd.contains("%% kind: function"));
        assert!(foo_artifact.mmd.contains("classDef function"));
    }

    #[test]
    fn sanitize_id_replaces_special_chars() {
        assert_eq!(
            sanitize_id("code:src/lib.rs::function::foo"),
            "code_src_lib.rs__function__foo"
        );
        assert_eq!(sanitize_id("abc_123-def.rs"), "abc_123-def.rs");
        assert_eq!(sanitize_id("a::b"), "a__b");
    }

    #[test]
    fn write_artifacts_creates_files() {
        let store = Store::new();
        store.add_atom(fn_atom(
            "code:src/lib.rs::function::foo",
            "src/lib.rs",
            "foo",
        ));
        let artifacts = emit_artifacts(&store, "/src");

        let tmp = tempfile::tempdir().expect("tmp");
        write_artifacts(&artifacts, tmp.path()).expect("write");

        assert!(tmp.path().join("overview.mmd").exists());
        assert!(tmp.path().join("index.json").exists());
        // entities dir exists
        assert!(tmp.path().join("entities").is_dir());
    }
}
