use super::{check_ref_arg, open_default_cache};
use crate::artifacts::write_artifacts;
use crate::cli::flags::{BundleFlags, ExitCode};
use crate::cli::format::parse_csv_exclude;
use crate::pipeline::{AnalyzeOptions, bundle};

/// Run the artifact-bundle subcommand: parse → resolve → emit a directory
/// of per-entity Mermaid + metadata files plus a top-level `index.json`.
pub fn run_bundle(flags: &BundleFlags) -> ExitCode {
    if let Err(code) = check_ref_arg("bundle", flags.r#ref.as_deref()) {
        return code;
    }
    let exclude = parse_csv_exclude(&flags.exclude);

    let cache = open_default_cache(&flags.path);

    let opts = AnalyzeOptions {
        exclude,
        git_ref: flags.r#ref.clone(),
        cache,
        with_sequences: flags.with_sequences,
        ..AnalyzeOptions::default()
    };

    let (artifacts, report) = match bundle(&flags.path, &opts) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("bundle: bundle {}: {e}", flags.path.display());
            return ExitCode::Failure;
        }
    };

    if let Err(e) = write_artifacts(&artifacts, &flags.out) {
        eprintln!("bundle: write {}: {e}", flags.out.display());
        return ExitCode::Failure;
    }

    if !report.failures.is_empty() {
        eprintln!(
            "skipped {} files (see --trace=warn for details)",
            report.failures.len(),
        );
    }

    eprintln!(
        "bundled {} files, {} atoms, {} edges, {} entities → {}",
        report.files_parsed,
        report.atoms_indexed,
        report.edges_resolved,
        artifacts.entities.len(),
        flags.out.display(),
    );

    ExitCode::Success
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::run::test_helpers::{init_rust_repo, write_rust};
    use std::path::PathBuf;

    #[test]
    fn bundle_on_empty_dir_succeeds_and_writes_index() {
        let tmp = tempfile::tempdir().expect("tmp");
        let out = tmp.path().join("bundle-out");
        let flags = BundleFlags {
            path: tmp.path().to_path_buf(),
            out: out.clone(),
            exclude: String::new(),
            r#ref: None,
            with_sequences: false,
        };
        assert_eq!(run_bundle(&flags), ExitCode::Success);
        assert!(out.join("index.json").exists());
        assert!(out.join("overview.mmd").exists());
    }

    #[test]
    fn bundle_with_missing_path_returns_failure() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = BundleFlags {
            path: PathBuf::from("/no/such/path/here-cli-test"),
            out: tmp.path().join("bundle-out"),
            exclude: String::new(),
            r#ref: None,
            with_sequences: false,
        };
        assert_eq!(run_bundle(&flags), ExitCode::Failure);
    }

    #[test]
    fn bundle_with_sequences_flag_writes_sequences_dir() {
        let tmp = tempfile::tempdir().expect("tmp");
        write_rust(
            tmp.path(),
            "src/lib.rs",
            "pub fn caller() { helper(); }\npub fn helper() {}\n",
        );
        let out = tmp.path().join("bundle-out");
        let flags = BundleFlags {
            path: tmp.path().to_path_buf(),
            out: out.clone(),
            exclude: String::new(),
            r#ref: None,
            with_sequences: true,
        };
        assert_eq!(run_bundle(&flags), ExitCode::Success);
        assert!(out.join("sequences").is_dir());
    }

    #[test]
    fn bundle_without_sequences_flag_skips_sequences_dir() {
        let tmp = tempfile::tempdir().expect("tmp");
        write_rust(
            tmp.path(),
            "src/lib.rs",
            "pub fn caller() { helper(); }\npub fn helper() {}\n",
        );
        let out = tmp.path().join("bundle-out");
        let flags = BundleFlags {
            path: tmp.path().to_path_buf(),
            out: out.clone(),
            exclude: String::new(),
            r#ref: None,
            with_sequences: false,
        };
        assert_eq!(run_bundle(&flags), ExitCode::Success);
        assert!(!out.join("sequences").exists());
    }

    #[test]
    fn bundle_from_git_ref_succeeds() {
        let tmp = tempfile::tempdir().expect("tmp");
        init_rust_repo(tmp.path(), "src/lib.rs", "fn a(){}\n");
        let out = tmp.path().join("bundle-out");
        let flags = BundleFlags {
            path: tmp.path().to_path_buf(),
            out: out.clone(),
            exclude: String::new(),
            r#ref: Some("HEAD".into()),
            with_sequences: false,
        };
        assert_eq!(run_bundle(&flags), ExitCode::Success);
        assert!(out.join("index.json").exists());
    }
}
