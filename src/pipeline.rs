//! End-to-end orchestrator: filesystem → parser → store → resolver → renderer.
//!
//! No external graph backend. No `async` I/O — all store operations are
//! synchronous in-memory.

use crate::artifacts::{ArtifactSet, emit_artifacts, emit_artifacts_with_sequences};
use crate::cache::Cache;
use crate::error::{AstToMermaidError, Result};
use crate::git_source;
use crate::graph::Store;
use crate::parser::{CodeParser, Language, ParseFailure, ParseUnit, git_blob_sha1};
use crate::render::{Level, render};
use crate::resolve::{resolve_cross_module_calls, resolve_implements_edges};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Options controlling [`analyze`].
#[derive(Clone)]
#[non_exhaustive]
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
    /// Optional git ref to read source from (e.g. `main`, `v0.1.0`,
    /// `HEAD~3`). When set, the pipeline reads file contents via
    /// `git cat-file` rather than walking the working tree, and ignores
    /// `exclude` (git ls-tree already excludes ignored files).
    pub git_ref: Option<String>,
    /// Optional content-addressed cache for atom-level memoization. When set,
    /// the parse loop checks `<cache>/blobs/<git_blob_sha>.cbor` per file and
    /// replays cached `ParseUnit`s on hit (skipping tree-sitter entirely).
    /// On miss, the fresh `ParseUnit` is written back to the cache.
    pub cache: Option<Arc<Cache>>,
    /// Opt-in: when `true`, [`bundle`] also emits `sequences/<id>.mmd` for
    /// every Rust function whose body has at least one step. Off by
    /// default — the extra extraction roughly doubles bundle wall-time.
    pub with_sequences: bool,
}

impl Default for AnalyzeOptions {
    fn default() -> Self {
        Self {
            level: Level::Project,
            target: None,
            exclude: Vec::new(),
            git_ref: None,
            cache: None,
            with_sequences: false,
        }
    }
}

impl std::fmt::Debug for AnalyzeOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnalyzeOptions")
            .field("level", &self.level)
            .field("target", &self.target)
            .field("exclude", &self.exclude)
            .field("git_ref", &self.git_ref)
            .field(
                "cache",
                &self.cache.as_ref().map(|c| c.root().display().to_string()),
            )
            .field("with_sequences", &self.with_sequences)
            .finish()
    }
}

/// One unit of work for the parse loop: a logical path label, the bytes to
/// parse, and the language. Produced by either the FS walk or the git tree.
struct ParseInput {
    /// Display path used for entity ids (relative-to-root, slash-separated).
    display_path: String,
    /// File contents as bytes. Held behind an `Arc` so the bundle path can
    /// hand the same bytes to the sequence extractor without a deep copy
    /// (refcount bump only).
    content: Arc<[u8]>,
    /// Language detected from the file extension.
    language: Language,
    /// Git blob SHA-1 of the content. From `git ls-tree` in `--ref` mode,
    /// computed inline in worktree mode. Used as the atom-cache key.
    blob_sha: String,
}

/// Collect parse inputs from either the working tree (FS walk) or a git ref
/// (`git ls-tree` + `git cat-file`).
fn collect_inputs(root: &Path, opts: &AnalyzeOptions) -> Result<Vec<ParseInput>> {
    if let Some(git_ref) = opts.git_ref.as_deref() {
        collect_from_git_ref(root, git_ref)
    } else {
        collect_from_worktree(root, &opts.exclude)
    }
}

fn collect_from_worktree(root: &Path, exclude: &[String]) -> Result<Vec<ParseInput>> {
    let files = walk_for_languages_with_exclude(root, exclude)?;
    let mut out = Vec::with_capacity(files.len());
    for (path, language) in files {
        let content = std::fs::read(&path)?;
        let blob_sha = git_blob_sha1(&content);
        let display_path = display_path(root, &path);
        out.push(ParseInput {
            display_path,
            content: Arc::from(content.into_boxed_slice()),
            language,
            blob_sha,
        });
    }
    Ok(out)
}

fn collect_from_git_ref(root: &Path, git_ref: &str) -> Result<Vec<ParseInput>> {
    let toplevel = git_source::show_toplevel(root)?;
    // If the user pointed at a subdirectory, only keep entries under it.
    let prefix = root
        .canonicalize()
        .ok()
        .and_then(|abs| abs.strip_prefix(&toplevel).ok().map(Path::to_path_buf))
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty());

    let entries = git_source::ls_tree(&toplevel, git_ref)?;
    // One persistent `git cat-file --batch` child for the whole loop,
    // dropped (= killed + reaped) when this function returns. Avoids
    // ~50 ms fork+exec per blob on macOS.
    let mut reader = git_source::BatchReader::spawn(&toplevel)?;
    let mut out = Vec::new();
    for entry in entries {
        if let Some(p) = prefix.as_deref()
            && !entry.path.starts_with(p)
        {
            continue;
        }
        let Some(language) = language_for(Path::new(&entry.path)) else {
            continue;
        };
        let content = reader.read_blob(&entry.blob_sha)?;
        out.push(ParseInput {
            display_path: entry.path,
            content: Arc::from(content.into_boxed_slice()),
            language,
            blob_sha: entry.blob_sha,
        });
    }
    Ok(out)
}

/// What [`analyze`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AnalyzeReport {
    /// Rendered Mermaid source.
    pub mermaid: String,
    /// Files we successfully parsed.
    pub files_parsed: usize,
    /// Total atoms indexed across all files.
    pub atoms_indexed: usize,
    /// Cross-module call edges added by the resolver.
    pub edges_resolved: usize,
    /// Files we tried to parse but skipped because their parse failed. The
    /// pipeline now logs a `tracing::warn!` and continues per-file instead
    /// of aborting on the first failure, so callers see the full list here.
    pub failures: Vec<ParseFailure>,
}

/// Summary of one [`parse_phase`] invocation: how many files survived
/// parsing, how many atoms they produced, and the per-file failures we
/// caught instead of bubbling up.
#[derive(Debug, Clone, Default)]
pub struct ParseReport {
    /// Files that produced a [`ParseUnit`] (cache hit or fresh parse).
    pub files_parsed: usize,
    /// Total atoms indexed across all successful files.
    pub atoms_indexed: usize,
    /// Per-file parse failures — one entry per skipped file.
    pub failures: Vec<ParseFailure>,
}

/// Recursively analyze `root`, parsing every supported source file, building
/// an in-memory graph, resolving cross-module calls, and rendering the
/// requested Mermaid level.
///
/// Walks `root` skipping the [`DEFAULT_EXCLUDED_DIRS`] set plus any directory
/// whose name starts with `.`. Extra exclusions can be added via
/// [`AnalyzeOptions::exclude`]. Files whose extension is not recognized are
/// silently ignored.
///
/// # Errors
///
/// Returns errors from the filesystem walk or the renderer. Per-file parse
/// failures are no longer fatal — they land in [`AnalyzeReport::failures`]
/// and a `tracing::warn!` line is emitted for each, while parsing of the
/// remaining files continues.
pub fn analyze(root: &Path, opts: &AnalyzeOptions) -> Result<AnalyzeReport> {
    // FS-mode requires the path to exist. In git-ref mode the path can be a
    // subdir hint that was deleted on HEAD but present in the ref; defer the
    // check to `git_source::show_toplevel`.
    if opts.git_ref.is_none() && !root.exists() {
        return Err(AstToMermaidError::InvalidInput(format!(
            "path does not exist: {}",
            root.display()
        )));
    }

    let inputs = collect_inputs(root, opts)?;
    let store = Store::new();

    let parse_report = parse_phase(&inputs, &store, opts.cache.as_deref());
    let edges_resolved = resolve_phase(&store, parse_report.atoms_indexed);
    let mermaid = render(opts.level, &store, opts.target.as_deref())?;

    Ok(AnalyzeReport {
        mermaid,
        files_parsed: parse_report.files_parsed,
        atoms_indexed: parse_report.atoms_indexed,
        edges_resolved,
        failures: parse_report.failures,
    })
}

/// Run the parse pass over `inputs`, threading each one into `store`.
///
/// Per-file parse errors are caught: we emit a `tracing::warn!`, append a
/// [`ParseFailure`] to the returned [`ParseReport`], and move on. A single
/// malformed file no longer aborts the whole run.
///
/// When `cache` is `Some`, each input's `blob_sha` is checked against the
/// atom cache first:
/// - **Hit**: cached `ParseUnit` is replayed straight into `store`,
///   skipping tree-sitter entirely. This is the cross-branch dedup that
///   makes branch switches cheap.
/// - **Miss**: tree-sitter parses, the result is applied to `store` AND
///   persisted to the cache for future runs.
///
/// Cache hit/miss counts are emitted on completion as a `tracing::info!`
/// line so users see the cache effectiveness via `--trace=info`.
fn parse_phase(inputs: &[ParseInput], store: &Store, cache: Option<&Cache>) -> ParseReport {
    let span = tracing::info_span!("parse_phase", files = inputs.len());
    let _enter = span.enter();
    let started = std::time::Instant::now();

    // Tree-sitter parsing is CPU-bound and embarrassingly file-parallel, so
    // fan out across rayon workers and collect partial results in input
    // order. Applying units to the (non-Sync) `Store` and folding the
    // failure list happens sequentially after the fan-in, which keeps the
    // resulting atom/edge order — and therefore the rendered output —
    // byte-identical to the sequential implementation.
    let results: Vec<Result<(ParseUnit, bool)>> = inputs
        .par_iter()
        .map(|input| parse_one_file(input, cache))
        .collect();

    let mut report = ParseReport::default();
    let mut hits = 0usize;
    let mut misses = 0usize;

    for (input, res) in inputs.iter().zip(results) {
        match res {
            Ok((unit, was_hit)) => {
                report.atoms_indexed += unit.atom_count();
                unit.apply_to(store);
                if was_hit {
                    hits += 1;
                } else {
                    misses += 1;
                }
                report.files_parsed += 1;
            }
            Err(e) => {
                let reason = format!("{e}");
                tracing::warn!(path = %input.display_path, "parse failed: {reason}");
                report.failures.push(ParseFailure {
                    path: input.display_path.clone(),
                    reason,
                });
            }
        }
    }

    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    tracing::info!(
        parsed = report.files_parsed,
        atoms = report.atoms_indexed,
        failed = report.failures.len(),
        hits,
        misses,
        elapsed_ms,
        "parse_phase done",
    );
    report
}

/// Parse a single input, consulting `cache` first. The returned `bool` is
/// `true` when the result came from the cache (counted as a hit by the
/// caller), `false` when tree-sitter actually ran. Any error returned here
/// is surfaced by [`parse_phase`] as a [`ParseFailure`] rather than aborting
/// the whole run.
fn parse_one_file(input: &ParseInput, cache: Option<&Cache>) -> Result<(ParseUnit, bool)> {
    if let Some(c) = cache
        && let Some(unit) = c.get_unit(&input.blob_sha)
    {
        return Ok((unit, true));
    }

    let parser = match input.language {
        Language::Rust => CodeParser::rust(),
        Language::Python => CodeParser::python(),
    };
    let unit = parser
        .parse(&input.content, &input.display_path)
        .map_err(|e| {
            AstToMermaidError::InvalidInput(format!("parse {}: {e}", input.display_path))
        })?;

    if let Some(c) = cache
        && let Err(e) = c.put_unit(&input.blob_sha, &unit)
    {
        tracing::warn!(blob_sha = %input.blob_sha, "cache.put_unit failed: {e}");
    }
    Ok((unit, false))
}

/// Run the cross-module resolver (Calls + Implements edges). Wrapped in
/// `tracing::info_span!("resolve_phase", ...)`. Returns the total edge count.
///
/// **V1.5 decision gate**: the per-call elapsed and atom-count fields are
/// the data we'll use to decide whether V2 (edge-level cache) is justified
/// (resolve-phase wall-time as a fraction of the whole pipeline on real
/// repos — see `docs/perf/2026-05-01-resolve-cost-baseline.md`).
fn resolve_phase(store: &Store, atoms: usize) -> usize {
    let span = tracing::info_span!("resolve_phase", atoms);
    let _enter = span.enter();
    let started = std::time::Instant::now();

    let edges_resolved = resolve_cross_module_calls(store) + resolve_implements_edges(store);

    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    tracing::info!(edges = edges_resolved, elapsed_ms, "resolve_phase done");
    edges_resolved
}

/// Walk `root`, parse every supported file, resolve cross-module calls,
/// and produce the full 4-layer artifact bundle (`overview.mmd` +
/// per-entity `.mmd` / `.meta.json` + `index.json`).
///
/// Caller writes the result to disk via
/// [`crate::artifacts::write_artifacts`]. Splitting compute and write
/// keeps the function pure-ish for tests and lets downstream code
/// inspect the [`ArtifactSet`] in memory.
///
/// `opts.level` and `opts.target` are ignored — the bundle always
/// emits every level. `opts.exclude` still applies to the walk.
///
/// # Errors
///
/// Returns errors from the filesystem walk or resolver. Per-file parse
/// failures land in [`AnalyzeReport::failures`] instead of aborting the
/// run — see [`analyze`] for the same trade-off.
pub fn bundle(root: &Path, opts: &AnalyzeOptions) -> Result<(ArtifactSet, AnalyzeReport)> {
    if opts.git_ref.is_none() && !root.exists() {
        return Err(AstToMermaidError::InvalidInput(format!(
            "path does not exist: {}",
            root.display()
        )));
    }

    let inputs = collect_inputs(root, opts)?;
    let store = Store::new();

    let parse_report = parse_phase(&inputs, &store, opts.cache.as_deref());
    let edges_resolved = resolve_phase(&store, parse_report.atoms_indexed);

    let source_root = match opts.git_ref.as_deref() {
        Some(ref_name) => format!("{}@{ref_name}", root.display()),
        None => root.to_string_lossy().into_owned(),
    };
    let artifacts = if opts.with_sequences {
        // Re-use the bytes already in memory — only Rust files can produce
        // sequence diagrams, so we filter the rest out cheaply. The
        // `Arc::clone` is a refcount bump; no source bytes are copied.
        let sources: Vec<(String, Arc<[u8]>)> = inputs
            .iter()
            .filter(|i| matches!(i.language, Language::Rust))
            .map(|i| (i.display_path.clone(), Arc::clone(&i.content)))
            .collect();
        emit_artifacts_with_sequences(&store, &source_root, &sources)
    } else {
        emit_artifacts(&store, &source_root)
    };

    let report = AnalyzeReport {
        // The "rendered" mermaid for a bundle is the project view —
        // matches what `analyze --level project` would have returned.
        mermaid: artifacts.overview_mmd.clone(),
        files_parsed: parse_report.files_parsed,
        atoms_indexed: parse_report.atoms_indexed,
        edges_resolved,
        failures: parse_report.failures,
    };
    Ok((artifacts, report))
}

/// Walk `root` recursively and return `(path, language)` pairs for every
/// supported source file. Skips every directory listed in
/// [`DEFAULT_EXCLUDED_DIRS`] plus any directory whose name starts with `.`.
///
/// # Errors
///
/// Propagates any I/O error from the walk.
pub fn walk_for_languages(root: &Path) -> Result<Vec<(PathBuf, Language)>> {
    walk_for_languages_with_exclude::<&str>(root, &[])
}

/// Same as [`walk_for_languages`] but skips any directory whose basename
/// matches an entry in `extra_exclude`. See [`DEFAULT_EXCLUDED_DIRS`] for
/// the built-in skip set; it is always applied on top.
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
    let mut visited: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    walk_into(root, &extra, &mut out, &mut visited)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Directory basenames the walker skips by default — output dirs and
/// virtual-env / cache dirs that almost never contain source you want
/// indexed.
///
/// Rust: `target`. JavaScript: `node_modules`. Python: `__pycache__`,
/// `venv`. Generic: `.git`, `dist`, `build`, `vendor`. Any directory whose
/// name starts with `.` is also skipped (covers `.venv`, `.tox`,
/// `.pytest_cache`, `.mypy_cache`, `.ruff_cache`, `.idea`, `.vscode`, etc.).
pub const DEFAULT_EXCLUDED_DIRS: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    "__pycache__",
    "venv",
    "dist",
    "build",
    "vendor",
];

fn is_excluded(name: &str, extra_exclude: &[&str]) -> bool {
    DEFAULT_EXCLUDED_DIRS.contains(&name) || name.starts_with('.') || extra_exclude.contains(&name)
}

fn walk_into(
    dir: &Path,
    extra_exclude: &[&str],
    out: &mut Vec<(PathBuf, Language)>,
    visited: &mut std::collections::HashSet<PathBuf>,
) -> Result<()> {
    // Use symlink_metadata so we don't follow links — symlink loops would
    // otherwise blow the stack on real-world repos (vendored crates,
    // recursive node_modules links, etc.).
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return Ok(());
    };
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if meta.is_file() {
        if let Some(lang) = language_for(dir) {
            out.push((dir.to_path_buf(), lang));
        }
        return Ok(());
    }
    if !meta.is_dir() {
        return Ok(());
    }
    // Belt-and-braces: even with the symlink check above, a hardlink cycle
    // (rare but possible) would still recurse. Track canonical paths.
    if let Ok(canon) = dir.canonicalize()
        && !visited.insert(canon)
    {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if file_type.is_dir() {
            if is_excluded(name, extra_exclude) {
                continue;
            }
            walk_into(&path, extra_exclude, out, visited)?;
        } else if file_type.is_file()
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

/// Resolve `<root>` to a snapshot id used as the cache bundle key.
///
/// - If `git_ref` is `Some`, returns the commit SHA from `git rev-parse`.
/// - If `git_ref` is `None`, walks the working tree and returns
///   `wt-<digest>` where `<digest>` is the first 16 hex chars of
///   `SHA1(<path>:<blob_sha>\0…)` over all source files sorted by path.
///   Deterministic — a clean working tree always hashes the same.
///
/// # Errors
/// I/O errors from the FS walk, or git errors from `rev-parse`.
pub fn snapshot_id(root: &Path, git_ref: Option<&str>) -> Result<String> {
    if let Some(r) = git_ref {
        return git_source::rev_parse(root, r);
    }
    let files = walk_for_languages_with_exclude::<&str>(root, &[])?;
    let mut pairs: Vec<(String, String)> = Vec::with_capacity(files.len());
    for (file, _lang) in files {
        let content = std::fs::read(&file)?;
        let blob = git_blob_sha1(&content);
        let display = display_path(root, &file);
        pairs.push((display, blob));
    }
    pairs.sort();
    let mut hasher = sha1_smol::Sha1::new();
    for (p, b) in pairs {
        hasher.update(p.as_bytes());
        hasher.update(b":");
        hasher.update(b.as_bytes());
        hasher.update(b"\0");
    }
    Ok(format!("wt-{}", &hasher.digest().to_string()[..16]))
}

/// Render an absolute path as relative-to-root for tidier IDs.
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

    #[test]
    fn analyze_missing_root_errors() {
        let err = analyze(
            Path::new("/definitely/does/not/exist/12345"),
            &AnalyzeOptions::default(),
        )
        .expect_err("must error");
        assert!(matches!(err, AstToMermaidError::InvalidInput(_)));
    }

    #[test]
    fn analyze_empty_dir_yields_empty_graph() {
        let tmp = tempdir().expect("tmp");
        let report = analyze(tmp.path(), &AnalyzeOptions::default()).expect("ok");
        assert_eq!(report.files_parsed, 0);
        assert_eq!(report.atoms_indexed, 0);
        assert_eq!(report.edges_resolved, 0);
        assert_eq!(report.mermaid, "graph TD\n");
    }

    #[test]
    fn analyze_simple_two_crate_workspace_renders_project() {
        let tmp = tempdir().expect("tmp");
        let root = tmp.path();
        // Cross-crate `module::helper(...)` (qualified inline) — the
        // resolver matches via the file-module-name fallback. Bare
        // unrewritten calls no longer cross-crate-resolve, so we use
        // an explicit qualifier.
        write(
            root,
            "crates/crate_a/src/lib.rs",
            "pub fn caller() { helper::helper(); }\n",
        );
        write(root, "crates/crate_b/src/helper.rs", "pub fn helper() {}\n");

        let report = analyze(
            root,
            &AnalyzeOptions {
                level: Level::Project,
                target: None,
                exclude: Vec::new(),
                git_ref: None,
                cache: None,
                with_sequences: false,
            },
        )
        .expect("analyze");

        assert_eq!(report.files_parsed, 2);
        assert!(report.atoms_indexed >= 4);
        assert_eq!(report.edges_resolved, 1);
        assert!(report.mermaid.contains("crate_a"));
        assert!(report.mermaid.contains("crate_b"));
        assert!(
            report.mermaid.contains("crate_a -->|\"1 calls\"| crate_b"),
            "got: {}",
            report.mermaid
        );
    }

    #[test]
    fn analyze_overview_level_renders_modules() {
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
        .expect("analyze");
        assert_eq!(report.files_parsed, 2);
        assert!(report.mermaid.contains("mod_a"));
        assert!(report.mermaid.contains("mod_b"));
        assert!(report.mermaid.contains(" -->|\""));
    }

    #[test]
    fn analyze_skips_invalid_utf8_into_failures() {
        // Behaviour change (issue #73): a single broken file used to abort
        // the run via `?`. It must now be caught and surfaced via
        // `AnalyzeReport::failures` instead.
        let tmp = tempdir().expect("tmp");
        fs::write(tmp.path().join("bad.rs"), [0xff, 0xfe, 0xfd]).expect("write");
        let report = analyze(tmp.path(), &AnalyzeOptions::default()).expect("must not abort");
        assert_eq!(report.files_parsed, 0);
        assert_eq!(report.failures.len(), 1);
        assert!(report.failures[0].path.ends_with("bad.rs"));
    }

    #[test]
    fn parse_phase_skips_broken_file() {
        // A directory mixing one valid and one broken Rust file must:
        // - return Ok (no abort),
        // - parse the valid file (files_parsed = 1, ≥ 1 atom),
        // - record the broken file in `failures`.
        let tmp = tempdir().expect("tmp");
        let root = tmp.path();
        write(root, "src/lib.rs", "fn ok() {}\n");
        // Invalid UTF-8 — trips `CodeParser::parse`'s utf-8 check, which is
        // the cleanest deterministic parse failure across grammar versions.
        fs::write(root.join("src/broken.rs"), [0xff, 0xfe, 0xfd]).expect("write");

        let report = analyze(root, &AnalyzeOptions::default()).expect("must not abort");
        assert_eq!(report.files_parsed, 1, "valid file still parsed");
        assert!(report.atoms_indexed >= 1);
        assert_eq!(report.failures.len(), 1);
        assert!(
            report.failures[0].path.ends_with("broken.rs"),
            "got: {}",
            report.failures[0].path
        );
        assert!(
            !report.failures[0].reason.is_empty(),
            "reason must be populated"
        );
    }

    #[test]
    fn walk_exclude_skips_named_dir() {
        let tmp = tempdir().expect("tmp");
        let root = tmp.path();
        write(root, "src/a.rs", "fn a() {}");
        // Use names that are NOT in DEFAULT_EXCLUDED_DIRS so the default
        // walk picks them up; the explicit exclude must then drop them.
        write(root, "workspaces/junk.rs", "fn dont_parse() {}");
        write(root, "examples/inner.rs", "fn dont_parse() {}");

        let default = walk_for_languages(root).expect("walk");
        assert_eq!(default.len(), 3, "default should walk all 3");

        let filtered =
            walk_for_languages_with_exclude(root, &["workspaces", "examples"]).expect("walk");
        assert_eq!(filtered.len(), 1);
        assert!(filtered[0].0.to_string_lossy().ends_with("a.rs"));
    }

    #[test]
    fn walk_exclude_does_not_override_builtin_skip_set() {
        let tmp = tempdir().expect("tmp");
        let root = tmp.path();
        write(root, "src/a.rs", "fn a() {}");
        write(root, "target/junk.rs", "fn dont_parse() {}");

        let no_exclude = walk_for_languages(root).expect("walk");
        let with_exclude = walk_for_languages_with_exclude(root, &["unrelated"]).expect("walk");
        assert_eq!(no_exclude.len(), 1);
        assert_eq!(with_exclude.len(), 1);
    }

    #[test]
    fn walk_skips_default_excluded_dirs_for_each_ecosystem() {
        let tmp = tempdir().expect("tmp");
        let root = tmp.path();
        // One real source file…
        write(root, "src/main.rs", "fn main() {}");
        // …plus a sample of every default-excluded dir.
        for dir in [
            "target",
            "node_modules",
            "__pycache__",
            "venv",
            "dist",
            "build",
            "vendor",
        ] {
            write(root, &format!("{dir}/junk.rs"), "fn nope() {}");
        }
        let files = walk_for_languages(root).expect("walk");
        assert_eq!(files.len(), 1, "only src/main.rs should survive");
        assert!(files[0].0.to_string_lossy().ends_with("main.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn walk_does_not_follow_symlinks_or_recurse_loops() {
        // A self-referential symlink inside the tree must not crash or hang.
        let tmp = tempdir().expect("tmp");
        let root = tmp.path();
        write(root, "src/a.rs", "fn a() {}");
        // Create `src/loop` → `src` (relative). Walking through this would
        // infinite-loop without the symlink guard.
        std::os::unix::fs::symlink(".", root.join("src/loop")).expect("symlink");
        let files = walk_for_languages(root).expect("walk");
        assert_eq!(files.len(), 1);
        assert!(files[0].0.to_string_lossy().ends_with("a.rs"));
    }

    #[test]
    fn analyze_with_exclude_skips_directory() {
        let tmp = tempdir().expect("tmp");
        let root = tmp.path();
        write(root, "src/main.rs", "pub fn main() {}");
        write(root, "examples/lib.rs", "pub fn example() {}");

        let report = analyze(root, &AnalyzeOptions::default()).expect("analyze");
        assert_eq!(report.files_parsed, 2);

        let report = analyze(
            root,
            &AnalyzeOptions {
                exclude: vec!["examples".to_owned()],
                ..AnalyzeOptions::default()
            },
        )
        .expect("analyze");
        assert_eq!(report.files_parsed, 1);
    }

    #[test]
    fn analyze_options_default_uses_project_level() {
        let opts = AnalyzeOptions::default();
        assert_eq!(opts.level, Level::Project);
    }

    #[test]
    fn analyze_report_clone_eq_debug() {
        let r = AnalyzeReport {
            mermaid: "graph TD\n".to_owned(),
            files_parsed: 2,
            atoms_indexed: 4,
            edges_resolved: 1,
            failures: Vec::new(),
        };
        let c = r.clone();
        assert_eq!(r, c);
        assert!(format!("{r:?}").contains("AnalyzeReport"));
    }

    #[test]
    fn bundle_with_sequences_emits_only_for_non_empty_bodies() {
        // Two free fns: one calls another (non-empty body), one is empty.
        // The empty one must be skipped; the non-empty one must appear in
        // both `artifacts.sequences` and as `sequence_path` in index.json.
        let tmp = tempdir().expect("tmp");
        write(
            tmp.path(),
            "src/lib.rs",
            "pub fn caller() { helper(); }\n\
             pub fn empty() {}\n\
             pub fn helper() {}\n",
        );

        let opts = AnalyzeOptions {
            with_sequences: true,
            ..AnalyzeOptions::default()
        };
        let (artifacts, _report) = bundle(tmp.path(), &opts).expect("bundle");

        // `caller` has one CALL step → must appear. `empty` and `helper`
        // have no calls → no diagram, skipped.
        let seq_ids: Vec<&str> = artifacts
            .sequences
            .iter()
            .map(|s| s.entity_id.as_str())
            .collect();
        assert!(
            seq_ids.contains(&"code:src/lib.rs::function::caller"),
            "expected caller in sequences, got: {seq_ids:?}"
        );
        assert!(
            !seq_ids.contains(&"code:src/lib.rs::function::empty"),
            "empty-body fn must not produce a sequence"
        );

        // index.json must mirror that: caller has sequence_path, empty doesn't.
        let entities = artifacts.index_json["entities"]
            .as_array()
            .expect("entities array");
        let caller = entities
            .iter()
            .find(|e| e["id"] == "code:src/lib.rs::function::caller")
            .expect("caller entry");
        assert_eq!(
            caller["sequence_path"]
                .as_str()
                .expect("sequence_path string"),
            "sequences/code_src_lib.rs__function__caller.mmd"
        );
        let empty = entities
            .iter()
            .find(|e| e["id"] == "code:src/lib.rs::function::empty")
            .expect("empty entry");
        assert!(
            empty.get("sequence_path").is_none(),
            "empty fn must not have sequence_path"
        );
    }

    #[test]
    fn bundle_without_with_sequences_emits_no_sequence_layer() {
        let tmp = tempdir().expect("tmp");
        write(
            tmp.path(),
            "src/lib.rs",
            "pub fn caller() { helper(); }\npub fn helper() {}\n",
        );

        let (artifacts, _report) = bundle(tmp.path(), &AnalyzeOptions::default()).expect("bundle");

        assert!(artifacts.sequences.is_empty());
        let entities = artifacts.index_json["entities"]
            .as_array()
            .expect("entities array");
        for e in entities {
            assert!(
                e.get("sequence_path").is_none(),
                "no entity should carry sequence_path when the flag is off: {e}"
            );
        }
    }

    #[test]
    fn bundle_with_sequences_writes_sequences_dir_on_disk() {
        use crate::artifacts::write_artifacts;
        let tmp = tempdir().expect("tmp");
        write(
            tmp.path(),
            "src/lib.rs",
            "pub fn caller() { helper(); }\npub fn helper() {}\n",
        );
        let opts = AnalyzeOptions {
            with_sequences: true,
            ..AnalyzeOptions::default()
        };
        let (artifacts, _report) = bundle(tmp.path(), &opts).expect("bundle");

        let out = tmp.path().join("bundle-out");
        write_artifacts(&artifacts, &out).expect("write");

        let seq_dir = out.join("sequences");
        assert!(seq_dir.is_dir(), "sequences/ dir must exist");
        let written: Vec<_> = std::fs::read_dir(&seq_dir)
            .expect("readdir")
            .filter_map(std::result::Result::ok)
            .map(|e| e.file_name().into_string().unwrap_or_default())
            .collect();
        assert!(
            written.iter().any(|n| n.contains("caller")
                && Path::new(n)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("mmd"))),
            "expected a caller .mmd, got: {written:?}"
        );
    }
}
