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

/// Canonical sanitizer for Mermaid node IDs. Single source of truth — every
/// renderer (project / overview / module / function / impact + sequence)
/// goes through this function so the same input always produces the same
/// node ID across diagrams.
///
/// Contract:
/// - Output is `[A-Za-z0-9_]+`. Non-ASCII alphanumerics are replaced with
///   `_` (rejected); Mermaid's flowchart parser is unreliable on unicode
///   IDs in the wild, so we trade away that subset for byte-stable output.
/// - Empty input → `_`.
/// - Digit-leading IDs are prefixed with `_` because Mermaid's parser
///   reads a bare digit-leading token as a number, not an identifier.
/// - Reserved Mermaid keywords (case-insensitive: `graph`, `Graph`, `GRAPH`
///   all match) are prefixed with `n_` because using them as bare IDs
///   makes the parser interpret the line as a new diagram-type
///   declaration.
#[must_use]
pub fn sanitize_id(name: &str) -> String {
    if name.is_empty() {
        return "_".to_owned();
    }
    let mut sanitized = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            sanitized.push(c);
        } else {
            sanitized.push('_');
        }
    }
    if sanitized.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        let mut prefixed = String::with_capacity(sanitized.len() + 1);
        prefixed.push('_');
        prefixed.push_str(&sanitized);
        return prefixed;
    }
    if MERMAID_RESERVED
        .iter()
        .any(|r| sanitized.eq_ignore_ascii_case(r))
    {
        return format!("n_{sanitized}");
    }
    sanitized
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
    fn sanitize_id_keeps_alphanumeric_and_underscore() {
        assert_eq!(sanitize_id("foo_bar123"), "foo_bar123");
    }

    #[test]
    fn sanitize_id_replaces_punctuation() {
        assert_eq!(sanitize_id("foo-bar.rs"), "foo_bar_rs");
        assert_eq!(sanitize_id("crates/anatta-api"), "crates_anatta_api");
        assert_eq!(sanitize_id("a::b::c"), "a__b__c");
    }

    #[test]
    fn sanitize_id_handles_empty() {
        assert_eq!(sanitize_id(""), "_");
    }

    #[test]
    fn sanitize_id_rejects_unicode() {
        // Documented contract: non-ASCII alphanumerics are *replaced* with
        // `_`, not preserved. Mermaid's flowchart parser is iffy on unicode
        // IDs in the wild and we want byte-stable output.
        assert_eq!(sanitize_id("café"), "caf_");
        assert_eq!(sanitize_id("ε-greedy"), "__greedy");
    }

    #[test]
    fn sanitize_id_prefixes_digit_leading() {
        // `3things` would be parsed as a number by Mermaid; the `_` prefix
        // forces it back into identifier territory while keeping the
        // alphanumeric+`_` invariant.
        assert_eq!(sanitize_id("3things"), "_3things");
        assert_eq!(sanitize_id("42"), "_42");
        // Already-prefixed digit-leading is unchanged.
        assert_eq!(sanitize_id("_3things"), "_3things");
    }

    #[test]
    fn sanitize_id_escapes_reserved_keywords() {
        // `graph` as an ID makes the Mermaid parser see a nested diagram
        // declaration. Prefix with `n_` to keep it syntactically a node.
        assert_eq!(sanitize_id("graph"), "n_graph");
        assert_eq!(sanitize_id("subgraph"), "n_subgraph");
        assert_eq!(sanitize_id("end"), "n_end");
        assert_eq!(sanitize_id("flowchart"), "n_flowchart");
        assert_eq!(sanitize_id("classDef"), "n_classDef");
        assert_eq!(sanitize_id("style"), "n_style");
        assert_eq!(sanitize_id("click"), "n_click");
        // Sanitized-but-not-equal-to-reserved still needs no escape.
        assert_eq!(sanitize_id("graph!"), "graph_");
        assert_eq!(sanitize_id("graphical"), "graphical");
    }

    #[test]
    fn sanitize_id_reserved_keyword_check_is_case_insensitive() {
        // Mermaid's parser is case-insensitive on diagram-type keywords —
        // `Graph`, `GRAPH`, `gRaPh` all clash. Match the parser.
        assert_eq!(sanitize_id("Graph"), "n_Graph");
        assert_eq!(sanitize_id("GRAPH"), "n_GRAPH");
        assert_eq!(sanitize_id("Subgraph"), "n_Subgraph");
        assert_eq!(sanitize_id("END"), "n_END");
    }

    #[test]
    fn sanitize_id_does_not_escape_substring_matches() {
        // Only exact reserved-word matches get the prefix; substrings are fine.
        assert_eq!(sanitize_id("graphics"), "graphics");
        assert_eq!(sanitize_id("subgraph_inner"), "subgraph_inner");
        assert_eq!(sanitize_id("my_graph"), "my_graph");
        assert_eq!(sanitize_id("ending"), "ending");
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
