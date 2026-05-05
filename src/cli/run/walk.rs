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
        println!("{}\t{}", lang.name(), entry.path);
    }
    ExitCode::Success
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
