//! End-to-end orchestrator: filesystem → parser → store → resolver → renderer.
//!
//! No external graph backend. No `async` I/O — all store operations are
//! synchronous in-memory.

use crate::artifacts::{ArtifactSet, emit_artifacts};
use crate::error::{AstToMermaidError, Result};
use crate::graph::Store;
use crate::parser::{CodeParser, Language};
use crate::render::{Level, render};
use crate::resolve::{resolve_cross_module_calls, resolve_implements_edges};
use std::path::{Path, PathBuf};

/// Options controlling [`analyze`].
#[derive(Debug, Clone)]
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
}

impl Default for AnalyzeOptions {
    fn default() -> Self {
        Self {
            level: Level::Project,
            target: None,
            exclude: Vec::new(),
        }
    }
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
    if !root.exists() {
        return Err(AstToMermaidError::InvalidInput(format!(
            "path does not exist: {}",
            root.display()
        )));
    }

    let files = walk_for_languages_with_exclude(root, &opts.exclude)?;
    let store = Store::new();

    let mut atoms_indexed = 0;
    let mut files_parsed = 0;
    for (path, lang) in &files {
        let bytes = std::fs::read(path)?;
        let parser = match lang {
            Language::Rust => CodeParser::rust(),
            Language::Python => CodeParser::python(),
        };
        let display_path = display_path(root, path);
        let count = parser
            .parse_into(&bytes, &display_path, &store)
            .map_err(|e| AstToMermaidError::InvalidInput(format!("parse {display_path}: {e}")))?;
        atoms_indexed += count;
        files_parsed += 1;
    }

    let edges_resolved = resolve_cross_module_calls(&store) + resolve_implements_edges(&store);
    let mermaid = render(opts.level, &store, opts.target.as_deref())?;

    Ok(AnalyzeReport {
        mermaid,
        files_parsed,
        atoms_indexed,
        edges_resolved,
    })
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
    if !root.exists() {
        return Err(AstToMermaidError::InvalidInput(format!(
            "path does not exist: {}",
            root.display()
        )));
    }

    let files = walk_for_languages_with_exclude(root, &opts.exclude)?;
    let store = Store::new();

    let mut atoms_indexed = 0;
    let mut files_parsed = 0;
    for (path, lang) in &files {
        let bytes = std::fs::read(path)?;
        let parser = match lang {
            Language::Rust => CodeParser::rust(),
            Language::Python => CodeParser::python(),
        };
        let display_path = display_path(root, path);
        let count = parser
            .parse_into(&bytes, &display_path, &store)
            .map_err(|e| AstToMermaidError::InvalidInput(format!("parse {display_path}: {e}")))?;
        atoms_indexed += count;
        files_parsed += 1;
    }

    let edges_resolved = resolve_cross_module_calls(&store) + resolve_implements_edges(&store);

    let source_root = root.to_string_lossy();
    let artifacts = emit_artifacts(&store, source_root.as_ref());

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
        };
        let c = r.clone();
        assert_eq!(r, c);
        assert!(format!("{r:?}").contains("AnalyzeReport"));
    }
}
