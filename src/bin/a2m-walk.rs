//! `a2m-walk` — list source files under a path, filterable, no parsing.
//!
//! ```text
//! a2m-walk ./src                          # tab-separated <lang>\t<path>
//! a2m-walk . --exclude target,vendor
//! a2m-walk . | awk '$1=="rust"' | wc -l   # count rust files
//! ```

#![warn(missing_docs)]
#![deny(unsafe_code)]

use ast_to_mermaid::pipeline::walk_for_languages_with_exclude;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "a2m-walk",
    about = "List source files under a path (no parsing)"
)]
struct Cli {
    /// Root path.
    path: PathBuf,
    /// Extra directory basenames to skip (comma-separated).
    #[arg(short = 'x', long, default_value = "")]
    exclude: String,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let exclude: Vec<String> = cli
        .exclude
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    match walk_for_languages_with_exclude(&cli.path, &exclude) {
        Ok(files) => {
            for (path, lang) in files {
                println!("{}\t{}", lang.name(), path.display());
            }
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("walk: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
