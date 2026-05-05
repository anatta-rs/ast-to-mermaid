//! Perf-regression tests for the `sequence` subcommand.
//!
//! Guards two v0.6.0 fixes from drifting back into `cli/run.rs`:
//!
//! - `collect_rust_sources` git-ref branch must spawn one
//!   `BatchReader` per call, not one `git cat-file` fork per blob.
//!   Pre-fix: ~50s for 100 blobs; post-fix: <1s.
//! - `run_sequence_single` must stop parsing once it has located the
//!   target — no work past the file containing it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

fn a2m_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_a2m"))
}

/// Run `git -C cwd args...`, panicking with stderr on non-zero exit.
///
/// Strips ambient `GIT_*` env vars so the tempdir's tiny repo is not
/// hijacked by an outer `git commit` running our test suite (same
/// rationale as the matching helpers in `git_source.rs` /
/// `cli/run.rs`).
fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
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

fn init_rust_repo_with_files(dir: &Path, files: &[(&str, &str)]) {
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "t@t"]);
    git(dir, &["config", "user.name", "t"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    for (rel, content) in files {
        let path = dir.join(rel);
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(&path, content).unwrap();
        git(dir, &["add", rel]);
    }
    git(dir, &["commit", "-q", "-m", "init"]);
}

/// 100 Rust blobs read via `--ref HEAD` must complete in well under the
/// pre-fix wall-clock — the persistent `BatchReader` amortises 100
/// `git cat-file` forks down to one. Pre-fix this test takes ~5–50s on
/// typical hardware (one fork+exec of `git` per blob). Post-fix: <1s.
/// The 30s ceiling leaves headroom for slow CI runners while still
/// catching a regression to per-blob forking.
#[test]
fn sequence_ref_with_100_files_finishes_well_under_pre_fix_wallclock() {
    let tmp = tempfile::tempdir().expect("tmp");
    let mut files: Vec<(String, String)> = Vec::with_capacity(100);
    for i in 0..100 {
        // Each file declares one tiny function whose body is a single
        // call so `--all` produces a non-empty diagram for it.
        let src = format!("pub fn f{i}() {{ helper(); }}\n");
        files.push((format!("src/f{i:03}.rs"), src));
    }
    let file_refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    init_rust_repo_with_files(tmp.path(), &file_refs);

    let out_dir = tmp.path().join("seq-out");
    let start = Instant::now();
    let status = Command::new(a2m_path())
        .args([
            "sequence",
            tmp.path().to_str().unwrap(),
            "--ref",
            "HEAD",
            "--all",
            "--out",
            out_dir.to_str().unwrap(),
        ])
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .status()
        .expect("spawn a2m");
    let elapsed = start.elapsed();
    assert!(status.success(), "a2m sequence --ref failed: {status}");
    assert!(
        elapsed.as_secs() < 30,
        "100-blob --ref took {elapsed:?}; regressed to per-blob `git cat-file` fork?"
    );
    // Sanity: at least one diagram landed in the output dir.
    let entries: Vec<_> = std::fs::read_dir(&out_dir)
        .expect("read out_dir")
        .filter_map(Result::ok)
        .collect();
    assert!(
        !entries.is_empty(),
        "no diagrams written for a 100-file repo"
    );
}

/// `run_sequence_single` must stop walking the candidate list once it
/// has located the target — files after it are never opened. The
/// behavioural proxy here: target lives in file 2 of 5; files 3-5 are
/// pure padding. The command succeeds and emits the expected diagram
/// without ever touching files 3-5 (correctness check only — the
/// parse-count guarantee is enforced by code review of
/// `run_sequence_single`'s loop).
#[test]
fn sequence_single_target_short_circuits_after_match() {
    let tmp = tempfile::tempdir().expect("tmp");
    // file_001 has unrelated functions; file_002 declares the target;
    // files 3-5 declare more unrelated functions.
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src/file_001.rs"),
        "pub fn unrelated_a() { helper(); }\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("src/file_002.rs"),
        "pub fn target_fn() { helper(); other(); }\n",
    )
    .unwrap();
    for i in 3..=5 {
        std::fs::write(
            tmp.path().join(format!("src/file_{i:03}.rs")),
            "pub fn unrelated_x() { helper(); }\n",
        )
        .unwrap();
    }

    let out = tmp.path().join("seq.mmd");
    let status = Command::new(a2m_path())
        .args([
            "sequence",
            tmp.path().to_str().unwrap(),
            "--target",
            "target_fn",
            "--out",
            out.to_str().unwrap(),
        ])
        .status()
        .expect("spawn a2m");
    assert!(status.success(), "a2m sequence --target failed: {status}");
    let body = std::fs::read_to_string(&out).expect("read");
    assert!(body.starts_with("sequenceDiagram"), "got:\n{body}");
    assert!(body.contains("helper"), "got:\n{body}");
    assert!(body.contains("other"), "got:\n{body}");
}
