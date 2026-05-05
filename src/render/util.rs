//! Mermaid rendering primitives shared by every level.
//!
//! This module is the single source of truth for ID and label
//! sanitization. Every renderer (project / overview / module / function
//! / impact + sequence) and the artifact emitter goes through these
//! helpers so the same input always produces the same node ID across
//! diagrams. A previous iteration kept three divergent sanitizers
//! ([`crate::artifacts`]'s `mermaid_id_short` and
//! [`crate::sequence::render`]'s private `escape_label`) which silently
//! collapsed distinct entities to identical IDs — see C30.

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
/// and the artifact emitter goes through this function so the same input
/// always produces the same node ID across diagrams.
///
/// # Contract
/// - Output is `[A-Za-z0-9_]+`. Mermaid's flowchart parser is unreliable
///   on Unicode IDs in the wild, so we keep IDs strictly ASCII.
/// - Empty input → `_`.
/// - Digit-leading IDs are prefixed with `_` because Mermaid's parser
///   reads a bare digit-leading token as a number, not an identifier.
/// - Reserved Mermaid keywords (case-insensitive: `graph`, `Graph`,
///   `GRAPH` all match) are prefixed with `n_` because using them as
///   bare IDs makes the parser interpret the line as a new
///   diagram-type declaration.
/// - When the input contains *any* character outside
///   `[A-Za-z0-9_]`, an `_H<8-hex>` suffix derived from a fast hash of
///   the original input is appended. This guarantees that two distinct
///   inputs which fold to the same ASCII shape (e.g. `café` vs
///   `cafe_`, or `code:src/foo.rs::function::Foo` vs
///   `code_src_foo_rs__function__Foo`) get distinct node IDs. Inputs
///   that are already pure ASCII alphanumeric+`_` are returned
///   unchanged (modulo the digit / reserved-word guards).
///
/// # Examples
///
/// ```
/// use ast_to_mermaid::render::util::sanitize_id;
/// assert_eq!(sanitize_id("foo_bar123"), "foo_bar123");
/// assert!(sanitize_id("café").starts_with("caf__H"));
/// assert_ne!(sanitize_id("café"), sanitize_id("cafe_"));
/// assert_eq!(sanitize_id("3things"), "_3things");
/// assert_eq!(sanitize_id("graph"), "n_graph");
/// ```
#[must_use]
pub fn sanitize_id(name: &str) -> String {
    if name.is_empty() {
        return "_".to_owned();
    }
    let mut sanitized = String::with_capacity(name.len() + 10);
    let mut needs_hash = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            sanitized.push(c);
        } else {
            sanitized.push('_');
            needs_hash = true;
        }
    }
    if sanitized.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        sanitized.insert(0, '_');
    }
    if MERMAID_RESERVED
        .iter()
        .any(|r| sanitized.eq_ignore_ascii_case(r))
    {
        sanitized.insert_str(0, "n_");
    }
    if needs_hash {
        use std::fmt::Write as _;
        let _ = write!(sanitized, "_H{:08x}", hash_id_suffix(name));
    }
    sanitized
}

/// Deterministic 32-bit hash used to disambiguate
/// [`sanitize_id`]'s `_H<8-hex>` suffix.
///
/// Uses SHA-256 (already in our dependency tree) truncated to the first
/// four bytes. Cryptographic strength isn't required — we only need
/// distinct inputs to hit distinct suffixes with overwhelming
/// probability — but reusing the existing primitive avoids pulling
/// another hash crate in.
fn hash_id_suffix(input: &str) -> u32 {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(input.as_bytes());
    u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]])
}

/// Escape a string for safe inclusion in a Mermaid `flowchart` node label
/// (the `[".."]` part).
///
/// - `"` → `&quot;` so the label string isn't truncated.
/// - `]` → `&rsqb;` so the bracket notation isn't escaped early. This matters
///   for synthetic identifiers (proc-macro output, Python decorators, generic
///   instantiations) that contain `]`.
/// - newlines → space so multi-line signatures don't break Mermaid parsing.
///
/// Implemented as a single-pass char loop with `String::with_capacity` to
/// avoid the allocation churn of chained `String::replace` calls.
#[must_use]
pub fn escape_label_flowchart(s: &str) -> String {
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

/// Escape a string for safe inclusion in a Mermaid `sequenceDiagram`
/// message / participant / `alt` / `loop` / `Note` label.
///
/// `sequenceDiagram` parses labels differently from `flowchart` —
/// `<…>` is treated as inline HTML and an `alt` cond like `if x <= 0`
/// gets parsed as a malformed tag and lays out vertically. `#` is
/// reserved for Mermaid HTML entity references inside labels. Newlines
/// would split the label across lines and break the diagram entirely.
///
/// - `<` → `&lt;`, `>` → `&gt;` so HTML-like fragments render as text.
/// - `#` → `_` so HTML entity refs aren't accidentally introduced.
/// - newlines → space.
///
/// Implemented as a single-pass char loop.
#[must_use]
pub fn escape_label_sequence(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '#' => out.push('_'),
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
    fn sanitize_id_replaces_punctuation_and_appends_hash() {
        // Inputs with non-canonical chars get the `_H<8-hex>` suffix so
        // distinct sources don't collapse to the same node ID.
        let a = sanitize_id("foo-bar.rs");
        assert!(a.starts_with("foo_bar_rs_H"), "got: {a}");
        assert_eq!(a.len(), "foo_bar_rs".len() + 10);

        let b = sanitize_id("crates/anatta-api");
        assert!(b.starts_with("crates_anatta_api_H"), "got: {b}");

        let c = sanitize_id("a::b::c");
        assert!(c.starts_with("a__b__c_H"), "got: {c}");
    }

    #[test]
    fn sanitize_id_handles_empty() {
        assert_eq!(sanitize_id(""), "_");
    }

    #[test]
    fn sanitize_id_non_ascii() {
        // Regression for C30: `café` and `cafe_` previously collapsed to
        // identical sanitized IDs (`caf__` and `cafe_` both ended in
        // an underscore-rich shape) — the hash suffix on non-ASCII input
        // disambiguates them.
        let cafe_acute = sanitize_id("café");
        let cafe_plain = sanitize_id("cafe_");
        assert_ne!(cafe_acute, cafe_plain);
        assert!(cafe_acute.starts_with("caf__H"), "got: {cafe_acute}");
        assert_eq!(cafe_plain, "cafe_");

        let greek = sanitize_id("ε-greedy");
        assert!(greek.starts_with("__greedy_H"), "got: {greek}");
    }

    #[test]
    fn sanitize_id_no_unicode_alphanumeric_leaks() {
        // Anything outside [A-Za-z0-9_] must be replaced with `_`. The
        // canonical body never contains non-ASCII bytes — only the
        // `_H<hex>` suffix is allowed past the body.
        let out = sanitize_id("résumé");
        let body = out.split("_H").next().expect("split");
        assert!(body.is_ascii(), "body must be ASCII, got: {body}");
        assert!(
            body.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "body must be alnum+`_`, got: {body}"
        );
    }

    #[test]
    fn sanitize_id_distinct_inputs_get_distinct_ids() {
        // The whole point of the `_H<hex>` suffix: two entity ids that
        // would otherwise fold to the same ASCII shape end up with
        // distinct hash suffixes.
        let a = sanitize_id("code:src/foo.rs::function::Foo");
        let b = sanitize_id("code:src/foo.rs::function::foo");
        let c = sanitize_id("code_src_foo_rs__function__Foo");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
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
        // `graph!` has a non-canonical char → body is `graph_` (not a
        // bare reserved word) and the hash suffix is appended.
        let bang = sanitize_id("graph!");
        assert!(bang.starts_with("graph__H"), "got: {bang}");
        // Substring matches still need no escape.
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
    fn sanitize_id_is_deterministic() {
        // Running twice on the same input must produce the exact same
        // output — diff-by-id depends on this.
        for s in ["café", "code:src/foo.rs::function::Foo", "α::β::γ", "x", ""] {
            assert_eq!(sanitize_id(s), sanitize_id(s));
        }
    }

    #[test]
    fn escape_label_flowchart_replaces_quotes_and_brackets() {
        assert_eq!(escape_label_flowchart("foo"), "foo");
        assert_eq!(escape_label_flowchart(r#"a "b" c"#), "a &quot;b&quot; c");
        assert_eq!(escape_label_flowchart("x[y]"), "x[y&rsqb;");
        assert_eq!(escape_label_flowchart("a\nb"), "a b");
        assert_eq!(escape_label_flowchart(""), "");
    }

    #[test]
    fn escape_label_sequence_handles_angles_hash_and_newlines() {
        assert_eq!(escape_label_sequence("foo"), "foo");
        assert_eq!(escape_label_sequence("if x <= 0"), "if x &lt;= 0");
        assert_eq!(escape_label_sequence("a > b"), "a &gt; b");
        assert_eq!(escape_label_sequence("tag#1"), "tag_1");
        assert_eq!(escape_label_sequence("first\nsecond"), "first second");
        assert_eq!(escape_label_sequence("first\rsecond"), "first second");
        assert_eq!(escape_label_sequence(""), "");
    }

    #[test]
    fn escape_label_sequence_keeps_quotes_and_brackets() {
        // sequence labels don't share the `flowchart` `[".."]` syntax, so
        // `"` and `]` pass through untouched.
        assert_eq!(escape_label_sequence("a \"b\" c"), "a \"b\" c");
        assert_eq!(escape_label_sequence("[stuff]"), "[stuff]");
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
