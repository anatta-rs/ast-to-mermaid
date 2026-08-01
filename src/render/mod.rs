//! Mermaid renderers driven by a [`crate::graph::Store`] populated with code
//! atoms.
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

pub mod adj;
pub mod dot;
pub mod flow;
mod function;
mod impact;
pub mod lookup;
mod module;
mod overview;
mod project;
pub mod snapshot;
pub mod util;

use crate::error::{AstToMermaidError, Result as AtmResult};
use crate::graph::Store;
use std::fmt;
use std::str::FromStr;

pub use adj::AdjMaps;
pub use dot::mermaid_to_dot;
pub use impact::DEFAULT_HOPS;
pub use snapshot::AtomSnapshot;

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
    /// Forward call graph from an entry point, edges annotated with call
    /// order and control-flow context. Rendered by [`flow::render`],
    /// which takes depth and external-leaf options the shared [`render`]
    /// dispatcher does not carry — `pipeline::analyze` branches on it
    /// before reaching the dispatcher.
    Flow,
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
            Self::Flow => "flow",
        }
    }

    /// Whether this level requires a `--target` argument from the caller.
    #[must_use]
    pub fn requires_target(self) -> bool {
        matches!(
            self,
            Self::Module | Self::Function | Self::Impact | Self::Flow
        )
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

/// Render `level` against `snapshot`, reusing `adj` for every adjacency
/// lookup. `target` is required for module / function / impact levels and
/// ignored for project / overview.
///
/// `adj` should be built once per logical operation via [`AdjMaps::build`]
/// and shared across all level renders the caller needs — that is the whole
/// point of threading it explicitly: bundle invocations avoid re-sweeping
/// the edge slice once per level.
///
/// `snapshot` is a borrowed `id → &CodeAtom` view (see [`AtomSnapshot`]).
/// Build it once inside a [`crate::graph::Store::with_atoms`] callback —
/// every per-atom lookup downstream is then a single `HashMap` probe with
/// no `RwLock` traffic and no [`crate::model::CodeAtom`] clones.
///
/// # Errors
///
/// - [`AstToMermaidError::InvalidInput`] when a target is required but absent
///   or doesn't resolve.
pub fn render(
    level: Level,
    adj: &AdjMaps,
    snapshot: &AtomSnapshot<'_>,
    target: Option<&str>,
) -> AtmResult<String> {
    let s = match level {
        Level::Project => project::render(adj, snapshot),
        Level::Overview => overview::render(adj, snapshot),
        Level::Module => module::render(adj, snapshot, require_target(level, target)?)?,
        Level::Function => function::render(adj, snapshot, require_target(level, target)?)?,
        Level::Impact => {
            impact::render(adj, snapshot, require_target(level, target)?, DEFAULT_HOPS)?
        }
        // Handled upstream in `pipeline::analyze`, which has the depth
        // and external options this dispatcher does not receive.
        Level::Flow => flow::render(
            adj,
            snapshot,
            require_target(level, target)?,
            flow::DEFAULT_DEPTH,
            flow::External::NearOnly,
        )?,
    };
    Ok(s)
}

/// Convenience wrapper that builds an [`AtomSnapshot`] from `store` under a
/// single read guard and dispatches to [`render`].
///
/// Prefer [`render`] directly when the caller already needs the snapshot for
/// other work in the same critical section (e.g. [`crate::artifacts::emit_artifacts`]
/// builds the snapshot once and reuses it for every per-entity render +
/// metadata pass).
///
/// # Errors
///
/// Same as [`render`].
pub fn render_in_store(
    level: Level,
    store: &Store,
    adj: &AdjMaps,
    target: Option<&str>,
) -> AtmResult<String> {
    store.with_atoms(|atoms| {
        let snapshot = AtomSnapshot::build(atoms);
        render(level, adj, &snapshot, target)
    })
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
        let adj = AdjMaps::build(&store);
        let project = render_in_store(Level::Project, &store, &adj, None).expect("project");
        let overview = render_in_store(Level::Overview, &store, &adj, None).expect("overview");
        assert_eq!(project, "graph TD\n");
        assert_eq!(overview, "graph TD\n");
    }

    #[test]
    fn render_target_required_for_zoom_levels() {
        let store = Store::new();
        let adj = AdjMaps::build(&store);
        for lvl in [Level::Module, Level::Function, Level::Impact] {
            let err = render_in_store(lvl, &store, &adj, None).expect_err("must require target");
            assert!(err.to_string().contains("--target"));
        }
    }
}
