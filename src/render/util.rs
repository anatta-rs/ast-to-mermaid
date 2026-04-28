//! Mermaid rendering primitives shared by every level.

/// Sanitize a name so it can be used as a Mermaid node ID.
///
/// Mermaid IDs must be alphanumeric or `_`. Any other character is replaced
/// with `_`. Empty input becomes a single `_` to keep IDs syntactically valid.
#[must_use]
pub fn mermaid_id(name: &str) -> String {
    if name.is_empty() {
        return "_".to_owned();
    }
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
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
