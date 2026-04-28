//! `a2m-impact` — render the impact graph: who depends on a target.
//!
//! ```text
//! a2m-impact ./src --target render::lookup::module_for_atom
//! ```

#![warn(missing_docs)]
#![deny(unsafe_code)]

use ast_to_mermaid::cli_support::{AnalyzeFlags, ExitCode, run_analyze};
use ast_to_mermaid::render::Level;
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "a2m-impact",
    about = "Render the impact graph for a target (--target required)"
)]
struct Cli {
    #[command(flatten)]
    flags: AnalyzeFlags,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let code: ExitCode = run_analyze(Level::Impact, cli.flags).await;
    code.into()
}
