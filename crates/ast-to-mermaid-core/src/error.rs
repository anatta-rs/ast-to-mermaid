//! Error types for the ast-to-mermaid pipeline.

use thiserror::Error;

/// Errors raised by the parser, renderer, resolver, or storage adapters.
#[derive(Debug, Error)]
pub enum AstToMermaidError {
    /// Wraps a [`polystore::PolystoreError`] from any storage backend.
    #[error("storage: {0}")]
    Storage(#[from] polystore::PolystoreError),

    /// The requested feature is not yet implemented at this version.
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

    /// Invalid CLI arguments or MCP request.
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// Filesystem I/O failure (read / write / mkdir while building or
    /// loading an artifact bundle).
    #[error("io: {0}")]
    Io(String),

    /// A required artifact file (overview.mmd, index.json, a meta.json
    /// referenced from .mmd) was not found in the bundle directory.
    #[error("artifact not found: {0}")]
    ArtifactNotFound(String),
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, AstToMermaidError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_wraps_polystore_error() {
        let pe = polystore::PolystoreError::NotFound("x".to_owned());
        let e: AstToMermaidError = pe.into();
        assert!(e.to_string().starts_with("storage:"));
    }

    #[test]
    fn not_implemented_displays() {
        let e = AstToMermaidError::NotImplemented("parser");
        assert_eq!(e.to_string(), "not implemented: parser");
    }

    #[test]
    fn invalid_input_displays() {
        let e = AstToMermaidError::InvalidInput("bad scope".to_owned());
        assert_eq!(e.to_string(), "invalid input: bad scope");
    }

    #[test]
    fn debug_renders() {
        let e = AstToMermaidError::NotImplemented("render");
        assert!(format!("{e:?}").contains("NotImplemented"));
    }

    #[test]
    fn result_alias_carries_value_or_error() {
        fn maybe(success: bool) -> Result<i32> {
            if success {
                Ok(42)
            } else {
                Err(AstToMermaidError::NotImplemented("foo"))
            }
        }
        assert_eq!(maybe(true).expect("ok"), 42);
        assert!(maybe(false).is_err());
    }
}
