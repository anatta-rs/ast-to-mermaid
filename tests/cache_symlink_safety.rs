//! Symlink-safety regression tests for the cache.
//!
//! A hostile checkout could plant `.a2m/cache/refs/<sha>/` as a symlink
//! pointing at `/some/private/path`. Without symlink-aware traversal,
//! `fs::remove_dir_all` would happily delete the target during a `gc`,
//! and bundle writes would clobber unrelated files. These tests pin the
//! "refuse to follow symlinks" contract for `Cache::open`, `Cache::gc`,
//! and the `walk_safe` / `remove_dir_all_safe` helpers themselves.
//!
//! Unix-only: `std::os::unix::fs::symlink` is unavailable on Windows and
//! the cache is not yet exercised on Windows in CI.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::symlink;

use ast_to_mermaid::cache::{Cache, GcOptions, remove_dir_all_safe, walk_safe};
use tempfile::tempdir;

#[test]
fn gc_refuses_when_blob_dir_contains_a_symlink() {
    let tmp = tempdir().unwrap();
    let cache_root = tmp.path().join(".a2m").join("cache");
    let cache = Cache::open(&cache_root).unwrap();

    // Drop a precious file outside the cache; the symlink will point at
    // its parent, simulating a "wipe everything in /tmp/precious" attack.
    let precious = tmp.path().join("precious");
    fs::create_dir_all(&precious).unwrap();
    let canary = precious.join("keep-me");
    fs::write(&canary, b"do-not-delete").unwrap();

    // Plant a symlink at refs/<sha> → /tmp/.../precious.
    let evil = cache_root.join("refs").join("evil");
    symlink(&precious, &evil).unwrap();

    // Naive `remove_dir_all` would walk through `evil` and unlink the
    // canary. The safe helper must refuse and leave the target alone.
    let err = cache
        .gc(&GcOptions {
            max_size_bytes: Some(0), // force eviction of everything
            older_than: None,
            dry_run: false,
        })
        .expect_err("gc must refuse when a symlink is planted in refs/");
    let msg = err.to_string();
    assert!(
        msg.contains("symlink"),
        "expected error mentioning symlink, got: {msg}"
    );
    assert!(canary.is_file(), "canary file outside cache was deleted");
    assert!(precious.is_dir(), "precious target dir was deleted");
}

#[test]
fn gc_refuses_when_blob_file_is_a_symlink() {
    let tmp = tempdir().unwrap();
    let cache_root = tmp.path().join(".a2m").join("cache");
    let cache = Cache::open(&cache_root).unwrap();

    let precious = tmp.path().join("precious.txt");
    fs::write(&precious, b"do-not-delete").unwrap();

    let evil = cache_root.join("blobs").join("evil.cbor");
    symlink(&precious, &evil).unwrap();

    let err = cache
        .gc(&GcOptions::default())
        .expect_err("gc must refuse when a symlink is planted in blobs/");
    assert!(err.to_string().contains("symlink"));
    assert!(precious.is_file(), "symlink target was unlinked");
}

#[test]
fn open_refuses_existing_symlink_inside_cache_root() {
    let tmp = tempdir().unwrap();
    let cache_root = tmp.path().join(".a2m").join("cache");
    fs::create_dir_all(cache_root.join("refs")).unwrap();
    fs::create_dir_all(cache_root.join("blobs")).unwrap();

    let outside = tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    symlink(&outside, cache_root.join("refs").join("evil")).unwrap();

    let err = Cache::open(&cache_root)
        .err()
        .expect("Cache::open must refuse a cache root with planted symlinks");
    assert!(err.to_string().contains("symlink"));
}

#[test]
fn remove_dir_all_safe_refuses_top_level_symlink() {
    let tmp = tempdir().unwrap();
    let target = tmp.path().join("target");
    fs::create_dir_all(&target).unwrap();
    let canary = target.join("keep-me");
    fs::write(&canary, b"x").unwrap();

    let link = tmp.path().join("link");
    symlink(&target, &link).unwrap();

    let err = remove_dir_all_safe(&link).expect_err("must refuse a symlinked path");
    assert!(err.to_string().contains("symlink"));
    assert!(canary.is_file(), "symlink target was deleted");
}

#[test]
fn remove_dir_all_safe_refuses_nested_symlink_and_leaves_target() {
    let tmp = tempdir().unwrap();
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let canary = outside.join("keep-me");
    fs::write(&canary, b"x").unwrap();

    let dir = tmp.path().join("dir");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("regular"), b"y").unwrap();
    symlink(&outside, dir.join("evil")).unwrap();

    let err =
        remove_dir_all_safe(&dir).expect_err("must refuse a tree containing a nested symlink");
    assert!(err.to_string().contains("symlink"));
    assert!(canary.is_file(), "symlink target was deleted");
}

#[test]
fn walk_safe_returns_err_for_symlinked_root() {
    let tmp = tempdir().unwrap();
    let target = tmp.path().join("target");
    fs::create_dir_all(&target).unwrap();
    let link = tmp.path().join("link");
    symlink(&target, &link).unwrap();

    let err = walk_safe(&link).expect_err("walk_safe must refuse a symlinked root");
    assert!(err.to_string().contains("symlink"));
}

#[test]
fn walk_safe_visits_regular_subtree() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path().join("dir");
    fs::create_dir_all(dir.join("nested")).unwrap();
    fs::write(dir.join("a"), b"1").unwrap();
    fs::write(dir.join("nested").join("b"), b"2").unwrap();

    let entries = walk_safe(&dir).unwrap();
    let names: Vec<String> = entries
        .iter()
        .map(|(p, _)| p.strip_prefix(&dir).unwrap().display().to_string())
        .collect();
    assert!(names.iter().any(String::is_empty), "root not visited");
    assert!(names.iter().any(|n| n == "a"));
    assert!(names.iter().any(|n| n == "nested"));
    assert!(names.iter().any(|n| n.replace('\\', "/") == "nested/b"));
}
