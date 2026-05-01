//! `a2m-bundle` — produce the 4-layer artifact bundle for a project.
//!
//! Walks the source tree, parses every supported file, builds the
//! in-memory graph, and writes `overview.mmd` + `index.json` +
//! per-entity `.mmd` + `.meta.json` to the output directory.
//!
//! The output is the canonical input for `mermaid-graph project
//! --artifacts <dir>` (which projects the bundle into a Neo4j-backed
//! Anatta scope).
//!
//! ```text
//! a2m-bundle ./src --out ./.artifacts
//! a2m-bundle . --out /tmp/bundle --exclude target,vendor
//! ```

#![warn(missing_docs)]
#![deny(unsafe_code)]

use ast_to_mermaid::artifacts::write_artifacts;
use ast_to_mermaid::pipeline::{AnalyzeOptions, bundle};
use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "a2m-bundle",
    about = "Produce the 4-layer artifact bundle for a project"
)]
struct Cli {
    /// Path to a source root (file or directory).
    path: PathBuf,

    /// Output directory for the bundle (overview.mmd, index.json,
    /// entities/<id>.mmd, entities/<id>.meta.json).
    #[arg(short, long)]
    out: PathBuf,

    /// Comma-separated dir basenames to skip on top of the built-in
    /// skip set (target, node_modules, .git, dotfile dirs).
    #[arg(short = 'x', long, value_delimiter = ',', default_value = "")]
    exclude: Vec<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let opts = AnalyzeOptions {
        // bundle() ignores level + target — pin to defaults for clarity.
        exclude: cli.exclude.into_iter().filter(|s| !s.is_empty()).collect(),
        ..AnalyzeOptions::default()
    };

    let (artifacts, report) = match bundle(&cli.path, &opts) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("a2m-bundle: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = write_artifacts(&artifacts, &cli.out) {
        eprintln!("a2m-bundle: write {}: {e}", cli.out.display());
        return ExitCode::FAILURE;
    }

    eprintln!(
        "bundled {} files, {} atoms, {} edges, {} entities → {}",
        report.files_parsed,
        report.atoms_indexed,
        report.edges_resolved,
        artifacts.entities.len(),
        cli.out.display(),
    );

    ExitCode::SUCCESS
}
