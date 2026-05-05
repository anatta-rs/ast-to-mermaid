//! Git-backed source enumeration for `--ref` mode.
//!
//! Wraps shell-out to `git rev-parse`, `git ls-tree`, and `git cat-file`.
//! No `libgit2` dependency — `git` on `PATH` is already required for the
//! cache-key model (`git hash-object`-equivalent SHAs) so consistency is free.
//!
//! All functions accept the directory `git -C <dir>` should be invoked in;
//! they do not check for a `.git` directory upfront. Callers that need a
//! repo toplevel should call [`show_toplevel`] first.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use crate::error::{AstToMermaidError, Result};

/// One entry from `git ls-tree -r <ref>`: a blob path and its SHA-1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// Path relative to the tree root, as recorded in git.
    pub path: String,
    /// 40-char SHA-1 of the blob content.
    pub blob_sha: String,
}

/// Validate `s` is safe to pass to `git` as a positional ref argument.
///
/// Rejects strings that look like CLI flags (`--upload-pack=/usr/bin/evil`,
/// the canonical flag-injection vector against `git ls-tree` &
/// `git rev-parse`), strings that contain whitespace, NUL, the path-traversal
/// sequence `..`, or any byte outside printable ASCII (`0x21..=0x7e`).
///
/// On success returns the input slice unchanged so callers can chain:
/// `let ref_arg = validate_git_ref(input)?;`.
///
/// # Errors
/// Returns `InvalidInput` with a message that names the offending feature
/// of the ref so the CLI can surface it to the user.
pub fn validate_git_ref(s: &str) -> Result<&str> {
    if s.is_empty() {
        return Err(AstToMermaidError::InvalidInput("git ref is empty".into()));
    }
    if s.starts_with('-') {
        return Err(AstToMermaidError::InvalidInput(format!(
            "git ref looks like a CLI flag (starts with '-'): {s:?}"
        )));
    }
    if s.contains("..") {
        return Err(AstToMermaidError::InvalidInput(format!(
            "git ref contains '..' (path-traversal-like sequence): {s:?}"
        )));
    }
    for &b in s.as_bytes() {
        // Printable-ASCII gate: anything outside `!`..`~` (which excludes
        // the space, every C0/C1 control byte, NUL, tab, CR, LF, and
        // every non-ASCII byte) is rejected.
        if !(0x21..=0x7e).contains(&b) {
            return Err(AstToMermaidError::InvalidInput(format!(
                "git ref contains disallowed byte 0x{b:02x}: {s:?}"
            )));
        }
    }
    Ok(s)
}

/// Resolve `git_ref` to a 40-char commit SHA via `git rev-parse --verify`.
///
/// `git_ref` is validated by [`validate_git_ref`] before any subprocess is
/// spawned, and is passed after `--end-of-options` so it cannot be
/// reinterpreted as a flag (`--upload-pack=…`) even if validation regressed.
///
/// # Errors
/// Returns `InvalidInput` when the ref is malformed (validation), or when
/// the ref does not exist, with a hint to `git fetch` (the most common
/// cause for a missing ref).
pub fn rev_parse(repo_root: &Path, git_ref: &str) -> Result<String> {
    let git_ref = validate_git_ref(git_ref)?;
    let output = run_git(
        repo_root,
        &["rev-parse", "--verify", "--end-of-options", git_ref],
    )?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AstToMermaidError::InvalidInput(format!(
            "git rev-parse {git_ref}: {} (try `git fetch`?)",
            stderr.trim()
        )));
    }
    let sha = String::from_utf8(output.stdout)
        .map_err(|_| AstToMermaidError::InvalidInput("non-utf8 git output".into()))?;
    Ok(sha.trim().to_owned())
}

/// Locate the toplevel of the git repository containing `start`.
///
/// # Errors
/// Returns `InvalidInput` when `start` is not inside a git work tree.
pub fn show_toplevel(start: &Path) -> Result<PathBuf> {
    let output = run_git(start, &["rev-parse", "--show-toplevel"])?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AstToMermaidError::InvalidInput(format!(
            "not inside a git work tree (start={}): {}",
            start.display(),
            stderr.trim()
        )));
    }
    let raw = String::from_utf8(output.stdout)
        .map_err(|_| AstToMermaidError::InvalidInput("non-utf8 git output".into()))?;
    Ok(PathBuf::from(raw.trim()))
}

/// List `(path, blob_sha)` for every blob in `<ref>`'s tree, recursively.
///
/// Submodules and non-blob entries are skipped. Output is NUL-delimited so
/// paths with embedded whitespace are handled correctly.
///
/// `git_ref` is validated by [`validate_git_ref`] before any subprocess is
/// spawned, and is passed after `--end-of-options` so it cannot be
/// reinterpreted as a flag.
///
/// # Errors
/// Returns `InvalidInput` when the ref is malformed (validation) or
/// `git ls-tree` fails.
pub fn ls_tree(repo_root: &Path, git_ref: &str) -> Result<Vec<TreeEntry>> {
    let git_ref = validate_git_ref(git_ref)?;
    let output = run_git(
        repo_root,
        &[
            "ls-tree",
            "-r",
            "-z",
            "--full-tree",
            "--end-of-options",
            git_ref,
        ],
    )?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AstToMermaidError::InvalidInput(format!(
            "git ls-tree {git_ref}: {}",
            stderr.trim()
        )));
    }
    let raw = String::from_utf8(output.stdout)
        .map_err(|_| AstToMermaidError::InvalidInput("non-utf8 git output".into()))?;

    let mut out = Vec::new();
    for entry in raw.split('\0') {
        if entry.is_empty() {
            continue;
        }
        // Format: "<mode> <type> <sha>\t<path>"
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        let parts: Vec<&str> = meta.splitn(3, ' ').collect();
        if parts.len() != 3 {
            continue;
        }
        if parts[1] != "blob" {
            continue; // skip submodules ("commit"), trees, etc.
        }
        out.push(TreeEntry {
            path: path.to_owned(),
            blob_sha: parts[2].to_owned(),
        });
    }
    Ok(out)
}

/// Read a blob's content by SHA via `git cat-file -p`.
///
/// Thin wrapper around [`BatchReader`] kept for API compatibility and for
/// one-shot reads outside of [`collect_from_git_ref`]-style loops. Callers
/// reading many blobs from the same repo should construct a [`BatchReader`]
/// directly to amortise the subprocess fork+exec across the whole batch.
///
/// # Errors
/// Returns `InvalidInput` when the blob is missing or unreadable.
pub fn cat_file(repo_root: &Path, blob_sha: &str) -> Result<Vec<u8>> {
    let mut reader = BatchReader::spawn(repo_root)?;
    reader.read_blob(blob_sha)
}

/// Persistent `git cat-file --batch` child process for amortised blob reads.
///
/// Spawns one subprocess and feeds SHAs to its stdin, reading the blob bytes
/// back from its stdout. Callers loop with [`Self::read_blob`]; on drop the
/// child's stdin is closed and the process is killed and reaped, guaranteeing
/// no orphan even on panic or early return.
///
/// # Protocol
/// `git cat-file --batch` answers each `<sha>\n` request with a header line
/// `<sha> <type> <size>\n`, followed by `<size>` bytes of content, followed
/// by a single trailing `\n`. Missing objects produce `<sha> missing\n`.
pub struct BatchReader {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl BatchReader {
    /// Spawn `git cat-file --batch` rooted at `repo_root` with stdin / stdout
    /// piped.
    ///
    /// We deliberately do not pass `--buffer` — that flag makes git apply
    /// stdio buffering to its output, which deadlocks an interactive
    /// write-then-read loop because the response stays trapped in git's
    /// internal buffer until it fills up. Without it, git flushes after
    /// every object, which is what we need.
    ///
    /// # Errors
    /// Returns `InvalidInput` when the child fails to spawn.
    pub fn spawn(repo_root: &Path) -> Result<Self> {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .args(["cat-file", "--batch"])
            .env_remove("GIT_DIR")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                AstToMermaidError::InvalidInput(format!("git cat-file --batch spawn: {e}"))
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AstToMermaidError::InvalidInput("git cat-file: stdin missing".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| {
            AstToMermaidError::InvalidInput("git cat-file: stdout missing".into())
        })?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    /// Read the blob at `blob_sha` from the persistent batch stream.
    ///
    /// # Errors
    /// Returns `InvalidInput` when the blob is missing, the header is
    /// malformed, or the child died unexpectedly.
    pub fn read_blob(&mut self, blob_sha: &str) -> Result<Vec<u8>> {
        // Ask for the next blob. `--buffer` requires an explicit flush.
        self.stdin
            .write_all(blob_sha.as_bytes())
            .map_err(|e| AstToMermaidError::InvalidInput(format!("git cat-file write: {e}")))?;
        self.stdin
            .write_all(b"\n")
            .map_err(|e| AstToMermaidError::InvalidInput(format!("git cat-file write: {e}")))?;
        self.stdin
            .flush()
            .map_err(|e| AstToMermaidError::InvalidInput(format!("git cat-file flush: {e}")))?;

        let mut header = String::new();
        let n = self
            .stdout
            .read_line(&mut header)
            .map_err(|e| AstToMermaidError::InvalidInput(format!("git cat-file read: {e}")))?;
        if n == 0 {
            return Err(AstToMermaidError::InvalidInput(
                "git cat-file: unexpected EOF on header".into(),
            ));
        }
        let trimmed = header.trim_end_matches('\n');
        // Format: "<sha> <type> <size>" or "<sha> missing".
        let parts: Vec<&str> = trimmed.splitn(3, ' ').collect();
        if parts.len() == 2 && parts[1] == "missing" {
            return Err(AstToMermaidError::InvalidInput(format!(
                "git cat-file {blob_sha}: missing"
            )));
        }
        if parts.len() != 3 {
            return Err(AstToMermaidError::InvalidInput(format!(
                "git cat-file: malformed header {trimmed:?}"
            )));
        }
        let size: usize = parts[2].parse().map_err(|_| {
            AstToMermaidError::InvalidInput(format!(
                "git cat-file: non-numeric size in header {trimmed:?}"
            ))
        })?;

        let mut buf = vec![0u8; size];
        self.stdout
            .read_exact(&mut buf)
            .map_err(|e| AstToMermaidError::InvalidInput(format!("git cat-file body: {e}")))?;
        // Consume the trailing newline that follows every blob in the
        // `--batch` stream so the next header lines up.
        let mut nl = [0u8; 1];
        self.stdout
            .read_exact(&mut nl)
            .map_err(|e| AstToMermaidError::InvalidInput(format!("git cat-file trailer: {e}")))?;
        Ok(buf)
    }
}

impl Drop for BatchReader {
    fn drop(&mut self) {
        // Best-effort shutdown: closing stdin signals git to exit; if it
        // doesn't, kill the child outright. Always reap so the OS doesn't
        // accumulate zombies.
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<std::process::Output> {
    // Strip ambient `GIT_*` env vars before invoking the subprocess. They
    // override `-C` / `current_dir` and would silently retarget our
    // operations at whatever repository the caller was already inside —
    // most painfully when `a2m` is invoked from a pre-commit hook
    // running inside `git commit` (the parent operation exports
    // `GIT_DIR`, `GIT_INDEX_FILE`, …) but also any other nested call.
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| AstToMermaidError::InvalidInput(format!("git spawn ({args:?}): {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    /// Create a tiny git repo at `dir` with one committed file. Returns the
    /// committed file's blob SHA-1 and the commit SHA-1.
    fn init_repo(dir: &Path, file_rel: &str, content: &str) -> (String, String) {
        run_or_panic(dir, &["init", "-q", "-b", "main"]);
        run_or_panic(dir, &["config", "user.email", "t@t"]);
        run_or_panic(dir, &["config", "user.name", "t"]);
        run_or_panic(dir, &["config", "commit.gpgsign", "false"]);
        let path = dir.join(file_rel);
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(&path, content).unwrap();
        run_or_panic(dir, &["add", file_rel]);
        run_or_panic(dir, &["commit", "-q", "-m", "init"]);
        let blob = String::from_utf8(run_or_panic(dir, &["hash-object", file_rel]).stdout)
            .unwrap()
            .trim()
            .to_owned();
        let commit = String::from_utf8(run_or_panic(dir, &["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_owned();
        (blob, commit)
    }

    fn run_or_panic(cwd: &Path, args: &[&str]) -> std::process::Output {
        // Strip any ambient `GIT_*` env vars before invoking git: when this
        // test suite runs from inside a `git commit` / `git push` (e.g. via
        // a pre-commit hook that calls `cargo test`), the parent operation
        // exports `GIT_DIR`, `GIT_INDEX_FILE`, `GIT_WORK_TREE`, and
        // `GIT_OBJECT_DIRECTORY`. Those override `-C` / `current_dir` and
        // make the subprocess operate against the *parent* repository
        // instead of the tempdir we just initialised — historically this
        // produced "could not lock config" failures and, worse, ghost
        // commits authored by the fixture's `t <t@t>` identity that
        // overwrote real source files in the parent worktree.
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
        out
    }

    #[test]
    fn validate_git_ref_accepts_real_refs() {
        for good in [
            "HEAD",
            "main",
            "master",
            "v0.5.0",
            "abcdef0",
            "feature/foo-bar",
            "HEAD~3",
            "HEAD^",
            "release-1.2.3",
        ] {
            assert!(
                validate_git_ref(good).is_ok(),
                "expected `{good}` to validate"
            );
        }
    }

    #[test]
    fn validate_git_ref_rejects_flag_like_inputs() {
        for bad in [
            "--upload-pack=/usr/bin/evil",
            "--upload-pack=/bin/echo",
            "-x",
            "--help",
        ] {
            let err = validate_git_ref(bad).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("CLI flag") || msg.contains("starts with '-'"),
                "expected flag-rejection message for `{bad}`, got: {msg}"
            );
        }
    }

    #[test]
    fn validate_git_ref_rejects_path_traversal() {
        let err = validate_git_ref("..").unwrap_err();
        assert!(err.to_string().contains(".."));
        let err = validate_git_ref("foo/../bar").unwrap_err();
        assert!(err.to_string().contains(".."));
    }

    #[test]
    fn validate_git_ref_rejects_whitespace_nul_and_nonprintable() {
        for bad in [
            "with space",
            "tab\there",
            "newline\nhere",
            "carriage\rreturn",
            "nul\0byte",
            "café", // non-ASCII byte
        ] {
            assert!(
                validate_git_ref(bad).is_err(),
                "expected `{bad:?}` to be rejected"
            );
        }
    }

    #[test]
    fn validate_git_ref_rejects_empty_string() {
        assert!(validate_git_ref("").is_err());
    }

    #[test]
    fn rev_parse_rejects_flag_like_ref_without_spawning_git() {
        // No git invocation needed: validation runs before any subprocess.
        // Use a non-existent path to prove no `git` is even started — if it
        // were, it would error with a different message about the cwd.
        let nowhere = Path::new("/definitely/not/a/path/exists/here");
        let err = rev_parse(nowhere, "--upload-pack=/bin/echo HACKED").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("CLI flag") || msg.contains("starts with '-'"),
            "expected validation error, got: {msg}"
        );
    }

    #[test]
    fn ls_tree_rejects_flag_like_ref_without_spawning_git() {
        let nowhere = Path::new("/definitely/not/a/path/exists/here");
        let err = ls_tree(nowhere, "--upload-pack=evil").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("CLI flag") || msg.contains("starts with '-'"),
            "expected validation error, got: {msg}"
        );
    }

    #[test]
    fn rev_parse_resolves_head() {
        let tmp = tempdir().unwrap();
        let (_, commit) = init_repo(tmp.path(), "src/lib.rs", "fn x(){}\n");
        let resolved = rev_parse(tmp.path(), "HEAD").unwrap();
        assert_eq!(resolved, commit);
    }

    #[test]
    fn rev_parse_unknown_ref_errors_with_fetch_hint() {
        let tmp = tempdir().unwrap();
        let _ = init_repo(tmp.path(), "f.rs", "fn x(){}");
        let err = rev_parse(tmp.path(), "definitely-not-a-ref").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("git fetch"), "missing hint: {msg}");
    }

    #[test]
    fn show_toplevel_returns_repo_root() {
        let tmp = tempdir().unwrap();
        let (_, _) = init_repo(tmp.path(), "src/lib.rs", "fn x(){}");
        let top = show_toplevel(&tmp.path().join("src")).unwrap();
        // Realpath equality: macOS tmpdirs symlink /tmp to /private/tmp.
        assert_eq!(
            fs::canonicalize(&top).unwrap(),
            fs::canonicalize(tmp.path()).unwrap()
        );
    }

    #[test]
    fn ls_tree_yields_path_and_blob_sha() {
        let tmp = tempdir().unwrap();
        let (blob, _) = init_repo(tmp.path(), "src/lib.rs", "fn x(){}\n");
        let entries = ls_tree(tmp.path(), "HEAD").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "src/lib.rs");
        assert_eq!(entries[0].blob_sha, blob);
    }

    #[test]
    fn cat_file_reads_blob_content() {
        let tmp = tempdir().unwrap();
        let (blob, _) = init_repo(tmp.path(), "src/lib.rs", "fn x(){}\n");
        let content = cat_file(tmp.path(), &blob).unwrap();
        assert_eq!(content, b"fn x(){}\n");
    }

    #[test]
    fn cat_file_unknown_blob_errors() {
        let tmp = tempdir().unwrap();
        let _ = init_repo(tmp.path(), "f.rs", "fn x(){}");
        let err = cat_file(tmp.path(), "0000000000000000000000000000000000000000").unwrap_err();
        assert!(err.to_string().contains("git cat-file"));
    }

    #[test]
    fn batch_reader_reads_single_blob() {
        let tmp = tempdir().unwrap();
        let (blob, _) = init_repo(tmp.path(), "src/lib.rs", "fn x(){}\n");
        let mut reader = BatchReader::spawn(tmp.path()).unwrap();
        let content = reader.read_blob(&blob).unwrap();
        assert_eq!(content, b"fn x(){}\n");
    }

    #[test]
    fn batch_reader_reads_multiple_blobs_in_sequence() {
        let tmp = tempdir().unwrap();
        run_or_panic(tmp.path(), &["init", "-q", "-b", "main"]);
        run_or_panic(tmp.path(), &["config", "user.email", "t@t"]);
        run_or_panic(tmp.path(), &["config", "user.name", "t"]);
        run_or_panic(tmp.path(), &["config", "commit.gpgsign", "false"]);
        let files = [
            ("a.rs", "fn a(){}\n"),
            ("b.rs", "fn b(){println!(\"hi\");}\n"),
            ("c.rs", ""), // empty blob — exercises size=0 path
        ];
        let mut shas = Vec::new();
        for (name, content) in &files {
            fs::write(tmp.path().join(name), content).unwrap();
            run_or_panic(tmp.path(), &["add", name]);
            let sha = String::from_utf8(run_or_panic(tmp.path(), &["hash-object", name]).stdout)
                .unwrap()
                .trim()
                .to_owned();
            shas.push(sha);
        }
        run_or_panic(tmp.path(), &["commit", "-q", "-m", "init"]);

        let mut reader = BatchReader::spawn(tmp.path()).unwrap();
        for ((_, expected), sha) in files.iter().zip(&shas) {
            let got = reader.read_blob(sha).unwrap();
            assert_eq!(got, expected.as_bytes(), "sha {sha}");
        }
        // Same reader can be re-used for blobs it has already served — it's
        // just a stream of (sha, response) pairs to git.
        let again = reader.read_blob(&shas[0]).unwrap();
        assert_eq!(again, files[0].1.as_bytes());
    }

    #[test]
    fn batch_reader_missing_blob_errors_without_killing_stream() {
        let tmp = tempdir().unwrap();
        let (blob, _) = init_repo(tmp.path(), "src/lib.rs", "fn x(){}\n");
        let mut reader = BatchReader::spawn(tmp.path()).unwrap();
        let err = reader
            .read_blob("0000000000000000000000000000000000000000")
            .unwrap_err();
        assert!(err.to_string().contains("missing"), "{err}");
        // Stream is still usable: a real sha after a missing one must succeed.
        let content = reader.read_blob(&blob).unwrap();
        assert_eq!(content, b"fn x(){}\n");
    }

    #[test]
    fn batch_reader_drop_kills_child() {
        // Smoke test: dropping the reader must not leak zombies. We can't
        // easily assert that across platforms, but we can at least verify
        // the Drop runs without panic.
        let tmp = tempdir().unwrap();
        let _ = init_repo(tmp.path(), "f.rs", "fn x(){}\n");
        {
            let _reader = BatchReader::spawn(tmp.path()).unwrap();
        }
        // Spawning a fresh reader after the previous one was dropped must
        // still work (i.e. nothing left half-broken in the repo).
        let _reader2 = BatchReader::spawn(tmp.path()).unwrap();
    }
}
