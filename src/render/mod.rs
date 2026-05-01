//! Mermaid renderers driven by a [`Store`] populated with code atoms.
//!
//! v0.3 ships five levels:
//! - [`Level::Project`] — one node per crate with `(modules, functions,
//!   structs)` counts, plus cross-crate `calls` edges.
//! - [`Level::Overview`] — one node per module with `(functions, structs,
//!   traits)` counts, plus cross-module `calls` edges.
//! - [`Level::Module`] (target = module path or name) — one subgraph for
//!   the target module containing all its items, plus external nodes for
//!   incoming callers and outgoing callees.
//! - [`Level::Function`] (target = function name or id) — central target
//!   node with direct callers (in) and callees (out).
//! - [`Level::Impact`] (target = function name or id) — reverse call chain
//!   walked back N hops (default 3) — answers "who breaks if I change this?".

mod function;
mod impact;
pub mod lookup;
mod module;
mod overview;
mod project;
pub mod util;

use crate::error::{AstToMermaidError, Result as AtmResult};
use crate::graph::Store;
use std::fmt;
use std::str::FromStr;

pub use impact::DEFAULT_HOPS;

/// Which view to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Level {
    /// One node per crate, cross-crate `calls` edges.
    Project,
    /// One node per module, cross-module `calls` edges.
    Overview,
    /// Subgraph for one module + its external in/out callers.
    Module,
    /// Central target function + direct callers/callees.
    Function,
    /// Reverse call chain from target up to N hops.
    Impact,
}

impl Level {
    /// Lower-case canonical name as used on the CLI.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Overview => "overview",
            Self::Module => "module",
            Self::Function => "function",
            Self::Impact => "impact",
        }
    }

    /// Whether this level requires a `--target` argument from the caller.
    #[must_use]
    pub fn requires_target(self) -> bool {
        matches!(self, Self::Module | Self::Function | Self::Impact)
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
            "module" => Ok(Self::Module),
            "function" => Ok(Self::Function),
            "impact" => Ok(Self::Impact),
            other => Err(AstToMermaidError::InvalidInput(format!(
                "unknown render level: {other:?} (expected: project | overview | module | function | impact)"
            ))),
        }
    }
}

/// Render `level` against `store`. `target` is required for module / function
/// / impact levels and ignored for project / overview.
///
/// # Errors
///
/// - [`AstToMermaidError::InvalidInput`] when a target is required but absent
///   or doesn't resolve.
pub fn render(level: Level, store: &Store, target: Option<&str>) -> AtmResult<String> {
    let s = match level {
        Level::Project => project::render(store),
        Level::Overview => overview::render(store),
        Level::Module => module::render(store, require_target(level, target)?)?,
        Level::Function => function::render(store, require_target(level, target)?)?,
        Level::Impact => impact::render(store, require_target(level, target)?, DEFAULT_HOPS)?,
    };
    Ok(s)
}

fn require_target(level: Level, target: Option<&str>) -> AtmResult<&str> {
    target.ok_or_else(|| {
        AstToMermaidError::InvalidInput(format!("level={level} requires a --target argument"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Store;

    #[test]
    fn level_as_str_and_display() {
        for (lvl, s) in [
            (Level::Project, "project"),
            (Level::Overview, "overview"),
            (Level::Module, "module"),
            (Level::Function, "function"),
            (Level::Impact, "impact"),
        ] {
            assert_eq!(lvl.as_str(), s);
            assert_eq!(lvl.to_string(), s);
        }
    }

    #[test]
    fn level_from_str_known() {
        for (s, lvl) in [
            ("project", Level::Project),
            ("overview", Level::Overview),
            ("module", Level::Module),
            ("function", Level::Function),
            ("impact", Level::Impact),
        ] {
            assert_eq!(s.parse::<Level>().expect("ok"), lvl);
        }
    }

    #[test]
    fn level_from_str_unknown_errors() {
        let err = "graph".parse::<Level>().expect_err("unknown");
        assert!(matches!(err, AstToMermaidError::InvalidInput(_)));
        assert!(err.to_string().contains("graph"));
    }

    #[test]
    fn requires_target_per_variant() {
        assert!(!Level::Project.requires_target());
        assert!(!Level::Overview.requires_target());
        assert!(Level::Module.requires_target());
        assert!(Level::Function.requires_target());
        assert!(Level::Impact.requires_target());
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

    #[test]
    fn render_dispatches_project_and_overview_without_target() {
        let store = Store::new();
        let project = render(Level::Project, &store, None).expect("project");
        let overview = render(Level::Overview, &store, None).expect("overview");
        assert_eq!(project, "graph TD\n");
        assert_eq!(overview, "graph TD\n");
    }

    #[test]
    fn render_target_required_for_zoom_levels() {
        let store = Store::new();
        for lvl in [Level::Module, Level::Function, Level::Impact] {
            let err = render(lvl, &store, None).expect_err("must require target");
            assert!(err.to_string().contains("--target"));
        }
    }
}
