//! Rich 5-layer artifact emission.
//!
//! [`emit_artifacts`] walks a populated [`Store`] and produces:
//! - `overview.mmd` — top-level project Mermaid, GitHub-renderable.
//! - Per-entity `<id>.mmd` — Mermaid with `%% key: value` comments + `classDef`.
//! - Per-entity `<id>.meta.json` — full machine-readable sidecar JSON.
//! - `index.json` — global catalog: all entities + artifact paths + edges.
//! - Per-function `sequences/<id>.mmd` — opt-in `sequenceDiagram` of the
//!   function body (built only when the caller passes sequence sources via
//!   [`emit_artifacts_with_sequences`]; absent by default).
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
use crate::sequence;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

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

/// One per-function `sequenceDiagram` artifact.
///
/// `entity_id` matches a function [`CodeAtom`] in the store; `mmd` is the
/// rendered Mermaid `sequenceDiagram` source. Only emitted for functions
/// whose body produced at least one step — empty bodies are skipped.
pub struct SequenceArtifact {
    /// Entity id of the function this sequence belongs to.
    pub entity_id: EntityId,
    /// Rendered Mermaid `sequenceDiagram` text.
    pub mmd: String,
}

/// The full artifact set produced by [`emit_artifacts`].
pub struct ArtifactSet {
    /// Global top-level Mermaid (project view).
    pub overview_mmd: String,
    /// Per-entity bundles.
    pub entities: Vec<EntityArtifact>,
    /// Global catalog JSON.
    pub index_json: Value,
    /// Per-function `sequenceDiagram` artifacts. Empty unless the caller
    /// went through [`emit_artifacts_with_sequences`].
    pub sequences: Vec<SequenceArtifact>,
}

/// Emit the artifact set for all atoms in `store`. No sequences.
///
/// `source_root` is embedded in `index.json` as metadata only.
#[must_use]
pub fn emit_artifacts(store: &Store, source_root: &str) -> ArtifactSet {
    emit_artifacts_with_sequences(store, source_root, &[])
}

/// Like [`emit_artifacts`], but also extracts a `sequenceDiagram` for every
/// function whose body has at least one step.
///
/// `sequence_sources` is a slice of `(file_display_path, content_bytes)`
/// pairs covering the Rust files in the project — typically threaded
/// through from [`crate::pipeline::bundle`] so the file contents are not
/// re-read from disk. Only functions whose `file_path` matches one of
/// those entries are eligible.
#[must_use]
pub fn emit_artifacts_with_sequences(
    store: &Store,
    source_root: &str,
    sequence_sources: &[(String, Vec<u8>)],
) -> ArtifactSet {
    let overview_mmd = render(Level::Project, store, None).unwrap_or_default();

    let atoms = store.all_atoms();
    let mut entities: Vec<EntityArtifact> = Vec::with_capacity(atoms.len());

    let adj = build_adjacency_maps(store);
    let empty: Vec<EntityId> = Vec::new();

    for atom in &atoms {
        let kind = AtomKind::parse(&atom.kind);
        let outgoing = adj.callees.get(&atom.id).unwrap_or(&empty);
        let incoming = adj.callers.get(&atom.id).unwrap_or(&empty);
        let children = adj.children.get(&atom.id).unwrap_or(&empty);

        let mmd = entity_mmd(atom, outgoing, incoming);
        let meta = entity_meta(atom, outgoing, incoming, children, &adj);

        entities.push(EntityArtifact {
            id: atom.id.clone(),
            kind,
            mmd,
            meta,
        });
    }

    // Sort entities deterministically by id.
    entities.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));

    let sequences = build_sequences(&entities, sequence_sources);
    let sequence_ids: HashSet<&str> = sequences.iter().map(|s| s.entity_id.as_str()).collect();

    let now = chrono_now();
    let index_json = build_index(&entities, source_root, &now, &sequence_ids);

    ArtifactSet {
        overview_mmd,
        entities,
        index_json,
        sequences,
    }
}

/// Extract `sequenceDiagram` artifacts for every function entity whose
/// `file_path` resolves in `sequence_sources` and whose body has at
/// least one step.
///
/// Each source file is parsed exactly once: we group function entities by
/// file, drive [`sequence::parse_source_once`] + [`sequence::extract_all`]
/// once per file, then walk entities in their original order to preserve
/// the output Vec layout (callers depend on it for stable on-disk
/// ordering).
fn build_sequences(
    entities: &[EntityArtifact],
    sequence_sources: &[(String, Vec<u8>)],
) -> Vec<SequenceArtifact> {
    if sequence_sources.is_empty() {
        return Vec::new();
    }
    let by_path: HashMap<&str, &[u8]> = sequence_sources
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_slice()))
        .collect();

    // Pass 1: collect target function names per file (preserving original
    // entity order so the resulting maps are independent of HashMap
    // iteration order).
    let mut targets_by_file: HashMap<&str, Vec<String>> = HashMap::new();
    for entity in entities {
        if entity.kind.as_str() != "function" {
            continue;
        }
        let Some(file) = entity.meta.get("file").and_then(Value::as_str) else {
            continue;
        };
        // `file` looks like `src/foo.rs:10-42` — split on the trailing
        // `:line-line` we appended in `entity_meta`.
        let file_path = file.rsplit_once(':').map_or(file, |(p, _)| p);
        if !by_path.contains_key(file_path) {
            continue;
        }
        let Some(qualified) = qualified_fn_name(entity.id.as_str()) else {
            continue;
        };
        targets_by_file
            .entry(file_path)
            .or_default()
            .push(qualified);
    }

    // Pass 2: parse each file exactly once and bulk-extract every target.
    let mut sequences_by_file: HashMap<&str, sequence::SequenceMap> = HashMap::new();
    for (file_path, targets) in &targets_by_file {
        let content = by_path[*file_path];
        let Ok(text) = std::str::from_utf8(content) else {
            continue;
        };
        let Ok(tree) = sequence::parse_source_once(content, file_path) else {
            continue;
        };
        let target_refs: Vec<&str> = targets.iter().map(String::as_str).collect();
        sequences_by_file.insert(file_path, sequence::extract_all(&tree, text, &target_refs));
    }

    // Pass 3: assemble artifacts in original entity order, looking up
    // each diagram in the per-file map.
    let mut out = Vec::new();
    for entity in entities {
        if entity.kind.as_str() != "function" {
            continue;
        }
        let Some(file) = entity.meta.get("file").and_then(Value::as_str) else {
            continue;
        };
        let file_path = file.rsplit_once(':').map_or(file, |(p, _)| p);
        let Some(map) = sequences_by_file.get(file_path) else {
            continue;
        };
        let Some(qualified) = qualified_fn_name(entity.id.as_str()) else {
            continue;
        };
        let Some(diagram) = map.get(&qualified) else {
            continue;
        };
        if diagram.steps.is_empty() {
            continue;
        }
        out.push(SequenceArtifact {
            entity_id: entity.id.clone(),
            mmd: sequence::render(diagram),
        });
    }
    out
}

/// Recover the `name` / `Type::method` form expected by
/// [`sequence::extract`] from a function-atom id like
/// `code:src/foo.rs::function::Foo::bar` → `Foo::bar`.
fn qualified_fn_name(id: &str) -> Option<String> {
    id.split_once("::function::").map(|(_, q)| q.to_owned())
}

/// Write the artifact set to `out_dir`, reconciling against any existing
/// contents.
///
/// Behavior is incremental: files whose bytes already match the new artifact
/// are left untouched (mtimes preserved), and entity / sequence files for ids
/// no longer present in `artifacts` are deleted. The top-level `index.json`
/// is compared modulo its `generated_at` field — a run that produces the
/// same entity set keeps the previous timestamp on disk.
///
/// Layout:
/// - `<out_dir>/overview.mmd`
/// - `<out_dir>/index.json`
/// - `<out_dir>/entities/<sanitized-id>.mmd`
/// - `<out_dir>/entities/<sanitized-id>.meta.json`
/// - `<out_dir>/sequences/<sanitized-id>.mmd` (one per `SequenceArtifact`,
///   only when [`ArtifactSet::sequences`] is non-empty)
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

    let mut keep_entity_basenames: HashSet<String> = HashSet::new();
    for entity in &artifacts.entities {
        let base = filename_id(entity.id.as_str());
        keep_entity_basenames.insert(base.clone());
        write_if_changed(
            &entities_dir.join(format!("{base}.mmd")),
            entity.mmd.as_bytes(),
        )?;
        write_if_changed(
            &entities_dir.join(format!("{base}.meta.json")),
            serde_json::to_string_pretty(&entity.meta)
                .unwrap_or_default()
                .as_bytes(),
        )?;
    }

    let sequences_dir = out_dir.join("sequences");
    let mut keep_seq_basenames: HashSet<String> = HashSet::new();
    if !artifacts.sequences.is_empty() {
        fs::create_dir_all(&sequences_dir)?;
        for seq in &artifacts.sequences {
            let base = filename_id(seq.entity_id.as_str());
            keep_seq_basenames.insert(base.clone());
            write_if_changed(
                &sequences_dir.join(format!("{base}.mmd")),
                seq.mmd.as_bytes(),
            )?;
        }
    }

    prune_orphans(
        &entities_dir,
        &keep_entity_basenames,
        &[".meta.json", ".mmd"],
    )?;
    prune_orphans(&sequences_dir, &keep_seq_basenames, &[".mmd"])?;

    write_if_changed(
        &out_dir.join("overview.mmd"),
        artifacts.overview_mmd.as_bytes(),
    )?;
    write_index_json(&out_dir.join("index.json"), &artifacts.index_json)?;

    Ok(())
}

/// Write `contents` to `path` only if the on-disk bytes differ. Returns
/// `Ok(true)` if the file was (re)written, `Ok(false)` if it was identical
/// and left alone.
fn write_if_changed(path: &std::path::Path, contents: &[u8]) -> crate::error::Result<bool> {
    if let Ok(existing) = std::fs::read(path)
        && existing == contents
    {
        return Ok(false);
    }
    std::fs::write(path, contents)?;
    Ok(true)
}

/// Write `index.json`, preserving the on-disk `generated_at` when the rest
/// of the document is structurally unchanged. Keeps the timestamp meaningful
/// (= last data change) instead of "last invocation".
fn write_index_json(path: &std::path::Path, new_value: &Value) -> crate::error::Result<()> {
    if let Ok(existing_bytes) = std::fs::read(path)
        && let Ok(existing_value) = serde_json::from_slice::<Value>(&existing_bytes)
        && structurally_equal_modulo_generated_at(&existing_value, new_value)
    {
        return Ok(());
    }
    let pretty = serde_json::to_string_pretty(new_value).unwrap_or_default();
    std::fs::write(path, pretty)?;
    Ok(())
}

fn structurally_equal_modulo_generated_at(a: &Value, b: &Value) -> bool {
    let mut a = a.clone();
    let mut b = b.clone();
    if let Value::Object(m) = &mut a {
        m.remove("generated_at");
    }
    if let Value::Object(m) = &mut b {
        m.remove("generated_at");
    }
    a == b
}

/// Delete any file in `dir` whose name ends with one of `suffixes` and whose
/// pre-suffix basename is not in `keep`. No-ops when `dir` doesn't exist.
fn prune_orphans(
    dir: &std::path::Path,
    keep: &HashSet<String>,
    suffixes: &[&str],
) -> crate::error::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        for suffix in suffixes {
            if let Some(base) = name_str.strip_suffix(suffix) {
                if !keep.contains(base) {
                    let _ = std::fs::remove_file(entry.path());
                }
                break;
            }
        }
    }
    Ok(())
}

// ── Per-entity mermaid ────────────────────────────────────────────────────────

fn entity_mmd(atom: &CodeAtom, outgoing: &[EntityId], incoming: &[EntityId]) -> String {
    use crate::render::util::escape_label;
    use std::fmt::Write as FmtWrite;

    // Single-line, comment-safe text for `%% ...` headers — no newlines,
    // no leading `%` that would re-open a comment, just collapse them to
    // spaces.
    let one_line = |s: &str| -> String { s.replace(['\n', '\r'], " ").trim().to_owned() };

    let mut out = String::new();
    // Header comments.
    writeln!(out, "%% id: {}", one_line(atom.id.as_str())).expect("string write is infallible");
    writeln!(out, "%% kind: {}", one_line(&atom.kind)).expect("string write is infallible");
    writeln!(
        out,
        "%% file: {}:{}-{}",
        one_line(&atom.file_path),
        atom.line_start,
        atom.line_end
    )
    .expect("string write is infallible");
    writeln!(out, "%% content_hash: {}", one_line(&atom.content_hash))
        .expect("string write is infallible");
    if !atom.signature.is_empty() {
        writeln!(out, "%% signature: {}", one_line(&atom.signature))
            .expect("string write is infallible");
    }

    writeln!(out, "graph LR").expect("string write is infallible");

    // classDef for known kinds.
    let (fill, stroke) = class_colors(&atom.kind);
    writeln!(
        out,
        "  classDef {kind} fill:{fill},stroke:{stroke}",
        kind = atom.kind
    )
    .expect("string write is infallible");
    // Also add classDef for connected kinds.
    writeln!(out, "  classDef function fill:#e1f5fe,stroke:#01579b")
        .expect("string write is infallible");
    writeln!(out, "  classDef module fill:#f3e5f5,stroke:#4a148c")
        .expect("string write is infallible");

    let self_id = mermaid_id_short(atom.id.as_str());
    let self_label = escape_label(&atom.name);
    writeln!(out, "  {self_id}:::{}[{}]", atom.kind, self_label)
        .expect("string write is infallible");

    for callee_id in outgoing {
        let callee_name = callee_id
            .as_str()
            .rsplit("::")
            .next()
            .unwrap_or(callee_id.as_str());
        let label = escape_label(callee_name);
        let cid = mermaid_id_short(callee_id.as_str());
        writeln!(out, "  {self_id} -- calls --> {cid}:::function[{label}]")
            .expect("string write is infallible");
    }
    for caller_id in incoming {
        let caller_name = caller_id
            .as_str()
            .rsplit("::")
            .next()
            .unwrap_or(caller_id.as_str());
        let label = escape_label(caller_name);
        let cid = mermaid_id_short(caller_id.as_str());
        writeln!(out, "  {cid}:::function[{label}] -- calls --> {self_id}")
            .expect("string write is infallible");
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

// ── Adjacency maps ────────────────────────────────────────────────────────────

/// Five buckets of pre-computed adjacency, built once per
/// [`emit_artifacts`] invocation by [`build_adjacency_maps`]. Replaces
/// the per-atom O(E) edge filters that previously dominated bundle build
/// time.
///
/// `callers` and `callees` are the reverse / forward `Calls` map;
/// `children` is the forward `Contains` map; `uses_out` / `uses_in` and
/// `implements_out` / `implements_in` are the forward / reverse maps for
/// the two remaining edge kinds.
struct AdjMaps {
    callers: HashMap<EntityId, Vec<EntityId>>,
    callees: HashMap<EntityId, Vec<EntityId>>,
    children: HashMap<EntityId, Vec<EntityId>>,
    uses_out: HashMap<EntityId, Vec<EntityId>>,
    uses_in: HashMap<EntityId, Vec<EntityId>>,
    implements_out: HashMap<EntityId, Vec<EntityId>>,
    implements_in: HashMap<EntityId, Vec<EntityId>>,
}

/// Single sweep over `store.all_edges()` that yields the five logical
/// adjacency buckets consumed by [`emit_artifacts`] and [`entity_meta`].
fn build_adjacency_maps(store: &Store) -> AdjMaps {
    let mut maps = AdjMaps {
        callers: HashMap::new(),
        callees: HashMap::new(),
        children: HashMap::new(),
        uses_out: HashMap::new(),
        uses_in: HashMap::new(),
        implements_out: HashMap::new(),
        implements_in: HashMap::new(),
    };
    for edge in store.all_edges() {
        match edge.kind {
            EdgeKind::Calls => {
                maps.callees
                    .entry(edge.from.clone())
                    .or_default()
                    .push(edge.to.clone());
                maps.callers.entry(edge.to).or_default().push(edge.from);
            }
            EdgeKind::Contains => {
                maps.children.entry(edge.from).or_default().push(edge.to);
            }
            EdgeKind::Uses => {
                maps.uses_out
                    .entry(edge.from.clone())
                    .or_default()
                    .push(edge.to.clone());
                maps.uses_in.entry(edge.to).or_default().push(edge.from);
            }
            EdgeKind::Implements => {
                maps.implements_out
                    .entry(edge.from.clone())
                    .or_default()
                    .push(edge.to.clone());
                maps.implements_in
                    .entry(edge.to)
                    .or_default()
                    .push(edge.from);
            }
        }
    }
    maps
}

// ── Per-entity meta JSON ──────────────────────────────────────────────────────

fn entity_meta(
    atom: &CodeAtom,
    outgoing: &[EntityId],
    incoming: &[EntityId],
    children: &[EntityId],
    adj: &AdjMaps,
) -> Value {
    let to_strings = |ids: Option<&Vec<EntityId>>| -> Vec<String> {
        ids.map(|v| v.iter().map(|e| e.as_str().to_owned()).collect())
            .unwrap_or_default()
    };
    let imports = to_strings(adj.uses_out.get(&atom.id));
    let imported_by = to_strings(adj.uses_in.get(&atom.id));
    let implements = to_strings(adj.implements_out.get(&atom.id));
    let implemented_by = to_strings(adj.implements_in.get(&atom.id));

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
        "implements": implements,
        "implemented_by": implemented_by,
    })
}

// ── Index JSON ────────────────────────────────────────────────────────────────

fn build_index(
    entities: &[EntityArtifact],
    source_root: &str,
    generated_at: &str,
    sequence_ids: &HashSet<&str>,
) -> Value {
    let entity_list: Vec<Value> = entities
        .iter()
        .map(|e| {
            let base = filename_id(e.id.as_str());
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
            let mut entry = json!({
                "id": e.id.as_str(),
                "kind": e.kind.as_str(),
                "name": e.meta.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                "file": e.meta.get("file").and_then(|v| v.as_str()).unwrap_or(""),
                "content_hash": e.meta.get("content_hash").and_then(|v| v.as_str()).unwrap_or(""),
                "mmd_path": format!("entities/{base}.mmd"),
                "meta_path": format!("entities/{base}.meta.json"),
                "edges": {
                    "out": out_edges,
                    "in": in_edges,
                },
            });
            if sequence_ids.contains(e.id.as_str())
                && let Value::Object(map) = &mut entry
            {
                map.insert(
                    "sequence_path".to_owned(),
                    Value::String(format!("sequences/{base}.mmd")),
                );
            }
            entry
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
///
/// Conceptually distinct from [`crate::render::util::sanitize_id`]: that
/// function targets Mermaid node IDs (alphanumeric+`_`, with reserved-word
/// and digit-leading guards), whereas this one targets filesystem paths.
/// Filenames need to keep `.` (extensions) and `-` (idiomatic in repo
/// paths), so the allowed set is wider; we don't apply Mermaid's
/// keyword/digit guards because they're meaningless on disk.
///
/// When the input contains ASCII uppercase letters, the result is
/// lowercased and an `_H<hash>` suffix is appended so `Foo`, `foo`,
/// and `FOO` map to distinct filenames on case-insensitive
/// filesystems (macOS APFS default, Windows NTFS). The suffix is
/// derived deterministically from the original id via
/// [`hash_disambig`].
#[must_use]
pub fn filename_id(id: &str) -> String {
    let safe: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();

    if safe.bytes().any(|b| b.is_ascii_uppercase()) {
        format!("{}_H{}", safe.to_ascii_lowercase(), hash_disambig(id))
    } else {
        safe
    }
}

/// Short deterministic hex suffix used as a tie-breaker when two
/// entity ids fold to the same lowercase filename on a
/// case-insensitive filesystem.
///
/// Returns the first 6 hex chars of SHA-256 over `input` — 24 bits is
/// plenty for disambiguating a handful of case-only siblings within a
/// single bundle, and SHA-256 is already in our dependency tree.
#[must_use]
pub fn hash_disambig(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(6);
    for &b in digest.iter().take(3) {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Returns an RFC 3339 UTC timestamp. Falls back to the Unix epoch if the
/// system clock is unavailable (unusual but possible in sandboxes).
fn chrono_now() -> String {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
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
            method_calls: Vec::new(),
            parent: None,
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
            method_calls: Vec::new(),
            parent: None,
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
    fn filename_id_replaces_special_chars() {
        assert_eq!(
            filename_id("code:src/lib.rs::function::foo"),
            "code_src_lib.rs__function__foo"
        );
        assert_eq!(filename_id("abc_123-def.rs"), "abc_123-def.rs");
        assert_eq!(filename_id("a::b"), "a__b");
    }

    #[test]
    fn filename_id_case_collision_distinct_outputs() {
        // Inputs differ only in case → outputs must be distinct, even
        // after case-folding (so they survive APFS / NTFS).
        let a = filename_id("Foo");
        let b = filename_id("foo");
        let c = filename_id("FOO");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
        assert_ne!(a.to_ascii_lowercase(), b.to_ascii_lowercase());
        assert_ne!(a.to_ascii_lowercase(), c.to_ascii_lowercase());
        assert_ne!(b.to_ascii_lowercase(), c.to_ascii_lowercase());
        // All-lowercase input must keep its plain form (back-compat).
        assert_eq!(b, "foo");
        // Mixed-case forms are lowercased + suffixed with `_H<hash>`.
        assert!(a.starts_with("foo_H"), "got: {a}");
        assert!(c.starts_with("foo_H"), "got: {c}");
    }

    #[test]
    fn filename_id_is_deterministic() {
        // Same input must produce the same suffix every call.
        assert_eq!(filename_id("MyStruct"), filename_id("MyStruct"));
    }

    /// On macOS APFS the default filesystem is case-insensitive, so two
    /// files whose names differ only in case clobber each other on
    /// write. This test exercises the case-collision path end-to-end:
    /// it writes three artifacts whose ids fold to the same lowercase
    /// form, and asserts that all three end up on disk as distinct
    /// files.
    #[test]
    #[cfg(target_os = "macos")]
    fn filename_id_case_collision_survives_apfs() {
        let store = Store::new();
        store.add_atom(fn_atom(
            "code:src/lib.rs::function::Foo",
            "src/lib.rs",
            "Foo",
        ));
        store.add_atom(fn_atom(
            "code:src/lib.rs::function::foo",
            "src/lib.rs",
            "foo",
        ));
        store.add_atom(fn_atom(
            "code:src/lib.rs::function::FOO",
            "src/lib.rs",
            "FOO",
        ));
        let artifacts = emit_artifacts(&store, "/src");

        let tmp = tempfile::tempdir().expect("tmp");
        write_artifacts(&artifacts, tmp.path()).expect("string write is infallible");

        let entities_dir = tmp.path().join("entities");
        let mmd_files: Vec<String> = std::fs::read_dir(&entities_dir)
            .expect("read entities")
            .filter_map(std::result::Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| {
                std::path::Path::new(n)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("mmd"))
            })
            .collect();
        // 3 distinct entities → 3 distinct .mmd files survive on disk
        // (would be 1 without case-disambiguation on APFS).
        assert_eq!(
            mmd_files.len(),
            3,
            "expected 3 .mmd files for Foo/foo/FOO, got: {mmd_files:?}"
        );
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
        write_artifacts(&artifacts, tmp.path()).expect("string write is infallible");

        assert!(tmp.path().join("overview.mmd").exists());
        assert!(tmp.path().join("index.json").exists());
        // entities dir exists
        assert!(tmp.path().join("entities").is_dir());
    }

    /// Build a small artifact set with one fn entity at `(file, name, hash)`.
    /// `source_root` controls the index's `source_root` field — irrelevant
    /// for these tests but distinguishes runs.
    fn small_set(file: &str, name: &str, hash: &str) -> ArtifactSet {
        let store = Store::new();
        let mut atom = fn_atom(&format!("code:{file}::function::{name}"), file, name);
        atom.content_hash = hash.to_owned();
        store.add_atom(atom);
        emit_artifacts(&store, "/src")
    }

    fn mtime(path: &std::path::Path) -> std::time::SystemTime {
        std::fs::metadata(path)
            .expect("metadata")
            .modified()
            .expect("modified")
    }

    #[test]
    fn write_artifacts_skips_unchanged_files() {
        // Second write with the same artifacts must not rewrite any file —
        // compared via mtime equality. APFS / ext4 give us sub-second
        // resolution so no sleep is needed to make this observable.
        let artifacts = small_set("src/lib.rs", "foo", "h1");
        let tmp = tempfile::tempdir().expect("tmp");
        write_artifacts(&artifacts, tmp.path()).expect("first write");

        let overview = tmp.path().join("overview.mmd");
        let index = tmp.path().join("index.json");
        let foo_mmd = tmp
            .path()
            .join("entities/code_src_lib.rs__function__foo.mmd");
        let foo_meta = tmp
            .path()
            .join("entities/code_src_lib.rs__function__foo.meta.json");

        let m_overview = mtime(&overview);
        let m_index = mtime(&index);
        let m_mmd = mtime(&foo_mmd);
        let m_meta = mtime(&foo_meta);

        // Re-emit (so `generated_at` is fresh) and write again.
        let artifacts2 = small_set("src/lib.rs", "foo", "h1");
        write_artifacts(&artifacts2, tmp.path()).expect("second write");

        assert_eq!(mtime(&overview), m_overview, "overview.mmd was rewritten");
        assert_eq!(
            mtime(&index),
            m_index,
            "index.json was rewritten despite only generated_at differing"
        );
        assert_eq!(mtime(&foo_mmd), m_mmd, "entity .mmd was rewritten");
        assert_eq!(mtime(&foo_meta), m_meta, "entity .meta.json was rewritten");
    }

    #[test]
    fn write_artifacts_rewrites_only_modified_entity() {
        // Two entities; second pass changes one's content_hash. The
        // unchanged entity's files keep their original mtime.
        let store = Store::new();
        let mut foo = fn_atom("code:src/lib.rs::function::foo", "src/lib.rs", "foo");
        foo.content_hash = "h1".to_owned();
        let mut bar = fn_atom("code:src/lib.rs::function::bar", "src/lib.rs", "bar");
        bar.content_hash = "b1".to_owned();
        store.add_atom(foo);
        store.add_atom(bar);
        let artifacts = emit_artifacts(&store, "/src");

        let tmp = tempfile::tempdir().expect("tmp");
        write_artifacts(&artifacts, tmp.path()).expect("first write");

        let foo_mmd = tmp
            .path()
            .join("entities/code_src_lib.rs__function__foo.mmd");
        let bar_mmd = tmp
            .path()
            .join("entities/code_src_lib.rs__function__bar.mmd");
        let m_foo = mtime(&foo_mmd);
        let m_bar = mtime(&bar_mmd);

        // Second pass: bar's hash changes, foo unchanged.
        let store2 = Store::new();
        let mut foo2 = fn_atom("code:src/lib.rs::function::foo", "src/lib.rs", "foo");
        foo2.content_hash = "h1".to_owned();
        let mut bar2 = fn_atom("code:src/lib.rs::function::bar", "src/lib.rs", "bar");
        bar2.content_hash = "b2".to_owned();
        store2.add_atom(foo2);
        store2.add_atom(bar2);
        let artifacts2 = emit_artifacts(&store2, "/src");
        write_artifacts(&artifacts2, tmp.path()).expect("second write");

        assert_eq!(mtime(&foo_mmd), m_foo, "foo.mmd should not be rewritten");
        assert!(
            mtime(&bar_mmd) > m_bar,
            "bar.mmd should be rewritten after content_hash change"
        );
    }

    #[test]
    fn write_artifacts_prunes_removed_entities() {
        // First pass: foo + bar. Second pass: only foo. bar's .mmd and
        // .meta.json must be deleted.
        let store = Store::new();
        store.add_atom(fn_atom(
            "code:src/lib.rs::function::foo",
            "src/lib.rs",
            "foo",
        ));
        store.add_atom(fn_atom(
            "code:src/lib.rs::function::bar",
            "src/lib.rs",
            "bar",
        ));
        let artifacts = emit_artifacts(&store, "/src");

        let tmp = tempfile::tempdir().expect("tmp");
        write_artifacts(&artifacts, tmp.path()).expect("first write");

        let bar_mmd = tmp
            .path()
            .join("entities/code_src_lib.rs__function__bar.mmd");
        let bar_meta = tmp
            .path()
            .join("entities/code_src_lib.rs__function__bar.meta.json");
        assert!(bar_mmd.exists());
        assert!(bar_meta.exists());

        let store2 = Store::new();
        store2.add_atom(fn_atom(
            "code:src/lib.rs::function::foo",
            "src/lib.rs",
            "foo",
        ));
        let artifacts2 = emit_artifacts(&store2, "/src");
        write_artifacts(&artifacts2, tmp.path()).expect("second write");

        assert!(!bar_mmd.exists(), "bar.mmd should be pruned");
        assert!(!bar_meta.exists(), "bar.meta.json should be pruned");
    }
}
