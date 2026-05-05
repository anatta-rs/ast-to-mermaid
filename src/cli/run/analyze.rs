use super::{check_ref_arg, open_default_cache};
use crate::cli::flags::{AnalyzeFlags, AnalyzeFormat, ExitCode};
use crate::cli::format::{format_summary_comment, parse_csv_exclude};
use crate::pipeline::{AnalyzeOptions, analyze};
use crate::render::{Level, mermaid_to_dot};

/// Run the analyze pipeline for `level`, writing the resulting Mermaid to
/// `flags.out` or stdout. Returns the program's exit code.
pub fn run_analyze(level: Level, flags: &AnalyzeFlags) -> ExitCode {
    if level.requires_target() && flags.target.is_none() {
        eprintln!("{}: --target: required for this level", level.as_str());
        return ExitCode::UsageError;
    }
    if let Err(code) = check_ref_arg(level.as_str(), flags.r#ref.as_deref()) {
        return code;
    }

    let exclude = parse_csv_exclude(&flags.exclude);

    // Wire the cache transparently for analyze-flavoured subcommands when we
    // can find a git toplevel. Failures (not in a git repo, can't open cache)
    // are non-fatal — fall back to no caching.
    let cache = open_default_cache(&flags.path);

    let opts = AnalyzeOptions {
        level,
        target: flags.target.clone(),
        exclude,
        git_ref: flags.r#ref.clone(),
        cache,
        ..AnalyzeOptions::default()
    };

    let report = match analyze(&flags.path, &opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{}: analyze {}: {e}", level.as_str(), flags.path.display());
            return ExitCode::Failure;
        }
    };

    let rendered = match flags.format {
        AnalyzeFormat::Mermaid => report.mermaid.clone(),
        AnalyzeFormat::Dot => mermaid_to_dot(&report.mermaid),
    };

    if !report.failures.is_empty() {
        eprintln!(
            "skipped {} files (see --trace=warn for details)",
            report.failures.len(),
        );
    }

    if let Some(path) = flags.out.as_deref() {
        if let Err(e) = std::fs::write(path, &rendered) {
            eprintln!("{}: write {}: {e}", level.as_str(), path.display());
            return ExitCode::Failure;
        }
        eprintln!(
            "analyzed {} files, {} atoms, {} cross-module edges → {}",
            report.files_parsed,
            report.atoms_indexed,
            report.edges_resolved,
            path.display(),
        );
    } else {
        // Stdout path: terminal users copy-paste the rendered output
        // directly into mermaid.live or a fenced ```mermaid block. The
        // summary used to land on stderr, but stderr renders adjacently in
        // a TTY and gets swept into the copy-paste, breaking the parser.
        // Inline it as a parser-ignored comment instead.
        print!("{rendered}");
        println!(
            "{}",
            format_summary_comment(
                flags.format,
                report.files_parsed,
                report.atoms_indexed,
                report.edges_resolved,
            )
        );
    }
    ExitCode::Success
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::run::test_helpers::init_rust_repo;
    use std::path::PathBuf;

    #[test]
    fn module_level_without_target_returns_usage_error() {
        let flags = AnalyzeFlags {
            path: PathBuf::from("/dev/null"),
            target: None,
            exclude: String::new(),
            out: None,
            r#ref: None,
            format: AnalyzeFormat::default(),
        };
        let code = run_analyze(Level::Module, &flags);
        assert_eq!(code, ExitCode::UsageError);
    }

    #[test]
    fn analyze_with_missing_path_returns_failure() {
        let flags = AnalyzeFlags {
            path: PathBuf::from("/no/such/path/here"),
            target: None,
            exclude: String::new(),
            out: None,
            r#ref: None,
            format: AnalyzeFormat::default(),
        };
        let code = run_analyze(Level::Project, &flags);
        assert_eq!(code, ExitCode::Failure);
    }

    #[test]
    fn project_level_on_empty_dir_succeeds_and_writes_to_file() {
        let tmp = tempfile::tempdir().expect("tmp");
        let out_file = tmp.path().join("out.mmd");
        // Analyze the tempdir itself (no source files → empty diagram).
        let flags = AnalyzeFlags {
            path: tmp.path().to_path_buf(),
            target: None,
            exclude: String::new(),
            out: Some(out_file.clone()),
            r#ref: None,
            format: AnalyzeFormat::default(),
        };
        let code = run_analyze(Level::Project, &flags);
        assert_eq!(code, ExitCode::Success);
        assert!(out_file.exists(), "output file must be written");
        let body = std::fs::read_to_string(&out_file).expect("read");
        // The summary lives on stderr (with " → path" suffix) when --out is
        // set; the file itself stays clean for downstream renderers.
        assert!(
            !body.contains("%% analyzed"),
            "file output must not contain the stdout-only %% summary; got: {body}"
        );
    }

    #[test]
    fn project_level_on_empty_dir_prints_to_stdout() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = AnalyzeFlags {
            path: tmp.path().to_path_buf(),
            target: None,
            exclude: String::new(),
            out: None,
            r#ref: None,
            format: AnalyzeFormat::default(),
        };
        let code = run_analyze(Level::Project, &flags);
        assert_eq!(code, ExitCode::Success);
    }

    #[test]
    fn project_level_dot_format_emits_digraph() {
        let tmp = tempfile::tempdir().expect("tmp");
        let out_file = tmp.path().join("out.dot");
        let flags = AnalyzeFlags {
            path: tmp.path().to_path_buf(),
            target: None,
            exclude: String::new(),
            out: Some(out_file.clone()),
            r#ref: None,
            format: AnalyzeFormat::Dot,
        };
        assert_eq!(run_analyze(Level::Project, &flags), ExitCode::Success);
        let body = std::fs::read_to_string(&out_file).expect("read");
        assert!(body.starts_with("digraph G {"), "got: {body}");
        assert!(body.contains("rankdir=TB"), "got: {body}");
        // DOT file stays clean: the stdout-only `// analyzed …` summary
        // must not be appended when writing to a file.
        assert!(
            !body.contains("// analyzed"),
            "file output must not contain the stdout-only summary comment; got: {body}"
        );
    }

    #[test]
    fn analyze_from_git_ref_succeeds() {
        let tmp = tempfile::tempdir().expect("tmp");
        init_rust_repo(tmp.path(), "src/lib.rs", "fn a(){}\n");
        let flags = AnalyzeFlags {
            path: tmp.path().to_path_buf(),
            target: None,
            exclude: String::new(),
            out: None,
            r#ref: Some("HEAD".into()),
            format: AnalyzeFormat::default(),
        };
        assert_eq!(run_analyze(Level::Project, &flags), ExitCode::Success);
    }

    #[test]
    fn analyze_dot_format_to_stdout_succeeds() {
        // Distinct from the existing dot-to-file test — exercises the
        // stdout branch of run_analyze under --format=dot.
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = AnalyzeFlags {
            path: tmp.path().to_path_buf(),
            target: None,
            exclude: String::new(),
            out: None,
            r#ref: None,
            format: AnalyzeFormat::Dot,
        };
        assert_eq!(run_analyze(Level::Project, &flags), ExitCode::Success);
    }

    #[test]
    fn analyze_write_to_unwritable_path_returns_failure() {
        let tmp = tempfile::tempdir().expect("tmp");
        // Writing to a directory-as-file fails the std::fs::write call.
        let bogus_out = tmp.path().to_path_buf();
        let flags = AnalyzeFlags {
            path: tmp.path().to_path_buf(),
            target: None,
            exclude: String::new(),
            out: Some(bogus_out),
            r#ref: None,
            format: AnalyzeFormat::default(),
        };
        assert_eq!(run_analyze(Level::Project, &flags), ExitCode::Failure);
    }
}
