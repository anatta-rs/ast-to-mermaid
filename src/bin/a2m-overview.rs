//! `a2m-overview` — render the overview-level Mermaid view (modules + cross-module calls).
//!
//! ```text
//! a2m-overview ./src
//! ```

#![warn(missing_docs)]
#![deny(unsafe_code)]

use ast_to_mermaid::cli_support::{AnalyzeFlags, ExitCode, run_analyze};
use ast_to_mermaid::render::Level;
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "a2m-overview",
    about = "Render the overview-level Mermaid view"
)]
struct Cli {
    #[command(flatten)]
    flags: AnalyzeFlags,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let code: ExitCode = run_analyze(Level::Overview, &cli.flags);
    code.into()
}
