//! Mermaid renderers driven by a [`polystore::GraphStore`] populated with
//! ingester atoms.
//!
//! v0.2 ships two levels:
//! - [`Level::Project`] — one node per crate with `(modules, functions,
//!   structs)` counts, plus cross-crate `calls` edges.
//! - [`Level::Overview`] — one node per module with `(functions, structs,
//!   traits)` counts, plus cross-module `calls` edges.
//!
//! Three more levels (module / function / impact) are scheduled for v0.3.

mod overview;
mod project;
pub mod util;

use crate::error::{AstToMermaidError, Result as AtmResult};
use ingester_core::{Atom, Relation};
use polystore::GraphStore;
use std::fmt;
use std::str::FromStr;

/// Which view to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Level {
    /// One node per crate, cross-crate `calls` edges.
    Project,
    /// One node per module, cross-module `calls` edges.
    Overview,
}

impl Level {
    /// Lower-case canonical name as used on the CLI.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Overview => "overview",
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Level {
    type Err = AstToMermaidError;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "project" => Ok(Self::Project),
            "overview" => Ok(Self::Overview),
            other => Err(AstToMermaidError::InvalidInput(format!(
                "unknown render level: {other:?} (expected: project | overview)"
            ))),
        }
    }
}

/// Render `level` against `store`. Returns the Mermaid source.
///
/// # Errors
///
/// - [`AstToMermaidError::Storage`] for any storage-layer error during
///   the read traversal.
/// - [`AstToMermaidError::InvalidInput`] for unknown levels (only via
///   [`Level::from_str`]).
pub async fn render<S>(level: Level, store: &S) -> AtmResult<String>
where
    S: GraphStore<Atom, Relation>,
{
    let s = match level {
        Level::Project => project::render(store).await?,
        Level::Overview => overview::render(store).await?,
    };
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::InMemoryStore;
    use polystore::Scope;

    fn scope() -> Scope {
        Scope::new("ns", "repo", "branch")
    }

    #[test]
    fn level_as_str_and_display() {
        assert_eq!(Level::Project.as_str(), "project");
        assert_eq!(Level::Overview.as_str(), "overview");
        assert_eq!(Level::Project.to_string(), "project");
        assert_eq!(format!("{}", Level::Overview), "overview");
    }

    #[test]
    fn level_from_str_known() {
        assert_eq!("project".parse::<Level>().expect("ok"), Level::Project);
        assert_eq!("overview".parse::<Level>().expect("ok"), Level::Overview);
    }

    #[test]
    fn level_from_str_unknown_errors() {
        let err = "module".parse::<Level>().expect_err("unknown");
        assert!(matches!(err, AstToMermaidError::InvalidInput(_)));
        assert!(err.to_string().contains("module"));
    }

    #[test]
    fn level_clone_copy_eq_hash() {
        use std::collections::HashSet;
        let a = Level::Project;
        let b = a;
        assert_eq!(a, b);
        let mut set = HashSet::new();
        set.insert(a);
        set.insert(b);
        assert_eq!(set.len(), 1);
        assert!(format!("{a:?}").contains("Project"));
    }

    #[tokio::test]
    async fn render_dispatches_to_correct_level() {
        let store = InMemoryStore::new(scope());
        let project = render(Level::Project, &store).await.expect("project");
        let overview = render(Level::Overview, &store).await.expect("overview");
        // Both empty stores yield only the graph header.
        assert_eq!(project, "graph TD\n");
        assert_eq!(overview, "graph TD\n");
    }
}
