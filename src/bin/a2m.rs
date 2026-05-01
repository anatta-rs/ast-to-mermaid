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
    AnalyzeFlags, BundleFlags, ExitCode, WalkFlags, run_analyze, run_bundle, run_walk,
};
use ast_to_mermaid::render::Level;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "a2m",
    version,
    about = "Tree-sitter-based code-graph builder that emits Mermaid diagrams at five zoom levels"
)]
struct Cli {
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
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let code: ExitCode = match cli.command {
        Command::Project(f) => run_analyze(Level::Project, &f),
        Command::Overview(f) => run_analyze(Level::Overview, &f),
        Command::Module(f) => run_analyze(Level::Module, &f),
        Command::Function(f) => run_analyze(Level::Function, &f),
        Command::Impact(f) => run_analyze(Level::Impact, &f),
        Command::Walk(f) => run_walk(&f),
        Command::Bundle(f) => run_bundle(&f),
    };
    code.into()
}
