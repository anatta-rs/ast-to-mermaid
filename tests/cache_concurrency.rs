//! Concurrency regression for issue #117 (C26): rayon workers that race
//! on `Cache::put_unit(same_blob_sha, ...)` must not corrupt each other's
//! tmp files. Pre-fix the per-pid tmp suffix collided across threads;
//! the second worker's `fs::write` truncated the first's bytes mid-rename
//! and `get_unit` would see `UnexpectedEof` (or, on a different rename
//! interleaving, `NotFound`).
//!
//! Also pins the [`Cache::open`] crash-recovery sweep contract — any
//! leftover `blobs/.*.tmp.*` (from any prior pid/thread/counter) is
//! removed silently on open.

use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use ast_to_mermaid::cache::Cache;
use ast_to_mermaid::model::{CodeAtom, EntityId};
use ast_to_mermaid::parser::ParseUnit;
use rayon::prelude::*;
use tempfile::tempdir;

fn distinct_unit(seed: usize) -> ParseUnit {
    ParseUnit {
        atoms: vec![CodeAtom {
            id: EntityId::new(format!("code:race.rs::function::worker_{seed}")),
            kind: "function".to_owned(),
            name: format!("worker_{seed}"),
            file_path: "src/race.rs".to_owned(),
            line_start: 1,
            line_end: 5,
            doc: String::new(),
            signature: format!("fn worker_{seed}()"),
            content_hash: "deadbeef".to_owned(),
            calls: vec![],
            method_calls: vec![],
            parent: None,
        }],
        edges: vec![],
    }
}

#[test]
fn rayon_workers_racing_same_blob_sha_never_corrupt() {
    // 4 workers × 100 writes against the same blob_sha. Pre-fix this
    // surfaced as `UnexpectedEof` / `NotFound` in roughly 1 of 5 runs
    // on a busy machine. Post-fix every read of the final on-disk
    // envelope must deserialize cleanly.
    let tmp = tempdir().unwrap();
    let cache = Cache::open(tmp.path().join("c")).unwrap();
    let blob_sha = "0000000000000000000000000000000000000042";

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .unwrap();

    let read_errors = AtomicUsize::new(0);
    let write_errors = AtomicUsize::new(0);

    pool.install(|| {
        (0..400usize).into_par_iter().for_each(|i| {
            let unit = distinct_unit(i);
            if cache.put_unit(blob_sha, &unit).is_err() {
                write_errors.fetch_add(1, Ordering::Relaxed);
                return;
            }
            // Interleave reads with writes — the read-side must always
            // see a complete envelope (some prior writer's bytes), never
            // a torn or truncated file.
            if cache.get_unit(blob_sha).is_none() {
                read_errors.fetch_add(1, Ordering::Relaxed);
            }
        });
    });

    assert_eq!(
        write_errors.load(Ordering::Relaxed),
        0,
        "put_unit returned errors under concurrent same-sha writes",
    );
    assert_eq!(
        read_errors.load(Ordering::Relaxed),
        0,
        "get_unit observed a torn/missing envelope under concurrent writes",
    );

    // Final state: file deserializes, contains some worker's atom.
    let final_unit = cache
        .get_unit(blob_sha)
        .expect("final envelope must deserialize");
    assert_eq!(final_unit.atoms.len(), 1);
    assert!(
        final_unit.atoms[0].name.starts_with("worker_"),
        "unexpected atom: {}",
        final_unit.atoms[0].name,
    );

    // No tmp residue in the blobs dir — every successful rename should
    // leave only `<sha>.cbor` behind.
    let blobs_dir = cache.root().join("blobs");
    let leaked: Vec<_> = fs::read_dir(&blobs_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.contains(".tmp."))
        .collect();
    assert!(
        leaked.is_empty(),
        "tmp residue after rayon stress: {leaked:?}"
    );
}

#[test]
fn cache_open_sweeps_stale_tmp_blobs() {
    // Spec: pre-place `.foo.tmp.123.456.0` (new shape) and `.foo.tmp.999`
    // (legacy pid-only shape) — both removed at open, no warning.
    //
    // Open once first so the version file exists; otherwise the
    // version-mismatch path would wipe `blobs/` wholesale and the
    // sweep wouldn't be exercised.
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("c");
    drop(Cache::open(&root).unwrap());

    let new_shape = root.join("blobs").join(".foo.tmp.123.456.0");
    let legacy_shape = root.join("blobs").join(".foo.tmp.999");
    let real_blob = root.join("blobs").join("abcd.cbor");
    fs::write(&new_shape, b"stale-new").unwrap();
    fs::write(&legacy_shape, b"stale-legacy").unwrap();
    fs::write(&real_blob, b"keep-me").unwrap();

    let _cache = Cache::open(&root).unwrap();

    assert!(!new_shape.exists(), "new-shape tmp not swept");
    assert!(!legacy_shape.exists(), "legacy-shape tmp not swept");
    assert!(
        real_blob.exists(),
        "non-tmp blob must not be touched by sweep"
    );
}
