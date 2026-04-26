//! MCP server entry point (stdio JSON-RPC).
//!
//! `v0.1.0` ships the entry point only — actual tool dispatch (`analyze`,
//! `code_map`, `code_search`, `call_graph`, …) lands in a subsequent PR.

use crate::error::{AstToMermaidError, Result};

/// Start the MCP server on stdin/stdout.
///
/// Returns [`AstToMermaidError::NotImplemented`] in `v0.1.0`.
///
/// # Errors
///
/// Currently always returns `NotImplemented`. Future versions will return
/// errors from the underlying transport, JSON-RPC framing, or tool handlers.
pub fn serve_stdio() -> Result<()> {
    Err(AstToMermaidError::NotImplemented("mcp::serve_stdio"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_stdio_is_not_implemented_yet() {
        let err = serve_stdio().expect_err("v0.1.0 must return NotImplemented");
        let AstToMermaidError::NotImplemented(what) = err else {
            unreachable!("guarded by previous expect_err contract")
        };
        assert_eq!(what, "mcp::serve_stdio");
    }
}
