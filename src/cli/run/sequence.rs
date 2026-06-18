use super::check_ref_arg;
use crate::cli::flags::{ExitCode, SequenceFlags};
use crate::cli::format::parse_csv_exclude;
use crate::parser::Language;
use crate::pipeline::{language_for, walk_for_languages_with_exclude};
use crate::sequence;
use std::path::Path;

/// A collected source file: `(display_path, content, language)`.
type Source = (String, Vec<u8>, Language);

/// Run the `sequence` subcommand. Two modes:
///
/// - Single-target (`--target <name>`): locate one function, render its
///   body to stdout or `--out <FILE>`.
/// - All (`--all`, requires `--out <DIR>`): every function (Rust or Python)
///   in the source tree is rendered into its own `<DIR>/<file>__<name>.mmd`.
pub fn run_sequence(flags: &SequenceFlags) -> ExitCode {
    if !flags.all && flags.target.as_deref().is_none_or(|t| t.trim().is_empty()) {
        eprintln!("sequence: pass --target <NAME> or --all");
        return ExitCode::UsageError;
    }
    if let Err(code) = check_ref_arg("sequence", flags.r#ref.as_deref()) {
        return code;
    }
    let exclude = parse_csv_exclude(&flags.exclude);

    let candidates = match collect_sources(&flags.path, &exclude, flags.r#ref.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("sequence: collect sources {}: {e}", flags.path.display());
            return ExitCode::Failure;
        }
    };
    if candidates.is_empty() {
        eprintln!(
            "sequence: no Rust or Python files found under {}",
            flags.path.display()
        );
        return ExitCode::Failure;
    }

    if flags.all {
        return run_sequence_all(&candidates, flags.out.as_deref());
    }
    run_sequence_single(
        &candidates,
        flags.target.as_deref().unwrap_or(""),
        flags.out.as_deref(),
        &flags.path,
    )
}

fn run_sequence_single(
    candidates: &[Source],
    target: &str,
    out: Option<&Path>,
    path: &Path,
) -> ExitCode {
    // Parse each file at most once: a single `parse_source_once` feeds
    // both `list_functions_in_tree` (does the file declare `target`?)
    // and `extract_all` (extract the diagram on the same tree). Once a
    // match is found we break — files past it are never parsed.
    let mut diagram = None;
    for (file_rel, content, lang) in candidates {
        let Ok(text) = std::str::from_utf8(content) else {
            continue;
        };
        let Ok(tree) = sequence::parse_source_once(content, file_rel, *lang) else {
            continue;
        };
        let names = sequence::list_functions_in_tree(&tree, text, *lang);
        if names.iter().any(|n| n == target) {
            let mut map = sequence::extract_all(&tree, text, &[target], *lang);
            if let Some(d) = map.remove(target) {
                diagram = Some((file_rel.clone(), d));
                break;
            }
        }
    }
    let Some((file_rel, diagram)) = diagram else {
        eprintln!(
            "sequence: function `{target}` not found under {}",
            path.display()
        );
        return ExitCode::Failure;
    };
    let rendered = sequence::render(&diagram);

    let suffix = if let Some(path) = out {
        if let Err(e) = std::fs::write(path, &rendered) {
            eprintln!("sequence: write {}: {e}", path.display());
            return ExitCode::Failure;
        }
        format!(" → {}", path.display())
    } else {
        print!("{rendered}");
        String::new()
    };
    eprintln!(
        "sequence {target} from {file_rel} ({} participants, {} steps){suffix}",
        diagram.participants.len(),
        diagram.steps.len(),
    );
    ExitCode::Success
}

fn run_sequence_all(candidates: &[Source], out: Option<&Path>) -> ExitCode {
    let Some(out_dir) = out else {
        eprintln!("sequence: --all requires --out <DIR>");
        return ExitCode::UsageError;
    };
    if let Err(e) = std::fs::create_dir_all(out_dir) {
        eprintln!("sequence: create {}: {e}", out_dir.display());
        return ExitCode::Failure;
    }

    // Parse each file exactly once, enumerate functions on the resulting
    // tree, then `extract_all` to collect every diagram in a single AST
    // traversal. Pre-v0.6.0 this re-parsed each file 1+N times (once for
    // `list_functions`, once per function for `extract`).
    let mut written = 0usize;
    let mut skipped = 0usize;
    for (file_rel, content, lang) in candidates {
        let text = match std::str::from_utf8(content) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("sequence: parse {file_rel}: {e}");
                skipped += 1;
                continue;
            }
        };
        let tree = match sequence::parse_source_once(content, file_rel, *lang) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("sequence: parse {file_rel}: {e}");
                skipped += 1;
                continue;
            }
        };
        let names = sequence::list_functions_in_tree(&tree, text, *lang);
        let target_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut map = sequence::extract_all(&tree, text, &target_refs, *lang);
        for name in names {
            let Some(diagram) = map.remove(&name) else {
                skipped += 1;
                continue;
            };
            if diagram.steps.is_empty() {
                skipped += 1;
                continue;
            }
            let final_name = build_sequence_path(file_rel, &name);
            let target_path = out_dir.join(&final_name);
            let rendered = sequence::render(&diagram);
            if let Err(e) = std::fs::write(&target_path, rendered) {
                eprintln!("sequence: write {}: {e}", target_path.display());
                return ExitCode::Failure;
            }
            written += 1;
        }
    }
    eprintln!(
        "sequence --all: {written} diagrams written to {} ({skipped} skipped: empty / parse fail)",
        out_dir.display(),
    );
    ExitCode::Success
}

/// Build the on-disk filename for a `(file_rel, qualified_name)` sequence
/// diagram by funneling through the canonical [`crate::artifacts::filename_id`].
/// That helper handles the alphanumeric+`._-` allow-list and applies a
/// `_H<hash>` suffix when the input has uppercase bytes, so case-only
/// siblings (`Foo`/`foo`/`FOO`) map to distinct files on case-insensitive
/// filesystems without any caller-side collision detection.
fn build_sequence_path(file_rel: &str, qualified_name: &str) -> String {
    let key = format!("{file_rel}::{qualified_name}");
    format!("{}.mmd", crate::artifacts::filename_id(&key))
}

/// Collect `(display_path, content, language)` tuples for every supported
/// source file (Rust + Python) under `root`, honouring `exclude`. With
/// `git_ref`, reads from `git ls-tree` instead of the working tree.
///
/// The per-file language drives which tree-sitter grammar
/// [`sequence::parse_source_once`] uses, so a mixed Rust/Python tree emits
/// diagrams for both.
fn collect_sources(
    root: &Path,
    exclude: &[String],
    git_ref: Option<&str>,
) -> Result<Vec<Source>, crate::error::AstToMermaidError> {
    if let Some(git_ref) = git_ref {
        let toplevel = crate::git_source::show_toplevel(root)?;
        let entries = crate::git_source::ls_tree(&toplevel, git_ref)?;
        // One persistent `git cat-file --batch` child amortises the
        // subprocess fork across every blob — a per-blob spawn is ~50x
        // slower on a 100-blob ref (50+s vs <1s).
        let mut reader = crate::git_source::BatchReader::spawn(&toplevel)?;
        let mut out = Vec::new();
        for entry in entries {
            let Some(lang) = language_for(Path::new(&entry.path)) else {
                continue;
            };
            let content = reader.read_blob(&entry.blob_sha)?;
            out.push((entry.path, content, lang));
        }
        Ok(out)
    } else {
        let files = walk_for_languages_with_exclude(root, exclude)?;
        let mut out = Vec::new();
        for (path, lang) in files {
            let content = std::fs::read(&path)?;
            let display = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            out.push((display, content, lang));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::run::test_helpers::{init_rust_repo, write_rust};
    use std::path::PathBuf;

    fn flags_for(path: PathBuf, target: Option<&str>) -> SequenceFlags {
        SequenceFlags {
            path,
            target: target.map(str::to_owned),
            all: false,
            exclude: String::new(),
            out: None,
            r#ref: None,
        }
    }

    #[test]
    fn sequence_no_target_no_all_is_usage_error() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = flags_for(tmp.path().to_path_buf(), None);
        assert_eq!(run_sequence(&flags), ExitCode::UsageError);
    }

    #[test]
    fn sequence_empty_target_is_usage_error() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = flags_for(tmp.path().to_path_buf(), Some("   "));
        assert_eq!(run_sequence(&flags), ExitCode::UsageError);
    }

    #[test]
    fn sequence_no_rust_files_returns_failure() {
        let tmp = tempfile::tempdir().expect("tmp");
        let flags = flags_for(tmp.path().to_path_buf(), Some("anything"));
        assert_eq!(run_sequence(&flags), ExitCode::Failure);
    }

    #[test]
    fn sequence_unknown_function_returns_failure() {
        let tmp = tempfile::tempdir().expect("tmp");
        write_rust(tmp.path(), "lib.rs", "fn other(){}\n");
        let flags = flags_for(tmp.path().to_path_buf(), Some("missing"));
        assert_eq!(run_sequence(&flags), ExitCode::Failure);
    }

    #[test]
    fn sequence_writes_mermaid_to_file() {
        let tmp = tempfile::tempdir().expect("tmp");
        write_rust(
            tmp.path(),
            "lib.rs",
            "fn run(cache: &Cache) { cache.open(); foo(); }\n",
        );
        let out = tmp.path().join("seq.mmd");
        let mut flags = flags_for(tmp.path().to_path_buf(), Some("run"));
        flags.out = Some(out.clone());
        assert_eq!(run_sequence(&flags), ExitCode::Success);
        let body = std::fs::read_to_string(&out).expect("read");
        assert!(body.starts_with("sequenceDiagram"), "got:\n{body}");
        assert!(body.contains("self->>cache: open"), "got:\n{body}");
        assert!(body.contains("self->>self: foo"), "got:\n{body}");
    }

    #[test]
    fn sequence_writes_python_mermaid_to_file() {
        let tmp = tempfile::tempdir().expect("tmp");
        // `write_rust` is a generic file writer — the `.py` extension drives
        // `language_for` to pick the Python grammar.
        write_rust(
            tmp.path(),
            "mod.py",
            "def run(cache):\n    cache.open()\n    helper()\n",
        );
        let out = tmp.path().join("seq.mmd");
        let mut flags = flags_for(tmp.path().to_path_buf(), Some("run"));
        flags.out = Some(out.clone());
        assert_eq!(run_sequence(&flags), ExitCode::Success);
        let body = std::fs::read_to_string(&out).expect("read");
        assert!(body.starts_with("sequenceDiagram"), "got:\n{body}");
        assert!(body.contains("self->>cache: open"), "got:\n{body}");
        assert!(body.contains("self->>self: helper"), "got:\n{body}");
    }

    #[test]
    fn sequence_all_writes_python_and_rust_in_mixed_tree() {
        let tmp = tempfile::tempdir().expect("tmp");
        write_rust(tmp.path(), "lib.rs", "fn rust_fn(){ helper(); }\nfn helper(){}\n");
        write_rust(tmp.path(), "mod.py", "def py_fn():\n    work()\n");
        let out = tmp.path().join("diagrams");
        let mut flags = flags_for(tmp.path().to_path_buf(), None);
        flags.all = true;
        flags.out = Some(out.clone());
        assert_eq!(run_sequence(&flags), ExitCode::Success);
        let entries: Vec<String> = std::fs::read_dir(&out)
            .expect("read out_dir")
            .filter_map(std::result::Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        assert!(
            entries.iter().any(|n| n.ends_with("__rust_fn.mmd")),
            "rust diagram missing: {entries:?}"
        );
        assert!(
            entries.iter().any(|n| n.ends_with("__py_fn.mmd")),
            "python diagram missing: {entries:?}"
        );
    }

    #[test]
    fn sequence_from_git_ref_succeeds() {
        let tmp = tempfile::tempdir().expect("tmp");
        init_rust_repo(tmp.path(), "lib.rs", "fn run() { foo(); }\n");
        let mut flags = flags_for(tmp.path().to_path_buf(), Some("run"));
        flags.r#ref = Some("HEAD".into());
        assert_eq!(run_sequence(&flags), ExitCode::Success);
    }

    #[test]
    fn sequence_all_requires_out_dir() {
        let tmp = tempfile::tempdir().expect("tmp");
        write_rust(tmp.path(), "lib.rs", "fn a(){}\n");
        let mut flags = flags_for(tmp.path().to_path_buf(), None);
        flags.all = true;
        assert_eq!(run_sequence(&flags), ExitCode::UsageError);
    }

    #[test]
    fn sequence_all_writes_one_file_per_nonempty_function() {
        // `fn b(){}` has no calls — its diagram would be a header-only
        // stub, so --all skips it. `a` and `S::m` both call something
        // and so produce real files.
        let tmp = tempfile::tempdir().expect("tmp");
        write_rust(
            tmp.path(),
            "lib.rs",
            "fn a(){ b(); }\nfn b(){}\nstruct S;\nimpl S { fn m(&self){ a(); } }\n",
        );
        let out = tmp.path().join("diagrams");
        let mut flags = flags_for(tmp.path().to_path_buf(), None);
        flags.all = true;
        flags.out = Some(out.clone());
        assert_eq!(run_sequence(&flags), ExitCode::Success);
        let entries: Vec<String> = std::fs::read_dir(&out)
            .expect("read out_dir")
            .filter_map(std::result::Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        assert!(
            entries.iter().any(|n| n.ends_with("__a.mmd")),
            "got: {entries:?}"
        );
        // `S::m` is qualified — `filename_id` lowercases the whole id when
        // any byte is uppercase and appends `_H<hash>`, so the on-disk
        // marker is `s__m_H` (not `S__m`).
        assert!(
            entries.iter().any(|n| n.contains("s__m_H")),
            "got: {entries:?}"
        );
        // Empty function `b` must NOT produce a file.
        assert!(
            !entries.iter().any(|n| n.ends_with("__b.mmd")),
            "empty b leaked: {entries:?}",
        );
    }

    /// `Foo`, `foo`, `FOO` clobber each other on case-insensitive
    /// APFS. `run_sequence_all` must pre-detect the collision and
    /// disambiguate so all three survive on disk.
    #[test]
    #[cfg(target_os = "macos")]
    fn sequence_all_disambiguates_case_collisions_on_macos() {
        let tmp = tempfile::tempdir().expect("tmp");
        write_rust(
            tmp.path(),
            "lib.rs",
            "fn Foo(){ helper(); }\nfn foo(){ helper(); }\nfn FOO(){ helper(); }\nfn helper(){}\n",
        );
        let out = tmp.path().join("diagrams");
        let mut flags = flags_for(tmp.path().to_path_buf(), None);
        flags.all = true;
        flags.out = Some(out.clone());
        assert_eq!(run_sequence(&flags), ExitCode::Success);
        let entries: Vec<String> = std::fs::read_dir(&out)
            .expect("read out_dir")
            .filter_map(std::result::Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| {
                Path::new(n)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("mmd"))
            })
            .collect();
        // 3 distinct .mmd files survive on APFS — Foo, foo, FOO with
        // disambig suffixes.
        let case_files: Vec<&String> = entries
            .iter()
            .filter(|n| {
                let lc = n.to_ascii_lowercase();
                lc.contains("__foo") || lc.contains("__foo_h")
            })
            .collect();
        assert_eq!(
            case_files.len(),
            3,
            "expected 3 distinct files for Foo/foo/FOO, got: {entries:?}"
        );
        // And their lowercased names are all distinct (the actual
        // case-insensitive-FS guarantee).
        let mut lowered: Vec<String> = case_files.iter().map(|s| s.to_ascii_lowercase()).collect();
        lowered.sort();
        lowered.dedup();
        assert_eq!(lowered.len(), 3, "filenames collide on APFS: {entries:?}");
    }

    #[test]
    fn build_sequence_path_funnels_through_filename_id() {
        // `Cache::open` has uppercase, so the canonical `filename_id`
        // lowercases the whole id and appends `_H<hash>` — caller no
        // longer hand-rolls a sanitizer.
        let f = build_sequence_path("src/cache.rs", "Cache::open");
        let expected = format!(
            "{}.mmd",
            crate::artifacts::filename_id("src/cache.rs::Cache::open")
        );
        assert_eq!(f, expected);
        assert!(f.contains("cache"), "got: {f}");
        assert!(f.contains("__cache__open_H"), "got: {f}");
        assert!(
            Path::new(&f)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("mmd"))
        );
    }

    #[test]
    fn build_sequence_path_disambiguates_case_collisions() {
        // `Foo`/`foo`/`FOO` would clobber each other on case-insensitive
        // filesystems, but `filename_id`'s `_H<hash>` suffix on
        // any-uppercase ids gives all three distinct on-disk names.
        let foo_upper = build_sequence_path("lib.rs", "Foo");
        let foo_lower = build_sequence_path("lib.rs", "foo");
        let foo_screaming = build_sequence_path("lib.rs", "FOO");
        let mut names = [&foo_upper, &foo_lower, &foo_screaming];
        names.sort();
        assert_ne!(names[0], names[1], "got: {names:?}");
        assert_ne!(names[1], names[2], "got: {names:?}");
        // And after case-folding (the actual case-insensitive-FS check).
        let mut lowered: Vec<String> = names.iter().map(|n| n.to_ascii_lowercase()).collect();
        lowered.sort();
        lowered.dedup();
        assert_eq!(lowered.len(), 3, "case-fold collision: {lowered:?}");
    }
}
