use crate::cache::{Cache, GcOptions};
use crate::cli::flags::{ExitCode, GcFlags};
use crate::cli::format::{parse_duration, parse_size};

/// Run the `gc` subcommand: evict old / oversized cache entries.
pub fn run_gc(flags: &GcFlags) -> ExitCode {
    let max_size = match parse_size(&flags.max_size) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("gc: --max-size: {e}");
            return ExitCode::UsageError;
        }
    };
    let older_than = match flags.older_than.as_deref() {
        Some(s) => match parse_duration(s) {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("gc: --older-than: {e}");
                return ExitCode::UsageError;
            }
        },
        None => None,
    };

    let (cache_root, _ephemeral) = match flags.cache.resolve(&flags.path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("gc: resolve cache root: {e}");
            return ExitCode::Failure;
        }
    };
    let cache = match Cache::open(&cache_root) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("gc: open cache {}: {e}", cache_root.display());
            return ExitCode::Failure;
        }
    };

    let opts = GcOptions {
        max_size_bytes: Some(max_size),
        older_than,
        dry_run: flags.dry_run,
    };
    let report = match cache.gc(&opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("gc: collect {}: {e}", cache_root.display());
            return ExitCode::Failure;
        }
    };

    let verb = if flags.dry_run {
        "would remove"
    } else {
        "removed"
    };
    eprintln!(
        "{verb} {} entries ({} bytes) from {} (had {} entries, {} bytes)",
        report.removed_count,
        report.removed_size,
        cache_root.display(),
        report.count_before,
        report.total_before,
    );
    ExitCode::Success
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::flags::CacheArgs;

    #[test]
    fn gc_succeeds_on_empty_cache() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = GcFlags {
            path: tmp.path().to_path_buf(),
            max_size: "1G".into(),
            older_than: None,
            dry_run: false,
            cache: CacheArgs {
                cache_dir: Some(tmp.path().join("cache")),
                no_cache: false,
            },
        };
        assert_eq!(run_gc(&flags), ExitCode::Success);
    }

    #[test]
    fn gc_dry_run_with_age_filter_succeeds() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = GcFlags {
            path: tmp.path().to_path_buf(),
            max_size: "10M".into(),
            older_than: Some("30d".into()),
            dry_run: true,
            cache: CacheArgs {
                cache_dir: Some(tmp.path().join("cache")),
                no_cache: false,
            },
        };
        assert_eq!(run_gc(&flags), ExitCode::Success);
    }

    #[test]
    fn gc_bad_max_size_is_usage_error() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = GcFlags {
            path: tmp.path().to_path_buf(),
            max_size: "huge".into(),
            older_than: None,
            dry_run: false,
            cache: CacheArgs::default(),
        };
        assert_eq!(run_gc(&flags), ExitCode::UsageError);
    }

    #[test]
    fn gc_bad_older_than_is_usage_error() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = GcFlags {
            path: tmp.path().to_path_buf(),
            max_size: "1G".into(),
            older_than: Some("forever".into()),
            dry_run: false,
            cache: CacheArgs::default(),
        };
        assert_eq!(run_gc(&flags), ExitCode::UsageError);
    }
}
