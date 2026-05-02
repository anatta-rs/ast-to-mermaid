//! Shared CLI infrastructure for the unified `a2m` binary.
//!
//! The crate ships one binary (`a2m`) with seven subcommands. Per-subcommand
//! arg structs and dispatch helpers live here so the binary file itself stays
//! a thin clap parser.

use crate::artifacts::write_artifacts;
use crate::cache::{Cache, GcOptions, atomic_rename};
use crate::diff::{compute_diff, load_bundle_entities, render_mermaid};
use crate::pipeline::{
    AnalyzeOptions, analyze, bundle, snapshot_id, walk_for_languages_with_exclude,
};
use crate::render::{Level, mermaid_to_dot};
use crate::sequence;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::time::Duration;

/// Open the default cache (`<git-toplevel>/.a2m/cache`) for transparent
/// atom-level caching on the analyze/bundle subcommands. Returns `None` if
/// not in a git repo or if the cache can't be opened — both are non-fatal,
/// the caller falls back to running without atom caching.
fn open_default_cache(start: &Path) -> Option<Arc<Cache>> {
    let toplevel = crate::git_source::show_toplevel(start).ok()?;
    let root = Cache::default_root(&toplevel);
    Cache::open(&root).ok().map(Arc::new)
}

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

    // Wire the cache transparently for analyze-flavoured subcommands when we
    // can find a git toplevel. Failures (not in a git repo, can't open cache)
    // are non-fatal — fall back to no caching.
    let cache = open_default_cache(&flags.path);

    let opts = AnalyzeOptions {
        level,
        target: flags.target.clone(),
        exclude,
        git_ref: flags.r#ref.clone(),
        cache,
    };

    let report = match analyze(&flags.path, &opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("analyze: {e}");
            return ExitCode::Failure;
        }
    };

    let rendered = match flags.format {
        AnalyzeFormat::Mermaid => report.mermaid.clone(),
        AnalyzeFormat::Dot => mermaid_to_dot(&report.mermaid),
    };

    let suffix = if let Some(path) = flags.out.as_deref() {
        if let Err(e) = std::fs::write(path, &rendered) {
            eprintln!("write {}: {e}", path.display());
            return ExitCode::Failure;
        }
        format!(" → {}", path.display())
    } else {
        print!("{rendered}");
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

    /// Shared cache flags (`--cache-dir`, `--no-cache`).
    #[command(flatten)]
    pub cache: CacheArgs,
}

/// Run the `index` subcommand: materialize a bundle for a ref (or the
/// working tree) into the cache. Idempotent — cached re-runs are a no-op
/// unless `--force` is set.
pub fn run_index(flags: &IndexFlags) -> ExitCode {
    let (cache_root, _ephemeral) = match flags.cache.resolve(&flags.path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("index: resolve cache root: {e}");
            return ExitCode::Failure;
        }
    };
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

    if cache.has_bundle(&sha) && !flags.force && !flags.cache.no_cache {
        eprintln!("cached {} → {}", sha, cache.bundle_dir(&sha).display());
        return ExitCode::Success;
    }

    // The parse loop also gets the cache so atom-level dedup applies even
    // during a fresh `index` (cross-ref blob reuse). Cache::open errors are
    // non-fatal; skip atom caching in that case.
    let opts = AnalyzeOptions {
        git_ref: flags.r#ref.clone(),
        cache: Cache::open(&cache_root).ok().map(Arc::new),
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
    if let Err(e) = write_bundle_atomic(&artifacts, &bundle_dir) {
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

/// Run the `diff` subcommand: compute the structural diff between two
/// cached bundles. Auto-runs `index` for any ref that isn't already cached.
#[must_use]
pub fn run_diff(flags: &DiffFlags) -> ExitCode {
    let Some((ref_a, ref_b)) = flags.range.split_once("..") else {
        eprintln!("diff: expected `<ref-a>..<ref-b>`, got `{}`", flags.range);
        return ExitCode::UsageError;
    };
    if ref_a.is_empty() || ref_b.is_empty() {
        eprintln!("diff: both refs must be non-empty in `{}`", flags.range);
        return ExitCode::UsageError;
    }

    let (cache_root, _ephemeral) = match flags.cache.resolve(&flags.path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("diff: resolve cache root: {e}");
            return ExitCode::Failure;
        }
    };
    let cache = match Cache::open(&cache_root) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("diff: open cache {}: {e}", cache_root.display());
            return ExitCode::Failure;
        }
    };

    let from_sha = match ensure_indexed(&cache, &flags.path, ref_a, flags.cache.no_cache) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("diff: index {ref_a}: {e}");
            return ExitCode::Failure;
        }
    };
    let to_sha = match ensure_indexed(&cache, &flags.path, ref_b, flags.cache.no_cache) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("diff: index {ref_b}: {e}");
            return ExitCode::Failure;
        }
    };

    let from_entities = match load_bundle_entities(&cache.bundle_dir(&from_sha)) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("diff: {e}");
            return ExitCode::Failure;
        }
    };
    let to_entities = match load_bundle_entities(&cache.bundle_dir(&to_sha)) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("diff: {e}");
            return ExitCode::Failure;
        }
    };

    // Clone the post-state entities so the renderer can walk their edges
    // (compute_diff consumes its inputs to build the lookup HashMap).
    let to_for_render = to_entities.clone();
    let result = compute_diff(ref_a, ref_b, &from_sha, &to_sha, from_entities, to_entities);

    match flags.format {
        DiffFormat::Mermaid => print!("{}", render_mermaid(&result, &to_for_render)),
        DiffFormat::Json => match serde_json::to_string_pretty(&result) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("diff: serialize json: {e}");
                return ExitCode::Failure;
            }
        },
    }

    eprintln!(
        "diff {ref_a} → {ref_b}: +{} -{} ~{} ↪{}",
        result.added.len(),
        result.removed.len(),
        result.modified.len(),
        result.renamed.len(),
    );
    ExitCode::Success
}

fn ensure_indexed(
    cache: &Cache,
    path: &Path,
    git_ref: &str,
    no_cache: bool,
) -> Result<String, crate::error::AstToMermaidError> {
    let sha = snapshot_id(path, Some(git_ref))?;
    if !no_cache && cache.has_bundle(&sha) {
        return Ok(sha);
    }
    let opts = AnalyzeOptions {
        git_ref: Some(git_ref.to_owned()),
        cache: Some(Arc::new(Cache::open(cache.root())?)),
        ..AnalyzeOptions::default()
    };
    let (artifacts, _report) = bundle(path, &opts)?;
    write_bundle_atomic(&artifacts, &cache.bundle_dir(&sha))?;
    Ok(sha)
}

/// Write a bundle to its final location via tempdir + atomic rename so
/// concurrent runs on the same ref never see a partial bundle. If `final_dir`
/// already exists, it's wiped first (caller checked `has_bundle` if it cared
/// about idempotence).
fn write_bundle_atomic(
    artifacts: &crate::artifacts::ArtifactSet,
    final_dir: &Path,
) -> Result<(), crate::error::AstToMermaidError> {
    let parent = final_dir.parent().ok_or_else(|| {
        crate::error::AstToMermaidError::InvalidInput(format!(
            "bundle final dir has no parent: {}",
            final_dir.display()
        ))
    })?;
    std::fs::create_dir_all(parent)?;
    let pid = std::process::id();
    let stem = final_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("bundle");
    let tmp_dir = parent.join(format!(".{stem}.tmp.{pid}"));
    if tmp_dir.exists() {
        std::fs::remove_dir_all(&tmp_dir)?;
    }
    write_artifacts(artifacts, &tmp_dir)?;
    if final_dir.exists() {
        std::fs::remove_dir_all(final_dir)?;
    }
    atomic_rename(&tmp_dir, final_dir)?;
    Ok(())
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

/// Run the `gc` subcommand: evict old / oversized cache entries.
#[must_use]
pub fn run_gc(flags: &GcFlags) -> ExitCode {
    let max_size = match parse_size(&flags.max_size) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("gc: --max-size: {e}");
            return ExitCode::UsageError;
        }
    };
    let older_than = match flags.older_than.as_deref() {
        Some(s) => match parse_duration(s) {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("gc: --older-than: {e}");
                return ExitCode::UsageError;
            }
        },
        None => None,
    };

    let (cache_root, _ephemeral) = match flags.cache.resolve(&flags.path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("gc: resolve cache root: {e}");
            return ExitCode::Failure;
        }
    };
    let cache = match Cache::open(&cache_root) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("gc: open cache {}: {e}", cache_root.display());
            return ExitCode::Failure;
        }
    };

    let opts = GcOptions {
        max_size_bytes: Some(max_size),
        older_than,
        dry_run: flags.dry_run,
    };
    let report = match cache.gc(&opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("gc: {e}");
            return ExitCode::Failure;
        }
    };

    let verb = if flags.dry_run {
        "would remove"
    } else {
        "removed"
    };
    eprintln!(
        "{verb} {} entries ({} bytes) from {} (had {} entries, {} bytes)",
        report.removed_count,
        report.removed_size,
        cache_root.display(),
        report.count_before,
        report.total_before,
    );
    ExitCode::Success
}

fn parse_size(s: &str) -> std::result::Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty value".into());
    }
    let (num, mult) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1024_u64),
        Some('M' | 'm') => (&s[..s.len() - 1], 1024_u64 * 1024),
        Some('G' | 'g') => (&s[..s.len() - 1], 1024_u64 * 1024 * 1024),
        _ => (s, 1_u64),
    };
    num.parse::<u64>()
        .map_err(|e| format!("expected `<num>[K|M|G]`, got `{s}`: {e}"))
        .map(|n| n * mult)
}

fn parse_duration(s: &str) -> std::result::Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty value".into());
    }
    let (num, secs) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1_u64),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 60 * 60),
        Some('d') => (&s[..s.len() - 1], 24 * 60 * 60),
        Some('w') => (&s[..s.len() - 1], 7 * 24 * 60 * 60),
        _ => (s, 1),
    };
    num.parse::<u64>()
        .map_err(|e| format!("expected `<num>[s|m|h|d|w]`, got `{s}`: {e}"))
        .map(|n| Duration::from_secs(n * secs))
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

    let cache = open_default_cache(&flags.path);

    let opts = AnalyzeOptions {
        exclude,
        git_ref: flags.r#ref.clone(),
        cache,
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

/// CLI args for the `sequence` subcommand.
#[derive(Debug, Clone, clap::Args)]
pub struct SequenceFlags {
    /// Path to a source root (file or directory).
    pub path: PathBuf,

    /// Required: function to render. Plain `name` for a free function,
    /// `Type::method` to disambiguate by impl owner.
    #[arg(short, long)]
    pub target: String,

    /// Extra directory basenames to skip during the walk
    /// (comma-separated). Combined with the built-in skip set.
    #[arg(short = 'x', long, default_value = "")]
    pub exclude: String,

    /// Write output to this file instead of stdout.
    #[arg(short, long)]
    pub out: Option<PathBuf>,

    /// Read source from a git ref instead of the working tree.
    #[arg(long, value_name = "GIT-REF")]
    pub r#ref: Option<String>,
}

/// Run the `sequence` subcommand: scan source files for the target
/// function, re-parse its body in order, render Mermaid `sequenceDiagram`.
pub fn run_sequence(flags: &SequenceFlags) -> ExitCode {
    if flags.target.trim().is_empty() {
        eprintln!("sequence: --target must be non-empty");
        return ExitCode::UsageError;
    }
    let exclude: Vec<String> = flags
        .exclude
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();

    let candidates = match collect_rust_sources(&flags.path, &exclude, flags.r#ref.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("sequence: {e}");
            return ExitCode::Failure;
        }
    };
    if candidates.is_empty() {
        eprintln!(
            "sequence: no Rust files found under {}",
            flags.path.display()
        );
        return ExitCode::Failure;
    }

    let mut diagram = None;
    for (file_rel, content) in &candidates {
        if let Ok(d) = sequence::extract(content, file_rel, &flags.target) {
            diagram = Some((file_rel.clone(), d));
            break;
        }
    }
    let Some((file_rel, diagram)) = diagram else {
        eprintln!(
            "sequence: function `{}` not found under {}",
            flags.target,
            flags.path.display()
        );
        return ExitCode::Failure;
    };
    let rendered = sequence::render(&diagram);

    let suffix = if let Some(path) = flags.out.as_deref() {
        if let Err(e) = std::fs::write(path, &rendered) {
            eprintln!("write {}: {e}", path.display());
            return ExitCode::Failure;
        }
        format!(" → {}", path.display())
    } else {
        print!("{rendered}");
        String::new()
    };
    eprintln!(
        "sequence {} from {} ({} participants, {} steps){}",
        flags.target,
        file_rel,
        diagram.participants.len(),
        diagram.steps.len(),
        suffix,
    );
    ExitCode::Success
}

/// Collect `(display_path, content)` pairs for every Rust source file under
/// `root`, honouring `exclude`. With `git_ref`, reads from `git ls-tree`
/// instead of the working tree.
fn collect_rust_sources(
    root: &Path,
    exclude: &[String],
    git_ref: Option<&str>,
) -> Result<Vec<(String, Vec<u8>)>, crate::error::AstToMermaidError> {
    if let Some(git_ref) = git_ref {
        let toplevel = crate::git_source::show_toplevel(root)?;
        let entries = crate::git_source::ls_tree(&toplevel, git_ref)?;
        let mut out = Vec::new();
        for entry in entries {
            if !Path::new(&entry.path)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("rs"))
            {
                continue;
            }
            let content = crate::git_source::cat_file(&toplevel, &entry.blob_sha)?;
            out.push((entry.path, content));
        }
        Ok(out)
    } else {
        let files = walk_for_languages_with_exclude(root, exclude)?;
        let mut out = Vec::new();
        for (path, lang) in files {
            if lang != crate::parser::Language::Rust {
                continue;
            }
            let content = std::fs::read(&path)?;
            let display = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            out.push((display, content));
        }
        Ok(out)
    }
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
            format: AnalyzeFormat::default(),
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
            format: AnalyzeFormat::default(),
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
            format: AnalyzeFormat::default(),
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
            format: AnalyzeFormat::default(),
        };
        let code = run_analyze(Level::Project, &flags);
        assert_eq!(code, ExitCode::Success);
    }

    #[test]
    fn project_level_dot_format_emits_digraph() {
        let tmp = tempfile::tempdir().expect("tmp");
        let out_file = tmp.path().join("out.dot");
        let flags = AnalyzeFlags {
            path: tmp.path().to_path_buf(),
            target: None,
            exclude: String::new(),
            out: Some(out_file.clone()),
            r#ref: None,
            format: AnalyzeFormat::Dot,
        };
        assert_eq!(run_analyze(Level::Project, &flags), ExitCode::Success);
        let body = std::fs::read_to_string(&out_file).expect("read");
        assert!(body.starts_with("digraph G {"), "got: {body}");
        assert!(body.contains("rankdir=TB"), "got: {body}");
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

    // --- helpers -----------------------------------------------------------

    /// Spawn `git` against `cwd`, scrubbing inherited `GIT_*` env vars so the
    /// tempdir's tiny repo isn't hijacked by an outer `git commit` /
    /// `git push` running our test suite (same rationale as the helper in
    /// `git_source` tests — see that comment for the gory details).
    fn git(cwd: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            panic!("git {args:?} failed: {stderr}");
        }
    }

    /// Initialize a tiny single-file Rust repo with one commit on `main`.
    /// Returns the path (== `dir`) for chaining convenience.
    fn init_rust_repo(dir: &Path, file_rel: &str, content: &str) {
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.email", "t@t"]);
        git(dir, &["config", "user.name", "t"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
        let path = dir.join(file_rel);
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(&path, content).unwrap();
        git(dir, &["add", file_rel]);
        git(dir, &["commit", "-q", "-m", "init"]);
    }

    // --- parse_size --------------------------------------------------------

    #[test]
    fn parse_size_plain_bytes() {
        assert_eq!(parse_size("42").unwrap(), 42);
    }

    #[test]
    fn parse_size_suffixes_kilo_mega_giga() {
        assert_eq!(parse_size("2K").unwrap(), 2 * 1024);
        assert_eq!(parse_size("3m").unwrap(), 3 * 1024 * 1024);
        assert_eq!(parse_size("1G").unwrap(), 1024 * 1024 * 1024);
        // Mixed-case suffixes accepted.
        assert_eq!(parse_size("4k").unwrap(), 4 * 1024);
    }

    #[test]
    fn parse_size_empty_errors() {
        let err = parse_size("   ").unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
    }

    #[test]
    fn parse_size_non_numeric_errors() {
        let err = parse_size("abc").unwrap_err();
        assert!(err.contains("expected"), "got: {err}");
    }

    // --- parse_duration ----------------------------------------------------

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86400));
        assert_eq!(parse_duration("1w").unwrap(), Duration::from_secs(604_800));
        // No suffix => seconds.
        assert_eq!(parse_duration("7").unwrap(), Duration::from_secs(7));
    }

    #[test]
    fn parse_duration_empty_errors() {
        assert!(parse_duration("").unwrap_err().contains("empty"));
    }

    #[test]
    fn parse_duration_non_numeric_errors() {
        let err = parse_duration("xs").unwrap_err();
        assert!(err.contains("expected"), "got: {err}");
    }

    // --- CacheArgs::resolve ------------------------------------------------

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

    // --- run_walk_ref ------------------------------------------------------

    #[test]
    fn walk_with_ref_lists_supported_languages_only() {
        let tmp = tempfile::tempdir().expect("tmp");
        init_rust_repo(tmp.path(), "src/lib.rs", "fn x() {}\n");
        // Add a non-source file in a follow-up commit so it appears in HEAD.
        std::fs::write(tmp.path().join("README.md"), "doc").unwrap();
        git(tmp.path(), &["add", "README.md"]);
        git(tmp.path(), &["commit", "-q", "-m", "doc"]);

        let flags = WalkFlags {
            path: tmp.path().to_path_buf(),
            exclude: String::new(),
            r#ref: Some("HEAD".into()),
        };
        assert_eq!(run_walk(&flags), ExitCode::Success);
    }

    #[test]
    fn walk_with_ref_outside_git_repo_fails() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = WalkFlags {
            path: tmp.path().to_path_buf(),
            exclude: String::new(),
            r#ref: Some("HEAD".into()),
        };
        assert_eq!(run_walk(&flags), ExitCode::Failure);
    }

    #[test]
    fn walk_with_unknown_ref_fails() {
        let tmp = tempfile::tempdir().expect("tmp");
        init_rust_repo(tmp.path(), "src/lib.rs", "fn x() {}\n");
        let flags = WalkFlags {
            path: tmp.path().to_path_buf(),
            exclude: String::new(),
            r#ref: Some("definitely-not-a-ref".into()),
        };
        assert_eq!(run_walk(&flags), ExitCode::Failure);
    }

    #[test]
    fn walk_honours_exclude_list() {
        let tmp = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(tmp.path().join("vendor")).unwrap();
        std::fs::write(tmp.path().join("vendor/skip.rs"), "fn s(){}").unwrap();
        std::fs::write(tmp.path().join("keep.rs"), "fn k(){}").unwrap();
        let flags = WalkFlags {
            path: tmp.path().to_path_buf(),
            exclude: "vendor".into(),
            r#ref: None,
        };
        assert_eq!(run_walk(&flags), ExitCode::Success);
    }

    // --- run_index ---------------------------------------------------------

    #[test]
    fn index_succeeds_then_is_idempotent() {
        let tmp = tempfile::tempdir().expect("tmp");
        init_rust_repo(tmp.path(), "src/lib.rs", "fn a(){}\nfn b(){a()}\n");
        let cache_dir = tmp.path().join("cache");

        let flags = IndexFlags {
            path: tmp.path().to_path_buf(),
            r#ref: Some("HEAD".into()),
            force: false,
            cache: CacheArgs {
                cache_dir: Some(cache_dir.clone()),
                no_cache: false,
            },
        };
        assert_eq!(run_index(&flags), ExitCode::Success);

        // Second run hits the `cached` short-circuit branch.
        assert_eq!(run_index(&flags), ExitCode::Success);

        // With --force, we re-materialize even when the bundle exists.
        let forced = IndexFlags {
            force: true,
            ..flags.clone()
        };
        assert_eq!(run_index(&forced), ExitCode::Success);
    }

    #[test]
    fn index_with_unknown_ref_fails() {
        let tmp = tempfile::tempdir().expect("tmp");
        init_rust_repo(tmp.path(), "src/lib.rs", "fn x(){}\n");
        let flags = IndexFlags {
            path: tmp.path().to_path_buf(),
            r#ref: Some("nope".into()),
            force: false,
            cache: CacheArgs {
                cache_dir: Some(tmp.path().join("cache")),
                no_cache: false,
            },
        };
        assert_eq!(run_index(&flags), ExitCode::Failure);
    }

    #[test]
    fn index_no_cache_skips_persistence() {
        let tmp = tempfile::tempdir().expect("tmp");
        init_rust_repo(tmp.path(), "src/lib.rs", "fn x(){}\n");
        let flags = IndexFlags {
            path: tmp.path().to_path_buf(),
            r#ref: Some("HEAD".into()),
            force: false,
            cache: CacheArgs {
                cache_dir: None,
                no_cache: true,
            },
        };
        assert_eq!(run_index(&flags), ExitCode::Success);
    }

    // --- run_diff ----------------------------------------------------------

    #[test]
    fn diff_range_without_double_dot_is_usage_error() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = DiffFlags {
            range: "abc".into(),
            path: tmp.path().to_path_buf(),
            format: DiffFormat::Mermaid,
            cache: CacheArgs::default(),
        };
        assert_eq!(run_diff(&flags), ExitCode::UsageError);
    }

    #[test]
    fn diff_range_with_empty_side_is_usage_error() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = DiffFlags {
            range: "..main".into(),
            path: tmp.path().to_path_buf(),
            format: DiffFormat::Mermaid,
            cache: CacheArgs::default(),
        };
        assert_eq!(run_diff(&flags), ExitCode::UsageError);
    }

    #[test]
    fn diff_with_unknown_first_ref_fails() {
        let tmp = tempfile::tempdir().expect("tmp");
        init_rust_repo(tmp.path(), "src/lib.rs", "fn x(){}\n");
        let flags = DiffFlags {
            range: "nope..HEAD".into(),
            path: tmp.path().to_path_buf(),
            format: DiffFormat::Mermaid,
            cache: CacheArgs {
                cache_dir: Some(tmp.path().join("cache")),
                no_cache: false,
            },
        };
        assert_eq!(run_diff(&flags), ExitCode::Failure);
    }

    #[test]
    fn diff_two_commits_succeeds_in_both_formats() {
        let tmp = tempfile::tempdir().expect("tmp");
        init_rust_repo(tmp.path(), "src/lib.rs", "fn a(){}\n");
        // Second commit modifies the file → non-trivial diff.
        std::fs::write(tmp.path().join("src/lib.rs"), "fn a(){}\nfn b(){}\n").unwrap();
        git(tmp.path(), &["add", "src/lib.rs"]);
        git(tmp.path(), &["commit", "-q", "-m", "add b"]);

        let cache_dir = tmp.path().join("cache");
        let mermaid = DiffFlags {
            range: "HEAD~1..HEAD".into(),
            path: tmp.path().to_path_buf(),
            format: DiffFormat::Mermaid,
            cache: CacheArgs {
                cache_dir: Some(cache_dir.clone()),
                no_cache: false,
            },
        };
        assert_eq!(run_diff(&mermaid), ExitCode::Success);

        // Re-run with JSON to exercise the second arm (and the
        // ensure_indexed cache-hit short-circuit).
        let json = DiffFlags {
            format: DiffFormat::Json,
            ..mermaid.clone()
        };
        assert_eq!(run_diff(&json), ExitCode::Success);
    }

    // --- run_gc ------------------------------------------------------------

    #[test]
    fn gc_succeeds_on_empty_cache() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = GcFlags {
            path: tmp.path().to_path_buf(),
            max_size: "1G".into(),
            older_than: None,
            dry_run: false,
            cache: CacheArgs {
                cache_dir: Some(tmp.path().join("cache")),
                no_cache: false,
            },
        };
        assert_eq!(run_gc(&flags), ExitCode::Success);
    }

    #[test]
    fn gc_dry_run_with_age_filter_succeeds() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = GcFlags {
            path: tmp.path().to_path_buf(),
            max_size: "10M".into(),
            older_than: Some("30d".into()),
            dry_run: true,
            cache: CacheArgs {
                cache_dir: Some(tmp.path().join("cache")),
                no_cache: false,
            },
        };
        assert_eq!(run_gc(&flags), ExitCode::Success);
    }

    #[test]
    fn gc_bad_max_size_is_usage_error() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = GcFlags {
            path: tmp.path().to_path_buf(),
            max_size: "huge".into(),
            older_than: None,
            dry_run: false,
            cache: CacheArgs::default(),
        };
        assert_eq!(run_gc(&flags), ExitCode::UsageError);
    }

    #[test]
    fn gc_bad_older_than_is_usage_error() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = GcFlags {
            path: tmp.path().to_path_buf(),
            max_size: "1G".into(),
            older_than: Some("forever".into()),
            dry_run: false,
            cache: CacheArgs::default(),
        };
        assert_eq!(run_gc(&flags), ExitCode::UsageError);
    }

    // --- run_bundle / run_analyze with --ref ------------------------------

    #[test]
    fn bundle_from_git_ref_succeeds() {
        let tmp = tempfile::tempdir().expect("tmp");
        init_rust_repo(tmp.path(), "src/lib.rs", "fn a(){}\n");
        let out = tmp.path().join("bundle-out");
        let flags = BundleFlags {
            path: tmp.path().to_path_buf(),
            out: out.clone(),
            exclude: String::new(),
            r#ref: Some("HEAD".into()),
        };
        assert_eq!(run_bundle(&flags), ExitCode::Success);
        assert!(out.join("index.json").exists());
    }

    #[test]
    fn analyze_from_git_ref_succeeds() {
        let tmp = tempfile::tempdir().expect("tmp");
        init_rust_repo(tmp.path(), "src/lib.rs", "fn a(){}\n");
        let flags = AnalyzeFlags {
            path: tmp.path().to_path_buf(),
            target: None,
            exclude: String::new(),
            out: None,
            r#ref: Some("HEAD".into()),
            format: AnalyzeFormat::default(),
        };
        assert_eq!(run_analyze(Level::Project, &flags), ExitCode::Success);
    }

    #[test]
    fn analyze_dot_format_to_stdout_succeeds() {
        // Distinct from the existing dot-to-file test — exercises the
        // stdout branch of run_analyze under --format=dot.
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = AnalyzeFlags {
            path: tmp.path().to_path_buf(),
            target: None,
            exclude: String::new(),
            out: None,
            r#ref: None,
            format: AnalyzeFormat::Dot,
        };
        assert_eq!(run_analyze(Level::Project, &flags), ExitCode::Success);
    }

    #[test]
    fn analyze_write_to_unwritable_path_returns_failure() {
        let tmp = tempfile::tempdir().expect("tmp");
        // Writing to a directory-as-file fails the std::fs::write call.
        let bogus_out = tmp.path().to_path_buf();
        let flags = AnalyzeFlags {
            path: tmp.path().to_path_buf(),
            target: None,
            exclude: String::new(),
            out: Some(bogus_out),
            r#ref: None,
            format: AnalyzeFormat::default(),
        };
        assert_eq!(run_analyze(Level::Project, &flags), ExitCode::Failure);
    }

    // --- run_sequence ------------------------------------------------------

    fn write_rust(dir: &Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(&path, contents).unwrap();
    }

    #[test]
    fn sequence_empty_target_is_usage_error() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = SequenceFlags {
            path: tmp.path().to_path_buf(),
            target: "   ".into(),
            exclude: String::new(),
            out: None,
            r#ref: None,
        };
        assert_eq!(run_sequence(&flags), ExitCode::UsageError);
    }

    #[test]
    fn sequence_no_rust_files_returns_failure() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = SequenceFlags {
            path: tmp.path().to_path_buf(),
            target: "anything".into(),
            exclude: String::new(),
            out: None,
            r#ref: None,
        };
        assert_eq!(run_sequence(&flags), ExitCode::Failure);
    }

    #[test]
    fn sequence_unknown_function_returns_failure() {
        let tmp = tempfile::tempdir().expect("tmp");
        write_rust(tmp.path(), "lib.rs", "fn other(){}\n");
        let flags = SequenceFlags {
            path: tmp.path().to_path_buf(),
            target: "missing".into(),
            exclude: String::new(),
            out: None,
            r#ref: None,
        };
        assert_eq!(run_sequence(&flags), ExitCode::Failure);
    }

    #[test]
    fn sequence_writes_mermaid_to_file() {
        let tmp = tempfile::tempdir().expect("tmp");
        write_rust(
            tmp.path(),
            "lib.rs",
            "fn run(cache: &Cache) { cache.open(); foo(); }\n",
        );
        let out = tmp.path().join("seq.mmd");
        let flags = SequenceFlags {
            path: tmp.path().to_path_buf(),
            target: "run".into(),
            exclude: String::new(),
            out: Some(out.clone()),
            r#ref: None,
        };
        assert_eq!(run_sequence(&flags), ExitCode::Success);
        let body = std::fs::read_to_string(&out).expect("read");
        assert!(body.starts_with("sequenceDiagram"), "got:\n{body}");
        assert!(body.contains("self->>cache: open"), "got:\n{body}");
        assert!(body.contains("self->>self: foo"), "got:\n{body}");
    }

    #[test]
    fn sequence_from_git_ref_succeeds() {
        let tmp = tempfile::tempdir().expect("tmp");
        init_rust_repo(tmp.path(), "lib.rs", "fn run() { foo(); }\n");
        let flags = SequenceFlags {
            path: tmp.path().to_path_buf(),
            target: "run".into(),
            exclude: String::new(),
            out: None,
            r#ref: Some("HEAD".into()),
        };
        assert_eq!(run_sequence(&flags), ExitCode::Success);
    }
}
