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
//!
//! ## Atomic-write tmp file naming (C26)
//!
//! [`atomic_write`] writes to a sibling tmp file then `rename`s into place.
//! Under v0.6.0's rayon parallelism, two workers persisting byte-identical
//! files (e.g. empty `mod.rs`, `__init__.py`) collide on the same
//! `blob_sha`, and a pid-only suffix produces identical tmp paths across
//! threads — the second worker's `fs::write` corrupts the first's bytes
//! mid-rename.
//!
//! Tmp suffix shape:
//!
//! ```text
//! .{file_name}.tmp.{pid}.{thread_id}.{counter}
//! ```
//!
//! - `pid` = `std::process::id()` (cross-process disambiguation).
//! - `thread_id` = u64 hash of `std::thread::current().id()`
//!   (intra-process per-thread disambiguation; `ThreadId::as_u64` is still
//!   unstable, so we hash to extract a stable u64 with the same effect).
//! - `counter` = per-process `AtomicU64::fetch_add(1)` (disambiguates
//!   sequential writes from the same thread targeting the same path).
//!
//! [`Cache::open`] sweeps any leftover `blobs/.*.tmp.*` files (any
//! suffix shape) on startup so a crashed prior run can't accumulate
//! garbage in the blob dir.

use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::error::{AstToMermaidError, Result};
use crate::parser::ParseUnit;

/// Magic bytes for `<sha>.cbor` blob files. Detects garbage on read.
const BLOB_MAGIC: u32 = 0xa2_a2_b1_0b;

/// Schema version embedded in each blob envelope. Bump when the layout
/// of `BlobEnvelope` changes; mismatched files are treated as cache miss.
/// V2 (this) carries `ParseUnit` (atoms + intra-file edges) instead of just atoms.
const BLOB_ENVELOPE_VERSION: u32 = 2;

/// On-disk envelope wrapping a cached parse unit. Mismatched magic or
/// version on read returns `None` from `get_unit` (treated as cache miss).
///
/// `unit` is `Cow<'a, ParseUnit>` so `put_unit` can serialize from a
/// borrow (`Cow::Borrowed`) without cloning the full unit, while
/// `get_unit` still deserializes into an owned `ParseUnit`.
#[derive(Serialize, Deserialize)]
struct BlobEnvelope<'a> {
    magic: u32,
    version: u32,
    unit: Cow<'a, ParseUnit>,
}

/// Cache schema version. Bump when the on-disk layout changes incompatibly.
///
/// V2: `CodeAtom` gained a `parent: Option<String>` field. Old cache
/// entries deserialize methods with `parent = None`, which would defeat
/// the qualified-method-only resolver — bumping forces a cold reparse so
/// every method is re-emitted with its real owner type.
///
/// V3: `CodeAtom` gained a `method_calls: Vec<String>` field; the
/// parser now routes receiver-style captures (`obj.method()`) there
/// instead of `calls`. Without a fresh reparse, a v2 unit's `calls`
/// list still includes those captures and the resolver would happily
/// ghost-bind them to free fns of the same name.
///
/// V4: resolver-only change — cross-language matches (`.py` ↔ `.rs`)
/// are now rejected. Cache layout unchanged but bumping forces a
/// re-resolve so old `Calls` edges between Python and Rust files are
/// not silently kept.
///
/// V5: resolver-only change — bare-name calls no longer fall back to
/// a unique cross-crate candidate (closure-parameter shadowing was
/// producing ghosts), and `Self::`/`crate::`/`self::`/`super::` no
/// longer fall through to bare-name resolution after the qualified
/// path fails. Bump forces stale ghost edges to be dropped on next
/// resolve.
///
/// V6: resolver-only change — bare-name calls that have *already*
/// been resolved by the parser's intra-file linker no longer get a
/// second cross-file edge added by the resolver. Closes the "twin
/// files share a private helper name" cycle (`l2_sq` in voronoi tree
/// pairs, `build_request_url` in dork's google/duckduckgo backends,
/// `find_atom` shared across the ingester parse-test files, etc.).
const SCHEMA_VERSION: u32 = 6;

/// Grammar version. Forces a cold reparse on next run.
///
/// Bump on **either** of:
/// - a `tree-sitter-*` dependency changing in `Cargo.toml` (including a
///   new language);
/// - the extractors themselves changing what they emit for unchanged
///   source — new node kinds handled, a receiver classified differently,
///   a call site that used to be dropped now surfacing.
///
/// The second case is the one that bites. Blobs are keyed by content, so
/// an unchanged file is served from cache no matter how much the parser
/// improved underneath. `a2m={CARGO_PKG_VERSION}` in the version string
/// papers over it for released builds, but not during development, where
/// the version is pinned across many commits — exactly when the
/// extractors move most.
///
/// `2`: Dart support (#170). Added `tree-sitter-dart`, and across #171 to
/// #175 changed what the parser emits for Rust and Python files too
/// (`declared_name` is shared by all three).
///
/// `3`: Dart typed receivers (#184). `ClassName.method()` now emits a
/// qualified `ClassName::method` instead of a bare method name, and
/// `null_aware_member_expression` stopped being dropped — the same file
/// therefore yields different `calls` than it did under `2`.
///
/// `4`: Dart top-level functions (#186). Their calls were never extracted
/// at all, so every cached `ParseUnit` for a `.dart` file holding free
/// functions carries an empty `calls` list. Without this bump the fix is
/// invisible on any tree that has been indexed before — which is how it
/// was found: a project with a warm `.a2m` still showed `main()` with no
/// callees while a fresh copy of the same source showed four.
///
/// `5`: Dart receiver-type inference (#188). `obj.method()` now emits
/// `Type::method` whenever the file declares `obj`'s type, so cached
/// units carry method names where qualified calls are now produced.
///
/// `6`: call sites carry rank and control-flow flags (#190). Cached units
/// hold bare strings and would deserialise without the new fields.
const GRAMMAR_VERSION: u32 = 6;

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

        // Reject if a hostile checkout has planted symlinks anywhere inside
        // the cache: subsequent destructive ops (`fs::remove_dir_all`) would
        // follow them out of the workspace.
        walk_safe(&root)?;

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
                    remove_dir_all_safe(&p)?;
                    fs::create_dir_all(&p)?;
                }
            }
            fs::write(&version_path, &want)?;
        }

        sweep_stale_tmp_blobs(&root.join("blobs"))?;

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

    /// Read cached `ParseUnit` for a blob, if present and parseable.
    ///
    /// Returns `None` on any miss (file absent, deserialize failure, or
    /// magic/version mismatch) — the caller re-parses on miss; corrupt or
    /// stale-format entries don't escalate to errors.
    #[must_use]
    pub fn get_unit(&self, blob_sha: &str) -> Option<ParseUnit> {
        let path = self.blob_path(blob_sha);
        let bytes = fs::read(&path).ok()?;
        let env: BlobEnvelope<'_> = ciborium::de::from_reader(&bytes[..]).ok()?;
        if env.magic != BLOB_MAGIC || env.version != BLOB_ENVELOPE_VERSION {
            return None;
        }
        Some(env.unit.into_owned())
    }

    /// Write a `ParseUnit` for a blob to the cache. Uses write-tmp + atomic
    /// rename so concurrent runs cannot observe a partially-written file.
    ///
    /// # Errors
    /// CBOR serialization failures or filesystem write errors.
    pub fn put_unit(&self, blob_sha: &str, unit: &ParseUnit) -> Result<()> {
        let env = BlobEnvelope {
            magic: BLOB_MAGIC,
            version: BLOB_ENVELOPE_VERSION,
            unit: Cow::Borrowed(unit),
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&env, &mut buf)
            .map_err(|e| AstToMermaidError::InvalidInput(format!("cbor serialize: {e}")))?;
        let final_path = self.blob_path(blob_sha);
        atomic_write(&final_path, &buf)?;
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
        gc_at(&self.root, opts)
    }
}

/// Free-function form of [`Cache::gc`], operating directly on a cache
/// root path. Shared with [`maybe_auto_gc`] so the auto path doesn't
/// have to re-`Cache::open` (and re-walk the tree for the symlink
/// audit) on every bundle write.
///
/// Per-path I/O failures during eviction (disk-full mid-`remove_file`,
/// permission flip, file vanished concurrently) populate
/// [`GcReport::errors`] instead of bubbling out as `Err`, so the caller
/// can still observe what was freed before the failure. Symlink-refusal
/// remains a hard error — a planted symlink is a security signal, not a
/// transient condition the caller should paper over.
fn gc_at(root: &Path, opts: &GcOptions) -> Result<GcReport> {
    let mut entries: Vec<GcEntry> = Vec::new();
    collect_gc_entries(&root.join("blobs"), GcKind::Blob, &mut entries)?;
    collect_gc_entries(&root.join("refs"), GcKind::Bundle, &mut entries)?;

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

    // Belt-and-suspenders: even though the size pass already filters out
    // age-flagged paths, an explicit dedup by path means the remove loop
    // can never double-fire on a single path and have the second
    // `fs::remove_file` surface its `NotFound` as an I/O error.
    to_remove.sort_by(|a, b| a.path.cmp(&b.path));
    to_remove.dedup_by(|a, b| a.path == b.path);

    let planned_size: u64 = to_remove.iter().map(|e| e.size).sum();
    let planned_count = to_remove.len();

    let mut removed_size: u64 = 0;
    let mut removed_count: usize = 0;
    let mut errors: Vec<(PathBuf, std::io::Error)> = Vec::new();

    if opts.dry_run {
        removed_size = planned_size;
        removed_count = planned_count;
    } else {
        for e in &to_remove {
            let meta = match fs::symlink_metadata(&e.path) {
                Ok(m) => m,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    errors.push((e.path.clone(), err));
                    continue;
                }
            };
            if meta.file_type().is_symlink() {
                return Err(AstToMermaidError::InvalidInput(format!(
                    "refusing to remove symlink in cache: {}",
                    e.path.display()
                )));
            }
            let res = if meta.is_dir() {
                remove_dir_all_safe(&e.path)
            } else {
                fs::remove_file(&e.path).map_err(AstToMermaidError::from)
            };
            match res {
                Ok(()) => {
                    removed_size += e.size;
                    removed_count += 1;
                }
                Err(AstToMermaidError::Io(io_err)) => {
                    errors.push((e.path.clone(), io_err));
                }
                Err(other) => return Err(other),
            }
        }
    }

    Ok(GcReport {
        total_before,
        removed_count,
        removed_size,
        count_before,
        dry_run: opts.dry_run,
        errors,
    })
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

/// High-water-mark policy for auto-triggered cache GC.
///
/// Both caps are optional; `None` disables that dimension. Defaults are
/// 1 GiB total / 30-day age. Policy is consulted by [`maybe_auto_gc`]
/// after each [`write_bundle_atomic`]; if any cap is breached, an
/// iterative LRU eviction runs until the cache is back within bounds.
///
/// Use [`GcPolicy::from_env`] to honour the operator-facing env knobs:
/// - `A2M_AUTO_GC=0` → return `None` (auto-gc disabled entirely)
/// - `A2M_AUTO_GC_MAX_BYTES=<u64>` → override `max_total_bytes`
/// - `A2M_AUTO_GC_MAX_AGE_SECS=<u64>` → override `max_age_secs`
#[derive(Debug, Clone)]
pub struct GcPolicy {
    /// Total cache footprint cap (bytes). `None` disables the size cap.
    pub max_total_bytes: Option<u64>,
    /// Per-entry age cap (seconds). `None` disables the age filter.
    pub max_age_secs: Option<u64>,
}

impl Default for GcPolicy {
    fn default() -> Self {
        Self {
            // 1 GiB — conservative single-repo default.
            max_total_bytes: Some(1024 * 1024 * 1024),
            // 30 days.
            max_age_secs: Some(30 * 24 * 3600),
        }
    }
}

impl GcPolicy {
    /// Read policy from `A2M_AUTO_GC_*` env. Returns `None` when
    /// `A2M_AUTO_GC=0` (operator opt-out — `maybe_auto_gc` becomes a
    /// no-op). Otherwise returns a `GcPolicy` with defaults overridden
    /// by `A2M_AUTO_GC_MAX_BYTES` / `A2M_AUTO_GC_MAX_AGE_SECS` when
    /// those parse as `u64`.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        if std::env::var("A2M_AUTO_GC").as_deref() == Ok("0") {
            return None;
        }
        let mut p = Self::default();
        if let Ok(v) = std::env::var("A2M_AUTO_GC_MAX_BYTES")
            && let Ok(n) = v.parse::<u64>()
        {
            p.max_total_bytes = Some(n);
        }
        if let Ok(v) = std::env::var("A2M_AUTO_GC_MAX_AGE_SECS")
            && let Ok(n) = v.parse::<u64>()
        {
            p.max_age_secs = Some(n);
        }
        Some(p)
    }
}

impl From<&GcPolicy> for GcOptions {
    fn from(p: &GcPolicy) -> Self {
        Self {
            max_size_bytes: p.max_total_bytes,
            older_than: p.max_age_secs.map(std::time::Duration::from_secs),
            dry_run: false,
        }
    }
}

/// Summary of a [`Cache::gc`] run.
///
/// Partial-success: a remove that fails mid-iteration with a transient
/// I/O error (disk-full, permission flip, racing manual `rm`) is
/// recorded in [`GcReport::errors`] and the loop continues. Callers
/// that need strict semantics can check `errors.is_empty()`.
///
/// Not `Clone` because [`std::io::Error`] isn't.
#[derive(Debug)]
pub struct GcReport {
    /// Total bytes used by cache entries before eviction.
    pub total_before: u64,
    /// Number of entries removed (or that would be, with `dry_run`).
    /// Excludes entries that hit a per-path I/O error in `errors`.
    pub removed_count: usize,
    /// Bytes freed (or that would be, with `dry_run`). Sums only the
    /// sizes of entries that were actually unlinked.
    pub removed_size: u64,
    /// Entry count before eviction.
    pub count_before: usize,
    /// Whether this run was a dry run.
    pub dry_run: bool,
    /// Per-path I/O errors encountered during eviction. Empty on a
    /// fully successful run. `dry_run` runs never populate this.
    pub errors: Vec<(PathBuf, std::io::Error)>,
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

/// Process-wide counter appended to every [`atomic_write`] tmp suffix.
/// Combined with pid + thread id, guarantees no two concurrent writers
/// pick the same tmp path even when targeting the same final path.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Hash `std::thread::current().id()` to a u64. Used as a stable
/// substitute for the unstable [`std::thread::ThreadId::as_u64`] in
/// [`atomic_write`]'s tmp suffix — only the per-thread distinctness
/// matters, not the exact numeric value.
fn current_thread_id_u64() -> u64 {
    use std::hash::BuildHasher;
    std::collections::hash_map::RandomState::new().hash_one(std::thread::current().id())
}

/// Write `bytes` to `path` atomically: a temp sibling is written and
/// `rename`'d into place. Concurrent readers either see the old content
/// or the new content — never partial bytes.
///
/// Tmp suffix is `.{file_name}.tmp.{pid}.{thread_id}.{counter}` so two
/// workers targeting the same final path (rayon's `parse_phase` racing
/// on byte-identical blobs) cannot collide on the tmp file. See the
/// module header for the full naming contract.
///
/// # Errors
/// Propagates I/O errors. If `rename` fails, the temp file is left on
/// disk for diagnosis (caller can retry or `gc`).
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        AstToMermaidError::InvalidInput(format!(
            "atomic_write: path has no parent: {}",
            path.display()
        ))
    })?;
    fs::create_dir_all(parent)?;
    let pid = std::process::id();
    let tid = current_thread_id_u64();
    let counter = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let tmp = parent.join(format!(".{stem}.tmp.{pid}.{tid}.{counter}"));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Crash-recovery sweep: remove any leftover [`atomic_write`] tmp files
/// from `blobs_dir`. Matches files whose name starts with `.` and
/// contains `.tmp.` — covers every suffix shape this module has ever
/// produced (pid-only, pid+tid+counter, future variants).
///
/// Silent: a stale tmp is expected after a crash, not an error.
/// Symlinks are refused (consistent with the rest of the module).
fn sweep_stale_tmp_blobs(blobs_dir: &Path) -> Result<()> {
    let read = match fs::read_dir(blobs_dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in read {
        let entry = entry?;
        let name = entry.file_name();
        let Some(s) = name.to_str() else { continue };
        if !s.starts_with('.') || !s.contains(".tmp.") {
            continue;
        }
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            return Err(AstToMermaidError::InvalidInput(format!(
                "refusing to remove symlinked tmp in cache: {}",
                path.display()
            )));
        }
        if meta.is_dir() {
            remove_dir_all_safe(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Atomically rename `from` (existing dir or file) to `to`. Replaces any
/// existing file at `to`; for directories, requires `to` to not exist
/// (Unix `rename` semantics over a non-empty target are platform-defined).
///
/// # Errors
/// Propagates I/O errors.
pub fn atomic_rename(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(from, to)?;
    Ok(())
}

fn collect_gc_entries(dir: &Path, kind: GcKind, out: &mut Vec<GcEntry>) -> Result<()> {
    let dir_meta = match fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if dir_meta.file_type().is_symlink() {
        return Err(AstToMermaidError::InvalidInput(format!(
            "refusing to enumerate symlinked cache directory: {}",
            dir.display()
        )));
    }
    if !dir_meta.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            return Err(AstToMermaidError::InvalidInput(format!(
                "refusing to follow symlink in cache: {}",
                path.display()
            )));
        }
        let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let size = match kind {
            GcKind::Bundle if meta.is_dir() => dir_size_recursive(&path)?,
            GcKind::Blob | GcKind::Bundle => meta.len(),
        };
        out.push(GcEntry { path, mtime, size });
    }
    Ok(())
}

fn dir_size_recursive(dir: &Path) -> Result<u64> {
    // Delegated to `walk_safe` (C13): every entry is verified
    // non-symlink before recursion, so a planted `a -> b -> a` loop
    // surfaces as `InvalidInput` on the first hop instead of blowing
    // the stack. Sums regular-file lengths only; directory inode
    // sizes are intentionally ignored to match `du -sb` semantics.
    let entries = walk_safe(dir)?;
    Ok(entries
        .iter()
        .filter(|(_, m)| m.is_file())
        .map(|(_, m)| m.len())
        .sum())
}

/// Total bytes used by the two cache subtrees (`blobs/` and `refs/`).
/// Returns 0 if either is missing. Errors propagate (e.g. planted
/// symlink → [`AstToMermaidError::InvalidInput`]).
fn cache_total_size(cache_root: &Path) -> Result<u64> {
    let mut total = 0;
    for sub in ["blobs", "refs"] {
        let p = cache_root.join(sub);
        if p.exists() {
            total += dir_size_recursive(&p)?;
        }
    }
    Ok(total)
}

/// High-water-mark check after a bundle write: if the cache exceeds
/// `policy.max_total_bytes` (or has any age cap set), runs an
/// iterative LRU eviction via [`Cache::gc`] until the cache fits.
/// Returns `Ok(None)` when no GC was needed, `Ok(Some(report))` when
/// it ran. Called from [`write_bundle_atomic`] post-rename.
///
/// Distinct from explicit `Cache::gc`: this is the *automatic* path
/// — operator opt-out lives in [`GcPolicy::from_env`] (`A2M_AUTO_GC=0`).
///
/// # Errors
/// Propagates filesystem errors and the symlink-refusal contract from
/// the underlying [`dir_size_recursive`] / [`Cache::gc`].
pub fn maybe_auto_gc(cache_root: &Path, policy: &GcPolicy) -> Result<Option<GcReport>> {
    let total = cache_total_size(cache_root)?;
    let over_size = policy.max_total_bytes.is_some_and(|cap| total > cap);
    let has_age = policy.max_age_secs.is_some();
    if !over_size && !has_age {
        return Ok(None);
    }
    let report = gc_at(cache_root, &GcOptions::from(policy))?;
    Ok(Some(report))
}

/// Recursively walk `root` without following symlinks. Visits every regular
/// file and directory in the subtree (including `root` itself) and returns
/// `(path, metadata)` pairs in pre-order. If any symlink is encountered at
/// `root` or any descendant, returns
/// [`AstToMermaidError::InvalidInput`] without yielding partial results.
///
/// Used as the read-side counterpart to [`remove_dir_all_safe`] when the
/// cache code needs to traverse a tree it cannot trust to be free of
/// attacker-planted symlinks.
///
/// # Errors
/// Propagates I/O errors, and returns `InvalidInput` when a symlink is
/// found anywhere in the tree.
pub fn walk_safe(root: &Path) -> Result<Vec<(PathBuf, fs::Metadata)>> {
    let mut out = Vec::new();
    walk_safe_into(root, &mut out)?;
    Ok(out)
}

fn walk_safe_into(path: &Path, out: &mut Vec<(PathBuf, fs::Metadata)>) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(AstToMermaidError::InvalidInput(format!(
            "refusing to follow symlink in cache: {}",
            path.display()
        )));
    }
    let is_dir = meta.is_dir();
    out.push((path.to_path_buf(), meta));
    if is_dir {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            walk_safe_into(&entry.path(), out)?;
        }
    }
    Ok(())
}

/// Recursively remove `path`, refusing to follow symlinks at any level.
///
/// Unlike [`std::fs::remove_dir_all`], which dereferences symlinked
/// directories and deletes their targets, this helper checks every entry
/// with [`fs::symlink_metadata`] and bails out with
/// [`AstToMermaidError::InvalidInput`] on the first symlink found. The
/// removal itself walks the tree manually using `remove_file` for
/// non-directories and `remove_dir` after children are gone, so an
/// attacker who plants a symlink mid-traversal cannot trick the helper
/// into deleting outside the cache.
///
/// # Errors
/// Propagates I/O errors, and returns `InvalidInput` when a symlink is
/// found anywhere in the tree (including at `path` itself).
pub fn remove_dir_all_safe(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(AstToMermaidError::InvalidInput(format!(
            "refusing to remove symlink: {}",
            path.display()
        )));
    }
    if !meta.is_dir() {
        return Err(AstToMermaidError::InvalidInput(format!(
            "remove_dir_all_safe: not a directory: {}",
            path.display()
        )));
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let child = entry.path();
        let child_meta = fs::symlink_metadata(&child)?;
        if child_meta.file_type().is_symlink() {
            return Err(AstToMermaidError::InvalidInput(format!(
                "refusing to remove symlink in cache: {}",
                child.display()
            )));
        }
        if child_meta.is_dir() {
            remove_dir_all_safe(&child)?;
        } else {
            fs::remove_file(&child)?;
        }
    }
    fs::remove_dir(path)?;
    Ok(())
}

/// Write a bundle to its final location via tempdir + atomic rename so
/// concurrent runs on the same ref never see a partial bundle.
///
/// If `final_dir` already exists it is wiped first via
/// [`remove_dir_all_safe`] — meaning the helper refuses to delete a
/// symlink that hostile checkout planted at the bundle path. Likewise the
/// scratch tmp dir is required to be a regular directory.
///
/// # Errors
/// I/O errors, or `InvalidInput` when the destination (or any tmp/leftover
/// scratch dir) is a symlink.
pub fn write_bundle_atomic(
    artifacts: &crate::artifacts::ArtifactSet,
    final_dir: &Path,
) -> Result<()> {
    let parent = final_dir.parent().ok_or_else(|| {
        AstToMermaidError::InvalidInput(format!(
            "bundle final dir has no parent: {}",
            final_dir.display()
        ))
    })?;
    fs::create_dir_all(parent)?;
    if let Ok(meta) = fs::symlink_metadata(parent)
        && meta.file_type().is_symlink()
    {
        return Err(AstToMermaidError::InvalidInput(format!(
            "refusing to write bundle into symlinked parent: {}",
            parent.display()
        )));
    }
    let pid = std::process::id();
    let stem = final_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("bundle");
    let tmp_dir = parent.join(format!(".{stem}.tmp.{pid}"));
    if let Ok(meta) = fs::symlink_metadata(&tmp_dir) {
        if meta.file_type().is_symlink() {
            return Err(AstToMermaidError::InvalidInput(format!(
                "refusing to clobber symlinked tmp dir: {}",
                tmp_dir.display()
            )));
        }
        if meta.is_dir() {
            remove_dir_all_safe(&tmp_dir)?;
        } else {
            fs::remove_file(&tmp_dir)?;
        }
    }
    // Atomic-rename target is a freshly-created tmp dir, never an
    // existing populated bundle, so the empty-input safety in
    // `write_artifacts` is a no-op here — pass `allow_empty=true` to
    // make that explicit.
    crate::artifacts::write_artifacts(artifacts, &tmp_dir, true)?;
    if let Ok(meta) = fs::symlink_metadata(final_dir) {
        if meta.file_type().is_symlink() {
            return Err(AstToMermaidError::InvalidInput(format!(
                "refusing to replace symlinked bundle dir: {}",
                final_dir.display()
            )));
        }
        if meta.is_dir() {
            remove_dir_all_safe(final_dir)?;
        } else {
            return Err(AstToMermaidError::InvalidInput(format!(
                "bundle path exists and is not a directory: {}",
                final_dir.display()
            )));
        }
    }
    atomic_rename(&tmp_dir, final_dir)?;

    // High-water-mark auto-gc (C14): once a bundle lands, check whether
    // the cache has crossed its size/age policy and evict LRU until it
    // hasn't. Derives the cache root as `final_dir/../..` to match the
    // `<root>/refs/<sha>` layout. `A2M_AUTO_GC=0` disables this path.
    if let Some(policy) = GcPolicy::from_env()
        && let Some(cache_root) = final_dir.parent().and_then(Path::parent)
    {
        maybe_auto_gc(cache_root, &policy)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CodeAtom, EntityId};
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
            method_calls: vec![],
            parent: None,
        }
    }

    fn dummy_unit(id: &str) -> ParseUnit {
        ParseUnit {
            atoms: vec![dummy_atom(id)],
            edges: vec![],
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
    fn put_then_get_unit_roundtrips() {
        let tmp = tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("c")).unwrap();
        let unit = dummy_unit("code:foo.rs::function::x");
        cache.put_unit("abc123", &unit).unwrap();
        let got = cache.get_unit("abc123").unwrap();
        assert_eq!(got.atoms.len(), 1);
        assert_eq!(got.atoms[0].id, unit.atoms[0].id);
    }

    #[test]
    fn get_unit_returns_none_on_miss() {
        let tmp = tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("c")).unwrap();
        assert!(cache.get_unit("nonexistent").is_none());
    }

    #[test]
    fn version_mismatch_wipes_blobs() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("c");
        {
            let cache = Cache::open(&root).unwrap();
            cache.put_unit("abc", &dummy_unit("code:x")).unwrap();
            assert!(cache.get_unit("abc").is_some());
        }
        // Tamper with version.
        fs::write(root.join("version"), "stale").unwrap();
        let cache = Cache::open(&root).unwrap();
        assert!(cache.get_unit("abc").is_none());
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
                .put_unit(sha, &dummy_unit(&format!("code:{sha}")))
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
        assert!(cache.get_unit("c").is_some());
    }

    #[test]
    fn gc_dry_run_touches_nothing() {
        let tmp = tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("c")).unwrap();
        cache.put_unit("x", &dummy_unit("code:x")).unwrap();
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
        assert!(cache.get_unit("x").is_some());
    }

    #[test]
    fn atomic_write_replaces_existing() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("x");
        atomic_write(&p, b"hello").unwrap();
        atomic_write(&p, b"world").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"world");
    }

    #[test]
    fn atomic_write_leaves_no_tmp_on_success() {
        let tmp = tempdir().unwrap();
        let p = tmp.path().join("x");
        atomic_write(&p, b"hello").unwrap();
        let entries: Vec<_> = fs::read_dir(tmp.path()).unwrap().collect();
        assert_eq!(entries.len(), 1, "tmp file leaked: {entries:?}");
    }

    #[test]
    fn put_unit_uses_atomic_rename() {
        // Concurrent put_unit calls on the same blob_sha must not see
        // partial content. We can't easily test true concurrency here,
        // but we can confirm the .tmp file doesn't linger.
        let tmp = tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("c")).unwrap();
        cache.put_unit("ab", &dummy_unit("code:y")).unwrap();
        let blobs_dir = cache.root().join("blobs");
        let entries: Vec<_> = fs::read_dir(&blobs_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(entries, vec!["ab.cbor"], "no .tmp residue: {entries:?}");
    }

    #[test]
    fn corrupt_blob_returns_none_from_get_unit() {
        let tmp = tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("c")).unwrap();
        // Hand-write garbage with a valid filename.
        let p = cache.root().join("blobs").join("corrupt.cbor");
        fs::write(&p, b"not even cbor").unwrap();
        assert!(cache.get_unit("corrupt").is_none());
    }

    #[test]
    fn version_mismatch_envelope_is_treated_as_miss() {
        let tmp = tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("c")).unwrap();
        // Hand-write a CBOR envelope with bogus version.
        let unit = dummy_unit("code:y");
        let env = BlobEnvelope {
            magic: BLOB_MAGIC,
            version: 9999,
            unit: Cow::Borrowed(&unit),
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&env, &mut buf).unwrap();
        let p = cache.root().join("blobs").join("vmm.cbor");
        fs::write(&p, buf).unwrap();
        assert!(cache.get_unit("vmm").is_none());
    }

    #[test]
    fn gc_empty_cache_is_a_noop() {
        let tmp = tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("c")).unwrap();
        let r = cache.gc(&GcOptions::default()).unwrap();
        assert_eq!(r.removed_count, 0);
        assert_eq!(r.total_before, 0);
        assert!(r.errors.is_empty());
    }

    #[test]
    fn gc_dedups_when_age_and_size_passes_overlap() {
        // Same blob is "old" AND total > cap → both passes flag it.
        // Pre-fix, the second `fs::remove_file` would surface NotFound
        // and `gc` would `Err`. Post-fix, dedup_by-path collapses the
        // two flags and the remove loop fires exactly once.
        let tmp = tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("c")).unwrap();
        for sha in &["a", "b", "c"] {
            cache
                .put_unit(sha, &dummy_unit(&format!("code:{sha}")))
                .unwrap();
        }
        let report = cache
            .gc(&GcOptions {
                // Cap of zero forces every entry into the size pass...
                max_size_bytes: Some(0),
                // ...and a zero age cap forces every entry into the age pass.
                older_than: Some(std::time::Duration::from_secs(0)),
                dry_run: false,
            })
            .expect("dedup must keep gc Ok-not-Err");
        assert_eq!(report.removed_count, 3, "expected 3 removed: {report:?}");
        assert!(report.errors.is_empty(), "no errors expected: {report:?}");
        let blobs = cache.root().join("blobs");
        let leftover: Vec<_> = fs::read_dir(&blobs).unwrap().collect();
        assert!(leftover.is_empty(), "blobs leftover: {leftover:?}");
    }

    #[cfg(unix)]
    #[test]
    fn gc_partial_success_records_errors_instead_of_bubbling() {
        // Plant a bundle, then chmod it read-only so the inner
        // `fs::remove_file` fails with PermissionDenied (a stand-in for
        // disk-full / EROFS / EBUSY). The pre-fix behaviour was `Err`
        // mid-iteration with no record of what was removed; post-fix,
        // the per-path failure lands in `report.errors`.
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("c")).unwrap();

        let bundle = cache.root().join("refs").join("locked");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(bundle.join("index.json"), b"{}").unwrap();

        // Read-exec-only on the bundle dir → can read children but can't
        // unlink them. Restored at the end so tempdir can drop cleanly.
        let mut perms = fs::metadata(&bundle).unwrap().permissions();
        perms.set_mode(0o500);
        fs::set_permissions(&bundle, perms).unwrap();

        let report = cache
            .gc(&GcOptions {
                max_size_bytes: Some(0),
                older_than: None,
                dry_run: false,
            })
            .expect("partial-success path must keep gc Ok-not-Err");
        assert!(
            !report.errors.is_empty(),
            "expected at least one per-path error: {report:?}"
        );
        assert!(
            report.errors.iter().any(|(p, _)| p == &bundle),
            "expected bundle path in errors: {report:?}"
        );

        // Restore so tempdir cleanup can succeed.
        let mut perms = fs::metadata(&bundle).unwrap().permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&bundle, perms).unwrap();
    }

    #[test]
    fn open_sweeps_stale_tmp_blobs() {
        // Crashed prior run left a `.foo.tmp.<pid>.<tid>.<n>` shaped
        // file in blobs/. Cache::open must wipe it on startup so the
        // blob dir can't accumulate garbage across crashes.
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("c");
        fs::create_dir_all(root.join("blobs")).unwrap();
        let stale = root.join("blobs").join(".foo.tmp.123.456.0");
        fs::write(&stale, b"crashed midwrite").unwrap();
        assert!(stale.exists());
        let _cache = Cache::open(&root).unwrap();
        assert!(
            !stale.exists(),
            "Cache::open must sweep stale .tmp. blob: {}",
            stale.display()
        );
    }
}
