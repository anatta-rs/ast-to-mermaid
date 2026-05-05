use super::check_ref_arg;
use crate::cli::flags::{ExitCode, SequenceFlags};
use crate::cli::format::parse_csv_exclude;
use crate::pipeline::walk_for_languages_with_exclude;
use crate::sequence;
use std::path::Path;

/// Run the `sequence` subcommand. Two modes:
///
/// - Single-target (`--target <name>`): locate one function, render its
///   body to stdout or `--out <FILE>`.
/// - All (`--all`, requires `--out <DIR>`): every Rust function in the
///   source tree is rendered into its own `<DIR>/<file>__<name>.mmd`.
pub fn run_sequence(flags: &SequenceFlags) -> ExitCode {
    if !flags.all && flags.target.as_deref().is_none_or(|t| t.trim().is_empty()) {
        eprintln!("sequence: pass --target <NAME> or --all");
        return ExitCode::UsageError;
    }
    if let Err(code) = check_ref_arg("sequence", flags.r#ref.as_deref()) {
        return code;
    }
    let exclude = parse_csv_exclude(&flags.exclude);

    let candidates = match collect_rust_sources(&flags.path, &exclude, flags.r#ref.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("sequence: collect sources {}: {e}", flags.path.display());
            return ExitCode::Failure;
        }
    };
    if candidates.is_empty() {
        eprintln!(
            "sequence: no Rust files found under {}",
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
    candidates: &[(String, Vec<u8>)],
    target: &str,
    out: Option<&Path>,
    path: &Path,
) -> ExitCode {
    // Parse each file at most once: a single `parse_source_once` feeds
    // both `list_functions_in_tree` (does the file declare `target`?)
    // and `extract_all` (extract the diagram on the same tree). Once a
    // match is found we break — files past it are never parsed.
    let mut diagram = None;
    for (file_rel, content) in candidates {
        let Ok(text) = std::str::from_utf8(content) else {
            continue;
        };
        let Ok(tree) = sequence::parse_source_once(content, file_rel) else {
            continue;
        };
        let names = sequence::list_functions_in_tree(&tree, text);
        if names.iter().any(|n| n == target) {
            let mut map = sequence::extract_all(&tree, text, &[target]);
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

fn run_sequence_all(candidates: &[(String, Vec<u8>)], out: Option<&Path>) -> ExitCode {
    let Some(out_dir) = out else {
        eprintln!("sequence: --all requires --out <DIR>");
        return ExitCode::UsageError;
    };
    if let Err(e) = std::fs::create_dir_all(out_dir) {
        eprintln!("sequence: create {}: {e}", out_dir.display());
        return ExitCode::Failure;
    }

    // Pass 1: parse each file exactly once, enumerate functions on the
    // resulting tree, then `extract_all` to collect every diagram in a
    // single AST traversal. Pre-v0.6.0 this re-parsed each file 1+N
    // times (once for `list_functions`, once per function for `extract`).
    let mut entries: Vec<(String, String, String, sequence::SequenceDiagram)> = Vec::new();
    let mut skipped = 0usize;
    for (file_rel, content) in candidates {
        let text = match std::str::from_utf8(content) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("sequence: parse {file_rel}: {e}");
                skipped += 1;
                continue;
            }
        };
        let tree = match sequence::parse_source_once(content, file_rel) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("sequence: parse {file_rel}: {e}");
                skipped += 1;
                continue;
            }
        };
        let names = sequence::list_functions_in_tree(&tree, text);
        let target_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut map = sequence::extract_all(&tree, text, &target_refs);
        for name in names {
            let Some(diagram) = map.remove(&name) else {
                skipped += 1;
                continue;
            };
            if diagram.steps.is_empty() {
                skipped += 1;
                continue;
            }
            let base = sequence_filename(file_rel, &name);
            entries.push((file_rel.clone(), name, base, diagram));
        }
    }

    // Detect pre-collisions on case-insensitive filesystems (macOS APFS
    // default, Windows NTFS) before any file is written.
    let mut lower_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (_, _, base, _) in &entries {
        *lower_counts.entry(base.to_ascii_lowercase()).or_insert(0) += 1;
    }

    // Pass 2: render + write. Bases that case-fold to the same lowercase
    // as another candidate get a `_H<hash>` suffix from
    // [`crate::artifacts::hash_disambig`] so all members survive on disk.
    let mut written = 0usize;
    for (file_rel, name, base, diagram) in &entries {
        let collides = lower_counts
            .get(&base.to_ascii_lowercase())
            .copied()
            .unwrap_or(0)
            > 1;
        let final_name = if collides {
            let stem = base.strip_suffix(".mmd").unwrap_or(base);
            let key = format!("{file_rel}::{name}");
            format!(
                "{stem}_H{hash}.mmd",
                hash = crate::artifacts::hash_disambig(&key)
            )
        } else {
            base.clone()
        };
        let target_path = out_dir.join(&final_name);
        let rendered = sequence::render(diagram);
        if let Err(e) = std::fs::write(&target_path, rendered) {
            eprintln!("sequence: write {}: {e}", target_path.display());
            return ExitCode::Failure;
        }
        written += 1;
    }
    eprintln!(
        "sequence --all: {written} diagrams written to {} ({skipped} skipped: empty / parse fail)",
        out_dir.display(),
    );
    ExitCode::Success
}

/// Build a collision-resistant filename from `(file_rel, qualified_name)`:
/// `cli_support__run_diff.mmd`, `cache__Cache__open.mmd`, etc.
fn sequence_filename(file_rel: &str, qualified_name: &str) -> String {
    let stem = Path::new(file_rel)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let parent = Path::new(file_rel)
        .parent()
        .and_then(|p| p.to_str())
        .unwrap_or("");
    let prefix = if parent.is_empty() {
        stem.to_owned()
    } else {
        format!("{}_{stem}", parent.replace(['/', '\\'], "_"))
    };
    let safe_name: String = qualified_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{prefix}__{safe_name}.mmd")
}

/// Collect `(display_path, content)` pairs for every Rust source file under
/// `root`, honouring `exclude`. With `git_ref`, reads from `git ls-tree`
/// instead of the working tree.
fn collect_rust_sources(
    root: &Path,
    exclude: &[String],
    git_ref: Option<&str>,
) -> Result<Vec<(String, Vec<u8>)>, crate::error::AstToMermaidError> {
    if let Some(git_ref) = git_ref {
        let toplevel = crate::git_source::show_toplevel(root)?;
        let entries = crate::git_source::ls_tree(&toplevel, git_ref)?;
        // One persistent `git cat-file --batch` child amortises the
        // subprocess fork across every blob — a per-blob spawn is ~50x
        // slower on a 100-blob ref (50+s vs <1s).
        let mut reader = crate::git_source::BatchReader::spawn(&toplevel)?;
        let mut out = Vec::new();
        for entry in entries {
            if !Path::new(&entry.path)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("rs"))
            {
                continue;
            }
            let content = reader.read_blob(&entry.blob_sha)?;
            out.push((entry.path, content));
        }
        Ok(out)
    } else {
        let files = walk_for_languages_with_exclude(root, exclude)?;
        let mut out = Vec::new();
        for (path, lang) in files {
            if lang != crate::parser::Language::Rust {
                continue;
            }
            let content = std::fs::read(&path)?;
            let display = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            out.push((display, content));
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
        assert!(
            entries.iter().any(|n| n.contains("S__m")),
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
    fn sequence_filename_qualifies_methods() {
        let f = sequence_filename("src/cache.rs", "Cache::open");
        assert!(f.contains("cache"), "got: {f}");
        assert!(f.contains("Cache__open"), "got: {f}");
        assert!(
            Path::new(&f)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("mmd"))
        );
    }
}
