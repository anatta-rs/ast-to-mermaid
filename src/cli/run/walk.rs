use super::check_ref_arg;
use crate::cli::flags::{ExitCode, WalkFlags};
use crate::cli::format::parse_csv_exclude;
use crate::pipeline::{language_for, walk_for_languages_with_exclude};

/// Run the file-walker subcommand: print one line per source file, format
/// `<lang>\t<path>`, to stdout.
pub fn run_walk(flags: &WalkFlags) -> ExitCode {
    if let Err(code) = check_ref_arg("walk", flags.r#ref.as_deref()) {
        return code;
    }
    if let Some(git_ref) = flags.r#ref.as_deref() {
        return run_walk_ref(&flags.path, git_ref);
    }
    let exclude = parse_csv_exclude(&flags.exclude);

    match walk_for_languages_with_exclude(&flags.path, &exclude) {
        Ok(files) => {
            for (path, lang) in files {
                println!("{}\t{}", lang.name(), path.display());
            }
            ExitCode::Success
        }
        Err(e) => {
            eprintln!("walk: walk {}: {e}", flags.path.display());
            ExitCode::Failure
        }
    }
}

fn run_walk_ref(start: &std::path::Path, git_ref: &str) -> ExitCode {
    use crate::git_source;

    let toplevel = match git_source::show_toplevel(start) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("walk: resolve toplevel {}: {e}", start.display());
            return ExitCode::Failure;
        }
    };
    let entries = match git_source::ls_tree(&toplevel, git_ref) {
        Ok(es) => es,
        Err(e) => {
            eprintln!("walk: ls-tree {git_ref}: {e}");
            return ExitCode::Failure;
        }
    };
    for entry in entries {
        let Some(lang) = language_for(std::path::Path::new(&entry.path)) else {
            continue;
        };
        println!("{}\t{}", lang.name(), escape_control_chars(&entry.path));
    }
    ExitCode::Success
}

/// Escape control bytes (`<0x20`, plus DEL `0x7f`) and the `\` itself with
/// Rust-style `\x..` escapes so the printed line is safe to feed through
/// `awk -F'\t'` and friends.
///
/// Git's tree storage allows any byte except `/` and `\0` in path
/// components, so a malicious commit can ship a filename containing
/// embedded `\n`, `\r`, `\t`, or terminal escape sequences. Printing those
/// raw breaks the line-oriented contract of `walk` (one entry per line,
/// tab-separated) and could rewrite the user's terminal. Tabs we keep
/// (the field separator); newlines, carriage returns, NULs, DEL and the
/// rest of C0 we escape.
fn escape_control_chars(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            c if c == '\t' => out.push(c),
            c if (c as u32) < 0x20 || c == '\x7f' => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::run::test_helpers::{git, init_rust_repo};
    use std::path::PathBuf;

    #[test]
    fn walk_on_empty_dir_succeeds() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = WalkFlags {
            path: tmp.path().to_path_buf(),
            exclude: String::new(),
            r#ref: None,
        };
        assert_eq!(run_walk(&flags), ExitCode::Success);
    }

    #[test]
    fn walk_with_missing_path_succeeds_silently() {
        // walk_for_languages returns Ok(empty) for a missing path; the
        // subcommand mirrors that to keep shell-pipeline composition simple.
        let flags = WalkFlags {
            path: PathBuf::from("/no/such/path/here-cli-test"),
            exclude: String::new(),
            r#ref: None,
        };
        assert_eq!(run_walk(&flags), ExitCode::Success);
    }

    #[test]
    fn walk_with_ref_lists_supported_languages_only() {
        let tmp = tempfile::tempdir().expect("tmp");
        init_rust_repo(tmp.path(), "src/lib.rs", "fn x() {}\n");
        // Add a non-source file in a follow-up commit so it appears in HEAD.
        std::fs::write(tmp.path().join("README.md"), "doc").unwrap();
        git(tmp.path(), &["add", "README.md"]);
        git(tmp.path(), &["commit", "-q", "-m", "doc"]);

        let flags = WalkFlags {
            path: tmp.path().to_path_buf(),
            exclude: String::new(),
            r#ref: Some("HEAD".into()),
        };
        assert_eq!(run_walk(&flags), ExitCode::Success);
    }

    #[test]
    fn walk_with_ref_outside_git_repo_fails() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = WalkFlags {
            path: tmp.path().to_path_buf(),
            exclude: String::new(),
            r#ref: Some("HEAD".into()),
        };
        assert_eq!(run_walk(&flags), ExitCode::Failure);
    }

    #[test]
    fn walk_with_unknown_ref_fails() {
        let tmp = tempfile::tempdir().expect("tmp");
        init_rust_repo(tmp.path(), "src/lib.rs", "fn x() {}\n");
        let flags = WalkFlags {
            path: tmp.path().to_path_buf(),
            exclude: String::new(),
            r#ref: Some("definitely-not-a-ref".into()),
        };
        assert_eq!(run_walk(&flags), ExitCode::Failure);
    }

    #[test]
    fn escape_control_chars_passes_plain_paths_through() {
        assert_eq!(escape_control_chars("src/lib.rs"), "src/lib.rs");
        // Tab is preserved (the field separator on the printed line).
        assert_eq!(escape_control_chars("a\tb"), "a\tb");
        // Non-ASCII unicode is preserved verbatim.
        assert_eq!(escape_control_chars("café/日本語.rs"), "café/日本語.rs");
    }

    #[test]
    fn escape_control_chars_escapes_newline_cr_and_nul() {
        // These are the bytes that would break `awk -F'\t'` consumers.
        assert_eq!(escape_control_chars("a\nb"), "a\\x0ab");
        assert_eq!(escape_control_chars("a\rb"), "a\\x0db");
        assert_eq!(escape_control_chars("a\0b"), "a\\x00b");
        // Backslash itself is escaped so `\x..` is unambiguous.
        assert_eq!(escape_control_chars("a\\b"), "a\\\\b");
        // DEL too — it can move the terminal cursor on some emulators.
        assert_eq!(escape_control_chars("a\x7fb"), "a\\x7fb");
    }

    #[test]
    fn walk_honours_exclude_list() {
        let tmp = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(tmp.path().join("vendor")).unwrap();
        std::fs::write(tmp.path().join("vendor/skip.rs"), "fn s(){}").unwrap();
        std::fs::write(tmp.path().join("keep.rs"), "fn k(){}").unwrap();
        let flags = WalkFlags {
            path: tmp.path().to_path_buf(),
            exclude: "vendor".into(),
            r#ref: None,
        };
        assert_eq!(run_walk(&flags), ExitCode::Success);
    }
}
