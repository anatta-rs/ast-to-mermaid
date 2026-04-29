//! Error types for the ast-to-mermaid pipeline.

use thiserror::Error;

/// Errors raised by the parser, renderer, resolver, or storage adapters.
#[derive(Debug, Error)]
pub enum AstToMermaidError {
    /// The requested feature is not yet implemented at this version.
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

    /// Invalid CLI arguments or user input.
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// I/O error (file read, directory walk, artifact write).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, AstToMermaidError>;

#[cfg(test)]
mod tests {
    use super::*;

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
