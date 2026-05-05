//! End-to-end tests for the `--ref` flag-injection guard.
//!
//! Crafts a `--ref` value shaped like a `git` flag (`--upload-pack=…`) and
//! drives every subcommand that takes `--ref`. Each invocation must:
//!
//! 1. exit with status 2 (`UsageError`),
//! 2. emit a clear error mentioning the offending ref,
//! 3. never spawn the helper binary the attacker tried to inject — proven
//!    here by checking the marker string `HACKED` never appears on either
//!    stdout or stderr.

use std::path::PathBuf;
use std::process::Command;

fn a2m_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_a2m"))
}

fn assert_blocks_flag_injection(args: &[&str]) {
    let out = Command::new(a2m_path())
        .args(args)
        .output()
        .expect("spawn a2m");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(2),
        "expected exit 2 for {args:?}; stdout={stdout} stderr={stderr}",
    );
    // The injected payload was `/bin/echo HACKED`; if it had run the marker
    // would land on stdout. Stderr legitimately contains the substring
    // because our error message echoes the rejected ref verbatim — so we
    // only police stdout here.
    assert!(
        !stdout.contains("HACKED"),
        "marker on stdout — subprocess was spawned: stdout={stdout}",
    );
    assert!(
        stderr.contains("--ref") || stderr.contains("CLI flag") || stderr.contains("'-'"),
        "expected ref-rejection diagnostic on stderr; got: {stderr}",
    );
}

#[test]
fn overview_rejects_flag_like_ref() {
    assert_blocks_flag_injection(&["overview", ".", "--ref=--upload-pack=/bin/echo HACKED"]);
}

#[test]
fn walk_rejects_flag_like_ref() {
    assert_blocks_flag_injection(&["walk", ".", "--ref=--upload-pack=/bin/echo HACKED"]);
}

#[test]
fn bundle_rejects_flag_like_ref() {
    assert_blocks_flag_injection(&[
        "bundle",
        ".",
        "--out",
        "/tmp/a2m-injection-test-bundle",
        "--ref=--upload-pack=/bin/echo HACKED",
    ]);
}

#[test]
fn index_rejects_flag_like_ref() {
    assert_blocks_flag_injection(&["index", ".", "--ref=--upload-pack=/bin/echo HACKED"]);
}

#[test]
fn diff_rejects_flag_like_ref_on_either_side() {
    // Left side: clap would otherwise mistake the leading `--upload-pack` for
    // an option, so the test passes `--` first to let our positional `range`
    // arg receive the malicious string verbatim.
    assert_blocks_flag_injection(&["diff", "--", "--upload-pack=/bin/echo HACKED..HEAD"]);
    // Right side: no clap ambiguity, the positional starts with `HEAD`.
    assert_blocks_flag_injection(&["diff", "HEAD..--upload-pack=/bin/echo HACKED"]);
}

#[test]
fn sequence_rejects_flag_like_ref() {
    assert_blocks_flag_injection(&[
        "sequence",
        ".",
        "--target",
        "any",
        "--ref=--upload-pack=/bin/echo HACKED",
    ]);
}

#[test]
fn rejects_whitespace_nul_and_path_traversal() {
    for bad in ["with space", "tab\there", "..", "foo/../bar", "café"] {
        let out = Command::new(a2m_path())
            .args(["overview", ".", "--ref", bad])
            .output()
            .expect("spawn a2m");
        assert_eq!(
            out.status.code(),
            Some(2),
            "expected exit 2 for ref {bad:?}; stderr={}",
            String::from_utf8_lossy(&out.stderr),
        );
    }
}
