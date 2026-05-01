//! Mermaid rendering primitives shared by every level.

/// Mermaid keywords that the parser refuses as node ids — using any of these
/// as a bare identifier (e.g. `graph["label"]`) raises a parse error because
/// the keyword is consumed by surrounding grammar (diagram-type declarations,
/// subgraph blocks, classDef/style statements, etc.).
///
/// Sourced from the mermaid-js flowchart grammar
/// (`packages/mermaid/src/diagrams/flowchart/parser/flow.jison`). When in
/// doubt, prefer to suffix — false positives only cost a `_`, false negatives
/// produce a broken diagram.
const MERMAID_RESERVED: &[&str] = &[
    "graph",
    "flowchart",
    "subgraph",
    "end",
    "direction",
    "classDef",
    "class",
    "style",
    "linkStyle",
    "click",
    "default",
    "interpolate",
    "accTitle",
    "accDescr",
];

/// Sanitize a name so it can be used as a Mermaid node ID.
///
/// Mermaid IDs must be alphanumeric or `_`. Any other character is replaced
/// with `_`. Empty input becomes a single `_` to keep IDs syntactically valid.
/// Reserved Mermaid keywords get a trailing `_` to avoid grammar collisions
/// (e.g. a module named `graph` becomes `graph_`).
#[must_use]
pub fn mermaid_id(name: &str) -> String {
    if name.is_empty() {
        return "_".to_owned();
    }
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if MERMAID_RESERVED.contains(&sanitized.as_str()) {
        return format!("{sanitized}_");
    }
    sanitized
}

/// Escape a string for safe inclusion in a Mermaid node label (the `[".."]`
/// part). Replaces `"` with `&quot;` so the label string isn't truncated.
#[must_use]
pub fn escape_label(s: &str) -> String {
    s.replace('"', "&quot;")
}

/// Best-effort crate-root extraction from a file path.
///
/// `crates/foo/src/lib.rs` → `foo` (the segment after `crates/`).
/// Falls back to the path's leading segment for non-`crates/` layouts.
#[must_use]
pub fn crate_name(file_path: &str) -> &str {
    if let Some(rest) = file_path.strip_prefix("crates/") {
        rest.split('/').next().unwrap_or(file_path)
    } else {
        file_path.split('/').next().unwrap_or(file_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mermaid_id_keeps_alphanumeric_and_underscore() {
        assert_eq!(mermaid_id("foo_bar123"), "foo_bar123");
    }

    #[test]
    fn mermaid_id_replaces_punctuation() {
        assert_eq!(mermaid_id("foo-bar.rs"), "foo_bar_rs");
        assert_eq!(mermaid_id("crates/anatta-api"), "crates_anatta_api");
        assert_eq!(mermaid_id("a::b::c"), "a__b__c");
    }

    #[test]
    fn mermaid_id_handles_empty() {
        assert_eq!(mermaid_id(""), "_");
    }

    #[test]
    fn mermaid_id_handles_unicode() {
        // is_alphanumeric is unicode-aware
        assert_eq!(mermaid_id("café"), "café");
        assert_eq!(mermaid_id("ε-greedy"), "ε_greedy");
    }

    #[test]
    fn mermaid_id_suffixes_reserved_keywords() {
        // The bug that motivated the reserved-keyword guard: a module called
        // `graph` rendered as `graph["graph — 2 mod, 0 fn, 2 struct"]` clashed
        // with the `graph TD` declaration on line 1 and crashed the parser.
        assert_eq!(mermaid_id("graph"), "graph_");
        assert_eq!(mermaid_id("flowchart"), "flowchart_");
        assert_eq!(mermaid_id("subgraph"), "subgraph_");
        assert_eq!(mermaid_id("end"), "end_");
        assert_eq!(mermaid_id("classDef"), "classDef_");
        assert_eq!(mermaid_id("style"), "style_");
        assert_eq!(mermaid_id("click"), "click_");
    }

    #[test]
    fn mermaid_id_does_not_suffix_substring_matches() {
        // Only exact reserved-word matches get the suffix; substrings are fine.
        assert_eq!(mermaid_id("graphics"), "graphics");
        assert_eq!(mermaid_id("subgraph_inner"), "subgraph_inner");
        assert_eq!(mermaid_id("my_graph"), "my_graph");
        assert_eq!(mermaid_id("ending"), "ending");
    }

    #[test]
    fn mermaid_id_collision_via_punctuation_replacement_also_suffixed() {
        // Inputs that *become* a reserved keyword after punctuation replacement
        // must still be suffixed.
        assert_eq!(mermaid_id("graph!"), "graph_"); // `!` → `_`, but the result `graph_` is fine
        // …but check the trickier case: a name that sanitizes to bare `graph`.
        // Actually any sanitized form ending in `_` won't collide — only a bare
        // name like "graph" or a name like "Graph" (which stays alphanumeric)
        // is at risk. Verify Graph stays as Graph (case-sensitive list).
        assert_eq!(mermaid_id("Graph"), "Graph");
    }

    #[test]
    fn escape_label_replaces_quotes() {
        assert_eq!(escape_label("foo"), "foo");
        assert_eq!(escape_label(r#"a "b" c"#), "a &quot;b&quot; c");
        assert_eq!(escape_label(""), "");
    }

    #[test]
    fn crate_name_under_crates_dir() {
        assert_eq!(crate_name("crates/anatta-api/src/lib.rs"), "anatta-api");
        assert_eq!(crate_name("crates/foo/src/mod_a.rs"), "foo");
    }

    #[test]
    fn crate_name_outside_crates_dir() {
        assert_eq!(crate_name("src/lib.rs"), "src");
        assert_eq!(crate_name("just_a_file.rs"), "just_a_file.rs");
    }

    #[test]
    fn crate_name_empty() {
        assert_eq!(crate_name(""), "");
    }
}
