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

/// Validate `s` is a canonical 40-character lowercase ASCII hex SHA-1.
///
/// `git cat-file --batch` is a stdin-driven protocol: anything we write to
/// git's stdin is interpreted as a fresh request line. A SHA that contains
/// a `\n` (or any other byte git treats as a separator) would let a caller
/// inject extra requests, desync the response stream, or hang the process.
/// 40 lowercase hex is the canonical form `ls-tree` and `hash-object` emit,
/// so anything else is a bug at the call site, not user input we have to
/// be lenient about.
///
/// # Errors
/// Returns `InvalidInput` when `s` is not exactly 40 chars, or when any
/// char is outside `0-9a-f`.
pub fn validate_blob_sha(s: &str) -> Result<&str> {
    if s.len() != 40 {
        return Err(AstToMermaidError::InvalidInput(format!(
            "blob sha must be 40 hex chars, got {} chars: {s:?}",
            s.len()
        )));
    }
    for &b in s.as_bytes() {
        if !matches!(b, b'0'..=b'9' | b'a'..=b'f') {
            return Err(AstToMermaidError::InvalidInput(format!(
                "blob sha contains non-hex byte 0x{b:02x}: {s:?}"
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
/// Non-UTF-8 paths (rare but legal in git — filesystems on Linux allow any
/// byte except `/` and NUL) are decoded with `from_utf8_lossy`, logged via
/// `tracing::warn!`, and skipped. The rest of the tree is still returned;
/// a single weird path no longer aborts the whole scan.
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

    Ok(parse_ls_tree_z(&output.stdout))
}

/// Parse the NUL-delimited stdout of `git ls-tree -r -z`.
///
/// Each entry is `<mode> SP <type> SP <sha> TAB <path>`. Only blob entries
/// are emitted; submodules (`commit`) and trees are filtered out. Paths
/// that are not valid UTF-8 are skipped with a `tracing::warn!` (lossy-
/// decoded into the message) so a single weird path does not abort the
/// whole scan.
fn parse_ls_tree_z(stdout: &[u8]) -> Vec<TreeEntry> {
    let mut out = Vec::new();
    for entry in stdout.split(|&b| b == 0) {
        if entry.is_empty() {
            continue;
        }
        let Some(tab_pos) = entry.iter().position(|&b| b == b'\t') else {
            continue;
        };
        let (meta_bytes, path_bytes_with_tab) = entry.split_at(tab_pos);
        let path_bytes = &path_bytes_with_tab[1..];

        // Meta is "<mode> <type> <sha>" — always ASCII, so utf-8 decoding
        // here is essentially total; we still guard against corruption.
        let Ok(meta) = std::str::from_utf8(meta_bytes) else {
            tracing::warn!(
                meta = %String::from_utf8_lossy(meta_bytes),
                "git ls-tree: non-utf8 in entry meta; skipping",
            );
            continue;
        };
        let parts: Vec<&str> = meta.splitn(3, ' ').collect();
        if parts.len() != 3 {
            continue;
        }
        if parts[1] != "blob" {
            continue; // skip submodules ("commit"), trees, etc.
        }

        let Ok(path_str) = std::str::from_utf8(path_bytes) else {
            tracing::warn!(
                path = %String::from_utf8_lossy(path_bytes),
                "git ls-tree: non-utf8 path; skipping",
            );
            continue;
        };
        let path = path_str.to_owned();
        out.push(TreeEntry {
            path,
            blob_sha: parts[2].to_owned(),
        });
    }
    out
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
    /// Reusable scratch buffer for the per-blob header line. Cleared on
    /// every [`Self::read_blob`] call instead of re-allocating a fresh
    /// `String` per request.
    header_scratch: String,
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
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(repo_root)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        strip_git_env(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| {
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
            header_scratch: String::new(),
        })
    }

    /// Read the blob at `blob_sha` from the persistent batch stream.
    ///
    /// `blob_sha` must be exactly 40 lowercase ASCII hex characters — the
    /// canonical SHA-1 form git emits. The check runs before any byte
    /// reaches git's stdin, so a caller that smuggles a `\n` or any other
    /// `--batch` control sequence into the SHA cannot desync the protocol
    /// or inject extra requests.
    ///
    /// # Errors
    /// Returns `InvalidInput` when `blob_sha` is not 40 lowercase hex chars,
    /// when the blob is missing, when the header is malformed, or when the
    /// child died unexpectedly.
    pub fn read_blob(&mut self, blob_sha: &str) -> Result<Vec<u8>> {
        validate_blob_sha(blob_sha)?;
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

        self.header_scratch.clear();
        let n = self
            .stdout
            .read_line(&mut self.header_scratch)
            .map_err(|e| AstToMermaidError::InvalidInput(format!("git cat-file read: {e}")))?;
        if n == 0 {
            return Err(AstToMermaidError::InvalidInput(
                "git cat-file: unexpected EOF on header".into(),
            ));
        }
        let trimmed = self.header_scratch.trim_end_matches('\n');
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
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    strip_git_env(&mut cmd);
    cmd.output()
        .map_err(|e| AstToMermaidError::InvalidInput(format!("git spawn ({args:?}): {e}")))
}

/// Strip ambient `GIT_*` env vars before invoking a git subprocess.
///
/// `GIT_DIR` / `GIT_INDEX_FILE` / `GIT_WORK_TREE` / `GIT_OBJECT_DIRECTORY`
/// override `-C` / `current_dir` and would silently retarget operations
/// at whatever repository the caller was already inside — most painfully
/// when `a2m` runs from a pre-commit hook (the parent `git commit` exports
/// these), but any nested invocation is at risk.
///
/// The other vars are about *how* git authenticates, configures itself,
/// and resolves objects:
///
/// - `GIT_SSH_COMMAND` / `GIT_ASKPASS` / `SSH_ASKPASS` — credential helpers
///   that could be made to spawn an attacker binary if a ref ever fed back
///   into a fetch path.
/// - `GIT_CONFIG`, `GIT_CONFIG_COUNT`, `GIT_CONFIG_PARAMETERS`,
///   `GIT_CONFIG_GLOBAL`, `GIT_CONFIG_SYSTEM`, `GIT_CONFIG_NOSYSTEM`,
///   plus the indexed `GIT_CONFIG_KEY_<n>` / `GIT_CONFIG_VALUE_<n>` pairs
///   (enumerated by walking `std::env::vars_os` and matching the prefix
///   since `Command::env_remove` doesn't take a wildcard) — these can
///   inject arbitrary git config (`core.sshCommand`, `protocol.ext.allow`,
///   …) into the child.
/// - `GIT_ALTERNATE_OBJECT_DIRECTORIES` — extra object stores git will
///   read from; an attacker who controls this can shadow blobs with their
///   own content.
fn strip_git_env(cmd: &mut Command) {
    for var in [
        "GIT_DIR",
        "GIT_INDEX_FILE",
        "GIT_WORK_TREE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_SSH_COMMAND",
        "GIT_ASKPASS",
        "SSH_ASKPASS",
        "GIT_CONFIG",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_NOSYSTEM",
    ] {
        cmd.env_remove(var);
    }
    // Wildcard-equivalent: the indexed `GIT_CONFIG_KEY_<n>` /
    // `GIT_CONFIG_VALUE_<n>` pairs aren't enumerable by `env_remove`,
    // so collect names from both the ambient process env and any vars
    // the caller already set on the `Command`, and remove every match.
    let is_config_pair = |name: &std::ffi::OsStr| {
        let b = name.as_encoded_bytes();
        b.starts_with(b"GIT_CONFIG_KEY_") || b.starts_with(b"GIT_CONFIG_VALUE_")
    };
    let mut to_remove: Vec<std::ffi::OsString> = Vec::new();
    for (k, _) in std::env::vars_os() {
        if is_config_pair(&k) {
            to_remove.push(k);
        }
    }
    for (k, _) in cmd.get_envs() {
        if is_config_pair(k) {
            to_remove.push(k.to_owned());
        }
    }
    for k in &to_remove {
        cmd.env_remove(k);
    }
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
    fn encoding_edges_parse_ls_tree_skips_non_utf8_paths() {
        // Two entries: one valid ASCII path, one with an invalid UTF-8 byte
        // (0xff) embedded in the path. The second must be silently dropped
        // (with a warn), but the first must survive.
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"100644 blob 1111111111111111111111111111111111111111\tsrc/good.rs");
        buf.push(0);
        buf.extend_from_slice(b"100644 blob 2222222222222222222222222222222222222222\tsrc/b");
        buf.push(0xff); // invalid UTF-8
        buf.extend_from_slice(b"ad.rs");
        buf.push(0);

        let entries = parse_ls_tree_z(&buf);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "src/good.rs");
        assert_eq!(
            entries[0].blob_sha,
            "1111111111111111111111111111111111111111"
        );
    }

    #[test]
    fn encoding_edges_parse_ls_tree_skips_submodules() {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(
            b"160000 commit 3333333333333333333333333333333333333333\tvendor/sub",
        );
        buf.push(0);
        buf.extend_from_slice(b"100644 blob 4444444444444444444444444444444444444444\tsrc/lib.rs");
        buf.push(0);

        let entries = parse_ls_tree_z(&buf);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "src/lib.rs");
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
    fn validate_blob_sha_accepts_canonical_form() {
        assert!(validate_blob_sha("0000000000000000000000000000000000000000").is_ok());
        assert!(validate_blob_sha("abcdef0123456789abcdef0123456789abcdef01").is_ok());
    }

    #[test]
    fn validate_blob_sha_rejects_wrong_length() {
        assert!(validate_blob_sha("").is_err());
        assert!(validate_blob_sha("abc").is_err());
        assert!(validate_blob_sha(&"a".repeat(39)).is_err());
        assert!(validate_blob_sha(&"a".repeat(41)).is_err());
    }

    #[test]
    fn validate_blob_sha_rejects_non_hex() {
        // Uppercase is rejected (git emits lowercase) and so is anything outside 0-9a-f.
        assert!(validate_blob_sha(&"A".repeat(40)).is_err());
        assert!(validate_blob_sha(&"g".repeat(40)).is_err());
        // Embedded newline — the protocol-injection vector this guard exists for.
        let mut s = "a".repeat(39);
        s.push('\n');
        assert!(validate_blob_sha(&s).is_err());
    }

    #[test]
    fn batch_reader_read_blob_rejects_non_hex_before_write() {
        // The reader must refuse a malformed sha *before* writing to git's
        // stdin: anything we send becomes a fresh request line, so a sha
        // containing `\n` would let a caller inject extra requests and
        // desync the response stream.
        let tmp = tempdir().unwrap();
        let (blob, _) = init_repo(tmp.path(), "f.rs", "fn x(){}\n");
        let mut reader = BatchReader::spawn(tmp.path()).unwrap();
        let err = reader.read_blob("not-hex").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("blob sha"),
            "expected blob-sha rejection, got: {msg}"
        );
        // After rejection, the stream must still be usable for a real read.
        let content = reader.read_blob(&blob).unwrap();
        assert_eq!(content, b"fn x(){}\n");
    }

    #[test]
    fn batch_reader_read_blob_rejects_newline_injection() {
        // SHA followed by `\n<another sha>` would have been two requests;
        // validation must catch the newline before any byte hits stdin.
        let tmp = tempdir().unwrap();
        let (blob, _) = init_repo(tmp.path(), "f.rs", "fn x(){}\n");
        let mut reader = BatchReader::spawn(tmp.path()).unwrap();
        let payload = format!("{blob}\n{blob}");
        let err = reader.read_blob(&payload).unwrap_err();
        assert!(
            err.to_string().contains("blob sha"),
            "expected rejection on newline-bearing sha, got: {err}"
        );
        // Stream still functional for the canonical sha.
        let got = reader.read_blob(&blob).unwrap();
        assert_eq!(got, b"fn x(){}\n");
    }

    #[test]
    fn strip_git_env_removes_known_git_vars() {
        // Drive `Command` through `strip_git_env` and observe the resulting
        // child process's environment. We pipe through `/usr/bin/env` (or
        // the closest moral equivalent on this platform) and check that the
        // sensitive `GIT_*` vars set by the parent never make it across.
        let mut cmd = Command::new("/usr/bin/env");
        // Inject every var the strip helper is supposed to remove. We set
        // them via `Command::env` rather than the process env so the test
        // can't poison sibling tests running in parallel.
        for (k, v) in [
            ("GIT_DIR", "/tmp/leak-git-dir"),
            ("GIT_INDEX_FILE", "/tmp/leak-git-index"),
            ("GIT_WORK_TREE", "/tmp/leak-git-worktree"),
            ("GIT_OBJECT_DIRECTORY", "/tmp/leak-git-objects"),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES", "/tmp/leak-git-alt"),
            ("GIT_SSH_COMMAND", "/tmp/leak-ssh-evil"),
            ("GIT_ASKPASS", "/tmp/leak-askpass"),
            ("SSH_ASKPASS", "/tmp/leak-ssh-askpass"),
            ("GIT_CONFIG", "/tmp/leak-git-config"),
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "core.sshCommand"),
            ("GIT_CONFIG_VALUE_0", "/tmp/leak-key-value"),
            ("GIT_CONFIG_PARAMETERS", "leak-config-params"),
            ("GIT_CONFIG_GLOBAL", "/tmp/leak-config-global"),
            ("GIT_CONFIG_SYSTEM", "/tmp/leak-config-system"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
        ] {
            cmd.env(k, v);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        strip_git_env(&mut cmd);
        let Ok(out) = cmd.output() else {
            // /usr/bin/env may be missing — best-effort test.
            return;
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        for marker in [
            "leak-git-dir",
            "leak-git-index",
            "leak-git-worktree",
            "leak-git-objects",
            "leak-git-alt",
            "leak-ssh-evil",
            "leak-askpass",
            "leak-ssh-askpass",
            "leak-git-config",
            "leak-key-value",
            "leak-config-params",
            "leak-config-global",
            "leak-config-system",
        ] {
            assert!(
                !stdout.contains(marker),
                "marker {marker:?} survived strip_git_env; child env was:\n{stdout}",
            );
        }
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
