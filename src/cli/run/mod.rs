//! Subcommand entry points for the unified `a2m` binary. Each `run_*`
//! function takes the corresponding [`crate::cli::flags`] arg struct and
//! returns an [`ExitCode`].

// `run_*` deliberately drop `#[must_use]`: every caller in `bin/a2m.rs`
// already feeds the return into `ExitCode::into()`, and uniformity beats
// the lint's hint here.
#![allow(clippy::must_use_candidate)]

mod analyze;
mod bundle;
mod diff;
mod flow;
mod gc;
mod index;
mod sequence;
mod walk;

pub use analyze::run_analyze;
pub use bundle::run_bundle;
pub use diff::run_diff;
pub use flow::run_flow;
pub use gc::run_gc;
pub use index::run_index;
pub use sequence::run_sequence;
pub use walk::run_walk;

use crate::cache::Cache;
use crate::cli::flags::ExitCode;
use std::path::Path;
use std::sync::Arc;

/// Open the default cache (`<git-toplevel>/.a2m/cache`) for transparent
/// atom-level caching on the analyze/bundle subcommands. Returns `None` if
/// not in a git repo or if the cache can't be opened — both are non-fatal,
/// the caller falls back to running without atom caching.
fn open_default_cache(start: &Path) -> Option<Arc<Cache>> {
    let toplevel = crate::git_source::show_toplevel(start).ok()?;
    let root = Cache::default_root(&toplevel);
    Cache::open(&root).ok().map(Arc::new)
}

/// Reject a `--ref` value that's empty or shaped like a `git` flag (the
/// `--upload-pack=…` flag-injection vector and friends). Mirrors the
/// stricter [`crate::git_source::validate_git_ref`] check, but maps the
/// failure to [`ExitCode::UsageError`] (exit 2) and emits the error on
/// stderr without ever invoking `git`. `subcommand` is included in the
/// message so users see e.g. `overview: --ref: …` rather than a bare
/// `invalid input: …`.
fn check_ref_arg(subcommand: &str, value: Option<&str>) -> Result<(), ExitCode> {
    let Some(s) = value else {
        return Ok(());
    };
    match crate::git_source::validate_git_ref(s) {
        Ok(_) => Ok(()),
        Err(e) => {
            eprintln!("{subcommand}: --ref: {e}");
            Err(ExitCode::UsageError)
        }
    }
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use std::path::Path;

    /// Spawn `git` against `cwd`, scrubbing inherited `GIT_*` env vars so the
    /// tempdir's tiny repo isn't hijacked by an outer `git commit` /
    /// `git push` running our test suite (same rationale as the helper in
    /// `git_source` tests — see that comment for the gory details).
    pub fn git(cwd: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            panic!("git {args:?} failed: {stderr}");
        }
    }

    /// Initialize a tiny single-file Rust repo with one commit on `main`.
    pub fn init_rust_repo(dir: &Path, file_rel: &str, content: &str) {
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.email", "t@t"]);
        git(dir, &["config", "user.name", "t"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
        let path = dir.join(file_rel);
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(&path, content).unwrap();
        git(dir, &["add", file_rel]);
        git(dir, &["commit", "-q", "-m", "init"]);
    }

    pub fn write_rust(dir: &Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(&path, contents).unwrap();
    }
}
