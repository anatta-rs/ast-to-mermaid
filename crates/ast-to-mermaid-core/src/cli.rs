//! CLI command parsing and dispatch.
//!
//! The `ast-to-mermaid-cli` binary is a 3-line wrapper that calls [`run`].
//! All testable logic lives here so coverage stays > 95% on the core lib.

use crate::pipeline::{AnalyzeOptions, analyze};
use crate::render::Level;
use std::path::PathBuf;
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
/// v0.2 commands:
/// - `version` / `--version` / `-V` — print version
/// - `analyze <path> [--level project|overview] [--out <file>]` — parse,
///   resolve, render Mermaid
/// - `mcp` — redirects to the dedicated `ast-to-mermaid-mcp` binary
///
/// Output for `analyze` goes to stdout unless `--out FILE` is specified.
#[must_use]
pub fn run(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("version" | "--version" | "-V") => {
            println!("ast-to-mermaid {}", env!("CARGO_PKG_VERSION"));
            ExitCode::Success
        }
        Some("analyze") => run_analyze(&args[1..]),
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

fn run_analyze(args: &[String]) -> ExitCode {
    let parsed = match AnalyzeArgs::parse(args) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("analyze: {msg}");
            print_analyze_usage();
            return ExitCode::UsageError;
        }
    };

    // Block on the async pipeline using the current-thread runtime so the CLI
    // stays single-purpose and dependency-light.
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("analyze: failed to start tokio runtime: {e}");
            return ExitCode::Failure;
        }
    };

    let opts = AnalyzeOptions {
        level: parsed.level,
        ..AnalyzeOptions::default()
    };

    let report = match runtime.block_on(analyze(&parsed.path, &opts)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("analyze: {e}");
            return ExitCode::Failure;
        }
    };

    let written_to: Option<&std::path::Path> = if let Some(path) = parsed.out.as_deref() {
        if let Err(e) = std::fs::write(path, &report.mermaid) {
            eprintln!("analyze: write {}: {e}", path.display());
            return ExitCode::Failure;
        }
        Some(path)
    } else {
        print!("{}", report.mermaid);
        None
    };

    let suffix = written_to
        .map(|p| format!(" → {}", p.display()))
        .unwrap_or_default();
    eprintln!(
        "analyzed {} files, {} atoms, {} cross-module edges{}",
        report.files_parsed, report.atoms_indexed, report.edges_resolved, suffix
    );

    ExitCode::Success
}

#[derive(Debug, PartialEq, Eq)]
struct AnalyzeArgs {
    path: PathBuf,
    level: Level,
    out: Option<PathBuf>,
}

impl AnalyzeArgs {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut path: Option<PathBuf> = None;
        let mut level = Level::Project;
        let mut out: Option<PathBuf> = None;

        let mut i = 0;
        while i < args.len() {
            let arg = args[i].as_str();
            match arg {
                "--level" | "-l" => {
                    let val = args
                        .get(i + 1)
                        .ok_or_else(|| format!("{arg} requires a value"))?;
                    level = val
                        .parse::<Level>()
                        .map_err(|e| format!("invalid --level: {e}"))?;
                    i += 2;
                }
                "--out" | "-o" => {
                    let val = args
                        .get(i + 1)
                        .ok_or_else(|| format!("{arg} requires a value"))?;
                    out = Some(PathBuf::from(val));
                    i += 2;
                }
                _ if arg.starts_with('-') => {
                    return Err(format!("unknown flag: {arg}"));
                }
                _ => {
                    if path.is_some() {
                        return Err(format!("unexpected positional arg: {arg}"));
                    }
                    path = Some(PathBuf::from(arg));
                    i += 1;
                }
            }
        }

        let path = path.ok_or_else(|| "missing <path>".to_owned())?;
        Ok(Self { path, level, out })
    }
}

fn print_usage() {
    println!("usage: ast-to-mermaid <command>");
    println!();
    println!("commands:");
    println!("  version    Print version and exit");
    println!("  analyze    Parse a code path → Mermaid (see `analyze --help` style below)");
    println!();
    println!("for the MCP server, run the `ast-to-mermaid-mcp` binary.");
}

fn print_analyze_usage() {
    println!("usage: ast-to-mermaid analyze <path> [--level project|overview] [--out FILE]");
    println!();
    println!("flags:");
    println!("  -l, --level   Mermaid view to render (default: project)");
    println!("  -o, --out     Write to FILE instead of stdout");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

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
    fn analyze_without_path_returns_usage_error() {
        assert_eq!(run(&args(&["analyze"])), ExitCode::UsageError);
    }

    #[test]
    fn analyze_with_unknown_flag_returns_usage_error() {
        assert_eq!(
            run(&args(&["analyze", "--bogus", "x", "/tmp"])),
            ExitCode::UsageError
        );
    }

    #[test]
    fn analyze_with_missing_flag_value_returns_usage_error() {
        assert_eq!(run(&args(&["analyze", "--level"])), ExitCode::UsageError);
        assert_eq!(run(&args(&["analyze", "--out"])), ExitCode::UsageError);
    }

    #[test]
    fn analyze_with_invalid_level_returns_usage_error() {
        assert_eq!(
            run(&args(&["analyze", "--level", "bogus", "/tmp"])),
            ExitCode::UsageError
        );
    }

    #[test]
    fn analyze_with_two_positional_args_returns_usage_error() {
        assert_eq!(run(&args(&["analyze", "/a", "/b"])), ExitCode::UsageError);
    }

    #[test]
    fn analyze_on_missing_path_returns_failure() {
        assert_eq!(
            run(&args(&["analyze", "/no/such/path/12345"])),
            ExitCode::Failure
        );
    }

    #[test]
    fn analyze_real_path_to_stdout_returns_success() {
        let tmp = tempdir().expect("tmp");
        fs::create_dir_all(tmp.path().join("src")).expect("mkdir");
        fs::write(tmp.path().join("src/lib.rs"), "pub fn hello() {}\n").expect("write");

        let path_str = tmp.path().to_string_lossy().to_string();
        let exit = run(&args(&["analyze", &path_str]));
        assert_eq!(exit, ExitCode::Success);
    }

    #[test]
    fn analyze_real_path_with_out_writes_file() {
        let tmp = tempdir().expect("tmp");
        fs::create_dir_all(tmp.path().join("src")).expect("mkdir");
        fs::write(tmp.path().join("src/lib.rs"), "pub fn hello() {}\n").expect("write");
        let out = tmp.path().join("graph.mmd");
        let path_str = tmp.path().to_string_lossy().to_string();
        let out_str = out.to_string_lossy().to_string();

        let exit = run(&args(&[
            "analyze", &path_str, "--level", "project", "--out", &out_str,
        ]));
        assert_eq!(exit, ExitCode::Success);
        let content = fs::read_to_string(&out).expect("written");
        assert!(content.starts_with("graph TD"));
    }

    #[test]
    fn analyze_short_flags_alias_long_flags() {
        let tmp = tempdir().expect("tmp");
        fs::write(tmp.path().join("a.rs"), "pub fn x() {}\n").expect("write");
        let out = tmp.path().join("g.mmd");
        let p = tmp.path().to_string_lossy().to_string();
        let o = out.to_string_lossy().to_string();
        let exit = run(&args(&["analyze", &p, "-l", "overview", "-o", &o]));
        assert_eq!(exit, ExitCode::Success);
        assert!(out.exists());
    }

    #[test]
    fn analyze_args_parse_picks_up_all_flags() {
        let parsed = AnalyzeArgs::parse(&args(&[
            "/some/path",
            "--level",
            "overview",
            "--out",
            "g.mmd",
        ]))
        .expect("ok");
        assert_eq!(parsed.path, PathBuf::from("/some/path"));
        assert_eq!(parsed.level, Level::Overview);
        assert_eq!(parsed.out, Some(PathBuf::from("g.mmd")));
    }

    #[test]
    fn analyze_args_parse_default_level_is_project() {
        let parsed = AnalyzeArgs::parse(&args(&["/some/path"])).expect("ok");
        assert_eq!(parsed.level, Level::Project);
        assert!(parsed.out.is_none());
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
