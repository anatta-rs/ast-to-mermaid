//! Shared CLI infrastructure for the `a2m-*` binary family.
//!
//! Each Mermaid level (project / overview / module / function / impact)
//! ships as its own binary so users can tab-complete a verb that maps
//! directly to a level. This module carries the bits they all share:
//! exit codes, arg shape, and the analyze-and-write helper.

use crate::pipeline::{AnalyzeOptions, analyze};
use crate::render::Level;
use std::path::PathBuf;
use std::process;

/// Exit code returned by CLI functions, convertible into [`process::ExitCode`].
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

/// Shared CLI args for every `a2m-*` analyze binary.
#[derive(Debug, Clone, clap::Args)]
pub struct AnalyzeFlags {
    /// Path to a source root (file or directory).
    pub path: PathBuf,

    /// Required for `module` / `function` / `impact` levels: the target
    /// module path or symbol name. Ignored by `project` / `overview`.
    #[arg(short, long)]
    pub target: Option<String>,

    /// Extra directory basenames to skip during walk (comma-separated).
    /// Always combined with the built-in skip set (`target`,
    /// `node_modules`, `.git`, hidden dirs).
    #[arg(short = 'x', long, default_value = "")]
    pub exclude: String,

    /// Write Mermaid output to this file instead of stdout.
    #[arg(short, long)]
    pub out: Option<PathBuf>,
}

/// Run the analyze pipeline for `level`, writing the resulting Mermaid to
/// `flags.out` or stdout. Returns the program's exit code.
///
/// # Errors
///
/// All failures are reported via `eprintln!` and surfaced as
/// `ExitCode::Failure`. Bad CLI input (missing target for a level that
/// requires one) yields `ExitCode::UsageError`.
pub async fn run_analyze(level: Level, flags: AnalyzeFlags) -> ExitCode {
    if level.requires_target() && flags.target.is_none() {
        eprintln!("level={level} requires --target");
        return ExitCode::UsageError;
    }

    let exclude: Vec<String> = flags
        .exclude
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();

    let opts = AnalyzeOptions {
        level,
        target: flags.target.clone(),
        exclude,
        ..AnalyzeOptions::default()
    };

    let report = match analyze(&flags.path, &opts).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("analyze: {e}");
            return ExitCode::Failure;
        }
    };

    let suffix = if let Some(path) = flags.out.as_deref() {
        if let Err(e) = std::fs::write(path, &report.mermaid) {
            eprintln!("write {}: {e}", path.display());
            return ExitCode::Failure;
        }
        format!(" → {}", path.display())
    } else {
        print!("{}", report.mermaid);
        String::new()
    };

    eprintln!(
        "analyzed {} files, {} atoms, {} cross-module edges{}",
        report.files_parsed, report.atoms_indexed, report.edges_resolved, suffix,
    );
    ExitCode::Success
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn module_level_without_target_returns_usage_error() {
        let flags = AnalyzeFlags {
            path: PathBuf::from("/dev/null"),
            target: None,
            exclude: String::new(),
            out: None,
        };
        let code = run_analyze(Level::Module, flags).await;
        assert_eq!(code, ExitCode::UsageError);
    }

    #[tokio::test]
    async fn analyze_with_missing_path_returns_failure() {
        let flags = AnalyzeFlags {
            path: PathBuf::from("/no/such/path/here"),
            target: None,
            exclude: String::new(),
            out: None,
        };
        let code = run_analyze(Level::Project, flags).await;
        assert_eq!(code, ExitCode::Failure);
    }
}
