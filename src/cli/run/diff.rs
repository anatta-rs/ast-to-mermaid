use super::check_ref_arg;
use crate::cache::{Cache, write_bundle_atomic};
use crate::cli::flags::{DiffFlags, DiffFormat, ExitCode};
use crate::diff::{compute_diff, load_bundle_entities, render_mermaid};
use crate::pipeline::{AnalyzeOptions, bundle, snapshot_id};
use std::path::Path;
use std::sync::Arc;

/// Run the `diff` subcommand: compute the structural diff between two
/// cached bundles. Auto-runs `index` for any ref that isn't already cached.
pub fn run_diff(flags: &DiffFlags) -> ExitCode {
    let Some((ref_a, ref_b)) = flags.range.split_once("..") else {
        eprintln!("diff: expected `<ref-a>..<ref-b>`, got `{}`", flags.range);
        return ExitCode::UsageError;
    };
    if ref_a.is_empty() || ref_b.is_empty() {
        eprintln!("diff: both refs must be non-empty in `{}`", flags.range);
        return ExitCode::UsageError;
    }
    if let Err(code) = check_ref_arg("diff", Some(ref_a)) {
        return code;
    }
    if let Err(code) = check_ref_arg("diff", Some(ref_b)) {
        return code;
    }

    let (cache_root, _ephemeral) = match flags.cache.resolve(&flags.path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("diff: resolve cache root: {e}");
            return ExitCode::Failure;
        }
    };
    let cache = match Cache::open(&cache_root) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("diff: open cache {}: {e}", cache_root.display());
            return ExitCode::Failure;
        }
    };

    let from_sha = match ensure_indexed(&cache, &flags.path, ref_a, flags.cache.no_cache) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("diff: index {ref_a}: {e}");
            return ExitCode::Failure;
        }
    };
    let to_sha = match ensure_indexed(&cache, &flags.path, ref_b, flags.cache.no_cache) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("diff: index {ref_b}: {e}");
            return ExitCode::Failure;
        }
    };

    let from_entities = match load_bundle_entities(&cache.bundle_dir(&from_sha)) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("diff: load bundle {ref_a}: {e}");
            return ExitCode::Failure;
        }
    };
    let to_entities = match load_bundle_entities(&cache.bundle_dir(&to_sha)) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("diff: load bundle {ref_b}: {e}");
            return ExitCode::Failure;
        }
    };

    // Clone the post-state entities so the renderer can walk their edges
    // (compute_diff consumes its inputs to build the lookup HashMap).
    let to_for_render = to_entities.clone();
    let result = compute_diff(ref_a, ref_b, &from_sha, &to_sha, from_entities, to_entities);

    match flags.format {
        DiffFormat::Mermaid => print!("{}", render_mermaid(&result, &to_for_render)),
        DiffFormat::Json => match serde_json::to_string_pretty(&result) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("diff: serialize json: {e}");
                return ExitCode::Failure;
            }
        },
    }

    eprintln!(
        "diff {ref_a} → {ref_b}: +{} -{} ~{} ↪{}",
        result.added.len(),
        result.removed.len(),
        result.modified.len(),
        result.renamed.len(),
    );
    ExitCode::Success
}

fn ensure_indexed(
    cache: &Cache,
    path: &Path,
    git_ref: &str,
    no_cache: bool,
) -> Result<String, crate::error::AstToMermaidError> {
    let sha = snapshot_id(path, Some(git_ref))?;
    if !no_cache && cache.has_bundle(&sha) {
        return Ok(sha);
    }
    let opts = AnalyzeOptions {
        git_ref: Some(git_ref.to_owned()),
        cache: Some(Arc::new(Cache::open(cache.root())?)),
        ..AnalyzeOptions::default()
    };
    let (artifacts, _report) = bundle(path, &opts)?;
    write_bundle_atomic(&artifacts, &cache.bundle_dir(&sha))?;
    Ok(sha)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::flags::CacheArgs;
    use crate::cli::run::test_helpers::{git, init_rust_repo};

    #[test]
    fn diff_range_without_double_dot_is_usage_error() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = DiffFlags {
            range: "abc".into(),
            path: tmp.path().to_path_buf(),
            format: DiffFormat::Mermaid,
            cache: CacheArgs::default(),
        };
        assert_eq!(run_diff(&flags), ExitCode::UsageError);
    }

    #[test]
    fn diff_range_with_empty_side_is_usage_error() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = DiffFlags {
            range: "..main".into(),
            path: tmp.path().to_path_buf(),
            format: DiffFormat::Mermaid,
            cache: CacheArgs::default(),
        };
        assert_eq!(run_diff(&flags), ExitCode::UsageError);
    }

    #[test]
    fn diff_with_unknown_first_ref_fails() {
        let tmp = tempfile::tempdir().expect("tmp");
        init_rust_repo(tmp.path(), "src/lib.rs", "fn x(){}\n");
        let flags = DiffFlags {
            range: "nope..HEAD".into(),
            path: tmp.path().to_path_buf(),
            format: DiffFormat::Mermaid,
            cache: CacheArgs {
                cache_dir: Some(tmp.path().join("cache")),
                no_cache: false,
            },
        };
        assert_eq!(run_diff(&flags), ExitCode::Failure);
    }

    #[test]
    fn diff_two_commits_succeeds_in_both_formats() {
        let tmp = tempfile::tempdir().expect("tmp");
        init_rust_repo(tmp.path(), "src/lib.rs", "fn a(){}\n");
        // Second commit modifies the file → non-trivial diff.
        std::fs::write(tmp.path().join("src/lib.rs"), "fn a(){}\nfn b(){}\n").unwrap();
        git(tmp.path(), &["add", "src/lib.rs"]);
        git(tmp.path(), &["commit", "-q", "-m", "add b"]);

        let cache_dir = tmp.path().join("cache");
        let mermaid = DiffFlags {
            range: "HEAD~1..HEAD".into(),
            path: tmp.path().to_path_buf(),
            format: DiffFormat::Mermaid,
            cache: CacheArgs {
                cache_dir: Some(cache_dir.clone()),
                no_cache: false,
            },
        };
        assert_eq!(run_diff(&mermaid), ExitCode::Success);

        // Re-run with JSON to exercise the second arm (and the
        // ensure_indexed cache-hit short-circuit).
        let json = DiffFlags {
            format: DiffFormat::Json,
            ..mermaid.clone()
        };
        assert_eq!(run_diff(&json), ExitCode::Success);
    }
}
