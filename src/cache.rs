//! Two-tier content-addressed cache for parsed atoms and materialized bundles.
//!
//! Layout (typically rooted at `<repo>/.a2m/cache/`):
//!
//! ```text
//! <root>/
//! ├── version                   # schema + grammar + a2m versions
//! ├── blobs/
//! │   └── <git_blob_sha>.cbor   # serialized Vec<CodeAtom> for one file blob
//! └── refs/
//!     └── <commit_sha>/         # one materialized bundle per ref
//!         ├── overview.mmd
//!         ├── index.json
//!         └── entities/
//! ```
//!
//! On open, if the version file doesn't match the current expected version,
//! both `blobs/` and `refs/` are wiped — coarse but correct, and matches
//! the design's chosen invalidation strategy.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{AstToMermaidError, Result};
use crate::model::CodeAtom;

/// Cache schema version. Bump when the on-disk layout changes incompatibly.
const SCHEMA_VERSION: u32 = 1;

/// Grammar version. Bump when `tree-sitter-rust` or `tree-sitter-python`
/// is updated in `Cargo.toml`. Forces a cold reparse on next run.
const GRAMMAR_VERSION: u32 = 1;

/// Compute the cache version string written to `<root>/version`.
fn cache_version() -> String {
    format!(
        "schema={SCHEMA_VERSION};grammar={GRAMMAR_VERSION};a2m={}",
        env!("CARGO_PKG_VERSION")
    )
}

/// Two-tier content-addressed cache rooted at a directory.
///
/// Construct via [`Cache::open`]. All filesystem state is created lazily.
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    /// Open or create a cache at `root`. Wipes `blobs/` and `refs/` if the
    /// version file doesn't match the current expected version.
    ///
    /// # Errors
    /// Propagates I/O errors from directory creation, file read, or wipe.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        fs::create_dir_all(root.join("blobs"))?;
        fs::create_dir_all(root.join("refs"))?;

        let version_path = root.join("version");
        let want = cache_version();
        let stale = match fs::read_to_string(&version_path) {
            Ok(have) => have.trim() != want,
            Err(_) => true,
        };
        if stale {
            for sub in ["blobs", "refs"] {
                let p = root.join(sub);
                if p.exists() {
                    fs::remove_dir_all(&p)?;
                    fs::create_dir_all(&p)?;
                }
            }
            fs::write(&version_path, &want)?;
        }

        Ok(Self { root })
    }

    /// Default cache root inside a repo: `<repo_root>/.a2m/cache`.
    #[must_use]
    pub fn default_root(repo_root: &Path) -> PathBuf {
        repo_root.join(".a2m").join("cache")
    }

    /// Auto-create `.a2m/.gitignore` next to the cache parent dir if it
    /// doesn't already exist. Mirrors pytest's auto-managed
    /// `.pytest_cache/.gitignore` (pytest-dev/pytest#3982). Idempotent —
    /// never overwrites an existing file.
    ///
    /// # Errors
    /// Propagates I/O errors from `fs::write`.
    pub fn ensure_gitignore(&self) -> Result<()> {
        let Some(parent) = self.root.parent() else {
            return Ok(());
        };
        let gi = parent.join(".gitignore");
        if gi.exists() {
            return Ok(());
        }
        fs::create_dir_all(parent)?;
        fs::write(&gi, "# created by a2m\n*\n")?;
        Ok(())
    }

    /// Path to the cache root (for diagnostics and `--cache-dir` echo).
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn blob_path(&self, blob_sha: &str) -> PathBuf {
        self.root.join("blobs").join(format!("{blob_sha}.cbor"))
    }

    /// Read cached atoms for a blob, if present and parseable.
    ///
    /// Returns `None` on any miss (file absent or deserialize failure) — the
    /// caller re-parses on miss; corrupt entries don't escalate to errors.
    #[must_use]
    pub fn get_atoms(&self, blob_sha: &str) -> Option<Vec<CodeAtom>> {
        let path = self.blob_path(blob_sha);
        let bytes = fs::read(&path).ok()?;
        ciborium::de::from_reader(&bytes[..]).ok()
    }

    /// Write atoms for a blob to the cache.
    ///
    /// # Errors
    /// CBOR serialization failures or filesystem write errors.
    pub fn put_atoms(&self, blob_sha: &str, atoms: &[CodeAtom]) -> Result<()> {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(atoms, &mut buf)
            .map_err(|e| AstToMermaidError::InvalidInput(format!("cbor serialize: {e}")))?;
        fs::write(self.blob_path(blob_sha), buf)?;
        Ok(())
    }

    /// Bundle directory for a `commit_sha` (or synthetic `wt-<digest>`).
    /// Always returns the path even if the bundle doesn't exist.
    #[must_use]
    pub fn bundle_dir(&self, commit_sha: &str) -> PathBuf {
        self.root.join("refs").join(commit_sha)
    }

    /// Whether a bundle for `commit_sha` already exists on disk.
    #[must_use]
    pub fn has_bundle(&self, commit_sha: &str) -> bool {
        self.bundle_dir(commit_sha).join("index.json").is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EntityId;
    use tempfile::tempdir;

    fn dummy_atom(id: &str) -> CodeAtom {
        CodeAtom {
            id: EntityId::new(id.to_owned()),
            kind: "function".to_owned(),
            name: "x".to_owned(),
            file_path: "src/foo.rs".to_owned(),
            line_start: 1,
            line_end: 5,
            doc: String::new(),
            signature: String::new(),
            content_hash: "abcdef".to_owned(),
            calls: vec![],
        }
    }

    #[test]
    fn open_creates_dirs_and_version() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("cache");
        let _cache = Cache::open(&root).unwrap();
        assert!(root.join("blobs").is_dir());
        assert!(root.join("refs").is_dir());
        assert!(root.join("version").is_file());
    }

    #[test]
    fn put_then_get_atoms_roundtrips() {
        let tmp = tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("c")).unwrap();
        let atoms = vec![dummy_atom("code:foo.rs::function::x")];
        cache.put_atoms("abc123", &atoms).unwrap();
        let got = cache.get_atoms("abc123").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, atoms[0].id);
    }

    #[test]
    fn get_atoms_returns_none_on_miss() {
        let tmp = tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("c")).unwrap();
        assert!(cache.get_atoms("nonexistent").is_none());
    }

    #[test]
    fn version_mismatch_wipes_blobs() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("c");
        {
            let cache = Cache::open(&root).unwrap();
            cache.put_atoms("abc", &[dummy_atom("code:x")]).unwrap();
            assert!(cache.get_atoms("abc").is_some());
        }
        // Tamper with version.
        fs::write(root.join("version"), "stale").unwrap();
        let cache = Cache::open(&root).unwrap();
        assert!(cache.get_atoms("abc").is_none());
    }

    #[test]
    fn ensure_gitignore_creates_file_with_wildcard() {
        let tmp = tempdir().unwrap();
        let parent = tmp.path().join(".a2m");
        let cache = Cache::open(parent.join("cache")).unwrap();
        cache.ensure_gitignore().unwrap();
        let gi = fs::read_to_string(parent.join(".gitignore")).unwrap();
        assert!(gi.contains('*'));
    }

    #[test]
    fn ensure_gitignore_does_not_overwrite_existing() {
        let tmp = tempdir().unwrap();
        let parent = tmp.path().join(".a2m");
        fs::create_dir_all(&parent).unwrap();
        fs::write(parent.join(".gitignore"), "user-content\n").unwrap();
        let cache = Cache::open(parent.join("cache")).unwrap();
        cache.ensure_gitignore().unwrap();
        let gi = fs::read_to_string(parent.join(".gitignore")).unwrap();
        assert_eq!(gi, "user-content\n");
    }

    #[test]
    fn has_bundle_returns_false_until_index_written() {
        let tmp = tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("c")).unwrap();
        assert!(!cache.has_bundle("deadbeef"));
        let bdir = cache.bundle_dir("deadbeef");
        fs::create_dir_all(&bdir).unwrap();
        assert!(!cache.has_bundle("deadbeef"));
        fs::write(bdir.join("index.json"), "{}").unwrap();
        assert!(cache.has_bundle("deadbeef"));
    }

    #[test]
    fn default_root_is_dot_a2m_cache() {
        let p = Path::new("/tmp/repo");
        assert_eq!(Cache::default_root(p), Path::new("/tmp/repo/.a2m/cache"));
    }
}
