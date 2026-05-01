//! End-to-end orchestrator: filesystem → parser → store → resolver → renderer.
//!
//! No external graph backend. No `async` I/O — all store operations are
//! synchronous in-memory.

use crate::artifacts::{ArtifactSet, emit_artifacts};
use crate::error::{AstToMermaidError, Result};
use crate::git_source;
use crate::graph::Store;
use crate::parser::{CodeParser, Language, git_blob_sha1};
use crate::render::{Level, render};
use crate::resolve::resolve_cross_module_calls;
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
    /// Optional git ref to read source from (e.g. `main`, `v0.1.0`,
    /// `HEAD~3`). When set, the pipeline reads file contents via
    /// `git cat-file` rather than walking the working tree, and ignores
    /// `exclude` (git ls-tree already excludes ignored files).
    pub git_ref: Option<String>,
}

impl Default for AnalyzeOptions {
    fn default() -> Self {
        Self {
            level: Level::Project,
            target: None,
            exclude: Vec::new(),
            git_ref: None,
        }
    }
}

/// One unit of work for the parse loop: a logical path label, the bytes to
/// parse, and the language. Produced by either the FS walk or the git tree.
struct ParseInput {
    /// Display path used for entity ids (relative-to-root, slash-separated).
    display_path: String,
    /// File contents as bytes.
    content: Vec<u8>,
    /// Language detected from the file extension.
    language: Language,
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
        let display_path = display_path(root, &path);
        out.push(ParseInput {
            display_path,
            content,
            language,
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
        let content = git_source::cat_file(&toplevel, &entry.blob_sha)?;
        out.push(ParseInput {
            display_path: entry.path,
            content,
            language,
        });
    }
    Ok(out)
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
/// Returns the first error from filesystem walk, parsing, or renderer.
/// Individual files that fail to parse are propagated.
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

    let (files_parsed, atoms_indexed) = parse_phase(&inputs, &store)?;
    let edges_resolved = resolve_phase(&store, atoms_indexed);
    let mermaid = render(opts.level, &store, opts.target.as_deref())?;

    Ok(AnalyzeReport {
        mermaid,
        files_parsed,
        atoms_indexed,
        edges_resolved,
    })
}

/// Run the parse pass over `inputs`, threading each one into `store`.
/// Wrapped in a `tracing::info_span!("parse_phase", ...)` so users can see
/// breakdown via `--trace=info`. Returns `(files_parsed, atoms_indexed)`.
fn parse_phase(inputs: &[ParseInput], store: &Store) -> Result<(usize, usize)> {
    let span = tracing::info_span!("parse_phase", files = inputs.len());
    let _enter = span.enter();
    let started = std::time::Instant::now();

    let mut atoms_indexed = 0;
    let mut files_parsed = 0;
    for input in inputs {
        let parser = match input.language {
            Language::Rust => CodeParser::rust(),
            Language::Python => CodeParser::python(),
        };
        let count = parser
            .parse_into(&input.content, &input.display_path, store)
            .map_err(|e| {
                AstToMermaidError::InvalidInput(format!("parse {}: {e}", input.display_path))
            })?;
        atoms_indexed += count;
        files_parsed += 1;
    }

    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    tracing::info!(parsed = files_parsed, atoms = atoms_indexed, elapsed_ms, "parse_phase done");
    Ok((files_parsed, atoms_indexed))
}

/// Run the cross-module resolver. Wrapped in
/// `tracing::info_span!("resolve_phase", ...)`. Returns the number of edges
/// added by the resolver.
///
/// **V1.5 decision gate**: the per-call elapsed and atom-count fields are
/// the data we'll use to decide whether V2 (edge-level cache) is justified
/// (resolve-phase wall-time as a fraction of the whole pipeline on real
/// repos — see issue #47).
fn resolve_phase(store: &Store, atoms: usize) -> usize {
    let span = tracing::info_span!("resolve_phase", atoms);
    let _enter = span.enter();
    let started = std::time::Instant::now();

    let edges_resolved = resolve_cross_module_calls(store);

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
/// Returns the first error from filesystem walk, parsing, or
/// resolver. Individual files that fail to parse are propagated.
pub fn bundle(root: &Path, opts: &AnalyzeOptions) -> Result<(ArtifactSet, AnalyzeReport)> {
    if opts.git_ref.is_none() && !root.exists() {
        return Err(AstToMermaidError::InvalidInput(format!(
            "path does not exist: {}",
            root.display()
        )));
    }

    let inputs = collect_inputs(root, opts)?;
    let store = Store::new();

    let (files_parsed, atoms_indexed) = parse_phase(&inputs, &store)?;
    let edges_resolved = resolve_phase(&store, atoms_indexed);

    let source_root = match opts.git_ref.as_deref() {
        Some(ref_name) => format!("{}@{ref_name}", root.display()),
        None => root.to_string_lossy().into_owned(),
    };
    let artifacts = emit_artifacts(&store, &source_root);

    let report = AnalyzeReport {
        // The "rendered" mermaid for a bundle is the project view —
        // matches what `analyze --level project` would have returned.
        mermaid: artifacts.overview_mmd.clone(),
        files_parsed,
        atoms_indexed,
        edges_resolved,
    };
    Ok((artifacts, report))
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
    let entries = std::fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
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
                git_ref: None,
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
    fn analyze_propagates_invalid_utf8() {
        let tmp = tempdir().expect("tmp");
        let path = tmp.path().join("bad.rs");
        fs::write(&path, [0xff, 0xfe, 0xfd]).expect("write");
        let err = analyze(tmp.path(), &AnalyzeOptions::default()).expect_err("must reject");
        assert!(matches!(err, AstToMermaidError::InvalidInput(_)));
    }

    #[test]
    fn walk_exclude_skips_named_dir() {
        let tmp = tempdir().expect("tmp");
        let root = tmp.path();
        write(root, "src/a.rs", "fn a() {}");
        write(root, "vendor/junk.rs", "fn dont_parse() {}");
        write(root, "workspaces/inner.rs", "fn dont_parse() {}");

        let default = walk_for_languages(root).expect("walk");
        assert_eq!(default.len(), 3, "default should walk all 3");

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

        let no_exclude = walk_for_languages(root).expect("walk");
        let with_exclude = walk_for_languages_with_exclude(root, &["unrelated"]).expect("walk");
        assert_eq!(no_exclude.len(), 1);
        assert_eq!(with_exclude.len(), 1);
    }

    #[test]
    fn analyze_with_exclude_skips_directory() {
        let tmp = tempdir().expect("tmp");
        let root = tmp.path();
        write(root, "src/main.rs", "pub fn main() {}");
        write(root, "vendor/lib.rs", "pub fn vendored() {}");

        let report = analyze(root, &AnalyzeOptions::default()).expect("analyze");
        assert_eq!(report.files_parsed, 2);

        let report = analyze(
            root,
            &AnalyzeOptions {
                exclude: vec!["vendor".to_owned()],
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
        };
        let c = r.clone();
        assert_eq!(r, c);
        assert!(format!("{r:?}").contains("AnalyzeReport"));
    }
}
