//! End-to-end orchestrator: filesystem → ingester → store → resolver → renderer.
//!
//! Used by both the CLI and (later) the MCP server. Pure async; backed by an
//! [`InMemoryStore`] for the v0.2 MVP.

use crate::artifacts::{ArtifactDir, EntityArtifact, build_index, sanitize_id};
use crate::error::{AstToMermaidError, Result};
use crate::render::{Level, render};
use crate::resolve::resolve_cross_module_calls;
use crate::store::{InMemoryStore, ingest_parse_output};
use ingester_code::{CodeParser, Language};
use ingester_core::{Atom, Origin, Parser, Relation};
use polystore::{Direction, EntityId, GraphStore, Scope};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Options controlling [`analyze`].
#[derive(Debug, Clone)]
pub struct AnalyzeOptions {
    /// Mermaid view to render.
    pub level: Level,
    /// Required for `module` / `function` / `impact` levels: a module path
    /// or name, or a function name. Ignored by `project` / `overview`.
    pub target: Option<String>,
    /// Extra directory names (matched on the basename) to skip during the
    /// walk. Always combined with the built-in skip set
    /// (`target`, `node_modules`, `.git`, any dotfile dir).
    pub exclude: Vec<String>,
    /// Tenant scope to attribute the in-memory store to. Defaults aren't
    /// useful for analysis output, but downstream tooling may surface it.
    pub scope: Scope,
}

impl Default for AnalyzeOptions {
    fn default() -> Self {
        Self {
            level: Level::Project,
            target: None,
            exclude: Vec::new(),
            scope: Scope::new("local", "local", "main"),
        }
    }
}

/// What [`analyze`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzeReport {
    /// Rendered Mermaid source.
    pub mermaid: String,
    /// Files we successfully parsed.
    pub files_parsed: usize,
    /// Total atoms indexed across all files.
    pub atoms_indexed: usize,
    /// Cross-module call edges added by the resolver.
    pub edges_resolved: usize,
}

/// Recursively analyze `root`, parsing every supported source file, building
/// an in-memory graph, resolving cross-module calls, and rendering the
/// requested Mermaid level.
///
/// Walks `root` skipping common heavy directories (`target`, `node_modules`,
/// `.git`). Files whose extension is not recognized are silently ignored.
///
/// # Errors
///
/// Returns the first error from filesystem walk, parsing, store ingestion,
/// resolver, or renderer. Individual files that fail to parse are propagated
/// (callers can choose to wrap or filter via [`pre-walk`](walk_for_languages)).
pub async fn analyze(root: &Path, opts: &AnalyzeOptions) -> Result<AnalyzeReport> {
    if !root.exists() {
        return Err(AstToMermaidError::InvalidInput(format!(
            "path does not exist: {}",
            root.display()
        )));
    }

    let files = walk_for_languages_with_exclude(root, &opts.exclude)?;
    let store = InMemoryStore::new(opts.scope.clone());

    let mut atoms_indexed = 0;
    let mut files_parsed = 0;
    for (path, lang) in &files {
        let bytes = std::fs::read(path).map_err(|e| {
            AstToMermaidError::InvalidInput(format!("read {}: {e}", path.display()))
        })?;
        let parser = match lang {
            Language::Rust => CodeParser::rust(),
            Language::Python => CodeParser::python(),
        };
        let display_path = display_path(root, path);
        let origin = Origin::file(display_path, Some(lang.name()));
        let output = parser
            .parse(&bytes, &origin)
            .map_err(|e| AstToMermaidError::InvalidInput(format!("parse {origin}: {e}")))?;
        atoms_indexed += output.atoms.len();
        ingest_parse_output(&store, &output).await?;
        files_parsed += 1;
    }

    let edges_resolved = resolve_cross_module_calls(&store).await?;
    let mermaid = render(opts.level, &store, opts.target.as_deref()).await?;

    Ok(AnalyzeReport {
        mermaid,
        files_parsed,
        atoms_indexed,
        edges_resolved,
    })
}

/// Build the full 4-layer artifact bundle for `root`.
///
/// Walks the project, ingests every supported source file, resolves
/// cross-module call edges, then renders :
/// - `overview.mmd` (module-level view)
/// - `project.mmd` (crate-level view)
/// - one `.mmd` + `.meta.json` per indexed entity (module, function,
///   struct, trait, impl, enum)
/// - `index.json` with the registry + per-entity edge summary
///
/// The per-entity `.mmd` shipped here is a minimal 1-hop renderer
/// (central node + neighbours) — kind-specialised renderers
/// (struct / trait / enum subgraphs) land in PR 2 of the bundle
/// rollout.
///
/// # Errors
///
/// Returns the first error from filesystem walk, parsing, store
/// ingestion, resolver, or renderer.
pub async fn bundle(root: &Path, opts: &AnalyzeOptions) -> Result<(ArtifactDir, AnalyzeReport)> {
    if !root.exists() {
        return Err(AstToMermaidError::InvalidInput(format!(
            "path does not exist: {}",
            root.display()
        )));
    }

    let files = walk_for_languages_with_exclude(root, &opts.exclude)?;
    let store = InMemoryStore::new(opts.scope.clone());

    let mut atoms_indexed = 0;
    let mut files_parsed = 0;
    for (path, lang) in &files {
        let bytes = std::fs::read(path).map_err(|e| {
            AstToMermaidError::InvalidInput(format!("read {}: {e}", path.display()))
        })?;
        let parser = match lang {
            Language::Rust => CodeParser::rust(),
            Language::Python => CodeParser::python(),
        };
        let display_path = display_path(root, path);
        let origin = Origin::file(display_path, Some(lang.name()));
        let output = parser
            .parse(&bytes, &origin)
            .map_err(|e| AstToMermaidError::InvalidInput(format!("parse {origin}: {e}")))?;
        atoms_indexed += output.atoms.len();
        ingest_parse_output(&store, &output).await?;
        files_parsed += 1;
    }

    let edges_resolved = resolve_cross_module_calls(&store).await?;

    let overview_mmd = render(Level::Overview, &store, None).await?;
    let project_mmd = render(Level::Project, &store, None).await?;

    let (entities, index_entries) = collect_entities(&store).await?;

    let report = AnalyzeReport {
        mermaid: overview_mmd.clone(),
        files_parsed,
        atoms_indexed,
        edges_resolved,
    };

    let source_root = root.to_string_lossy();
    let index_json = build_index(
        &index_entries,
        source_root.as_ref(),
        files_parsed,
        atoms_indexed,
        edges_resolved,
    );

    let dir = ArtifactDir {
        overview_mmd,
        project_mmd,
        index_json,
        entities,
    };
    Ok((dir, report))
}

/// Kinds we materialise into per-entity bundle artifacts. Matches the
/// list the renderers already understand — keeps the bundle in lockstep
/// with the in-memory schema.
const ENTITY_KINDS: &[&str] = &["module", "function", "struct", "trait", "impl", "enum"];

/// Walk the indexed store and produce one [`EntityArtifact`] +
/// matching `index.json` row per entity in [`ENTITY_KINDS`].
///
/// Edges are computed via `neighbors_bulk` once for each direction —
/// no N+1 per-entity round trips on backends that override.
#[allow(clippy::too_many_lines, clippy::similar_names)]
async fn collect_entities<S>(store: &S) -> Result<(Vec<EntityArtifact>, Vec<Value>)>
where
    S: GraphStore<Atom, Relation>,
{
    // 1. Bulk-list every indexed kind.
    let kind_groups = store.list_by_kinds(ENTITY_KINDS).await?;
    let mut all_ids: Vec<EntityId> = Vec::new();
    for (_, ids) in &kind_groups {
        all_ids.extend(ids.iter().cloned());
    }
    all_ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    all_ids.dedup_by(|a, b| a.as_str() == b.as_str());

    // 2. Bulk-fetch every node + its outgoing/incoming neighbours.
    let nodes = store.get_nodes_bulk(&all_ids).await?;
    let outgoing = store.neighbors_bulk(&all_ids, Direction::Outgoing).await?;
    let incoming = store.neighbors_bulk(&all_ids, Direction::Incoming).await?;

    let mut entities = Vec::new();
    let mut index_entries = Vec::new();

    for (id, atom_opt) in all_ids.iter().zip(nodes) {
        let Some(atom) = atom_opt else { continue };

        let kind = atom.kind.clone();
        let name = atom.name.clone();
        let id_str = id.as_str().to_owned();

        let file = atom
            .metadata
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let line_start = atom
            .metadata
            .get("line_start")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let line_end = atom
            .metadata
            .get("line_end")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let signature = atom
            .metadata
            .get("signature")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let doc = atom
            .metadata
            .get("doc")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();

        let outs = outgoing
            .iter()
            .find(|(eid, _)| eid.as_str() == id.as_str())
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let ins = incoming
            .iter()
            .find(|(eid, _)| eid.as_str() == id.as_str())
            .map(|(_, v)| v.clone())
            .unwrap_or_default();

        // Index-level edge summary (compact — one row per neighbour).
        let edges_out: Vec<Value> = outs
            .iter()
            .map(|(target, rel)| json!({"to": target.as_str(), "kind": rel.kind}))
            .collect();
        let edges_in: Vec<Value> = ins
            .iter()
            .map(|(source, rel)| json!({"from": source.as_str(), "kind": rel.kind}))
            .collect();

        // meta.json — detailed view with neighbour names.
        let callers: Vec<Value> = ins
            .iter()
            .filter(|(_, rel)| rel.kind == "calls")
            .map(|(source, _)| neighbor_meta(source.as_str()))
            .collect();
        let callees: Vec<Value> = outs
            .iter()
            .filter(|(_, rel)| rel.kind == "calls")
            .map(|(target, _)| neighbor_meta(target.as_str()))
            .collect();
        let children: Vec<Value> = outs
            .iter()
            .filter(|(_, rel)| rel.kind == "contains")
            .map(|(target, _)| neighbor_meta(target.as_str()))
            .collect();

        let meta = json!({
            "id": id_str,
            "kind": kind,
            "name": name,
            "file": file,
            "line_start": line_start,
            "line_end": line_end,
            "signature": signature,
            "doc": doc,
            "content_hash": atom.content_hash,
            "callers": callers,
            "callees": callees,
            "children": children,
            "imports": Vec::<Value>::new(),
            "imported_by": Vec::<Value>::new(),
        });

        let mmd = render_entity_mmd(
            &id_str, &kind, &name, &file, line_start, line_end, &outs, &ins,
        );

        let stem = sanitize_id(&id_str);
        index_entries.push(json!({
            "id": id_str,
            "kind": kind,
            "name": name,
            "file": file,
            "mmd_path": format!("entities/{stem}.mmd"),
            "meta_path": format!("entities/{stem}.meta.json"),
            "edges": {"out": edges_out, "in": edges_in},
        }));
        entities.push(EntityArtifact {
            id: id_str,
            mmd,
            meta,
        });
    }

    entities.sort_by(|a, b| a.id.cmp(&b.id));
    Ok((entities, index_entries))
}

/// Bare-bones neighbour record for `meta.json`. PR 2 enriches with
/// signature / line numbers ; for now we just keep `id`.
fn neighbor_meta(neighbour_id: &str) -> Value {
    // The store's bulk API returns Relation only — for richer
    // neighbour metadata we'd need a second lookup pass. Keep PR1
    // cheap : emit just the id ; PR2 attaches names/lines via a
    // second `get_nodes_bulk` round-trip on the union of caller +
    // callee + children sets.
    json!({"id": neighbour_id})
}

/// Generic per-entity Mermaid renderer.
///
/// PR 1 ships the same 1-hop layout for every entity kind : the
/// central node + 1-hop callers above + 1-hop callees below. The
/// `%% id`, `%% kind`, `%% file` headers make the file
/// self-describing so `mermaid-graph` can correlate it with its
/// `meta.json` without reparsing the path.
///
/// PR 2 swaps this for kind-specific renderers (struct subgraphs
/// with fields, trait subgraphs with methods, …).
#[allow(clippy::too_many_arguments)]
fn render_entity_mmd(
    id: &str,
    kind: &str,
    name: &str,
    file: &str,
    line_start: u64,
    line_end: u64,
    outs: &[(EntityId, Relation)],
    ins: &[(EntityId, Relation)],
) -> String {
    use std::fmt::Write as _;

    let mut s = String::new();
    writeln!(s, "%% id: {id}").ok();
    writeln!(s, "%% kind: {kind}").ok();
    if !file.is_empty() {
        writeln!(s, "%% file: {file}:{line_start}-{line_end}").ok();
    }
    writeln!(s, "graph TD").ok();
    let self_id = crate::render::util::mermaid_id(name);
    let label = crate::render::util::escape_label(name);
    writeln!(s, "    {self_id}[\"{label}\"]").ok();

    for (source, rel) in ins {
        let neigh = crate::render::util::mermaid_id(source.as_str());
        let lbl = crate::render::util::escape_label(source.as_str());
        writeln!(s, "    {neigh}[\"{lbl}\"] -->|\"{}\"| {self_id}", rel.kind).ok();
    }
    for (target, rel) in outs {
        let neigh = crate::render::util::mermaid_id(target.as_str());
        let lbl = crate::render::util::escape_label(target.as_str());
        writeln!(s, "    {self_id} -->|\"{}\"| {neigh}[\"{lbl}\"]", rel.kind).ok();
    }

    s
}

/// Walk `root` recursively and return `(path, language)` pairs for every
/// supported source file. Skips heavy/uninteresting dirs (`target`,
/// `node_modules`, `.git`, hidden dirs starting with `.`).
///
/// # Errors
///
/// Propagates any I/O error from the walk.
pub fn walk_for_languages(root: &Path) -> Result<Vec<(PathBuf, Language)>> {
    walk_for_languages_with_exclude::<&str>(root, &[])
}

/// Same as [`walk_for_languages`] but skips any directory whose basename
/// matches an entry in `extra_exclude`. The built-in skip set
/// (`target`, `node_modules`, `.git`, any dotfile dir) is always applied
/// on top.
///
/// # Errors
///
/// Propagates any I/O error from the walk.
pub fn walk_for_languages_with_exclude<S: AsRef<str>>(
    root: &Path,
    extra_exclude: &[S],
) -> Result<Vec<(PathBuf, Language)>> {
    let extra: Vec<&str> = extra_exclude.iter().map(AsRef::as_ref).collect();
    let mut out = Vec::new();
    walk_into(root, &extra, &mut out)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn is_excluded(name: &str, extra_exclude: &[&str]) -> bool {
    matches!(name, "target" | "node_modules" | ".git")
        || name.starts_with('.')
        || extra_exclude.contains(&name)
}

fn walk_into(dir: &Path, extra_exclude: &[&str], out: &mut Vec<(PathBuf, Language)>) -> Result<()> {
    if dir.is_file() {
        if let Some(lang) = language_for(dir) {
            out.push((dir.to_path_buf(), lang));
        }
        return Ok(());
    }
    if !dir.is_dir() {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir)
        .map_err(|e| AstToMermaidError::InvalidInput(format!("read dir {}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry
            .map_err(|e| AstToMermaidError::InvalidInput(format!("walk {}: {e}", dir.display())))?;
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if is_excluded(name, extra_exclude) {
                continue;
            }
            walk_into(&path, extra_exclude, out)?;
        } else if path.is_file()
            && let Some(lang) = language_for(&path)
        {
            out.push((path, lang));
        }
    }
    Ok(())
}

fn language_for(path: &Path) -> Option<Language> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => Some(Language::Rust),
        Some("py") => Some(Language::Python),
        _ => None,
    }
}

/// Render an absolute path as `crates/...` or relative-to-root for tidier
/// mermaid IDs. Falls back to the path's basename when no relativization
/// is possible.
fn display_path(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .ok()
        .map_or_else(|| file.display().to_string(), |p| p.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(dir: &Path, rel: &str, content: &str) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&path, content).expect("write");
        path
    }

    #[test]
    fn language_for_recognizes_rs_and_py() {
        assert_eq!(language_for(Path::new("/x/foo.rs")), Some(Language::Rust));
        assert_eq!(language_for(Path::new("foo.py")), Some(Language::Python));
        assert_eq!(language_for(Path::new("Makefile")), None);
        assert_eq!(language_for(Path::new("a.txt")), None);
    }

    #[test]
    fn walk_returns_sorted_pairs_skipping_target() {
        let tmp = tempdir().expect("tmp");
        let root = tmp.path();
        write(root, "src/a.rs", "fn a() {}");
        write(root, "src/b.py", "def b():\n    pass\n");
        write(root, "target/junk.rs", "fn dont_parse() {}");
        write(root, ".git/HEAD", "junk");
        write(root, "README.md", "# nope");

        let files = walk_for_languages(root).expect("walk");
        assert_eq!(files.len(), 2);
        assert!(
            files
                .iter()
                .all(|(p, _)| !p.to_string_lossy().contains("target"))
        );
        assert!(
            files
                .iter()
                .all(|(p, _)| !p.to_string_lossy().contains(".git"))
        );
    }

    #[test]
    fn walk_skips_hidden_directories() {
        let tmp = tempdir().expect("tmp");
        let root = tmp.path();
        write(root, ".hidden/inner.rs", "fn x() {}");
        write(root, "src/visible.rs", "fn y() {}");
        let files = walk_for_languages(root).expect("walk");
        assert_eq!(files.len(), 1);
        assert!(files[0].0.to_string_lossy().contains("visible.rs"));
    }

    #[test]
    fn walk_handles_single_file_input() {
        let tmp = tempdir().expect("tmp");
        let path = write(tmp.path(), "lone.rs", "fn x() {}");
        let files = walk_for_languages(&path).expect("walk");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].1, Language::Rust);
    }

    #[test]
    fn walk_unknown_path_is_empty() {
        let tmp = tempdir().expect("tmp");
        let path = tmp.path().join("does-not-exist");
        let files = walk_for_languages(&path).expect("walk silent");
        assert!(files.is_empty());
    }

    #[test]
    fn display_path_strips_root_prefix() {
        assert_eq!(
            display_path(Path::new("/work"), Path::new("/work/src/foo.rs")),
            "src/foo.rs"
        );
    }

    #[test]
    fn display_path_falls_back_when_strip_fails() {
        let s = display_path(Path::new("/work"), Path::new("/elsewhere/foo.rs"));
        assert!(s.contains("foo.rs"));
    }

    #[tokio::test]
    async fn analyze_missing_root_errors() {
        let err = analyze(
            Path::new("/definitely/does/not/exist/12345"),
            &AnalyzeOptions::default(),
        )
        .await
        .expect_err("must error");
        assert!(matches!(err, AstToMermaidError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn analyze_empty_dir_yields_empty_graph() {
        let tmp = tempdir().expect("tmp");
        let report = analyze(tmp.path(), &AnalyzeOptions::default())
            .await
            .expect("ok");
        assert_eq!(report.files_parsed, 0);
        assert_eq!(report.atoms_indexed, 0);
        assert_eq!(report.edges_resolved, 0);
        assert_eq!(report.mermaid, "graph TD\n");
    }

    #[tokio::test]
    async fn bundle_produces_overview_project_and_per_entity_files() {
        let tmp = tempdir().expect("tmp");
        let root = tmp.path();
        write(root, "src/lib.rs", "pub fn hello() {}\n");

        let (artifacts, report) = bundle(
            root,
            &AnalyzeOptions {
                level: Level::Project,
                target: None,
                exclude: Vec::new(),
                scope: Scope::new("ns", "repo", "branch"),
            },
        )
        .await
        .expect("bundle");

        // Stats land in the report and in index.json.
        assert_eq!(report.files_parsed, 1);
        assert!(report.atoms_indexed >= 2);

        assert!(artifacts.overview_mmd.starts_with("graph TD"));
        assert!(artifacts.project_mmd.starts_with("graph TD"));
        assert_eq!(artifacts.index_json["schema_version"], json!(1));
        assert_eq!(artifacts.index_json["stats"]["files_parsed"], json!(1));

        // At least the module + the function should be there.
        let kinds: Vec<&str> = artifacts
            .entities
            .iter()
            .filter_map(|e| e.meta["kind"].as_str())
            .collect();
        assert!(
            kinds.contains(&"module"),
            "missing module entity: {kinds:?}"
        );
        assert!(
            kinds.contains(&"function"),
            "missing function entity: {kinds:?}",
        );

        // Each entity's .mmd is self-describing.
        for e in &artifacts.entities {
            assert!(
                e.mmd.contains(&format!("%% id: {}", e.id)),
                "entity {} missing id header",
                e.id,
            );
            assert!(
                e.mmd.contains(&format!(
                    "%% kind: {}",
                    e.meta["kind"].as_str().unwrap_or("")
                )),
                "entity {} missing kind header",
                e.id,
            );
            assert!(e.mmd.starts_with("%% id:"));
        }
    }

    #[tokio::test]
    async fn bundle_round_trips_through_write_and_load() {
        use crate::artifacts::{load_artifact_dir, write_artifacts};

        let tmp = tempdir().expect("tmp");
        let root = tmp.path();
        write(root, "src/lib.rs", "pub fn foo() {}\n");

        let (artifacts, _) = bundle(root, &AnalyzeOptions::default())
            .await
            .expect("bundle");

        let out_tmp = tempdir().expect("out tmp");
        write_artifacts(&artifacts, out_tmp.path()).expect("write");
        let loaded = load_artifact_dir(out_tmp.path()).expect("load");

        assert_eq!(loaded.overview_mmd, artifacts.overview_mmd);
        assert_eq!(loaded.project_mmd, artifacts.project_mmd);
        assert_eq!(loaded.entities.len(), artifacts.entities.len());
    }

    #[tokio::test]
    async fn analyze_simple_two_crate_workspace_renders_project() {
        let tmp = tempdir().expect("tmp");
        let root = tmp.path();
        // crate_a calls helper from crate_b
        write(
            root,
            "crates/crate_a/src/lib.rs",
            "pub fn caller() { helper(); }\n",
        );
        write(root, "crates/crate_b/src/lib.rs", "pub fn helper() {}\n");

        let report = analyze(
            root,
            &AnalyzeOptions {
                level: Level::Project,
                target: None,
                exclude: Vec::new(),
                scope: Scope::new("ns", "repo", "branch"),
            },
        )
        .await
        .expect("analyze");

        assert_eq!(report.files_parsed, 2);
        assert!(report.atoms_indexed >= 4); // 2 modules + 2 functions
        assert_eq!(report.edges_resolved, 1);

        // Mermaid output should mention both crates and the cross-crate edge.
        assert!(report.mermaid.contains("crate_a"));
        assert!(report.mermaid.contains("crate_b"));
        assert!(
            report.mermaid.contains("crate_a -->|\"1 calls\"| crate_b"),
            "got: {}",
            report.mermaid
        );
    }

    #[tokio::test]
    async fn analyze_overview_level_renders_modules() {
        let tmp = tempdir().expect("tmp");
        let root = tmp.path();
        write(
            root,
            "src/mod_a.rs",
            "pub fn caller() { helper(); }\npub fn other() {}\n",
        );
        write(root, "src/mod_b.rs", "pub fn helper() {}\n");

        let report = analyze(
            root,
            &AnalyzeOptions {
                level: Level::Overview,
                ..AnalyzeOptions::default()
            },
        )
        .await
        .expect("analyze");
        assert_eq!(report.files_parsed, 2);
        assert!(report.mermaid.contains("mod_a"));
        assert!(report.mermaid.contains("mod_b"));
        assert!(report.mermaid.contains(" -->|\""));
    }

    #[tokio::test]
    async fn analyze_propagates_invalid_utf8() {
        let tmp = tempdir().expect("tmp");
        let path = tmp.path().join("bad.rs");
        fs::write(&path, [0xff, 0xfe, 0xfd]).expect("write");
        let err = analyze(tmp.path(), &AnalyzeOptions::default())
            .await
            .expect_err("must reject");
        assert!(matches!(err, AstToMermaidError::InvalidInput(_)));
    }

    #[test]
    fn walk_exclude_skips_named_dir() {
        let tmp = tempdir().expect("tmp");
        let root = tmp.path();
        write(root, "src/a.rs", "fn a() {}");
        write(root, "vendor/junk.rs", "fn dont_parse() {}");
        write(root, "workspaces/inner.rs", "fn dont_parse() {}");

        // Default: vendor/ + workspaces/ are walked (not in built-in skip set).
        let default = walk_for_languages(root).expect("walk");
        assert_eq!(default.len(), 3, "default should walk all 3");

        // With exclude: skip vendor + workspaces.
        let filtered =
            walk_for_languages_with_exclude(root, &["vendor", "workspaces"]).expect("walk");
        assert_eq!(filtered.len(), 1);
        assert!(filtered[0].0.to_string_lossy().ends_with("a.rs"));
    }

    #[test]
    fn walk_exclude_does_not_override_builtin_skip_set() {
        let tmp = tempdir().expect("tmp");
        let root = tmp.path();
        write(root, "src/a.rs", "fn a() {}");
        write(root, "target/junk.rs", "fn dont_parse() {}");

        // target/ is always skipped, with or without explicit exclude.
        let no_exclude = walk_for_languages(root).expect("walk");
        let with_exclude = walk_for_languages_with_exclude(root, &["unrelated"]).expect("walk");
        assert_eq!(no_exclude.len(), 1);
        assert_eq!(with_exclude.len(), 1);
    }

    #[tokio::test]
    async fn analyze_with_exclude_skips_directory() {
        let tmp = tempdir().expect("tmp");
        let root = tmp.path();
        write(root, "src/main.rs", "pub fn main() {}");
        write(root, "vendor/lib.rs", "pub fn vendored() {}");

        // Without exclude: 2 files.
        let report = analyze(root, &AnalyzeOptions::default())
            .await
            .expect("analyze");
        assert_eq!(report.files_parsed, 2);

        // With exclude: 1 file (vendored skipped).
        let report = analyze(
            root,
            &AnalyzeOptions {
                exclude: vec!["vendor".to_owned()],
                ..AnalyzeOptions::default()
            },
        )
        .await
        .expect("analyze");
        assert_eq!(report.files_parsed, 1);
    }

    #[test]
    fn analyze_options_default_uses_project_level() {
        let opts = AnalyzeOptions::default();
        assert_eq!(opts.level, Level::Project);
        assert_eq!(opts.scope.namespace, "local");
    }

    #[test]
    fn analyze_report_clone_eq_debug() {
        let r = AnalyzeReport {
            mermaid: "graph TD\n".to_owned(),
            files_parsed: 2,
            atoms_indexed: 4,
            edges_resolved: 1,
        };
        let c = r.clone();
        assert_eq!(r, c);
        assert!(format!("{r:?}").contains("AnalyzeReport"));
    }
}
