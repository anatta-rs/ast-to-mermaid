//! `a2m` — turn a source tree into Mermaid diagrams.
//!
//! Seven subcommands, one binary:
//!
//! ```text
//! a2m project  ./src                              # crate-level overview
//! a2m overview ./src                              # module-level overview
//! a2m module   ./src --target render/mod.rs       # one module, fully linked
//! a2m function ./src --target analyze             # reverse call chain
//! a2m impact   ./src --target analyze             # blast radius (3 hops)
//! a2m walk     ./src                              # list source files
//! a2m bundle   ./src --out ./.artifacts           # full 4-layer bundle
//! ```

#![warn(missing_docs)]
#![deny(unsafe_code)]

use ast_to_mermaid::cli_support::{
    AnalyzeFlags, BundleFlags, DiffFlags, ExitCode, GcFlags, IndexFlags, WalkFlags, run_analyze,
    run_bundle, run_diff, run_gc, run_index, run_walk,
};
use ast_to_mermaid::render::Level;
use clap::{Parser, Subcommand};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

#[derive(Parser)]
#[command(
    name = "a2m",
    version,
    about = "Tree-sitter-based code-graph builder that emits Mermaid diagrams at five zoom levels"
)]
struct Cli {
    /// Trace verbosity. Honors `RUST_LOG` env var; this flag is a shortcut
    /// (e.g. `--trace=info` to see parse/resolve phase timings).
    #[arg(long, value_name = "LEVEL", global = true)]
    trace: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Render the project-level Mermaid view (one node per crate).
    Project(AnalyzeFlags),
    /// Render the overview-level Mermaid view (one node per module).
    Overview(AnalyzeFlags),
    /// Render a single module's items + intra/cross-module calls.
    Module(AnalyzeFlags),
    /// Render a function's reverse call chain (who calls it, N hops back).
    Function(AnalyzeFlags),
    /// Render the impact graph for a target (forward + backward, 3 hops).
    Impact(AnalyzeFlags),
    /// List source files under a path (no parsing).
    Walk(WalkFlags),
    /// Produce the 4-layer artifact bundle for a project.
    Bundle(BundleFlags),
    /// Materialize a bundle for a git ref (or the working tree) into the
    /// `.a2m/cache/refs/<sha>/` cache. Idempotent — cached re-runs are
    /// no-ops unless `--force` is set.
    Index(IndexFlags),
    /// Set-diff between two cached bundles (`<ref-a>..<ref-b>`).
    /// Auto-runs `index` for missing refs.
    Diff(DiffFlags),
    /// Garbage-collect the cache: evict old / oversized entries.
    Gc(GcFlags),
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.trace.as_deref());
    let code: ExitCode = match cli.command {
        Command::Project(f) => run_analyze(Level::Project, &f),
        Command::Overview(f) => run_analyze(Level::Overview, &f),
        Command::Module(f) => run_analyze(Level::Module, &f),
        Command::Function(f) => run_analyze(Level::Function, &f),
        Command::Impact(f) => run_analyze(Level::Impact, &f),
        Command::Walk(f) => run_walk(&f),
        Command::Bundle(f) => run_bundle(&f),
        Command::Index(f) => run_index(&f),
        Command::Diff(f) => run_diff(&f),
        Command::Gc(f) => run_gc(&f),
    };
    code.into()
}

/// Initialize the tracing subscriber. Resolves verbosity in this order:
/// explicit `--trace <level>`, then `RUST_LOG`, then default (warn).
/// Silently no-ops if a subscriber is already set (e.g. integration tests).
fn init_tracing(trace_flag: Option<&str>) {
    let filter = match trace_flag {
        Some(level) => EnvFilter::new(level),
        None => EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
    };
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false).with_writer(std::io::stderr))
        .try_init();
}
