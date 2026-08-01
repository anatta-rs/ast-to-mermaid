//! Per-subcommand argument structs and the [`ExitCode`] mapping that the
//! [`crate::cli::run`] handlers return.

use crate::cache::Cache;
use std::path::{Path, PathBuf};
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

/// Output format for the analyze-flavoured subcommands. `Mermaid` is the
/// default and what the renderers natively emit; `Dot` post-processes that
/// into Graphviz DOT for graphs too large for browser-based mermaid
/// rendering (GitHub caps at 500 edges).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum AnalyzeFormat {
    /// Mermaid (default — renders in GitHub markdown, mermaid.live, etc.).
    #[default]
    Mermaid,
    /// Graphviz DOT — pipe to `dot -Tsvg` for huge graphs (10k+ nodes).
    Dot,
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

    /// Include generated Dart (`.g.dart`, `.freezed.dart`, `.mocks.dart`,
    /// `.gr.dart`). Skipped by default: it is `build_runner` output, 27% of
    /// the bytes on a typical Flutter project and no architectural signal.
    #[arg(long)]
    pub include_generated: bool,

    /// Write output to this file instead of stdout.
    #[arg(short, long)]
    pub out: Option<PathBuf>,

    /// Read source from a git ref (e.g. `main`, `v0.1.0`, `HEAD~3`)
    /// instead of the working tree. The path argument becomes a
    /// subdirectory hint within that ref's tree.
    #[arg(long, value_name = "GIT-REF")]
    pub r#ref: Option<String>,

    /// Output format. `mermaid` (default) renders natively in GitHub
    /// markdown / mermaid.live up to ~500 edges. `dot` emits Graphviz DOT
    /// for graphs too large for those renderers — pipe to
    /// `dot -Tsvg > graph.svg`.
    #[arg(long, value_enum, default_value_t = AnalyzeFormat::Mermaid)]
    pub format: AnalyzeFormat,
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

    /// Include generated Dart (`.g.dart`, `.freezed.dart`, `.mocks.dart`,
    /// `.gr.dart`). Skipped by default: it is `build_runner` output, 27% of
    /// the bytes on a typical Flutter project and no architectural signal.
    #[arg(long)]
    pub include_generated: bool,

    /// Read source from a git ref instead of the working tree. With `--ref`,
    /// `walk` lists `git ls-tree` paths (filtered to supported languages).
    #[arg(long, value_name = "GIT-REF")]
    pub r#ref: Option<String>,
}

/// Shared CLI args for cache-touching subcommands (`index`, `diff`).
#[derive(Debug, Clone, Default, clap::Args)]
pub struct CacheArgs {
    /// Override the cache root (default: `<git-toplevel>/.a2m/cache`).
    /// Useful for CI (per-job dirs), shared caches, or XDG opt-in.
    #[arg(long, value_name = "DIR", global = true)]
    pub cache_dir: Option<PathBuf>,

    /// Bypass the persistent cache entirely. Bundles are materialized in
    /// a tempdir for the duration of the command and cleaned up at exit.
    /// Useful for cold-path benchmarks or to verify the persistent cache
    /// isn't lying. Cannot share data across runs.
    #[arg(long, global = true)]
    pub no_cache: bool,
}

impl CacheArgs {
    /// Resolve the effective cache root for `path`, honoring overrides.
    /// Returns `(root, ephemeral_handle)` — keep `ephemeral_handle` alive
    /// for the duration of the command; dropping it deletes the tempdir.
    ///
    /// # Errors
    /// Propagates I/O errors from tempdir creation when `no_cache` is set.
    pub fn resolve(
        &self,
        path: &Path,
    ) -> Result<(PathBuf, Option<tempfile::TempDir>), crate::error::AstToMermaidError> {
        if self.no_cache {
            let dir = tempfile::Builder::new().prefix("a2m-no-cache-").tempdir()?;
            return Ok((dir.path().to_path_buf(), Some(dir)));
        }
        let root = self.cache_dir.clone().unwrap_or_else(|| {
            let toplevel =
                crate::git_source::show_toplevel(path).unwrap_or_else(|_| path.to_path_buf());
            Cache::default_root(&toplevel)
        });
        Ok((root, None))
    }
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

    /// Also emit `sequences/<id>.mmd` for every Rust function whose body
    /// has at least one step. Off by default; roughly doubles wall-time.
    #[arg(long)]
    pub with_sequences: bool,

    /// Shared cache flags (`--cache-dir`, `--no-cache`).
    #[command(flatten)]
    pub cache: CacheArgs,
}

/// CLI args for the `diff` subcommand.
#[derive(Debug, Clone, clap::Args)]
pub struct DiffFlags {
    /// `<ref-a>..<ref-b>` — set-diff bundle(a) → bundle(b). Mirrors
    /// `git diff a..b` syntax.
    pub range: String,

    /// Path inside the repo (subdir hint). Default = current directory.
    #[arg(long, default_value = ".")]
    pub path: PathBuf,

    /// Output format.
    #[arg(long, value_enum, default_value_t = DiffFormat::Mermaid)]
    pub format: DiffFormat,

    /// Shared cache flags (`--cache-dir`, `--no-cache`).
    #[command(flatten)]
    pub cache: CacheArgs,
}

/// Output format for `a2m diff`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum DiffFormat {
    /// Annotated Mermaid graph (human-readable).
    Mermaid,
    /// Structured JSON delta (machine-readable).
    Json,
}

/// CLI args for the `gc` subcommand.
#[derive(Debug, Clone, clap::Args)]
pub struct GcFlags {
    /// Path inside the repo. Used only to locate the cache root via
    /// `git rev-parse --show-toplevel`.
    #[arg(long, default_value = ".")]
    pub path: PathBuf,

    /// Soft cap in bytes (suffixes: `K`, `M`, `G`). Default 1G.
    #[arg(long, value_name = "SIZE", default_value = "1G")]
    pub max_size: String,

    /// Evict entries older than this duration (e.g. `30d`, `2w`, `12h`).
    /// No default — when unset, eviction is purely size-based.
    #[arg(long, value_name = "DURATION")]
    pub older_than: Option<String>,

    /// Compute what would be removed, but don't touch the filesystem.
    #[arg(long)]
    pub dry_run: bool,

    /// Shared cache flags (`--cache-dir`, `--no-cache`).
    #[command(flatten)]
    pub cache: CacheArgs,
}

/// CLI args for the `bundle` subcommand.
#[derive(Debug, Clone, clap::Args)]
pub struct BundleFlags {
    /// Path to a source root (file or directory).
    pub path: PathBuf,

    /// Output directory for the bundle (`overview.mmd`, `index.json`,
    /// `entities/<id>.mmd`, `entities/<id>.meta.json`, and optionally
    /// `sequences/<id>.mmd` when `--with-sequences` is set).
    #[arg(short, long)]
    pub out: PathBuf,

    /// Extra directory basenames to skip (comma-separated). Always combined
    /// with the built-in skip set.
    #[arg(short = 'x', long, default_value = "")]
    pub exclude: String,

    /// Include generated Dart (`.g.dart`, `.freezed.dart`, `.mocks.dart`,
    /// `.gr.dart`). Skipped by default: it is `build_runner` output, 27% of
    /// the bytes on a typical Flutter project and no architectural signal.
    #[arg(long)]
    pub include_generated: bool,

    /// Read source from a git ref (e.g. `main`, `v0.1.0`, `HEAD~3`)
    /// instead of the working tree.
    #[arg(long, value_name = "GIT-REF")]
    pub r#ref: Option<String>,

    /// Also emit `sequences/<id>.mmd` for every Rust function whose body
    /// has at least one step. Off by default; roughly doubles wall-time.
    #[arg(long)]
    pub with_sequences: bool,

    /// Allow bundling to overwrite a populated `--out` dir even when the
    /// new run produced zero entities. Default refuses, because the
    /// orphan-prune step would otherwise wipe every `.mmd` and
    /// `.meta.json` under `entities/` (and `sequences/`) — pointing
    /// `--out` at an existing bundle dir while passing an empty/wrong
    /// source path would silently destroy the previous run.
    #[arg(long)]
    pub allow_empty: bool,
}

/// CLI args for the `sequence` subcommand.
#[derive(Debug, Clone, clap::Args)]
pub struct SequenceFlags {
    /// Path to a source root (file or directory).
    pub path: PathBuf,

    /// Function to render. Plain `name` for a free function,
    /// `Type::method` to disambiguate by impl owner. Required unless
    /// `--all` is set.
    #[arg(short, long, conflicts_with = "all")]
    pub target: Option<String>,

    /// Render every function in the source tree. One `.mmd` file is
    /// written per function; requires `--out <DIR>`.
    #[arg(long, conflicts_with = "target")]
    pub all: bool,

    /// Extra directory basenames to skip during the walk
    /// (comma-separated). Combined with the built-in skip set.
    #[arg(short = 'x', long, default_value = "")]
    pub exclude: String,

    /// Include generated Dart (`.g.dart`, `.freezed.dart`, `.mocks.dart`,
    /// `.gr.dart`). Skipped by default: it is `build_runner` output, 27% of
    /// the bytes on a typical Flutter project and no architectural signal.
    #[arg(long)]
    pub include_generated: bool,

    /// In single-target mode: file to write Mermaid output to (default
    /// stdout). In `--all` mode: directory that receives one `.mmd` per
    /// function.
    #[arg(short, long)]
    pub out: Option<PathBuf>,

    /// Read source from a git ref instead of the working tree.
    #[arg(long, value_name = "GIT-REF")]
    pub r#ref: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_converts_to_process_exit_code() {
        let _ = process::ExitCode::from(ExitCode::Success);
        let _ = process::ExitCode::from(ExitCode::Failure);
        let _ = process::ExitCode::from(ExitCode::UsageError);
    }

    #[test]
    fn cache_args_resolve_no_cache_returns_tempdir_handle() {
        let args = CacheArgs {
            cache_dir: None,
            no_cache: true,
        };
        let (root, handle) = args.resolve(Path::new("/tmp")).expect("resolve no_cache");
        assert!(handle.is_some(), "no_cache must keep a tempdir alive");
        assert!(root.exists(), "tempdir root must exist while handle held");
    }

    #[test]
    fn cache_args_resolve_explicit_cache_dir_wins() {
        let tmp = tempfile::tempdir().expect("tmp");
        let explicit = tmp.path().join("explicit-cache");
        let args = CacheArgs {
            cache_dir: Some(explicit.clone()),
            no_cache: false,
        };
        let (root, handle) = args.resolve(Path::new("/tmp")).expect("resolve");
        assert_eq!(root, explicit);
        assert!(handle.is_none(), "explicit cache_dir is persistent");
    }

    #[test]
    fn cache_args_resolve_default_outside_git_falls_back_to_path() {
        let tmp = tempfile::tempdir().expect("tmp");
        let args = CacheArgs::default();
        let (root, handle) = args.resolve(tmp.path()).expect("resolve");
        // Outside a git repo, default_root is built from `path` itself.
        assert_eq!(root, Cache::default_root(tmp.path()));
        assert!(handle.is_none());
    }
}

/// CLI args for the `flow` subcommand.
#[derive(Debug, Clone, clap::Args)]
pub struct FlowFlags {
    /// Path to a source root (file or directory).
    pub path: PathBuf,

    /// Entry point to walk forward from: `name` for a free function,
    /// `Type::method` to disambiguate by owner.
    #[arg(short, long)]
    pub target: String,

    /// How many call hops to follow. `1` shows only direct calls.
    #[arg(long, default_value_t = crate::render::flow::DEFAULT_DEPTH)]
    pub depth: u8,

    /// Show external leaves (calls into code outside the graph) at every
    /// depth, not just the first.
    #[arg(long, conflicts_with = "no_external")]
    pub include_external: bool,

    /// Hide external leaves entirely.
    #[arg(long)]
    pub no_external: bool,

    /// Extra directory basenames to skip during walk (comma-separated).
    #[arg(short = 'x', long, default_value = "")]
    pub exclude: String,

    /// Include generated Dart (`.g.dart`, `.freezed.dart`, `.mocks.dart`,
    /// `.gr.dart`). Skipped by default.
    #[arg(long)]
    pub include_generated: bool,

    /// Write output to this file instead of stdout.
    #[arg(short, long)]
    pub out: Option<PathBuf>,
}
