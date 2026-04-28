//! `a2m-function` — render a function's call graph (callers + callees).
//!
//! ```text
//! a2m-function ./src --target render::project::render_project
//! a2m-function . -t parse_module
//! ```

#![warn(missing_docs)]
#![deny(unsafe_code)]

use ast_to_mermaid_cli::{AnalyzeFlags, run_analyze};
use ast_to_mermaid_core::cli::ExitCode;
use ast_to_mermaid_core::render::Level;
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "a2m-function",
    about = "Render a function's call graph (--target required)"
)]
struct Cli {
    #[command(flatten)]
    flags: AnalyzeFlags,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let code: ExitCode = run_analyze(Level::Function, cli.flags).await;
    code.into()
}
