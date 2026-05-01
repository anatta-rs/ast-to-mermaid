//! Artifact bundle producer — writes the 4-layer format consumed by
//! `mermaid-graph project --artifacts <dir>`.
//!
//! Layout (mirrors `mermaid_graph::artifacts::load_artifact_dir`) :
//!
//! ```text
//! <out>/
//!   overview.mmd
//!   project.mmd
//!   index.json
//!   entities/
//!     <sanitized-id>.mmd
//!     <sanitized-id>.meta.json
//! ```
//!
//! `sanitize_id` MUST stay byte-identical with mermaid-graph's loader,
//! otherwise the round-trip breaks. The shared rule : map every
//! character outside `[A-Za-z0-9._-]` to `_`. Tests guard the parity.
//!
//! Schema version : `index.json["schema_version"] == 1`. Future
//! field additions are backward-compatible (consumer ignores unknown
//! fields). Breaking changes bump the version.

use crate::error::{AstToMermaidError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

// ── Public types ─────────────────────────────────────────────────────────────

/// One entity's artifact pair : `.mmd` + `.meta.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntityArtifact {
    /// Canonical entity id (e.g. `code:src/lib.rs::function::foo`).
    pub id: String,
    /// Mermaid source for this entity, headed by `%% id:`, `%% kind:`,
    /// `%% file:` comments so the file is self-describing.
    pub mmd: String,
    /// Per-entity metadata. Schema documented in
    /// `docs/design/2026-05-01-artifact-bundle.md`.
    pub meta: Value,
}

/// Full artifact bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactDir {
    /// Content of `overview.mmd`.
    pub overview_mmd: String,
    /// Content of `project.mmd`.
    pub project_mmd: String,
    /// Parsed `index.json`.
    pub index_json: Value,
    /// Per-entity artifacts, sorted by id for determinism.
    pub entities: Vec<EntityArtifact>,
}

// ── sanitize_id ──────────────────────────────────────────────────────────────

/// Map an arbitrary entity id to a safe filename stem.
///
/// Keep `[A-Za-z0-9._-]` ; everything else becomes `_`. Must stay
/// byte-identical with `mermaid_graph::artifacts::sanitize_id` —
/// otherwise loaders won't find the files we wrote.
#[must_use]
pub fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ── Write ────────────────────────────────────────────────────────────────────

/// Write `artifacts` to `out_dir` in the canonical 4-layer layout.
///
/// Creates `out_dir` and `out_dir/entities/` if missing. Files are
/// pretty-printed JSON for human-friendly diffs ; the parser ignores
/// whitespace.
///
/// # Errors
///
/// - [`AstToMermaidError::Io`] for any filesystem failure.
/// - [`AstToMermaidError::InvalidInput`] if a meta json fails to
///   serialize (would only happen if the caller stuffed a non-encodable
///   `Value` in there — should be unreachable in practice).
pub fn write_artifacts(artifacts: &ArtifactDir, out_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(out_dir)
        .map_err(|e| AstToMermaidError::Io(format!("create {}: {e}", out_dir.display())))?;
    let entities_dir = out_dir.join("entities");
    std::fs::create_dir_all(&entities_dir)
        .map_err(|e| AstToMermaidError::Io(format!("create {}: {e}", entities_dir.display())))?;

    write_text(&out_dir.join("overview.mmd"), &artifacts.overview_mmd)?;
    write_text(&out_dir.join("project.mmd"), &artifacts.project_mmd)?;
    let pretty = serde_json::to_string_pretty(&artifacts.index_json)
        .map_err(|e| AstToMermaidError::InvalidInput(format!("serialize index.json: {e}")))?;
    write_text(&out_dir.join("index.json"), &pretty)?;

    for entity in &artifacts.entities {
        let base = sanitize_id(&entity.id);
        write_text(&entities_dir.join(format!("{base}.mmd")), &entity.mmd)?;
        let meta_pretty = serde_json::to_string_pretty(&entity.meta).map_err(|e| {
            AstToMermaidError::InvalidInput(format!("serialize meta.json for {}: {e}", entity.id))
        })?;
        write_text(
            &entities_dir.join(format!("{base}.meta.json")),
            &meta_pretty,
        )?;
    }

    Ok(())
}

fn write_text(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text)
        .map_err(|e| AstToMermaidError::Io(format!("write {}: {e}", path.display())))
}

// ── Load (round-trip primitive) ──────────────────────────────────────────────

/// Read a bundle written by [`write_artifacts`] back into an
/// [`ArtifactDir`]. Useful for round-trip tests and for tools that
/// want to inspect a bundle without re-running the analyzer.
///
/// Mirrors `mermaid_graph::artifacts::load_artifact_dir`'s behaviour
/// — same error types (missing overview / index, bad JSON), same
/// sort order (entities sorted by id).
///
/// # Errors
///
/// - [`AstToMermaidError::ArtifactNotFound`] if `overview.mmd` /
///   `index.json` / a referenced `meta.json` is missing.
/// - [`AstToMermaidError::Io`] for other I/O failures.
/// - [`AstToMermaidError::InvalidInput`] if a JSON file is malformed
///   or a `meta.json` lacks the required `id` field.
pub fn load_artifact_dir(dir: &Path) -> Result<ArtifactDir> {
    let overview_path = dir.join("overview.mmd");
    let project_path = dir.join("project.mmd");
    let index_path = dir.join("index.json");

    if !overview_path.exists() {
        return Err(AstToMermaidError::ArtifactNotFound(
            overview_path.display().to_string(),
        ));
    }
    if !index_path.exists() {
        return Err(AstToMermaidError::ArtifactNotFound(
            index_path.display().to_string(),
        ));
    }

    let overview_mmd = read_text(&overview_path)?;
    // project.mmd is optional in older bundles ; default to empty so
    // the round-trip accepts mermaid-graph fixtures that pre-date this
    // field.
    let project_mmd = if project_path.exists() {
        read_text(&project_path)?
    } else {
        String::new()
    };
    let index_raw = read_text(&index_path)?;
    let index_json: Value = serde_json::from_str(&index_raw).map_err(|e| {
        AstToMermaidError::InvalidInput(format!("parse {}: {e}", index_path.display()))
    })?;

    let entities_dir = dir.join("entities");
    let mut entities: Vec<EntityArtifact> = Vec::new();
    if entities_dir.is_dir() {
        let read_dir = std::fs::read_dir(&entities_dir)
            .map_err(|e| AstToMermaidError::Io(format!("read {}: {e}", entities_dir.display())))?;
        let mut mmd_paths: Vec<std::path::PathBuf> = Vec::new();
        for entry in read_dir {
            let entry = entry.map_err(|e| AstToMermaidError::Io(format!("walk entities/: {e}")))?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("mmd") {
                mmd_paths.push(path);
            }
        }
        mmd_paths.sort();

        for mmd_path in mmd_paths {
            let stem = mmd_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_owned();
            let meta_path = entities_dir.join(format!("{stem}.meta.json"));
            if !meta_path.exists() {
                return Err(AstToMermaidError::ArtifactNotFound(
                    meta_path.display().to_string(),
                ));
            }
            let mmd = read_text(&mmd_path)?;
            let meta_raw = read_text(&meta_path)?;
            let meta: Value = serde_json::from_str(&meta_raw).map_err(|e| {
                AstToMermaidError::InvalidInput(format!("parse {}: {e}", meta_path.display()))
            })?;
            let id = meta
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    AstToMermaidError::InvalidInput(format!(
                        "{}: missing `id` field",
                        meta_path.display()
                    ))
                })?
                .to_owned();
            entities.push(EntityArtifact { id, mmd, meta });
        }
    }
    entities.sort_by(|a, b| a.id.cmp(&b.id));

    Ok(ArtifactDir {
        overview_mmd,
        project_mmd,
        index_json,
        entities,
    })
}

fn read_text(path: &Path) -> Result<String> {
    std::fs::read_to_string(path)
        .map_err(|e| AstToMermaidError::Io(format!("read {}: {e}", path.display())))
}

// ── Index helpers ────────────────────────────────────────────────────────────

/// Build the canonical `index.json` body. Pure ; consumers feed it
/// the entity list + stats and the version is stamped in.
#[must_use]
pub fn build_index(
    entities: &[Value],
    source_root: &str,
    files_parsed: usize,
    atoms_indexed: usize,
    edges_resolved: usize,
) -> Value {
    let now = chrono_lite_now_utc();
    json!({
        "schema_version": 1,
        "generated_at": now,
        "ast_to_mermaid_version": env!("CARGO_PKG_VERSION"),
        "source_root": source_root,
        "stats": {
            "files_parsed": files_parsed,
            "atoms_indexed": atoms_indexed,
            "edges_resolved": edges_resolved,
        },
        "entities": entities,
    })
}

/// RFC-3339 UTC timestamp as a string, hand-rolled to avoid a `chrono`
/// dependency just for `now`. Format : `YYYY-MM-DDTHH:MM:SSZ`.
fn chrono_lite_now_utc() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // Days since 1970 ; pull out date + time. Good enough for an
    // artifact timestamp ; full chrono is overkill here.
    let days = secs / 86_400;
    let time_of_day = secs % 86_400;
    let hour = time_of_day / 3600;
    let minute = (time_of_day / 60) % 60;
    let second = time_of_day % 60;

    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Convert Unix epoch days to (year, month, day). Civil-from-days
/// algorithm by Howard Hinnant, public domain. Handles dates from
/// 1970-01-01 forward.
fn days_to_ymd(days: u64) -> (i64, u64, u64) {
    let z = i64::try_from(days).unwrap_or(i64::MAX) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = u64::try_from(z.rem_euclid(146_097)).unwrap_or(0);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = i64::try_from(yoe).unwrap_or(0) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    // ── sanitize_id ──────────────────────────────────────────────────────────

    #[test]
    fn sanitize_id_keeps_alphanumeric_and_safe_punctuation() {
        assert_eq!(sanitize_id("abc_123-def.rs"), "abc_123-def.rs");
    }

    #[test]
    fn sanitize_id_replaces_colons_and_slashes_with_underscore() {
        assert_eq!(
            sanitize_id("code:src/lib.rs::function::foo"),
            "code_src_lib.rs__function__foo"
        );
    }

    #[test]
    fn sanitize_id_replaces_unicode_with_underscore() {
        // Non-ASCII chars all collapse to `_`.
        assert_eq!(sanitize_id("café"), "caf_");
    }

    #[test]
    fn sanitize_id_handles_empty_string() {
        assert_eq!(sanitize_id(""), "");
    }

    /// The producer side (this crate) and the consumer side
    /// (mermaid-graph) MUST agree byte-identically. This test pins the
    /// rule for the producer ; the consumer has its own equivalent
    /// test. If either drifts, the bundle round-trip breaks.
    #[test]
    fn sanitize_id_is_byte_identical_with_documented_rule() {
        for (input, expected) in &[
            (
                "code:src/lib.rs::function::foo",
                "code_src_lib.rs__function__foo",
            ),
            ("a/b/c", "a_b_c"),
            ("hello world", "hello_world"),
            ("UPPER_lower-123", "UPPER_lower-123"),
            ("dot.in.path", "dot.in.path"),
        ] {
            assert_eq!(sanitize_id(input), *expected, "input: {input}");
        }
    }

    // ── build_index ──────────────────────────────────────────────────────────

    #[test]
    fn build_index_stamps_schema_version_and_stats() {
        let idx = build_index(&[], "/tmp/repo", 5, 42, 7);
        assert_eq!(idx["schema_version"], json!(1));
        assert_eq!(idx["source_root"], json!("/tmp/repo"));
        assert_eq!(idx["stats"]["files_parsed"], json!(5));
        assert_eq!(idx["stats"]["atoms_indexed"], json!(42));
        assert_eq!(idx["stats"]["edges_resolved"], json!(7));
        assert_eq!(idx["entities"], json!([]));
        assert!(idx.get("generated_at").is_some(), "must stamp timestamp");
        assert!(
            idx.get("ast_to_mermaid_version").is_some(),
            "must record producer version"
        );
    }

    #[test]
    fn build_index_carries_entity_list() {
        let entities = vec![json!({
            "id": "code:src/lib.rs::function::foo",
            "kind": "function",
            "name": "foo",
        })];
        let idx = build_index(&entities, "/r", 1, 1, 0);
        assert_eq!(
            idx["entities"][0]["id"],
            json!("code:src/lib.rs::function::foo")
        );
    }

    #[test]
    fn generated_at_uses_rfc3339_zulu_format() {
        let s = chrono_lite_now_utc();
        // YYYY-MM-DDTHH:MM:SSZ — 20 chars, fixed shape.
        assert_eq!(s.len(), 20);
        assert!(s.ends_with('Z'));
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
        assert_eq!(&s[10..11], "T");
        assert_eq!(&s[13..14], ":");
        assert_eq!(&s[16..17], ":");
    }

    #[test]
    fn days_to_ymd_recognises_unix_epoch() {
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
    }

    #[test]
    fn days_to_ymd_handles_leap_year_boundary() {
        // 2024-03-01 is day 19_783 since epoch (2024 is a leap year).
        // Just check the round-trip self-consistency.
        let (y, m, d) = days_to_ymd(19_783);
        assert_eq!((y, m, d), (2024, 3, 1));
    }

    // ── write + load round-trip ──────────────────────────────────────────────

    fn fixture_artifacts() -> ArtifactDir {
        let entities = vec![
            EntityArtifact {
                id: "code:src/lib.rs::function::foo".to_owned(),
                mmd: "%% id: code:src/lib.rs::function::foo\n%% kind: function\ngraph TD\n    foo[\"foo\"]\n".to_owned(),
                meta: json!({
                    "id": "code:src/lib.rs::function::foo",
                    "kind": "function",
                    "name": "foo",
                    "file": "src/lib.rs",
                    "line_start": 1,
                    "line_end": 5,
                    "signature": "pub fn foo()",
                    "doc": "",
                    "content_hash": "h1",
                    "callers": [],
                    "callees": [],
                    "children": [],
                    "imports": [],
                    "imported_by": [],
                }),
            },
            EntityArtifact {
                id: "code:src/main.rs::function::main".to_owned(),
                mmd: "%% id: code:src/main.rs::function::main\n%% kind: function\ngraph TD\n    main[\"main\"]\n".to_owned(),
                meta: json!({
                    "id": "code:src/main.rs::function::main",
                    "kind": "function",
                    "name": "main",
                    "file": "src/main.rs",
                    "line_start": 1,
                    "line_end": 3,
                    "signature": "fn main()",
                    "doc": "",
                    "content_hash": "h2",
                    "callers": [],
                    "callees": [],
                    "children": [],
                    "imports": [],
                    "imported_by": [],
                }),
            },
        ];
        let index_entities: Vec<Value> = entities
            .iter()
            .map(|e| {
                json!({
                    "id": e.id,
                    "kind": e.meta["kind"],
                    "name": e.meta["name"],
                    "file": e.meta["file"],
                    "mmd_path": format!("entities/{}.mmd", sanitize_id(&e.id)),
                    "meta_path": format!("entities/{}.meta.json", sanitize_id(&e.id)),
                    "edges": {"out": [], "in": []},
                })
            })
            .collect();
        ArtifactDir {
            overview_mmd: "graph TD\n    src[\"src — 1 mod\"]\n".to_owned(),
            project_mmd: "graph TD\n    repo[\"repo — 1 mod\"]\n".to_owned(),
            index_json: build_index(&index_entities, "/tmp/repo", 2, 4, 0),
            entities,
        }
    }

    #[test]
    fn write_then_load_round_trip_preserves_struct() {
        let dir = tempdir().expect("tmp");
        let original = fixture_artifacts();

        write_artifacts(&original, dir.path()).expect("write");
        let loaded = load_artifact_dir(dir.path()).expect("load");

        assert_eq!(loaded.overview_mmd, original.overview_mmd);
        assert_eq!(loaded.project_mmd, original.project_mmd);
        assert_eq!(loaded.entities.len(), original.entities.len());
        for (a, b) in loaded.entities.iter().zip(original.entities.iter()) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.mmd, b.mmd);
            assert_eq!(a.meta, b.meta);
        }
        // index_json may differ in `generated_at` if the function is
        // called twice — rebuild() not invoked in load_artifact_dir,
        // so just assert structural fields are intact.
        assert_eq!(
            loaded.index_json["schema_version"],
            original.index_json["schema_version"]
        );
        assert_eq!(
            loaded.index_json["entities"].as_array().map(Vec::len),
            original.index_json["entities"].as_array().map(Vec::len),
        );
    }

    #[test]
    fn write_creates_entities_dir_with_sanitized_filenames() {
        let dir = tempdir().expect("tmp");
        let artifacts = fixture_artifacts();
        write_artifacts(&artifacts, dir.path()).expect("write");

        // Both .mmd and .meta.json land at sanitized stems.
        let foo_stem = sanitize_id("code:src/lib.rs::function::foo");
        assert!(
            dir.path()
                .join(format!("entities/{foo_stem}.mmd"))
                .is_file(),
            "expected sanitized .mmd path"
        );
        assert!(
            dir.path()
                .join(format!("entities/{foo_stem}.meta.json"))
                .is_file(),
            "expected sanitized .meta.json path"
        );
    }

    #[test]
    fn load_artifact_dir_missing_overview_errors() {
        let dir = tempdir().expect("tmp");
        let result = load_artifact_dir(dir.path());
        assert!(matches!(
            result,
            Err(AstToMermaidError::ArtifactNotFound(_))
        ));
    }

    #[test]
    fn load_artifact_dir_missing_index_errors() {
        let dir = tempdir().expect("tmp");
        // Write only overview.mmd, no index.json.
        std::fs::write(dir.path().join("overview.mmd"), "graph TD\n").expect("write overview");
        let result = load_artifact_dir(dir.path());
        assert!(matches!(
            result,
            Err(AstToMermaidError::ArtifactNotFound(_))
        ));
    }

    #[test]
    fn load_artifact_dir_treats_missing_project_mmd_as_empty() {
        // Older bundles (pre-PR-1) may not carry project.mmd.
        let dir = tempdir().expect("tmp");
        std::fs::write(dir.path().join("overview.mmd"), "graph TD\n").expect("write");
        std::fs::write(
            dir.path().join("index.json"),
            r#"{"schema_version":1,"entities":[]}"#,
        )
        .expect("write");
        let loaded = load_artifact_dir(dir.path()).expect("load");
        assert_eq!(loaded.project_mmd, "");
    }

    #[test]
    fn load_artifact_dir_missing_meta_json_errors() {
        let dir = tempdir().expect("tmp");
        std::fs::write(dir.path().join("overview.mmd"), "graph TD\n").expect("write");
        std::fs::write(
            dir.path().join("index.json"),
            r#"{"schema_version":1,"entities":[]}"#,
        )
        .expect("write");
        std::fs::create_dir_all(dir.path().join("entities")).expect("mkdir");
        // Write an .mmd without its sibling .meta.json — should error.
        std::fs::write(dir.path().join("entities/orphan.mmd"), "graph TD\n").expect("write");
        let result = load_artifact_dir(dir.path());
        assert!(matches!(
            result,
            Err(AstToMermaidError::ArtifactNotFound(_))
        ));
    }

    #[test]
    fn load_artifact_dir_meta_without_id_field_errors() {
        let dir = tempdir().expect("tmp");
        std::fs::write(dir.path().join("overview.mmd"), "graph TD\n").expect("write");
        std::fs::write(
            dir.path().join("index.json"),
            r#"{"schema_version":1,"entities":[]}"#,
        )
        .expect("write");
        std::fs::create_dir_all(dir.path().join("entities")).expect("mkdir");
        std::fs::write(dir.path().join("entities/x.mmd"), "graph TD\n").expect("write");
        // meta.json without "id" field.
        std::fs::write(
            dir.path().join("entities/x.meta.json"),
            r#"{"kind":"function"}"#,
        )
        .expect("write");
        let result = load_artifact_dir(dir.path());
        assert!(
            matches!(result, Err(AstToMermaidError::InvalidInput(_))),
            "expected InvalidInput, got {result:?}"
        );
    }

    #[test]
    fn load_artifact_dir_sorts_entities_by_id() {
        let dir = tempdir().expect("tmp");
        let mut artifacts = fixture_artifacts();
        // Reverse the order in memory ; load_artifact_dir should still
        // return them sorted by id.
        artifacts.entities.reverse();
        write_artifacts(&artifacts, dir.path()).expect("write");
        let loaded = load_artifact_dir(dir.path()).expect("load");
        assert_eq!(loaded.entities[0].id, "code:src/lib.rs::function::foo");
        assert_eq!(loaded.entities[1].id, "code:src/main.rs::function::main");
    }
}
