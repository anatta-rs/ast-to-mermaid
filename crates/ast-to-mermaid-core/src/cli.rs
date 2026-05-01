//! CLI command parsing and dispatch.
//!
//! The `ast-to-mermaid-cli` binary is a 3-line wrapper that calls [`run`].
//! All testable logic lives here so coverage stays > 95% on the core lib.

use crate::artifacts::write_artifacts;
use crate::pipeline::{AnalyzeOptions, analyze, bundle};
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
        Some("bundle") => run_bundle(&args[1..]),
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
        target: parsed.target.clone(),
        exclude: parsed.exclude.clone(),
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
    target: Option<String>,
    exclude: Vec<String>,
    out: Option<PathBuf>,
}

impl AnalyzeArgs {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut path: Option<PathBuf> = None;
        let mut level = Level::Project;
        let mut target: Option<String> = None;
        let mut exclude: Vec<String> = Vec::new();
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
                "--target" | "-t" => {
                    let val = args
                        .get(i + 1)
                        .ok_or_else(|| format!("{arg} requires a value"))?;
                    target = Some(val.clone());
                    i += 2;
                }
                "--exclude" | "-x" => {
                    let val = args
                        .get(i + 1)
                        .ok_or_else(|| format!("{arg} requires a value"))?;
                    exclude.extend(
                        val.split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned),
                    );
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
        if level.requires_target() && target.is_none() {
            return Err(format!("level={level} requires --target"));
        }
        Ok(Self {
            path,
            level,
            target,
            exclude,
            out,
        })
    }
}

fn print_usage() {
    println!("usage: ast-to-mermaid <command>");
    println!();
    println!("commands:");
    println!("  version    Print version and exit");
    println!("  analyze    Parse a code path → single Mermaid file");
    println!("  bundle     Parse a code path → 4-layer artifact bundle");
    println!("             (consumed by `mermaid-graph project --artifacts`)");
    println!();
    println!("for the MCP server, run the `ast-to-mermaid-mcp` binary.");
}

fn print_bundle_usage() {
    println!("usage: ast-to-mermaid bundle <path> --out <dir> [--exclude DIRS]");
    println!();
    println!("flags:");
    println!("  -o, --out      output directory for the bundle (REQUIRED)");
    println!("  -x, --exclude  comma-separated dir basenames to skip on top of");
    println!("                 the built-in skip set (target, node_modules, .git,");
    println!("                 dotfile dirs).");
    println!();
    println!("layout written to <dir>/ :");
    println!("  overview.mmd       module-level call graph");
    println!("  project.mmd        crate-level summary");
    println!("  index.json         registry of every indexed entity");
    println!("  entities/<id>.mmd          per-entity Mermaid");
    println!("  entities/<id>.meta.json    per-entity metadata");
}

fn run_bundle(args: &[String]) -> ExitCode {
    let parsed = match BundleArgs::parse(args) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("bundle: {msg}");
            print_bundle_usage();
            return ExitCode::UsageError;
        }
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("bundle: failed to start tokio runtime: {e}");
            return ExitCode::Failure;
        }
    };

    let opts = AnalyzeOptions {
        level: Level::Project,
        target: None,
        exclude: parsed.exclude.clone(),
        ..AnalyzeOptions::default()
    };

    let (artifacts, report) = match runtime.block_on(bundle(&parsed.path, &opts)) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("bundle: {e}");
            return ExitCode::Failure;
        }
    };

    if let Err(e) = write_artifacts(&artifacts, &parsed.out) {
        eprintln!("bundle: write {}: {e}", parsed.out.display());
        return ExitCode::Failure;
    }

    eprintln!(
        "bundled {} files, {} atoms, {} edges, {} entities → {}",
        report.files_parsed,
        report.atoms_indexed,
        report.edges_resolved,
        artifacts.entities.len(),
        parsed.out.display(),
    );

    ExitCode::Success
}

#[derive(Debug, PartialEq, Eq)]
struct BundleArgs {
    path: PathBuf,
    out: PathBuf,
    exclude: Vec<String>,
}

impl BundleArgs {
    fn parse(args: &[String]) -> std::result::Result<Self, String> {
        let mut path: Option<PathBuf> = None;
        let mut out: Option<PathBuf> = None;
        let mut exclude: Vec<String> = Vec::new();

        let mut i = 0;
        while i < args.len() {
            let arg = args[i].as_str();
            match arg {
                "--out" | "-o" => {
                    let val = args
                        .get(i + 1)
                        .ok_or_else(|| format!("{arg} requires a value"))?;
                    out = Some(PathBuf::from(val));
                    i += 2;
                }
                "--exclude" | "-x" => {
                    let val = args
                        .get(i + 1)
                        .ok_or_else(|| format!("{arg} requires a value"))?;
                    exclude.extend(
                        val.split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned),
                    );
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
        let out = out.ok_or_else(|| "missing --out <dir>".to_owned())?;
        Ok(Self { path, out, exclude })
    }
}

fn print_analyze_usage() {
    println!(
        "usage: ast-to-mermaid analyze <path> [--level LVL] [--target NAME] [--exclude DIRS] [--out FILE]"
    );
    println!();
    println!("flags:");
    println!("  -l, --level    project | overview | module | function | impact");
    println!("                 (default: project)");
    println!("  -t, --target   required for module/function/impact:");
    println!("                 module path/name or function name/id");
    println!("  -x, --exclude  comma-separated dir basenames to skip on top of");
    println!("                 the built-in skip set (target, node_modules, .git,");
    println!("                 dotfile dirs). E.g. --exclude workspaces,vendor");
    println!("  -o, --out      write to FILE instead of stdout");
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

    // ── bundle subcommand ────────────────────────────────────────────────────

    #[test]
    fn bundle_without_path_returns_usage_error() {
        assert_eq!(run(&args(&["bundle"])), ExitCode::UsageError);
    }

    #[test]
    fn bundle_without_out_returns_usage_error() {
        let tmp = tempdir().expect("tmp");
        let p = tmp.path().to_string_lossy().to_string();
        assert_eq!(run(&args(&["bundle", &p])), ExitCode::UsageError);
    }

    #[test]
    fn bundle_with_unknown_flag_returns_usage_error() {
        assert_eq!(
            run(&args(&["bundle", "--bogus", "x", "/tmp"])),
            ExitCode::UsageError
        );
    }

    #[test]
    fn bundle_with_two_positional_args_returns_usage_error() {
        assert_eq!(
            run(&args(&["bundle", "/a", "/b", "--out", "/c"])),
            ExitCode::UsageError
        );
    }

    #[test]
    fn bundle_with_missing_flag_value_returns_usage_error() {
        assert_eq!(run(&args(&["bundle", "/x", "--out"])), ExitCode::UsageError);
    }

    #[test]
    fn bundle_on_missing_path_returns_failure() {
        let tmp = tempdir().expect("tmp");
        let out = tmp.path().join("bundle-out");
        let exit = run(&args(&[
            "bundle",
            "/no/such/path/12345",
            "--out",
            out.to_string_lossy().as_ref(),
        ]));
        assert_eq!(exit, ExitCode::Failure);
    }

    #[test]
    fn bundle_writes_full_layout_to_out_dir() {
        let tmp = tempdir().expect("tmp");
        fs::create_dir_all(tmp.path().join("src")).expect("mkdir");
        fs::write(tmp.path().join("src/lib.rs"), "pub fn foo() {}\n").expect("write");

        let out = tmp.path().join("bundle-out");
        let path_str = tmp.path().to_string_lossy().to_string();
        let out_str = out.to_string_lossy().to_string();

        let exit = run(&args(&["bundle", &path_str, "--out", &out_str]));
        assert_eq!(exit, ExitCode::Success);

        // Spot-check the canonical layout.
        assert!(out.join("overview.mmd").is_file());
        assert!(out.join("project.mmd").is_file());
        assert!(out.join("index.json").is_file());
        assert!(out.join("entities").is_dir());

        // index.json has the schema fields we promise.
        let index: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(out.join("index.json")).expect("read"))
                .expect("parse");
        assert_eq!(index["schema_version"], serde_json::json!(1));
        assert!(index["entities"].as_array().is_some_and(|a| !a.is_empty()));
    }

    #[test]
    fn bundle_short_flags_alias_long_flags() {
        let tmp = tempdir().expect("tmp");
        fs::write(tmp.path().join("a.rs"), "pub fn x() {}\n").expect("write");

        let out = tmp.path().join("bundle-out");
        let p = tmp.path().to_string_lossy().to_string();
        let o = out.to_string_lossy().to_string();
        let exit = run(&args(&["bundle", &p, "-o", &o, "-x", "vendor"]));
        assert_eq!(exit, ExitCode::Success);
        assert!(out.join("overview.mmd").is_file());
    }

    #[test]
    fn bundle_args_parse_picks_up_all_flags() {
        let parsed = BundleArgs::parse(&args(&[
            "/some/path",
            "--out",
            "/tmp/bundle",
            "--exclude",
            "vendor,target",
        ]))
        .expect("ok");
        assert_eq!(parsed.path, PathBuf::from("/some/path"));
        assert_eq!(parsed.out, PathBuf::from("/tmp/bundle"));
        assert_eq!(parsed.exclude, vec!["vendor", "target"]);
    }
}
