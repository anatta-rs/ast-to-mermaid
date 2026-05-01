//! Shared CLI infrastructure for the unified `a2m` binary.
//!
//! The crate ships one binary (`a2m`) with seven subcommands. Per-subcommand
//! arg structs and dispatch helpers live here so the binary file itself stays
//! a thin clap parser.

use crate::artifacts::write_artifacts;
use crate::cache::Cache;
use crate::pipeline::{AnalyzeOptions, analyze, bundle, snapshot_id, walk_for_languages_with_exclude};
use crate::render::Level;
use std::path::PathBuf;
use std::process;

/// Exit code returned by CLI functions, convertible into [`process::ExitCode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    /// Command succeeded.
    Success,
    /// Command failed at runtime (e.g. parse error, IO error).
    Failure,
    /// User error (unknown subcommand, bad flags).
    UsageError,
}

impl From<ExitCode> for process::ExitCode {
    fn from(c: ExitCode) -> Self {
        match c {
            ExitCode::Success => Self::SUCCESS,
            ExitCode::Failure => Self::FAILURE,
            ExitCode::UsageError => Self::from(2),
        }
    }
}

/// Shared CLI args for the analyze-flavoured subcommands
/// (`project`, `overview`, `module`, `function`, `impact`).
#[derive(Debug, Clone, clap::Args)]
pub struct AnalyzeFlags {
    /// Path to a source root (file or directory).
    pub path: PathBuf,

    /// Required for `module` / `function` / `impact` levels: the target
    /// module path or symbol name. Ignored by `project` / `overview`.
    #[arg(short, long)]
    pub target: Option<String>,

    /// Extra directory basenames to skip during walk (comma-separated).
    /// Always combined with the built-in skip set (`target`,
    /// `node_modules`, `.git`, hidden dirs).
    #[arg(short = 'x', long, default_value = "")]
    pub exclude: String,

    /// Write Mermaid output to this file instead of stdout.
    #[arg(short, long)]
    pub out: Option<PathBuf>,

    /// Read source from a git ref (e.g. `main`, `v0.1.0`, `HEAD~3`)
    /// instead of the working tree. The path argument becomes a
    /// subdirectory hint within that ref's tree.
    #[arg(long, value_name = "GIT-REF")]
    pub r#ref: Option<String>,
}

/// Run the analyze pipeline for `level`, writing the resulting Mermaid to
/// `flags.out` or stdout. Returns the program's exit code.
///
/// # Errors
///
/// All failures are reported via `eprintln!` and surfaced as
/// `ExitCode::Failure`. Bad CLI input (missing target for a level that
/// requires one) yields `ExitCode::UsageError`.
pub fn run_analyze(level: Level, flags: &AnalyzeFlags) -> ExitCode {
    if level.requires_target() && flags.target.is_none() {
        eprintln!("level={level} requires --target");
        return ExitCode::UsageError;
    }

    let exclude: Vec<String> = flags
        .exclude
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();

    let opts = AnalyzeOptions {
        level,
        target: flags.target.clone(),
        exclude,
        git_ref: flags.r#ref.clone(),
    };

    let report = match analyze(&flags.path, &opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("analyze: {e}");
            return ExitCode::Failure;
        }
    };

    let suffix = if let Some(path) = flags.out.as_deref() {
        if let Err(e) = std::fs::write(path, &report.mermaid) {
            eprintln!("write {}: {e}", path.display());
            return ExitCode::Failure;
        }
        format!(" → {}", path.display())
    } else {
        print!("{}", report.mermaid);
        String::new()
    };

    eprintln!(
        "analyzed {} files, {} atoms, {} cross-module edges{}",
        report.files_parsed, report.atoms_indexed, report.edges_resolved, suffix,
    );
    ExitCode::Success
}

/// CLI args for the `walk` subcommand.
#[derive(Debug, Clone, clap::Args)]
pub struct WalkFlags {
    /// Path to a source root.
    pub path: PathBuf,

    /// Extra directory basenames to skip (comma-separated). Always combined
    /// with the built-in skip set (`target`, `node_modules`, `.git`,
    /// hidden dirs).
    #[arg(short = 'x', long, default_value = "")]
    pub exclude: String,

    /// Read source from a git ref instead of the working tree. With `--ref`,
    /// `walk` lists `git ls-tree` paths (filtered to supported languages).
    #[arg(long, value_name = "GIT-REF")]
    pub r#ref: Option<String>,
}

/// Run the file-walker subcommand: print one line per source file, format
/// `<lang>\t<path>`, to stdout.
pub fn run_walk(flags: &WalkFlags) -> ExitCode {
    if let Some(git_ref) = flags.r#ref.as_deref() {
        return run_walk_ref(&flags.path, git_ref);
    }
    let exclude: Vec<String> = flags
        .exclude
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();

    match walk_for_languages_with_exclude(&flags.path, &exclude) {
        Ok(files) => {
            for (path, lang) in files {
                println!("{}\t{}", lang.name(), path.display());
            }
            ExitCode::Success
        }
        Err(e) => {
            eprintln!("walk: {e}");
            ExitCode::Failure
        }
    }
}

fn run_walk_ref(start: &std::path::Path, git_ref: &str) -> ExitCode {
    use crate::git_source;
    use crate::parser::Language;

    let toplevel = match git_source::show_toplevel(start) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("walk: {e}");
            return ExitCode::Failure;
        }
    };
    let entries = match git_source::ls_tree(&toplevel, git_ref) {
        Ok(es) => es,
        Err(e) => {
            eprintln!("walk: {e}");
            return ExitCode::Failure;
        }
    };
    for entry in entries {
        let lang = match std::path::Path::new(&entry.path)
            .extension()
            .and_then(|e| e.to_str())
        {
            Some("rs") => Language::Rust,
            Some("py") => Language::Python,
            _ => continue,
        };
        println!("{}\t{}", lang.name(), entry.path);
    }
    ExitCode::Success
}

/// CLI args for the `index` subcommand.
#[derive(Debug, Clone, clap::Args)]
pub struct IndexFlags {
    /// Path to a source root. Used as a subdir hint when `--ref` is set.
    pub path: PathBuf,

    /// Read source from a git ref. Without this, the working tree is
    /// indexed under a synthetic `wt-<digest>` snapshot id.
    #[arg(long, value_name = "GIT-REF")]
    pub r#ref: Option<String>,

    /// Re-materialize the bundle even if a cached one exists.
    #[arg(long)]
    pub force: bool,

    /// Override the cache root (default: `<repo>/.a2m/cache`).
    #[arg(long, value_name = "DIR")]
    pub cache_dir: Option<PathBuf>,
}

/// Run the `index` subcommand: materialize a bundle for a ref (or the
/// working tree) into the cache. Idempotent — cached re-runs are a no-op
/// unless `--force` is set.
pub fn run_index(flags: &IndexFlags) -> ExitCode {
    let cache_root = flags.cache_dir.clone().unwrap_or_else(|| {
        let repo_root = crate::git_source::show_toplevel(&flags.path)
            .unwrap_or_else(|_| flags.path.clone());
        Cache::default_root(&repo_root)
    });
    let cache = match Cache::open(&cache_root) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("index: open cache {}: {e}", cache_root.display());
            return ExitCode::Failure;
        }
    };
    if let Err(e) = cache.ensure_gitignore() {
        eprintln!("index: write .gitignore: {e}");
    }

    let sha = match snapshot_id(&flags.path, flags.r#ref.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("index: resolve snapshot: {e}");
            return ExitCode::Failure;
        }
    };

    if cache.has_bundle(&sha) && !flags.force {
        eprintln!("cached {} → {}", sha, cache.bundle_dir(&sha).display());
        return ExitCode::Success;
    }

    let opts = AnalyzeOptions {
        git_ref: flags.r#ref.clone(),
        ..AnalyzeOptions::default()
    };
    let (artifacts, report) = match bundle(&flags.path, &opts) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("index: {e}");
            return ExitCode::Failure;
        }
    };

    let bundle_dir = cache.bundle_dir(&sha);
    if let Err(e) = write_artifacts(&artifacts, &bundle_dir) {
        eprintln!("index: write {}: {e}", bundle_dir.display());
        return ExitCode::Failure;
    }

    eprintln!(
        "indexed {} → {} ({} files, {} atoms, {} edges)",
        sha,
        bundle_dir.display(),
        report.files_parsed,
        report.atoms_indexed,
        report.edges_resolved,
    );
    ExitCode::Success
}

/// CLI args for the `bundle` subcommand.
#[derive(Debug, Clone, clap::Args)]
pub struct BundleFlags {
    /// Path to a source root (file or directory).
    pub path: PathBuf,

    /// Output directory for the bundle (`overview.mmd`, `index.json`,
    /// `entities/<id>.mmd`, `entities/<id>.meta.json`).
    #[arg(short, long)]
    pub out: PathBuf,

    /// Extra directory basenames to skip (comma-separated). Always combined
    /// with the built-in skip set.
    #[arg(short = 'x', long, default_value = "")]
    pub exclude: String,

    /// Read source from a git ref (e.g. `main`, `v0.1.0`, `HEAD~3`)
    /// instead of the working tree.
    #[arg(long, value_name = "GIT-REF")]
    pub r#ref: Option<String>,
}

/// Run the artifact-bundle subcommand: parse → resolve → emit a directory
/// of per-entity Mermaid + metadata files plus a top-level `index.json`.
pub fn run_bundle(flags: &BundleFlags) -> ExitCode {
    let exclude: Vec<String> = flags
        .exclude
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();

    let opts = AnalyzeOptions {
        exclude,
        git_ref: flags.r#ref.clone(),
        ..AnalyzeOptions::default()
    };

    let (artifacts, report) = match bundle(&flags.path, &opts) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("bundle: {e}");
            return ExitCode::Failure;
        }
    };

    if let Err(e) = write_artifacts(&artifacts, &flags.out) {
        eprintln!("bundle: write {}: {e}", flags.out.display());
        return ExitCode::Failure;
    }

    eprintln!(
        "bundled {} files, {} atoms, {} edges, {} entities → {}",
        report.files_parsed,
        report.atoms_indexed,
        report.edges_resolved,
        artifacts.entities.len(),
        flags.out.display(),
    );

    ExitCode::Success
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_level_without_target_returns_usage_error() {
        let flags = AnalyzeFlags {
            path: PathBuf::from("/dev/null"),
            target: None,
            exclude: String::new(),
            out: None,
            r#ref: None,
        };
        let code = run_analyze(Level::Module, &flags);
        assert_eq!(code, ExitCode::UsageError);
    }

    #[test]
    fn analyze_with_missing_path_returns_failure() {
        let flags = AnalyzeFlags {
            path: PathBuf::from("/no/such/path/here"),
            target: None,
            exclude: String::new(),
            out: None,
            r#ref: None,
        };
        let code = run_analyze(Level::Project, &flags);
        assert_eq!(code, ExitCode::Failure);
    }

    #[test]
    fn exit_code_converts_to_process_exit_code() {
        let _ = process::ExitCode::from(ExitCode::Success);
        let _ = process::ExitCode::from(ExitCode::Failure);
        let _ = process::ExitCode::from(ExitCode::UsageError);
    }

    #[test]
    fn project_level_on_empty_dir_succeeds_and_writes_to_file() {
        let tmp = tempfile::tempdir().expect("tmp");
        let out_file = tmp.path().join("out.mmd");
        // Analyze the tempdir itself (no source files → empty diagram).
        let flags = AnalyzeFlags {
            path: tmp.path().to_path_buf(),
            target: None,
            exclude: String::new(),
            out: Some(out_file.clone()),
            r#ref: None,
        };
        let code = run_analyze(Level::Project, &flags);
        assert_eq!(code, ExitCode::Success);
        assert!(out_file.exists(), "output file must be written");
    }

    #[test]
    fn project_level_on_empty_dir_prints_to_stdout() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = AnalyzeFlags {
            path: tmp.path().to_path_buf(),
            target: None,
            exclude: String::new(),
            out: None,
            r#ref: None,
        };
        let code = run_analyze(Level::Project, &flags);
        assert_eq!(code, ExitCode::Success);
    }

    #[test]
    fn walk_on_empty_dir_succeeds() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = WalkFlags {
            path: tmp.path().to_path_buf(),
            exclude: String::new(),
            r#ref: None,
        };
        assert_eq!(run_walk(&flags), ExitCode::Success);
    }

    #[test]
    fn walk_with_missing_path_succeeds_silently() {
        // walk_for_languages returns Ok(empty) for a missing path; the
        // subcommand mirrors that to keep shell-pipeline composition simple.
        let flags = WalkFlags {
            path: PathBuf::from("/no/such/path/here-cli-test"),
            exclude: String::new(),
            r#ref: None,
        };
        assert_eq!(run_walk(&flags), ExitCode::Success);
    }

    #[test]
    fn bundle_on_empty_dir_succeeds_and_writes_index() {
        let tmp = tempfile::tempdir().expect("tmp");
        let out = tmp.path().join("bundle-out");
        let flags = BundleFlags {
            path: tmp.path().to_path_buf(),
            out: out.clone(),
            exclude: String::new(),
            r#ref: None,
        };
        assert_eq!(run_bundle(&flags), ExitCode::Success);
        assert!(out.join("index.json").exists());
        assert!(out.join("overview.mmd").exists());
    }

    #[test]
    fn bundle_with_missing_path_returns_failure() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = BundleFlags {
            path: PathBuf::from("/no/such/path/here-cli-test"),
            out: tmp.path().join("bundle-out"),
            exclude: String::new(),
            r#ref: None,
        };
        assert_eq!(run_bundle(&flags), ExitCode::Failure);
    }
}
