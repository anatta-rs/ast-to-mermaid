//! End-to-end orchestrator: filesystem → ingester → store → resolver → renderer.
//!
//! Used by both the CLI and (later) the MCP server. Pure async; backed by an
//! [`InMemoryStore`] for the v0.2 MVP.

use crate::error::{AstToMermaidError, Result};
use crate::graph::{InMemoryStore, ingest_parse_output};
use crate::render::{Level, render};
use crate::resolve::resolve_cross_module_calls;
use ingester_code::{CodeParser, Language};
use ingester_core::{Origin, Parser};
use polystore::Scope;
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
