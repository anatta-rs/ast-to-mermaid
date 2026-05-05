//! Bounded GC: high-water-mark auto-trigger + symlink-loop guard.
//!
//! Acceptance tests for issue #79 (C14). The auto-trigger test exercises
//! the [`maybe_auto_gc`] contract directly with an explicit policy so it
//! doesn't depend on env-var ordering between integration test binaries.
//! A second test wires through `write_bundle_atomic` end-to-end via
//! `A2M_AUTO_GC_MAX_BYTES` to pin the documented integration point.
//!
//! The symlink-loop test plants `a -> b -> a` inside a bundle dir and
//! confirms `dir_size_recursive` refuses (via `walk_safe`) instead of
//! blowing the stack.
//!
//! Unix-only because the symlink primitive is.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::symlink;
use std::sync::Mutex;

use ast_to_mermaid::artifacts::ArtifactSet;
use ast_to_mermaid::cache::{Cache, GcOptions, GcPolicy, maybe_auto_gc, write_bundle_atomic};
use tempfile::tempdir;

/// Tests that mutate `A2M_AUTO_GC_*` must hold this lock — env is
/// process-global and `cargo test` runs tests in parallel by default.
/// Tests that don't touch env (the direct-`maybe_auto_gc` ones) ignore
/// this lock since `from_env` is only consulted from `write_bundle_atomic`.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Plant `count` fake "bundle" directories under `refs/`, each
/// containing an `index.json` of `bytes_each` bytes. Returns the total
/// planted byte count. Used to push the cache deliberately over a tiny
/// `max_total_bytes` so the policy threshold trips.
fn plant_fake_bundles(cache_root: &std::path::Path, count: usize, bytes_each: usize) -> u64 {
    let refs = cache_root.join("refs");
    fs::create_dir_all(&refs).unwrap();
    let payload = vec![b'x'; bytes_each];
    let mut total = 0u64;
    for i in 0..count {
        let bdir = refs.join(format!("planted-{i:04}"));
        fs::create_dir_all(&bdir).unwrap();
        fs::write(bdir.join("index.json"), &payload).unwrap();
        total += bytes_each as u64;
        // Stagger mtimes so eviction is unambiguous (oldest first).
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    total
}

#[test]
fn cache_gc_auto_trigger_evicts_under_policy() {
    let tmp = tempdir().unwrap();
    let cache_root = tmp.path().join(".a2m").join("cache");
    let _cache = Cache::open(&cache_root).unwrap();

    // Plant ~8 KiB of cache content; cap at 2 KiB.
    let planted = plant_fake_bundles(&cache_root, 8, 1024);
    let cap = 2 * 1024;
    assert!(planted > cap, "test setup: must plant over the cap");

    let policy = GcPolicy {
        max_total_bytes: Some(cap),
        max_age_secs: None,
    };

    let report = maybe_auto_gc(&cache_root, &policy)
        .expect("maybe_auto_gc must succeed")
        .expect("policy is breached, a GC should have run");
    assert!(!report.dry_run);
    assert!(report.removed_count > 0, "expected evictions: {report:?}");

    // Compute remaining size by re-running gc with a generous cap (no-op
    // run that just enumerates).
    let cache = Cache::open(&cache_root).unwrap();
    let after = cache
        .gc(&GcOptions {
            max_size_bytes: Some(u64::MAX),
            older_than: None,
            dry_run: true,
        })
        .unwrap();
    assert!(
        after.total_before <= cap,
        "post-gc total {} must be ≤ cap {}",
        after.total_before,
        cap
    );
}

#[test]
fn cache_gc_auto_trigger_noop_when_under_policy() {
    let tmp = tempdir().unwrap();
    let cache_root = tmp.path().join(".a2m").join("cache");
    let _cache = Cache::open(&cache_root).unwrap();

    plant_fake_bundles(&cache_root, 2, 256);
    let policy = GcPolicy {
        max_total_bytes: Some(1024 * 1024),
        max_age_secs: None,
    };

    let report = maybe_auto_gc(&cache_root, &policy).unwrap();
    assert!(
        report.is_none(),
        "under-policy + no age cap → no GC should run, got {report:?}"
    );
}

#[test]
fn cache_gc_auto_trigger_via_write_bundle_atomic() {
    // Pins the wiring described in the C14 mermaid:
    //   write_bundle_atomic --> maybe_auto_gc --> gc
    // We set A2M_AUTO_GC_MAX_BYTES tiny, plant filler under refs/, then
    // call write_bundle_atomic and verify the planted bundles got
    // evicted to bring the cache back under the cap.
    //
    // Env vars are process-global; this is the only test in this binary
    // that touches A2M_AUTO_GC_*. `set_var` is unsafe under 2024 edition
    // — scoped allow keeps the lib-level `unsafe_code = deny` honest.

    let tmp = tempdir().unwrap();
    let cache_root = tmp.path().join(".a2m").join("cache");
    let _cache = Cache::open(&cache_root).unwrap();

    let planted = plant_fake_bundles(&cache_root, 8, 1024);
    let cap: u64 = 2 * 1024;
    assert!(planted > cap);

    let guard = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("A2M_AUTO_GC_MAX_BYTES", cap.to_string());
        std::env::remove_var("A2M_AUTO_GC");
        std::env::remove_var("A2M_AUTO_GC_MAX_AGE_SECS");
    }

    // Trigger the wiring with a real (empty) bundle write.
    let bundle_dir = cache_root.join("refs").join("trigger");
    let empty = ArtifactSet {
        overview_mmd: String::new(),
        entities: vec![],
        index_json: serde_json::json!({}),
        sequences: vec![],
    };
    write_bundle_atomic(&empty, &bundle_dir).unwrap();

    // Cleanup env so it doesn't leak to any later test.
    #[allow(unsafe_code)]
    unsafe {
        std::env::remove_var("A2M_AUTO_GC_MAX_BYTES");
    }
    drop(guard);

    let cache = Cache::open(&cache_root).unwrap();
    let after = cache
        .gc(&GcOptions {
            max_size_bytes: Some(u64::MAX),
            older_than: None,
            dry_run: true,
        })
        .unwrap();
    assert!(
        after.total_before <= cap,
        "post-write_bundle_atomic auto-gc total {} must be ≤ cap {}",
        after.total_before,
        cap
    );
}

#[test]
fn cache_gc_auto_trigger_disabled_by_env_opt_out() {
    let tmp = tempdir().unwrap();
    let cache_root = tmp.path().join(".a2m").join("cache");
    let _cache = Cache::open(&cache_root).unwrap();

    plant_fake_bundles(&cache_root, 8, 1024);

    let guard = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("A2M_AUTO_GC", "0");
        std::env::set_var("A2M_AUTO_GC_MAX_BYTES", "1");
    }
    let policy_from_env = GcPolicy::from_env();
    #[allow(unsafe_code)]
    unsafe {
        std::env::remove_var("A2M_AUTO_GC");
        std::env::remove_var("A2M_AUTO_GC_MAX_BYTES");
    }
    drop(guard);
    assert!(
        policy_from_env.is_none(),
        "A2M_AUTO_GC=0 must yield None from GcPolicy::from_env"
    );
}

#[test]
fn cache_gc_symlink_loop_does_not_recurse_forever() {
    // Plant `refs/<sha>/a -> b` and `refs/<sha>/b -> a`. The pre-walk_safe
    // dir_size_recursive would have stack-overflowed; the new one
    // delegates to walk_safe which refuses on first symlink.
    let tmp = tempdir().unwrap();
    let cache_root = tmp.path().join(".a2m").join("cache");
    let cache = Cache::open(&cache_root).unwrap();

    let bundle = cache_root.join("refs").join("looped");
    fs::create_dir_all(&bundle).unwrap();
    // index.json so it looks like a real bundle.
    fs::write(bundle.join("index.json"), b"{}").unwrap();
    // Symlink loop: a -> b, b -> a. Relative targets so the loop is
    // self-contained inside the bundle dir.
    symlink("b", bundle.join("a")).unwrap();
    symlink("a", bundle.join("b")).unwrap();

    let err = cache
        .gc(&GcOptions::default())
        .expect_err("gc must refuse to follow symlink loop in bundle dir");
    let msg = err.to_string();
    assert!(
        msg.contains("symlink"),
        "expected symlink-refusal error, got: {msg}"
    );
}
