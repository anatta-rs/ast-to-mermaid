//! `a2m-project` — render the project-level Mermaid view.
//!
//! ```text
//! a2m-project ./src
//! a2m-project . --exclude target,vendor --out arch.mmd
//! ```

#![warn(missing_docs)]
#![deny(unsafe_code)]

use ast_to_mermaid::cli_support::{AnalyzeFlags, ExitCode, run_analyze};
use ast_to_mermaid::render::Level;
use clap::Parser;

#[derive(Parser)]
#[command(name = "a2m-project", about = "Render the project-level Mermaid view")]
struct Cli {
    #[command(flatten)]
    flags: AnalyzeFlags,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let code: ExitCode = run_analyze(Level::Project, cli.flags).await;
    code.into()
}
