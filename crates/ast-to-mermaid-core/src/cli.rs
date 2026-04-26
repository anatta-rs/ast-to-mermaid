//! CLI command parsing and dispatch.
//!
//! The `ast-to-mermaid-cli` binary is a 3-line wrapper that calls [`run`].
//! All testable logic lives here so coverage stays > 95% on the core lib.

use std::process;

/// Exit code returned by [`run`], convertible into [`process::ExitCode`].
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

/// Dispatch a CLI invocation. `args` is the slice of arguments (without `argv[0]`).
///
/// `v0.1.0` exposes only `version`; `analyze`, `mcp`, and the rest will land in
/// subsequent PRs.
#[must_use]
pub fn run(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("version" | "--version" | "-V") => {
            println!("ast-to-mermaid {}", env!("CARGO_PKG_VERSION"));
            ExitCode::Success
        }
        Some("analyze") => {
            eprintln!("analyze: not implemented in v0.1.0");
            ExitCode::Failure
        }
        Some("mcp") => {
            eprintln!("use the `ast-to-mermaid-mcp` binary for the MCP server");
            ExitCode::UsageError
        }
        Some(other) => {
            eprintln!("unknown command: {other}");
            print_usage();
            ExitCode::UsageError
        }
        None => {
            print_usage();
            ExitCode::UsageError
        }
    }
}

fn print_usage() {
    println!("usage: ast-to-mermaid <command>");
    println!();
    println!("commands:");
    println!("  version    Print version and exit");
    println!("  analyze    Parse a code path (not yet implemented)");
    println!();
    println!("for the MCP server, run the `ast-to-mermaid-mcp` binary.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn version_returns_success() {
        assert_eq!(run(&args(&["version"])), ExitCode::Success);
    }

    #[test]
    fn version_long_flag_returns_success() {
        assert_eq!(run(&args(&["--version"])), ExitCode::Success);
    }

    #[test]
    fn version_short_flag_returns_success() {
        assert_eq!(run(&args(&["-V"])), ExitCode::Success);
    }

    #[test]
    fn analyze_returns_failure_for_now() {
        assert_eq!(run(&args(&["analyze"])), ExitCode::Failure);
    }

    #[test]
    fn mcp_redirects_with_usage_error() {
        assert_eq!(run(&args(&["mcp"])), ExitCode::UsageError);
    }

    #[test]
    fn unknown_command_returns_usage_error() {
        assert_eq!(run(&args(&["frobnicate"])), ExitCode::UsageError);
    }

    #[test]
    fn no_args_prints_usage_and_returns_usage_error() {
        assert_eq!(run(&[]), ExitCode::UsageError);
    }

    #[test]
    fn exit_code_converts_to_process_exit_code() {
        let _: process::ExitCode = ExitCode::Success.into();
        let _: process::ExitCode = ExitCode::Failure.into();
        let _: process::ExitCode = ExitCode::UsageError.into();
    }

    #[test]
    fn exit_code_clones_and_compares() {
        let a = ExitCode::Success;
        let b = a;
        assert_eq!(a, b);
        assert_ne!(a, ExitCode::Failure);
        assert_ne!(ExitCode::Failure, ExitCode::UsageError);
        assert!(format!("{a:?}").contains("Success"));
    }
}
