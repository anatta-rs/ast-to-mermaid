//! `a2m-module` — render a single module's items + intra-module calls.
//!
//! ```text
//! a2m-module ./src --target src/parser/code.rs
//! a2m-module . -t parser::code
//! ```

#![warn(missing_docs)]
#![deny(unsafe_code)]

use ast_to_mermaid_cli::{AnalyzeFlags, run_analyze};
use ast_to_mermaid_core::cli::ExitCode;
use ast_to_mermaid_core::render::Level;
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "a2m-module",
    about = "Render a single module's Mermaid view (--target required)"
)]
struct Cli {
    #[command(flatten)]
    flags: AnalyzeFlags,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let code: ExitCode = run_analyze(Level::Module, cli.flags).await;
    code.into()
}
