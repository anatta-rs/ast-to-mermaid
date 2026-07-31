use super::{check_ref_arg, open_default_cache};
use crate::artifacts::{dir_contains_files, write_artifacts};
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
        allow_empty: flags.allow_empty,
        include_generated: flags.include_generated,
        ..AnalyzeOptions::default()
    };

    let (artifacts, report) = match bundle(&flags.path, &opts) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("bundle: bundle {}: {e}", flags.path.display());
            return ExitCode::Failure;
        }
    };

    // Refuse to wipe a populated `--out` dir with an empty bundle. Without
    // this, `a2m bundle wrong/path --out existing/bundle` would parse zero
    // files, then `write_artifacts` would prune *every* `.mmd` /
    // `.meta.json` in `entities/`. The escape hatch is `--allow-empty`.
    if artifacts.entities.is_empty()
        && !flags.allow_empty
        && dir_contains_files(&flags.out.join("entities"))
    {
        eprintln!(
            "bundle: refusing to overwrite populated {} with an empty bundle \
             ({} produced 0 entities). Pass --allow-empty to override.",
            flags.out.display(),
            flags.path.display(),
        );
        return ExitCode::Failure;
    }

    if let Err(e) = write_artifacts(&artifacts, &flags.out, flags.allow_empty) {
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
            include_generated: false,
            r#ref: None,
            with_sequences: false,
            allow_empty: false,
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
            include_generated: false,
            r#ref: None,
            with_sequences: false,
            allow_empty: false,
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
            include_generated: false,
            r#ref: None,
            with_sequences: true,
            allow_empty: false,
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
            include_generated: false,
            r#ref: None,
            with_sequences: false,
            allow_empty: false,
        };
        assert_eq!(run_bundle(&flags), ExitCode::Success);
        assert!(!out.join("sequences").exists());
    }

    /// `a2m bundle empty/dir --out existing/bundle` used to wipe every
    /// `.mmd` / `.meta.json` under `existing/bundle/entities/` because
    /// the empty parse → empty artifact → unconditional `prune_orphans`
    /// chain ran without any sanity check. The CLI now refuses, with a
    /// message pointing at `--allow-empty`.
    #[test]
    fn bundle_refuses_empty_into_populated_out_dir() {
        let tmp = tempfile::tempdir().expect("tmp");
        // Step 1: produce a populated bundle from a real source dir.
        let src = tmp.path().join("src-real");
        write_rust(
            &src,
            "lib.rs",
            "pub fn caller() { helper(); }\npub fn helper() {}\n",
        );
        let out = tmp.path().join("bundle-out");
        let populate = BundleFlags {
            path: src.clone(),
            out: out.clone(),
            exclude: String::new(),
            include_generated: false,
            r#ref: None,
            with_sequences: false,
            allow_empty: false,
        };
        assert_eq!(run_bundle(&populate), ExitCode::Success);
        let entities_dir = out.join("entities");
        let entity_count_before = std::fs::read_dir(&entities_dir)
            .expect("readdir entities")
            .count();
        assert!(
            entity_count_before > 0,
            "populated bundle must have entity files"
        );

        // Step 2: re-bundle from an empty source dir into the same out
        // dir. Without the safety, this would prune every entity. The
        // CLI must refuse.
        let empty = tmp.path().join("src-empty");
        std::fs::create_dir_all(&empty).expect("mkdir empty");
        let wipe = BundleFlags {
            path: empty.clone(),
            out: out.clone(),
            exclude: String::new(),
            include_generated: false,
            r#ref: None,
            with_sequences: false,
            allow_empty: false,
        };
        assert_eq!(
            run_bundle(&wipe),
            ExitCode::Failure,
            "empty bundle into populated --out must refuse without --allow-empty"
        );
        let entity_count_after = std::fs::read_dir(&entities_dir)
            .expect("readdir entities post-refuse")
            .count();
        assert_eq!(
            entity_count_after, entity_count_before,
            "refusal must leave entities/ untouched"
        );

        // Step 3: same call, `--allow-empty` set. Now the prune runs.
        let force = BundleFlags {
            path: empty,
            out: out.clone(),
            exclude: String::new(),
            include_generated: false,
            r#ref: None,
            with_sequences: false,
            allow_empty: true,
        };
        assert_eq!(run_bundle(&force), ExitCode::Success);
        let entity_count_forced = std::fs::read_dir(&entities_dir)
            .expect("readdir entities post-force")
            .count();
        assert_eq!(
            entity_count_forced, 0,
            "--allow-empty must let the prune sweep entities/"
        );
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
            include_generated: false,
            r#ref: Some("HEAD".into()),
            with_sequences: false,
            allow_empty: false,
        };
        assert_eq!(run_bundle(&flags), ExitCode::Success);
        assert!(out.join("index.json").exists());
    }
}
