//! Mermaid rendering primitives shared by every level.

/// Words Mermaid's flowchart grammar treats as reserved tokens; using one
/// as a node identifier triggers a parse error like
/// `Expecting ..., got 'GRAPH'`. We escape any clash by prefixing with
/// `n_` so renaming is mechanical and reversible by inspection.
const MERMAID_RESERVED: &[&str] = &[
    "graph",
    "subgraph",
    "end",
    "flowchart",
    "direction",
    "click",
    "class",
    "classDef",
    "linkStyle",
    "style",
    "default",
    "interpolate",
    "accTitle",
    "accDescr",
];

/// Sanitize a name so it can be used as a Mermaid node ID.
///
/// Mermaid IDs must be alphanumeric or `_`. Any other character is replaced
/// with `_`. Empty input becomes a single `_` to keep IDs syntactically valid.
/// Reserved Mermaid keywords (e.g. `graph`, `subgraph`, `end`) are prefixed
/// with `n_` because using them as bare IDs makes the parser interpret the
/// node line as a new diagram-type declaration.
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
        format!("n_{sanitized}")
    } else {
        sanitized
    }
}

/// Escape a string for safe inclusion in a Mermaid node label (the `[".."]`
/// part).
///
/// - `"` → `&quot;` so the label string isn't truncated.
/// - `]` → `&rsqb;` so the bracket notation isn't escaped early. This matters
///   for synthetic identifiers (proc-macro output, Python decorators, generic
///   instantiations) that contain `]`.
/// - newlines → space so multi-line signatures don't break Mermaid parsing.
#[must_use]
pub fn escape_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("&quot;"),
            ']' => out.push_str("&rsqb;"),
            '\n' | '\r' => out.push(' '),
            other => out.push(other),
        }
    }
    out
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
    fn mermaid_id_escapes_reserved_keywords() {
        // `graph` as an ID makes the Mermaid parser see a nested diagram
        // declaration. Prefix with `n_` to keep it syntactically a node.
        assert_eq!(mermaid_id("graph"), "n_graph");
        assert_eq!(mermaid_id("subgraph"), "n_subgraph");
        assert_eq!(mermaid_id("end"), "n_end");
        assert_eq!(mermaid_id("flowchart"), "n_flowchart");
        assert_eq!(mermaid_id("classDef"), "n_classDef");
        assert_eq!(mermaid_id("style"), "n_style");
        assert_eq!(mermaid_id("click"), "n_click");
        // Sanitized-but-not-equal-to-reserved still needs no escape.
        assert_eq!(mermaid_id("graph!"), "graph_");
        assert_eq!(mermaid_id("graphical"), "graphical");
    }

    #[test]
    fn mermaid_id_does_not_escape_substring_matches() {
        // Only exact reserved-word matches get the prefix; substrings are fine.
        assert_eq!(mermaid_id("graphics"), "graphics");
        assert_eq!(mermaid_id("subgraph_inner"), "subgraph_inner");
        assert_eq!(mermaid_id("my_graph"), "my_graph");
        assert_eq!(mermaid_id("ending"), "ending");
        // Case-sensitive: `Graph` is not in the reserved list.
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
