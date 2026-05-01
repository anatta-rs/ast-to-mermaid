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

    /// Run garbage collection over the cache.
    ///
    /// Removes entries whose mtime is older than `older_than` (if set), then
    /// evicts oldest-first until total size is below `max_size_bytes`. Both
    /// blobs and bundles are eligible. With `dry_run`, computes what would
    /// be removed without touching the filesystem.
    ///
    /// Eviction is by *write time*, not read time — V1.5 doesn't touch
    /// mtime on cache hits. Document as "evicts oldest-written".
    ///
    /// # Errors
    /// Propagates filesystem read or remove errors.
    pub fn gc(&self, opts: &GcOptions) -> Result<GcReport> {
        let mut entries: Vec<GcEntry> = Vec::new();
        collect_gc_entries(&self.root.join("blobs"), GcKind::Blob, &mut entries)?;
        collect_gc_entries(&self.root.join("refs"), GcKind::Bundle, &mut entries)?;

        let now = std::time::SystemTime::now();
        let total_before: u64 = entries.iter().map(|e| e.size).sum();
        let count_before = entries.len();

        let mut to_remove: Vec<&GcEntry> = Vec::new();
        if let Some(older_than) = opts.older_than {
            for e in &entries {
                if let Ok(age) = now.duration_since(e.mtime)
                    && age > older_than
                {
                    to_remove.push(e);
                }
            }
        }

        // Then size-cap: keep oldest-eviction order, only walk entries not
        // already marked for removal.
        let mut sorted: Vec<&GcEntry> = entries
            .iter()
            .filter(|e| !to_remove.iter().any(|r| r.path == e.path))
            .collect();
        sorted.sort_by_key(|e| e.mtime);
        let mut kept_size: u64 = sorted.iter().map(|e| e.size).sum();
        if let Some(cap) = opts.max_size_bytes {
            for e in &sorted {
                if kept_size <= cap {
                    break;
                }
                kept_size = kept_size.saturating_sub(e.size);
                to_remove.push(e);
            }
        }

        let removed_size: u64 = to_remove.iter().map(|e| e.size).sum();
        let removed_count = to_remove.len();

        if !opts.dry_run {
            for e in &to_remove {
                if e.path.is_dir() {
                    std::fs::remove_dir_all(&e.path)?;
                } else {
                    std::fs::remove_file(&e.path)?;
                }
            }
        }

        Ok(GcReport {
            total_before,
            removed_count,
            removed_size,
            count_before,
            dry_run: opts.dry_run,
        })
    }
}

/// Options controlling [`Cache::gc`].
#[derive(Debug, Clone, Default)]
pub struct GcOptions {
    /// Soft cap in bytes. After eviction, total size will be ≤ this.
    /// `None` = no size cap.
    pub max_size_bytes: Option<u64>,
    /// Evict entries older than this duration. `None` = no age filter.
    pub older_than: Option<std::time::Duration>,
    /// Compute what would be removed without touching the filesystem.
    pub dry_run: bool,
}

/// Summary of a [`Cache::gc`] run.
#[derive(Debug, Clone)]
pub struct GcReport {
    /// Total bytes used by cache entries before eviction.
    pub total_before: u64,
    /// Number of entries removed (or that would be, with `dry_run`).
    pub removed_count: usize,
    /// Bytes freed (or that would be, with `dry_run`).
    pub removed_size: u64,
    /// Entry count before eviction.
    pub count_before: usize,
    /// Whether this run was a dry run.
    pub dry_run: bool,
}

#[derive(Debug)]
struct GcEntry {
    path: PathBuf,
    mtime: std::time::SystemTime,
    size: u64,
}

#[derive(Debug, Clone, Copy)]
enum GcKind {
    Blob,
    Bundle,
}

fn collect_gc_entries(dir: &Path, kind: GcKind, out: &mut Vec<GcEntry>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let meta = entry.metadata()?;
        let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let size = match kind {
            GcKind::Blob => meta.len(),
            GcKind::Bundle if meta.is_dir() => dir_size_recursive(&path)?,
            _ => meta.len(),
        };
        out.push(GcEntry { path, mtime, size });
    }
    Ok(())
}

fn dir_size_recursive(dir: &Path) -> Result<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_dir() {
            total += dir_size_recursive(&entry.path())?;
        } else {
            total += meta.len();
        }
    }
    Ok(total)
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

    #[test]
    fn gc_size_cap_evicts_oldest_first() {
        let tmp = tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("c")).unwrap();
        // Three blobs, each ~ same size; touch in order to set mtime ordering.
        for sha in &["a", "b", "c"] {
            cache
                .put_atoms(sha, &[dummy_atom(&format!("code:{sha}"))])
                .unwrap();
            // Sleep a hair so mtimes differ on filesystems with coarse mtime.
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Cap at half of total → should remove oldest 2.
        let total: u64 = ["a", "b", "c"]
            .iter()
            .map(|s| {
                std::fs::metadata(cache.root().join("blobs").join(format!("{s}.cbor")))
                    .unwrap()
                    .len()
            })
            .sum();
        let report = cache
            .gc(&GcOptions {
                max_size_bytes: Some(total / 3 + 1),
                older_than: None,
                dry_run: false,
            })
            .unwrap();
        assert!(report.removed_count >= 1, "removed: {report:?}");
        // The most-recently-written one should still be there.
        assert!(cache.get_atoms("c").is_some());
    }

    #[test]
    fn gc_dry_run_touches_nothing() {
        let tmp = tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("c")).unwrap();
        cache.put_atoms("x", &[dummy_atom("code:x")]).unwrap();
        let report = cache
            .gc(&GcOptions {
                max_size_bytes: Some(0), // would normally evict everything
                older_than: None,
                dry_run: true,
            })
            .unwrap();
        assert!(report.dry_run);
        assert_eq!(report.removed_count, 1);
        // File still on disk.
        assert!(cache.get_atoms("x").is_some());
    }

    #[test]
    fn gc_empty_cache_is_a_noop() {
        let tmp = tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("c")).unwrap();
        let r = cache.gc(&GcOptions::default()).unwrap();
        assert_eq!(r.removed_count, 0);
        assert_eq!(r.total_before, 0);
    }
}
