//! Crate-wide AST recursion depth limit shared by every recursive
//! tree-sitter visitor — [`crate::parser::rust::flatten_use`],
//! [`crate::sequence::visit::State`], and the sequence renderer
//! ([`crate::sequence::render`]).
//!
//! Lifted out of [`crate::sequence`] so the parser layer no longer
//! reaches up into the sequence layer for a depth-limit constant.

/// Default AST recursion depth limit shared by every recursive AST
/// visitor in this crate.
///
/// The guard turns a deeply nested adversarial source file into a
/// degraded-but-finite diagram instead of a stack-overflow crash.
/// Override at runtime with the `A2M_MAX_AST_DEPTH` env var; see
/// [`max_ast_depth`].
pub const MAX_AST_DEPTH: usize = 256;

/// Resolved AST recursion depth limit. Returns the parsed value of
/// `A2M_MAX_AST_DEPTH` when it is set to a positive integer, otherwise
/// falls back to [`MAX_AST_DEPTH`]. Read fresh on every call so a test
/// (or operator) can change the limit per run without restart.
#[must_use]
pub fn max_ast_depth() -> usize {
    resolve_max_ast_depth(std::env::var("A2M_MAX_AST_DEPTH").ok().as_deref())
}

/// Pure resolver behind [`max_ast_depth`]: takes the raw env value (or
/// `None`) and returns the depth limit. Split out so unit tests can
/// exercise the override logic without mutating process env.
fn resolve_max_ast_depth(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(MAX_AST_DEPTH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_max_ast_depth_respects_env_override() {
        // Pure resolver: a positive integer in the env wins over the
        // default. This is the exact path `max_ast_depth()` runs after
        // reading `A2M_MAX_AST_DEPTH`.
        assert_eq!(resolve_max_ast_depth(Some("8")), 8);
        assert_eq!(resolve_max_ast_depth(Some("1024")), 1024);
    }

    #[test]
    fn resolve_max_ast_depth_falls_back_on_garbage() {
        assert_eq!(resolve_max_ast_depth(Some("0")), MAX_AST_DEPTH);
        assert_eq!(resolve_max_ast_depth(Some("-3")), MAX_AST_DEPTH);
        assert_eq!(resolve_max_ast_depth(Some("not-a-number")), MAX_AST_DEPTH);
        assert_eq!(resolve_max_ast_depth(Some("")), MAX_AST_DEPTH);
        assert_eq!(resolve_max_ast_depth(None), MAX_AST_DEPTH);
    }

    #[test]
    fn max_ast_depth_default_constant_value() {
        // The default cap stays put at 256 — bumped only by an
        // intentional code change. Test guards against accidental drift.
        assert_eq!(MAX_AST_DEPTH, 256);
    }

    #[test]
    fn max_ast_depth_resolver_treats_invalid_env_as_default() {
        // The public `max_ast_depth()` reads the env each call; with no
        // override set it must equal `MAX_AST_DEPTH`. We cannot mutate
        // env safely under `deny(unsafe_code)`, so the override path is
        // verified end-to-end by the binary `a2m` (see test plan in
        // issue #75) and locally by callers that feed an explicit
        // `A2M_MAX_AST_DEPTH` to a subprocess.
        let key = "A2M_MAX_AST_DEPTH";
        if std::env::var(key).is_err() {
            assert_eq!(max_ast_depth(), MAX_AST_DEPTH);
        }
    }
}
