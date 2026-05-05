use super::check_ref_arg;
use crate::cache::{Cache, write_bundle_atomic};
use crate::cli::flags::{ExitCode, IndexFlags};
use crate::pipeline::{AnalyzeOptions, bundle, snapshot_id};
use std::sync::Arc;

/// Run the `index` subcommand: materialize a bundle for a ref (or the
/// working tree) into the cache. Idempotent — cached re-runs are a no-op
/// unless `--force` is set.
pub fn run_index(flags: &IndexFlags) -> ExitCode {
    if let Err(code) = check_ref_arg("index", flags.r#ref.as_deref()) {
        return code;
    }
    let (cache_root, _ephemeral) = match flags.cache.resolve(&flags.path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("index: resolve cache root: {e}");
            return ExitCode::Failure;
        }
    };
    let cache = match Cache::open(&cache_root) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("index: open cache {}: {e}", cache_root.display());
            return ExitCode::Failure;
        }
    };
    if let Err(e) = cache.ensure_gitignore() {
        eprintln!("index: write .gitignore: {e}");
    }

    let sha = match snapshot_id(&flags.path, flags.r#ref.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("index: resolve snapshot: {e}");
            return ExitCode::Failure;
        }
    };

    if cache.has_bundle(&sha) && !flags.force && !flags.cache.no_cache {
        eprintln!("cached {} → {}", sha, cache.bundle_dir(&sha).display());
        return ExitCode::Success;
    }

    // The parse loop also gets the cache so atom-level dedup applies even
    // during a fresh `index` (cross-ref blob reuse). Cache::open errors are
    // non-fatal; skip atom caching in that case.
    let opts = AnalyzeOptions {
        git_ref: flags.r#ref.clone(),
        cache: Cache::open(&cache_root).ok().map(Arc::new),
        with_sequences: flags.with_sequences,
        ..AnalyzeOptions::default()
    };
    let (artifacts, report) = match bundle(&flags.path, &opts) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("index: bundle {}: {e}", flags.path.display());
            return ExitCode::Failure;
        }
    };

    let bundle_dir = cache.bundle_dir(&sha);
    if let Err(e) = write_bundle_atomic(&artifacts, &bundle_dir) {
        eprintln!("index: write {}: {e}", bundle_dir.display());
        return ExitCode::Failure;
    }

    eprintln!(
        "indexed {} → {} ({} files, {} atoms, {} edges)",
        sha,
        bundle_dir.display(),
        report.files_parsed,
        report.atoms_indexed,
        report.edges_resolved,
    );
    ExitCode::Success
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::flags::CacheArgs;
    use crate::cli::run::test_helpers::init_rust_repo;

    #[test]
    fn index_succeeds_then_is_idempotent() {
        let tmp = tempfile::tempdir().expect("tmp");
        init_rust_repo(tmp.path(), "src/lib.rs", "fn a(){}\nfn b(){a()}\n");
        let cache_dir = tmp.path().join("cache");

        let flags = IndexFlags {
            path: tmp.path().to_path_buf(),
            r#ref: Some("HEAD".into()),
            force: false,
            with_sequences: false,
            cache: CacheArgs {
                cache_dir: Some(cache_dir.clone()),
                no_cache: false,
            },
        };
        assert_eq!(run_index(&flags), ExitCode::Success);

        // Second run hits the `cached` short-circuit branch.
        assert_eq!(run_index(&flags), ExitCode::Success);

        // With --force, we re-materialize even when the bundle exists.
        let forced = IndexFlags {
            force: true,
            ..flags.clone()
        };
        assert_eq!(run_index(&forced), ExitCode::Success);
    }

    #[test]
    fn index_with_unknown_ref_fails() {
        let tmp = tempfile::tempdir().expect("tmp");
        init_rust_repo(tmp.path(), "src/lib.rs", "fn x(){}\n");
        let flags = IndexFlags {
            path: tmp.path().to_path_buf(),
            r#ref: Some("nope".into()),
            force: false,
            with_sequences: false,
            cache: CacheArgs {
                cache_dir: Some(tmp.path().join("cache")),
                no_cache: false,
            },
        };
        assert_eq!(run_index(&flags), ExitCode::Failure);
    }

    #[test]
    fn index_no_cache_skips_persistence() {
        let tmp = tempfile::tempdir().expect("tmp");
        init_rust_repo(tmp.path(), "src/lib.rs", "fn x(){}\n");
        let flags = IndexFlags {
            path: tmp.path().to_path_buf(),
            r#ref: Some("HEAD".into()),
            force: false,
            with_sequences: false,
            cache: CacheArgs {
                cache_dir: None,
                no_cache: true,
            },
        };
        assert_eq!(run_index(&flags), ExitCode::Success);
    }
}
