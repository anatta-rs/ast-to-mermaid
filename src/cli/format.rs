//! Small formatting / parsing helpers shared by the CLI subcommands:
//! CSV `--exclude` parsing, byte-size and duration parsing for the `gc`
//! flags, and the parser-ignored summary comment appended to stdout output
//! by `run_analyze`.

use crate::cli::flags::AnalyzeFormat;
use std::time::Duration;

/// Parse a CSV `--exclude` flag value into a list of directory basenames:
/// split on `,`, trim whitespace, drop empty entries. Shared across the
/// analyze / walk / bundle / sequence subcommands.
pub(crate) fn parse_csv_exclude(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect()
}

pub(crate) fn parse_size(s: &str) -> crate::error::Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        return Err(crate::error::AstToMermaidError::InvalidInput(
            "empty value".into(),
        ));
    }
    for (suffix, mul) in [
        ("K", 1_u64 << 10),
        ("k", 1_u64 << 10),
        ("M", 1_u64 << 20),
        ("m", 1_u64 << 20),
        ("G", 1_u64 << 30),
        ("g", 1_u64 << 30),
    ] {
        if let Some(head) = s.strip_suffix(suffix) {
            let n = head.parse::<u64>().map_err(|e| {
                crate::error::AstToMermaidError::InvalidInput(format!(
                    "expected `<num>[K|M|G]`, got `{s}`: {e}"
                ))
            })?;
            return n.checked_mul(mul).ok_or_else(|| {
                crate::error::AstToMermaidError::InvalidInput("size overflow".into())
            });
        }
    }
    s.parse::<u64>().map_err(|e| {
        crate::error::AstToMermaidError::InvalidInput(format!(
            "expected `<num>[K|M|G]`, got `{s}`: {e}"
        ))
    })
}

pub(crate) fn parse_duration(s: &str) -> crate::error::Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(crate::error::AstToMermaidError::InvalidInput(
            "empty value".into(),
        ));
    }
    for (suffix, secs) in [
        ("s", 1_u64),
        ("m", 60),
        ("h", 60 * 60),
        ("d", 24 * 60 * 60),
        ("w", 7 * 24 * 60 * 60),
    ] {
        if let Some(head) = s.strip_suffix(suffix) {
            let n = head.parse::<u64>().map_err(|e| {
                crate::error::AstToMermaidError::InvalidInput(format!(
                    "expected `<num>[s|m|h|d|w]`, got `{s}`: {e}"
                ))
            })?;
            return n
                .checked_mul(secs)
                .ok_or_else(|| {
                    crate::error::AstToMermaidError::InvalidInput("duration overflow".into())
                })
                .map(Duration::from_secs);
        }
    }
    s.parse::<u64>().map(Duration::from_secs).map_err(|e| {
        crate::error::AstToMermaidError::InvalidInput(format!(
            "expected `<num>[s|m|h|d|w]`, got `{s}`: {e}"
        ))
    })
}

/// Build the parser-ignored summary line appended to stdout when
/// `run_analyze` writes there directly. Mermaid uses `%%`, DOT uses `//`
/// — both are line comments in their respective grammars, so renderers
/// (mermaid.live, GitHub Markdown, `dot -Tsvg`) silently strip them.
///
/// Returns the line *without* a trailing newline; the caller decides via
/// `println!` vs `print!` whether to add one.
pub(crate) fn format_summary_comment(
    format: AnalyzeFormat,
    files_parsed: usize,
    atoms_indexed: usize,
    edges_resolved: usize,
) -> String {
    let prefix = match format {
        AnalyzeFormat::Mermaid => "%%",
        AnalyzeFormat::Dot => "//",
    };
    format!(
        "{prefix} analyzed {files_parsed} files, {atoms_indexed} atoms, {edges_resolved} cross-module edges"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_csv_exclude_trims_and_drops_empties() {
        assert_eq!(parse_csv_exclude("a,, b ,c"), vec!["a", "b", "c"]);
        assert!(parse_csv_exclude("").is_empty());
        assert!(parse_csv_exclude(" , , ").is_empty());
        assert_eq!(parse_csv_exclude("target"), vec!["target"]);
    }

    #[test]
    fn parse_size_plain_bytes() {
        assert_eq!(parse_size("42").unwrap(), 42);
    }

    #[test]
    fn parse_size_suffixes_kilo_mega_giga() {
        assert_eq!(parse_size("2K").unwrap(), 2 * 1024);
        assert_eq!(parse_size("3m").unwrap(), 3 * 1024 * 1024);
        assert_eq!(parse_size("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("4k").unwrap(), 4 * 1024);
    }

    #[test]
    fn parse_size_empty_errors() {
        let err = parse_size("   ").unwrap_err().to_string();
        assert!(err.contains("empty"), "got: {err}");
    }

    #[test]
    fn parse_size_non_numeric_errors() {
        let err = parse_size("abc").unwrap_err().to_string();
        assert!(err.contains("expected"), "got: {err}");
    }

    #[test]
    fn parse_size_non_ascii_suffix_errors_without_panic() {
        let err = parse_size("5€").unwrap_err().to_string();
        assert!(err.contains("expected"), "got: {err}");
    }

    #[test]
    fn parse_size_overflow_errors() {
        // u64::MAX / (1<<30) ≈ 1.7e10, so 1e11 G overflows.
        let err = parse_size("99999999999G").unwrap_err().to_string();
        assert!(err.contains("size overflow"), "got: {err}");
    }

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86400));
        assert_eq!(parse_duration("1w").unwrap(), Duration::from_secs(604_800));
        assert_eq!(parse_duration("7").unwrap(), Duration::from_secs(7));
    }

    #[test]
    fn parse_duration_empty_errors() {
        assert!(
            parse_duration("")
                .unwrap_err()
                .to_string()
                .contains("empty")
        );
    }

    #[test]
    fn parse_duration_non_numeric_errors() {
        let err = parse_duration("xs").unwrap_err().to_string();
        assert!(err.contains("expected"), "got: {err}");
    }

    #[test]
    fn parse_duration_non_ascii_suffix_errors_without_panic() {
        let err = parse_duration("5€").unwrap_err().to_string();
        assert!(err.contains("expected"), "got: {err}");
    }

    #[test]
    fn parse_duration_overflow_errors() {
        let err = parse_duration("999999999999999999w")
            .unwrap_err()
            .to_string();
        assert!(err.contains("duration overflow"), "got: {err}");
    }

    #[test]
    fn format_summary_comment_uses_mermaid_prefix() {
        let line = format_summary_comment(AnalyzeFormat::Mermaid, 10, 20, 30);
        assert!(line.starts_with("%% analyzed 10 files"), "got: {line}");
        assert!(line.contains("20 atoms"));
        assert!(line.contains("30 cross-module edges"));
    }

    #[test]
    fn format_summary_comment_uses_dot_prefix() {
        let line = format_summary_comment(AnalyzeFormat::Dot, 1, 2, 3);
        assert!(line.starts_with("// analyzed 1 files"), "got: {line}");
    }
}
